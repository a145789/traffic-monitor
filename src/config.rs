use std::sync::atomic::{AtomicBool, AtomicU32};

pub const APP_NAME: &str = "TrafficMonitor";
pub const WINDOW_CLASS: &str = "TrafficMonitorWnd\0";
pub const WINDOW_TITLE: &str = "Traffic Monitor\0";

pub const DISPLAY_WIDTH: i32 = 220;
pub const DISPLAY_HEIGHT: i32 = 32;
pub const GAP: i32 = 8;

pub const TIMER_ID_NETWORK: usize = 1;
pub const TIMER_ID_CPU_MEM: usize = 2;

pub const MOUSE_VID: u16 = 0x04D9;
pub const MOUSE_PID: u16 = 0xA02A;
pub const MOUSE_USAGE_PAGE: u16 = 0xFF00;
pub const MOUSE_USAGE: u16 = 0x0001;

pub const MOUSE_POLL_INTERVAL_ONLINE: u64 = 180;
pub const MOUSE_POLL_INTERVAL_OFFLINE: u64 = 300;
pub const MOUSE_FAIL_THRESHOLD: u32 = 2;

pub const COLOR_KEY: u32 = 0x00FF00FF;
pub const COLOR_DARK_TEXT: u32 = 0x00282828;
pub const COLOR_LIGHT_TEXT: u32 = 0x00FFFFFF;
pub const COLOR_LOW_BATTERY: u32 = 0x000000FF;
pub const LUMINANCE_THRESHOLD: f64 = 125.0;

pub static MOUSE_ONLINE: AtomicBool = AtomicBool::new(false);
pub static SUSPENDED: AtomicBool = AtomicBool::new(false);

pub static MOUSE_BATTERY_LEVEL: AtomicU32 = AtomicU32::new(0);
pub static MOUSE_IS_CHARGING: AtomicBool = AtomicBool::new(false);
pub static MOUSE_DPI_VALUE: AtomicU32 = AtomicU32::new(0);

pub static NET_SPEED_UP: AtomicU32 = AtomicU32::new(0);
pub static NET_SPEED_DOWN: AtomicU32 = AtomicU32::new(0);

pub static CPU_USAGE: AtomicU32 = AtomicU32::new(0);
pub static MEM_USAGE: AtomicU32 = AtomicU32::new(0);
