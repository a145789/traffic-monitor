use std::sync::atomic::{AtomicBool, AtomicU32};

pub const APP_NAME: &str = "TrafficMonitor";
pub const WINDOW_CLASS: &str = "TrafficMonitorWnd\0";
pub const WINDOW_TITLE: &str = "Traffic Monitor\0";
pub const MUTEX_NAME: &str = "TrafficMonitor_Mutex_Instance\0";

pub const DISPLAY_WIDTH: i32 = 240;
pub const DISPLAY_HEIGHT: i32 = 32;
pub const GAP: i32 = -3;

pub const TIMER_ID_NETWORK: usize = 1;
pub const TIMER_ID_CPU_MEM: usize = 2;
pub const TIMER_ID_FULLSCREEN: usize = 3;
pub const TIMER_ID_INIT_TRIM: usize = 99;

pub const TIMER_INTERVAL_NETWORK: u32 = 1000;
pub const TIMER_INTERVAL_NETWORK_BACKOFF: u32 = 15000;
pub const TIMER_INTERVAL_FULLSCREEN: u32 = 2000;
pub const BACKOFF_ZERO_THRESHOLD: u32 = 5;

pub const MOUSE_VIDS: [u16; 2] = [0xA8A4, 0xA8A5];
pub const MOUSE_PID: u16 = 0x2255;
pub const MOUSE_USAGE_PAGE: u16 = 0xFF01;
pub const MOUSE_USAGE: u16 = 0x0010;

pub const MOUSE_POLL_INTERVAL_ONLINE: u64 = 180;
pub const MOUSE_POLL_INTERVAL_OFFLINE: u64 = 300;
pub const MOUSE_FAIL_THRESHOLD: u32 = 2;

/// 鼠标线程启动后的初始等待（秒），给 HID 栈一点稳定时间。
pub const MOUSE_THREAD_START_DELAY: u64 = 2;
/// 系统挂起/全屏期间鼠标线程的空转轮询节奏（秒）。
pub const MOUSE_SUSPENDED_POLL_INTERVAL: u64 = 5;

/// HID 通信时序常量（毫秒）。read_timeout 签名为 i32，故用 i32 类型。
/// 命令发送后等待设备响应的固定 sleep。
pub const HID_CMD_SETTLE_MS: i32 = 100;
/// 电量查询：发送命令前 drain 残留报告的超时上界。
pub const HID_BATTERY_DRAIN_TIMEOUT_MS: i32 = 100;
/// 电量查询：等待实时响应的读取超时。
pub const HID_BATTERY_READ_TIMEOUT_MS: i32 = 500;
/// DPI 查询：发送命令前 drain 残留报告的超时上界。
pub const HID_DPI_DRAIN_TIMEOUT_MS: i32 = 200;
/// DPI 查询：等待实时响应的读取超时（DPI 响应包较大，需更长窗口）。
pub const HID_DPI_READ_TIMEOUT_MS: i32 = 3000;

pub const DPI_SCALE_FACTOR: f64 = 1.173;

pub const COLOR_KEY: u32 = 0x00FF00FF;
pub const COLOR_DARK_TEXT: u32 = 0x00282828;
pub const COLOR_LIGHT_TEXT: u32 = 0x00FFFFFF;
pub const COLOR_LOW_BATTERY: u32 = 0x004444FF;

pub const FONT_BASE_SIZE: i32 = 13;

pub static MOUSE_ONLINE: AtomicBool = AtomicBool::new(false);
pub static SUSPENDED: AtomicBool = AtomicBool::new(false);
pub static FULLSCREEN: AtomicBool = AtomicBool::new(false);
pub static SHOW_MOUSE_INFO: AtomicBool = AtomicBool::new(false);
pub static ENABLE_AUTO_UPDATE: AtomicBool = AtomicBool::new(true);
pub static UPDATE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

pub const MENU_ID_SHOW_MOUSE: u32 = 1003;
pub const MENU_ID_RESTART_HID: u32 = 1004;
pub const MENU_ID_AUTO_UPDATE_TOGGLE: u32 = 1005;
pub const MENU_ID_CHECK_UPDATE_MANUAL: u32 = 1006;

pub static MOUSE_BATTERY_LEVEL: AtomicU32 = AtomicU32::new(0);
pub static MOUSE_IS_CHARGING: AtomicBool = AtomicBool::new(false);
pub static MOUSE_DPI_VALUE: AtomicU32 = AtomicU32::new(0);

pub static NET_SPEED_UP: AtomicU32 = AtomicU32::new(0);
pub static NET_SPEED_DOWN: AtomicU32 = AtomicU32::new(0);

pub static NETWORK_BACKOFF: AtomicBool = AtomicBool::new(false);
pub static CONSECUTIVE_ZERO_COUNT: AtomicU32 = AtomicU32::new(0);

pub static CPU_USAGE: AtomicU32 = AtomicU32::new(0);
pub static MEM_USAGE: AtomicU32 = AtomicU32::new(0);
