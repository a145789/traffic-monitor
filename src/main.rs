#![windows_subsystem = "windows"]

mod collector;
mod config;
mod ffi_guard;
mod renderer;
mod state;
mod suspend;
mod thermal;
mod tray;
mod update;
mod util;
mod window;

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::Power::{
    HPOWERNOTIFY, RegisterPowerSettingNotification, UnregisterPowerSettingNotification,
};
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::Ime::ImmDisableIME;
use windows::Win32::UI::WindowsAndMessaging::REGISTER_NOTIFICATION_FLAGS;
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, FindWindowW, KillTimer, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PostMessageW,
    PostQuitMessage, RegisterWindowMessageW, SW_HIDE, SetTimer, ShowWindow, WM_CLOSE, WM_COMMAND,
    WM_CONTEXTMENU, WM_CREATE, WM_DPICHANGED, WM_PAINT, WM_POWERBROADCAST, WM_SETTINGCHANGE,
    WM_TIMER, WM_WTSSESSION_CHANGE,
};
use windows::core::w;

use crate::collector::{
    WM_USER_NETWORK_DISCONNECTED, WM_USER_NETWORK_RECONNECTED, collect_cpu, collect_memory,
    collect_network, init_network_listener, trim_working_set,
};
use crate::config::{
    LOWORD_MASK, TIMER_ID_CPU_MEM, TIMER_ID_FULLSCREEN, TIMER_ID_INIT_TRIM, TIMER_ID_NETWORK,
    TIMER_ID_THERMAL,
};
use crate::renderer::Renderer;
use crate::state::{
    CONSECUTIVE_ZERO_COUNT, ENABLE_AUTO_UPDATE, FULLSCREEN, NETWORK_BACKOFF,
    SUSPEND_REASON_MONITOR, SUSPEND_REASON_SESSION, SUSPEND_REASON_SYSTEM, THERMAL_STATE,
};
use crate::thermal::collect_thermal;
use crate::tray::{
    WM_APP_TRAY, create_main_window, create_tray_icon, register_window_class, remove_tray_icon,
};
use crate::update::{
    WM_USER_UPDATE_ACTION, init_cleanup_temp, load_auto_update_enabled, start_auto_check,
    subprocess_main,
};
use crate::util::show_error;

const PBT_POWERSETTINGCHANGE: u32 = 0x8013;
const DEVICE_NOTIFY_WINDOW_HANDLE: u32 = 1;

const GUID_MONITOR_POWER_ON: windows::core::GUID = windows::core::GUID::from_values(
    0x02731015,
    0x4510,
    0x4526,
    [0x99, 0xE6, 0xE5, 0xA1, 0x7E, 0xBD, 0x1A, 0xEA],
);

#[repr(C)]
#[allow(non_snake_case)]
struct POWERBROADCAST_SETTING {
    PowerSetting: windows::core::GUID,
    DataLength: u32,
    Data: [u8; 1],
}

thread_local! {
    static RENDERER: RefCell<Option<Renderer>> = const { RefCell::new(None) };
}

static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);
static POWER_NOTIFY_HANDLE: AtomicIsize = AtomicIsize::new(0);

fn quit_existing_instance() {
    let class_name: Vec<u16> = crate::config::WINDOW_CLASS.encode_utf16().collect();
    // SAFETY: class_name 以 NUL 结尾，FindWindowW 查询不存在的窗口时安全返回 None。
    let hwnd = unsafe {
        FindWindowW(
            windows::core::PCWSTR(class_name.as_ptr()),
            windows::core::PCWSTR(std::ptr::null()),
        )
    };

    if let Ok(h) = hwnd
        && !h.is_invalid()
    {
        // SAFETY: h 有效，PostMessageW 异步投递 WM_CLOSE 是线程安全的。
        unsafe {
            let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            // SAFETY: class_name 仍在作用域内。
            let exist = unsafe {
                FindWindowW(
                    windows::core::PCWSTR(class_name.as_ptr()),
                    windows::core::PCWSTR(std::ptr::null()),
                )
            };
            if exist.is_err() {
                break;
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--quit") {
        quit_existing_instance();
        return;
    }

    // 必须在单例 Mutex 锁之前拦截 --check-update，否则子进程会被当作重复实例直接退出。
    if args.iter().any(|a| a == "--check-update") {
        let is_manual = args.iter().any(|a| a == "--manual");
        std::process::exit(subprocess_main(is_manual));
    }

    let mutex_name: Vec<u16> = crate::config::MUTEX_NAME.encode_utf16().collect();

    // SAFETY: mutex_name 以 NUL 结尾，句柄由 MutexGuard 管理。
    let mutex_handle =
        unsafe { CreateMutexW(None, true, windows::core::PCWSTR(mutex_name.as_ptr())) };

    let _mutex_guard = match mutex_handle {
        Ok(handle) => {
            // SAFETY: 立即在 CreateMutexW 之后读取 GetLastError，避免中间插入其他 Win32 调用导致错误码被覆盖。
            let last_error = unsafe { GetLastError() };
            let guard = crate::ffi_guard::MutexGuard(handle);
            if last_error == ERROR_ALREADY_EXISTS {
                return;
            }
            guard
        }
        Err(_) => {
            show_error("Failed to create mutex");
            return;
        }
    };

    // 主进程没有任何文本输入需求。必须在首个顶层窗口收到 WM_CREATE 前禁用整个
    // 进程的 IME，避免更新子进程弹窗关闭后的焦点回落触发第三方 TSF/IME 常驻。
    // SAFETY:
    // ImmDisableIME 只修改当前进程的输入法管理状态，不接收指针或外部句柄；
    // u32::MAX 是 Win32 约定的“当前进程全部现有及后续线程”标识。此处尚未调用
    // CreateWindowExW，满足 API 必须在首个顶层窗口创建前执行的时序要求。
    unsafe {
        let _ = ImmDisableIME(u32::MAX);
    }

    if register_window_class().is_err() {
        show_error("Failed to register window class");
        return;
    }

    let hwnd = match create_main_window() {
        Ok(h) => h,
        Err(e) => {
            show_error(&format!("Failed to create window: {}", e));
            return;
        }
    };

    init_network_listener(hwnd);

    // SAFETY: "TaskbarCreated" 是 Windows 约定的常量字符串。
    let taskbar_msg = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    TASKBAR_CREATED_MSG.store(taskbar_msg, Ordering::Release);

    // SAFETY: hwnd 有效，GUID_MONITOR_POWER_ON 是系统静态 GUID。
    let power_notify = unsafe {
        RegisterPowerSettingNotification(
            HANDLE(hwnd.0),
            &GUID_MONITOR_POWER_ON,
            REGISTER_NOTIFICATION_FLAGS(DEVICE_NOTIFY_WINDOW_HANDLE),
        )
    };
    if let Ok(handle) = power_notify {
        POWER_NOTIFY_HANDLE.store(handle.0, Ordering::Release);
    }

    if !embed_in_taskbar(hwnd) {
        show_error("Failed to embed in taskbar. Make sure explorer.exe is running.");
        return;
    }

    let auto_update = load_auto_update_enabled();
    ENABLE_AUTO_UPDATE.store(auto_update, Ordering::Release);

    create_tray_icon(hwnd);

    RENDERER.with(|r| {
        *r.borrow_mut() = Some(Renderer::new());
    });

    RENDERER.with(|r| {
        if let Some(renderer) = r.borrow_mut().as_mut() {
            renderer.update_dpi(hwnd);
            renderer.update_text_color();
        }
    });

    // SAFETY: hwnd 有效，触发初始重绘。
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }

    if !sync_monitoring_timers(hwnd) {
        show_error("Failed to create monitoring timers");
        return;
    }

    // SAFETY: hwnd 有效，注册会话通知。
    unsafe {
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
    }

    init_cleanup_temp();
    start_auto_check(hwnd);

    // SAFETY:
    // hwnd 由 CreateWindowExW 返回，经 is_invalid() 校验通过，为当前进程的有效主窗口句柄。
    // TIMER_ID_INIT_TRIM (99) 为唯一常量，不与已有定时器 ID (1/2/3) 冲突。
    // 一次性定时器在 10 秒后触发 trim_working_set()，释放初始化阶段遗留的冷代码页。
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_ID_INIT_TRIM, 10000, None);
    }

    let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();

    // SAFETY: msg 由操作系统填充，消息循环是标准 Win32 模式。
    unsafe {
        while windows::Win32::UI::WindowsAndMessaging::GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
            windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
        }
    }

    // SAFETY: hwnd 有效，注销会话通知。
    unsafe {
        let _ = WTSUnRegisterSessionNotification(hwnd);
    }

    let power_handle = POWER_NOTIFY_HANDLE.load(Ordering::Acquire);
    if power_handle != 0 {
        // SAFETY: 注销先前注册的电源通知句柄。
        unsafe {
            let _ = UnregisterPowerSettingNotification(HPOWERNOTIFY(power_handle));
        }
    }

    RENDERER.with(|r| {
        let _ = r.borrow_mut().take();
    });
}

use crate::suspend::{
    check_fullscreen, is_immersive_color_set, is_suspended, resume_system, suspend_system,
    sync_monitoring_timers,
};
use crate::window::{embed_in_taskbar, invalidate_taskbar_cache, update_taskbar_position};

const WTS_SESSION_LOCK: usize = 0x7;
const WTS_SESSION_UNLOCK: usize = 0x8;

fn handle_taskbar_created(hwnd: HWND) -> LRESULT {
    invalidate_taskbar_cache();
    remove_tray_icon();
    // SAFETY: hwnd 有效，隐藏窗口。
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    if embed_in_taskbar(hwnd) {
        create_tray_icon(hwnd);
        RENDERER.with(|r| {
            if let Some(renderer) = r.borrow_mut().as_mut() {
                renderer.update_dpi(hwnd);
                renderer.update_text_color();
            }
        });

        if !sync_monitoring_timers(hwnd) {
            show_error("Failed to restore monitoring timers after Explorer restart");
        }
    }
    LRESULT(0)
}

fn handle_timer(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    match wparam.0 {
        TIMER_ID_INIT_TRIM => {
            trim_working_set();
            // SAFETY:
            // hwnd 来自窗口过程，为操作系统分配的有效窗口句柄。
            // TIMER_ID_INIT_TRIM 为本次启动时已创建的定时器 ID。
            // KillTimer 对已销毁或不存在的定时器仅返回错误，不会触发 UB。
            unsafe {
                KillTimer(Some(hwnd), TIMER_ID_INIT_TRIM).ok();
            }
        }
        TIMER_ID_FULLSCREEN => {
            if !is_suspended() {
                check_fullscreen(hwnd);
            }
        }
        TIMER_ID_NETWORK => {
            if !is_suspended() && !FULLSCREEN.load(Ordering::Acquire) {
                update_taskbar_position(hwnd);
                collect_network();
                // SAFETY: hwnd 有效，刷新网速显示。
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
        }
        TIMER_ID_CPU_MEM => {
            if !is_suspended() && !FULLSCREEN.load(Ordering::Acquire) {
                collect_cpu();
                collect_memory();
                // SAFETY: hwnd 有效，刷新 CPU/内存显示。
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
        }
        TIMER_ID_THERMAL => {
            if !is_suspended() && !FULLSCREEN.load(Ordering::Acquire) {
                let prev = THERMAL_STATE.load(Ordering::Relaxed);
                collect_thermal();
                // 仅在热状态实际跳变时触发重绘，避免每秒空转 DWM 合成。
                if THERMAL_STATE.load(Ordering::Relaxed) != prev {
                    // SAFETY: hwnd 有效，刷新热风险变色显示。
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
        }
        _ => {}
    }
    LRESULT(0)
}

fn handle_power_broadcast(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match wparam.0 as u32 {
        PBT_APMSUSPEND => {
            suspend_system(hwnd, SUSPEND_REASON_SYSTEM);
        }
        PBT_APMRESUMEAUTOMATIC => {
            resume_system(hwnd, SUSPEND_REASON_SYSTEM, true);
        }
        PBT_POWERSETTINGCHANGE => {
            let setting = lparam.0 as *const POWERBROADCAST_SETTING;
            if !setting.is_null() {
                // SAFETY: PBT_POWERSETTINGCHANGE 时 OS 保证 lparam 指向有效的 POWERBROADCAST_SETTING。
                let setting_ref = unsafe { &*setting };
                if setting_ref.PowerSetting == GUID_MONITOR_POWER_ON && setting_ref.DataLength >= 1
                {
                    let monitor_on = setting_ref.Data[0] != 0;
                    if monitor_on {
                        resume_system(hwnd, SUSPEND_REASON_MONITOR, true);
                    } else {
                        suspend_system(hwnd, SUSPEND_REASON_MONITOR);
                    }
                }
            }
        }
        _ => {}
    }
    LRESULT(0)
}

fn handle_session_change(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    match wparam.0 {
        WTS_SESSION_LOCK => {
            suspend_system(hwnd, SUSPEND_REASON_SESSION);
        }
        WTS_SESSION_UNLOCK => {
            resume_system(hwnd, SUSPEND_REASON_SESSION, true);
        }
        _ => {}
    }
    LRESULT(0)
}

pub extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let tcm = TASKBAR_CREATED_MSG.load(Ordering::Acquire);
    if msg == tcm && tcm != 0 {
        return handle_taskbar_created(hwnd);
    }

    match msg {
        WM_CREATE => LRESULT(0),

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: hwnd 有效，BeginPaint/EndPaint 配对使用。
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            RENDERER.with(|r| {
                if let Some(renderer) = r.borrow_mut().as_mut() {
                    renderer.render(hdc);
                }
            });
            // SAFETY: hwnd 与 ps 有效，结束绘图。
            unsafe {
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }

        WM_TIMER => handle_timer(hwnd, wparam),

        WM_USER_NETWORK_DISCONNECTED => {
            let _ = sync_monitoring_timers(hwnd);
            LRESULT(0)
        }

        WM_USER_NETWORK_RECONNECTED => {
            NETWORK_BACKOFF.store(false, Ordering::Release);
            CONSECUTIVE_ZERO_COUNT.store(0, Ordering::Release);
            let _ = sync_monitoring_timers(hwnd);
            start_auto_check(hwnd);
            LRESULT(0)
        }

        WM_USER_UPDATE_ACTION => {
            crate::update::handle_update_action(wparam.0);
            LRESULT(0)
        }

        WM_SETTINGCHANGE => {
            // SAFETY: WM_SETTINGCHANGE 的 lparam 由 OS 保证指向合法的 NUL 结尾宽字符串。
            if unsafe { is_immersive_color_set(lparam) } {
                RENDERER.with(|r| {
                    if let Some(renderer) = r.borrow_mut().as_mut() {
                        renderer.update_text_color();
                    }
                });
            }
            LRESULT(0)
        }

        WM_DPICHANGED => {
            RENDERER.with(|r| {
                if let Some(renderer) = r.borrow_mut().as_mut() {
                    renderer.update_dpi(hwnd);
                }
            });
            let _ = embed_in_taskbar(hwnd);
            LRESULT(0)
        }

        WM_POWERBROADCAST => handle_power_broadcast(hwnd, wparam, lparam),

        WM_WTSSESSION_CHANGE => handle_session_change(hwnd, wparam),

        WM_CLOSE => {
            remove_tray_icon();
            // SAFETY: PostQuitMessage 向当前线程投递 WM_QUIT。
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }

        WM_COMMAND => {
            let menu_id = (wparam.0 as u32) & LOWORD_MASK;
            tray::handle_menu_command(hwnd, menu_id);
            LRESULT(0)
        }

        x if x == WM_APP_TRAY => {
            let event = (lparam.0 as u32) & LOWORD_MASK;
            if event == WM_CONTEXTMENU {
                tray::show_context_menu(hwnd);
            }
            LRESULT(0)
        }

        // SAFETY: hwnd、msg、wparam、lparam 由操作系统传入，调用默认窗口过程是安全的。
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
