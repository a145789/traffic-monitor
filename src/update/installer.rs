//! 安装器管线：缓存复用、流式下载、提权启动、主进程退出等待。
//! 不变量：构造 `VerifiedInstaller` 的唯一依据是锁定句柄的重算哈希
//! （`compute_sha256_hex_locked`），不按路径另开文件。

use std::time::Instant;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, ERROR_LOCK_VIOLATION,
    ERROR_SHARING_VIOLATION, GetLastError,
};
use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
use windows::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SHELLEXECUTEINFOW, ShellExecuteExW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use crate::config::{
    INSTALLER_LAUNCH_MAX_ATTEMPTS, INSTALLER_LAUNCH_RETRY_DELAY_MS, INSTALLER_MAX_BYTES,
    MAIN_EXIT_POLL_INTERVAL_MS, MAIN_EXIT_WAIT_TIMEOUT_MS, RELAUNCHED_BY_UPDATE_ARG,
};
use crate::util::{log_event, os_to_wide, to_wide};

use super::cache::{create_locked_installer, open_locked_installer};
use super::crypto::compute_sha256_hex_locked;
use super::http::{FetchFileError, fetch_to_file};

#[derive(Debug)]
pub(super) struct VerifiedInstaller {
    pub(super) version: String,
    path: std::path::PathBuf,
    // 保持只读共享句柄直到 ShellExecuteExW 返回：拒绝其他进程改写或替换已
    // 校验文件，且不与映像加载器的 FILE_SHARE_READ|FILE_SHARE_DELETE 打开
    // 方式冲突（加载器不容纳并存句柄的写访问权，持写句柄启动必失败 32）。
    _file_lock: std::fs::File,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum InstallerLaunch {
    Started,
    Cancelled,
    Failed(u32),
}

/// 单源失败的错误归类：只有下载段失败才回落代理。
///
/// 背景：旧流程仅下载失败回落，哈希/创建/锁定失败直接返回；若把本地校验失败也
/// 回落，持续的本地磁盘故障会为空耗整包流量再失败一次。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FetchFailure {
    /// 抓取段失败（建连/发送/接收/状态码/查询/读取/超限），可回落代理。
    /// 写盘/哈希失败由 `http` 归为 `FetchFileError::Local`，经调用方转为本枚举的
    /// `Local`，不在此变体。
    Download(String),
    /// 本地失败（创建、流式写盘、锁定、锁柄重验），直接返回不再回落。
    Local(String),
    /// 取消（父进程已消失、本次动作已无人要）：既不是网络失败也不是本地失败，
    /// 不回落代理、不重试，调用方静默放弃。
    Cancelled,
}

/// 抓取段失败 → 单源归类的唯一实现。
///
/// 抽成纯函数是为了让「取消不得被归成 Download」这条判定可被单测钉死：
/// 归错会让调用方在父进程已消失时再整整下一遍 256 MiB 的代理包。
fn classify_fetch_error(error: FetchFileError) -> FetchFailure {
    match error {
        FetchFileError::Download(msg) => FetchFailure::Download(msg),
        FetchFileError::Local(msg) => FetchFailure::Local(msg),
        FetchFileError::Cancelled => FetchFailure::Cancelled,
    }
}

/// 缓存复用：先加只读共享锁，再对锁定句柄哈希——同一句柄验证与持有。
/// 不变量：不按路径另开文件做验证，不探针 `exists()`（加锁与哈希以 Err 表达缺失）；
/// 哈希不匹配/读取失败返回 `None`，调用方删文件后重下。锁随 `VerifiedInstaller`
/// 持有至安装器启动返回（见 `launch_installer`）。
pub(super) fn try_reuse_cached_installer(
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

/// 单源流式下载并加锁重验：创建写锁文件 → 流式抓取（边读边写）→ 降级只读锁
/// → 对锁定句柄重算哈希 → 构造持锁体。
///
/// 不变量：构造的唯一依据是锁定句柄的重算哈希（`compute_sha256_hex_locked`），
/// 不按路径另开文件；失败路径尽力删文件（删除结果忽略，
/// 外部占用下可能残留，由下次缓存哈希不匹配触发重下）。
/// 错误已带中文 `op`（抓取/哈希/写入/锁定），调用方按 `FetchFailure` 决定回落。
/// `should_continue` 透传给 `http::fetch_to_file`，用于逐块取消（父进程存活判定）。
pub(super) fn fetch_verified_installer(
    temp_path: &std::path::Path,
    host: &str,
    url_path: &str,
    expected_hash_hex: &str,
    version: &str,
    should_continue: &impl Fn() -> bool,
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
    if let Err(e) = fetch_to_file(
        host,
        url_path,
        INSTALLER_MAX_BYTES,
        &mut write_lock,
        should_continue,
    ) {
        // 先释放写锁再删，否则 Windows 下删除被占用文件会失败而残留。
        drop(write_lock);
        let _ = std::fs::remove_file(temp_path);
        // 错误来源由产生处分类，不匹配文案：Download 回落代理，Local 直接返回，
        // Cancelled 静默放弃（见 classify_fetch_error）。
        return Err(classify_fetch_error(e));
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

/// 轮询单实例互斥量直至其消失（主进程完全退出），超时则放行交由安装器 taskkill 兜底。
///
/// 超时/轮询与 `--quit` 的 `quit_existing_instance`（`main.rs`）共用
/// MAIN_EXIT_WAIT_TIMEOUT_MS / MAIN_EXIT_POLL_INTERVAL_MS（见 `config` 注释）。
/// 本子进程在 main() 单例锁创建前即被拦截，自身绝不持有该互斥量。
pub(super) fn wait_main_instance_gone() {
    let name: Vec<u16> = crate::config::MUTEX_NAME.encode_utf16().collect();
    let deadline = Instant::now() + std::time::Duration::from_millis(MAIN_EXIT_WAIT_TIMEOUT_MS);
    let mut logged_probe_error = false;

    loop {
        // SAFETY: name 以 NUL 结尾；句柄仅用于存在性探测，立即关闭。
        // 最小权限：SYNCHRONIZE 只够打开既有互斥量做存在性探测；
        // 主进程提权运行时低完整性子进程只会拿到 ACCESS_DENIED 而非「已消失」。
        match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) } {
            Err(_) => {
                // SAFETY: 紧接 OpenMutexW 失败读取 last-error，中间无其他 Win32 调用。
                let last = unsafe { GetLastError() };
                if last == ERROR_FILE_NOT_FOUND {
                    return;
                }
                // 其余错误（含 ACCESS_DENIED）保守按「互斥量仍存在」继续等待，
                // 超时后交安装器 taskkill 兜底；只记首条，避免 50ms 轮询刷屏。
                if !logged_probe_error {
                    logged_probe_error = true;
                    log_event!(
                        "等待主进程退出: 互斥量仍存在或无权打开 (0x{:08X})，继续等待",
                        last.0
                    );
                }
            }
            Ok(handle) => unsafe {
                let _ = CloseHandle(handle);
            },
        }

        if Instant::now() >= deadline {
            log_event!("等待主进程退出超时，交安装器兜底");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(MAIN_EXIT_POLL_INTERVAL_MS));
    }
}

/// 重新拉起常驻主程序（仅用于 EXIT_MAIN 发出后安装未能继续的场景）。
/// 携带一次性参数让新进程推迟首个自动检查冷却周期：更新确认框刚被用户
/// 决策过，立刻再弹同一版本的确认框属于骚扰；下个冷却周期恢复正常。
pub(super) fn relaunch_main_app() {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => {
            log_event!("重新拉起主程序失败: 无法获取自身路径");
            return;
        }
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

/// 启动安装器，对「文件正被外部进程占用」类瞬态错误（典型为杀软实时扫描
/// 刚写完的安装包）做有限次重试；只读锁保持到最后一次尝试结束后才释放，
/// 它不与映像加载器冲突，重试只针对外部占用者。
pub(super) fn launch_installer(verified: VerifiedInstaller) -> InstallerLaunch {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cancelled_is_not_classified_as_download() {
        assert_eq!(
            classify_fetch_error(FetchFileError::Cancelled),
            FetchFailure::Cancelled
        );
        assert_eq!(
            classify_fetch_error(FetchFileError::Download("建立网络连接失败".to_string())),
            FetchFailure::Download("建立网络连接失败".to_string())
        );
        assert_eq!(
            classify_fetch_error(FetchFileError::Local("写入安装包文件失败".to_string())),
            FetchFailure::Local("写入安装包文件失败".to_string())
        );
    }

    #[test]
    fn test_transient_launch_errors_are_retried() {
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

    fn cache_test_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "traffic-monitor-reuse-test-{}-{}.tmp",
            std::process::id(),
            tag
        ))
    }

    #[test]
    fn test_cached_reuse_accepts_matching_locked_content() {
        let path = cache_test_path("accept");
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
    fn test_cached_reuse_rejects_tampered_content() {
        // 模拟缓存文件在重新加锁前已被篡改：先按好内容算出期望哈希，再用坏内容覆盖文件，
        // 锁定句柄重验必须拒绝构造（返回 None）。该用例不试图复现并发替换竞态。
        let path = cache_test_path("reject");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"good-installer-payload").unwrap();
        let expected_good = {
            let mut f = open_locked_installer(&path).unwrap();
            compute_sha256_hex_locked(&mut f).unwrap()
        };
        std::fs::write(&path, b"tampered-installer-payload").unwrap();
        let reused = try_reuse_cached_installer(path.clone(), "9.9.9", &expected_good);
        assert!(reused.is_none(), "锁定句柄哈希不一致时必须拒绝复用");
        let _ = std::fs::remove_file(&path);
    }
}
