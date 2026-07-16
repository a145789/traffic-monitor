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
