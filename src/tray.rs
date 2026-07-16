use std::cell::RefCell;
use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NOTIFYICON_VERSION_4,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, GetCursorPos, HMENU, IDI_APPLICATION, InsertMenuItemW,
    LoadIconW, MENUITEMINFOW, MFS_CHECKED, MFS_DISABLED, MFS_UNCHECKED, MFT_SEPARATOR, MIIM_FTYPE,
    MIIM_ID, MIIM_STATE, MIIM_STRING, PostMessageW, SetForegroundWindow, TPM_BOTTOMALIGN,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_CLOSE, WM_USER, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};
use windows::core::{PCWSTR, PWSTR};

use crate::config::{
    APP_NAME, DISPLAY_HEIGHT, DISPLAY_WIDTH, MENU_ID_AUTO_UPDATE_TOGGLE,
    MENU_ID_CHECK_UPDATE_MANUAL, WINDOW_CLASS, WINDOW_TITLE,
};
use crate::ffi_guard::MenuGuard;
use crate::state::{ENABLE_AUTO_UPDATE, UPDATE_IN_PROGRESS};
use crate::util::to_wide;

pub const WM_APP_TRAY: u32 = WM_USER + 100;
pub const MENU_ID_AUTOSTART: u32 = 1001;
pub const MENU_ID_EXIT: u32 = 1002;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

thread_local! {
    static TRAY_DATA: RefCell<Option<NOTIFYICONDATAW>> = const { RefCell::new(None) };
}

fn module_instance() -> Result<windows::Win32::Foundation::HINSTANCE, String> {
    // SAFETY: GetModuleHandleW(None) 查询当前进程模块，无指针参数。
    unsafe { GetModuleHandleW(None) }
        .map(Into::into)
        .map_err(|e| format!("获取模块句柄失败: {e:?}"))
}

pub fn register_window_class() -> Result<(), String> {
    // WINDOW_CLASS 常量已含尾 NUL。
    let class_name: Vec<u16> = WINDOW_CLASS.encode_utf16().collect();
    let hinstance = module_instance()?;

    let wnd_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(crate::wnd_proc),
        hInstance: hinstance,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };

    // SAFETY: class_name 在调用期间保持存活；wnd_class 字段完整。
    let atom = unsafe { windows::Win32::UI::WindowsAndMessaging::RegisterClassExW(&wnd_class) };
    if atom == 0 {
        return Err("注册窗口类失败".to_string());
    }
    Ok(())
}

pub fn create_main_window() -> Result<HWND, String> {
    // WINDOW_CLASS / WINDOW_TITLE 常量已含尾 NUL。
    let class_name: Vec<u16> = WINDOW_CLASS.encode_utf16().collect();
    let window_name: Vec<u16> = WINDOW_TITLE.encode_utf16().collect();
    let hinstance = module_instance()?;

    // SAFETY: 宽字符串缓冲区在调用期间存活。
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(window_name.as_ptr()),
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

    hwnd.map_err(|e| format!("创建窗口失败: {e:?}"))
}

pub fn create_tray_icon(hwnd: HWND) {
    let hinstance = match module_instance() {
        Ok(h) => h,
        Err(_) => return,
    };

    // 1 as *const u16 对应 MAKEINTRESOURCEW(1)，资源 ID 1（assets/icon.ico）。
    #[allow(clippy::manual_dangling_ptr)]
    let hicon = unsafe {
        LoadIconW(Some(hinstance), PCWSTR(1 as *const u16))
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
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;

    let tip = to_wide("Traffic Monitor");
    let copy_len = tip.len().min(nid.szTip.len());
    nid.szTip[..copy_len].copy_from_slice(&tip[..copy_len]);

    // SAFETY: nid 完整初始化，同步调用期间存活。
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
    }
    TRAY_DATA.with(|t| {
        *t.borrow_mut() = Some(nid);
    });
}

pub fn remove_tray_icon() {
    TRAY_DATA.with(|t| {
        if let Some(nid) = t.borrow().as_ref() {
            // SAFETY: nid 来自 create_tray_icon，生命周期由 TRAY_DATA 管理。
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, nid);
            }
        }
    });
}

/// 插入字符串菜单项；`text` 须以 NUL 结尾（`to_wide` 产出即可）。
fn insert_string_item(hmenu: HMENU, pos: u32, id: u32, text: &[u16], state: u32) {
    let mut item = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STRING | MIIM_STATE | MIIM_ID,
        fState: windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_STATE(state),
        wID: id,
        ..Default::default()
    };
    item.dwTypeData = PWSTR(text.as_ptr() as *mut u16);
    // SAFETY: text 在 InsertMenuItemW 同步返回前保持存活。
    unsafe {
        let _ = InsertMenuItemW(hmenu, pos, true, &item);
    }
}

fn insert_separator(hmenu: HMENU, pos: u32) {
    let item = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_FTYPE,
        fType: MFT_SEPARATOR,
        ..Default::default()
    };
    unsafe {
        let _ = InsertMenuItemW(hmenu, pos, true, &item);
    }
}

pub fn show_context_menu(hwnd: HWND) {
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut point);
    }

    let Ok(hmenu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    let menu_guard = MenuGuard(hmenu);

    let version_text = to_wide(&format!("Traffic Monitor v{VERSION}"));
    insert_string_item(hmenu, 0, 0, &version_text, MFS_DISABLED.0);

    insert_separator(hmenu, 1);

    let autostart_text = to_wide("开机自启");
    let autostart_state = if is_autostart_enabled() {
        MFS_CHECKED.0
    } else {
        MFS_UNCHECKED.0
    };
    insert_string_item(
        hmenu,
        2,
        MENU_ID_AUTOSTART,
        &autostart_text,
        autostart_state,
    );

    let auto_update_enabled = ENABLE_AUTO_UPDATE.load(Ordering::Relaxed);
    let autoupdate_text = to_wide("自动检查更新");
    let autoupdate_state = if auto_update_enabled {
        MFS_CHECKED.0
    } else {
        MFS_UNCHECKED.0
    };
    insert_string_item(
        hmenu,
        3,
        MENU_ID_AUTO_UPDATE_TOGGLE,
        &autoupdate_text,
        autoupdate_state,
    );

    let update_in_progress = UPDATE_IN_PROGRESS.load(Ordering::Relaxed);
    let check_update_text = if update_in_progress {
        to_wide("检查更新中...")
    } else {
        to_wide("检查更新...")
    };
    let check_state = if update_in_progress {
        MFS_DISABLED.0
    } else {
        MFS_UNCHECKED.0
    };
    insert_string_item(
        hmenu,
        4,
        MENU_ID_CHECK_UPDATE_MANUAL,
        &check_update_text,
        check_state,
    );

    let exit_text = to_wide("退出");
    insert_string_item(hmenu, 5, MENU_ID_EXIT, &exit_text, MFS_UNCHECKED.0);

    // TPM_RETURNCMD：先取命令，再把前台权交还任务栏，最后执行，避免更新弹窗
    // 关闭后焦点回落主进程并初始化第三方 IME（见 AGENTS.md）。
    // SAFETY: hmenu 由 MenuGuard 持有至 TrackPopupMenu 返回；菜单项宽串仍在栈上。
    let selected_item = unsafe {
        let _ = SetForegroundWindow(hwnd);
        TrackPopupMenu(
            hmenu,
            TPM_BOTTOMALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            point.x,
            point.y,
            Some(0),
            hwnd,
            None,
        )
        .0 as u32
    };

    if let Some(taskbar) = crate::window::get_taskbar_hwnd() {
        unsafe {
            let _ = SetForegroundWindow(taskbar);
        }
    }

    drop(menu_guard);

    if selected_item != 0 {
        handle_menu_command(hwnd, selected_item);
    }
}

pub fn handle_menu_command(hwnd: HWND, item_id: u32) {
    match item_id {
        MENU_ID_AUTOSTART => toggle_autostart(),
        MENU_ID_AUTO_UPDATE_TOGGLE => toggle_auto_update(),
        MENU_ID_CHECK_UPDATE_MANUAL => crate::update::start_manual_check(hwnd),
        MENU_ID_EXIT => unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        },
        _ => {}
    }
}

fn is_autostart_enabled() -> bool {
    windows_registry::CURRENT_USER
        .open("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .and_then(|key| key.get_string(APP_NAME))
        .is_ok()
}

fn toggle_autostart() {
    if let Ok(key) =
        windows_registry::CURRENT_USER.create("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
    {
        if is_autostart_enabled() {
            let _ = key.remove_value(APP_NAME);
        } else if let Ok(exe_path) = std::env::current_exe() {
            let path_str = exe_path.to_string_lossy().to_string();
            let path_quoted = format!("\"{path_str}\"");
            let _ = key.set_string(APP_NAME, &path_quoted);
        }
    }
}

fn toggle_auto_update() {
    let current = ENABLE_AUTO_UPDATE.load(Ordering::Relaxed);
    let new_state = !current;
    ENABLE_AUTO_UPDATE.store(new_state, Ordering::Relaxed);
    crate::update::save_auto_update_enabled(new_state);
}
