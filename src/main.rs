#![windows_subsystem = "windows"]

mod collector;
mod config;
mod ffi_guard;
mod renderer;
mod tray;
mod update;
mod util;

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use windows::Win32::Foundation::{
    COLORREF, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST,
    MONITORINFOEXW, MonitorFromWindow, PAINTSTRUCT,
};
use windows::Win32::System::Power::{
    HPOWERNOTIFY, RegisterPowerSettingNotification, UnregisterPowerSettingNotification,
};
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::REGISTER_NOTIFICATION_FLAGS;
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, FindWindowExW, FindWindowW, GWL_EXSTYLE, GWL_STYLE, GetDesktopWindow,
    GetForegroundWindow, GetShellWindow, GetWindowLongPtrW, GetWindowRect, HWND_TOP, IsWindow,
    KillTimer, LWA_COLORKEY, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PostMessageW, PostQuitMessage,
    RegisterWindowMessageW, SW_HIDE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOZORDER,
    SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetParent, SetTimer, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_CREATE, WM_DPICHANGED,
    WM_PAINT, WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_TIMER, WM_WTSSESSION_CHANGE, WS_CHILD,
    WS_EX_LAYERED, WS_VISIBLE,
};
use windows::core::w;

use crate::collector::{
    WM_USER_NETWORK_DISCONNECTED, WM_USER_NETWORK_RECONNECTED, collect_cpu, collect_memory,
    collect_network, init_network_listener, trim_working_set,
};
use crate::config::{
    COLOR_KEY, CONSECUTIVE_ZERO_COUNT, DISPLAY_HEIGHT, DISPLAY_WIDTH, ENABLE_AUTO_UPDATE,
    FULLSCREEN, GAP, LOWORD_MASK, NETWORK_BACKOFF, SUSPENDED, TIMER_ID_CPU_MEM,
    TIMER_ID_FULLSCREEN, TIMER_ID_INIT_TRIM, TIMER_ID_NETWORK, TIMER_INTERVAL_FULLSCREEN,
    TIMER_INTERVAL_NETWORK, TIMER_INTERVAL_NETWORK_BACKOFF,
};
use crate::renderer::Renderer;
use crate::tray::{
    WM_APP_TRAY, create_main_window, create_tray_icon, register_window_class, remove_tray_icon,
};
use crate::update::{
    WM_USER_UPDATE_READY, init_cleanup_temp, load_auto_update_enabled, start_auto_check,
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
static TASKBAR_HWND: AtomicIsize = AtomicIsize::new(0);

fn get_taskbar_hwnd() -> Option<HWND> {
    let cached = TASKBAR_HWND.load(Ordering::Acquire);
    if cached != 0 {
        let hwnd = HWND(cached as *mut std::ffi::c_void);
        // SAFETY: IsWindow 是纯查询 API，hwnd 来自缓存，仅做有效性判断。
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Some(hwnd);
        }
        TASKBAR_HWND.store(0, Ordering::Release);
    }
    // SAFETY:
    // "Shell_TrayWnd" 是 Windows 任务栏窗口的标准类名，常量宽字符串生命周期覆盖调用。
    // FindWindowW 仅查询窗口句柄，不解引用任何裸指针，失败时安全返回 Err。
    let hwnd = unsafe { FindWindowW(w!("Shell_TrayWnd"), w!("")).ok() };
    if let Some(h) = hwnd {
        TASKBAR_HWND.store(h.0 as isize, Ordering::Release);
    }
    hwnd
}

// MutexGuard 定义已提取至 ffi_guard 模块中进行共用。

fn suspend_system(hwnd: HWND) {
    SUSPENDED.store(true, Ordering::Release);
    FULLSCREEN.store(false, Ordering::Release);
    // SAFETY:
    // hwnd 是操作系统分配的有效主窗口句柄。
    // 在系统休眠或锁屏时安全关闭所有监测定时器。
    unsafe {
        KillTimer(Some(hwnd), TIMER_ID_NETWORK).ok();
        KillTimer(Some(hwnd), TIMER_ID_CPU_MEM).ok();
        KillTimer(Some(hwnd), TIMER_ID_FULLSCREEN).ok();
    }
    trim_working_set();
}

fn resume_system(hwnd: HWND, reset_backoff: bool) {
    SUSPENDED.store(false, Ordering::Release);
    if reset_backoff {
        CONSECUTIVE_ZERO_COUNT.store(0, Ordering::Release);
        NETWORK_BACKOFF.store(false, Ordering::Release);
    }
    let network_interval = if NETWORK_BACKOFF.load(Ordering::Acquire) {
        TIMER_INTERVAL_NETWORK_BACKOFF
    } else {
        TIMER_INTERVAL_NETWORK
    };
    // SAFETY: hwnd 是系统分配的有效主窗口句柄。
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, network_interval, None);
        let _ = SetTimer(
            Some(hwnd),
            TIMER_ID_FULLSCREEN,
            TIMER_INTERVAL_FULLSCREEN,
            None,
        );
    }
    if !FULLSCREEN.load(Ordering::Acquire) {
        // SAFETY: hwnd 有效，定时器 ID 合法。
        unsafe {
            let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
        }
    }
}

/// Converts an ASCII string to a fixed-size UTF-16 array. Only works for ASCII; non-ASCII bytes
/// will produce incorrect results.
const fn utf16<const N: usize>(s: &str) -> [u16; N] {
    let mut buf = [0u16; N];
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

/// # Safety
///
/// 调用者必须保证 `lparam` 指向一个有效的、以 NUL 结尾的 UTF-16 宽字符序列。
/// 由 `WM_SETTINGCHANGE` 消息传入时 OS 保证此条件成立。
unsafe fn is_immersive_color_set(lparam: LPARAM) -> bool {
    let ptr = lparam.0 as *const u16;
    if ptr.is_null() {
        return false;
    }
    const EXPECTED: &[u16] = &utf16::<18>("ImmersiveColorSet\0");
    for (i, &expected_char) in EXPECTED.iter().enumerate() {
        // SAFETY: 调用者保证 ptr 指向有效的 NUL 结尾 UTF-16 序列，按偏移遍历安全。
        let actual_char = unsafe { *ptr.add(i) };
        if actual_char != expected_char {
            return false;
        }
        if actual_char == 0 {
            return true;
        }
    }
    true
}

fn check_fullscreen(hwnd: HWND) {
    // SAFETY: 纯查询 API，无副作用。
    let foreground = unsafe { GetForegroundWindow() };
    let is_invalid = foreground.is_invalid();
    // SAFETY: GetDesktopWindow 和 GetShellWindow 是纯查询 Win32 API，无副作用。
    let is_desktop_or_shell =
        unsafe { GetDesktopWindow() == foreground || GetShellWindow() == foreground };

    if is_invalid || is_desktop_or_shell || foreground == hwnd {
        let was = FULLSCREEN.load(Ordering::Acquire);
        if was {
            FULLSCREEN.store(false, Ordering::Release);
            // SAFETY: hwnd 是当前进程所持有并处于活动状态的有效主窗口句柄，重新启动此线程关联的定时器不会引发未定义行为。
            unsafe {
                let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
            }
        }
        return;
    }

    let mut rect = RECT::default();
    // SAFETY: foreground 非空，rect 在栈上分配。
    let _ = unsafe { GetWindowRect(foreground, &mut rect) };

    // 使用 MonitorFromWindow 获取前台窗口所在显示器
    // SAFETY: foreground 有效，MONITOR_DEFAULTTONEAREST 是合法标志。
    let hmon_fg = unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) };
    let mut mi_fg = MONITORINFOEXW::default();
    mi_fg.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: hmon_fg 有效，mi_fg 在栈上分配且 cbSize 已初始化。
    let fg_ok = unsafe { GetMonitorInfoW(hmon_fg, &mut mi_fg as *mut MONITORINFOEXW as *mut _) };

    let is_full = if fg_ok.as_bool() {
        let mon_rect = mi_fg.monitorInfo.rcMonitor;
        rect.left == mon_rect.left
            && rect.top == mon_rect.top
            && rect.right == mon_rect.right
            && rect.bottom == mon_rect.bottom
    } else {
        false
    };

    // 检查前台窗口是否覆盖任务栏所在显示器
    let same_monitor = match get_taskbar_hwnd() {
        Some(h_taskbar) => {
            // SAFETY: h_taskbar 有效。
            let hmon_tb = unsafe { MonitorFromWindow(h_taskbar, MONITOR_DEFAULTTONEAREST) };
            hmon_fg == hmon_tb
        }
        None => false,
    };

    let was = FULLSCREEN.load(Ordering::Acquire);
    let should_suspend = is_full && same_monitor;
    FULLSCREEN.store(should_suspend, Ordering::Release);

    if should_suspend && !was {
        // SAFETY: hwnd 有效，销毁定时器。
        unsafe {
            KillTimer(Some(hwnd), TIMER_ID_CPU_MEM).ok();
        }
    } else if !should_suspend && was {
        // SAFETY: hwnd 有效，重建定时器。
        unsafe {
            let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
        }
    }
}

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

    // SAFETY: hwnd 有效，创建初始定时器。
    unsafe {
        let _ = SetTimer(
            Some(hwnd),
            TIMER_ID_FULLSCREEN,
            TIMER_INTERVAL_FULLSCREEN,
            None,
        );
        let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, TIMER_INTERVAL_NETWORK, None);
        let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
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

fn calc_widget_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let h_taskbar = get_taskbar_hwnd()?;
    // SAFETY: h_taskbar 已被验证为有效句柄，"TrayNotifyWnd" 为系统 Tray 窗口类名。
    let h_tray = unsafe { FindWindowExW(Some(h_taskbar), None, w!("TrayNotifyWnd"), w!("")).ok()? };

    let mut rc_tray = RECT::default();
    let mut rc_taskbar = RECT::default();
    // SAFETY: h_tray 和 h_taskbar 有效，rect 在栈上分配。
    unsafe {
        GetWindowRect(h_tray, &mut rc_tray).ok()?;
        GetWindowRect(h_taskbar, &mut rc_taskbar).ok()?;
    }

    // SAFETY: hwnd 有效，GetDpiForWindow 是纯查询 API。
    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
    let scale = dpi as f64 / 96.0;
    let display_width = (DISPLAY_WIDTH as f64 * scale).round() as i32;
    let display_height = (DISPLAY_HEIGHT as f64 * scale).round() as i32;
    let gap = (GAP as f64 * scale).round() as i32;

    let display_x = rc_tray.left - rc_taskbar.left - gap - display_width;
    let display_y = (rc_taskbar.bottom - rc_taskbar.top - display_height) / 2;

    Some((display_x, display_y, display_width, display_height))
}

fn embed_in_taskbar(hwnd: HWND) -> bool {
    let (display_x, display_y, display_width, display_height) = match calc_widget_rect(hwnd) {
        Some(rect) => rect,
        None => {
            show_error("Cannot find Shell_TrayWnd or TrayNotifyWnd");
            return false;
        }
    };

    let h_taskbar = match get_taskbar_hwnd() {
        Some(h) => h,
        None => {
            show_error("Cannot find Shell_TrayWnd");
            return false;
        }
    };

    // SAFETY: hwnd 和 h_taskbar 均为已验证的有效句柄。
    unsafe {
        let _ = SetParent(hwnd, Some(h_taskbar));
        SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_CHILD.0 | WS_VISIBLE.0) as isize);
        let current_ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            current_ex_style | (WS_EX_LAYERED.0 as isize),
        );
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            display_x,
            display_y,
            display_width,
            display_height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        );
        if let Err(e) = SetLayeredWindowAttributes(hwnd, COLORREF(COLOR_KEY), 0, LWA_COLORKEY) {
            show_error(&format!("Failed to set layered window attributes: {:?}", e));
            return false;
        }
    }

    true
}

fn update_taskbar_position(hwnd: HWND) {
    thread_local! {
        static LAST_RECT: std::cell::Cell<Option<(i32, i32, i32, i32)>> = const { std::cell::Cell::new(None) };
    }

    let Some((display_x, display_y, display_width, display_height)) = calc_widget_rect(hwnd) else {
        return;
    };

    let changed = LAST_RECT.with(|lp| match lp.get() {
        Some((lx, ly, lw, lh))
            if lx == display_x
                && ly == display_y
                && lw == display_width
                && lh == display_height =>
        {
            false
        }
        _ => {
            lp.set(Some((display_x, display_y, display_width, display_height)));
            true
        }
    });

    if changed {
        // SAFETY: hwnd 有效，SWP_NOZORDER 不调整层级。
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                display_x,
                display_y,
                display_width,
                display_height,
                SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_NOZORDER,
            );
        }
    }
}

const WTS_SESSION_LOCK: usize = 0x7;
const WTS_SESSION_UNLOCK: usize = 0x8;

fn handle_taskbar_created(hwnd: HWND) -> LRESULT {
    TASKBAR_HWND.store(0, Ordering::Release);
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

        let network_interval = if NETWORK_BACKOFF.load(Ordering::Acquire) {
            TIMER_INTERVAL_NETWORK_BACKOFF
        } else {
            TIMER_INTERVAL_NETWORK
        };
        // SAFETY: hwnd 有效，重建网络定时器。
        unsafe {
            let _ = SetTimer(
                Some(hwnd),
                TIMER_ID_FULLSCREEN,
                TIMER_INTERVAL_FULLSCREEN,
                None,
            );
            let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, network_interval, None);
        }
        if !SUSPENDED.load(Ordering::Acquire) && !FULLSCREEN.load(Ordering::Acquire) {
            // SAFETY: hwnd 有效，重建 CPU/内存定时器。
            unsafe {
                let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
            }
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
            if !SUSPENDED.load(Ordering::Acquire) {
                check_fullscreen(hwnd);
            }
        }
        TIMER_ID_NETWORK => {
            if !SUSPENDED.load(Ordering::Acquire) && !FULLSCREEN.load(Ordering::Acquire) {
                update_taskbar_position(hwnd);
                collect_network();
                // SAFETY: hwnd 有效，刷新网速显示。
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
        }
        TIMER_ID_CPU_MEM => {
            if !SUSPENDED.load(Ordering::Acquire) && !FULLSCREEN.load(Ordering::Acquire) {
                collect_cpu();
                collect_memory();
                // SAFETY: hwnd 有效，刷新 CPU/内存显示。
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
            suspend_system(hwnd);
        }
        PBT_APMRESUMEAUTOMATIC => {
            resume_system(hwnd, true);
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
                        resume_system(hwnd, true);
                    } else {
                        suspend_system(hwnd);
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
            suspend_system(hwnd);
        }
        WTS_SESSION_UNLOCK => {
            resume_system(hwnd, true);
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
            if !SUSPENDED.load(Ordering::Acquire) {
                // SAFETY: hwnd 有效，切换到退避间隔。
                unsafe {
                    let _ = SetTimer(
                        Some(hwnd),
                        TIMER_ID_NETWORK,
                        TIMER_INTERVAL_NETWORK_BACKOFF,
                        None,
                    );
                }
            }
            LRESULT(0)
        }

        WM_USER_NETWORK_RECONNECTED => {
            NETWORK_BACKOFF.store(false, Ordering::Release);
            CONSECUTIVE_ZERO_COUNT.store(0, Ordering::Release);
            if !SUSPENDED.load(Ordering::Acquire) {
                // SAFETY: hwnd 有效，恢复默认网速采集间隔。
                unsafe {
                    let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, TIMER_INTERVAL_NETWORK, None);
                }
            }
            start_auto_check(hwnd);
            LRESULT(0)
        }

        WM_USER_UPDATE_READY => {
            let status = wparam.0;
            crate::update::handle_update_ready(hwnd, status);
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
