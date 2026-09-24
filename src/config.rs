//! 编译期常量：窗口尺寸、颜色、定时器 ID/间隔、自定义消息号、菜单 ID 等。
//!
//! 运行时可变状态见 `state.rs`。
//!
//! 含尾 `\0` 的字符串常量可直接 `encode_utf16().collect()` 交给 Win32；
//! 业务侧动态字符串请用 `util::to_wide`。

use windows::Win32::UI::WindowsAndMessaging::WM_USER;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 本次构建是否为 `scripts/package.ts <tag>` 打出的开发版。
///
/// 真值源是构建期注入的环境变量（见 `build.rs`），不是版本号后缀：打包脚本接受任意
/// tag，`x.y.z-<tag><ts>` 的后缀形状并不唯一对应「开发版」。常规与 CI 构建为 `false`；
/// 开发版不参与升级安装，手动检查更新时必须如实说明（见 `update::no_update_message`）。
pub const DEV_BUILD: bool = option_env!("TRAFFIC_MONITOR_DEV_BUILD").is_some();

pub const APP_NAME: &str = "TrafficMonitor";
/// 用户可见显示标题：MessageBox、托盘 tip、HTTP User-Agent 等字符串的统一来源。
pub const APP_TITLE: &str = "Traffic Monitor";
pub const WINDOW_CLASS: &str = "TrafficMonitorWnd\0";
pub const WINDOW_TITLE: &str = "Traffic Monitor\0";
/// 隐藏看门狗窗口类名：永不嵌入任务栏的顶层窗口，
/// 是唯一能可靠接收 TaskbarCreated 广播并触发主窗口重建的常驻接收者。
pub const WATCHDOG_CLASS: &str = "TrafficMonitorWatchdog\0";
pub const MUTEX_NAME: &str = "TrafficMonitor_Mutex_Instance\0";
/// 更新子进程专用互斥量名：同一会话内只允许一个 `--check-update` 子进程，
/// 后来者输出 `BUSY` 协议行并静默退出。不带 `Global\` 前缀即会话级，与
/// `MUTEX_NAME` 一致；跨会话并发的残留由缓存文件锁兜底（见 AGENTS.md 第 4 条）。
pub const UPDATE_MUTEX_NAME: &str = "TrafficMonitor_Mutex_Update\0";
/// 父身份绑定参数（父进程 PID 与其创建时刻 FILETIME 的十进制值）。
/// 只传 PID 不足以承载：长时间开机的机器上 PID 必然被复用，子进程拿不到原始
/// 创建时刻就无法判断 `OpenProcess` 打开的是不是自己的父进程。
pub const PARENT_PID_ARG: &str = "--parent-pid";
pub const PARENT_START_ARG: &str = "--parent-start";
pub const REG_PATH_APP: &str = "Software\\Traffic Monitor";
pub const REG_PATH_RUN: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
pub const REG_PATH_PERSONALIZE: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
/// 调试日志开关（DWORD：1=开，0/缺失=关，见 `util::log_event!`）。
pub const REG_VALUE_DEBUG_LOG: &str = "EnableDebugLog";
/// release 现场诊断日志位置：`%LOCALAPPDATA%\Traffic Monitor\debug.log`。
pub const DEBUG_LOG_DIR_NAME: &str = "Traffic Monitor";
pub const DEBUG_LOG_FILE_NAME: &str = "debug.log";
/// 日志环形截断上限（字节）：超限时只保留尾部一半。
pub const DEBUG_LOG_MAX_BYTES: u64 = 256 * 1024;
/// 连续写失败达此次数即在进程内自动关开关（注册表值不动，重启后重载）。
pub const DEBUG_LOG_DISABLE_AFTER_FAILURES: u32 = 3;

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
/// 请求主进程退出（`--quit` 的对外入口，投递给看门狗窗口）。
/// 主窗口嵌入任务栏后是跨进程子窗口，`FindWindowW` 检索不到它；看门狗是唯一
/// 全生命周期不重建的顶层窗口，退出请求必须以它为落点。
pub const WM_USER_QUIT_REQUEST: u32 = WM_USER + 6;
pub const WM_APP_TRAY: u32 = WM_USER + 100;

// 定时器 ID 只在其所属窗口内唯一：看门狗与主窗口各自独立计时，互不遮蔽。
pub const TIMER_ID_NETWORK: usize = 1;
pub const TIMER_ID_CPU_MEM: usize = 2;
pub const TIMER_ID_FULLSCREEN: usize = 3;
pub const TIMER_ID_AUTO_UPDATE: usize = 4;
/// 主窗口重建失败后的重试定时器，挂在看门狗窗口上（见 `arm_rebuild_retry`）。
pub const TIMER_ID_REBUILD_RETRY: usize = 5;
pub const TIMER_ID_INIT_TRIM: usize = 99;

pub const TIMER_INTERVAL_NETWORK: u32 = 1000;
pub const TIMER_INTERVAL_NETWORK_BACKOFF: u32 = 15000;
pub const TIMER_INTERVAL_FULLSCREEN: u32 = 2000;
/// 主窗口重建重试的首次间隔与上限（毫秒）：每次失败后翻倍至上限。
/// `TaskbarCreated` 每次任务栏创建只广播一次，重建失败后不重试等于永久失去主窗口。
pub const TIMER_INTERVAL_REBUILD_RETRY_MIN: u32 = 1000;
pub const TIMER_INTERVAL_REBUILD_RETRY_MAX: u32 = 60000;
pub const TIMER_INTERVAL_INIT_TRIM: u32 = 10000;
pub const CPU_MEM_INTERVAL: u32 = 5000;
pub const TIMER_COALESCING_TOLERANCE_MS: u32 = 100;
pub const BACKOFF_ZERO_THRESHOLD: u32 = 5;

/// 虚拟网卡黑名单缓存有效期（秒），避免每次采样重建。
pub const BLACKLIST_REFRESH_SECS: u64 = 30;

pub const VERSION_METADATA_MAX_BYTES: usize = 4 * 1024;
pub const INSTALLER_MAX_BYTES: usize = 256 * 1024 * 1024;
pub const HTTP_READ_CHUNK_BYTES: usize = 64 * 1024;
/// WinHTTP 四段超时（毫秒）：名称解析 / 连接 / 发送 / 接收统一取此值，两抓取路径共用。
pub const HTTP_TIMEOUT_MS: i32 = 15000;

/// 自动检查更新的正常冷却与失败后短冷却（秒）。
pub const AUTO_CHECK_COOLDOWN_SECS: u64 = 3600;
pub const AUTO_CHECK_ERROR_COOLDOWN_SECS: u64 = 300;
/// 启动安装包遇共享冲突类瞬态错误（如杀软实时扫描瞬时占用刚写完的文件）
/// 时的最大尝试次数与每次重试前的等待时长。
pub const INSTALLER_LAUNCH_MAX_ATTEMPTS: u32 = 3;
pub const INSTALLER_LAUNCH_RETRY_DELAY_MS: u64 = 400;

/// 版本文件抓取失败后的重试等待（毫秒）：防抖一次，避免抖动空转。
pub const UPDATE_FETCH_RETRY_DELAY_MS: u64 = 500;
/// 更新工作线程栈大小（字节）。
///
/// 该线程只做 `current_exe` + `Command::spawn` + 子进程 stdout 逐行扫描 + `wait`；
/// WinHTTP/BCrypt/MessageBox 全在 re-exec 出的子进程主线程上（见 AGENTS.md 第 4 条），
/// 不在本线程栈上。取 256KB 是因为 `Command::spawn` 的 CreateProcessW 路径在 64KB 下
/// 余量过薄，而栈触顶是 STATUS_STACK_OVERFLOW 直接终结进程、无日志可查。
/// 栈大小为**保留量**，Windows 按需提交页面，调大只占地址空间、不进工作集。
pub const UPDATE_WORKER_STACK_BYTES: usize = 256 * 1024;

/// 子进程发出 EXIT_MAIN 后等待主进程退出（单实例互斥量消失）的总超时与轮询间隔。
/// 超时后照常启动安装器，由安装器内 taskkill 兜底强杀。
/// 同一对常量亦被 `--quit` 的 `quit_existing_instance`（`main.rs`）共用：两处都是
/// “等主进程退净、上限 5 秒”，仅存在性探针不同（此处 `OpenMutexW` 互斥量消失，
/// 对方 `FindWindowW` 看门狗窗口消失），原先两处轮询间隔（50ms/100ms）之差无原则含义，
/// 故收口到同一对常量。
/// `installer.iss` 的 `GracefulWaitTimeoutMs` 与此同量级，三处调整须同步。
pub const MAIN_EXIT_WAIT_TIMEOUT_MS: u64 = 5000;
pub const MAIN_EXIT_POLL_INTERVAL_MS: u64 = 50;

/// 重新拉起主进程时携带的一次性参数：更新确认框刚被用户决策过（UAC 取消
/// 或安装器启动失败），拉起后的首个自动检查冷却周期被推迟，避免立刻
/// 再弹同一版本的确认框。
pub const RELAUNCHED_BY_UPDATE_ARG: &str = "--relaunched-by-update";

/// 临时安装包缓存有效期（秒），超时才在启动期清理。有效期内是否复用由
/// 哈希校验裁决（不匹配由 do_update_check 自行删除重下）；无条件删除会
/// 摧毁有效缓存，迫使每次检查都重新下载。
pub const INSTALLER_CACHE_MAX_AGE_SECS: u64 = 7 * 24 * 3600;

/// 自动更新的定时器轮询间隔。刻意远小于冷却时长：`sync_monitoring_timers` 在
/// 息屏/锁屏/全屏等状态切换时会销毁重建全部定时器，若轮询周期≈冷却时长，
/// 倒计时会被反复清零导致检查被无限推迟。因此定时器只做短周期轮询，
/// 是否真正发起检查完全由 `NEXT_CHECK_TIME` 冷却门唯一裁决。
pub const TIMER_INTERVAL_AUTO_UPDATE: u32 = 60 * 1000;

pub const COLOR_KEY: u32 = 0x00FF00FF;
pub const COLOR_DARK_TEXT: u32 = 0x00282828;
pub const COLOR_LIGHT_TEXT: u32 = 0x00FFFFFF;

pub const FONT_BASE_SIZE: i32 = 13;
/// GDI 逻辑字体面名（`LOGFONTW.lfFaceName` 来源，调用方经 `util::to_wide` 转宽串）。
/// 字重与品质见 `FONT_WEIGHT_NORMAL`；品质具名常量（`NONANTIALIASED_QUALITY`）
/// 由 `windows` crate 提供，避免裸数字构造该 newtype 时被改错位宽或取值。
pub const FONT_FACE_NAME: &str = "Segoe UI";
/// 常规字重 400（`LOGFONTW.lfWeight`）。
pub const FONT_WEIGHT_NORMAL: i32 = 400;

/// 托盘图标资源 ID（`MAKEINTRESOURCEW` 语义）：`assets/icon.ico` 经 `build.rs`
///（`winresource::set_icon`）写入资源表的默认 ID。三处命名关联，改 ID 须同步。
pub const TRAY_ICON_RESOURCE_ID: u16 = 1;

/// 流式 SHA-256 分块读缓冲（字节）：吞吐调参，只影响单次 `read` 大小，
/// 不改变哈希结果；调用方不得依赖该值做截断。
pub const HASH_READ_BUF_BYTES: usize = 8 * 1024;

pub const MENU_ID_AUTOSTART: u32 = 1001;
pub const MENU_ID_EXIT: u32 = 1002;
pub const MENU_ID_AUTO_UPDATE_TOGGLE: u32 = 1005;
pub const MENU_ID_CHECK_UPDATE_MANUAL: u32 = 1006;

/// 从 WPARAM/LPARAM 提取低 16 位（LOWORD）的掩码，用于菜单 ID 与托盘事件。
pub const LOWORD_MASK: u32 = 0xFFFF;
