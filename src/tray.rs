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
    MENU_ID_CHECK_UPDATE_MANUAL, MENU_ID_EXIT, REG_PATH_RUN, TRAY_ICON_RESOURCE_ID, VERSION,
    WM_APP_TRAY,
};
use crate::ffi_guard::MenuGuard;
use crate::state::{ENABLE_AUTO_UPDATE, UPDATE_IN_PROGRESS};
use crate::util::{
    copy_wide_truncated, diag, module_instance, reg_read_string, reg_remove_value,
    reg_write_string_os, to_wide,
};

thread_local! {
    static TRAY_DATA: RefCell<Option<NOTIFYICONDATAW>> = const { RefCell::new(None) };
}

/// 创建托盘图标。返回 false 表示图标不存在（调用方据此知道状态机里没有图标，
/// 如 `NIM_ADD` 失败时不得把 `nid` 存进 `TRAY_DATA` 让移除/重建路径误判）。
pub fn create_tray_icon(hwnd: HWND) -> bool {
    let Ok(hinstance) = module_instance() else {
        diag!("创建托盘图标失败: 无法获取模块实例");
        // 失败早退同样置空：返回 false 时 `TRAY_DATA` 必为空，不留旧句柄让
        // 移除/重建路径误判图标仍在。
        TRAY_DATA.with(|t| *t.borrow_mut() = None);
        return false;
    };

    // TRAY_ICON_RESOURCE_ID 对应 MAKEINTRESOURCEW 语义（见 config 注释的命名关联）。
    #[allow(clippy::manual_dangling_ptr)]
    let hicon = unsafe {
        LoadIconW(Some(hinstance), PCWSTR(TRAY_ICON_RESOURCE_ID as *const u16))
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
    copy_wide_truncated(&mut nid.szTip, &tip);

    // SAFETY: nid 完整初始化，同步调用期间存活。
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool();
    if !added {
        diag!("创建托盘图标失败: Shell_NotifyIconW(NIM_ADD) 未成功");
        TRAY_DATA.with(|t| *t.borrow_mut() = None);
        return false;
    }

    // v4 语义是右键菜单（WM_CONTEXTMENU 经 lParam 低字分发）所必需；
    // SETVERSION 失败时回调停留在 v0（右键发 WM_RBUTTONUP，菜单静默失灵）。
    // 此处仅对瞬态失败同版本重试一次；旧 shell 不支持 V4 时仍会失败，
    // 由 diag 留痕，不做版本降级（降级需连带处理 WM_RBUTTONUP，超出本轮范围）。
    if !unsafe { Shell_NotifyIconW(NIM_SETVERSION, &nid) }.as_bool() {
        diag!("托盘图标 NIM_SETVERSION 失败，重试一次");
        let _ = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &nid) };
    }

    TRAY_DATA.with(|t| {
        *t.borrow_mut() = Some(nid);
    });
    true
}

pub fn remove_tray_icon() {
    TRAY_DATA.with(|t| {
        let mut slot = t.borrow_mut();
        if let Some(nid) = slot.as_ref() {
            // SAFETY: nid 来自 create_tray_icon，生命周期由 TRAY_DATA 管理。
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, nid);
            }
        }
        // 删除后置空：`TRAY_DATA` 非空 ⟺ 图标存在，不再靠「重复 NIM_DELETE 无害」兜底。
        *slot = None;
    });
}

/// 托盘图标缺失时补建一次（恢复调度器每个周期调用）。
///
/// `TRAY_DATA` 非空 ⟺ 图标存在（见 [`remove_tray_icon`]），它就是「是否需要补做」的
/// 唯一真值源：非空直接返回，**不重复 `NIM_ADD`**（重复添加不会覆盖既有图标，只会
/// 多出一个绑定同一窗口的图标）。返回 false 表示本轮真的尝试了补建且失败
/// （`TRAY_DATA` 保持为空），由下个恢复周期继续补——托盘是唯一的 UI 入口，
/// 没有图标时退出、开机自启、更新开关全部不可达，不能只等下一次 Explorer 重建。
pub fn ensure_tray_icon(hwnd: HWND) -> bool {
    if TRAY_DATA.with(|t| t.borrow().is_some()) {
        return true;
    }
    create_tray_icon(hwnd)
}

/// 只读观测托盘图标当前绑定的窗口（`#[cfg(test)]` 冒烟测试用，不暴露写入口）。
///
/// `Some(hwnd)` ⟺ 图标存在且回调落点是该窗口；重建前后对比它即验证托盘迁移
/// （旧绑定残留到已销毁窗口 vs 新绑定的区分，见冒烟用例）。
#[cfg(test)]
pub(crate) fn tray_owner() -> Option<HWND> {
    TRAY_DATA.with(|t| t.borrow().as_ref().map(|nid| nid.hWnd))
}

/// 逐项构建菜单，pos 递增、顺序即唯一真源；形状由 menu_shape_matches_command_routes 锁定。
/// 文本缓冲只需活过同步插入调用：菜单复制持有文本（inserted_menu_text_is_copied 实测锁定）。
fn build_menu(hmenu: HMENU) {
    let update_in_progress = UPDATE_IN_PROGRESS.load(Ordering::Relaxed);
    let mut pos = 0;
    insert_string_item(
        hmenu,
        pos,
        0,
        &to_wide(&format!("{APP_TITLE} v{VERSION}")),
        MFS_DISABLED.0,
    );
    pos += 1;
    insert_separator(hmenu, pos);
    pos += 1;
    insert_string_item(
        hmenu,
        pos,
        MENU_ID_AUTOSTART,
        &to_wide("开机自启"),
        if is_autostart_enabled() {
            MFS_CHECKED.0
        } else {
            MFS_UNCHECKED.0
        },
    );
    pos += 1;
    insert_string_item(
        hmenu,
        pos,
        MENU_ID_AUTO_UPDATE_TOGGLE,
        &to_wide("自动检查更新"),
        if ENABLE_AUTO_UPDATE.load(Ordering::Relaxed) {
            MFS_CHECKED.0
        } else {
            MFS_UNCHECKED.0
        },
    );
    pos += 1;
    insert_string_item(
        hmenu,
        pos,
        MENU_ID_CHECK_UPDATE_MANUAL,
        &to_wide(if update_in_progress {
            "检查更新中..."
        } else {
            "检查更新..."
        }),
        if update_in_progress {
            MFS_DISABLED.0
        } else {
            MFS_UNCHECKED.0
        },
    );
    pos += 1;
    insert_string_item(hmenu, pos, MENU_ID_EXIT, &to_wide("退出"), MFS_UNCHECKED.0);
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
    // SAFETY: text NUL 终止且在 InsertMenuItemW 同步返回前保持存活；
    // 此后菜单持有自己的副本（inserted_menu_text_is_copied 实测锁定），不再引用该缓冲。
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

    build_menu(hmenu);

    // TPM_RETURNCMD：先取命令，再把前台权交还任务栏，最后执行，避免更新弹窗
    // 关闭后焦点回落主进程并初始化第三方 IME（见 AGENTS.md）。
    // SAFETY: hmenu 由 MenuGuard 持有至 TrackPopupMenu 返回；菜单文本由菜单自身持有。
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

fn handle_menu_command(hwnd: HWND, item_id: u32) {
    match item_id {
        MENU_ID_AUTOSTART => toggle_autostart(),
        MENU_ID_AUTO_UPDATE_TOGGLE => toggle_auto_update(),
        MENU_ID_CHECK_UPDATE_MANUAL => crate::update::start_manual_check(),
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
        // OsString 直拼引号：不经 String 中转，安装目录含非 Unicode
        // 可解码字符时自启项无损；读取侧（is_autostart_enabled）只判存在性，不动。
        let mut quoted = std::ffi::OsString::from("\"");
        quoted.push(&exe_path);
        quoted.push("\"");
        reg_write_string_os(REG_PATH_RUN, APP_NAME, &quoted);
    }
}

fn toggle_auto_update() {
    let current = ENABLE_AUTO_UPDATE.load(Ordering::Relaxed);
    let new_state = !current;
    ENABLE_AUTO_UPDATE.store(new_state, Ordering::Relaxed);
    crate::update::save_auto_update_enabled(new_state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        DestroyMenu, GetMenuItemCount, GetMenuItemID, GetMenuItemInfoW,
    };

    /// 两段式取回菜单项文本与状态（GetMenuItemInfoW 协议：先探长度，再备缓冲拷出）。
    fn menu_item(hmenu: HMENU, pos: u32) -> (String, MENU_ITEM_STATE) {
        let mut item = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_STATE,
            ..Default::default()
        };
        // SAFETY: hmenu 有效；dwTypeData 为 NULL 时本次调用只探文本长度。
        unsafe { GetMenuItemInfoW(hmenu, pos, true, &mut item) }.unwrap();
        let mut buf = vec![0u16; item.cch as usize + 1];
        item.dwTypeData = PWSTR(buf.as_mut_ptr());
        item.cch = buf.len() as u32;
        // SAFETY: buf 容量含 NUL 且在同步返回前保持存活、无别名。
        unsafe { GetMenuItemInfoW(hmenu, pos, true, &mut item) }.unwrap();
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        (String::from_utf16_lossy(&buf[..len]), item.fState)
    }

    #[test]
    fn menu_shape_matches_command_routes() {
        // SAFETY: 本测试线程创建弹出菜单，用例末尾 DestroyMenu 配对释放。
        // 不用 MenuGuard：ffi_guard 的计数用例要求它是默认门禁下唯一构造点。
        let hmenu = unsafe { CreatePopupMenu() }.unwrap();
        let autostart = is_autostart_enabled();
        let auto_update = ENABLE_AUTO_UPDATE.load(Ordering::Relaxed);
        build_menu(hmenu);
        // SAFETY: hmenu 在本用例内有效。
        unsafe {
            assert_eq!(GetMenuItemCount(Some(hmenu)), 6);
            assert_eq!(GetMenuItemID(hmenu, 0), 0);
            // 分隔线用 fType 判定：文档称 GetMenuItemID 返回 -1，Win11 实测返回 0。
            let mut sep = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE,
                ..Default::default()
            };
            GetMenuItemInfoW(hmenu, 1, true, &mut sep).unwrap();
            assert_eq!(sep.fType, MFT_SEPARATOR, "位置 1 必须是分隔线");
            assert_eq!(GetMenuItemID(hmenu, 2), MENU_ID_AUTOSTART);
            assert_eq!(GetMenuItemID(hmenu, 3), MENU_ID_AUTO_UPDATE_TOGGLE);
            assert_eq!(GetMenuItemID(hmenu, 4), MENU_ID_CHECK_UPDATE_MANUAL);
            assert_eq!(GetMenuItemID(hmenu, 5), MENU_ID_EXIT);
        }
        let (title, state) = menu_item(hmenu, 0);
        assert_eq!(title, format!("{APP_TITLE} v{VERSION}"));
        assert_eq!(state, MFS_DISABLED);
        let (text, state) = menu_item(hmenu, 2);
        assert_eq!(text, "开机自启");
        assert_eq!(
            state,
            if autostart {
                MFS_CHECKED
            } else {
                MFS_UNCHECKED
            }
        );
        let (text, state) = menu_item(hmenu, 3);
        assert_eq!(text, "自动检查更新");
        assert_eq!(
            state,
            if auto_update {
                MFS_CHECKED
            } else {
                MFS_UNCHECKED
            }
        );
        // UPDATE_IN_PROGRESS 在并行测试里有写者（protocol.rs 的复位用例），
        // 只锁定文案与置灰态来自同一快照，不对标志本身做精确断言。
        let (text, state) = menu_item(hmenu, 4);
        assert!(
            (text == "检查更新中..." && state == MFS_DISABLED)
                || (text == "检查更新..." && state == MFS_UNCHECKED),
            "文案与置灰态必须成对: {text:?} {state:?}"
        );
        let (text, state) = menu_item(hmenu, 5);
        assert_eq!(text, "退出");
        assert_eq!(state, MFS_UNCHECKED);
        // SAFETY: hmenu 为本用例创建的成功句柄，此处恰好释放一次。
        unsafe {
            let _ = DestroyMenu(hmenu);
        }
    }

    #[test]
    fn inserted_menu_text_is_copied() {
        // SAFETY: 本测试线程创建弹出菜单，用例末尾 DestroyMenu 配对释放。
        let hmenu = unsafe { CreatePopupMenu() }.unwrap();
        let mut text = to_wide("开机自启 Traffic Monitor lifetime probe");
        let expected = String::from_utf16_lossy(&text[..text.len() - 1]);
        insert_string_item(hmenu, 0, MENU_ID_EXIT, &text, MFS_UNCHECKED.0);
        // 若菜单引用调用方缓冲，覆写后取回的将是全 X；取回原文即证明复制语义。
        text.fill(u16::from(b'X'));
        assert_eq!(menu_item(hmenu, 0).0, expected);
        drop(text);
        assert_eq!(menu_item(hmenu, 0).0, expected);
        // SAFETY: hmenu 为本用例创建的成功句柄，此处恰好释放一次。
        unsafe {
            let _ = DestroyMenu(hmenu);
        }
    }
}
