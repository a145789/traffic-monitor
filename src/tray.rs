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
use crate::ffi_guard::{RegKey, MenuGuard};

pub const WM_APP_TRAY: u32 = WM_USER + 100;
pub const MENU_ID_AUTOSTART: u32 = 1001;
pub const MENU_ID_EXIT: u32 = 1002;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

thread_local! {
    static TRAY_DATA: RefCell<Option<NOTIFYICONDATAW>> = RefCell::new(None);
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
    // 1. GetModuleHandleW(None) 在当前运行进程中总是能成功返回有效的 HMODULE 实例。
    // 2. 1 as *const u16 是有效的资源标识常数。如果应用资源中不存在此图标，
    //    则 LoadIconW 将返回错误，并能被 or_else 分支捕获，回退到安全的系统默认图标 IDI_APPLICATION，确保 hicon 的合法性。
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
    // nid 结构体的所有必要字段（包括 cbSize、hWnd、hIcon 等）在此前已经在当前作用域被完全初始化，
    // 其生命周期在此函数调用期间仍然存在，因此将其引用传给 Shell_NotifyIconW 不会导致野指针或内存越界。
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
            // SAFETY:
            // nid 为先前由 create_tray_icon 成功添加且在 TRAY_DATA 静态变量中合法持有的托盘数据。
            // 它的生存周期依然存在，将它的引用传递给 Shell_NotifyIconW 执行 NIM_DELETE 操作是完全安全的。
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, nid);
            }
        }
    });
}

pub fn show_context_menu(hwnd: HWND) {
    let mut point = POINT::default();
    // SAFETY: 传入指向栈上分配的 POINT 结构体可变指针，GetCursorPos 保证写入有效的鼠标屏幕坐标，不会发生越界或写入未初始化内存。
    unsafe {
        let _ = GetCursorPos(&mut point);
    }

    // SAFETY: CreatePopupMenu 在不需要特殊参数的情况下请求操作系统分配一个空菜单资源，其句柄在成功时被 wrap 并通过 MenuGuard 自动生命周期释放，保证无悬空指针或资源泄漏。
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
    
    // SAFETY: hmenu 是有效的弹出菜单句柄。version_item 已被正确初始化，其中 dwTypeData 指向在栈上生存周期内（此函数内）有效的 version_text 缓冲区的常数指针，符合 InsertMenuItemW 的生存期要求。
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
    
    // SAFETY: sep_item 结构体 cbSize 已设置正确，且只指定了分隔线类型，不需要其他的缓冲区资源，将其传给 InsertMenuItemW 是完全安全的。
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
    
    // SAFETY: autostart_item 结构体已成功填充，且 dwTypeData 指向在本函数周期内有效的 autostart_text 缓冲区，调用 InsertMenuItemW 安全。
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
    
    // SAFETY: mouse_item 结构体已成功填充，dwTypeData 指向在当前作用域有效的 mouse_text 缓冲区，调用 InsertMenuItemW 安全。
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
        
        // SAFETY: restart_item 已成功填充，dwTypeData 指向在当前作用域内有效且以 NUL 结尾的 restart_text 缓冲区。
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
    
    // SAFETY: exit_item 结构体初始化正确，dwTypeData 指向有效的 exit_text 缓冲区，符合 InsertMenuItemW 的规范。
    unsafe {
        let _ = InsertMenuItemW(hmenu, exit_pos, true, &exit_item);
    }

    // SAFETY:
    // 1. hwnd 是有效的窗口句柄。
    // 2. point 是 GetCursorPos 初始化的有效屏幕坐标。
    // 3. hmenu 是成功创建的有效菜单句柄。
    // 该函数在调用 TrackPopupMenu 前调用 SetForegroundWindow 确保菜单弹出后能正常处理键盘/鼠标焦点消息。
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
        MENU_ID_EXIT => {
            // SAFETY: hwnd 是从主窗口传来的有效 HWND 句柄，使用 PostMessageW 异步发送 WM_CLOSE 消息是线程安全的，操作系统会将该消息排入对应的消息队列。
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
    // 1. HKEY_CURRENT_USER 是有效的系统注册表根键。
    // 2. key_path 是已处理且以 NUL 结尾的宽字符指针，保证 Windows API 正确读取子键路径。
    // 3. 将栈上分配的 hkey 变量的可变地址传给 RegOpenKeyExW，一旦成功，其句柄被 RegKey 管理，确保即使发生异常也能 RAII 清理句柄。
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
        // 1. hkey 是先前成功打开的合法注册表键句柄。
        // 2. value_name 是以 NUL 结尾的有效宽字符串指针。
        // 3. buf 和 buf_size 设置合理，RegQueryValueExW 写入数据的字节数不会超过 buf 的固定容量 512，防止溢出。
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
    // 1. HKEY_CURRENT_USER 为预定义的合法根键。
    // 2. key_path 已转换为以 NUL 结尾的宽字符串，确保传入 RegOpenKeyExW 的路径有效。
    // 3. 栈上的 hkey 在此函数中保持有效，成功后由 RegKey 管理其 RAII 生命周期，防止句柄泄露。
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

