use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32};

pub const APP_NAME: &str = "TrafficMonitor";
pub const WINDOW_CLASS: &str = "TrafficMonitorWnd\0";
pub const WINDOW_TITLE: &str = "Traffic Monitor\0";
pub const MUTEX_NAME: &str = "TrafficMonitor_Mutex_Instance\0";

pub const DISPLAY_WIDTH: i32 = 170;
pub const DISPLAY_HEIGHT: i32 = 32;
pub const GAP: i32 = -3;

pub const TIMER_ID_NETWORK: usize = 1;
pub const TIMER_ID_CPU_MEM: usize = 2;
pub const TIMER_ID_FULLSCREEN: usize = 3;
pub const TIMER_ID_THERMAL: usize = 4;
pub const TIMER_ID_INIT_TRIM: usize = 99;

pub const TIMER_INTERVAL_NETWORK: u32 = 1000;
pub const TIMER_INTERVAL_NETWORK_BACKOFF: u32 = 15000;
pub const TIMER_INTERVAL_FULLSCREEN: u32 = 2000;
pub const TIMER_INTERVAL_THERMAL: u32 = 1000;
pub const BACKOFF_ZERO_THRESHOLD: u32 = 5;

pub const COLOR_KEY: u32 = 0x00FF00FF;
pub const COLOR_DARK_TEXT: u32 = 0x00282828;
pub const COLOR_LIGHT_TEXT: u32 = 0x00FFFFFF;
pub const COLOR_HOT_TEXT: u32 = 0x00008CFF;
pub const COLOR_CRIT_TEXT: u32 = 0x003030FF;

pub const FONT_BASE_SIZE: i32 = 13;

pub static SUSPENDED: AtomicBool = AtomicBool::new(false);
pub static FULLSCREEN: AtomicBool = AtomicBool::new(false);
pub static ENABLE_AUTO_UPDATE: AtomicBool = AtomicBool::new(true);
pub static UPDATE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

pub const MENU_ID_AUTO_UPDATE_TOGGLE: u32 = 1005;
pub const MENU_ID_CHECK_UPDATE_MANUAL: u32 = 1006;

/// 从 WPARAM/LPARAM 提取低 16 位（LOWORD）的掩码，用于菜单 ID 与托盘事件。
pub const LOWORD_MASK: u32 = 0xFFFF;

pub static NET_SPEED_UP: AtomicU32 = AtomicU32::new(0);
pub static NET_SPEED_DOWN: AtomicU32 = AtomicU32::new(0);

pub static NETWORK_BACKOFF: AtomicBool = AtomicBool::new(false);
pub static CONSECUTIVE_ZERO_COUNT: AtomicU32 = AtomicU32::new(0);

pub static CPU_USAGE: AtomicU32 = AtomicU32::new(0);
pub static MEM_USAGE: AtomicU32 = AtomicU32::new(0);

// ===== 热风险模型 (Thermal Risk) =====
// 针对 Meteor Lake H (Core Ultra 7 155H) + 16" 轻薄本标定。
// 其他机型需重新标定常量。拔电时用电池放电功率直测总发热,
// 插电时用 CPU/MEM/内核比多信号推断(有 GPU/NPU 盲区)。

pub static THERMAL_RISK: AtomicU32 = AtomicU32::new(0); // 0..=100, 预留给未来 UI/调试, 当前 renderer 只用 THERMAL_STATE
pub static THERMAL_STATE: AtomicU8 = AtomicU8::new(0);

pub const P_IDLE_PLUG_MW: i32 = 7000;
pub const A_CPU_MW_PER_PCT: i32 = 350;
pub const B_MEM_MW_PER_PCT: i32 = 100;
pub const C_KERNEL_HEAVY_MW: i32 = 8000;
pub const KERNEL_GATE_CPU_PCT: i32 = 30;
pub const KU_HEAVY_THRESHOLD_Q8: u32 = 384;
pub const EMA_ALPHA_FAST_Q8: u32 = 23;
pub const EMA_ALPHA_SLOW_Q8: u32 = 3;
pub const TREND_RISE_MW: i32 = 8000;
pub const TREND_FALL_MW: i32 = 5000;
pub const TREND_BONUS_UP: i32 = 10;
pub const TREND_BONUS_DN: i32 = -5;
pub const FP_BREAKS_MW: [i32; 5] = [0, 12000, 22000, 35000, 50000];
pub const FP_BREAKS_RISK: [i32; 5] = [0, 20, 50, 80, 100];
pub const ST_COOL_TO_WARM: u32 = 25;
pub const ST_WARM_TO_COOL: u32 = 15;
pub const ST_WARM_TO_HOT: u32 = 55;
pub const ST_HOT_TO_WARM: u32 = 45;
pub const ST_HOT_TO_CRIT: u32 = 85;
pub const ST_CRIT_TO_HOT: u32 = 75;
pub const ST_DWELL_SECS: u32 = 10;
