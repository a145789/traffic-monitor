//! 自动/手动检查更新、下载新版本安装包、SHA-256 校验、UAC 提权覆盖安装。
//!
//! 安装交接由临时目录里的自身副本执行（见 [`installer::spawn_install_helper`]）：
//! 协调者与主程序共用同一 exe 映像，必须先退出让出映像，安装器才能覆写主程序；
//! 「重验安装包→启动安装器→等收场→按需补拉起」整体在副本进程内完成
//! （`--update-install` 分支，入口 [`install_handoff_main`]）。
//!

mod cache;
mod crypto;
mod http;
mod installer;
mod protocol;
mod version;

pub use cache::init_cleanup_temp;

use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONINFORMATION, MB_YESNO, SW_SHOWNORMAL};
use windows::core::{PCWSTR, w};

use crate::config::{
    AUTO_CHECK_COOLDOWN_SECS, AUTO_CHECK_ERROR_COOLDOWN_SECS, DEV_BUILD, MAIN_EXIT_WAIT_TIMEOUT_MS,
    REG_PATH_APP, UPDATE_FETCH_RETRY_DELAY_MS, UPDATE_WORKER_STACK_BYTES, VERSION,
    VERSION_METADATA_MAX_BYTES,
};
use crate::state::{ENABLE_AUTO_UPDATE, UPDATE_IN_PROGRESS};
use crate::util::{
    compact_and_trim, configure_background_process, log_event, message_box, refresh_debug_log_flag,
    reg_read_dword, reg_read_string, reg_write_dword, reg_write_string, show_error, show_info,
    to_wide,
};

use cache::get_temp_installer_path;
use http::{FetchFileError, fetch_url};
use installer::{
    VerifiedInstaller, fetch_verified_installer, relaunch_main_app, relaunch_main_app_at,
    reverify_installer_for_handoff, run_install_handoff, spawn_install_helper,
    try_reuse_cached_installer, wait_main_instance_gone,
};
use protocol::{
    ParentProbe, UpdateContext, reset_update_progress_after_check, run_check_subprocess,
};
/// 子进程侧协议出口：`--check-update` 分支（`main.rs`）需要自己申请跨进程更新互斥
/// 并用 `BUSY` 收尾，故这两项提到 `update` 模块边界之外可见。
pub(crate) use protocol::{acquire_update_mutex, emit_protocol_line};
use version::{compare_versions, parse_update_metadata};

/// 仓库唯一来源：所有 GitHub 路径与 URL 均从这里派生，更换仓库只需改这一处。
/// 以宏而非 const 定义，因为 `concat!` 只接受字面量。
macro_rules! repo_owner_name {
    () => {
        "a145789/traffic-monitor"
    };
}
const GITHUB_HOST: &str = "github.com";
const PROXY_HOST: &str = "ghproxy.cn";
const GITHUB_REPOSITORY_URL: &str = concat!("https://github.com/", repo_owner_name!());
const RELEASE_PAGE_URL: &str = concat!("https://github.com/", repo_owner_name!(), "/releases");
const VERSION_PATH: &str = concat!(
    "/",
    repo_owner_name!(),
    "/releases/latest/download/version.txt"
);

/// 用户在更新确认框点「否」后记住的版本号（REG_SZ）。
/// 后续自动检查遇到同一版本不再弹框，直到出现更新的版本。
const REG_VALUE_SKIPPED_VERSION: &str = "SkippedUpdateVersion";

/// 下一次允许发起自动检查的时刻（deadline 语义，不是「上次检查时刻」）。
///
/// 唯一写方是 [`update_check_worker`] 与 [`defer_initial_auto_check`]，唯一读方是
/// [`start_auto_check`] 的冷却门。用 deadline 而非「上次检查时刻」表达，是因为后者为了
/// 表达更短的错误冷却必须「把时间戳往回推」，而 `Instant` 在 Windows 上以 QPC 为原点
/// （自系统启动计数），开机初期回推会 panic（见 [`next_check_deadline`]）。
static NEXT_CHECK_TIME: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));

pub fn load_auto_update_enabled() -> bool {
    reg_read_dword(REG_PATH_APP, "EnableAutoUpdate")
        .map(|v| v != 0)
        .unwrap_or(true)
}

pub fn save_auto_update_enabled(enabled: bool) {
    reg_write_dword(
        REG_PATH_APP,
        "EnableAutoUpdate",
        if enabled { 1 } else { 0 },
    );
}

fn read_skipped_version() -> Option<String> {
    reg_read_string(REG_PATH_APP, REG_VALUE_SKIPPED_VERSION)
}

fn record_skipped_version(version: &str) {
    reg_write_string(REG_PATH_APP, REG_VALUE_SKIPPED_VERSION, version);
}

/// 判断当前可执行文件是否位于安装版目录（父目录存在 `unins000.exe`）。
/// 仅安装版支持原地自更新；便携版提示用户去网页下载。
fn is_installed_version() -> bool {
    match std::env::current_exe() {
        Ok(exe) => match exe.parent() {
            Some(dir) => dir.join("unins000.exe").exists(),
            None => false,
        },
        Err(_) => false,
    }
}

pub fn start_auto_check() {
    if !ENABLE_AUTO_UPDATE.load(Ordering::Relaxed) {
        return;
    }

    if UPDATE_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return;
    }

    {
        let next = NEXT_CHECK_TIME.lock().unwrap();
        if let Some(t) = *next
            && Instant::now() < t
        {
            UPDATE_IN_PROGRESS.store(false, Ordering::Release);
            return;
        }
    }

    spawn_update_worker(false);
}

/// 手动检查（托盘菜单）。
///
/// 不接受 HWND：更新交接消息的落点是看门狗窗口（见 `post_update_action_to_watchdog`），
/// 它全生命周期不重建；主窗口句柄会在 Explorer 重启时失效，快照它必然丢消息。
pub fn start_manual_check() {
    // 更新相关提示必须全部由短生命周期子进程显示，避免 MessageBox/IME DLL
    // 因重复点击进入常驻主进程；已有检查运行时直接忽略本次点击。
    if UPDATE_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return;
    }

    spawn_update_worker(true);
}

/// spawn 更新工作线程；spawn 失败时复位进行中标志。
///
/// 仅负责线程创建与失败复位；自动检查的两道前置门（开关、冷却）保留在
/// `start_auto_check` 内，占坑与门序不因本函数改变。
fn spawn_update_worker(is_manual: bool) {
    if std::thread::Builder::new()
        .stack_size(UPDATE_WORKER_STACK_BYTES)
        .spawn(move || {
            update_check_worker(is_manual);
        })
        .is_err()
    {
        UPDATE_IN_PROGRESS.store(false, Ordering::Release);
    }
}

// 编译期契约：错误冷却不得长于正常冷却，否则「失败后重试」反而比「成功后等待」更久，
// 重试语义被反转。deadline 模型下它不再是 panic 防线（本模块已无任何 Instant 回推）。
const _: () = assert!(AUTO_CHECK_ERROR_COOLDOWN_SECS <= AUTO_CHECK_COOLDOWN_SECS);

/// 由「当前时刻 + 冷却时长」算出下次可检查时刻。
///
/// 一律只做加法（本仓的「禁止 Instant 回推」约束标记见 `collector::rate` 模块头）：
/// Windows 上 `Instant` 以 QPC 为原点、QPC 自系统启动计数，而 `Instant - Duration` 在结果
/// 不可表示时不是饱和而是 panic；release 是 `panic = "abort"`，会让整个常驻进程静默消失。
/// deadline 模型从结构上消除了这条路径，无需 `checked_sub` 与兜底魔法数。
fn next_check_deadline(now: Instant, is_error: bool) -> Instant {
    let cooldown_secs = if is_error {
        AUTO_CHECK_ERROR_COOLDOWN_SECS
    } else {
        AUTO_CHECK_COOLDOWN_SECS
    };
    now + Duration::from_secs(cooldown_secs)
}

fn update_check_worker(is_manual: bool) {
    let outcome = run_check_subprocess(is_manual);

    if outcome.busy {
        // 另一处更新进程占用了更新互斥量，本次一个请求都没发出去。
        //
        // 刻意不给用户任何提示（这是取舍，不是遗漏）：① 协议面要求这种占用静默收尾，
        // 验收场景 B 的手工 `--check-update --manual` 必须立即静默退出；② 更新相关提示
        // 不得回流常驻主进程（AGENTS.md 第 4 条，会常驻网络/UI DLL）；③ 若改由子进程弹框，
        // 父进程的更新工作线程会一直阻塞在 `child.wait()` 直到框被点掉，且框可能在主界面
        // 消失后成为孤儿框——正是本 RFC 要收的问题。只留日志，供「更新一直不动」时排查。
        log_event!("本次更新检查因另一处更新进程占用而跳过（BUSY）");
    }

    if !is_manual {
        let mut next = NEXT_CHECK_TIME.lock().unwrap();
        // 成功与失败都在检查完成后写入：deadline 一律从此刻起算，节奏与改造前一致。
        // BUSY 不是一次成功检查（见 should_use_error_cooldown），不得写成正常冷却。
        *next = Some(next_check_deadline(
            Instant::now(),
            should_use_error_cooldown(outcome.is_error, outcome.busy),
        ));
    }

    // 只有「读到 EXIT_MAIN」且消息已成功入队，主进程才会继续执行退出交接。
    if !reset_update_progress_after_check(&outcome) {
        return;
    }
    compact_and_trim();
}

fn should_use_error_cooldown(is_error: bool, busy: bool) -> bool {
    is_error || busy
}

/// 把启动期自动检查推迟一个冷却周期（relaunch 场景调用）。
///
/// 将 NEXT_CHECK_TIME 置为「现在 + 一个正常冷却」，使启动与断网重连触发的自动检查
/// 都命中冷却门；一个冷却周期后由定时器轮询恢复正常检查节奏。
pub fn defer_initial_auto_check() {
    let mut next = NEXT_CHECK_TIME.lock().unwrap();
    *next = Some(Instant::now() + Duration::from_secs(AUTO_CHECK_COOLDOWN_SECS));
}

#[derive(Debug)]
enum CheckResult {
    NoUpdate,
    PortableFound(String),
    InstalledReady(VerifiedInstaller),
    Error(String),
    /// R1：父进程已消失、且用户尚未确认安装 ⇒ 静默放弃（不下载、不弹框、不启安装器）。
    /// 与 `Error` 分开：它既不该弹错误框，也不该回落代理或重试。
    Abandoned,
}

fn do_update_check(is_manual: bool, ctx: &UpdateContext) -> CheckResult {
    // 元数据抓取前的第一次 R1 检查：父进程若已消失，后面每一步都是无人要的动作。
    if ctx.abandoned() {
        log_event!("父进程已退出且用户未确认安装，放弃本次更新检查");
        return CheckResult::Abandoned;
    }

    // 已知限制：元数据抓取本身**不带**取消谓词——检查点只覆盖安装包下载的每个数据块，
    // 这一次 4KiB 请求从建连到收完之间不可中断（`HttpGet::open` 同理），因此父进程若在
    // 该窗口内退出，「静默放弃」最坏要等这次抓取走完自身超时；该阶段无弹框、无写盘、
    // 不启动安装器，用户可见后果只是多一次无人消费的元数据请求。这里保证的是：
    // 抓取之间不再发起无人要的重试（见下面的重试前复判）。
    let mut response = fetch_url(GITHUB_HOST, VERSION_PATH, VERSION_METADATA_MAX_BYTES);
    if response.is_err() {
        // 失败时增加 1 次重试，并等待片刻防止抖动
        std::thread::sleep(Duration::from_millis(UPDATE_FETCH_RETRY_DELAY_MS));
        // 复判 R1：这 500ms 的等待里父进程可能已退出，规则要求父已消失就不再发起任何
        // 网络动作——不能因为「已经决定要重试」而把一个无人要的请求发出去。
        if ctx.abandoned() {
            log_event!("父进程已退出且用户未确认安装，放弃本次更新检查");
            return CheckResult::Abandoned;
        }
        response = fetch_url(GITHUB_HOST, VERSION_PATH, VERSION_METADATA_MAX_BYTES);
    }

    let response = match response {
        Ok(data) => data,
        Err(e) => return CheckResult::Error(format!("获取版本文件失败: {e}")),
    };

    let text = match String::from_utf8(response) {
        Ok(t) => t,
        Err(_) => return CheckResult::Error("版本文件编码不是 UTF-8".to_string()),
    };

    let metadata = match parse_update_metadata(&text) {
        Ok(m) => m,
        Err(e) => return CheckResult::Error(e),
    };
    let latest_version = metadata.version;
    let expected_hash_hex = metadata.hash_hex;

    let current_version = VERSION;
    if !compare_versions(current_version, &latest_version) {
        return CheckResult::NoUpdate;
    }

    // 自动检查跳过用户明确拒绝过的版本，且在下载前早退以省流量；
    // 手动检查不受限（用户主动发起，理应给出完整结果）。
    if !is_manual && read_skipped_version().as_deref() == Some(latest_version.as_str()) {
        return CheckResult::NoUpdate;
    }

    if !is_installed_version() {
        return CheckResult::PortableFound(latest_version.to_string());
    }

    let asset_path =
        format!("releases/download/v{latest_version}/TrafficMonitor-Setup-{latest_version}.exe");
    let download_path = format!("/{}/{asset_path}", repo_owner_name!());

    let temp_path = get_temp_installer_path();

    // 下载/复用前的第二次 R1 检查：元数据抓取带一次 500ms 重试，其间父进程可能已退出；
    // 命中后连缓存复用（可能要对 256MiB 缓存重算哈希）也不必再做。
    if ctx.abandoned() {
        log_event!("父进程已退出且用户未确认安装，放弃本次更新检查");
        return CheckResult::Abandoned;
    }

    // 缓存复用：先加锁再对锁定句柄哈希（见 try_reuse_cached_installer），
    // 同一句柄验证与持有，不按路径另开文件。缺失/占用/不匹配都落到重下。
    if let Some(verified) =
        try_reuse_cached_installer(temp_path.clone(), &latest_version, &expected_hash_hex)
    {
        return CheckResult::InstalledReady(verified);
    }

    let still_wanted = || !ctx.abandoned();

    // 主源下载失败才回落代理；校验/锁定等本地失败直接返回，不多下整包。
    // 流式写入全程不经过整包 Vec（见 fetch_verified_installer）。
    match fetch_verified_installer(
        &temp_path,
        GITHUB_HOST,
        &download_path,
        &expected_hash_hex,
        &latest_version,
        &still_wanted,
    ) {
        Ok(verified) => CheckResult::InstalledReady(verified),
        Err(FetchFileError::Download(e)) => {
            let proxy_path = format!("/{GITHUB_REPOSITORY_URL}/{asset_path}");
            match fetch_verified_installer(
                &temp_path,
                PROXY_HOST,
                &proxy_path,
                &expected_hash_hex,
                &latest_version,
                &still_wanted,
            ) {
                Ok(verified) => CheckResult::InstalledReady(verified),
                Err(FetchFileError::Download(pe)) => {
                    CheckResult::Error(format!("主源失败({e}), 代理源失败({pe})"))
                }
                // 主源下载已失败，代理本地失败：两段都报出，不只报后者。
                Err(FetchFileError::Local(pe)) => {
                    CheckResult::Error(format!("主源失败({e}), 代理源失败({pe})"))
                }
                // 两段都只在父进程消失时取消：静默放弃，不报「代理源失败」的假错误。
                Err(FetchFileError::Cancelled) => CheckResult::Abandoned,
            }
        }
        Err(FetchFileError::Local(e)) => CheckResult::Error(e),
        // 取消不是失败：不再回落代理、不再重试。
        Err(FetchFileError::Cancelled) => CheckResult::Abandoned,
    }
}

/// 子进程入口：完成更新检查、用户交互和外部动作，仅将最终动作回传主进程。
///
/// stdout 单行协议：
/// - `DONE`：子进程已处理完毕，主进程继续运行。
/// - `EXIT_MAIN`：用户确认安装。必须在子进程移交安装交接**之前**发出——主进程
///   看门狗收到并处理后，主进程退出并释放 exe 映像句柄；子进程等单实例互斥量消失后
///   把安装交接移交给临时目录里的自身副本（协调者与主程序共用同一 exe 映像，不退出
///   就无法让安装器覆写主程序；默认动词启动，提权由 SetupLdr 自己完成），从源头
///   消除「文件正在使用」竞态；安装器内 taskkill 仅作兜底。
/// - 无协议行：R1 静默放弃（父进程已消失且用户未确认），退出码非零。
///
/// 退出码：0 = 检查流程成功完成（含 `EXIT_MAIN` 交接），1 = 检查失败或 R1 静默放弃。
/// 手动检查失败时错误提示已由子进程显示；退出码只供主进程决定自动检查的重试冷却时间。
///
/// 父身份由 `parent_pid` / `parent_start` 两参数给出（见 [`UpdateContext`]）；
/// 任一缺失即退化为「无父可查」，`--check-update --manual` 的独立可用性不受影响。
pub fn subprocess_main(is_manual: bool, parent_pid: Option<u32>, parent_start: Option<u64>) -> i32 {
    // EcoQoS/低内存优先级只加给本短生命周期子进程，不拖慢常驻监控主进程。
    configure_background_process();
    refresh_debug_log_flag();

    let mut ctx = UpdateContext::new(parent_pid, parent_start);

    let result = do_update_check(is_manual, &ctx);
    let is_error = matches!(result, CheckResult::Error(_));
    match complete_update_interaction(result, is_manual, &mut ctx) {
        // EXIT_MAIN 已在启动安装器之前输出完毕；其余路径统一以 DONE 收尾。
        SubprocessEnd::Done => {
            if let Err(e) = emit_protocol_line("DONE") {
                // 无人在等这条行（父进程要么已收尾，要么已消失）：只留日志。
                log_event!("协议行 DONE 写出失败: {e}");
            }
            i32::from(is_error)
        }
        SubprocessEnd::ExitMain => 0,
        // R1 静默放弃：不输出任何协议行。父侧只会看到非零退出码，自然按失败记账
        // （错误冷却），不会把「没执行」记成「已检查过」。
        SubprocessEnd::Abandoned => 1,
    }
}

/// `--update-install` 分支入口：安装交接副本进程。
///
/// 由 [`spawn_install_helper`] 以临时目录里的自身副本 re-exec，交接载荷（安装包
/// 路径、期望哈希、补拉起目标）与协调者身份（`--parent-pid`/`--parent-start`）
/// 经命令行传入。副本不输出协议行、不做 R1 存活检查——R1/R2 不适用：用户已在
/// 确认框点「是」（R2 已成立），父身份在这里只用于等待协调者退出（见下）。
///
/// 退出码：0 = 交接流程走完（安装成功与否都算——失败侧已按客观事实补拉起主程序）；
/// 1 = 载荷残缺或安装包重验失败（有补拉起目标时已先恢复主程序）。
pub fn install_handoff_main(
    installer_path: Option<std::ffi::OsString>,
    installer_hash: Option<String>,
    app_exe: Option<std::ffi::OsString>,
    parent_pid: Option<u32>,
    parent_start: Option<u64>,
) -> i32 {
    configure_background_process();
    refresh_debug_log_flag();

    // 补拉起目标最先解出：载荷其余部分残缺时也要先把主程序还回来，不能让
    // 「点完『是』的组件」凭空消失。协调者与副本永远来自同一 exe（副本是协调者
    // 刚复制出去的自身），载荷残缺只可能来自命令行被外部改动，属防御性分支。
    let Some(app_exe) = app_exe else {
        log_event!("安装交接缺少补拉起目标参数，放弃交接");
        return 1;
    };
    let app_exe = std::path::PathBuf::from(app_exe);
    let Some(installer_path) = installer_path else {
        log_event!("安装交接缺少安装包路径参数，恢复主程序");
        let _ = relaunch_main_app_at(&app_exe);
        return 1;
    };
    let Some(expected_hash_hex) = installer_hash else {
        log_event!("安装交接缺少安装包哈希参数，恢复主程序");
        let _ = relaunch_main_app_at(&app_exe);
        return 1;
    };

    // 身份绑定不可省略：协调者的校验发生在另一个进程里，本副本若按路径直接启动
    // 安装器，等于重新打开 TOCTOU 窗口（AGENTS.md：校验后不得仅按路径另开文件
    // 重新建立信任）。对锁定句柄重算哈希并与期望值比对，验过才持锁启动。
    //
    // 这一步刻意排在「等协调者退出」之前：协调者若仍存活，它持有的安装包只读共享锁
    // 会把第三方写者挡在门外，这次打开因此能落在一个没有写者的窗口里。这只覆盖
    // 「协调者还没退完」的那一小段时序，不是保证；真正的兜底是
    // `reverify_installer_for_handoff` 内部的有限次重试。
    let Some(verified) = reverify_installer_for_handoff(
        std::path::PathBuf::from(installer_path),
        &expected_hash_hex,
    ) else {
        log_event!("安装交接: 安装包重验失败（缺失/占用/哈希不符），恢复主程序");
        // 先拉起再弹框：show_error 是模态的，组件不该等用户点掉框才回来。
        // 本分支可达（杀软隔离/占用安装包、缓存被清理），且用户刚在确认框点过「是」，
        // 必须给一个可见交代——只落 debug.log 等于「点了『是』却什么都没发生」。
        let _ = relaunch_main_app_at(&app_exe);
        show_error("更新安装包校验失败，已取消本次安装。\n组件已重新启动，请稍后重试更新。");
        return 1;
    };
    log_event!("安装交接: 安装包重验通过，等待协调者让出 exe 映像");

    // 客观事实门：协调者退出即 {app} 下 exe 映像锁释放。副本等到这一事实再移交
    // 安装器，不依赖「UAC 弹框留出秒级余量」这类时序假设（UAC 关闭或协调者已提权
    // 时不成立）。等待有上界：协调者若异常滞留，照常移交，由安装器侧
    // ForceKillRemnant 与原生文件占用提示兜底；绝不无限等——那会让交接卡死。
    {
        let probe = ParentProbe::bind(parent_pid, parent_start);
        if !probe.is_bound() {
            // `Unbound`（无身份可查）与 `Gone`（已退出）都会走到这里：两者都无需等待，
            // 文案不宣称是其中哪一种。
            log_event!("安装交接: 协调者不在或不可查，不等待，直接移交安装器");
        } else if probe.wait_for_exit(MAIN_EXIT_WAIT_TIMEOUT_MS as u32) {
            // 该常量编译期为 5000（见 config），远小于 u32 上限，收窄无损；
            // WaitForSingleObject 的超时参数本就是 u32（同 INSTALLER_EXIT_WAIT_TIMEOUT_MS
            // 取 u32 的理由）。
            log_event!("安装交接: 协调者已退出，exe 映像锁已释放");
        } else {
            log_event!("安装交接: 等待协调者退出超时，照常移交安装器（安装器侧兜底）");
        }
    }

    // 跨进程更新互斥刻意在「协调者已退出」之后申请：协调者此刻才释放它，这里是确定
    // 可得；提前到 `main()` 里申请只会拿到「已被占用」（副本还要先加载映像、读注册表，
    // 协调者多半尚未退完），让整段安装过程都失去这层保护。与 `--check-update` 同一把，
    // 防止交接窗口内并发的更新检查再启动第二套安装流程。占用/创建失败时交接照常继续
    // ——本进程是用户确认后的唯一交接者，因互斥量让位等于让已退出的主程序无人接管
    // （组件蒸发）。guard 活到本函数返回（即交接收尾，进程随即 exit），覆盖的正是安装窗口。
    let _update_mutex = acquire_update_mutex();
    if _update_mutex.is_none() {
        log_event!("安装交接: 更新互斥量被占用或创建失败，继续交接");
    }

    run_install_handoff(verified, &app_exe);
    0
}

/// 子进程本次检查的收尾方式（与协议行 `UpdateAction` 分开：R1 不产出协议行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubprocessEnd {
    /// 输出 `DONE`：检查流程结束，主进程继续运行。
    Done,
    /// 已输出 `EXIT_MAIN`：安装交接开始，主进程将按约定退出。
    ExitMain,
    /// R1 静默放弃：不输出协议行，退出码非零。
    Abandoned,
}

/// 手动检查「无更新」时的提示文案；`None` 表示不提示（自动检查不弹框）。
///
/// `DEV_BUILD` 为真的构建不参与升级安装，其版本号带 `-<tag><ts>` 后缀、永远解析不出
/// 版本三元组，因此 `NoUpdate` 是它的必然结果——此时说「已是最新版本」是可证伪的假话。
/// 判定读的是本地 [`VERSION`]（即 `CARGO_PKG_VERSION`），与远端 metadata 无关；`NoUpdate`
/// 的其它成因（远端更旧、远端解析失败）不受影响。
fn no_update_message(is_manual: bool, dev_build: bool, version: &str) -> Option<String> {
    if !is_manual {
        return None;
    }
    Some(if dev_build {
        "当前为开发版，不参与升级安装。".to_string()
    } else {
        format!("当前已是最新版本 (v{version})。")
    })
}

/// 用户交互与外部动作；返回本次检查的收尾方式。
///
/// 每个模态框（`show_info` / `show_yes_no`）返回之后都要再判一次 R1：模态框可能
/// 一直开到父进程退出，决定必须在框关闭之后做（见 [`UpdateContext::abandoned`]）。
/// 用户点「是」的那一刻起进入 R2，此后父进程是否还在都不再撤销交接。
fn complete_update_interaction(
    result: CheckResult,
    is_manual: bool,
    ctx: &mut UpdateContext,
) -> SubprocessEnd {
    match result {
        CheckResult::Abandoned => SubprocessEnd::Abandoned,
        CheckResult::NoUpdate => {
            if ctx.abandoned() {
                return SubprocessEnd::Abandoned;
            }
            if let Some(msg) = no_update_message(is_manual, DEV_BUILD, VERSION) {
                show_info(&msg);
                if ctx.abandoned() {
                    return SubprocessEnd::Abandoned;
                }
            }
            SubprocessEnd::Done
        }
        CheckResult::PortableFound(version) => {
            if ctx.abandoned() {
                return SubprocessEnd::Abandoned;
            }
            let msg = format!("发现新版本 v{version}。\n是否打开网页下载免安装版？");
            if show_yes_no(&msg) {
                // 用户已明确表达意图：后续动作不再因父进程消失而撤销（R2）。
                ctx.mark_user_confirmed();
                open_url(RELEASE_PAGE_URL);
            } else {
                record_skipped_version(&version);
            }
            if ctx.abandoned() {
                return SubprocessEnd::Abandoned;
            }
            SubprocessEnd::Done
        }
        CheckResult::InstalledReady(verified) => {
            // 弹安装确认框之前再判一次：父已消失就不把一个无人接管的问题抛给用户。
            if ctx.abandoned() {
                return SubprocessEnd::Abandoned;
            }
            let msg = format!(
                "新版本 v{} 已准备就绪。\n是否立即关闭程序并安装？",
                verified.version
            );
            if !show_yes_no(&msg) {
                record_skipped_version(&verified.version);
                // 与其余模态框返回点一致：父进程若已消失，本次结果已无人消费（R1）。
                if ctx.abandoned() {
                    return SubprocessEnd::Abandoned;
                }
                return SubprocessEnd::Done;
            }
            // R2 分界：用户已在模态框里点「是」，此后父进程是否还在都不再撤销交接。
            ctx.mark_user_confirmed();

            // 关键顺序：先发 EXIT_MAIN 让主进程退出让出 exe 映像，等单实例
            // 互斥量消失后再启动安装器；安装器的 taskkill 仅负责清理残存进程。
            if let Err(e) = emit_protocol_line("EXIT_MAIN") {
                if ctx.parent_alive() {
                    // 父进程仍在却收不到交接：它不会退出、也不会释放 exe 映像，
                    // 此时启动安装器必然撞上文件占用——按硬错误收尾，不启安装器。
                    //
                    // 这里刻意不复判 R1：用户已在上面点过「是」，`user_confirmed` 为真，
                    // `ctx.abandoned()` 在此恒为假（R2 的定义就是不因父进程消失而撤销
                    // 用户刚表达的意图），补一条只等于写死代码。该框是本此交接失败的唯一
                    // 可见提示，不能删；它是 R2 路径的错误提示，不受 R1「不弹框」约束。
                    log_event!("EXIT_MAIN 写出失败 ({e})，主进程仍在，取消本次安装");
                    show_error(&format!("无法通知主程序退出，已取消安装: {e}"));
                    return SubprocessEnd::Done;
                }
                // 父进程已消失：没有需要通知的对象，继续完成用户已确认的安装。
                log_event!("EXIT_MAIN 写出失败 ({e})，但主进程已退出，继续安装");
            } else {
                log_event!("已发出 EXIT_MAIN，等待主进程退出");
            }
            wait_main_instance_gone();

            // 安装交接移交：协调者与主程序是**同一个 exe** 的两个进程，若留在原地等
            // 安装器收场，复制阶段必然撞上本进程的映像占用（运行中的进程映像不可覆写，
            // v1.7.1→v1.7.2 升级「装完还是旧版」的根因）。把「重验安装包→启动安装器→
            // 等收场→按需补拉起」整体移交给临时目录里的自身副本，本进程随即退出让出
            // 映像；「谁负责拉起」的裁决随移交一起搬进副本（见
            // `installer::run_install_handoff`）。
            match spawn_install_helper(&verified) {
                Ok(()) => log_event!("安装交接已移交给临时副本进程，协调者退出让出 exe 映像"),
                Err(e) => {
                    // 副本没起来就没有任何人继续交接：主程序已按约定退出，必须在这里
                    // 把它还回来。先拉起再弹框——show_error 是模态的，顺序反了会让
                    // 组件在用户点掉框之前一直缺席。弹框与安装器启动失败同风格，
                    // 这是本次交接失败的唯一可见提示，不能删。
                    log_event!("安装交接副本启动失败 ({e})，重新拉起主程序");
                    relaunch_main_app();
                    show_error(&format!("启动更新交接失败: {e}"));
                }
            }
            SubprocessEnd::ExitMain
        }
        CheckResult::Error(message) => {
            log_event!("更新检查失败: {message}");
            // R1 必须先于任何弹框：父进程已消失时这次失败结果无人消费，
            // 不能出现「主界面已经关掉，却还冒出『检查更新失败』」的模态框。
            if ctx.abandoned() {
                return SubprocessEnd::Abandoned;
            }
            if is_manual {
                show_error(&format!("检查更新失败: {message}"));
                // 模态框返回后再判一次：框可能一直开到父进程退出。
                if ctx.abandoned() {
                    return SubprocessEnd::Abandoned;
                }
            }
            SubprocessEnd::Done
        }
    }
}

/// 执行更新交接的退出语义：复位进行中标志并进入退出序列。
///
/// 仅由看门狗过程处理 `WM_USER_UPDATE_ACTION` 时调用；看门狗与主窗口同属 UI 消息
/// 循环线程，因此线程前提（`PostQuitMessage` 面向当前线程）成立。本路径不触碰
/// `EXIT_REQUESTED`，[`crate::begin_exit`] 的幂等门会正常放行。
pub fn handle_update_action() {
    UPDATE_IN_PROGRESS.store(false, Ordering::Release);

    crate::begin_exit();
}

fn show_yes_no(msg: &str) -> bool {
    message_box(msg, MB_YESNO | MB_ICONINFORMATION) == IDYES
}

fn open_url(url: &str) {
    let url_wide = to_wide(url);
    // SAFETY: url_wide 含尾 NUL，ShellExecuteW 同步返回前存活。
    unsafe {
        let _ = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(url_wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Instant 的原点不可控（Windows 上就是 QPC 的开机计数），无法在用例里构造「接近
    // 时钟原点」的时刻，因此这里只钉 delta。下溢的消失由「本模块不存在 Instant 回推」
    // 这一结构事实承担，不靠用例假装覆盖。
    #[test]
    fn next_check_deadline_delta_matches_cooldown() {
        let now = Instant::now();
        assert_eq!(
            next_check_deadline(now, true),
            now + Duration::from_secs(AUTO_CHECK_ERROR_COOLDOWN_SECS)
        );
        assert_eq!(
            next_check_deadline(now, false),
            now + Duration::from_secs(AUTO_CHECK_COOLDOWN_SECS)
        );
    }

    #[test]
    fn next_check_deadline_is_strictly_in_the_future() {
        let now = Instant::now();
        assert!(next_check_deadline(now, true) > now);
        assert!(next_check_deadline(now, false) > now);
    }

    #[test]
    fn busy_result_must_not_take_the_normal_cooldown() {
        assert!(should_use_error_cooldown(false, true));
        assert!(should_use_error_cooldown(true, false));
        assert!(!should_use_error_cooldown(false, false));
    }

    #[test]
    fn no_update_message_is_silent_for_auto_check() {
        assert!(no_update_message(false, false, "1.6.0").is_none());
        assert!(no_update_message(false, true, "1.6.0-devk3x9zq").is_none());
    }

    #[test]
    fn no_update_message_is_honest_for_dev_build() {
        let msg = no_update_message(true, true, "1.6.0-devk3x9zq").expect("手动检查应给出文案");
        assert_eq!(msg, "当前为开发版，不参与升级安装。");
    }

    #[test]
    fn no_update_message_reports_current_version_for_release_build() {
        // 用字面 false 而不是 DEV_BUILD：单测必须与构建环境无关，环境里带了
        // TRAFFIC_MONITOR_DEV_BUILD 不该让这条变红。「标记真的只由 dev 打包注入」由
        // scripts/package.ts 的非 tag 分支显式清理 + 那次「带标记则该断言变红」的负例实测承担。
        let msg = no_update_message(true, false, "1.6.0").expect("手动检查应给出文案");
        assert_eq!(msg, "当前已是最新版本 (v1.6.0)。");
    }
}
