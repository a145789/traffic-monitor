//! 系统托盘：图标生命周期维护、右键菜单、开机自启读写。

use std::cell::RefCell;
use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, GetCursorPos, HMENU, IDI_APPLICATION, InsertMenuItemW, LoadIconW,
    MENU_ITEM_STATE, MENUITEMINFOW, MFS_CHECKED, MFS_DISABLED, MFS_UNCHECKED, MFT_SEPARATOR,
    MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING, PostMessageW, SetForegroundWindow,
    TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_CLOSE,
};
use windows::core::{PCWSTR, PWSTR};

use crate::config::{
    APP_NAME, APP_TITLE, MENU_ID_AUTO_UPDATE_TOGGLE, MENU_ID_AUTOSTART,
    MENU_ID_CHECK_UPDATE_MANUAL, MENU_ID_EXIT, REG_PATH_RUN, VERSION, WM_APP_TRAY,
};
use crate::ffi_guard::MenuGuard;
use crate::state::{ENABLE_AUTO_UPDATE, UPDATE_IN_PROGRESS};
use crate::util::{module_instance, reg_read_string, reg_remove_value, reg_write_string, to_wide};

thread_local! {
    static TRAY_DATA: RefCell<Option<NOTIFYICONDATAW>> = const { RefCell::new(None) };
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
        // NOTIFYICON_VERSION_4 默认抑制标准悬浮提示（szTip 不显示），
        // 必须同时置 NIF_SHOWTIP 才展示 tooltip；v4 又是右键菜单
        // （WM_CONTEXTMENU 经 lParam 低字分发）所必需，二者缺一不可。
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_SHOWTIP | NIF_TIP,
        uCallbackMessage: WM_APP_TRAY,
        hIcon: hicon,
        ..Default::default()
    };
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;

    let tip = to_wide(APP_TITLE);
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

/// 菜单项定义。`text` 为 NUL 结尾 UTF-16（`to_wide` 产出），由调用方持有至菜单关闭。
enum MenuEntry {
    Item { id: u32, text: Vec<u16>, state: u32 },
    Separator,
}

/// 依据当前运行时状态构建完整菜单项列表（版本号、自启、自动更新、检查更新、退出）。
fn build_menu_entries() -> Vec<MenuEntry> {
    let update_in_progress = UPDATE_IN_PROGRESS.load(Ordering::Relaxed);
    vec![
        MenuEntry::Item {
            id: 0,
            text: to_wide(&format!("{APP_TITLE} v{VERSION}")),
            state: MFS_DISABLED.0,
        },
        MenuEntry::Separator,
        MenuEntry::Item {
            id: MENU_ID_AUTOSTART,
            text: to_wide("开机自启"),
            state: if is_autostart_enabled() {
                MFS_CHECKED.0
            } else {
                MFS_UNCHECKED.0
            },
        },
        MenuEntry::Item {
            id: MENU_ID_AUTO_UPDATE_TOGGLE,
            text: to_wide("自动检查更新"),
            state: if ENABLE_AUTO_UPDATE.load(Ordering::Relaxed) {
                MFS_CHECKED.0
            } else {
                MFS_UNCHECKED.0
            },
        },
        MenuEntry::Item {
            id: MENU_ID_CHECK_UPDATE_MANUAL,
            text: to_wide(if update_in_progress {
                "检查更新中..."
            } else {
                "检查更新..."
            }),
            state: if update_in_progress {
                MFS_DISABLED.0
            } else {
                MFS_UNCHECKED.0
            },
        },
        MenuEntry::Item {
            id: MENU_ID_EXIT,
            text: to_wide("退出"),
            state: MFS_UNCHECKED.0,
        },
    ]
}

/// 插入字符串菜单项；`text` 须以 NUL 结尾（`to_wide` 产出即可）。
fn insert_string_item(hmenu: HMENU, pos: u32, id: u32, text: &[u16], state: u32) {
    let mut item = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STRING | MIIM_STATE | MIIM_ID,
        fState: MENU_ITEM_STATE(state),
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

    // entries 持有全部菜单文本缓冲区，须存活至 TrackPopupMenu 返回。
    let entries = build_menu_entries();
    for (pos, entry) in entries.iter().enumerate() {
        match entry {
            MenuEntry::Separator => insert_separator(hmenu, pos as u32),
            MenuEntry::Item { id, text, state } => {
                insert_string_item(hmenu, pos as u32, *id, text, *state);
            }
        }
    }

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
    reg_read_string(REG_PATH_RUN, APP_NAME).is_some()
}

fn toggle_autostart() {
    if is_autostart_enabled() {
        reg_remove_value(REG_PATH_RUN, APP_NAME);
    } else if let Ok(exe_path) = std::env::current_exe() {
        let path_str = exe_path.to_string_lossy().to_string();
        let path_quoted = format!("\"{path_str}\"");
        reg_write_string(REG_PATH_RUN, APP_NAME, &path_quoted);
    }
}

fn toggle_auto_update() {
    let current = ENABLE_AUTO_UPDATE.load(Ordering::Relaxed);
    let new_state = !current;
    ENABLE_AUTO_UPDATE.store(new_state, Ordering::Relaxed);
    crate::update::save_auto_update_enabled(new_state);
}
