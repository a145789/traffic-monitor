//! 编译期常量：窗口尺寸、颜色、定时器 ID/间隔等。
//!
//! 运行时可变状态见 `state.rs`。
//!
//! 含尾 `\0` 的字符串常量可直接 `encode_utf16().collect()` 交给 Win32；
//! 业务侧动态字符串请用 `util::to_wide`。

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
pub const TIMER_ID_INIT_TRIM: usize = 99;

pub const TIMER_INTERVAL_NETWORK: u32 = 1000;
pub const TIMER_INTERVAL_NETWORK_BACKOFF: u32 = 15000;
pub const TIMER_INTERVAL_FULLSCREEN: u32 = 2000;
pub const CPU_MEM_INTERVAL: u32 = 5000;
pub const BACKOFF_ZERO_THRESHOLD: u32 = 5;

pub const VERSION_METADATA_MAX_BYTES: usize = 4 * 1024;
pub const INSTALLER_MAX_BYTES: usize = 256 * 1024 * 1024;
pub const HTTP_READ_CHUNK_BYTES: usize = 64 * 1024;

pub const COLOR_KEY: u32 = 0x00FF00FF;
pub const COLOR_DARK_TEXT: u32 = 0x00282828;
pub const COLOR_LIGHT_TEXT: u32 = 0x00FFFFFF;

pub const FONT_BASE_SIZE: i32 = 13;

pub const MENU_ID_AUTOSTART: u32 = 1001;
pub const MENU_ID_EXIT: u32 = 1002;
pub const MENU_ID_AUTO_UPDATE_TOGGLE: u32 = 1005;
pub const MENU_ID_CHECK_UPDATE_MANUAL: u32 = 1006;

/// 从 WPARAM/LPARAM 提取低 16 位（LOWORD）的掩码，用于菜单 ID 与托盘事件。
pub const LOWORD_MASK: u32 = 0xFFFF;
