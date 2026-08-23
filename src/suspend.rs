//! 系统暂停/恢复、全屏检测与主题变更检测。
//!
//! 负责管理定时器的启停（休眠/锁屏/全屏时暂停），以节省 CPU 资源；
//! 电源广播（WM_POWERBROADCAST）与锁屏（WM_WTSSESSION_CHANGE）消息在此处理。

use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::System::SystemServices::GUID_MONITOR_POWER_ON;
use windows::Win32::UI::WindowsAndMessaging::{
    GetDesktopWindow, GetForegroundWindow, GetShellWindow, GetWindowRect, KillTimer,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE, SetCoalescableTimer,
};

use crate::config::{
    CPU_MEM_INTERVAL, TIMER_COALESCING_TOLERANCE_MS, TIMER_ID_AUTO_UPDATE, TIMER_ID_CPU_MEM,
    TIMER_ID_FULLSCREEN, TIMER_ID_MEMORY_MAINTENANCE, TIMER_ID_NETWORK, TIMER_INTERVAL_AUTO_UPDATE,
    TIMER_INTERVAL_FULLSCREEN, TIMER_INTERVAL_MEMORY_MAINTENANCE, TIMER_INTERVAL_NETWORK,
    TIMER_INTERVAL_NETWORK_BACKOFF,
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

/// 挂起/全屏期间分层窗口表面可能被系统丢弃（显示模式变化、RDP 重连、DWM 重置），
/// 而数值未变时增量重绘不会触发；恢复后强制整幅重绘以自愈陈旧画面。
fn force_repaint(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
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
    force_repaint(hwnd);
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

#[derive(Debug, PartialEq, Eq)]
struct TimerPlan {
    fullscreen: bool,
    network_interval: Option<u32>,
    cpu_mem: bool,
    auto_update: bool,
    memory_maintenance: bool,
}

/// 纯函数决定当前状态下应存在的定时器集合，供状态机测试覆盖暂停/恢复对称性。
fn timer_plan(suspended: bool, fullscreen: bool, network_backoff: bool) -> TimerPlan {
    if suspended {
        return TimerPlan {
            fullscreen: false,
            network_interval: None,
            cpu_mem: false,
            auto_update: false,
            memory_maintenance: false,
        };
    }

    if fullscreen {
        return TimerPlan {
            fullscreen: true,
            network_interval: None,
            cpu_mem: false,
            auto_update: false,
            memory_maintenance: false,
        };
    }

    TimerPlan {
        fullscreen: true,
        network_interval: Some(if network_backoff {
            TIMER_INTERVAL_NETWORK_BACKOFF
        } else {
            TIMER_INTERVAL_NETWORK
        }),
        cpu_mem: true,
        auto_update: true,
        memory_maintenance: true,
    }
}

/// 依据暂停原因、全屏状态和网络退避状态，将所有周期任务定时器收敛到唯一正确集合。
///
/// 返回值仅反映**核心监测定时器**（全屏检测/网络/CPU 内存）的创建结果：
/// 任一失败返回 false。辅助定时器（自动更新、内存维护）为 best-effort，
/// 失败被刻意忽略——它们不影响监测主功能，不应触发错误弹窗或窗口退出。
pub fn sync_monitoring_timers(hwnd: HWND) -> bool {
    let plan = timer_plan(
        is_suspended(),
        MONITOR_FULLSCREEN.load(Ordering::Acquire),
        NETWORK_BACKOFF.load(Ordering::Acquire),
    );

    // 先统一移除，再按当前状态重建，避免调用方各自维护不完整的定时器子集。
    // SAFETY: hwnd 是主窗口句柄；移除不存在的定时器只会返回错误，不会破坏状态。
    unsafe {
        KillTimer(Some(hwnd), TIMER_ID_NETWORK).ok();
        KillTimer(Some(hwnd), TIMER_ID_CPU_MEM).ok();
        KillTimer(Some(hwnd), TIMER_ID_FULLSCREEN).ok();
        KillTimer(Some(hwnd), TIMER_ID_AUTO_UPDATE).ok();
        KillTimer(Some(hwnd), TIMER_ID_MEMORY_MAINTENANCE).ok();
    }

    let fullscreen_ok = if plan.fullscreen {
        set_coalescable_timer(hwnd, TIMER_ID_FULLSCREEN, TIMER_INTERVAL_FULLSCREEN)
    } else {
        true
    };

    let network_ok = plan
        .network_interval
        .is_none_or(|interval| set_coalescable_timer(hwnd, TIMER_ID_NETWORK, interval));
    let cpu_mem_ok = if plan.cpu_mem {
        set_coalescable_timer(hwnd, TIMER_ID_CPU_MEM, CPU_MEM_INTERVAL)
    } else {
        true
    };

    // 这些是辅助功能，失败不应让核心监测窗口退出或弹出错误框。
    if plan.auto_update {
        let _ = set_coalescable_timer(hwnd, TIMER_ID_AUTO_UPDATE, TIMER_INTERVAL_AUTO_UPDATE);
    }
    if plan.memory_maintenance {
        let _ = set_coalescable_timer(
            hwnd,
            TIMER_ID_MEMORY_MAINTENANCE,
            TIMER_INTERVAL_MEMORY_MAINTENANCE,
        );
    }

    fullscreen_ok && network_ok && cpu_mem_ok
}

fn set_coalescable_timer(hwnd: HWND, timer_id: usize, interval: u32) -> bool {
    // SAFETY: hwnd 由当前 UI 线程拥有；定时器 ID/间隔为受控常量；不使用回调函数。
    unsafe {
        SetCoalescableTimer(
            Some(hwnd),
            timer_id,
            interval,
            None,
            TIMER_COALESCING_TOLERANCE_MS,
        ) != 0
    }
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
            force_repaint(hwnd);
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
        if !should_suspend {
            force_repaint(hwnd);
        }
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
    use crate::config::AUTO_CHECK_COOLDOWN_SECS;
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

    // ===== timer_plan =====

    #[test]
    fn auto_update_poll_interval_must_be_far_below_cooldown() {
        // sync_monitoring_timers 每次状态切换（息屏/锁屏/全屏/网络事件）都会销毁重建
        // 全部定时器并使倒计时归零。若轮询周期≈自动检查冷却时长，事件频繁的机器上
        // 检查会被无限推迟；因此周期必须显著小于冷却，让 LAST_CHECK_TIME 冷却门
        // 成为唯一权威。此处以 1/10 冷却为上界钉死该关系。
        assert!(u64::from(TIMER_INTERVAL_AUTO_UPDATE) <= AUTO_CHECK_COOLDOWN_SECS * 1000 / 10);
    }

    #[test]
    fn test_timer_plan_suspended_has_no_timers() {
        assert_eq!(
            timer_plan(true, false, false),
            TimerPlan {
                fullscreen: false,
                network_interval: None,
                cpu_mem: false,
                auto_update: false,
                memory_maintenance: false,
            }
        );
    }

    #[test]
    fn test_timer_plan_fullscreen_only_keeps_detection_timer() {
        let plan = timer_plan(false, true, false);
        assert!(plan.fullscreen);
        assert_eq!(plan.network_interval, None);
        assert!(!plan.cpu_mem);
        assert!(!plan.auto_update);
        assert!(!plan.memory_maintenance);
    }

    #[test]
    fn test_timer_plan_normal_backoff_uses_slow_network_interval() {
        let plan = timer_plan(false, false, true);
        assert!(plan.fullscreen);
        assert_eq!(plan.network_interval, Some(TIMER_INTERVAL_NETWORK_BACKOFF));
        assert!(plan.cpu_mem);
        assert!(plan.auto_update);
        assert!(plan.memory_maintenance);
    }

    #[test]
    fn test_timer_plan_normal_online_uses_regular_network_interval() {
        let plan = timer_plan(false, false, false);
        assert_eq!(plan.network_interval, Some(TIMER_INTERVAL_NETWORK));
        assert!(plan.cpu_mem);
        assert!(plan.auto_update);
        assert!(plan.memory_maintenance);
    }
}
