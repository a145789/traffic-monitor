//! 全局运行状态（原子变量）。
//!
//! 与 `config.rs` 分离：config 存编译期常量，state 存运行时可变全局状态。
//!
//! ## 内存序约定
//! - **Relaxed**：单写多读的展示/开关类字段，或同一线程内定时器读写。
//! - **Acquire / Release / AcqRel**：跨线程握手（更新工作线程、句柄发布/订阅）。
//!
//! `SUSPEND_REASONS` 的位协议（fetch_or/fetch_and + AcqRel）已封装在
//! [`SuspendReasons`] 的方法上，调用方不得绕过 API 直接操作内部原子量。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// 系统睡眠导致暂停。
pub const SUSPEND_REASON_SYSTEM: u32 = 1 << 0;
/// 会话锁定导致暂停。
pub const SUSPEND_REASON_SESSION: u32 = 1 << 1;
/// 显示器关闭导致暂停。
pub const SUSPEND_REASON_MONITOR: u32 = 1 << 2;

/// 暂停原因位集；仅当全部原因清除后才恢复采集。
pub struct SuspendReasons(AtomicU32);

impl SuspendReasons {
    pub const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    /// 是否处于暂停态（任一原因位置位）。
    pub fn is_suspended(&self) -> bool {
        self.0.load(Ordering::Acquire) != 0
    }

    /// 置位一个暂停原因，返回置位前的位集（供调用方判断是否为首次暂停）。
    pub fn suspend(&self, reason: u32) -> u32 {
        self.0.fetch_or(reason, Ordering::AcqRel)
    }

    /// 清除一个暂停原因；对已清除的位重复调用幂等。
    pub fn resume(&self, reason: u32) {
        self.0.fetch_and(!reason, Ordering::AcqRel);
    }
}

/// 全局暂停原因位集。读写：AcqRel / Acquire（见 [`SuspendReasons`] 方法注释）。
pub static SUSPEND_REASONS: SuspendReasons = SuspendReasons::new();

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

/// 连续零速计数。读写：Relaxed（采集线程计数）/ Release（主线程恢复路径清零）。
pub static CONSECUTIVE_ZERO_COUNT: AtomicU32 = AtomicU32::new(0);

/// CPU 使用率（0-100）。读写：Relaxed（采集写、渲染读）。
pub static CPU_USAGE: AtomicU32 = AtomicU32::new(0);

/// 内存使用率（0-100）。读写：Relaxed。
pub static MEM_USAGE: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
mod tests {
    //! 暂停原因状态机测试。
    //!
    //! 直接构造局部 [`SuspendReasons`] 实例验证真实位协议——测试与生产共用
    //! 同一实现，不存在镜像漂移问题。

    use super::{
        SUSPEND_REASON_MONITOR, SUSPEND_REASON_SESSION, SUSPEND_REASON_SYSTEM, SuspendReasons,
    };

    #[test]
    fn lock_then_monitor_off_then_monitor_on_still_suspended() {
        let state = SuspendReasons::new();

        state.suspend(SUSPEND_REASON_SESSION);
        assert!(state.is_suspended(), "锁屏后应暂停");

        state.suspend(SUSPEND_REASON_MONITOR);
        assert!(state.is_suspended(), "锁屏+显示器关后应暂停");

        state.resume(SUSPEND_REASON_MONITOR);
        assert!(state.is_suspended(), "显示器开启但未解锁应仍暂停");

        state.resume(SUSPEND_REASON_SESSION);
        assert!(!state.is_suspended(), "解锁后应恢复");
    }

    #[test]
    fn monitor_off_then_lock_then_unlock_still_suspended() {
        let state = SuspendReasons::new();

        state.suspend(SUSPEND_REASON_MONITOR);
        assert!(state.is_suspended(), "显示器关后应暂停");

        state.suspend(SUSPEND_REASON_SESSION);
        assert!(state.is_suspended(), "显示器关+锁屏后应暂停");

        state.resume(SUSPEND_REASON_SESSION);
        assert!(state.is_suspended(), "解锁但显示器仍关应仍暂停");

        state.resume(SUSPEND_REASON_MONITOR);
        assert!(!state.is_suspended(), "显示器开启后应恢复");
    }

    #[test]
    fn all_three_reasons_must_clear() {
        let state = SuspendReasons::new();

        state.suspend(SUSPEND_REASON_SYSTEM);
        state.suspend(SUSPEND_REASON_SESSION);
        state.suspend(SUSPEND_REASON_MONITOR);
        assert!(state.is_suspended());

        state.resume(SUSPEND_REASON_SYSTEM);
        assert!(state.is_suspended(), "剩两原因未清");

        state.resume(SUSPEND_REASON_SESSION);
        assert!(state.is_suspended(), "剩一原因未清");

        state.resume(SUSPEND_REASON_MONITOR);
        assert!(!state.is_suspended(), "全部清除后应恢复");
    }

    #[test]
    fn resume_idempotent_preserves_other_reasons() {
        let state = SuspendReasons::new();

        state.suspend(SUSPEND_REASON_SESSION | SUSPEND_REASON_MONITOR);
        state.resume(SUSPEND_REASON_SESSION);
        assert!(state.is_suspended(), "MONITOR 原因仍在");

        state.resume(SUSPEND_REASON_SESSION);
        assert!(state.is_suspended(), "重置已清位后 MONITOR 仍在");

        state.resume(SUSPEND_REASON_MONITOR);
        assert!(!state.is_suspended());
    }

    #[test]
    fn single_reason_clears_immediately() {
        let state = SuspendReasons::new();

        state.suspend(SUSPEND_REASON_SESSION);
        assert!(state.is_suspended());
        state.resume(SUSPEND_REASON_SESSION);
        assert!(!state.is_suspended());
    }
}
