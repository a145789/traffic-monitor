//! 任务栏窗口管理：查找 Shell_TrayWnd、计算嵌入位置、嵌入与位置更新。

use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, GWL_EXSTYLE, GWL_STYLE, GetWindowLongPtrW, GetWindowRect, HWND_TOP,
    IsWindow, LWA_COLORKEY, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SetLayeredWindowAttributes, SetParent, SetWindowLongPtrW, SetWindowPos, WS_CHILD,
    WS_EX_LAYERED, WS_VISIBLE,
};
use windows::core::w;

use crate::config::{COLOR_KEY, DISPLAY_HEIGHT, DISPLAY_WIDTH, GAP};
use crate::util::show_error;

static TASKBAR_HWND: AtomicIsize = AtomicIsize::new(0);

pub fn get_taskbar_hwnd() -> Option<HWND> {
    let cached = TASKBAR_HWND.load(Ordering::Acquire);
    if cached != 0 {
        let hwnd = HWND(cached as *mut std::ffi::c_void);
        // SAFETY: IsWindow 是纯查询 API，hwnd 来自缓存，仅做有效性判断。
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Some(hwnd);
        }
        TASKBAR_HWND.store(0, Ordering::Release);
    }
    // SAFETY:
    // "Shell_TrayWnd" 是 Windows 任务栏窗口的标准类名，常量宽字符串生命周期覆盖调用。
    // FindWindowW 仅查询窗口句柄，不解引用任何裸指针，失败时安全返回 Err。
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

pub fn calc_widget_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let h_taskbar = get_taskbar_hwnd()?;
    // SAFETY: h_taskbar 已被验证为有效句柄，"TrayNotifyWnd" 为系统 Tray 窗口类名。
    let h_tray = unsafe { FindWindowExW(Some(h_taskbar), None, w!("TrayNotifyWnd"), w!("")).ok()? };

    let mut rc_tray = RECT::default();
    let mut rc_taskbar = RECT::default();
    // SAFETY: h_tray 和 h_taskbar 有效，rect 在栈上分配。
    unsafe {
        GetWindowRect(h_tray, &mut rc_tray).ok()?;
        GetWindowRect(h_taskbar, &mut rc_taskbar).ok()?;
    }

    // SAFETY: hwnd 有效，GetDpiForWindow 是纯查询 API。
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
            show_error("Cannot find Shell_TrayWnd or TrayNotifyWnd");
            return false;
        }
    };

    let h_taskbar = match get_taskbar_hwnd() {
        Some(h) => h,
        None => {
            show_error("Cannot find Shell_TrayWnd");
            return false;
        }
    };

    // SAFETY: hwnd 和 h_taskbar 均为已验证的有效句柄。
    unsafe {
        let _ = SetParent(hwnd, Some(h_taskbar));
        SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_CHILD.0 | WS_VISIBLE.0) as isize);
        let current_ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            current_ex_style | (WS_EX_LAYERED.0 as isize),
        );
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
            return false;
        }
    }

    true
}

pub fn update_taskbar_position(hwnd: HWND) {
    thread_local! {
        static LAST_RECT: std::cell::Cell<Option<(i32, i32, i32, i32)>> = const { std::cell::Cell::new(None) };
    }

    let Some((display_x, display_y, display_width, display_height)) = calc_widget_rect(hwnd) else {
        return;
    };

    let changed = LAST_RECT.with(|lp| match lp.get() {
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
    });

    if changed {
        // SAFETY: hwnd 有效，SWP_NOZORDER 不调整层级。
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
