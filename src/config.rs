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
/// 安装交接副本进程的模式参数：更新协调者在启动安装器前把自身复制到临时目录并以此
/// 参数 re-exec，随后立即退出让出主程序 exe 映像（运行中的进程映像不可被安装器覆写，
/// 协调者留在原地等安装器收场会让复制阶段必然失败，见 `update::installer`）。
/// 载荷由下面三个参数承载，在 `main()` 单例锁之前被拦截（与 `--check-update` 同一位置）。
pub const UPDATE_INSTALL_ARG: &str = "--update-install";
/// 交接载荷：已校验安装包的路径。副本必须对它重验哈希后才启动安装器（身份绑定不可
/// 按路径跳过）。
pub const INSTALLER_PATH_ARG: &str = "--installer-path";
/// 交接载荷：安装包的期望 SHA-256（hex，来自 version.txt 元数据），副本重验的比对基准。
pub const INSTALLER_HASH_ARG: &str = "--installer-hash";
/// 交接载荷：补拉起目标（主程序安装路径）。副本自身在临时目录，`current_exe()` 不是
/// 主程序，补拉起必须用这个显式路径。
pub const APP_EXE_ARG: &str = "--app-exe";
pub const REG_PATH_APP: &str = "Software\\Traffic Monitor";
pub const REG_PATH_RUN: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
pub const REG_PATH_PERSONALIZE: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
/// 调试日志开关（DWORD：1=开，0/缺失=关，见 `util::log_event!`）。
pub const REG_VALUE_DEBUG_LOG: &str = "EnableDebugLog";
pub const DEBUG_LOG_DIR_NAME: &str = "Traffic Monitor";
pub const DEBUG_LOG_FILE_NAME: &str = "debug.log";
pub const DEBUG_LOG_MAX_BYTES: u64 = 256 * 1024;
/// 连续写失败达此次数即在进程内自动关开关（注册表值不动，重启后重载）。
pub const DEBUG_LOG_DISABLE_AFTER_FAILURES: u32 = 3;

pub const DISPLAY_WIDTH: i32 = 170;
pub const DISPLAY_HEIGHT: i32 = 32;
pub const GAP: i32 = -3;

pub const LAYOUT_COL_GAP: i32 = 13;
pub const LAYOUT_SPEED_MARGIN: i32 = 4;
pub const LAYOUT_COL_WIDTH: i32 = 76;

pub const WM_USER_NETWORK_DISCONNECTED: u32 = WM_USER + 3;
pub const WM_USER_NETWORK_RECONNECTED: u32 = WM_USER + 4;
pub const WM_USER_UPDATE_ACTION: u32 = WM_USER + 5;
/// 请求主进程退出（`--quit` 的对外入口，投递给看门狗窗口）。
/// 主窗口嵌入任务栏后是跨进程子窗口，`FindWindowW` 检索不到它；看门狗是唯一
/// 全生命周期不重建的顶层窗口，退出请求必须以它为落点。
pub const WM_USER_QUIT_REQUEST: u32 = WM_USER + 6;
pub const WM_APP_TRAY: u32 = WM_USER + 100;

pub const TIMER_ID_NETWORK: usize = 1;
pub const TIMER_ID_CPU_MEM: usize = 2;
pub const TIMER_ID_FULLSCREEN: usize = 3;
pub const TIMER_ID_AUTO_UPDATE: usize = 4;
/// 主窗口重建失败后的重试定时器，挂在看门狗窗口上（见 `recovery::arm_rebuild_retry`）。
pub const TIMER_ID_REBUILD_RETRY: usize = 5;
/// 看门狗上的恢复调度 tick（见 `recovery::recovery_tick`）。
///
/// 刻意不进 `timer_plan`：挂起态与全屏态的监测定时器集合必须保持全空（销毁与
/// 恢复对称），而恢复调度在任何状态下都必须存在——看门狗永不参与挂起、永不重建，
/// 因此它的定时器不会随状态切换被清掉，这正是「唯一的自愈 tick 被一起杀掉」
/// 这一死结的解法。
pub const TIMER_ID_RECOVERY: usize = 6;
pub const TIMER_ID_INIT_TRIM: usize = 99;

pub const TIMER_INTERVAL_NETWORK: u32 = 1000;
pub const TIMER_INTERVAL_NETWORK_BACKOFF: u32 = 15000;
pub const TIMER_INTERVAL_FULLSCREEN: u32 = 2000;
/// 主窗口重建重试的首次间隔与上限（毫秒）：每次失败后翻倍至上限。
/// `TaskbarCreated` 每次任务栏创建只广播一次，重建失败后不重试等于永久失去主窗口。
pub const TIMER_INTERVAL_REBUILD_RETRY_MIN: u32 = 1000;
pub const TIMER_INTERVAL_REBUILD_RETRY_MAX: u32 = 60000;
/// 恢复调度 tick 的基础间隔与失败退避上限（毫秒）：一轮恢复动作全部成功即回到
/// 基础间隔，出现失败则翻倍至上限。恢复动作本身幂等，周期只为最终收敛服务；
/// 「失败即立刻重试」会把持续单点失败放大成高频轮询。
pub const TIMER_INTERVAL_RECOVERY: u32 = 60 * 1000;
pub const TIMER_INTERVAL_RECOVERY_MAX: u32 = 10 * 60 * 1000;
pub const TIMER_INTERVAL_INIT_TRIM: u32 = 10000;
pub const CPU_MEM_INTERVAL: u32 = 5000;
pub const TIMER_COALESCING_TOLERANCE_MS: u32 = 100;
pub const BACKOFF_ZERO_THRESHOLD: u32 = 5;

pub const BLACKLIST_REFRESH_SECS: u64 = 30;

pub const VERSION_METADATA_MAX_BYTES: usize = 4 * 1024;
pub const INSTALLER_MAX_BYTES: usize = 256 * 1024 * 1024;
pub const HTTP_READ_CHUNK_BYTES: usize = 64 * 1024;
/// WinHTTP 四段超时（毫秒）：名称解析 / 连接 / 发送 / 接收统一取此值，两抓取路径共用。
pub const HTTP_TIMEOUT_MS: i32 = 15000;

pub const AUTO_CHECK_COOLDOWN_SECS: u64 = 3600;
pub const AUTO_CHECK_ERROR_COOLDOWN_SECS: u64 = 300;
/// `SUSPEND_REASON_MONITOR`（显示器关闭）挂起位的保守超长 TTL（秒）。
///
/// 显示器开关在本仓库 feature 集内没有可靠的只读真值源（只在变化时推送），
/// 因此该位只能按「已置位够久」清理。12 小时刻意长于「整夜息屏/锁屏」这一最常见
/// 的合法长挂起场景：误恢复的代价是显示器关闭期间多跑 1 Hz 采样，且下一次点亮
/// 通知必然到达（可自愈）；误冻结的代价是组件永久假活、只能重启程序。
pub const SUSPEND_MONITOR_TTL_SECS: u64 = 12 * 60 * 60;
/// 「启动一个外部进程」失败时的最大尝试次数与每次重试前的等待时长，两处共用：
/// 启动安装包（`update::installer::launch_installer`）与重新拉起主程序
/// （`update::installer::relaunch_main_app`）。两者遇到的都是同一类瞬态失败——文件
/// 刚写完就被杀软实时扫描占用/替换——原先只有安装器那侧有重试，而 `relaunch_main_app`
/// 是静默更新交接唯一的拉起点，它静默失败就等于组件蒸发。
pub const INSTALLER_LAUNCH_MAX_ATTEMPTS: u32 = 3;
pub const INSTALLER_LAUNCH_RETRY_DELAY_MS: u64 = 400;

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
/// 同一对常量亦被另两处共用：`--quit` 的 `quit_existing_instance`（`main.rs`）等主进程
/// 退净，`main.rs` 的 `wait_for_relayed_instance` 等替代实例接管（同一把互斥量**出现**）。
/// 三处都是「等一个进程状态到位、上限 5 秒」，仅探针不同（此处与自去提权处用
/// `OpenMutexW` 看互斥量消失/出现，`--quit` 用 `FindWindowW` 看门狗窗口消失）；
/// 原先几处轮询间隔（50ms/100ms）之差无原则含义，故收口到同一对常量。
/// `installer.iss` 的 `GracefulWaitTimeoutMs` 与此同量级，各处调整须同步。
pub const MAIN_EXIT_WAIT_TIMEOUT_MS: u64 = 5000;
pub const MAIN_EXIT_POLL_INTERVAL_MS: u64 = 50;

/// 安装器收场后等待「组件实例已经在跑」的上限（毫秒）；轮询间隔复用
/// `MAIN_EXIT_POLL_INTERVAL_MS`。
///
/// 判据是单例互斥量而不是安装器退出码：安装成功时 `[Run]` 条目本应已把组件拉起来，
/// 但「静默模式下 `postinstall` 条目是否照常处理」这件事没有逐字保证，而设置
/// `skipifsilent` 又会让手动静默安装/升级失去唯一的拉起者。以互斥量为准可在两种语义下
/// 都得到正确答案：已经在跑就什么都不做，不在跑就补一次。
///
/// 刻意短于 `MAIN_EXIT_WAIT_TIMEOUT_MS`：这里判错的代价只是多拉起一个注定按重复实例
/// 静默退出的进程（单例互斥兜住），而拖长等待会让刚点过「是」的用户多等。
pub const INSTALLER_SETTLE_TAKEOVER_WAIT_MS: u64 = 2000;

/// 更新子进程等待安装器收场的上限（毫秒），直接交给 `WaitForSingleObject`，故为 `u32`。
///
/// 取得远大于任何健康安装（十几秒级）：上界不是用来"催"安装器的，只用来兜住它**永不收场**
/// 的那一种卡死——`/SUPPRESSMSGBOXES` 官方列明有 5 类消息框压不住，安装器若卡在其中之一，
/// 无上界等待会让更新子进程永久存活、跨进程更新互斥量永久被占，而组件早已随旧实例退出，
/// 正是要消灭的"永久冻结的假活"。超时按"结果未知"处置：重新拉起组件（此时安装器不是卡在
/// `ssInstall` 之前就是之后，拉起即可恢复），子进程随即退出、更新互斥量归还。
pub const INSTALLER_EXIT_WAIT_TIMEOUT_MS: u32 = 30 * 60 * 1000;

/// 重新拉起主进程时携带的一次性参数：更新确认框刚被用户决策过，或者安装器刚
/// 收场（成功/失败/被取消都算），拉起后的首个自动检查冷却周期被推迟，避免
/// 立刻再弹同一版本的确认框。
///
/// 注意它不是「本次实例是否由更新拉起」的判据：`installer.iss` 的 `[Run]` 条目
/// 不传任何参数，自去提权（`main::de_elevate_self`）因此不能以它作门。
pub const RELAUNCHED_BY_UPDATE_ARG: &str = "--relaunched-by-update";

/// 临时安装包缓存有效期（秒），超时才在启动期清理。有效期内是否复用由
/// 哈希校验裁决（不匹配由 do_update_check 自行删除重下）；无条件删除会
/// 摧毁有效缓存，迫使每次检查都重新下载。
pub const INSTALLER_CACHE_MAX_AGE_SECS: u64 = 7 * 24 * 3600;

/// 交接副本残留的启动期清理门槛（秒）：副本文件 mtime 距今超过该值才删。
/// 门控的不是「有效期」而是「活跃交接」：刚落盘的副本可能正处在协调者
/// 「复制 → spawn」的窗口内，另一实例的启动期清理若恰好插入会让 spawn 撞上
/// FILE_NOT_FOUND；保留数分钟即把该窗口完全排除在清理之外。真残留（交接结束
/// 后的垃圾）不会在紧接着的那次启动就被清掉——交接失败会立刻重新拉起组件，
/// 那次启动距落盘只有秒级，必然不足门槛；它在之后某次距落盘超过门槛的启动里
/// 被清。副本进程仍在跑时文件被映像占用，删除失败静默忽略，不影响本门槛。
pub const UPDATE_HELPER_STALE_SECS: u64 = 300;

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

pub const LOWORD_MASK: u32 = 0xFFFF;
