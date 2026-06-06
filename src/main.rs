mod collector;
mod config;
mod mouse_hid;
mod renderer;
mod tray;

use std::sync::atomic::Ordering;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, FindWindowExW, FindWindowW, GetWindowRect,
    MessageBoxW, PostQuitMessage, RegisterWindowMessageW, SetLayeredWindowAttributes,
    SetParent, SetTimer, SetWindowPos, HWND_TOP, LWA_COLORKEY,
    MB_ICONERROR, MB_OK, SWP_NOACTIVATE, SWP_SHOWWINDOW, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DPICHANGED,
    WM_PAINT, WM_POWERBROADCAST, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND,
};

use crate::collector::{collect_cpu, collect_memory, collect_network, trim_working_set};
use crate::config::{
    COLOR_KEY, DISPLAY_HEIGHT, DISPLAY_WIDTH, GAP, SUSPENDED,
    TIMER_ID_CPU_MEM, TIMER_ID_NETWORK,
};
use crate::mouse_hid::{start_mouse_thread, stop_mouse_thread, WM_USER_MOUSE_UPDATE, WM_USER_MOUSE_STATUS};
use crate::renderer::Renderer;
use crate::tray::{create_main_window, create_tray_icon, register_window_class, remove_tray_icon, WM_APP_TRAY};

static mut RENDERER: Option<Renderer> = None;
static mut MOUSE_THREAD: Option<std::thread::JoinHandle<()>> = None;
static mut TASKBAR_CREATED_MSG: u32 = 0;

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

fn main() {
    unsafe {
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

        TASKBAR_CREATED_MSG = RegisterWindowMessageW(w!("TaskbarCreated"));

        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(COLOR_KEY), 0, LWA_COLORKEY);

        if !embed_in_taskbar(hwnd) {
            show_error("Failed to embed in taskbar. Make sure explorer.exe is running.");
            return;
        }

        create_tray_icon(hwnd);

        RENDERER = Some(Renderer::new());

        let _ = SetTimer(hwnd, TIMER_ID_NETWORK, 1000, None);
        let _ = SetTimer(hwnd, TIMER_ID_CPU_MEM, 5000, None);

        MOUSE_THREAD = Some(start_mouse_thread());

        let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
        while windows::Win32::UI::WindowsAndMessaging::GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
            windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
        }
    }
}

unsafe fn embed_in_taskbar(hwnd: HWND) -> bool {
    let h_taskbar = match FindWindowW(w!("Shell_TrayWnd"), w!("")) {
        Ok(h) => h,
        Err(_) => {
            show_error("Cannot find Shell_TrayWnd");
            return false;
        }
    };

    let h_tray = match FindWindowExW(h_taskbar, None, w!("TrayNotifyWnd"), w!("")) {
        Ok(h) => h,
        Err(_) => {
            show_error("Cannot find TrayNotifyWnd");
            return false;
        }
    };

    let mut rc_tray = windows::Win32::Foundation::RECT::default();
    let mut rc_taskbar = windows::Win32::Foundation::RECT::default();
    let _ = GetWindowRect(h_tray, &mut rc_tray);
    let _ = GetWindowRect(h_taskbar, &mut rc_taskbar);

    let display_x = rc_tray.left - GAP - DISPLAY_WIDTH;
    let display_y = (rc_taskbar.bottom - rc_taskbar.top - DISPLAY_HEIGHT) / 2;

    let _ = SetParent(hwnd, h_taskbar);
    let _ = SetWindowPos(
        hwnd,
        HWND_TOP,
        display_x,
        display_y,
        DISPLAY_WIDTH,
        DISPLAY_HEIGHT,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );

    true
}

pub unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == TASKBAR_CREATED_MSG && TASKBAR_CREATED_MSG != 0 {
        remove_tray_icon();
        embed_in_taskbar(hwnd);
        create_tray_icon(hwnd);
        return LRESULT(0);
    }

    match msg {
        WM_CREATE => LRESULT(0),

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if let Some(renderer) = &RENDERER {
                renderer.render(hdc);
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        WM_TIMER => {
            match wparam.0 {
                TIMER_ID_NETWORK => {
                    if !SUSPENDED.load(Ordering::Relaxed) {
                        collect_network();
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                TIMER_ID_CPU_MEM => {
                    if !SUSPENDED.load(Ordering::Relaxed) {
                        collect_cpu();
                        collect_memory();
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }

        WM_USER_MOUSE_UPDATE | WM_USER_MOUSE_STATUS => {
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }

        WM_SETTINGCHANGE => {
            if let Some(renderer) = &mut RENDERER {
                let h_taskbar = FindWindowW(w!("Shell_TrayWnd"), w!(""));
                if let Ok(taskbar) = h_taskbar {
                    renderer.update_text_color(taskbar);
                }
            }
            LRESULT(0)
        }

        WM_DPICHANGED => {
            if let Some(renderer) = &mut RENDERER {
                renderer.recreate_font(14);
            }
            LRESULT(0)
        }

        WM_POWERBROADCAST => {
            match wparam.0 as u32 {
                PBT_APMSUSPEND => {
                    SUSPENDED.store(true, Ordering::Relaxed);
                    stop_mouse_thread();
                    trim_working_set();
                }
                PBT_APMRESUMEAUTOMATIC => {
                    SUSPENDED.store(false, Ordering::Relaxed);
                    MOUSE_THREAD = Some(start_mouse_thread());
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
            tray::handle_menu_command(hwnd, menu_id);
            LRESULT(0)
        }

        x if x == WM_APP_TRAY => {
            if lparam.0 as u32 == WM_RBUTTONUP {
                tray::show_context_menu(hwnd);
            }
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
