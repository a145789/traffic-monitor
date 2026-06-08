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
    MFS_CHECKED, MFS_UNCHECKED, MFS_DISABLED, MFT_SEPARATOR, InsertMenuItemW, PostMessageW, WM_CLOSE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;

use std::cell::RefCell;
use crate::config::{APP_NAME, WINDOW_CLASS, WINDOW_TITLE, DISPLAY_WIDTH, DISPLAY_HEIGHT, SHOW_MOUSE_INFO, MENU_ID_SHOW_MOUSE, MENU_ID_RESTART_HID};

pub const WM_APP_TRAY: u32 = WM_USER + 100;
pub const MENU_ID_AUTOSTART: u32 = 1001;
pub const MENU_ID_EXIT: u32 = 1002;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

thread_local! {
    static TRAY_DATA: RefCell<Option<NOTIFYICONDATAW>> = RefCell::new(None);
}

struct RegKey(windows::Win32::System::Registry::HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: self.0 是有效注册表句柄，退出作用域时安全关闭。
        unsafe {
            let _ = windows::Win32::System::Registry::RegCloseKey(self.0);
        }
    }
}

struct MenuGuard(windows::Win32::UI::WindowsAndMessaging::HMENU);

impl Drop for MenuGuard {
    fn drop(&mut self) {
        // SAFETY: self.0 是有效的菜单句柄，销毁它防止内存泄漏。
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyMenu(self.0);
        }
    }
}

pub fn register_window_class() -> Result<(), String> {
    let class_name: Vec<u16> = WINDOW_CLASS.encode_utf16().collect();
    let class_name_pcw = PCWSTR(class_name.as_ptr());
    
    // SAFETY:
    // GetModuleHandleW 获取当前执行实例的句柄，在当前进程内有效。
    let hinstance = unsafe { GetModuleHandleW(None).unwrap().into() };
    
    let wnd_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(crate::wnd_proc),
        hInstance: hinstance,
        lpszClassName: class_name_pcw,
        ..Default::default()
    };

    // SAFETY:
    // wnd_class 结构体已由 safe 代码完整地被初始化且 class_name_pcw 具有合理生命周期。
    // 调用 RegisterClassExW 是安全的。
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

    // SAFETY:
    // GetModuleHandleW 获取当前进程句柄。
    let hinstance = unsafe { GetModuleHandleW(None).unwrap().into() };

    // SAFETY:
    // 传入的 PCWSTR 具有长生命周期。
    // CreateWindowExW 会创建此窗口并返回其有效的 HWND 句柄，如果不成功则返回 Err。
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
    // SAFETY:
    // 获取当前实例模块句柄，尝试加载资源 ID 为 1 的图标，若无则回退加载系统默认图标。
    let hicon = unsafe {
        LoadIconW(Some(GetModuleHandleW(None).unwrap().into()), PCWSTR(1 as *const u16))
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

    // SAFETY:
    // nid 已被正确初始化。Shell_NotifyIconW 安全地添加系统托盘图标并设置其版本。
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
            // SAFETY: nid 为此前由主线程创建并管理的合法托盘图标信息结构体，在此安全删除。
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, nid);
            }
        }
    });
}

pub fn show_context_menu(hwnd: HWND) {
    let mut point = POINT::default();
    // SAFETY: 获取当前鼠标坐标并存入 point 中。
    unsafe {
        let _ = GetCursorPos(&mut point);
    }

    // SAFETY: 创建一个弹出菜单。
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
    
    // SAFETY: 插入版本菜单项。
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
    
    // SAFETY: 插入分隔线项。
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
    
    // SAFETY: 插入自启项。
    unsafe {
        let _ = InsertMenuItemW(hmenu, 2, true, &autostart_item);
    }

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
    
    // SAFETY: 插入显示鼠标信息项。
    unsafe {
        let _ = InsertMenuItemW(hmenu, 3, true, &mouse_item);
    }

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
        
        // SAFETY: 插入重置鼠标项。
        unsafe {
            let _ = InsertMenuItemW(hmenu, 4, true, &restart_item);
        }
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
    
    // SAFETY: 插入退出项。
    unsafe {
        let _ = InsertMenuItemW(hmenu, exit_pos, true, &exit_item);
    }

    // SAFETY:
    // 将指定窗口设为前台活动窗口。
    // 在指定位置跟踪托盘菜单并捕获用户的点击，hmenu 已通过 MenuGuard 实现 RAII 自动释放。
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
}

pub fn handle_menu_command(hwnd: HWND, item_id: u32) {
    match item_id {
        MENU_ID_AUTOSTART => toggle_autostart(),
        MENU_ID_EXIT => {
            // SAFETY:
            // hwnd 是由主窗口实例传递过来的有效窗口句柄。
            // PostMessageW 发送 WM_CLOSE 消息是安全的。
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
        RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ,
    };

    let key_path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
        .encode_utf16()
        .collect();
    let value_name: Vec<u16> = APP_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let mut hkey = Default::default();

    // SAFETY:
    // HKEY_CURRENT_USER 是预定义的根键。
    // key_path 是以 NUL 结尾的宽字符数组。
    // 打开的注册表项句柄将被存入 hkey 并通过 RegKey 自动进行生命周期释放（RAII）。
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
        let _key_guard = RegKey(hkey);
        let mut buf = [0u8; 512];
        let mut buf_size = buf.len() as u32;
        
        // SAFETY:
        // hkey 是成功打开的键句柄，value_name 是以 NUL 结尾的宽字符数组。
        // 缓冲区 buf 的指针和长度变量均合法，操作系统向其填充值，在该调用期间是安全的。
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
        RegOpenKeyExW, RegSetValueExW, RegDeleteValueW, HKEY_CURRENT_USER, KEY_WRITE, REG_SZ,
    };

    let key_path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
        .encode_utf16()
        .collect();
    let value_name: Vec<u16> = APP_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let mut hkey = Default::default();

    // SAFETY:
    // 使用有效的预定义根键和以 NUL 结尾的子键名称路径打开注册表键。
    // 成功打开后将其句柄保存在 hkey 并通过 RegKey 进行 RAII 自动关闭。
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
        let _key_guard = RegKey(hkey);
        if is_autostart_enabled() {
            // SAFETY: 删除自启动项值。
            unsafe {
                let _ = RegDeleteValueW(hkey, PCWSTR(value_name.as_ptr())).ok();
            }
        } else {
            let exe_path = std::env::current_exe().unwrap();
            let path_str = exe_path.to_string_lossy().to_string();
            let path_quoted = format!("\"{}\"", path_str);
            let path_wide: Vec<u16> = path_quoted.encode_utf16().chain(std::iter::once(0)).collect();
            
            // SAFETY:
            // 写入键值，数据缓冲区的指针和字节数在调用期间合法且对应。
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

