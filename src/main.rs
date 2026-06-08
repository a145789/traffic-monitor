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

const GUID_MONITOR_POWER_ON: windows::core::GUID = windows::core::GUID::from_u128(0x0273b28b2d604396a078d5f143136a7e);

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

fn show_error(msg: &str) {
    unsafe {
        let title: Vec<u16> = "Traffic Monitor\0".encode_utf16().collect();
        let msg_wide: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
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
    SUSPENDED.store(true, Ordering::Relaxed);
    unsafe {
        KillTimer(Some(hwnd), TIMER_ID_NETWORK).ok();
        KillTimer(Some(hwnd), TIMER_ID_CPU_MEM).ok();
    }
    stop_and_join_mouse_thread();
}

fn resume_system(hwnd: HWND) {
    SUSPENDED.store(false, Ordering::Relaxed);
    CONSECUTIVE_ZERO_COUNT.store(0, Ordering::Relaxed);
    NETWORK_BACKOFF.store(false, Ordering::Relaxed);
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, TIMER_INTERVAL_NETWORK, None);
        let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
    }
    if SHOW_MOUSE_INFO.load(Ordering::Relaxed) && !FULLSCREEN.load(Ordering::Relaxed) {
        stop_and_join_mouse_thread();
        MOUSE_THREAD.with(|m| {
            *m.borrow_mut() = Some(start_mouse_thread());
        });
    }
}

fn is_immersive_color_set(lparam: LPARAM) -> bool {
    let ptr = lparam.0 as *const u16;
    if ptr.is_null() {
        return false;
    }
    let expected: Vec<u16> = "ImmersiveColorSet\0".encode_utf16().collect();
    for (i, &expected_char) in expected.iter().enumerate() {
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
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_invalid()
            || GetDesktopWindow() == foreground
            || GetShellWindow() == foreground
            || foreground == hwnd
        {
            let was = FULLSCREEN.load(Ordering::Relaxed);
            if was {
                FULLSCREEN.store(false, Ordering::Relaxed);
                let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
                if SHOW_MOUSE_INFO.load(Ordering::Relaxed) {
                    stop_and_join_mouse_thread();
                    MOUSE_THREAD.with(|m| {
                        *m.borrow_mut() = Some(start_mouse_thread());
                    });
                }
            }
            return;
        }

        let mut rect = RECT::default();
        let _ = GetWindowRect(foreground, &mut rect);
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let is_full = (rect.right - rect.left) == screen_w
            && (rect.bottom - rect.top) == screen_h;

        let was = FULLSCREEN.load(Ordering::Relaxed);
        FULLSCREEN.store(is_full, Ordering::Relaxed);

        if is_full && !was {
            KillTimer(Some(hwnd), TIMER_ID_CPU_MEM).ok();
            stop_and_join_mouse_thread();
        } else if !is_full && was {
            let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
            if SHOW_MOUSE_INFO.load(Ordering::Relaxed) {
                stop_and_join_mouse_thread();
                MOUSE_THREAD.with(|m| {
                    *m.borrow_mut() = Some(start_mouse_thread());
                });
            }
        }
    }
}

fn quit_existing_instance() {
    unsafe {
        let class_name: Vec<u16> = crate::config::WINDOW_CLASS.encode_utf16().collect();
        let hwnd = FindWindowW(
            windows::core::PCWSTR(class_name.as_ptr()),
            windows::core::PCWSTR(std::ptr::null()),
        );

        if let Ok(h) = hwnd {
            if !h.is_invalid() {
                let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
                for _ in 0..50 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    if FindWindowW(
                        windows::core::PCWSTR(class_name.as_ptr()),
                        windows::core::PCWSTR(std::ptr::null()),
                    )
                    .is_err()
                    {
                        break;
                    }
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

    unsafe {
        let mutex_name: Vec<u16> = crate::config::MUTEX_NAME.encode_utf16().collect();
        let mutex_handle = CreateMutexW(None, true, windows::core::PCWSTR(mutex_name.as_ptr()));
        if let Ok(handle) = mutex_handle {
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                return;
            }
        } else {
            show_error("Failed to create mutex");
            return;
        }

        if register_window_class().is_err() {
            show_error("Failed to register window class");
            if let Ok(handle) = mutex_handle {
                let _ = CloseHandle(handle);
            }
            return;
        }

        let hwnd = match create_main_window() {
            Ok(h) => h,
            Err(e) => {
                show_error(&format!("Failed to create window: {}", e));
                if let Ok(handle) = mutex_handle {
                    let _ = CloseHandle(handle);
                }
                return;
            }
        };

        mouse_hid::init(hwnd);
        init_network_listener(hwnd);

        TASKBAR_CREATED_MSG.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

        if let Ok(handle) = RegisterPowerSettingNotification(
            HANDLE(hwnd.0),
            &GUID_MONITOR_POWER_ON,
            REGISTER_NOTIFICATION_FLAGS(DEVICE_NOTIFY_WINDOW_HANDLE),
        ) {
            POWER_NOTIFY_HANDLE.store(handle.0, Ordering::Relaxed);
        }

        if !embed_in_taskbar(hwnd) {
            show_error("Failed to embed in taskbar. Make sure explorer.exe is running.");
            if let Ok(handle) = mutex_handle {
                let _ = CloseHandle(handle);
            }
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

        let _ = InvalidateRect(Some(hwnd), None, false);

        let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, TIMER_INTERVAL_NETWORK, None);
        let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);

        if SHOW_MOUSE_INFO.load(Ordering::Relaxed) {
            MOUSE_THREAD.with(|m| {
                *m.borrow_mut() = Some(start_mouse_thread());
            });
        }

        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);

        let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
        while windows::Win32::UI::WindowsAndMessaging::GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
            windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
        }

        stop_and_join_mouse_thread();
        let _ = WTSUnRegisterSessionNotification(hwnd);

        let power_handle = POWER_NOTIFY_HANDLE.load(Ordering::Relaxed);
        if power_handle != 0 {
            let _ = UnregisterPowerSettingNotification(HPOWERNOTIFY(power_handle));
        }

        RENDERER.with(|r| {
            let _ = r.borrow_mut().take();
        });

        if let Ok(handle) = mutex_handle {
            let _ = CloseHandle(handle);
        }
    }
}

fn calc_widget_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    unsafe {
        let h_taskbar = match FindWindowW(w!("Shell_TrayWnd"), w!("")) {
            Ok(h) => h,
            Err(_) => return None,
        };
        let h_tray = match FindWindowExW(Some(h_taskbar), None, w!("TrayNotifyWnd"), w!("")) {
            Ok(h) => h,
            Err(_) => return None,
        };

        let mut rc_tray = RECT::default();
        let mut rc_taskbar = RECT::default();
        if GetWindowRect(h_tray, &mut rc_tray).is_err() {
            return None;
        }
        if GetWindowRect(h_taskbar, &mut rc_taskbar).is_err() {
            return None;
        }

        let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd);
        let scale = dpi as f64 / 96.0;
        let display_width = (DISPLAY_WIDTH as f64 * scale).round() as i32;
        let display_height = (DISPLAY_HEIGHT as f64 * scale).round() as i32;
        let gap = (GAP as f64 * scale).round() as i32;

        let display_x = rc_tray.left - rc_taskbar.left - gap - display_width;
        let display_y = (rc_taskbar.bottom - rc_taskbar.top - display_height) / 2;

        Some((display_x, display_y, display_width, display_height))
    }
}

unsafe fn embed_in_taskbar(hwnd: HWND) -> bool {
    unsafe {
        let (display_x, display_y, display_width, display_height) = match calc_widget_rect(hwnd) {
            Some(rect) => rect,
            None => {
                show_error("Cannot find Shell_TrayWnd or TrayNotifyWnd");
                return false;
            }
        };

        let h_taskbar = match FindWindowW(w!("Shell_TrayWnd"), w!("")) {
            Ok(h) => h,
            Err(_) => {
                show_error("Cannot find Shell_TrayWnd");
                return false;
            }
        };

        let _ = SetParent(hwnd, Some(h_taskbar));

        SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_CHILD.0 | WS_VISIBLE.0) as isize);

        let current_ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, current_ex_style | (WS_EX_LAYERED.0 as isize));

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
        }

        true
    }
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

pub unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let tcm = TASKBAR_CREATED_MSG.load(Ordering::Relaxed);
        if msg == tcm && tcm != 0 {
            remove_tray_icon();
            let _ = ShowWindow(hwnd, SW_HIDE);
            if embed_in_taskbar(hwnd) {
                create_tray_icon(hwnd);
                RENDERER.with(|r| {
                    if let Some(renderer) = r.borrow_mut().as_mut() {
                        renderer.update_dpi(hwnd);
                        renderer.update_text_color();
                    }
                });
                let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, TIMER_INTERVAL_NETWORK, None);
                if !SUSPENDED.load(Ordering::Relaxed) && !FULLSCREEN.load(Ordering::Relaxed) {
                    let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, 5000, None);
                }
                if SHOW_MOUSE_INFO.load(Ordering::Relaxed)
                    && !SUSPENDED.load(Ordering::Relaxed)
                    && !FULLSCREEN.load(Ordering::Relaxed)
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
                let hdc = BeginPaint(hwnd, &mut ps);
                RENDERER.with(|r| {
                    if let Some(renderer) = r.borrow().as_ref() {
                        renderer.render(hdc);
                    }
                });
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }

            WM_TIMER => {
                match wparam.0 {
                    TIMER_ID_NETWORK => {
                        check_fullscreen(hwnd);
                        if !SUSPENDED.load(Ordering::Relaxed) && !FULLSCREEN.load(Ordering::Relaxed) {
                            update_taskbar_position(hwnd);
                            collect_network();
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                    TIMER_ID_CPU_MEM => {
                        if !SUSPENDED.load(Ordering::Relaxed) && !FULLSCREEN.load(Ordering::Relaxed) {
                            collect_cpu();
                            collect_memory();
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }

            WM_USER_MOUSE_UPDATE | WM_USER_MOUSE_STATUS => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }

            WM_USER_NETWORK_DISCONNECTED => {
                if !SUSPENDED.load(Ordering::Relaxed) && !FULLSCREEN.load(Ordering::Relaxed) {
                    let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, TIMER_INTERVAL_NETWORK_BACKOFF, None);
                }
                LRESULT(0)
            }

            WM_USER_NETWORK_RECONNECTED => {
                NETWORK_BACKOFF.store(false, Ordering::Relaxed);
                CONSECUTIVE_ZERO_COUNT.store(0, Ordering::Relaxed);
                if !SUSPENDED.load(Ordering::Relaxed) && !FULLSCREEN.load(Ordering::Relaxed) {
                    let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, TIMER_INTERVAL_NETWORK, None);
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
                        resume_system(hwnd);
                    }
                    PBT_POWERSETTINGCHANGE => {
                        let setting = lparam.0 as *const POWERBROADCAST_SETTING;
                        if !setting.is_null() {
                            let setting_ref = &*setting;
                            if setting_ref.PowerSetting == GUID_MONITOR_POWER_ON && setting_ref.DataLength >= 1 {
                                let monitor_on = setting_ref.Data[0] != 0;
                                if monitor_on {
                                    resume_system(hwnd);
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
                        resume_system(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }

            WM_CLOSE => {
                remove_tray_icon();
                stop_mouse_thread();
                PostQuitMessage(0);
                LRESULT(0)
            }

            WM_COMMAND => {
                let menu_id = (wparam.0 & 0xFFFF) as u32;
                if menu_id == MENU_ID_SHOW_MOUSE {
                    let current_state = SHOW_MOUSE_INFO.load(Ordering::Relaxed);
                    if !current_state {
                        if check_mouse_available() {
                            SHOW_MOUSE_INFO.store(true, Ordering::Relaxed);
                            stop_and_join_mouse_thread();
                            MOUSE_THREAD.with(|m| {
                                *m.borrow_mut() = Some(start_mouse_thread());
                            });
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        } else {
                            show_error("未检测到物理鼠标或鼠标不支持");
                        }
                    } else {
                        SHOW_MOUSE_INFO.store(false, Ordering::Relaxed);
                        stop_and_join_mouse_thread();
                        MOUSE_ONLINE.store(false, Ordering::Relaxed);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                } else if menu_id == MENU_ID_RESTART_HID {
                    if SHOW_MOUSE_INFO.load(Ordering::Relaxed) {
                        stop_and_join_mouse_thread();
                        MOUSE_ONLINE.store(false, Ordering::Relaxed);
                        MOUSE_BATTERY_LEVEL.store(0, Ordering::Relaxed);
                        MOUSE_DPI_VALUE.store(0, Ordering::Relaxed);
                        MOUSE_THREAD.with(|m| {
                            *m.borrow_mut() = Some(start_mouse_thread());
                        });
                        let _ = InvalidateRect(Some(hwnd), None, false);
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

            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}