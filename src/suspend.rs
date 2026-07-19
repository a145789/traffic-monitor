//! 系统暂停/恢复、全屏检测与主题变更检测。
//!
//! 负责管理定时器的启停（休眠/锁屏/全屏时暂停），以节省 CPU 资源；
//! 电源广播（WM_POWERBROADCAST）与锁屏（WM_WTSSESSION_CHANGE）消息在此处理。

use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::System::SystemServices::GUID_MONITOR_POWER_ON;
use windows::Win32::UI::WindowsAndMessaging::{
    GetDesktopWindow, GetForegroundWindow, GetShellWindow, GetWindowRect, KillTimer,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE, SetTimer,
};

use crate::config::{
    CPU_MEM_INTERVAL, TIMER_ID_CPU_MEM, TIMER_ID_FULLSCREEN, TIMER_ID_NETWORK,
    TIMER_INTERVAL_FULLSCREEN, TIMER_INTERVAL_NETWORK, TIMER_INTERVAL_NETWORK_BACKOFF,
};
use crate::state::{
    CONSECUTIVE_ZERO_COUNT, MONITOR_FULLSCREEN, NETWORK_BACKOFF, SUSPEND_REASON_MONITOR,
    SUSPEND_REASON_SESSION, SUSPEND_REASON_SYSTEM, SUSPEND_REASONS,
};
use crate::util::{trim_working_set, utf16};
use crate::window::get_taskbar_hwnd;

const WTS_SESSION_LOCK: usize = 0x7;
const WTS_SESSION_UNLOCK: usize = 0x8;

#[repr(C)]
#[allow(non_snake_case)]
struct POWERBROADCAST_SETTING {
    PowerSetting: windows::core::GUID,
    DataLength: u32,
    Data: [u8; 1],
}

pub fn is_suspended() -> bool {
    SUSPEND_REASONS.is_suspended()
}

pub fn suspend_system(hwnd: HWND, reason: u32) {
    let previous = SUSPEND_REASONS.suspend(reason);
    MONITOR_FULLSCREEN.store(false, Ordering::Release);
    let _ = sync_monitoring_timers(hwnd);
    if previous == 0 {
        trim_working_set();
    }
}

pub fn resume_system(hwnd: HWND, reason: u32, reset_backoff: bool) {
    SUSPEND_REASONS.resume(reason);
    if reset_backoff {
        CONSECUTIVE_ZERO_COUNT.store(0, Ordering::Release);
        NETWORK_BACKOFF.store(false, Ordering::Release);
    }
    let _ = sync_monitoring_timers(hwnd);
}

/// WM_POWERBROADCAST 处理：系统休眠/唤醒、显示器开关。
pub fn handle_power_broadcast(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match wparam.0 as u32 {
        PBT_APMSUSPEND => {
            suspend_system(hwnd, SUSPEND_REASON_SYSTEM);
        }
        PBT_APMRESUMEAUTOMATIC => {
            resume_system(hwnd, SUSPEND_REASON_SYSTEM, true);
        }
        PBT_POWERSETTINGCHANGE => {
            let setting = lparam.0 as *const POWERBROADCAST_SETTING;
            if !setting.is_null() {
                // SAFETY: PBT_POWERSETTINGCHANGE 时 OS 保证 lparam 指向有效结构。
                let setting_ref = unsafe { &*setting };
                if setting_ref.PowerSetting == GUID_MONITOR_POWER_ON && setting_ref.DataLength >= 1
                {
                    if setting_ref.Data[0] != 0 {
                        resume_system(hwnd, SUSPEND_REASON_MONITOR, true);
                    } else {
                        suspend_system(hwnd, SUSPEND_REASON_MONITOR);
                    }
                }
            }
        }
        _ => {}
    }
    LRESULT(0)
}

/// WM_WTSSESSION_CHANGE 处理：锁屏/解锁。
pub fn handle_session_change(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    match wparam.0 {
        WTS_SESSION_LOCK => {
            suspend_system(hwnd, SUSPEND_REASON_SESSION);
        }
        WTS_SESSION_UNLOCK => {
            resume_system(hwnd, SUSPEND_REASON_SESSION, true);
        }
        _ => {}
    }
    LRESULT(0)
}

/// 依据暂停原因、全屏状态和网络退避状态，将所有监测定时器收敛到唯一正确集合。
/// 返回 false 表示至少一个应创建的定时器创建失败。
pub fn sync_monitoring_timers(hwnd: HWND) -> bool {
    // 先统一移除，再按当前状态重建，避免调用方各自维护不完整的定时器子集。
    // SAFETY: hwnd 是主窗口句柄；移除不存在的定时器只会返回错误，不会破坏状态。
    unsafe {
        KillTimer(Some(hwnd), TIMER_ID_NETWORK).ok();
        KillTimer(Some(hwnd), TIMER_ID_CPU_MEM).ok();
        KillTimer(Some(hwnd), TIMER_ID_FULLSCREEN).ok();
    }

    if is_suspended() {
        return true;
    }

    // SAFETY: hwnd 有效，ID 和间隔均为进程内固定常量；返回 0 表示创建失败。
    let fullscreen_ok = unsafe {
        SetTimer(
            Some(hwnd),
            TIMER_ID_FULLSCREEN,
            TIMER_INTERVAL_FULLSCREEN,
            None,
        ) != 0
    };

    if MONITOR_FULLSCREEN.load(Ordering::Acquire) {
        return fullscreen_ok;
    }

    let network_interval = if NETWORK_BACKOFF.load(Ordering::Acquire) {
        TIMER_INTERVAL_NETWORK_BACKOFF
    } else {
        TIMER_INTERVAL_NETWORK
    };
    // SAFETY: hwnd 有效，ID 和间隔均为进程内固定常量；返回 0 表示创建失败。
    let network_ok = unsafe { SetTimer(Some(hwnd), TIMER_ID_NETWORK, network_interval, None) != 0 };
    // SAFETY: 同上，创建 CPU/内存定时器。
    let cpu_mem_ok = unsafe { SetTimer(Some(hwnd), TIMER_ID_CPU_MEM, CPU_MEM_INTERVAL, None) != 0 };

    fullscreen_ok && network_ok && cpu_mem_ok
}

pub fn check_fullscreen(hwnd: HWND) {
    let foreground = unsafe { GetForegroundWindow() };
    let is_invalid = foreground.is_invalid();
    let is_desktop_or_shell =
        unsafe { GetDesktopWindow() == foreground || GetShellWindow() == foreground };

    if is_invalid || is_desktop_or_shell || foreground == hwnd {
        let was = MONITOR_FULLSCREEN.load(Ordering::Acquire);
        if was {
            MONITOR_FULLSCREEN.store(false, Ordering::Release);
            let _ = sync_monitoring_timers(hwnd);
        }
        return;
    }

    let mut rect = RECT::default();
    let _ = unsafe { GetWindowRect(foreground, &mut rect) };

    // 前台窗口所在显示器 vs 任务栏所在显示器，仅同屏全屏才暂停。
    let hmon_fg = unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) };
    let mut mi_fg = MONITORINFOEXW::default();
    mi_fg.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: cbSize 已设；GetMonitorInfoW 写入 mi_fg。
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

    let same_monitor = match get_taskbar_hwnd() {
        Some(h_taskbar) => {
            let hmon_tb = unsafe { MonitorFromWindow(h_taskbar, MONITOR_DEFAULTTONEAREST) };
            hmon_fg == hmon_tb
        }
        None => false,
    };

    let was = MONITOR_FULLSCREEN.load(Ordering::Acquire);
    let should_suspend = is_full && same_monitor;
    MONITOR_FULLSCREEN.store(should_suspend, Ordering::Release);

    if should_suspend != was {
        let _ = sync_monitoring_timers(hwnd);
    }
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
