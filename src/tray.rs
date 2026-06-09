use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, GetCursorPos, IDI_APPLICATION, InsertMenuItemW, LoadIconW,
    MENUITEMINFOW, MFS_CHECKED, MFS_DISABLED, MFS_UNCHECKED, MFT_SEPARATOR, MIIM_FTYPE, MIIM_ID,
    MIIM_STATE, MIIM_STRING, PostMessageW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON,
    TrackPopupMenu, WM_CLOSE, WM_USER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};
use windows::core::{PCWSTR, PWSTR};

use crate::config::{
    APP_NAME, DISPLAY_HEIGHT, DISPLAY_WIDTH, ENABLE_AUTO_UPDATE, MENU_ID_AUTO_UPDATE_TOGGLE,
    MENU_ID_CHECK_UPDATE_MANUAL, MENU_ID_RESTART_HID, MENU_ID_SHOW_MOUSE, SHOW_MOUSE_INFO,
    WINDOW_CLASS, WINDOW_TITLE,
};
use crate::ffi_guard::{MenuGuard, RegKey};
use std::cell::RefCell;

pub const WM_APP_TRAY: u32 = WM_USER + 100;
pub const MENU_ID_AUTOSTART: u32 = 1001;
pub const MENU_ID_EXIT: u32 = 1002;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

thread_local! {
    static TRAY_DATA: RefCell<Option<NOTIFYICONDATAW>> = const { RefCell::new(None) };
}

pub fn register_window_class() -> Result<(), String> {
    let class_name: Vec<u16> = WINDOW_CLASS.encode_utf16().collect();
    let class_name_pcw = PCWSTR(class_name.as_ptr());

    // SAFETY: GetModuleHandleW(None) 在当前进程内总是成功。
    let hinstance = unsafe { GetModuleHandleW(None).unwrap().into() };

    let wnd_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(crate::wnd_proc),
        hInstance: hinstance,
        lpszClassName: class_name_pcw,
        ..Default::default()
    };

    // SAFETY: wnd_class 已完整初始化，class_name_pcw 生命周期覆盖调用。
    let atom = unsafe { windows::Win32::UI::WindowsAndMessaging::RegisterClassExW(&wnd_class) };
    if atom == 0 {
        return Err("Failed to register window class".to_string());
    }

    Ok(())
}

pub fn create_main_window() -> Result<HWND, String> {
    let class_name: Vec<u16> = WINDOW_CLASS.encode_utf16().collect();
    let window_name: Vec<u16> = WINDOW_TITLE.encode_utf16().collect();
    let class_name_pcw = PCWSTR(class_name.as_ptr());
    let window_name_pcw = PCWSTR(window_name.as_ptr());

    // SAFETY: GetModuleHandleW(None) 在当前进程内总是成功。
    let hinstance = unsafe { GetModuleHandleW(None).unwrap().into() };

    // SAFETY: PCWSTR 指针生命周期覆盖调用，CreateWindowExW 失败时返回 Err。
    let hwnd = unsafe {
        CreateWindowExW(
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
            Some(hinstance),
            None,
        )
    };

    match hwnd {
        Ok(h) => Ok(h),
        Err(e) => Err(format!("Failed to create window: {:?}", e)),
    }
}

pub fn create_tray_icon(hwnd: HWND) {
    // SAFETY: LoadIconW 失败时回退到 IDI_APPLICATION。
    #[allow(clippy::manual_dangling_ptr)]
    // 1 as *const u16 对应 Win32 MAKEINTRESOURCEW(1)，表示嵌入的资源 ID 1（assets/icon.ico），
    // clippy 的 manual_dangling_ptr 规则无法识别 Windows 资源 ID 惯用法，此处需抑制该 lint。
    let hicon = unsafe {
        LoadIconW(
            Some(GetModuleHandleW(None).unwrap().into()),
            PCWSTR(1 as *const u16),
        )
        .or_else(|_| LoadIconW(None, IDI_APPLICATION))
        .unwrap_or_default()
    };

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

    // SAFETY: nid 已完整初始化，生命周期覆盖调用。
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        let _ = Shell_NotifyIconW(windows::Win32::UI::Shell::NIM_SETVERSION, &nid);
    }
    TRAY_DATA.with(|t| {
        *t.borrow_mut() = Some(nid);
    });
}

pub fn remove_tray_icon() {
    TRAY_DATA.with(|t| {
        if let Some(nid) = t.borrow().as_ref() {
            // SAFETY: nid 由 create_tray_icon 成功添加，生命周期由 TRAY_DATA 管理。
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, nid);
            }
        }
    });
}

pub fn show_context_menu(hwnd: HWND) {
    let mut point = POINT::default();
    // SAFETY: point 在栈上分配，GetCursorPos 填充有效坐标。
    unsafe {
        let _ = GetCursorPos(&mut point);
    }

    // SAFETY: CreatePopupMenu 失败时 unwrap panic，成功时句柄由 MenuGuard 管理。
    let hmenu = unsafe { CreatePopupMenu().unwrap() };
    let _menu_guard = MenuGuard(hmenu);

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

    // SAFETY: version_item 已初始化，dwTypeData 指向有效的 version_text。
    unsafe {
        let _ = InsertMenuItemW(hmenu, 0, true, &version_item);
    }

    // 2. Separator
    let sep_item = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_FTYPE,
        fType: MFT_SEPARATOR,
        ..Default::default()
    };

    // SAFETY: sep_item 仅指定分隔线类型，无缓冲区依赖。
    unsafe {
        let _ = InsertMenuItemW(hmenu, 1, true, &sep_item);
    }

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

    // SAFETY: autostart_item 已初始化，dwTypeData 指向有效的 autostart_text。
    unsafe {
        let _ = InsertMenuItemW(hmenu, 2, true, &autostart_item);
    }

    // 4. Auto-update toggle
    let auto_update_enabled = ENABLE_AUTO_UPDATE.load(std::sync::atomic::Ordering::Relaxed);
    let autoupdate_text: Vec<u16> = "自动检查更新\0".encode_utf16().collect();
    let mut autoupdate_item = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STRING | MIIM_STATE | MIIM_ID,
        fState: if auto_update_enabled {
            MFS_CHECKED
        } else {
            MFS_UNCHECKED
        },
        wID: MENU_ID_AUTO_UPDATE_TOGGLE,
        ..Default::default()
    };
    autoupdate_item.dwTypeData = PWSTR(autoupdate_text.as_ptr() as *mut u16);

    // SAFETY: autoupdate_item 已初始化，dwTypeData 指向有效的 autoupdate_text。
    unsafe {
        let _ = InsertMenuItemW(hmenu, 3, true, &autoupdate_item);
    }

    // 5. Manual check update
    let check_update_text: Vec<u16> = "检查更新...\0".encode_utf16().collect();
    let mut check_update_item = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STRING | MIIM_STATE | MIIM_ID,
        fState: MFS_UNCHECKED,
        wID: MENU_ID_CHECK_UPDATE_MANUAL,
        ..Default::default()
    };
    check_update_item.dwTypeData = PWSTR(check_update_text.as_ptr() as *mut u16);

    // SAFETY: check_update_item 已初始化，dwTypeData 指向有效的 check_update_text。
    unsafe {
        let _ = InsertMenuItemW(hmenu, 4, true, &check_update_item);
    }

    // 6. Show mouse info
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

    // SAFETY: mouse_item 已初始化，dwTypeData 指向有效的 mouse_text。
    unsafe {
        let _ = InsertMenuItemW(hmenu, 5, true, &mouse_item);
    }

    // 7. Restart HID (only visible when mouse info is shown)
    let mut exit_pos = 6;
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

        // SAFETY: restart_item 已初始化，dwTypeData 指向有效的 restart_text。
        unsafe {
            let _ = InsertMenuItemW(hmenu, 6, true, &restart_item);
        }
        exit_pos = 7;
    }

    // 8. Exit
    let exit_text: Vec<u16> = "退出\0".encode_utf16().collect();
    let mut exit_item = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STRING | MIIM_STATE | MIIM_ID,
        fState: MFS_UNCHECKED,
        wID: MENU_ID_EXIT,
        ..Default::default()
    };
    exit_item.dwTypeData = PWSTR(exit_text.as_ptr() as *mut u16);

    // SAFETY: exit_item 已初始化，dwTypeData 指向有效的 exit_text。
    unsafe {
        let _ = InsertMenuItemW(hmenu, exit_pos, true, &exit_item);
    }

    // SAFETY: hwnd、point、hmenu 均有效。
    unsafe {
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
    }

    drop(_menu_guard);
}

pub fn handle_menu_command(hwnd: HWND, item_id: u32) {
    match item_id {
        MENU_ID_AUTOSTART => toggle_autostart(),
        MENU_ID_AUTO_UPDATE_TOGGLE => toggle_auto_update(),
        MENU_ID_CHECK_UPDATE_MANUAL => crate::update::start_manual_check(hwnd),
        MENU_ID_EXIT => {
            // SAFETY: hwnd 有效，PostMessageW 异步投递 WM_CLOSE 是线程安全的。
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
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, RegOpenKeyExW, RegQueryValueExW,
    };

    let key_path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
        .encode_utf16()
        .collect();
    let value_name: Vec<u16> = APP_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let mut hkey = Default::default();

    // SAFETY: key_path 以 NUL 结尾，hkey 在栈上分配，成功后由 RegKey 管理。
    let open_ok = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )
        .is_ok()
    };

    if open_ok {
        let _key_guard = RegKey::new(hkey);
        let mut buf = [0u8; 512];
        let mut buf_size = buf.len() as u32;

        // SAFETY: hkey 有效，buf 容量 512 足够。
        let result = unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR(value_name.as_ptr()),
                None,
                None,
                Some(buf.as_mut_ptr()),
                Some(&mut buf_size),
            )
        };
        result.is_ok()
    } else {
        false
    }
}

fn toggle_autostart() {
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_WRITE, REG_SZ, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW,
    };

    let key_path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
        .encode_utf16()
        .collect();
    let value_name: Vec<u16> = APP_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let mut hkey = Default::default();

    // SAFETY: key_path 以 NUL 结尾，hkey 在栈上分配，成功后由 RegKey 管理。
    let open_ok = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            Some(0),
            KEY_WRITE,
            &mut hkey,
        )
        .is_ok()
    };

    if open_ok {
        let _key_guard = RegKey::new(hkey);
        if is_autostart_enabled() {
            // SAFETY: hkey 有效，删除自启动项值。
            unsafe {
                let _ = RegDeleteValueW(hkey, PCWSTR(value_name.as_ptr())).ok();
            }
        } else {
            let exe_path = std::env::current_exe().unwrap();
            let path_str = exe_path.to_string_lossy().to_string();
            let path_quoted = format!("\"{}\"", path_str);
            let path_wide: Vec<u16> = path_quoted
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();

            // SAFETY: hkey 有效，path_wide 以 NUL 结尾，数据长度正确。
            unsafe {
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
        }
    }
}

fn toggle_auto_update() {
    let current = ENABLE_AUTO_UPDATE.load(std::sync::atomic::Ordering::Acquire);
    let new_state = !current;
    ENABLE_AUTO_UPDATE.store(new_state, std::sync::atomic::Ordering::Release);
    crate::update::save_auto_update_enabled(new_state);
}

pub fn load_show_mouse_info() -> bool {
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, RegOpenKeyExW, RegQueryValueExW,
    };

    let key_path: Vec<u16> = "Software\\Traffic Monitor\0".encode_utf16().collect();
    let value_name: Vec<u16> = "ShowMouseInfo\0".encode_utf16().collect();
    let mut hkey = Default::default();

    // SAFETY: key_path 以 NUL 结尾，hkey 在栈上分配，成功后由 RegKey 管理以自动释放句柄。
    let open_ok = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )
        .is_ok()
    };

    if open_ok {
        let _key_guard = RegKey::new(hkey);
        let mut dword: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;

        // SAFETY: hkey 为已验证有效的注册表键句柄，dword 和 size 均为栈上分配的变量，且 dword 缓冲区大小与 size 一致。
        let result = unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR(value_name.as_ptr()),
                None,
                None,
                Some(&mut dword as *mut u32 as *mut u8),
                Some(&mut size),
            )
        };
        if result.is_ok() {
            return dword != 0;
        }
    }

    false
}

pub fn save_show_mouse_info(show: bool) {
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_WRITE, REG_CREATE_KEY_DISPOSITION, REG_DWORD, RegCreateKeyExW,
        RegSetValueExW,
    };

    let key_path: Vec<u16> = "Software\\Traffic Monitor\0".encode_utf16().collect();
    let value_name: Vec<u16> = "ShowMouseInfo\0".encode_utf16().collect();
    let mut hkey = Default::default();
    let mut disposition = REG_CREATE_KEY_DISPOSITION(0);

    // SAFETY: key_path 以 NUL 结尾，hkey 和 disposition 均在栈上分配，成功后句柄由 RegKey 自动接管释放。
    let open_ok = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            None,
            None,
            Default::default(),
            KEY_WRITE,
            None,
            &mut hkey,
            Some(&mut disposition),
        )
        .is_ok()
    };

    if open_ok {
        let _key_guard = RegKey::new(hkey);
        let dword: u32 = if show { 1 } else { 0 };

        // SAFETY: hkey 为已验证有效的注册表键句柄，value_name 以 NUL 结尾，
        // 通过 std::slice::from_raw_parts 安全将栈上 dword 的指针转换为字节切片，且长度正确。
        unsafe {
            let _ = RegSetValueExW(
                hkey,
                PCWSTR(value_name.as_ptr()),
                Some(0),
                REG_DWORD,
                Some(std::slice::from_raw_parts(
                    &dword as *const u32 as *const u8,
                    std::mem::size_of::<u32>(),
                )),
            );
        }
    }
}
