//! 全局运行状态（原子变量）。
//!
//! 与 `config.rs` 分离：config 存放编译期常量，state 存放运行时可变的全局状态。
//! 所有字段使用 `Atomic*` 以保证跨线程安全访问。

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32};

/// 系统睡眠导致暂停。
pub const SUSPEND_REASON_SYSTEM: u32 = 1 << 0;
/// 会话锁定导致暂停。
pub const SUSPEND_REASON_SESSION: u32 = 1 << 1;
/// 显示器关闭导致暂停。
pub const SUSPEND_REASON_MONITOR: u32 = 1 << 2;

/// 当前生效的暂停原因位集合。仅当所有原因均清除后才恢复采集。
pub static SUSPEND_REASONS: AtomicU32 = AtomicU32::new(0);

/// 全屏应用在前台运行。
pub static FULLSCREEN: AtomicBool = AtomicBool::new(false);

/// 允许自动检查更新。
pub static ENABLE_AUTO_UPDATE: AtomicBool = AtomicBool::new(true);

/// 更新检查正在进行中（防止并发检查）。
pub static UPDATE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// 上行速率（B/s）。
pub static NET_SPEED_UP: AtomicU32 = AtomicU32::new(0);

/// 下行速率（B/s）。
pub static NET_SPEED_DOWN: AtomicU32 = AtomicU32::new(0);

/// 网速连续为零，进入退避模式。
pub static NETWORK_BACKOFF: AtomicBool = AtomicBool::new(false);

/// 连续零速计数器（用于触发退避）。
pub static CONSECUTIVE_ZERO_COUNT: AtomicU32 = AtomicU32::new(0);

/// CPU 使用率（0-100）。
pub static CPU_USAGE: AtomicU32 = AtomicU32::new(0);

/// 内存使用率（0-100）。
pub static MEM_USAGE: AtomicU32 = AtomicU32::new(0);

/// 热风险指数（0-100），预留给未来 UI/调试，当前 renderer 只用 THERMAL_STATE。
pub static THERMAL_RISK: AtomicU32 = AtomicU32::new(0);

/// 热风险状态机输出：0=Cool, 1=Warm, 2=Hot, 3=Critical。
pub static THERMAL_STATE: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
mod tests {
    //! 暂停原因状态机测试：使用局部 `AtomicU32` 复现 `suspend_system`/`resume_system`
    //! 对 `SUSPEND_REASONS` 使用的 `fetch_or`/`fetch_and` 协议，验证多个独立原因叠加时
    //! 只有全部清除后才解除暂停——避免真实场景下的误恢复 (BUG)。
    //!
    //! 不直接调用 `suspend_system`/`resume_system`，因为它们会触发 `sync_monitoring_timers`，
    //! 需要有效窗口句柄而无法在单元测试中执行。原子操作协议与生产代码完全一致。

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

    /// 锁屏 → 显示器关闭 → 显示器开启：必须仍暂停直到解锁。
    #[test]
    fn lock_then_monitor_off_then_monitor_on_still_suspended() {
        let state = AtomicU32::new(0);

        suspend(&state, SUSPEND_REASON_SESSION);
        assert!(suspended(&state), "锁屏后应暂停");

        suspend(&state, SUSPEND_REASON_MONITOR);
        assert!(suspended(&state), "锁屏+显示器关后应暂停");

        // 显示器开启不应解除暂停，因锁屏原因仍生效。
        resume(&state, SUSPEND_REASON_MONITOR);
        assert!(suspended(&state), "显示器开启但未解锁应仍暂停");

        // 解锁后才完全恢复。
        resume(&state, SUSPEND_REASON_SESSION);
        assert!(!suspended(&state), "解锁后应恢复");
    }

    /// 显示器关闭 → 锁屏 → 解锁：必须仍暂停直到显示器开启。
    #[test]
    fn monitor_off_then_lock_then_unlock_still_suspended() {
        let state = AtomicU32::new(0);

        suspend(&state, SUSPEND_REASON_MONITOR);
        assert!(suspended(&state), "显示器关后应暂停");

        suspend(&state, SUSPEND_REASON_SESSION);
        assert!(suspended(&state), "显示器关+锁屏后应暂停");

        // 解锁不应解除暂停，因显示器关闭原因仍生效。
        resume(&state, SUSPEND_REASON_SESSION);
        assert!(suspended(&state), "解锁但显示器仍关应仍暂停");

        // 显示器开启后才完全恢复。
        resume(&state, SUSPEND_REASON_MONITOR);
        assert!(!suspended(&state), "显示器开启后应恢复");
    }

    /// 三种原因全部叠加，必须按任意顺序全部清除才恢复。
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

    /// 重复清除同一原因不应误清其它原因（fetch_and 幂等性）。
    #[test]
    fn resume_idempotent_preserves_other_reasons() {
        let state = AtomicU32::new(0);

        suspend(&state, SUSPEND_REASON_SESSION | SUSPEND_REASON_MONITOR);
        resume(&state, SUSPEND_REASON_SESSION);
        assert!(suspended(&state), "MONITOR 原因仍在");

        resume(&state, SUSPEND_REASON_SESSION); // 重置已清位，无害。
        assert!(suspended(&state), "重置已清位后 MONITOR 仍在");

        resume(&state, SUSPEND_REASON_MONITOR);
        assert!(!suspended(&state));
    }

    /// 单一原因无叠加：清除后立即恢复。
    #[test]
    fn single_reason_clears_immediately() {
        let state = AtomicU32::new(0);

        suspend(&state, SUSPEND_REASON_SESSION);
        assert!(suspended(&state));
        resume(&state, SUSPEND_REASON_SESSION);
        assert!(!suspended(&state));
    }
}
