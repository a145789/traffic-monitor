use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, GetCursorPos, LoadIconW,
    SetForegroundWindow, TrackPopupMenu, WM_USER, WNDCLASSEXW, IDI_APPLICATION,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_VISIBLE, WS_POPUP,
    TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, MENUITEMINFOW, MIIM_STRING, MIIM_STATE, MIIM_ID, MIIM_FTYPE,
    MFS_CHECKED, MFS_UNCHECKED, MFS_DISABLED, MFT_SEPARATOR, InsertMenuItemW, DestroyMenu, PostMessageW, WM_CLOSE,
};

use std::cell::RefCell;
use crate::config::{APP_NAME, WINDOW_CLASS, WINDOW_TITLE, DISPLAY_WIDTH, DISPLAY_HEIGHT, SHOW_MOUSE_INFO, MENU_ID_SHOW_MOUSE, MENU_ID_RESTART_HID};

pub const WM_APP_TRAY: u32 = WM_USER + 100;
pub const MENU_ID_AUTOSTART: u32 = 1001;
pub const MENU_ID_EXIT: u32 = 1002;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

thread_local! {
    static TRAY_DATA: RefCell<Option<NOTIFYICONDATAW>> = RefCell::new(None);
}

pub fn register_window_class() -> Result<(), String> {
    unsafe {
        let class_name: Vec<u16> = WINDOW_CLASS.encode_utf16().collect();
        let class_name_pcw = PCWSTR(class_name.as_ptr());
        let wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(crate::wnd_proc),
            hInstance: windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .unwrap()
                .into(),
            lpszClassName: class_name_pcw,
            ..Default::default()
        };

        if windows::Win32::UI::WindowsAndMessaging::RegisterClassExW(&wnd_class) == 0 {
            return Err("Failed to register window class".to_string());
        }

        Ok(())
    }
}

pub fn create_main_window() -> Result<HWND, String> {
    unsafe {
        let class_name: Vec<u16> = WINDOW_CLASS.encode_utf16().collect();
        let window_name: Vec<u16> = WINDOW_TITLE.encode_utf16().collect();
        let class_name_pcw = PCWSTR(class_name.as_ptr());
        let window_name_pcw = PCWSTR(window_name.as_ptr());

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name_pcw,
            window_name_pcw,
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            DISPLAY_WIDTH,
            DISPLAY_HEIGHT,
            None,
            None,
            Some(windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .unwrap()
                .into()),
            None,
        );

        match hwnd {
            Ok(h) => Ok(h),
            Err(e) => Err(format!("Failed to create window: {:?}", e)),
        }
    }
}

pub fn create_tray_icon(hwnd: HWND) {
    unsafe {
        let hicon = LoadIconW(None, IDI_APPLICATION).unwrap_or_default();

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_APP_TRAY,
            hIcon: hicon,
            ..Default::default()
        };
        nid.Anonymous.uVersion = windows::Win32::UI::Shell::NOTIFYICON_VERSION_4;

        let tip: Vec<u16> = "Traffic Monitor\0".encode_utf16().collect();
        nid.szTip[..tip.len()].copy_from_slice(&tip);

        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        let _ = Shell_NotifyIconW(windows::Win32::UI::Shell::NIM_SETVERSION, &nid);
        TRAY_DATA.with(|t| {
            *t.borrow_mut() = Some(nid);
        });
    }
}

pub fn remove_tray_icon() {
    unsafe {
        TRAY_DATA.with(|t| {
            if let Some(nid) = t.borrow().as_ref() {
                let _ = Shell_NotifyIconW(NIM_DELETE, nid);
            }
        });
    }
}

pub fn show_context_menu(hwnd: HWND) {
    unsafe {
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);

        let hmenu = CreatePopupMenu().unwrap();

        // 1. Version item (Disabled)
        let version_str = format!("Traffic Monitor v{}\0", VERSION);
        let version_text: Vec<u16> = version_str.encode_utf16().collect();
        let mut version_item = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_STATE | MIIM_ID,
            fState: MFS_DISABLED,
            wID: 0,
            ..Default::default()
        };
        version_item.dwTypeData = PWSTR(version_text.as_ptr() as *mut u16);
        let _ = InsertMenuItemW(hmenu, 0, true, &version_item);

        // 2. Separator
        let sep_item = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE,
            fType: MFT_SEPARATOR,
            ..Default::default()
        };
        let _ = InsertMenuItemW(hmenu, 1, true, &sep_item);

        // 3. Autostart
        let autostart_text: Vec<u16> = "开机自启\0".encode_utf16().collect();
        let mut autostart_item = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_STATE | MIIM_ID,
            fState: if is_autostart_enabled() {
                MFS_CHECKED
            } else {
                MFS_UNCHECKED
            },
            wID: MENU_ID_AUTOSTART,
            ..Default::default()
        };
        autostart_item.dwTypeData = PWSTR(autostart_text.as_ptr() as *mut u16);
        let _ = InsertMenuItemW(hmenu, 2, true, &autostart_item);

        // 4. Show mouse info
        let show_mouse = SHOW_MOUSE_INFO.load(std::sync::atomic::Ordering::Relaxed);
        let mouse_text: Vec<u16> = "显示鼠标信息\0".encode_utf16().collect();
        let mut mouse_item = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_STATE | MIIM_ID,
            fState: if show_mouse {
                MFS_CHECKED
            } else {
                MFS_UNCHECKED
            },
            wID: MENU_ID_SHOW_MOUSE,
            ..Default::default()
        };
        mouse_item.dwTypeData = PWSTR(mouse_text.as_ptr() as *mut u16);
        let _ = InsertMenuItemW(hmenu, 3, true, &mouse_item);

        // 5. Restart HID (only visible when mouse info is shown)
        let mut exit_pos = 4;
        if show_mouse {
            let restart_text: Vec<u16> = "重置鼠标\0".encode_utf16().collect();
            let mut restart_item = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_STRING | MIIM_STATE | MIIM_ID,
                fState: MFS_UNCHECKED,
                wID: MENU_ID_RESTART_HID,
                ..Default::default()
            };
            restart_item.dwTypeData = PWSTR(restart_text.as_ptr() as *mut u16);
            let _ = InsertMenuItemW(hmenu, 4, true, &restart_item);
            exit_pos = 5;
        }

        // 6. Exit
        let exit_text: Vec<u16> = "退出\0".encode_utf16().collect();
        let mut exit_item = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_STATE | MIIM_ID,
            fState: MFS_UNCHECKED,
            wID: MENU_ID_EXIT,
            ..Default::default()
        };
        exit_item.dwTypeData = PWSTR(exit_text.as_ptr() as *mut u16);
        let _ = InsertMenuItemW(hmenu, exit_pos, true, &exit_item);

        let _ = SetForegroundWindow(hwnd);

        let _ = TrackPopupMenu(
            hmenu,
            TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            Some(0),
            hwnd,
            None,
        );

        let _ = DestroyMenu(hmenu);
    }
}

pub fn handle_menu_command(hwnd: HWND, item_id: u32) {
    match item_id {
        MENU_ID_AUTOSTART => toggle_autostart(),
        MENU_ID_EXIT => {
            unsafe {
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_CLOSE,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                );
            }
        }
        _ => {}
    }
}

fn is_autostart_enabled() -> bool {
    unsafe {
        use windows::Win32::System::Registry::{
            RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ,
        };

        let key_path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
            .encode_utf16()
            .collect();
        let value_name: Vec<u16> = APP_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        let mut hkey = Default::default();

        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )
        .is_ok()
        {
            let mut buf = [0u8; 512];
            let mut buf_size = buf.len() as u32;
            let result = RegQueryValueExW(
                hkey,
                PCWSTR(value_name.as_ptr()),
                None,
                None,
                Some(buf.as_mut_ptr()),
                Some(&mut buf_size),
            );
            let _ = windows::Win32::System::Registry::RegCloseKey(hkey).ok();
            result.is_ok()
        } else {
            false
        }
    }
}

fn toggle_autostart() {
    unsafe {
        use windows::Win32::System::Registry::{
            RegOpenKeyExW, RegSetValueExW, RegDeleteValueW, HKEY_CURRENT_USER, KEY_WRITE, REG_SZ,
        };

        let key_path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
            .encode_utf16()
            .collect();
        let value_name: Vec<u16> = APP_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        let mut hkey = Default::default();

        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            Some(0),
            KEY_WRITE,
            &mut hkey,
        )
        .is_ok()
        {
            if is_autostart_enabled() {
                let _ = RegDeleteValueW(hkey, PCWSTR(value_name.as_ptr())).ok();
            } else {
                let exe_path = std::env::current_exe().unwrap();
                let path_str = exe_path.to_string_lossy().to_string();
                let path_quoted = format!("\"{}\"", path_str);
                let path_wide: Vec<u16> = path_quoted.encode_utf16().chain(std::iter::once(0)).collect();
                let _ = RegSetValueExW(
                    hkey,
                    PCWSTR(value_name.as_ptr()),
                    Some(0),
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        path_wide.as_ptr() as *const u8,
                        path_wide.len() * 2,
                    )),
                )
                .ok();
            }
            let _ = windows::Win32::System::Registry::RegCloseKey(hkey).ok();
        }
    }
}

