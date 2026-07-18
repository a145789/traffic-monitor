#![windows_subsystem = "windows"]

mod collector;
mod config;
mod ffi_guard;
mod renderer;
mod state;
mod suspend;
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
use windows::Win32::System::SystemServices::GUID_MONITOR_POWER_ON;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::Ime::ImmDisableIME;
use windows::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, FindWindowW, KillTimer, PBT_APMRESUMEAUTOMATIC,
    PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE, PostMessageW, PostQuitMessage, RegisterWindowMessageW,
    SW_HIDE, SetTimer, ShowWindow, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_CREATE, WM_DPICHANGED,
    WM_PAINT, WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_TIMER, WM_WTSSESSION_CHANGE,
};
use windows::core::{PCWSTR, w};

use crate::collector::{
    WM_USER_NETWORK_DISCONNECTED, WM_USER_NETWORK_RECONNECTED, collect_cpu, collect_memory,
    collect_network, init_network_listener, trim_working_set,
};
use crate::config::{
    LOWORD_MASK, TIMER_ID_CPU_MEM, TIMER_ID_FULLSCREEN, TIMER_ID_INIT_TRIM, TIMER_ID_NETWORK,
};
use crate::renderer::Renderer;
use crate::state::{
    CONSECUTIVE_ZERO_COUNT, ENABLE_AUTO_UPDATE, MONITOR_FULLSCREEN, NETWORK_BACKOFF,
    SUSPEND_REASON_MONITOR, SUSPEND_REASON_SESSION, SUSPEND_REASON_SYSTEM,
};
use crate::suspend::{
    check_fullscreen, is_immersive_color_set, is_suspended, resume_system, suspend_system,
    sync_monitoring_timers,
};
use crate::tray::{
    WM_APP_TRAY, create_main_window, create_tray_icon, register_window_class, remove_tray_icon,
};
use crate::update::{
    WM_USER_UPDATE_ACTION, init_cleanup_temp, load_auto_update_enabled, start_auto_check,
    subprocess_main,
};
use crate::util::show_error;
use crate::window::{embed_in_taskbar, invalidate_taskbar_cache, update_taskbar_position};

const WTS_SESSION_LOCK: usize = 0x7;
const WTS_SESSION_UNLOCK: usize = 0x8;

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
    // WINDOW_CLASS 常量已含尾 NUL。
    let class_name: Vec<u16> = crate::config::WINDOW_CLASS.encode_utf16().collect();
    let class_pcw = PCWSTR(class_name.as_ptr());
    // SAFETY: class_name 以 NUL 结尾，查询不存在的窗口时安全返回错误。
    let hwnd = unsafe { FindWindowW(class_pcw, PCWSTR(std::ptr::null())) };

    if let Ok(h) = hwnd
        && !h.is_invalid()
    {
        unsafe {
            let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let exist = unsafe { FindWindowW(class_pcw, PCWSTR(std::ptr::null())) };
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

    // 必须在单例 Mutex 之前拦截 --check-update，否则子进程会被当作重复实例退出。
    if args.iter().any(|a| a == "--check-update") {
        let is_manual = args.iter().any(|a| a == "--manual");
        std::process::exit(subprocess_main(is_manual));
    }

    // MUTEX_NAME 常量已含尾 NUL。
    let mutex_name: Vec<u16> = crate::config::MUTEX_NAME.encode_utf16().collect();
    // SAFETY: mutex_name 以 NUL 结尾；句柄由 MutexGuard 关闭。
    let mutex_handle = unsafe { CreateMutexW(None, true, PCWSTR(mutex_name.as_ptr())) };

    let _mutex_guard = match mutex_handle {
        Ok(handle) => {
            // SAFETY: 紧接 CreateMutexW 读取 last-error，避免被中间调用覆盖。
            let last_error = unsafe { GetLastError() };
            let guard = crate::ffi_guard::MutexGuard(handle);
            if last_error == ERROR_ALREADY_EXISTS {
                return;
            }
            guard
        }
        Err(_) => {
            show_error("创建单例互斥量失败");
            return;
        }
    };

    // 首个顶层窗口创建前禁用进程 IME，避免更新弹窗焦点回落触发第三方 TSF 常驻。
    // SAFETY: ImmDisableIME(u32::MAX) 仅改本进程输入法状态；须在 CreateWindowExW 前调用。
    unsafe {
        let _ = ImmDisableIME(u32::MAX);
    }

    if let Err(e) = register_window_class() {
        show_error(&e);
        return;
    }

    let hwnd = match create_main_window() {
        Ok(h) => h,
        Err(e) => {
            show_error(&e);
            return;
        }
    };

    init_network_listener(hwnd);

    // RegisterWindowMessageW 失败返回 0；窗口过程用 `msg == tcm && tcm != 0` 防御。
    let taskbar_msg = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if taskbar_msg == 0 {
        // SAFETY: 紧随 RegisterWindowMessageW，未插入其他可改写 last-error 的调用。
        let last = unsafe { GetLastError() };
        show_error(&format!("注册 TaskbarCreated 消息失败: 0x{:08X}", last.0));
    }
    TASKBAR_CREATED_MSG.store(taskbar_msg, Ordering::Release);

    // DEVICE_NOTIFY_WINDOW_HANDLE = 0；误用 SERVICE_HANDLE(1) 会把 HWND 当服务句柄，
    // 返回 ERROR_SERVICE_NOT_IN_EXE (0x8007043B)。
    let power_notify = unsafe {
        RegisterPowerSettingNotification(
            HANDLE(hwnd.0),
            &GUID_MONITOR_POWER_ON,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
    };
    match power_notify {
        Ok(handle) => {
            POWER_NOTIFY_HANDLE.store(handle.0, Ordering::Release);
        }
        Err(e) => {
            // 非致命：仅影响显示器开关节能。
            show_error(&format!("注册电源设置通知失败: {e:?}"));
        }
    }

    if !embed_in_taskbar(hwnd) {
        show_error("嵌入任务栏失败。请确认 explorer.exe 正在运行。");
        return;
    }

    let auto_update = load_auto_update_enabled();
    ENABLE_AUTO_UPDATE.store(auto_update, Ordering::Relaxed);

    create_tray_icon(hwnd);

    let renderer = match Renderer::new() {
        Ok(r) => r,
        Err(e) => {
            show_error(&format!("初始化渲染器失败: {e}"));
            remove_tray_icon();
            return;
        }
    };
    RENDERER.with(|r| {
        *r.borrow_mut() = Some(renderer);
    });

    RENDERER.with(|r| {
        if let Some(renderer) = r.borrow_mut().as_mut() {
            renderer.update_dpi(hwnd);
            renderer.update_text_color();
        }
    });

    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }

    if !sync_monitoring_timers(hwnd) {
        show_error("创建监测定时器失败");
        return;
    }

    // 失败非致命：锁屏暂停失效，显示器关闭仍可由电源通知覆盖。
    unsafe {
        if let Err(e) = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) {
            show_error(&format!("注册会话通知失败: {e:?}"));
        }
    }

    init_cleanup_temp();
    start_auto_check(hwnd);

    // 一次性定时器：10s 后 trim 初始化冷页；ID 99 不与监测定时器冲突。
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_ID_INIT_TRIM, 10000, None);
    }

    let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();

    // GetMessageW：>0 有消息；0 收到 WM_QUIT；-1 致命错误须退出。
    unsafe {
        loop {
            match windows::Win32::UI::WindowsAndMessaging::GetMessageW(&mut msg, None, 0, 0).0 {
                0 => break,
                -1 => {
                    let last = GetLastError();
                    show_error(&format!("消息循环 GetMessageW 致命错误: 0x{:08X}", last.0));
                    break;
                }
                _ => {
                    let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                    windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
                }
            }
        }
    }

    unsafe {
        let _ = WTSUnRegisterSessionNotification(hwnd);
    }

    let power_handle = POWER_NOTIFY_HANDLE.load(Ordering::Acquire);
    if power_handle != 0 {
        unsafe {
            let _ = UnregisterPowerSettingNotification(HPOWERNOTIFY(power_handle));
        }
    }

    RENDERER.with(|r| {
        let _ = r.borrow_mut().take();
    });
}

// --- 窗口过程与消息处理 ---

fn handle_taskbar_created(hwnd: HWND) -> LRESULT {
    invalidate_taskbar_cache();
    remove_tray_icon();
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
            show_error("Explorer 重启后恢复监测定时器失败");
        }
    }
    LRESULT(0)
}

fn handle_timer(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    match wparam.0 {
        TIMER_ID_INIT_TRIM => {
            trim_working_set();
            // KillTimer 对不存在的定时器仅返回错误，不触发 UB。
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
            if !is_suspended() && !MONITOR_FULLSCREEN.load(Ordering::Acquire) {
                update_taskbar_position(hwnd);
                collect_network();
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
        }
        TIMER_ID_CPU_MEM => {
            if !is_suspended() && !MONITOR_FULLSCREEN.load(Ordering::Acquire) {
                let _ = collect_cpu();
                collect_memory();
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
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
                // SAFETY: PBT_POWERSETTINGCHANGE 时 OS 保证 lparam 指向有效结构。
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
            // SAFETY: BeginPaint/EndPaint 必须配对。
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            RENDERER.with(|r| {
                if let Some(renderer) = r.borrow_mut().as_mut() {
                    renderer.render(hdc);
                }
            });
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
            // SAFETY: OS 保证 lparam 指向 NUL 结尾宽字符串（或 null）。
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

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
