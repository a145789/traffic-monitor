//! 窗口创建与任务栏嵌入：窗口类注册、主窗口创建、任务栏查找、嵌入与位置更新。

use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{COLORREF, GetLastError, HWND, RECT, SetLastError, WIN32_ERROR};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, FindWindowExW, FindWindowW, GWL_EXSTYLE, GWL_STYLE, GetWindowLongPtrW,
    GetWindowRect, HWND_TOP, IsWindow, LWA_COLORKEY, RegisterClassExW, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetParent,
    SetWindowLongPtrW, SetWindowPos, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSEXW, WNDPROC, WS_CHILD,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPED, WS_POPUP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use crate::config::{
    COLOR_KEY, DISPLAY_HEIGHT, DISPLAY_WIDTH, GAP, WATCHDOG_CLASS, WINDOW_CLASS, WINDOW_TITLE,
};
use crate::util::{module_instance, show_error};

static TASKBAR_HWND: AtomicIsize = AtomicIsize::new(0);

fn register_class(class_name: &str, proc: WNDPROC, err: &str) -> Result<(), String> {
    // 类名常量已含尾 NUL，直接编码后原样保留；指针仅在本次注册调用期间使用。
    let class_wide: Vec<u16> = class_name.encode_utf16().collect();
    let hinstance = module_instance()?;

    let wnd_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: proc,
        hInstance: hinstance,
        lpszClassName: PCWSTR(class_wide.as_ptr()),
        ..Default::default()
    };

    // SAFETY: class_wide 与 wnd_class 在 RegisterClassExW 返回前存活。
    let atom = unsafe { RegisterClassExW(&wnd_class) };
    if atom == 0 {
        return Err(err.to_string());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn create_window(
    class_name: &str,
    title: &[u16],
    width: i32,
    height: i32,
    style: WINDOW_STYLE,
    ex_style: WINDOW_EX_STYLE,
    err: &str,
) -> Result<HWND, String> {
    // 类名常量已含尾 NUL，title 由调用方保证含尾 NUL；指针仅在本次创建调用期间使用。
    let class_wide: Vec<u16> = class_name.encode_utf16().collect();
    let hinstance = module_instance()?;

    // SAFETY: 宽字符串缓冲区在 CreateWindowExW 返回前存活；title 含尾 NUL。
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            PCWSTR(class_wide.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
            0,
            0,
            width,
            height,
            None,
            None,
            Some(hinstance),
            None,
        )
    };

    hwnd.map_err(|e| format!("{err}: {e:?}"))
}

pub fn register_window_class() -> Result<(), String> {
    register_class(WINDOW_CLASS, Some(crate::wnd_proc), "注册窗口类失败")
}

pub fn create_main_window() -> Result<HWND, String> {
    // WINDOW_TITLE 常量已含尾 NUL。
    let window_name: Vec<u16> = WINDOW_TITLE.encode_utf16().collect();
    create_window(
        WINDOW_CLASS,
        &window_name,
        DISPLAY_WIDTH,
        DISPLAY_HEIGHT,
        WS_POPUP | WS_VISIBLE,
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        "创建窗口失败",
    )
}

/// 注册看门狗窗口类：隐藏顶层消息窗口，唯一可靠的 TaskbarCreated 接收者。
pub fn register_watchdog_class() -> Result<(), String> {
    register_class(
        WATCHDOG_CLASS,
        Some(crate::watchdog_wnd_proc),
        "注册看门狗窗口类失败",
    )
}

/// 创建隐藏的顶层看门狗窗口。
///
/// 主窗口 SetParent 进任务栏后成为跨进程子窗口，explorer 销毁任务栏时会将其
/// 级联销毁，且 TaskbarCreated 广播只投递顶层窗口——主窗口自身永远收不到。
/// 看门狗永不嵌入、从不显示（无 GDI 位图/DC），常驻开销可忽略。
pub fn create_watchdog_window() -> Result<HWND, String> {
    // 空标题的 NUL 结尾切片，与原 w!("") 等价。
    const EMPTY_TITLE: [u16; 1] = [0];
    create_window(
        WATCHDOG_CLASS,
        &EMPTY_TITLE,
        0,
        0,
        WS_OVERLAPPED,
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        "创建看门狗窗口失败",
    )
}

pub fn get_taskbar_hwnd() -> Option<HWND> {
    let cached = TASKBAR_HWND.load(Ordering::Acquire);
    if cached != 0 {
        let hwnd = HWND(cached as *mut std::ffi::c_void);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Some(hwnd);
        }
        TASKBAR_HWND.store(0, Ordering::Release);
    }
    // SAFETY: 静态类名 "Shell_TrayWnd"；FindWindowW 仅查询句柄。
    let hwnd = unsafe { FindWindowW(w!("Shell_TrayWnd"), w!("")).ok() };
    if let Some(h) = hwnd {
        TASKBAR_HWND.store(h.0 as isize, Ordering::Release);
    }
    hwnd
}

/// 重置任务栏句柄缓存（由 `TaskbarCreated` 消息触发）。
pub fn invalidate_taskbar_cache() {
    TASKBAR_HWND.store(0, Ordering::Release);
}

/// 计算小组件在任务栏上的目标矩形 (x, y, w, h)；仅 window.rs 内部消费。
fn calc_widget_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let h_taskbar = get_taskbar_hwnd()?;
    // SAFETY: "TrayNotifyWnd" 为系统托盘子窗口类名。
    let h_tray = unsafe { FindWindowExW(Some(h_taskbar), None, w!("TrayNotifyWnd"), w!("")).ok()? };

    let mut rc_tray = RECT::default();
    let mut rc_taskbar = RECT::default();
    unsafe {
        GetWindowRect(h_tray, &mut rc_tray).ok()?;
        GetWindowRect(h_taskbar, &mut rc_taskbar).ok()?;
    }

    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
    let scale = dpi as f64 / 96.0;
    let display_width = (DISPLAY_WIDTH as f64 * scale).round() as i32;
    let display_height = (DISPLAY_HEIGHT as f64 * scale).round() as i32;
    let gap = (GAP as f64 * scale).round() as i32;

    let display_x = rc_tray.left - rc_taskbar.left - gap - display_width;
    let display_y = (rc_taskbar.bottom - rc_taskbar.top - display_height) / 2;

    Some((display_x, display_y, display_width, display_height))
}

pub fn embed_in_taskbar(hwnd: HWND) -> bool {
    let (display_x, display_y, display_width, display_height) = match calc_widget_rect(hwnd) {
        Some(rect) => rect,
        None => {
            show_error("找不到 Shell_TrayWnd 或 TrayNotifyWnd");
            return false;
        }
    };

    let h_taskbar = match get_taskbar_hwnd() {
        Some(h) => h,
        None => {
            show_error("找不到 Shell_TrayWnd");
            return false;
        }
    };

    // SAFETY:
    // 1. 输入与依赖校验：hwnd 与 h_taskbar 均为已通过有效性校验的窗口句柄；
    //    rect 由 calc_widget_rect 计算得到的有效几何。
    // 2. 状态不变性约束：必须严格按 AGENTS.md 既定顺序执行
    //    （SetParent → GWL_STYLE → 重新应用 WS_EX_LAYERED → SetWindowPos →
    //    SetLayeredWindowAttributes）。调换会导致分层透明失效或被任务栏图标遮挡。
    //    任一步失败立即返回 false，避免任务栏嵌入进入不可恢复的中间状态。
    unsafe {
        if SetParent(hwnd, Some(h_taskbar)).is_err() {
            show_error("SetParent 嵌入任务栏失败");
            return false;
        }

        // SetWindowLongPtrW 返回 isize（前值），0 既可能表示"前值就是 0"也可能表示失败，
        // 必须先 SetLastError(WIN32_ERROR(0)) 再调用，事后用 GetLastError 才能可靠区分。
        SetLastError(WIN32_ERROR(0));
        let prev_style = SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_CHILD.0 | WS_VISIBLE.0) as isize);
        if prev_style == 0 {
            let last = GetLastError();
            if last.0 != 0 {
                show_error(&format!("覆盖 GWL_STYLE 失败: 0x{:08X}", last.0));
                return false;
            }
        }

        let current_ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetLastError(WIN32_ERROR(0));
        let prev_ex = SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            current_ex_style | (WS_EX_LAYERED.0 as isize),
        );
        if prev_ex == 0 {
            let last = GetLastError();
            if last.0 != 0 {
                show_error(&format!("重新应用 WS_EX_LAYERED 失败: 0x{:08X}", last.0));
                return false;
            }
        }

        if SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            display_x,
            display_y,
            display_width,
            display_height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        )
        .is_err()
        {
            show_error("SetWindowPos 嵌入任务栏失败");
            return false;
        }
        if let Err(e) = SetLayeredWindowAttributes(hwnd, COLORREF(COLOR_KEY), 0, LWA_COLORKEY) {
            show_error(&format!("设置分层窗口属性失败: {e:?}"));
            return false;
        }
    }

    true
}

pub fn update_taskbar_position(hwnd: HWND) -> bool {
    thread_local! {
        static LAST_RECT: std::cell::Cell<Option<(i32, i32, i32, i32)>> = const { std::cell::Cell::new(None) };
    }

    let Some((display_x, display_y, display_width, display_height)) = calc_widget_rect(hwnd) else {
        return false;
    };

    let target = (display_x, display_y, display_width, display_height);
    if LAST_RECT.with(|lp| lp.get()) == Some(target) {
        return false;
    }

    // 缓存只在移动成功后提交：SetWindowPos 瞬时失败（如 Explorer 重启竞态）时
    // 下个周期会重试，而不是因“矩形==缓存”被永久跳过。
    let moved = unsafe {
        SetWindowPos(
            hwnd,
            None,
            display_x,
            display_y,
            display_width,
            display_height,
            SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_NOZORDER,
        )
        .is_ok()
    };
    if moved {
        LAST_RECT.with(|lp| lp.set(Some(target)));
    }
    moved
}
