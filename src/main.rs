#![windows_subsystem = "windows"]

mod collector;
mod config;
mod mouse_hid;
mod renderer;
mod tray;

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM, CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::System::Power::{
    RegisterPowerSettingNotification, UnregisterPowerSettingNotification,
    HPOWERNOTIFY,
};
use windows::Win32::UI::WindowsAndMessaging::REGISTER_NOTIFICATION_FLAGS;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, FindWindowExW, FindWindowW, GetDesktopWindow, GetForegroundWindow,
    GetShellWindow, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, KillTimer,
    MessageBoxW, PostMessageW, PostQuitMessage, RegisterWindowMessageW, SetLayeredWindowAttributes,
    SetParent, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    HWND_TOP, GWL_EXSTYLE, GWL_STYLE, LWA_COLORKEY, MB_ICONERROR, MB_OK, SM_CXSCREEN, SM_CYSCREEN,
    SW_HIDE, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, WS_CHILD, WS_EX_LAYERED, WS_VISIBLE, SWP_FRAMECHANGED,
    WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DPICHANGED, WM_PAINT, WM_POWERBROADCAST,
    WM_CONTEXTMENU, WM_SETTINGCHANGE, WM_TIMER, WM_WTSSESSION_CHANGE,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND,
};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};

use crate::collector::{collect_cpu, collect_memory, collect_network, init_network_listener, trim_working_set,
    WM_USER_NETWORK_DISCONNECTED, WM_USER_NETWORK_RECONNECTED};
use crate::config::{
    COLOR_KEY, DISPLAY_HEIGHT, DISPLAY_WIDTH, FULLSCREEN, GAP, SUSPENDED,
    TIMER_ID_CPU_MEM, TIMER_ID_NETWORK, TIMER_INTERVAL_NETWORK, TIMER_INTERVAL_NETWORK_BACKOFF,
    SHOW_MOUSE_INFO, MENU_ID_SHOW_MOUSE, MENU_ID_RESTART_HID, MOUSE_ONLINE,
    MOUSE_BATTERY_LEVEL, MOUSE_DPI_VALUE, NETWORK_BACKOFF, CONSECUTIVE_ZERO_COUNT,
};
use crate::mouse_hid::{start_mouse_thread, stop_mouse_thread, check_mouse_available, WM_USER_MOUSE_UPDATE, WM_USER_MOUSE_STATUS};
use crate::renderer::Renderer;
use crate::tray::{create_main_window, create_tray_icon, register_window_class, remove_tray_icon, WM_APP_TRAY};

const PBT_POWERSETTINGCHANGE: u32 = 0x8013;
const DEVICE_NOTIFY_WINDOW_HANDLE: u32 = 1;

const GUID_MONITOR_POWER_ON: windows::core::GUID = windows::core::GUID::from_values(
    0x02731015, 0x4510, 0x4526, [0x99, 0xE6, 0xE5, 0xA1, 0x7E, 0xBD, 0x1A, 0xEA],
);

#[repr(C)]
#[allow(non_snake_case)]
struct POWERBROADCAST_SETTING {
    PowerSetting: windows::core::GUID,
    DataLength: u32,
    Data: [u8; 1],
}

thread_local! {
    static RENDERER: RefCell<Option<Renderer>> = RefCell::new(None);
    static MOUSE_THREAD: RefCell<Option<std::thread::JoinHandle<()>>> = RefCell::new(None);
}

static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);
static POWER_NOTIFY_HANDLE: AtomicIsize = AtomicIsize::new(0);

struct MutexGuard(windows::Win32::Foundation::HANDLE);
impl Drop for MutexGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: self.0 是由 CreateMutexW 成功创建的有效互斥量句柄，在生命周期结束时自动关闭。
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

fn show_error(msg: &str) {
    let title: Vec<u16> = "Traffic Monitor\0".encode_utf16().collect();
    let msg_wide: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY:
    // title 和 msg_wide 均为当前分配的以 NUL 结尾的合法宽字符数组指针。
    // MessageBoxW 将安全弹出窗口通知用户。
    unsafe {
        MessageBoxW(
            None,
            windows::core::PCWSTR(msg_wide.as_ptr()),
            windows::core::PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn stop_and_join_mouse_thread() {
    stop_mouse_thread();
    MOUSE_THREAD.with(|m| {
        if let Some(handle) = m.borrow_mut().take() {
            let _ = handle.join();
        }
    });
    trim_working_set();
}

fn suspend_system(hwnd: HWND) {
    SUSPENDED.store(true, Ordering::Release);
    // SAFETY:
    // hwnd 是操作系统分配的有效主窗口句柄。
    // 在系统休眠或锁屏时安全关闭两个监测定时器。
    unsafe {
        KillTimer(Some(hwnd), TIMER_ID_NETWORK).ok();
        KillTimer(Some(hwnd), TIMER_ID_CPU_MEM).ok();
    }
    stop_and_join_mouse_thread();
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
    // SAFETY:
    // hwnd 是系统分配的有效主窗口句柄。
    // 重新启动系统监测的定时器。
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, network_interval, None);
    }
    if !FULLSCREEN.load(Ordering::Acquire) {
        // SAFETY: 同上。
        unsafe {
            let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
        }
        if SHOW_MOUSE_INFO.load(Ordering::Acquire) {
            stop_and_join_mouse_thread();
            MOUSE_THREAD.with(|m| {
                *m.borrow_mut() = Some(start_mouse_thread());
            });
        }
    }
}

fn is_immersive_color_set(lparam: LPARAM) -> bool {
    let ptr = lparam.0 as *const u16;
    if ptr.is_null() {
        return false;
    }
    let expected: Vec<u16> = "ImmersiveColorSet\0".encode_utf16().collect();
    for (i, &expected_char) in expected.iter().enumerate() {
        // SAFETY: 当 lparam 由 WM_SETTINGCHANGE 消息触发时，OS 保证 lparam 指向合法的以 0 结尾的宽字符串。
        // 我们在此严格按偏移遍历，并在遇到 NUL 终止符时立即返回，因此不会发生内存越界访问。
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
    // SAFETY: GetForegroundWindow, GetDesktopWindow, GetShellWindow 均为安全的 Win32 查询操作。
    let foreground = unsafe { GetForegroundWindow() };
    let is_invalid = foreground.is_invalid();
    let is_desktop_or_shell = unsafe { GetDesktopWindow() == foreground || GetShellWindow() == foreground };

    if is_invalid
        || is_desktop_or_shell
        || foreground == hwnd
    {
        let was = FULLSCREEN.load(Ordering::Acquire);
        if was {
            FULLSCREEN.store(false, Ordering::Release);
            // SAFETY: 重新启动 CPU/内存监测定时器。
            unsafe {
                let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
            }
            if SHOW_MOUSE_INFO.load(Ordering::Acquire) {
                stop_and_join_mouse_thread();
                MOUSE_THREAD.with(|m| {
                    *m.borrow_mut() = Some(start_mouse_thread());
                });
            }
        }
        return;
    }

    let mut rect = RECT::default();
    // SAFETY: foreground 是有效的窗口句柄，GetWindowRect 能够安全地获取其屏幕矩形。
    let _ = unsafe { GetWindowRect(foreground, &mut rect) };
    
    // SAFETY: 获取系统主显示器分辨率参数。
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let is_full = (rect.right - rect.left) == screen_w
        && (rect.bottom - rect.top) == screen_h;

    let was = FULLSCREEN.load(Ordering::Acquire);
    FULLSCREEN.store(is_full, Ordering::Release);

    if is_full && !was {
        // SAFETY: 前台程序全屏，安全暂停定时器与鼠标轮询。
        unsafe {
            KillTimer(Some(hwnd), TIMER_ID_CPU_MEM).ok();
        }
        stop_and_join_mouse_thread();
    } else if !is_full && was {
        // SAFETY: 恢复定时器。
        unsafe {
            let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
        }
        if SHOW_MOUSE_INFO.load(Ordering::Acquire) {
            stop_and_join_mouse_thread();
            MOUSE_THREAD.with(|m| {
                *m.borrow_mut() = Some(start_mouse_thread());
            });
        }
    }
}

fn quit_existing_instance() {
    let class_name: Vec<u16> = crate::config::WINDOW_CLASS.encode_utf16().collect();
    // SAFETY: 查找先前已经创建且运行着的实例窗口句柄。
    let hwnd = unsafe {
        FindWindowW(
            windows::core::PCWSTR(class_name.as_ptr()),
            windows::core::PCWSTR(std::ptr::null()),
        )
    };

    if let Ok(h) = hwnd {
        if !h.is_invalid() {
            // SAFETY: 发送 WM_CLOSE 消息退出之前的进程实例。
            unsafe {
                let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                // SAFETY: 检测原先窗口是否已经被注销。
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
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--quit") {
        quit_existing_instance();
        return;
    }

    let mutex_name: Vec<u16> = crate::config::MUTEX_NAME.encode_utf16().collect();
    
    // SAFETY:
    // 创建全局命名的进程互斥体，用于单例模式运行检测。
    let mutex_handle = unsafe {
        CreateMutexW(None, true, windows::core::PCWSTR(mutex_name.as_ptr()))
    };
    
    let _mutex_guard = match mutex_handle {
        Ok(handle) => {
            // SAFETY: 如果互斥量已存在，关闭句柄并立即退出。
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                return;
            }
            MutexGuard(handle)
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

    mouse_hid::init(hwnd);
    init_network_listener(hwnd);

    // SAFETY: 注册 explorer 重启的系统全局窗口消息。
    let taskbar_msg = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    TASKBAR_CREATED_MSG.store(taskbar_msg, Ordering::Release);

    // SAFETY: 注册系统电源电源通知。
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

    // SAFETY: 使窗口矩形失效以触发 WM_PAINT 绘制。
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }

    // SAFETY: 设置定时器。
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, TIMER_INTERVAL_NETWORK, None);
        let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
    }

    if SHOW_MOUSE_INFO.load(Ordering::Acquire) {
        MOUSE_THREAD.with(|m| {
            *m.borrow_mut() = Some(start_mouse_thread());
        });
    }

    // SAFETY: 注册当前窗口的终端会话锁定/解锁通知。
    unsafe {
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
    }

    let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
    
    // SAFETY: 运行标准的 Windows 消息循环。
    unsafe {
        while windows::Win32::UI::WindowsAndMessaging::GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
            windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
        }
    }

    stop_and_join_mouse_thread();
    
    // SAFETY: 注销会话锁通知。
    unsafe {
        let _ = WTSUnRegisterSessionNotification(hwnd);
    }

    let power_handle = POWER_NOTIFY_HANDLE.load(Ordering::Acquire);
    if power_handle != 0 {
        // SAFETY: 注销电源广播通知。
        unsafe {
            let _ = UnregisterPowerSettingNotification(HPOWERNOTIFY(power_handle));
        }
    }

    RENDERER.with(|r| {
        let _ = r.borrow_mut().take();
    });
}

fn calc_widget_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    // SAFETY: 寻找任务栏的 Shell 窗口和 TrayNotify 窗口。
    let h_taskbar = unsafe { FindWindowW(w!("Shell_TrayWnd"), w!("")).ok()? };
    let h_tray = unsafe { FindWindowExW(Some(h_taskbar), None, w!("TrayNotifyWnd"), w!("")).ok()? };

    let mut rc_tray = RECT::default();
    let mut rc_taskbar = RECT::default();
    // SAFETY: 获取相应窗口的屏幕坐标矩形。
    unsafe {
        GetWindowRect(h_tray, &mut rc_tray).ok()?;
        GetWindowRect(h_taskbar, &mut rc_taskbar).ok()?;
    }

    // SAFETY: 获取当前窗口的 DPI 参数。
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

    // SAFETY: 寻找 Shell_TrayWnd。
    let h_taskbar = match unsafe { FindWindowW(w!("Shell_TrayWnd"), w!("")) } {
        Ok(h) => h,
        Err(_) => {
            show_error("Cannot find Shell_TrayWnd");
            return false;
        }
    };

    // SAFETY:
    // 将该主窗口设置为任务栏的子窗口。这允许我们将组件真正嵌入到任务栏中。
    unsafe {
        let _ = SetParent(hwnd, Some(h_taskbar));
    }

    // SAFETY: 将窗口样式更改为子窗口。
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_CHILD.0 | WS_VISIBLE.0) as isize);
    }

    // SAFETY: 恢复透明图层扩展样式。
    let current_ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, current_ex_style | (WS_EX_LAYERED.0 as isize));
    }

    // SAFETY: 移动并调整窗口位置。
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            display_x,
            display_y,
            display_width,
            display_height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        );
    }

    // SAFETY: 设定窗口透明颜色键以使背景透明。
    if let Err(e) = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(COLOR_KEY), 0, LWA_COLORKEY) } {
        show_error(&format!("Failed to set layered window attributes: {:?}", e));
    }

    true
}

fn update_taskbar_position(hwnd: HWND) {
    thread_local! {
        static LAST_RECT: std::cell::Cell<Option<(i32, i32, i32, i32)>> = std::cell::Cell::new(None);
    }

    let Some((display_x, display_y, display_width, display_height)) = calc_widget_rect(hwnd) else {
        return;
    };

    let changed = LAST_RECT.with(|lp| {
        match lp.get() {
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
        }
    });

    if changed {
        // SAFETY: 当检测到任务栏图标被增减或分辨率变化导致坐标需要修正时，安全移动窗口。
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

pub extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let tcm = TASKBAR_CREATED_MSG.load(Ordering::Acquire);
    if msg == tcm && tcm != 0 {
        remove_tray_icon();
        // SAFETY: 隐藏当前窗口。
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
            
            // SAFETY: 设置定时器。
            unsafe {
                let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, TIMER_INTERVAL_NETWORK, None);
            }
            if !SUSPENDED.load(Ordering::Acquire) && !FULLSCREEN.load(Ordering::Acquire) {
                // SAFETY: 设置硬件读取定时器。
                unsafe {
                    let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
                }
            }
            if SHOW_MOUSE_INFO.load(Ordering::Acquire)
                && !SUSPENDED.load(Ordering::Acquire)
                && !FULLSCREEN.load(Ordering::Acquire)
            {
                stop_and_join_mouse_thread();
                MOUSE_THREAD.with(|m| {
                    *m.borrow_mut() = Some(start_mouse_thread());
                });
            }
        }
        return LRESULT(0);
    }

    match msg {
        WM_CREATE => LRESULT(0),

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: 传入 hwnd 获得该窗口绘图上下文并将其传给 renderer 进行双缓冲绘制。
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            RENDERER.with(|r| {
                if let Some(renderer) = r.borrow().as_ref() {
                    renderer.render(hdc);
                }
            });
            // SAFETY: 安全终止绘制过程。
            unsafe {
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }

        WM_TIMER => {
            match wparam.0 {
                TIMER_ID_NETWORK => {
                    check_fullscreen(hwnd);
                    if !SUSPENDED.load(Ordering::Acquire) && !FULLSCREEN.load(Ordering::Acquire) {
                        update_taskbar_position(hwnd);
                        collect_network();
                        // SAFETY: 触发窗口重绘刷新网速数值。
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                }
                TIMER_ID_CPU_MEM => {
                    if !SUSPENDED.load(Ordering::Acquire) && !FULLSCREEN.load(Ordering::Acquire) {
                        collect_cpu();
                        collect_memory();
                        // SAFETY: 触发窗口重绘刷新硬件数值。
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }

        WM_USER_MOUSE_UPDATE | WM_USER_MOUSE_STATUS => {
            // SAFETY: 触发主窗口重绘以显示更新后或下线的鼠标 DPI/电量。
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }

        WM_USER_NETWORK_DISCONNECTED => {
            if !SUSPENDED.load(Ordering::Acquire) {
                // SAFETY: 发生零流量退避，定时器降频为 backoff 时间间隔。
                unsafe {
                    let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, TIMER_INTERVAL_NETWORK_BACKOFF, None);
                }
            }
            LRESULT(0)
        }

        WM_USER_NETWORK_RECONNECTED => {
            NETWORK_BACKOFF.store(false, Ordering::Release);
            CONSECUTIVE_ZERO_COUNT.store(0, Ordering::Release);
            if !SUSPENDED.load(Ordering::Acquire) {
                // SAFETY: 流量恢复，设置定时器为原本的 1000ms 默认间隔。
                unsafe {
                    let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, TIMER_INTERVAL_NETWORK, None);
                }
            }
            LRESULT(0)
        }

        WM_SETTINGCHANGE => {
            if is_immersive_color_set(lparam) {
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

        WM_POWERBROADCAST => {
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
                        // SAFETY:
                        // 当 wparam 为 PBT_POWERSETTINGCHANGE 时，OS 保证其 lparam 为有效的
                        // POWERBROADCAST_SETTING 指针。检测 DataLength 长度后解引用 Data 数据是安全的。
                        let setting_ref = unsafe { &*setting };
                        if setting_ref.PowerSetting == GUID_MONITOR_POWER_ON && setting_ref.DataLength >= 1 {
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

        WM_WTSSESSION_CHANGE => {
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

        WM_CLOSE => {
            remove_tray_icon();
            stop_mouse_thread();
            // SAFETY: 发送退出信号终止 GetMessageW 消息泵。
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }

        WM_COMMAND => {
            let menu_id = (wparam.0 & 0xFFFF) as u32;
            if menu_id == MENU_ID_SHOW_MOUSE {
                let current_state = SHOW_MOUSE_INFO.load(Ordering::Acquire);
                if !current_state {
                    if check_mouse_available() {
                        SHOW_MOUSE_INFO.store(true, Ordering::Release);
                        stop_and_join_mouse_thread();
                        MOUSE_THREAD.with(|m| {
                            *m.borrow_mut() = Some(start_mouse_thread());
                        });
                        // SAFETY: 触发重画。
                        unsafe {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    } else {
                        show_error("未检测到物理鼠标或鼠标不支持");
                    }
                } else {
                    SHOW_MOUSE_INFO.store(false, Ordering::Release);
                    stop_and_join_mouse_thread();
                    MOUSE_ONLINE.store(false, Ordering::Release);
                    // SAFETY: 触发重画。
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            } else if menu_id == MENU_ID_RESTART_HID {
                if SHOW_MOUSE_INFO.load(Ordering::Acquire) {
                    stop_and_join_mouse_thread();
                    MOUSE_ONLINE.store(false, Ordering::Release);
                    MOUSE_BATTERY_LEVEL.store(0, Ordering::Release);
                    MOUSE_DPI_VALUE.store(0, Ordering::Release);
                    MOUSE_THREAD.with(|m| {
                        *m.borrow_mut() = Some(start_mouse_thread());
                    });
                    // SAFETY: 触发重画。
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            } else {
                tray::handle_menu_command(hwnd, menu_id);
            }
            LRESULT(0)
        }

        x if x == WM_APP_TRAY => {
            let event = (lparam.0 & 0xFFFF) as u32;
            if event == WM_CONTEXTMENU {
                tray::show_context_menu(hwnd);
            }
            LRESULT(0)
        }

        // SAFETY: 交由默认过程处理消息。
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
