//! 系统暂停/恢复与全屏检测逻辑。
//!
//! 负责管理定时器的启停（休眠/锁屏/全屏时暂停），以节省 CPU 资源。

use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetDesktopWindow, GetForegroundWindow, GetShellWindow, GetWindowRect, KillTimer, SetTimer,
};

use crate::collector::trim_working_set;
use crate::config::{
    CPU_MEM_INTERVAL, TIMER_ID_CPU_MEM, TIMER_ID_FULLSCREEN, TIMER_ID_NETWORK, TIMER_ID_THERMAL,
    TIMER_INTERVAL_FULLSCREEN, TIMER_INTERVAL_NETWORK, TIMER_INTERVAL_NETWORK_BACKOFF,
    TIMER_INTERVAL_THERMAL,
};
use crate::state::{CONSECUTIVE_ZERO_COUNT, FULLSCREEN, NETWORK_BACKOFF, SUSPENDED};
use crate::window::get_taskbar_hwnd;

pub fn suspend_system(hwnd: HWND) {
    SUSPENDED.store(true, Ordering::Release);
    FULLSCREEN.store(false, Ordering::Release);
    // SAFETY:
    // hwnd 是操作系统分配的有效主窗口句柄。
    // 在系统休眠或锁屏时安全关闭所有监测定时器。
    unsafe {
        KillTimer(Some(hwnd), TIMER_ID_NETWORK).ok();
        KillTimer(Some(hwnd), TIMER_ID_CPU_MEM).ok();
        KillTimer(Some(hwnd), TIMER_ID_FULLSCREEN).ok();
        KillTimer(Some(hwnd), TIMER_ID_THERMAL).ok();
    }
    trim_working_set();
}

pub fn resume_system(hwnd: HWND, reset_backoff: bool) {
    SUSPENDED.store(false, Ordering::Release);
    if reset_backoff {
        CONSECUTIVE_ZERO_COUNT.store(0, Ordering::Release);
        NETWORK_BACKOFF.store(false, Ordering::Release);
    }
    let network_interval = if NETWORK_BACKOFF.load(Ordering::Acquire) {
        TIMER_INTERVAL_NETWORK_BACKOFF
    } else {
        TIMER_INTERVAL_NETWORK
    };
    // SAFETY: hwnd 是系统分配的有效主窗口句柄。
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_ID_NETWORK, network_interval, None);
        let _ = SetTimer(
            Some(hwnd),
            TIMER_ID_FULLSCREEN,
            TIMER_INTERVAL_FULLSCREEN,
            None,
        );
        let _ = SetTimer(Some(hwnd), TIMER_ID_THERMAL, TIMER_INTERVAL_THERMAL, None);
    }
    if !FULLSCREEN.load(Ordering::Acquire) {
        // SAFETY: hwnd 有效，定时器 ID 合法。
        unsafe {
            let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, CPU_MEM_INTERVAL, None);
        }
    }
}

pub fn check_fullscreen(hwnd: HWND) {
    // SAFETY: 纯查询 API，无副作用。
    let foreground = unsafe { GetForegroundWindow() };
    let is_invalid = foreground.is_invalid();
    // SAFETY: GetDesktopWindow 和 GetShellWindow 是纯查询 Win32 API，无副作用。
    let is_desktop_or_shell =
        unsafe { GetDesktopWindow() == foreground || GetShellWindow() == foreground };

    if is_invalid || is_desktop_or_shell || foreground == hwnd {
        let was = FULLSCREEN.load(Ordering::Acquire);
        if was {
            FULLSCREEN.store(false, Ordering::Release);
            // SAFETY: hwnd 是当前进程所持有并处于活动状态的有效主窗口句柄，重新启动此线程关联的定时器不会引发未定义行为。
            unsafe {
                let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, CPU_MEM_INTERVAL, None);
                let _ = SetTimer(Some(hwnd), TIMER_ID_THERMAL, TIMER_INTERVAL_THERMAL, None);
            }
        }
        return;
    }

    let mut rect = RECT::default();
    // SAFETY: foreground 非空，rect 在栈上分配。
    let _ = unsafe { GetWindowRect(foreground, &mut rect) };

    // 使用 MonitorFromWindow 获取前台窗口所在显示器
    // SAFETY: foreground 有效，MONITOR_DEFAULTTONEAREST 是合法标志。
    let hmon_fg = unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) };
    let mut mi_fg = MONITORINFOEXW::default();
    mi_fg.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: hmon_fg 有效，mi_fg 在栈上分配且 cbSize 已初始化。
    let fg_ok = unsafe { GetMonitorInfoW(hmon_fg, &mut mi_fg as *mut MONITORINFOEXW as *mut _) };

    let is_full = if fg_ok.as_bool() {
        let mon_rect = mi_fg.monitorInfo.rcMonitor;
        rect.left == mon_rect.left
            && rect.top == mon_rect.top
            && rect.right == mon_rect.right
            && rect.bottom == mon_rect.bottom
    } else {
        false
    };

    // 检查前台窗口是否覆盖任务栏所在显示器
    let same_monitor = match get_taskbar_hwnd() {
        Some(h_taskbar) => {
            // SAFETY: h_taskbar 有效。
            let hmon_tb = unsafe { MonitorFromWindow(h_taskbar, MONITOR_DEFAULTTONEAREST) };
            hmon_fg == hmon_tb
        }
        None => false,
    };

    let was = FULLSCREEN.load(Ordering::Acquire);
    let should_suspend = is_full && same_monitor;
    FULLSCREEN.store(should_suspend, Ordering::Release);

    if should_suspend && !was {
        // SAFETY: hwnd 有效，销毁定时器。
        unsafe {
            KillTimer(Some(hwnd), TIMER_ID_CPU_MEM).ok();
            KillTimer(Some(hwnd), TIMER_ID_THERMAL).ok();
        }
    } else if !should_suspend && was {
        // SAFETY: hwnd 有效，重建定时器。
        unsafe {
            let _ = SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, CPU_MEM_INTERVAL, None);
            let _ = SetTimer(Some(hwnd), TIMER_ID_THERMAL, TIMER_INTERVAL_THERMAL, None);
        }
    }
}

/// Converts an ASCII string to a fixed-size UTF-16 array. Only works for ASCII; non-ASCII bytes
/// will produce incorrect results.
pub const fn utf16<const N: usize>(s: &str) -> [u16; N] {
    let mut buf = [0u16; N];
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

/// # Safety
///
/// 调用者必须保证 `lparam` 指向一个有效的、以 NUL 结尾的 UTF-16 宽字符序列。
/// 由 `WM_SETTINGCHANGE` 消息传入时 OS 保证此条件成立。
pub unsafe fn is_immersive_color_set(lparam: LPARAM) -> bool {
    let ptr = lparam.0 as *const u16;
    if ptr.is_null() {
        return false;
    }
    const EXPECTED: &[u16] = &utf16::<18>("ImmersiveColorSet\0");
    for (i, &expected_char) in EXPECTED.iter().enumerate() {
        // SAFETY: 调用者保证 ptr 指向有效的 NUL 结尾 UTF-16 序列，按偏移遍历安全。
        let actual_char = unsafe { *ptr.add(i) };
        if actual_char != expected_char {
            return false;
        }
        if actual_char == 0 {
            return true;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::LPARAM;

    // ===== utf16 =====

    #[test]
    fn test_utf16_ascii() {
        let result = utf16::<5>("test");
        // 't'=0x74 'e'=0x65 's'=0x73 't'=0x74 + NUL padding
        assert_eq!(result, [0x74, 0x65, 0x73, 0x74, 0]);
    }

    #[test]
    fn test_utf16_exact_fit() {
        // 恰好填满缓冲区时不应溢出。
        let result = utf16::<3>("ab");
        assert_eq!(result, [b'a' as u16, b'b' as u16, 0]);
    }

    // ===== is_immersive_color_set =====

    #[test]
    fn test_immersive_color_null_pointer() {
        // SAFETY: LPARAM(0) 表示 null 指针，函数应安全返回 false。
        let result = unsafe { is_immersive_color_set(LPARAM(0)) };
        assert!(!result);
    }

    #[test]
    fn test_immersive_color_valid_string() {
        let valid: Vec<u16> = "ImmersiveColorSet\0".encode_utf16().collect();
        // SAFETY: valid 在栈上，指针在调用期间有效。
        let result = unsafe { is_immersive_color_set(LPARAM(valid.as_ptr() as isize)) };
        assert!(result);
    }

    #[test]
    fn test_immersive_color_wrong_string() {
        let wrong: Vec<u16> = "SomeOtherSetting\0".encode_utf16().collect();
        // SAFETY: wrong 在栈上，指针在调用期间有效。
        let result = unsafe { is_immersive_color_set(LPARAM(wrong.as_ptr() as isize)) };
        assert!(!result);
    }

    #[test]
    fn test_immersive_color_prefix_only() {
        // 仅前缀匹配（如 "ImmersiveColor" 无 "Set"），应返回 false。
        let partial: Vec<u16> = "ImmersiveColor\0".encode_utf16().collect();
        // SAFETY: partial 在栈上，指针有效。
        let result = unsafe { is_immersive_color_set(LPARAM(partial.as_ptr() as isize)) };
        assert!(!result);
    }
}
