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
    DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DestroyWindow, FindWindowW, IsWindow, KillTimer,
    PostMessageW, PostQuitMessage, RegisterWindowMessageW, SetTimer, WM_CLOSE, WM_COMMAND,
    WM_CONTEXTMENU, WM_CREATE, WM_DPICHANGED, WM_PAINT, WM_POWERBROADCAST, WM_SETTINGCHANGE,
    WM_TIMER, WM_WTSSESSION_CHANGE,
};
use windows::core::{PCWSTR, w};

use crate::collector::{collect_cpu, collect_memory, collect_network};
use crate::config::{
    LOWORD_MASK, RELAUNCHED_BY_UPDATE_ARG, TIMER_ID_AUTO_UPDATE, TIMER_ID_CPU_MEM,
    TIMER_ID_FULLSCREEN, TIMER_ID_INIT_TRIM, TIMER_ID_MEMORY_MAINTENANCE, TIMER_ID_NETWORK,
    TIMER_INTERVAL_INIT_TRIM, WM_APP_TRAY, WM_USER_NETWORK_DISCONNECTED,
    WM_USER_NETWORK_RECONNECTED, WM_USER_UPDATE_ACTION,
};
use crate::renderer::Renderer;
use crate::state::{ENABLE_AUTO_UPDATE, MONITOR_FULLSCREEN, reset_network_backoff};
use crate::suspend::{
    check_fullscreen, handle_power_broadcast, handle_session_change, is_immersive_color_set,
    is_suspended, sync_monitoring_timers,
};
use crate::tray::{create_tray_icon, remove_tray_icon};
use crate::update::{
    defer_initial_auto_check, init_cleanup_temp, load_auto_update_enabled, start_auto_check,
    subprocess_main,
};
use crate::util::{
    set_low_memory_priority, show_error, trim_working_set, trim_working_set_if_needed,
};
use crate::window::{
    create_main_window, create_watchdog_window, embed_in_taskbar, invalidate_taskbar_cache,
    register_watchdog_class, register_window_class, update_taskbar_position,
};

static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);
static POWER_NOTIFY_HANDLE: AtomicIsize = AtomicIsize::new(0);
/// 当前主窗口句柄（isize）。Explorer 重启重建后更新；0 表示暂无主窗口。
static CURRENT_MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

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

    // 主进程常驻期间保持低内存优先级：内存紧张时 OS 优先回收本进程页面，
    // 不影响 CPU 调度与核心选择；EcoQoS 仍只用于 --check-update 子进程。
    set_low_memory_priority();

    // 首个顶层窗口创建前禁用进程 IME，避免更新弹窗焦点回落触发第三方 TSF 常驻。
    // SAFETY: ImmDisableIME(u32::MAX) 仅改本进程输入法状态；须在 CreateWindowExW 前调用。
    unsafe {
        let _ = ImmDisableIME(u32::MAX);
    }

    if let Err(e) = register_window_class() {
        show_error(&e);
        return;
    }

    // 看门狗是首个创建的顶层窗口；ImmDisableIME 已在其之前执行。
    if let Err(e) = register_watchdog_class() {
        show_error(&e);
        return;
    }

    // RegisterWindowMessageW 失败返回 0；看门狗过程用 `tcm != 0 && msg == tcm` 防御。
    // 必须在看门狗窗口创建前注册，保证其能收到首次 TaskbarCreated 广播。
    let taskbar_msg = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if taskbar_msg == 0 {
        // SAFETY: 紧随 RegisterWindowMessageW，未插入其他可改写 last-error 的调用。
        let last = unsafe { GetLastError() };
        show_error(&format!("注册 TaskbarCreated 消息失败: 0x{:08X}", last.0));
    }
    TASKBAR_CREATED_MSG.store(taskbar_msg, Ordering::Release);

    if let Err(e) = create_watchdog_window() {
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
    CURRENT_MAIN_HWND.store(hwnd.0 as isize, Ordering::Release);

    register_power_notify(hwnd);

    // 嵌入失败不中止启动：看门狗会在 TaskbarCreated 广播时再次尝试嵌入，
    // 此处先完成托盘/定时器等其余常驻功能，避免留下零定时器的活窗口。
    if !embed_in_taskbar(hwnd) {
        show_error("嵌入任务栏失败。请确认 explorer.exe 正在运行。");
    }

    let auto_update = load_auto_update_enabled();
    ENABLE_AUTO_UPDATE.store(auto_update, Ordering::Relaxed);

    match Renderer::new() {
        Ok(r) => renderer::set_renderer(r),
        Err(e) => {
            show_error(&format!("初始化渲染器失败: {e}"));
            remove_tray_icon();
            return;
        }
    }

    if !bind_display_and_timers(hwnd) {
        show_error("创建监测定时器失败");
        return;
    }

    register_session_notification(hwnd);

    init_cleanup_temp();
    // 更新流程 relaunch 拉起的进程：刚发生过 UAC 取消或安装器启动失败，
    // 推迟首个自动检查周期，避免立刻再弹同一版本的更新确认框。
    if args.iter().any(|a| a == RELAUNCHED_BY_UPDATE_ARG) {
        defer_initial_auto_check();
    }
    start_auto_check(hwnd);

    // 一次性定时器：到时后 trim 初始化冷页；ID 99 不与监测定时器冲突。
    unsafe {
        let _ = SetTimer(
            Some(hwnd),
            TIMER_ID_INIT_TRIM,
            TIMER_INTERVAL_INIT_TRIM,
            None,
        );
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

    // 注销须针对当前主窗口：Explorer 重启重建后原局部 hwnd 已陈旧。
    let current = CURRENT_MAIN_HWND.load(Ordering::Acquire);
    if current != 0 {
        unsafe {
            let _ = WTSUnRegisterSessionNotification(HWND(current as *mut std::ffi::c_void));
        }
    }

    let power_handle = POWER_NOTIFY_HANDLE.load(Ordering::Acquire);
    if power_handle != 0 {
        unsafe {
            let _ = UnregisterPowerSettingNotification(HPOWERNOTIFY(power_handle));
        }
    }

    renderer::take_renderer();
}

// --- 看门狗窗口与 Explorer 重启恢复 ---

/// 注册显示器开关电源通知到指定窗口。失败仅影响显示器开关节能（非致命）。
fn register_power_notify(hwnd: HWND) {
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
            show_error(&format!("注册电源设置通知失败: {e:?}"));
        }
    }
}

/// 注册会话锁屏通知。失败非致命：锁屏暂停失效，显示器关闭仍由电源通知覆盖。
fn register_session_notification(hwnd: HWND) {
    if let Err(e) = unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } {
        show_error(&format!("注册会话通知失败: {e:?}"));
    }
}

/// 启动与 Explorer 重建共用的资源绑定尾段：托盘图标 → 渲染参数 → 窗口失效
/// → 监测定时器。两条生命周期路径保持唯一实现，失败文案由各自调用方报告。
fn bind_display_and_timers(hwnd: HWND) -> bool {
    create_tray_icon(hwnd);

    renderer::with_renderer(|r| {
        r.update_dpi(hwnd);
        r.update_text_color();
    });

    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }

    sync_monitoring_timers(hwnd)
}

/// Explorer 重启后的主窗口完整重建。
///
/// 不变量：任务栏销毁会级联销毁嵌入其中的跨进程子窗口（旧主窗口已死），
/// 且 TaskbarCreated 广播只投递顶层窗口——因此重建只能由看门狗触发，
/// 禁止把该处理挂回主窗口过程。所有绑定在旧 hwnd 上的资源
/// （电源/会话通知、托盘、定时器）必须逐一重绑到新 hwnd；
/// 网络采样由 WM_TIMER tick 携带的 hwnd 直接投递，无需重绑。
fn rebuild_main_window() {
    invalidate_taskbar_cache();

    let old = CURRENT_MAIN_HWND.swap(0, Ordering::AcqRel);
    if old != 0 {
        let old_hwnd = HWND(old as *mut std::ffi::c_void);
        // SAFETY: 主窗口与看门狗同在 UI 线程创建；IsWindow 过滤陈旧句柄后销毁安全。
        if unsafe { IsWindow(Some(old_hwnd)) }.as_bool() {
            unsafe {
                let _ = DestroyWindow(old_hwnd);
            }
        }
    }

    let hwnd = match create_main_window() {
        Ok(h) => h,
        Err(e) => {
            show_error(&format!("Explorer 重启后重建主窗口失败: {e}"));
            return;
        }
    };
    CURRENT_MAIN_HWND.store(hwnd.0 as isize, Ordering::Release);

    // 旧电源通知绑定在已销毁的窗口上，先注销再对新窗口重新注册。
    let prev_power = POWER_NOTIFY_HANDLE.swap(0, Ordering::AcqRel);
    if prev_power != 0 {
        unsafe {
            let _ = UnregisterPowerSettingNotification(HPOWERNOTIFY(prev_power));
        }
    }
    register_power_notify(hwnd);
    register_session_notification(hwnd);

    remove_tray_icon();

    // 嵌入失败同样不中止恢复：托盘与定时器必须重建，嵌入交给后续广播重试。
    if !embed_in_taskbar(hwnd) {
        show_error("Explorer 重启后嵌入任务栏失败");
    }

    if !bind_display_and_timers(hwnd) {
        show_error("Explorer 重启后恢复监测定时器失败");
    }
}

pub extern "system" fn watchdog_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let tcm = TASKBAR_CREATED_MSG.load(Ordering::Acquire);
    if tcm != 0 && msg == tcm {
        rebuild_main_window();
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
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
                // 任务栏通知区域变化时，即使数值不变也必须刷新新位置的画布。
                let position_changed = update_taskbar_position(hwnd);
                collect_network(hwnd);
                if position_changed {
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                } else {
                    renderer::invalidate_if_values_changed(hwnd);
                }
            }
        }
        TIMER_ID_CPU_MEM => {
            if !is_suspended() && !MONITOR_FULLSCREEN.load(Ordering::Acquire) {
                collect_cpu();
                collect_memory();
                renderer::invalidate_if_values_changed(hwnd);
            }
        }
        TIMER_ID_AUTO_UPDATE if !is_suspended() && !MONITOR_FULLSCREEN.load(Ordering::Acquire) => {
            start_auto_check(hwnd);
        }
        TIMER_ID_MEMORY_MAINTENANCE
            if !is_suspended() && !MONITOR_FULLSCREEN.load(Ordering::Acquire) =>
        {
            trim_working_set_if_needed();
        }
        _ => {}
    }
    LRESULT(0)
}

pub extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => LRESULT(0),

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: BeginPaint/EndPaint 必须配对。
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            renderer::with_renderer(|r| r.render(hdc));
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
            reset_network_backoff();
            let _ = sync_monitoring_timers(hwnd);
            start_auto_check(hwnd);
            LRESULT(0)
        }

        WM_USER_UPDATE_ACTION => {
            crate::update::handle_update_action();
            LRESULT(0)
        }

        WM_SETTINGCHANGE => {
            // SAFETY: OS 保证 lparam 指向 NUL 结尾宽字符串（或 null）。
            if unsafe { is_immersive_color_set(lparam) } {
                renderer::with_renderer(|r| r.update_text_color());
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }

        WM_DPICHANGED => {
            renderer::with_renderer(|r| r.update_dpi(hwnd));
            let _ = embed_in_taskbar(hwnd);
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
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

        WM_APP_TRAY => {
            let event = (lparam.0 as u32) & LOWORD_MASK;
            if event == WM_CONTEXTMENU {
                tray::show_context_menu(hwnd);
            }
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
