//! 编译期常量：窗口尺寸、颜色、定时器 ID/间隔、自定义消息号、菜单 ID 等。
//!
//! 运行时可变状态见 `state.rs`。
//!
//! 含尾 `\0` 的字符串常量可直接 `encode_utf16().collect()` 交给 Win32；
//! 业务侧动态字符串请用 `util::to_wide`。

use windows::Win32::UI::WindowsAndMessaging::WM_USER;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const APP_NAME: &str = "TrafficMonitor";
pub const WINDOW_CLASS: &str = "TrafficMonitorWnd\0";
pub const WINDOW_TITLE: &str = "Traffic Monitor\0";
pub const MUTEX_NAME: &str = "TrafficMonitor_Mutex_Instance\0";

pub const DISPLAY_WIDTH: i32 = 170;
pub const DISPLAY_HEIGHT: i32 = 32;
pub const GAP: i32 = -3;

// --- 布局基准（96 DPI 物理像素，渲染时按当前 DPI 缩放） ---
pub const LAYOUT_COL_GAP: i32 = 13;
pub const LAYOUT_SPEED_MARGIN: i32 = 4;
pub const LAYOUT_COL_WIDTH: i32 = 76;

// --- 自定义窗口消息（WM_USER 偏移，全进程唯一，禁止重复取值） ---
pub const WM_USER_NETWORK_DISCONNECTED: u32 = WM_USER + 3;
pub const WM_USER_NETWORK_RECONNECTED: u32 = WM_USER + 4;
pub const WM_USER_UPDATE_ACTION: u32 = WM_USER + 5;
pub const WM_APP_TRAY: u32 = WM_USER + 100;

pub const TIMER_ID_NETWORK: usize = 1;
pub const TIMER_ID_CPU_MEM: usize = 2;
pub const TIMER_ID_FULLSCREEN: usize = 3;
pub const TIMER_ID_INIT_TRIM: usize = 99;

pub const TIMER_INTERVAL_NETWORK: u32 = 1000;
pub const TIMER_INTERVAL_NETWORK_BACKOFF: u32 = 15000;
pub const TIMER_INTERVAL_FULLSCREEN: u32 = 2000;
pub const TIMER_INTERVAL_INIT_TRIM: u32 = 10000;
pub const CPU_MEM_INTERVAL: u32 = 5000;
pub const BACKOFF_ZERO_THRESHOLD: u32 = 5;

/// 虚拟网卡黑名单缓存有效期（秒），避免每次采样重建。
pub const BLACKLIST_REFRESH_SECS: u64 = 30;

pub const VERSION_METADATA_MAX_BYTES: usize = 4 * 1024;
pub const INSTALLER_MAX_BYTES: usize = 256 * 1024 * 1024;
pub const HTTP_READ_CHUNK_BYTES: usize = 64 * 1024;

/// 自动检查更新的正常冷却与失败后短冷却（秒）。
pub const AUTO_CHECK_COOLDOWN_SECS: u64 = 3600;
pub const AUTO_CHECK_ERROR_COOLDOWN_SECS: u64 = 300;

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
