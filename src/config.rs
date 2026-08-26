//! 编译期常量：窗口尺寸、颜色、定时器 ID/间隔、自定义消息号、菜单 ID 等。
//!
//! 运行时可变状态见 `state.rs`。
//!
//! 含尾 `\0` 的字符串常量可直接 `encode_utf16().collect()` 交给 Win32；
//! 业务侧动态字符串请用 `util::to_wide`。

use windows::Win32::UI::WindowsAndMessaging::WM_USER;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const APP_NAME: &str = "TrafficMonitor";
/// 用户可见显示标题：MessageBox、托盘 tip、HTTP User-Agent 等字符串的统一来源。
pub const APP_TITLE: &str = "Traffic Monitor";
pub const WINDOW_CLASS: &str = "TrafficMonitorWnd\0";
pub const WINDOW_TITLE: &str = "Traffic Monitor\0";
/// 隐藏看门狗窗口类名：永不嵌入任务栏的顶层窗口，
/// 是唯一能可靠接收 TaskbarCreated 广播并触发主窗口重建的常驻接收者。
pub const WATCHDOG_CLASS: &str = "TrafficMonitorWatchdog\0";
pub const MUTEX_NAME: &str = "TrafficMonitor_Mutex_Instance\0";
pub const REG_PATH_APP: &str = "Software\\Traffic Monitor";
pub const REG_PATH_RUN: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
pub const REG_PATH_PERSONALIZE: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";

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
pub const TIMER_ID_AUTO_UPDATE: usize = 4;
pub const TIMER_ID_MEMORY_MAINTENANCE: usize = 5;
pub const TIMER_ID_INIT_TRIM: usize = 99;

pub const TIMER_INTERVAL_NETWORK: u32 = 1000;
pub const TIMER_INTERVAL_NETWORK_BACKOFF: u32 = 15000;
pub const TIMER_INTERVAL_FULLSCREEN: u32 = 2000;
pub const TIMER_INTERVAL_INIT_TRIM: u32 = 10000;
pub const CPU_MEM_INTERVAL: u32 = 5000;
pub const TIMER_INTERVAL_MEMORY_MAINTENANCE: u32 = 60 * 1000;
pub const TIMER_COALESCING_TOLERANCE_MS: u32 = 100;
pub const BACKOFF_ZERO_THRESHOLD: u32 = 5;

/// 虚拟网卡黑名单缓存有效期（秒），避免每次采样重建。
pub const BLACKLIST_REFRESH_SECS: u64 = 30;

pub const VERSION_METADATA_MAX_BYTES: usize = 4 * 1024;
pub const INSTALLER_MAX_BYTES: usize = 256 * 1024 * 1024;
pub const HTTP_READ_CHUNK_BYTES: usize = 64 * 1024;

/// 自动检查更新的正常冷却与失败后短冷却（秒）。
pub const AUTO_CHECK_COOLDOWN_SECS: u64 = 3600;
pub const AUTO_CHECK_ERROR_COOLDOWN_SECS: u64 = 300;
/// 启动安装包遇共享冲突类瞬态错误（如杀软实时扫描瞬时占用刚写完的文件）
/// 时的最大尝试次数与每次重试前的等待时长。
pub const INSTALLER_LAUNCH_MAX_ATTEMPTS: u32 = 3;
pub const INSTALLER_LAUNCH_RETRY_DELAY_MS: u64 = 400;

/// 子进程发出 EXIT_MAIN 后等待主进程退出（单实例互斥量消失）的总超时与轮询间隔。
/// 超时后照常启动安装器，由安装器内 taskkill 兜底强杀。
pub const MAIN_EXIT_WAIT_TIMEOUT_MS: u64 = 5000;
pub const MAIN_EXIT_POLL_INTERVAL_MS: u64 = 50;

/// 自动更新的定时器轮询间隔。刻意远小于冷却时长：`sync_monitoring_timers` 在
/// 息屏/锁屏/全屏等状态切换时会销毁重建全部定时器，若轮询周期≈冷却时长，
/// 倒计时会被反复清零导致检查被无限推迟。因此定时器只做短周期轮询，
/// 是否真正发起检查完全由 `LAST_CHECK_TIME` 冷却门唯一裁决。
pub const TIMER_INTERVAL_AUTO_UPDATE: u32 = 60 * 1000;

/// 工作集水位的最低门槛。实际触发阈值为 `max(本值, 稳态基线 × 放大系数)`，
/// 稳态基线在每次 trim 后由下一个维护周期实测校准（见 `state::TrimBookkeeping`），
/// 避免静态阈值低于进程稳态工作集导致每个冷却期都全量清洗、反复制造缺页。
pub const WORKING_SET_TRIM_MIN_BYTES: usize = 6 * 1024 * 1024;
/// 相对上次实测稳态基线的增长百分比门槛（200 = 翻倍才 trim）。
pub const WORKING_SET_TRIM_BASELINE_GROWTH_PCT: u64 = 200;
/// 两次水位 trim 的最短间隔，避免共享页或 GDI 页无法立即回收时反复抖动。
pub const WORKING_SET_TRIM_COOLDOWN_SECS: u64 = 15 * 60;

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
