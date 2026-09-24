//! 自动/手动检查更新、下载新版本安装包、SHA-256 校验、UAC 提权覆盖安装。
//!
//! 模块拆分：
//! - [`version`]：版本号解析与远端 metadata 严格解析（纯字符串处理）。
//! - [`http`]：WinHTTP 抓取与友好的中文错误映射。
//! - [`crypto`]：BCrypt SHA-256 哈希与 RAII 句柄守卫。
//! - [`cache`]：临时安装包路径、加锁打开/创建、启动期过期清理。
//! - [`protocol`]：子进程 stdout 单行协议扫描、EXIT_MAIN 转发、收尾复位判定。
//! - [`installer`]：缓存复用、流式下载校验、安装器提权启动、主进程退出等待。
//! - 本文件：自动/手动编排、更新检查流程、用户交互、注册表开关读写。

mod cache;
mod crypto;
mod http;
mod installer;
mod protocol;
mod version;

pub use cache::init_cleanup_temp;

use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONINFORMATION, MB_YESNO, SW_SHOWNORMAL};
use windows::core::{PCWSTR, w};

use crate::config::{
    AUTO_CHECK_COOLDOWN_SECS, AUTO_CHECK_ERROR_COOLDOWN_SECS, REG_PATH_APP,
    UPDATE_FETCH_RETRY_DELAY_MS, UPDATE_WORKER_STACK_BYTES, VERSION, VERSION_METADATA_MAX_BYTES,
};
use crate::state::{ENABLE_AUTO_UPDATE, UPDATE_IN_PROGRESS};
use crate::util::{
    compact_and_trim, configure_background_process, log_event, message_box, refresh_debug_log_flag,
    reg_read_dword, reg_read_string, reg_write_dword, reg_write_string, show_error, show_info,
    to_wide,
};

use cache::get_temp_installer_path;
use http::fetch_url;
use installer::{
    FetchFailure, InstallerLaunch, VerifiedInstaller, fetch_verified_installer, launch_installer,
    relaunch_main_app, try_reuse_cached_installer, wait_main_instance_gone,
};
use protocol::{
    UpdateAction, emit_protocol_line, reset_update_progress_after_check, run_check_subprocess,
};
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
        .stack_size(UPDATE_WORKER_STACK_BYTES)
        .spawn(move || {
            update_check_worker(is_manual);
        })
        .is_err()
    {
        UPDATE_IN_PROGRESS.store(false, Ordering::Release);
    }
}

// 编译期契约：错误冷却不得大于正常冷却，否则下方差值 u64 下溢、release 回绕出
// 巨大 Duration，随后 Instant - Duration panic → abort。
const _: () = assert!(AUTO_CHECK_ERROR_COOLDOWN_SECS <= AUTO_CHECK_COOLDOWN_SECS);

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

    // 只有「读到 EXIT_MAIN」且消息已成功入队，主进程才会继续执行退出交接。
    if !reset_update_progress_after_check(&outcome) {
        return;
    }
    compact_and_trim();
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
enum CheckResult {
    NoUpdate,
    PortableFound(String),
    InstalledReady(VerifiedInstaller),
    Error(String),
}

fn do_update_check(is_manual: bool) -> CheckResult {
    let mut response = fetch_url(GITHUB_HOST, VERSION_PATH, VERSION_METADATA_MAX_BYTES);
    if response.is_err() {
        // 失败时增加 1 次重试，并等待片刻防止抖动
        std::thread::sleep(std::time::Duration::from_millis(
            UPDATE_FETCH_RETRY_DELAY_MS,
        ));
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

/// 子进程入口：完成更新检查、用户交互和外部动作，仅将最终动作回传主进程。
///
/// stdout 单行协议：
/// - `DONE`：子进程已处理完毕，主进程继续运行。
/// - `EXIT_MAIN`：用户确认安装。必须在子进程启动安装器**之前**发出——主进程
///   看门狗收到并处理后，主进程退出并释放 exe 映像句柄；子进程等单实例互斥量消失后才提权
///   运行安装器，从源头消除「文件正在使用」竞态；安装器内 taskkill 仅作兜底。
///
/// 退出码：0 = 检查流程成功完成，1 = 更新检查失败。手动检查失败时，错误提示
/// 已由子进程显示；退出码只供主进程决定自动检查的重试冷却时间。
pub fn subprocess_main(is_manual: bool) -> i32 {
    // EcoQoS/低内存优先级只加给本短生命周期子进程，不拖慢常驻监控主进程。
    configure_background_process();
    refresh_debug_log_flag();

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
            log_event!("已发出 EXIT_MAIN，等待主进程退出");
            wait_main_instance_gone();

            match launch_installer(verified) {
                InstallerLaunch::Started => {
                    log_event!("安装器已启动");
                    UpdateAction::ExitMain
                }
                InstallerLaunch::Cancelled => {
                    // 主进程已按约定退出（如 UAC 被取消），重新拉起应用，
                    // 避免任务栏小组件凭空消失。
                    log_event!("安装器启动被取消，重新拉起主程序");
                    relaunch_main_app();
                    UpdateAction::ExitMain
                }
                InstallerLaunch::Failed(code) => {
                    log_event!("安装器启动失败 (错误码: {code})，重新拉起主程序");
                    show_error(&format!("启动安装程序失败 (错误码: {code})"));
                    relaunch_main_app();
                    UpdateAction::ExitMain
                }
            }
        }
        CheckResult::Error(message) => {
            log_event!("更新检查失败: {message}");
            if is_manual {
                show_error(&format!("检查更新失败: {message}"));
            }
            UpdateAction::Done
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
