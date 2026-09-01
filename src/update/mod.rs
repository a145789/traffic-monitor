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
    HWND, LPARAM, WPARAM,
};
use windows::Win32::System::Threading::{MUTEX_ALL_ACCESS, OpenMutexW};
use windows::Win32::UI::Shell::{SHELLEXECUTEINFOW, ShellExecuteExW, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_ICONINFORMATION, MB_YESNO, PostMessageW, PostQuitMessage, SW_SHOWNORMAL,
};
use windows::core::{PCWSTR, w};

use crate::config::{
    AUTO_CHECK_COOLDOWN_SECS, AUTO_CHECK_ERROR_COOLDOWN_SECS, INSTALLER_LAUNCH_MAX_ATTEMPTS,
    INSTALLER_LAUNCH_RETRY_DELAY_MS, INSTALLER_MAX_BYTES, MAIN_EXIT_POLL_INTERVAL_MS,
    MAIN_EXIT_WAIT_TIMEOUT_MS, REG_PATH_APP, VERSION, VERSION_METADATA_MAX_BYTES,
    WM_USER_UPDATE_ACTION,
};
use crate::state::{ENABLE_AUTO_UPDATE, UPDATE_IN_PROGRESS};
use crate::tray::remove_tray_icon;
use crate::util::{
    compact_and_trim, configure_background_process, message_box, reg_read_dword, reg_read_string,
    reg_write_dword, reg_write_string, show_error, show_info, to_wide,
};

use crypto::{compute_sha256_hex, compute_sha256_hex_file};
use http::fetch_url;
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
    let local_appdata = std::env::var("LOCALAPPDATA")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    std::path::PathBuf::from(local_appdata)
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

pub fn start_auto_check(hwnd: HWND) {
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

    spawn_update_worker(hwnd, false);
}

pub fn start_manual_check(hwnd: HWND) {
    // 更新相关提示必须全部由短生命周期子进程显示，避免 MessageBox/IME DLL
    // 因重复点击进入常驻主进程；已有检查运行时直接忽略本次点击。
    if UPDATE_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return;
    }

    spawn_update_worker(hwnd, true);
}

/// spawn 更新工作线程；spawn 失败时复位进行中标志。
///
/// 仅负责线程创建与失败复位；自动检查的两道前置门（开关、冷却）保留在
/// `start_auto_check` 内，占坑与门序不因本函数改变。
fn spawn_update_worker(hwnd: HWND, is_manual: bool) {
    let hwnd_raw: isize = hwnd.0 as isize;

    if std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            update_check_worker(hwnd_raw, is_manual);
        })
        .is_err()
    {
        UPDATE_IN_PROGRESS.store(false, Ordering::Release);
    }
}

fn update_check_worker(hwnd_raw: isize, is_manual: bool) {
    let outcome = run_check_subprocess(is_manual, hwnd_raw);

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

    // EXIT_MAIN 在子进程读取阶段就已即时转发（见 run_check_subprocess），
    // 主进程即将退出，不再重置进行中标志。
    if outcome.exit_signalled {
        return;
    }

    UPDATE_IN_PROGRESS.store(false, Ordering::Release);
    compact_and_trim();
}

#[derive(Debug)]
struct VerifiedInstaller {
    version: String,
    path: std::path::PathBuf,
    // 保持只读共享句柄直到 ShellExecuteExW 返回，阻止其他进程改写或替换已校验文件。
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
    /// 已在 stdout 中读到 EXIT_MAIN 并即时转发给主窗口，worker 无需再做任何事。
    exit_signalled: bool,
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

    // 若临时文件已存在且哈希匹配，以只读共享锁打开后直接复用。
    if temp_path.exists() {
        if let Ok(existing_hash) = compute_sha256_hex_file(&temp_path) {
            if existing_hash.to_uppercase() == expected_hash_hex {
                match open_locked_installer(&temp_path) {
                    Ok(file_lock) => {
                        return CheckResult::InstalledReady(VerifiedInstaller {
                            version: latest_version.to_string(),
                            path: temp_path,
                            _file_lock: file_lock,
                        });
                    }
                    Err(_) => {
                        // 无法锁定已有文件，删除后重新下载。
                        let _ = std::fs::remove_file(&temp_path);
                    }
                }
            } else {
                let _ = std::fs::remove_file(&temp_path);
            }
        } else {
            let _ = std::fs::remove_file(&temp_path);
        }
    }

    // 主源失败时回落到代理源；两者都失败时报组合错误。
    let installer_data = match fetch_url(GITHUB_HOST, &download_path, INSTALLER_MAX_BYTES) {
        Ok(data) => data,
        Err(e) => {
            let proxy_path = format!("/{GITHUB_REPOSITORY_URL}/{asset_path}");
            match fetch_url(PROXY_HOST, &proxy_path, INSTALLER_MAX_BYTES) {
                Ok(data) => data,
                Err(pe) => {
                    return CheckResult::Error(format!("主源失败({e}), 代理源失败({pe})"));
                }
            }
        }
    };

    let actual_hash_hex = match compute_sha256_hex(&installer_data) {
        Ok(h) => h,
        Err(e) => {
            return CheckResult::Error(format!("计算安装包哈希失败: {e}"));
        }
    };

    if actual_hash_hex.to_uppercase() != expected_hash_hex {
        return CheckResult::Error(format!(
            "安装包校验失败 (预期: {}, 实际: {})",
            expected_hash_hex, actual_hash_hex
        ));
    }

    // 确保父目录存在后，以 create_new(true) + FILE_SHARE_READ 创建独占写锁文件。
    if let Some(parent) = temp_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 先尝试移除可能残留的无效文件（上次哈希不匹配或被中断的下载）。
    let _ = std::fs::remove_file(&temp_path);
    let mut file_lock = match create_locked_installer(&temp_path) {
        Ok(f) => f,
        Err(_) => {
            return CheckResult::Error("创建安装包文件失败".to_string());
        }
    };
    if file_lock.write_all(&installer_data).is_err() {
        return CheckResult::Error("写入安装包文件失败".to_string());
    }

    CheckResult::InstalledReady(VerifiedInstaller {
        version: latest_version.to_string(),
        path: temp_path,
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
fn relaunch_main_app() {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return,
    };
    let path_wide = to_wide(&exe.to_string_lossy());
    // SAFETY: path_wide 含尾 NUL，ShellExecuteW 同步返回前存活。
    unsafe {
        let _ = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(path_wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

/// 主进程调用：re-exec 自身 `--check-update` 子进程，逐行解析其 stdout 协议。
///
/// winhttp/bcrypt、MessageBox/IME 和 ShellExecute 相关 DLL 只会进入子进程；
/// 主进程只解析 `DONE/EXIT_MAIN` 最终动作。读到 `EXIT_MAIN` 时立即转发给主
/// 窗口而不等子进程退出——此时安装器尚未启动，主进程必须先行退出释放 exe
/// 映像，子进程才会继续执行提权安装。
///
/// 此处使用 `spawn()` + 手动按行读取，而非 `output()`，避免后者为并发读取
/// stderr 创建一个使用默认 2MB 栈预留的隐藏线程。
fn run_check_subprocess(is_manual: bool, hwnd_raw: isize) -> SubprocessOutcome {
    let failed = || SubprocessOutcome {
        is_error: true,
        exit_signalled: false,
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

    let (parsed_action, exit_signalled, read_failed) = match child.stdout.take() {
        Some(stdout) => {
            let mut reader = BufReader::new(stdout);
            scan_subprocess_protocol(&mut reader, || {
                let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
                post_update_action(hwnd);
            })
        }
        None => (None, false, true),
    };

    let exit_status = match child.wait() {
        Ok(status) => status,
        Err(_) => {
            return SubprocessOutcome {
                is_error: true,
                exit_signalled,
            };
        }
    };

    SubprocessOutcome {
        is_error: read_failed || parsed_action.is_none() || !exit_status.success(),
        exit_signalled,
    }
}

/// 逐行扫描子进程 stdout 协议，返回 (首个有效动作, 是否已转发 EXIT_MAIN, 读取是否失败)。
///
/// 不变量（由本模块 tests 以 Cursor 喂协议行钉死）：读到 `EXIT_MAIN` 即调用
/// `on_exit_main` 转发且仅转发一次（exit_signalled 守卫），转发发生在扫描期间、
/// 早于 `child.wait()`；调用方无补发路径，转发失败由子进程超时照常启动安装器
/// + 安装器内 taskkill 兜底。
fn scan_subprocess_protocol(
    reader: &mut impl BufRead,
    mut on_exit_main: impl FnMut(),
) -> (Option<UpdateAction>, bool, bool) {
    let mut parsed_action: Option<UpdateAction> = None;
    let mut exit_signalled = false;
    let mut read_failed = false;

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
                        on_exit_main();
                    }
                }
            }
            Err(_) => {
                read_failed = true;
                break;
            }
        }
    }

    (parsed_action, exit_signalled, read_failed)
}

fn parse_update_action(stdout: &[u8]) -> Option<UpdateAction> {
    match std::str::from_utf8(stdout).ok()?.trim() {
        "DONE" => Some(UpdateAction::Done),
        "EXIT_MAIN" => Some(UpdateAction::ExitMain),
        _ => None,
    }
}

/// 通知主窗口「主进程退出并清理托盘」。单动作协议，消息无载荷。
fn post_update_action(hwnd: HWND) {
    // SAFETY:
    // hwnd 来自主线程创建的窗口句柄，并且仅在主进程仍持有该窗口期间由工作线程使用。
    // PostMessageW 只向目标线程队列复制整数消息参数，不会跨线程解引用 Rust 内存；
    // 若窗口已销毁，API 会返回错误，调用本身不会访问无效内存。
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_USER_UPDATE_ACTION, WPARAM(0), LPARAM(0));
    }
}

pub fn handle_update_action() {
    UPDATE_IN_PROGRESS.store(false, Ordering::Release);

    remove_tray_icon();
    // SAFETY:
    // 此函数仅由主窗口过程处理 WM_USER_UPDATE_ACTION 时调用，因此当前线程就是
    // UI 消息循环所属线程；PostQuitMessage 会向当前线程队列投递 WM_QUIT。
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

/// 启动安装器，对「文件正被占用」类瞬态错误（典型为杀软实时扫描刚写完的
/// 安装包）做有限次重试；文件锁保持到最后一次尝试结束后才释放。
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
    let path_str = path.to_string_lossy();
    let path_wide = to_wide(&path_str);
    let verb_wide = to_wide("runas");
    let params_wide = to_wide("/VERYSILENT /SUPPRESSMSGBOXES /NORESTART");

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        lpVerb: PCWSTR(verb_wide.as_ptr()),
        lpFile: PCWSTR(path_wide.as_ptr()),
        lpParameters: PCWSTR(params_wide.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // SAFETY:
    // path_wide、verb_wide 和 params_wide 都是 NUL 终止的 UTF-16 缓冲区，并在
    // ShellExecuteExW 同步读取 SHELLEXECUTEINFOW 期间保持存活。cbSize 与结构体
    // 实际大小一致，未设置需要调用方提供额外指针或接管进程句柄的掩码。
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

pub fn init_cleanup_temp() {
    let path = get_temp_installer_path();
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== parse_update_action =====

    #[test]
    fn test_parse_done_action() {
        assert_eq!(parse_update_action(b"DONE"), Some(UpdateAction::Done));
        assert_eq!(parse_update_action(b"  DONE\r\n"), Some(UpdateAction::Done));
    }

    #[test]
    fn test_parse_exit_main_action() {
        assert_eq!(
            parse_update_action(b"EXIT_MAIN"),
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

    /// 用内存缓冲驱动协议扫描，并记录转发回调次数。
    fn scan(data: &[u8]) -> (Option<UpdateAction>, bool, bool, usize) {
        let mut reader = std::io::Cursor::new(data);
        let mut forwards = 0usize;
        let (parsed, exit_signalled, read_failed) =
            scan_subprocess_protocol(&mut reader, || forwards += 1);
        (parsed, exit_signalled, read_failed, forwards)
    }

    #[test]
    fn test_scan_exit_main_forwards_exactly_once() {
        let (parsed, exit_signalled, read_failed, forwards) = scan(b"EXIT_MAIN\n");
        assert_eq!(parsed, Some(UpdateAction::ExitMain));
        assert!(exit_signalled);
        assert!(!read_failed);
        assert_eq!(forwards, 1);
    }

    #[test]
    fn test_scan_duplicate_exit_main_forward_only_once() {
        // 钉死不变量：无论子进程输出多少行 EXIT_MAIN，转发恰好一次。
        let (_, exit_signalled, _, forwards) = scan(b"EXIT_MAIN\nEXIT_MAIN\nEXIT_MAIN\n");
        assert!(exit_signalled);
        assert_eq!(forwards, 1);
    }

    #[test]
    fn test_scan_done_does_not_forward() {
        let (parsed, exit_signalled, read_failed, forwards) = scan(b"DONE\n");
        assert_eq!(parsed, Some(UpdateAction::Done));
        assert!(!exit_signalled);
        assert!(!read_failed);
        assert_eq!(forwards, 0);
    }

    #[test]
    fn test_scan_garbage_lines_forward_nothing_and_remember_nothing() {
        let (parsed, exit_signalled, read_failed, forwards) =
            scan(b"NO_UPDATE\nEXIT_MAIN|extra\n\n");
        assert_eq!(parsed, None);
        assert!(!exit_signalled);
        assert!(!read_failed);
        assert_eq!(forwards, 0);
    }

    #[test]
    fn test_scan_empty_stream() {
        let (parsed, exit_signalled, read_failed, forwards) = scan(b"");
        assert_eq!(parsed, None);
        assert!(!exit_signalled);
        assert!(!read_failed);
        assert_eq!(forwards, 0);
    }

    #[test]
    fn test_scan_invalid_utf8_marks_read_failed() {
        let (parsed, exit_signalled, read_failed, forwards) = scan(&[0xFF, 0xFE, b'\n']);
        assert_eq!(parsed, None);
        assert!(!exit_signalled);
        assert!(read_failed);
        assert_eq!(forwards, 0);
    }

    #[test]
    fn test_scan_memo_keeps_first_action_but_still_forwards() {
        // memo 记录首个有效动作（is_error 判定只消费 is_none）；转发与 memo 无关。
        let (parsed, exit_signalled, _, forwards) = scan(b"DONE\nEXIT_MAIN\n");
        assert_eq!(parsed, Some(UpdateAction::Done));
        assert!(exit_signalled);
        assert_eq!(forwards, 1);
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
}
