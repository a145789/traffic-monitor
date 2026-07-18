//! 全局运行状态（原子变量）。
//!
//! 与 `config.rs` 分离：config 存编译期常量，state 存运行时可变全局状态。
//!
//! ## 内存序约定
//! - **Relaxed**：单写多读的展示/开关类字段，或同一线程内定时器读写。
//! - **Acquire / Release / AcqRel**：跨线程握手（更新工作线程、句柄发布/订阅）。

use std::sync::atomic::{AtomicBool, AtomicU32};

/// 系统睡眠导致暂停。
pub const SUSPEND_REASON_SYSTEM: u32 = 1 << 0;
/// 会话锁定导致暂停。
pub const SUSPEND_REASON_SESSION: u32 = 1 << 1;
/// 显示器关闭导致暂停。
pub const SUSPEND_REASON_MONITOR: u32 = 1 << 2;

/// 暂停原因位集；仅当全部清除后才恢复采集。读写：AcqRel / Acquire。
pub static SUSPEND_REASONS: AtomicU32 = AtomicU32::new(0);

/// 本组件所在显示器上，前台窗口是否全屏（非系统全局全屏）。
/// 读写：Acquire / Release（定时器与全屏检测）。
pub static MONITOR_FULLSCREEN: AtomicBool = AtomicBool::new(false);

/// 允许自动检查更新。读写：Relaxed（非关键段开关，同进程）。
pub static ENABLE_AUTO_UPDATE: AtomicBool = AtomicBool::new(true);

/// 更新检查进行中。读写：AcqRel / Release（主线程与更新工作线程握手）。
pub static UPDATE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// 上行速率（B/s）。读写：Relaxed（采集写、渲染读）。
pub static NET_SPEED_UP: AtomicU32 = AtomicU32::new(0);

/// 下行速率（B/s）。读写：Relaxed。
pub static NET_SPEED_DOWN: AtomicU32 = AtomicU32::new(0);

/// 网速连续为零，进入退避。读写：Acquire / Release（与定时器重建握手）。
pub static NETWORK_BACKOFF: AtomicBool = AtomicBool::new(false);

/// 连续零速计数。读写：Relaxed（单写路径为主）。
pub static CONSECUTIVE_ZERO_COUNT: AtomicU32 = AtomicU32::new(0);

/// CPU 使用率（0-100）。读写：Relaxed（采集写、渲染读）。
pub static CPU_USAGE: AtomicU32 = AtomicU32::new(0);

/// 内存使用率（0-100）。读写：Relaxed。
pub static MEM_USAGE: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
mod tests {
    //! 暂停原因状态机测试。
    //!
    //! 使用局部 `AtomicU32` 镜像 `suspend_system`/`resume_system` 对
    //! `SUSPEND_REASONS` 的 `fetch_or`/`fetch_and` 协议。
    //! **改 `suspend.rs` 中的原子操作时必须同步改此测试**，否则会静默漂移。
    //!
    //! 不直接调用 suspend/resume：它们会 `sync_monitoring_timers`，需要有效 HWND。

    use super::{SUSPEND_REASON_MONITOR, SUSPEND_REASON_SESSION, SUSPEND_REASON_SYSTEM};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn suspended(state: &AtomicU32) -> bool {
        state.load(Ordering::Acquire) != 0
    }

    fn suspend(state: &AtomicU32, reason: u32) {
        state.fetch_or(reason, Ordering::AcqRel);
    }

    fn resume(state: &AtomicU32, reason: u32) {
        state.fetch_and(!reason, Ordering::AcqRel);
    }

    #[test]
    fn lock_then_monitor_off_then_monitor_on_still_suspended() {
        let state = AtomicU32::new(0);

        suspend(&state, SUSPEND_REASON_SESSION);
        assert!(suspended(&state), "锁屏后应暂停");

        suspend(&state, SUSPEND_REASON_MONITOR);
        assert!(suspended(&state), "锁屏+显示器关后应暂停");

        resume(&state, SUSPEND_REASON_MONITOR);
        assert!(suspended(&state), "显示器开启但未解锁应仍暂停");

        resume(&state, SUSPEND_REASON_SESSION);
        assert!(!suspended(&state), "解锁后应恢复");
    }

    #[test]
    fn monitor_off_then_lock_then_unlock_still_suspended() {
        let state = AtomicU32::new(0);

        suspend(&state, SUSPEND_REASON_MONITOR);
        assert!(suspended(&state), "显示器关后应暂停");

        suspend(&state, SUSPEND_REASON_SESSION);
        assert!(suspended(&state), "显示器关+锁屏后应暂停");

        resume(&state, SUSPEND_REASON_SESSION);
        assert!(suspended(&state), "解锁但显示器仍关应仍暂停");

        resume(&state, SUSPEND_REASON_MONITOR);
        assert!(!suspended(&state), "显示器开启后应恢复");
    }

    #[test]
    fn all_three_reasons_must_clear() {
        let state = AtomicU32::new(0);

        suspend(&state, SUSPEND_REASON_SYSTEM);
        suspend(&state, SUSPEND_REASON_SESSION);
        suspend(&state, SUSPEND_REASON_MONITOR);
        assert!(suspended(&state));

        resume(&state, SUSPEND_REASON_SYSTEM);
        assert!(suspended(&state), "剩两原因未清");

        resume(&state, SUSPEND_REASON_SESSION);
        assert!(suspended(&state), "剩一原因未清");

        resume(&state, SUSPEND_REASON_MONITOR);
        assert!(!suspended(&state), "全部清除后应恢复");
    }

    #[test]
    fn resume_idempotent_preserves_other_reasons() {
        let state = AtomicU32::new(0);

        suspend(&state, SUSPEND_REASON_SESSION | SUSPEND_REASON_MONITOR);
        resume(&state, SUSPEND_REASON_SESSION);
        assert!(suspended(&state), "MONITOR 原因仍在");

        resume(&state, SUSPEND_REASON_SESSION);
        assert!(suspended(&state), "重置已清位后 MONITOR 仍在");

        resume(&state, SUSPEND_REASON_MONITOR);
        assert!(!suspended(&state));
    }

    #[test]
    fn single_reason_clears_immediately() {
        let state = AtomicU32::new(0);

        suspend(&state, SUSPEND_REASON_SESSION);
        assert!(suspended(&state));
        resume(&state, SUSPEND_REASON_SESSION);
        assert!(!suspended(&state));
    }
}
