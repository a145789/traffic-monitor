//! 自动/手动检查更新、下载新版本安装包、SHA-256 校验、UAC 提权覆盖安装。
//!
//! 模块拆分：
//! - [`version`]：版本号解析与远端 metadata 严格解析（纯字符串处理）。
//! - [`http`]：WinHTTP 抓取与友好的中文错误映射。
//! - [`crypto`]：BCrypt SHA-256 哈希与 RAII 句柄守卫。
//! - 本文件：自动/手动编排、子进程协议（EXIT_MAIN 先于安装器启动）、安装器
//!   启动重试、注册表开关读写。

mod crypto;
mod http;
mod version;

use std::io::{BufRead, BufReader, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, GetLastError,
    LPARAM, WPARAM,
};
use windows::Win32::System::Threading::{MUTEX_ALL_ACCESS, OpenMutexW};
use windows::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SHELLEXECUTEINFOW, ShellExecuteExW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_ICONINFORMATION, MB_YESNO, PostMessageW, PostQuitMessage, SW_SHOWNORMAL,
};
use windows::core::{PCWSTR, w};

use crate::config::{
    AUTO_CHECK_COOLDOWN_SECS, AUTO_CHECK_ERROR_COOLDOWN_SECS, INSTALLER_CACHE_MAX_AGE_SECS,
    INSTALLER_LAUNCH_MAX_ATTEMPTS, INSTALLER_LAUNCH_RETRY_DELAY_MS, INSTALLER_MAX_BYTES,
    MAIN_EXIT_POLL_INTERVAL_MS, MAIN_EXIT_WAIT_TIMEOUT_MS, REG_PATH_APP, RELAUNCHED_BY_UPDATE_ARG,
    VERSION, VERSION_METADATA_MAX_BYTES, WM_USER_UPDATE_ACTION,
};
use crate::state::{ENABLE_AUTO_UPDATE, UPDATE_IN_PROGRESS};
use crate::tray::remove_tray_icon;
use crate::util::{
    compact_and_trim, configure_background_process, message_box, os_to_wide, reg_read_dword,
    reg_read_string, reg_write_dword, reg_write_string, show_error, show_info, to_wide,
};

use crypto::compute_sha256_hex_locked;
use http::{FetchFileError, fetch_to_file, fetch_url};
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
const TEMP_FILE_NAME: &str = "traffic-monitor-setup-temp.exe";

/// 用户在更新确认框点「否」后记住的版本号（REG_SZ）。
/// 后续自动检查遇到同一版本不再弹框，直到出现更新的版本。
const REG_VALUE_SKIPPED_VERSION: &str = "SkippedUpdateVersion";

/// 安装器文件以只读共享模式打开，阻止其他进程改写已校验文件。
const FILE_SHARE_READ_ONLY: u32 = 0x0000_0001;

static LAST_CHECK_TIME: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));

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

fn get_temp_installer_path() -> std::path::PathBuf {
    // var_os + PathBuf::from 无损：LOCALAPPDATA 含非 Unicode 可解码字符时，
    // var 会因非法 Unicode 返回 Err 而误走 temp 回退，有损中转则替换字符。
    std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Traffic Monitor")
        .join(TEMP_FILE_NAME)
}

/// 读取用户明确拒绝过的更新版本号；无记录返回 None。
fn read_skipped_version() -> Option<String> {
    reg_read_string(REG_PATH_APP, REG_VALUE_SKIPPED_VERSION)
}

/// 记住被拒绝的版本号，避免自动检查周期性重复弹同一版本的确认框；
/// 出现更新的版本后仍会正常提示。由子进程写入（与弹窗交互同进程）。
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

fn open_locked_installer(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ_ONLY)
        .open(path)
}

fn create_locked_installer(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ_ONLY)
        .open(path)
}

pub fn start_auto_check() {
    if !ENABLE_AUTO_UPDATE.load(Ordering::Relaxed) {
        return;
    }

    if UPDATE_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return;
    }

    {
        let last = LAST_CHECK_TIME.lock().unwrap();
        if let Some(t) = *last
            && t.elapsed().as_secs() < AUTO_CHECK_COOLDOWN_SECS
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
        .stack_size(64 * 1024)
        .spawn(move || {
            update_check_worker(is_manual);
        })
        .is_err()
    {
        UPDATE_IN_PROGRESS.store(false, Ordering::Release);
    }
}

fn update_check_worker(is_manual: bool) {
    let outcome = run_check_subprocess(is_manual);

    if !is_manual {
        let mut last = LAST_CHECK_TIME.lock().unwrap();
        if outcome.is_error {
            // 错误时把时间戳提前，仅保留较短冷却（错误冷却时长），避免短时间内重复失败。
            *last = Some(
                Instant::now()
                    - std::time::Duration::from_secs(
                        AUTO_CHECK_COOLDOWN_SECS - AUTO_CHECK_ERROR_COOLDOWN_SECS,
                    ),
            );
        } else {
            *last = Some(Instant::now());
        }
    }

    // 只有「读到 EXIT_MAIN」且「成功通知 UI」同时成立，主进程才真的要退出。
    if !reset_update_progress_after_check(&outcome) {
        return;
    }
    compact_and_trim();
}

/// worker 收尾判定：返回 true 表示本次检查已结束、调用方还需压缩内存。
///
/// 判定本身抽成纯函数（[`should_reset_update_progress`]）以便单测；
/// 本函数只负责把结论落到全局标志上。
fn reset_update_progress_after_check(outcome: &SubprocessOutcome) -> bool {
    if !should_reset_update_progress(outcome.exit_signalled, outcome.exit_forwarded) {
        return false;
    }
    UPDATE_IN_PROGRESS.store(false, Ordering::Release);
    true
}

/// 本次检查结束后是否必须复位进行中标志。
///
/// 只有「读到 EXIT_MAIN」且「通知已送达 UI」同时成立才免复位：此时主进程随即退出，
/// 复位反而与退出竞态。其余情况（含「读到 EXIT_MAIN 但通知未送达」）都必须复位：
/// 主进程仍在运行，不复位会让后续一切自动/手动检查被 `swap(true)` 永久挡掉，
/// 直到用户重启进程。
fn should_reset_update_progress(exit_signalled: bool, exit_forwarded: bool) -> bool {
    !(exit_signalled && exit_forwarded)
}

/// 把启动期自动检查推迟一个冷却周期（relaunch 场景调用）。
///
/// 将 LAST_CHECK_TIME 置为当前时刻，使启动与断网重连触发的自动检查都命中
/// 冷却门；一个冷却周期后由定时器轮询恢复正常检查节奏。
pub fn defer_initial_auto_check() {
    let mut last = LAST_CHECK_TIME.lock().unwrap();
    *last = Some(Instant::now());
}

#[derive(Debug)]
struct VerifiedInstaller {
    version: String,
    path: std::path::PathBuf,
    // 保持只读共享句柄直到 ShellExecuteExW 返回：拒绝其他进程改写或替换已
    // 校验文件，且不与映像加载器的 FILE_SHARE_READ|FILE_SHARE_DELETE 打开
    // 方式冲突（加载器不容纳并存句柄的写访问权，持写句柄启动必失败 32）。
    _file_lock: std::fs::File,
}

#[derive(Debug)]
enum CheckResult {
    NoUpdate,
    PortableFound(String),
    InstalledReady(VerifiedInstaller),
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateAction {
    Done,
    ExitMain,
}

struct SubprocessOutcome {
    is_error: bool,
    /// 已在 stdout 中读到 EXIT_MAIN（协议层事实，不代表通知已送达）。
    exit_signalled: bool,
    /// EXIT_MAIN 已成功投递给看门狗（UI 侧接到通知）。转发失败时主进程不会退出，
    /// worker 必须收尾复位进行中标志，否则后续检查被永久挡掉。
    exit_forwarded: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum InstallerLaunch {
    Started,
    Cancelled,
    Failed(u32),
}

fn do_update_check(is_manual: bool) -> CheckResult {
    let mut response = fetch_url(GITHUB_HOST, VERSION_PATH, VERSION_METADATA_MAX_BYTES);
    if response.is_err() {
        // 失败时增加 1 次重试，并等待 500ms 防止抖动
        std::thread::sleep(std::time::Duration::from_millis(500));
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

    // 缓存复用：先加锁再对锁定句柄哈希（见 try_reuse_cached_installer），
    // 同一句柄验证与持有，不按路径另开文件。缺失/占用/不匹配都落到重下。
    if let Some(verified) =
        try_reuse_cached_installer(temp_path.clone(), &latest_version, &expected_hash_hex)
    {
        return CheckResult::InstalledReady(verified);
    }

    // 主源下载失败才回落代理；校验/锁定等本地失败直接返回，不多下整包。
    // 流式写入全程不经过整包 Vec（见 fetch_verified_installer）。
    match fetch_verified_installer(
        &temp_path,
        GITHUB_HOST,
        &download_path,
        &expected_hash_hex,
        &latest_version,
    ) {
        Ok(verified) => CheckResult::InstalledReady(verified),
        Err(FetchFailure::Download(e)) => {
            let proxy_path = format!("/{GITHUB_REPOSITORY_URL}/{asset_path}");
            match fetch_verified_installer(
                &temp_path,
                PROXY_HOST,
                &proxy_path,
                &expected_hash_hex,
                &latest_version,
            ) {
                Ok(verified) => CheckResult::InstalledReady(verified),
                Err(FetchFailure::Download(pe)) => {
                    CheckResult::Error(format!("主源失败({e}), 代理源失败({pe})"))
                }
                // 主源下载已失败，代理本地失败：两段都报出，不只报后者。
                Err(FetchFailure::Local(pe)) => {
                    CheckResult::Error(format!("主源失败({e}), 代理源失败({pe})"))
                }
            }
        }
        Err(FetchFailure::Local(e)) => CheckResult::Error(e),
    }
}

/// 单源失败的错误归类：只有下载段失败才回落代理。
///
/// 背景：旧流程仅下载失败回落，哈希/创建/锁定失败直接返回；若把本地校验失败也
/// 回落，持续的本地磁盘故障会为空耗整包流量再失败一次。
enum FetchFailure {
    /// 抓取段失败（建连/发送/接收/状态码/查询/读取/超限），可回落代理。
    /// 写盘/哈希失败由 `http` 归为 `FetchFileError::Local`，经调用方转为本枚举的
    /// `Local`，不在此变体。
    Download(String),
    /// 本地失败（创建、流式写盘/哈希、流式哈希早验、锁定、锁柄重验），直接返回不再回落。
    Local(String),
}

/// 缓存复用：先加只读共享锁，再对锁定句柄哈希——同一句柄验证与持有。
/// 不变量：不按路径另开文件做验证，不探针 `exists()`（加锁与哈希以 Err 表达缺失）；
/// 哈希不匹配/读取失败返回 `None`，调用方删文件后重下。锁随 `VerifiedInstaller`
/// 持有至安装器启动返回（见 `launch_installer`）。
fn try_reuse_cached_installer(
    path: std::path::PathBuf,
    version: &str,
    expected_hash_hex: &str,
) -> Option<VerifiedInstaller> {
    let mut file_lock = open_locked_installer(&path).ok()?;
    // 对已锁定句柄哈希：验的就是将要持有的同一句柄，验后换文件无窗口。
    let existing_hash = compute_sha256_hex_locked(&mut file_lock).ok()?;
    if existing_hash.to_uppercase() != expected_hash_hex {
        return None;
    }
    Some(VerifiedInstaller {
        version: version.to_string(),
        path,
        _file_lock: file_lock,
    })
}

/// 单源流式下载并加锁重验：创建写锁文件 → 流式抓取（边读边哈希边写）
/// → 流式哈希早验 → 降级只读锁 → 对锁定句柄重算哈希 → 构造持锁体。
///
/// 不变量：最终构造的唯一依据是锁定句柄的重算哈希（`compute_sha256_hex_locked`），
/// 不采信流式哈希、不按路径另开文件；失败路径尽力删文件（删除结果忽略，
/// 外部占用下可能残留，由下次缓存哈希不匹配触发重下）。
/// 错误已带中文 `op`（抓取/哈希/写入/锁定），调用方按 `FetchFailure` 决定回落。
fn fetch_verified_installer(
    temp_path: &std::path::Path,
    host: &str,
    url_path: &str,
    expected_hash_hex: &str,
    version: &str,
) -> Result<VerifiedInstaller, FetchFailure> {
    if let Some(parent) = temp_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(temp_path);
    // 创建争用重试：上一次删除若被杀软实时扫描挡下，数百毫秒后通常自行解除；
    // 旧流程靠整包下载的数秒自然等待，新流程提前建文件，需显式等一次。
    // 复用启动重试的等待时长，不新增时间常量。
    let mut write_lock = match create_locked_installer(temp_path) {
        Ok(f) => f,
        Err(e) => {
            // 仅文件仍存在（上一次删除被瞬时占用挡下）才等一次重试；
            // 权限、路径等硬失败直接返回，不空等。
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(FetchFailure::Local("创建安装包文件失败".to_string()));
            }
            std::thread::sleep(std::time::Duration::from_millis(
                INSTALLER_LAUNCH_RETRY_DELAY_MS,
            ));
            let _ = std::fs::remove_file(temp_path);
            match create_locked_installer(temp_path) {
                Ok(f) => f,
                Err(_) => {
                    return Err(FetchFailure::Local("创建安装包文件失败".to_string()));
                }
            }
        }
    };
    let streaming_hash = match fetch_to_file(host, url_path, INSTALLER_MAX_BYTES, &mut write_lock) {
        Ok(h) => h,
        // 错误来源由产生处分类，不匹配文案：Download 回落代理，Local 直接返回。
        Err(FetchFileError::Download(e)) => {
            // 先释放写锁再删，否则 Windows 下删除被占用文件会失败而残留。
            drop(write_lock);
            let _ = std::fs::remove_file(temp_path);
            return Err(FetchFailure::Download(e));
        }
        Err(FetchFileError::Local(e)) => {
            drop(write_lock);
            let _ = std::fs::remove_file(temp_path);
            return Err(FetchFailure::Local(e));
        }
    };
    if streaming_hash.to_uppercase() != expected_hash_hex {
        drop(write_lock);
        let _ = std::fs::remove_file(temp_path);
        return Err(FetchFailure::Local(format!(
            "安装包校验失败 (预期: {}, 实际: {})",
            expected_hash_hex, streaming_hash
        )));
    }
    // 降级为只读共享锁：映像加载器以 FILE_SHARE_READ|FILE_SHARE_DELETE 打开，
    // 不容纳并存句柄的写访问权，持写句柄启动必失败 32。先关写再开只读，
    // 反向会因共享模式冲突开锁失败。
    drop(write_lock);
    let mut file_lock = open_locked_installer(temp_path).map_err(|_| {
        let _ = std::fs::remove_file(temp_path);
        FetchFailure::Local("锁定已下载的安装包失败".to_string())
    })?;
    // 对已锁定句柄重算哈希：关写到开读之间的无锁窗口若被篡改，在此现形。
    let verified_hash = match compute_sha256_hex_locked(&mut file_lock) {
        Ok(h) => h,
        Err(e) => {
            // 先释放锁再删，否则 Windows 下删除被占用文件会失败而残留。
            drop(file_lock);
            let _ = std::fs::remove_file(temp_path);
            return Err(FetchFailure::Local(format!("计算安装包哈希失败: {e}")));
        }
    };
    if verified_hash.to_uppercase() != expected_hash_hex {
        drop(file_lock);
        let _ = std::fs::remove_file(temp_path);
        return Err(FetchFailure::Local(format!(
            "安装包校验失败 (预期: {}, 实际: {})",
            expected_hash_hex, verified_hash
        )));
    }
    Ok(VerifiedInstaller {
        version: version.to_string(),
        path: temp_path.to_path_buf(),
        _file_lock: file_lock,
    })
}

/// 子进程入口：完成更新检查、用户交互和外部动作，仅将最终动作回传主进程。
///
/// stdout 单行协议：
/// - `DONE`：子进程已处理完毕，主进程继续运行。
/// - `EXIT_MAIN`：用户确认安装。必须在子进程启动安装器**之前**发出——主进程
///   收到后立即退出并释放 exe 映像句柄，子进程等单实例互斥量消失后才提权
///   运行安装器，从源头消除「文件正在使用」竞态；安装器内 taskkill 仅作兜底。
///
/// 退出码：0 = 检查流程成功完成，1 = 更新检查失败。手动检查失败时，错误提示
/// 已由子进程显示；退出码只供主进程决定自动检查的重试冷却时间。
pub fn subprocess_main(is_manual: bool) -> i32 {
    // EcoQoS/低内存优先级只加给本短生命周期子进程，不拖慢常驻监控主进程。
    configure_background_process();

    let result = do_update_check(is_manual);
    let is_error = matches!(result, CheckResult::Error(_));
    let action = complete_update_interaction(result, is_manual);

    // EXIT_MAIN 已在启动安装器之前输出完毕；其余路径统一以 DONE 收尾。
    if action == UpdateAction::Done {
        emit_protocol_line("DONE");
    }
    i32::from(is_error)
}

fn complete_update_interaction(result: CheckResult, is_manual: bool) -> UpdateAction {
    match result {
        CheckResult::NoUpdate => {
            if is_manual {
                show_info(&format!("当前已是最新版本 (v{VERSION})。"));
            }
            UpdateAction::Done
        }
        CheckResult::PortableFound(version) => {
            let msg = format!("发现新版本 v{version}。\n是否打开网页下载免安装版？");
            if show_yes_no(&msg) {
                open_url(RELEASE_PAGE_URL);
            } else {
                record_skipped_version(&version);
            }
            UpdateAction::Done
        }
        CheckResult::InstalledReady(verified) => {
            let msg = format!(
                "新版本 v{} 已准备就绪。\n是否立即关闭程序并安装？",
                verified.version
            );
            if !show_yes_no(&msg) {
                record_skipped_version(&verified.version);
                return UpdateAction::Done;
            }

            // 关键顺序：先发 EXIT_MAIN 让主进程退出让出 exe 映像，等单实例
            // 互斥量消失后再启动安装器；安装器的 taskkill 仅负责清理残存进程。
            emit_protocol_line("EXIT_MAIN");
            wait_main_instance_gone();

            match launch_installer(verified) {
                InstallerLaunch::Started => UpdateAction::ExitMain,
                InstallerLaunch::Cancelled => {
                    // 主进程已按约定退出（如 UAC 被取消），重新拉起应用，
                    // 避免任务栏小组件凭空消失。
                    relaunch_main_app();
                    UpdateAction::ExitMain
                }
                InstallerLaunch::Failed(code) => {
                    show_error(&format!("启动安装程序失败 (错误码: {code})"));
                    relaunch_main_app();
                    UpdateAction::ExitMain
                }
            }
        }
        CheckResult::Error(message) => {
            if is_manual {
                show_error(&format!("检查更新失败: {message}"));
            }
            UpdateAction::Done
        }
    }
}

fn emit_protocol_line(line: &str) {
    let _ = std::io::stdout().write_all(format!("{line}\n").as_bytes());
    let _ = std::io::stdout().flush();
}

/// 轮询单实例互斥量直至其消失（主进程完全退出），超时则放行交由安装器 taskkill 兜底。
///
/// 本子进程在 main() 单例锁创建前即被拦截，自身绝不持有该互斥量。
fn wait_main_instance_gone() -> bool {
    let name: Vec<u16> = crate::config::MUTEX_NAME.encode_utf16().collect();
    let deadline = Instant::now() + std::time::Duration::from_millis(MAIN_EXIT_WAIT_TIMEOUT_MS);

    loop {
        // SAFETY: name 以 NUL 结尾；句柄仅用于存在性探测，立即关闭。
        match unsafe { OpenMutexW(MUTEX_ALL_ACCESS, false, PCWSTR(name.as_ptr())) } {
            Err(_) => return true,
            Ok(handle) => unsafe {
                let _ = CloseHandle(handle);
            },
        }

        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(MAIN_EXIT_POLL_INTERVAL_MS));
    }
}

/// 重新拉起常驻主程序（仅用于 EXIT_MAIN 发出后安装未能继续的场景）。
/// 携带一次性参数让新进程推迟首个自动检查冷却周期：更新确认框刚被用户
/// 决策过，立刻再弹同一版本的确认框属于骚扰；下个冷却周期恢复正常。
fn relaunch_main_app() {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return,
    };
    let path_wide = os_to_wide(exe.as_os_str());
    let args_wide = to_wide(RELAUNCHED_BY_UPDATE_ARG);
    // SAFETY: 两个缓冲均含尾 NUL，ShellExecuteW 同步返回前存活。
    unsafe {
        let _ = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(path_wide.as_ptr()),
            PCWSTR(args_wide.as_ptr()),
            None,
            SW_SHOWNORMAL,
        );
    }
}

/// 主进程调用：re-exec 自身 `--check-update` 子进程，逐行解析其 stdout 协议。
///
/// winhttp/bcrypt、MessageBox/IME 和 ShellExecute 相关 DLL 只会进入子进程；
/// 主进程只解析 `DONE/EXIT_MAIN` 最终动作。读到 `EXIT_MAIN` 时立即转发给看门狗
/// （再由它落到主窗口）而不等子进程退出——此时安装器尚未启动，主进程必须先行
/// 退出释放 exe 映像，子进程才会继续执行提权安装。
///
/// 此处使用 `spawn()` + 手动按行读取，而非 `output()`，避免后者为并发读取
/// stderr 创建一个使用默认 2MB 栈预留的隐藏线程。
fn run_check_subprocess(is_manual: bool) -> SubprocessOutcome {
    let failed = || SubprocessOutcome {
        is_error: true,
        exit_signalled: false,
        exit_forwarded: false,
    };
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return failed(),
    };

    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let mut command = std::process::Command::new(exe);
    command.arg("--check-update");
    if is_manual {
        command.arg("--manual");
    }

    let mut child = match command
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return failed(),
    };

    let outcome_scan = match child.stdout.take() {
        Some(stdout) => {
            let mut reader = BufReader::new(stdout);
            scan_subprocess_protocol(&mut reader, post_update_action_to_watchdog)
        }
        None => ScanOutcome {
            action: None,
            exit_signalled: false,
            read_failed: true,
            exit_forwarded: false,
        },
    };
    let ScanOutcome {
        action: parsed_action,
        exit_signalled,
        read_failed,
        exit_forwarded,
    } = outcome_scan;

    let exit_status = match child.wait() {
        Ok(status) => status,
        Err(_) => {
            return SubprocessOutcome {
                is_error: true,
                exit_signalled,
                exit_forwarded,
            };
        }
    };

    SubprocessOutcome {
        is_error: read_failed || parsed_action.is_none() || !exit_status.success(),
        exit_signalled,
        exit_forwarded,
    }
}

/// 子进程 stdout 协议扫描结果：[`scan_subprocess_protocol`] 的具名返回。
///
/// 替代 `(Option<UpdateAction>, bool, bool, bool)` 四元组——三个 `bool` 在位置上
/// 无法区分，调用点只能靠顺序记忆；字段名即文档，零运行时成本。
struct ScanOutcome {
    /// 扫描到的首个有效动作（`DONE` / `EXIT_MAIN`）；空流或全无效行时为 `None`。
    action: Option<UpdateAction>,
    /// 是否读到 `EXIT_MAIN`（协议层事实，不代表通知已送达）。
    exit_signalled: bool,
    /// 读取是否失败（I/O 错误，含遇到无效 UTF-8 行）。
    read_failed: bool,
    /// `EXIT_MAIN` 是否成功转发给看门狗（UI 侧接到通知）。
    exit_forwarded: bool,
}

/// 逐行扫描子进程 stdout 协议，返回 [`ScanOutcome`]。
///
/// 不变量（由本模块 tests 以 Cursor 喂协议行钉死）：读到 `EXIT_MAIN` 即调用
/// `on_exit_main` 转发且仅转发一次（exit_signalled 守卫），转发发生在扫描期间、
/// 早于 `child.wait()`；调用方无补发路径。转发返回值独立于「是否读到」上报，
/// 使「消息没送出去」不再被当成「主进程即将退出」。
fn scan_subprocess_protocol(
    reader: &mut impl BufRead,
    mut on_exit_main: impl FnMut() -> bool,
) -> ScanOutcome {
    let mut parsed_action: Option<UpdateAction> = None;
    let mut exit_signalled = false;
    let mut read_failed = false;
    let mut exit_forwarded = false;

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Some(action) = parse_update_action(line.as_bytes()) {
                    if parsed_action.is_none() {
                        parsed_action = Some(action);
                    }
                    if action == UpdateAction::ExitMain && !exit_signalled {
                        exit_signalled = true;
                        // 收到即转发，不等子进程退出：主进程须抢在安装器拷贝前
                        // 退净并让出 exe 映像句柄。
                        exit_forwarded = on_exit_main();
                    }
                }
            }
            Err(_) => {
                read_failed = true;
                break;
            }
        }
    }

    ScanOutcome {
        action: parsed_action,
        exit_signalled,
        read_failed,
        exit_forwarded,
    }
}

fn parse_update_action(stdout: &[u8]) -> Option<UpdateAction> {
    match std::str::from_utf8(stdout).ok()?.trim() {
        "DONE" => Some(UpdateAction::Done),
        "EXIT_MAIN" => Some(UpdateAction::ExitMain),
        _ => None,
    }
}

/// 通知 UI 侧「主进程退出并清理托盘」。单动作协议，消息无载荷。
///
/// 投递目标是看门狗窗口——全生命周期不重建的顶层窗口——由它直接执行收尾语义，
/// 不再经主窗口转发（转发成功只代表消息入队，无法确认旧主窗口真的执行了）。
/// 返回是否成功投递：看门狗已消失时 `PostMessageW` 返回错误，调用方据此知道通知
/// 未送达，而不是把「消息丢了」当成「主进程即将退出」。
fn post_update_action_to_watchdog() -> bool {
    let Some(hwnd) = crate::window::watchdog_hwnd() else {
        return false;
    };
    // SAFETY: hwnd 已由 watchdog_hwnd 用 IsWindow 校验存活；PostMessageW 只向目标
    // 线程队列复制整数消息参数，不跨线程解引用 Rust 内存。
    unsafe { PostMessageW(Some(hwnd), WM_USER_UPDATE_ACTION, WPARAM(0), LPARAM(0)).is_ok() }
}

/// 执行更新交接的退出语义：复位进行中标志、清理托盘并结束消息循环。
///
/// 仅由看门狗过程处理 `WM_USER_UPDATE_ACTION` 时调用；看门狗与主窗口同属 UI 消息
/// 循环线程，因此线程前提（`PostQuitMessage` 面向当前线程）成立。
pub fn handle_update_action() {
    UPDATE_IN_PROGRESS.store(false, Ordering::Release);

    remove_tray_icon();
    // SAFETY:
    // 两个调用方都运行在 UI 消息循环所属线程上；PostQuitMessage 会向当前线程
    // 队列投递 WM_QUIT。
    unsafe {
        PostQuitMessage(0);
    }
}

fn show_yes_no(msg: &str) -> bool {
    // 复用 util 的统一 MessageBoxW 入口；返回 IDYES 表示用户选择「是」。
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

/// 启动安装器，对「文件正被外部进程占用」类瞬态错误（典型为杀软实时扫描
/// 刚写完的安装包）做有限次重试；只读锁保持到最后一次尝试结束后才释放，
/// 它不与映像加载器冲突，重试只针对外部占用者。
fn launch_installer(verified: VerifiedInstaller) -> InstallerLaunch {
    let mut attempt = 1;
    let result = loop {
        match try_launch_installer(&verified.path) {
            InstallerLaunch::Started => break InstallerLaunch::Started,
            other => {
                if !is_transient_launch_error(&other) || attempt >= INSTALLER_LAUNCH_MAX_ATTEMPTS {
                    break other;
                }
            }
        }
        attempt += 1;
        std::thread::sleep(std::time::Duration::from_millis(
            INSTALLER_LAUNCH_RETRY_DELAY_MS,
        ));
    };
    drop(verified);
    result
}

/// 判定是否值得重试的启动失败：仅共享冲突/锁冲突类瞬态错误。
fn is_transient_launch_error(launch: &InstallerLaunch) -> bool {
    matches!(
        launch,
        InstallerLaunch::Failed(code)
            if *code == ERROR_SHARING_VIOLATION.0 || *code == ERROR_LOCK_VIOLATION.0
    )
}

fn try_launch_installer(path: &std::path::Path) -> InstallerLaunch {
    let path_wide = os_to_wide(path.as_os_str());
    let verb_wide = to_wide("runas");
    let params_wide = to_wide("/VERYSILENT /SUPPRESSMSGBOXES /NORESTART");

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        // 失败时抑制 Shell 自带错误框（如「另一个程序正在使用此文件」）：
        // 重试期间会连弹多个标准错误框，且与子进程的 show_error 形成双重
        // 弹窗；统一由子进程报告错误码。
        fMask: SEE_MASK_FLAG_NO_UI,
        lpVerb: PCWSTR(verb_wide.as_ptr()),
        lpFile: PCWSTR(path_wide.as_ptr()),
        lpParameters: PCWSTR(params_wide.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // SAFETY:
    // path_wide、verb_wide 和 params_wide 都是 NUL 终止的 UTF-16 缓冲区，并在
    // ShellExecuteExW 同步读取 SHELLEXECUTEINFOW 期间保持存活。cbSize 与结构体
    // 实际大小一致；fMask 仅含 SEE_MASK_FLAG_NO_UI，不含需要调用方提供额外
    // 指针或接管进程句柄的掩码。
    let launched = unsafe { ShellExecuteExW(&mut sei) };

    if launched.is_ok() {
        return InstallerLaunch::Started;
    }

    // SAFETY:
    // 紧接失败的 ShellExecuteExW 调用读取当前线程 last-error，中间未调用其他
    // 可能覆盖错误码的 Win32 API。
    let error = unsafe { GetLastError() };
    if error == ERROR_CANCELLED {
        InstallerLaunch::Cancelled
    } else {
        InstallerLaunch::Failed(error.0)
    }
}

/// 启动期清理长期残留的临时安装包：仅删除超过缓存有效期的文件。
///
/// 有效期内的文件是已通过哈希校验的可复用缓存（哈希不匹配的残留由
/// `do_update_check` 自行删除重下）；无条件删除会摧毁缓存，迫使每次
/// 检查都重新下载。时钟回拨导致 mtime 不可解析时保守保留。
pub fn init_cleanup_temp() {
    let path = get_temp_installer_path();
    let expired = std::fs::metadata(&path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|mtime| mtime.elapsed().ok())
        .is_some_and(|age| age.as_secs() > INSTALLER_CACHE_MAX_AGE_SECS);
    if expired {
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== parse_update_action =====

    #[test]
    fn test_parse_update_actions() {
        assert_eq!(parse_update_action(b"DONE"), Some(UpdateAction::Done));
        assert_eq!(parse_update_action(b"  DONE\r\n"), Some(UpdateAction::Done));
        assert_eq!(
            parse_update_action(b"EXIT_MAIN"),
            Some(UpdateAction::ExitMain)
        );
        assert_eq!(
            parse_update_action(b"  EXIT_MAIN\r\n"),
            Some(UpdateAction::ExitMain)
        );
    }

    #[test]
    fn test_reject_invalid_update_actions() {
        for input in [
            b"".as_slice(),
            b"NO_UPDATE".as_slice(),
            b"EXIT_MAIN|extra".as_slice(),
            b"some random garbage".as_slice(),
            &[0xFF, 0xFE],
        ] {
            assert_eq!(parse_update_action(input), None);
        }
    }

    // ===== scan_subprocess_protocol =====

    /// 用内存缓冲驱动协议扫描，并记录转发回调次数与送达结果。
    fn scan(data: &[u8]) -> (Option<UpdateAction>, bool, bool, usize, bool) {
        let mut reader = std::io::Cursor::new(data);
        let mut forwards = 0usize;
        let outcome = scan_subprocess_protocol(&mut reader, || {
            forwards += 1;
            true
        });
        (
            outcome.action,
            outcome.exit_signalled,
            outcome.read_failed,
            forwards,
            outcome.exit_forwarded,
        )
    }

    /// 用内存队列驱动协议扫描，模拟「转发目标已失效」：回调返回 false，等价于
    /// 看门狗窗口已销毁时 `PostMessageW` 失败。
    fn scan_with_dead_target(data: &[u8]) -> (bool, bool) {
        let mut reader = std::io::Cursor::new(data);
        let outcome = scan_subprocess_protocol(&mut reader, || false);
        (outcome.exit_signalled, outcome.exit_forwarded)
    }

    #[test]
    fn test_scan_exit_main_forwards_exactly_once() {
        // forwards（回调次数）与 forwarded（送达结果）是两个独立观测通道：
        // 前者证明"只转发一次"，后者证明"转发成功被正确上报"，互不可推导。
        let (parsed, exit_signalled, read_failed, forwards, forwarded) = scan(b"EXIT_MAIN\n");
        assert_eq!(parsed, Some(UpdateAction::ExitMain));
        assert!(exit_signalled);
        assert!(!read_failed);
        assert_eq!(forwards, 1);
        assert!(forwarded);
    }

    #[test]
    fn test_scan_duplicate_exit_main_forward_only_once() {
        // 钉死不变量：无论子进程输出多少行 EXIT_MAIN，转发恰好一次。
        let (_, exit_signalled, _, forwards, _) = scan(b"EXIT_MAIN\nEXIT_MAIN\nEXIT_MAIN\n");
        assert!(exit_signalled);
        assert_eq!(forwards, 1);
    }

    #[test]
    fn test_scan_done_does_not_forward() {
        let (parsed, exit_signalled, read_failed, forwards, _) = scan(b"DONE\n");
        assert_eq!(parsed, Some(UpdateAction::Done));
        assert!(!exit_signalled);
        assert!(!read_failed);
        assert_eq!(forwards, 0);
    }

    #[test]
    fn test_scan_invalid_lines_do_not_block_later_exit_main() {
        // 无效行必须只被跳过，不得阻断其后的有效动作：EXIT_MAIN 故意放在无效行之后。
        // 旧输入（无效行之后无有效动作）对任何实现都成立，本用例才能真正区分
        // 「跳过无效行」与「遇无效行即停止读取」两种实现。
        let (parsed, exit_signalled, read_failed, forwards, _) =
            scan(b"NO_UPDATE\nEXIT_MAIN|extra\nEXIT_MAIN\n");
        assert_eq!(parsed, Some(UpdateAction::ExitMain));
        assert!(exit_signalled);
        assert!(!read_failed);
        assert_eq!(forwards, 1);
    }

    #[test]
    fn test_scan_empty_stream() {
        let (parsed, exit_signalled, read_failed, forwards, _) = scan(b"");
        assert_eq!(parsed, None);
        assert!(!exit_signalled);
        assert!(!read_failed);
        assert_eq!(forwards, 0);
    }

    #[test]
    fn test_scan_invalid_utf8_marks_read_failed() {
        let (parsed, exit_signalled, read_failed, forwards, _) = scan(&[0xFF, 0xFE, b'\n']);
        assert_eq!(parsed, None);
        assert!(!exit_signalled);
        assert!(read_failed);
        assert_eq!(forwards, 0);
    }

    #[test]
    fn test_scan_memo_keeps_first_action_but_still_forwards() {
        // memo 记录首个有效动作（is_error 判定只消费 is_none）；转发与 memo 无关。
        let (parsed, exit_signalled, _, forwards, _) = scan(b"DONE\nEXIT_MAIN\n");
        assert_eq!(parsed, Some(UpdateAction::Done));
        assert!(exit_signalled);
        assert_eq!(forwards, 1);
    }

    // ===== 转发送达与进行中标志复位 =====

    #[test]
    fn test_scan_dead_target_still_signals_but_not_forwarded() {
        // 目标窗口已失效（模拟 Explorer 重建后旧句柄）：协议层仍读到 EXIT_MAIN，
        // 但必须上报未送达——否则 worker 会误以为主进程要退出而跳过复位。
        let (exit_signalled, forwarded) = scan_with_dead_target(b"EXIT_MAIN\n");
        assert!(exit_signalled);
        assert!(!forwarded);
    }

    #[test]
    fn test_should_reset_update_progress_matrix() {
        // 唯一免复位的情形：EXIT_MAIN 已读到且通知已送达（主进程即将退出）。
        assert!(!should_reset_update_progress(true, true));
        // 读到但未送达：主进程仍在运行，必须复位——否则检查被永久挡掉。
        assert!(should_reset_update_progress(true, false));
        // 常规结束（DONE）与只读到无效行同样必须复位。
        assert!(should_reset_update_progress(false, false));
    }

    #[test]
    fn test_reset_update_progress_clears_global_flag() {
        // 唯一触碰全局标志的用例：cargo test 默认并行执行，多个用例读写同一全局
        // 原子量会相互覆盖产生假红，故全局断言只保留这一处（其余覆盖由上面的
        // 纯函数矩阵承担）。
        UPDATE_IN_PROGRESS.store(true, Ordering::Release);
        let outcome = SubprocessOutcome {
            is_error: false,
            exit_signalled: true,
            exit_forwarded: false,
        };
        assert!(reset_update_progress_after_check(&outcome), "仍需收尾");
        assert!(!UPDATE_IN_PROGRESS.load(Ordering::Acquire));
    }

    // ===== is_transient_launch_error =====

    #[test]
    fn test_transient_launch_errors_are_retried() {
        // 32 = ERROR_SHARING_VIOLATION，33 = ERROR_LOCK_VIOLATION。
        assert!(is_transient_launch_error(&InstallerLaunch::Failed(
            ERROR_SHARING_VIOLATION.0
        )));
        assert!(is_transient_launch_error(&InstallerLaunch::Failed(
            ERROR_LOCK_VIOLATION.0
        )));
    }

    #[test]
    fn test_permanent_launch_errors_are_not_retried() {
        assert!(!is_transient_launch_error(&InstallerLaunch::Started));
        assert!(!is_transient_launch_error(&InstallerLaunch::Cancelled));
        assert!(!is_transient_launch_error(&InstallerLaunch::Failed(5)));
        assert!(!is_transient_launch_error(&InstallerLaunch::Failed(2)));
    }

    // ===== 锁定句柄重验（TOCTOU 防护） =====

    /// 唯一使用真实临时文件的用例：文件名含进程 ID，避免与并行用例冲突；
    /// 首尾都删文件，不留残留。生产路径禁止 unwrap，此处为测试断言。
    fn tamper_test_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "traffic-monitor-reuse-test-{}-{}.tmp",
            std::process::id(),
            tag
        ))
    }

    #[test]
    fn test_cached_reuse_accepts_matching_locked_content() {
        let path = tamper_test_path("accept");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"good-installer-payload").unwrap();
        let expected = {
            let mut f = open_locked_installer(&path).unwrap();
            compute_sha256_hex_locked(&mut f).unwrap()
        };
        let reused = try_reuse_cached_installer(path.clone(), "9.9.9", &expected);
        assert!(reused.is_some(), "锁定句柄哈希一致时必须复用");
        drop(reused);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_cached_reuse_rejects_tampered_locked_content() {
        // 模拟“校验后替换文件内容”：先按好内容算出期望哈希，再用坏内容覆盖文件，
        // 锁后重验必须拒绝构造（返回 None），否则 TOCTOU 缺口仍在。
        let path = tamper_test_path("reject");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"good-installer-payload").unwrap();
        let expected_good = {
            let mut f = open_locked_installer(&path).unwrap();
            compute_sha256_hex_locked(&mut f).unwrap()
        };
        // 篡改：锁已释放后覆盖为坏内容（等价于无锁窗口内的替换）。
        std::fs::write(&path, b"tampered-installer-payload").unwrap();
        let reused = try_reuse_cached_installer(path.clone(), "9.9.9", &expected_good);
        assert!(reused.is_none(), "锁定句柄哈希不一致时必须拒绝复用");
        let _ = std::fs::remove_file(&path);
    }
}
