//! 安装器管线：缓存复用、流式下载、以默认动词启动安装器、等待它收场并读退出码、
//! 主进程退出等待。
//! 不变量：构造 `VerifiedInstaller` 的唯一依据是锁定句柄的重算哈希
//! （`compute_sha256_hex_locked`），不按路径另开文件。

use std::time::Instant;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, ERROR_LOCK_VIOLATION,
    ERROR_SHARING_VIOLATION, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use crate::config::{
    INSTALLER_EXIT_WAIT_TIMEOUT_MS, INSTALLER_LAUNCH_MAX_ATTEMPTS, INSTALLER_LAUNCH_RETRY_DELAY_MS,
    INSTALLER_MAX_BYTES, MAIN_EXIT_POLL_INTERVAL_MS, MAIN_EXIT_WAIT_TIMEOUT_MS,
    RELAUNCHED_BY_UPDATE_ARG,
};
use crate::util::{log_event, os_to_wide, to_wide, win32_code_from_hresult, win32_error_code};

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

/// 已启动的安装器进程（`SEE_MASK_NOCLOSEPROCESS` 取回）：Drop 关闭句柄，
/// [`wait_for_exit_code`](Self::wait_for_exit_code) 等它收场。
///
/// 为什么必须由更新子进程持有并等待：更新走静默安装，`installer.iss` 的 `[Run]` 条目
/// 已带 `skipifsilent`，安装成功后不再有人拉起组件；而「安装器起来了但没能成功收尾」
/// （复制阶段失败、目标文件被杀软占用、安装器崩溃/回滚）时 `[Run]` 根本不执行，旧实例
/// 早在 EXIT_MAIN 时已退出让出 exe 映像——没有人再拉起组件，用户看到的是「刚点了
/// 『是』，程序就人间蒸发」。等它收场并拿到退出码，这次交接才有确定的落点。
pub(super) struct InstallerProcess(HANDLE);

impl InstallerProcess {
    /// 等安装器收场并读 Inno 退出码；`None` 表示等待超时或取码失败（结果未知）。
    ///
    /// 上界是 `INSTALLER_EXIT_WAIT_TIMEOUT_MS`（远大于任何健康安装），它只兜「安装器永不
    /// 收场」这一种卡死：无上界等待会让本子进程与跨进程更新互斥量被永久占住，而组件早已随
    /// 旧实例退出——那是"永久冻结的假活"。超时按结果未知处置，调用方会重新拉起组件。
    /// 官方退出码表：0 = 成功跑完；其余任何值都表示没跑完（初始化失败 / 取消 / 致命错误 /
    /// 回滚）。
    pub(super) fn wait_for_exit_code(self) -> Option<u32> {
        // SAFETY: 句柄由成功的 ShellExecuteExW + SEE_MASK_NOCLOSEPROCESS 唯一取得，本类型
        // 是它的唯一持有者（Drop 中关闭一次，此处不关闭）。两个 API 都只读该句柄指向的
        // 进程状态，不消费句柄；`code` 为本地 u32。
        unsafe {
            if WaitForSingleObject(self.0, INSTALLER_EXIT_WAIT_TIMEOUT_MS) != WAIT_OBJECT_0 {
                return None;
            }
            let mut code = 0u32;
            GetExitCodeProcess(self.0, &mut code).ok()?;
            Some(code)
        }
    }
}

impl Drop for InstallerProcess {
    fn drop(&mut self) {
        // SAFETY: 句柄来自成功的 ShellExecuteExW，且本类型是它的唯一持有者，仅关闭一次。
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// 启动安装器失败的三种归因；成功的一侧由 [`InstallerProcess`] 承载。
///
/// 刻意不含「已启动」这样的单位变体：启动成功后调用方拿到的是进程句柄，于是
/// 「拿到了启动成功却忘了等它收场」在类型上就写不出来。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum InstallerLaunch {
    Cancelled,
    Failed(u32),
    /// 启动失败但 `Err` 不带裸 Win32 码（HRESULT 非 `FACILITY_WIN32`，或裸码恰为 0）。
    /// 与 `Failed` 分开是为了不把「错误码: 0」这种没有信息的话弹给用户：该分支只写
    /// 日志，重新拉起主程序的处置与 `Failed` 一致（见 `update::subprocess_main`）。
    FailedWithoutCode,
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
/// 错误已带中文 `op`（抓取/哈希/写入/锁定），调用方按 [`FetchFileError`] 的变体
/// 决定回落（Download 可回落代理，Local 直接返回，Cancelled 静默放弃）。
/// 本函数自己产生的失败（创建/锁定/哈希）一律构造 `Local`。
/// `should_continue` 透传给 `http::fetch_to_file`，用于逐块取消（父进程存活判定）。
pub(super) fn fetch_verified_installer(
    temp_path: &std::path::Path,
    host: &str,
    url_path: &str,
    expected_hash_hex: &str,
    version: &str,
    should_continue: &impl Fn() -> bool,
) -> Result<VerifiedInstaller, FetchFileError> {
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
                return Err(FetchFileError::Local("创建安装包文件失败".to_string()));
            }
            std::thread::sleep(std::time::Duration::from_millis(
                INSTALLER_LAUNCH_RETRY_DELAY_MS,
            ));
            let _ = std::fs::remove_file(temp_path);
            match create_locked_installer(temp_path) {
                Ok(f) => f,
                Err(_) => {
                    return Err(FetchFileError::Local("创建安装包文件失败".to_string()));
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
        // 错误来源由产生处分类，原样透传：Download 回落代理，Local 直接返回，
        // Cancelled 静默放弃。
        return Err(e);
    }
    // 降级为只读共享锁：映像加载器以 FILE_SHARE_READ|FILE_SHARE_DELETE 打开，
    // 不容纳并存句柄的写访问权，持写句柄启动必失败 32。先关写再开只读，
    // 反向会因共享模式冲突开锁失败。
    drop(write_lock);
    let mut file_lock = open_locked_installer(temp_path).map_err(|_| {
        let _ = std::fs::remove_file(temp_path);
        FetchFileError::Local("锁定已下载的安装包失败".to_string())
    })?;
    // 对已锁定句柄重算哈希：关写到开读之间的无锁窗口若被篡改，在此现形。
    let verified_hash = match compute_sha256_hex_locked(&mut file_lock) {
        Ok(h) => h,
        Err(e) => {
            drop(file_lock);
            let _ = std::fs::remove_file(temp_path);
            return Err(FetchFileError::Local(format!("计算安装包哈希失败: {e}")));
        }
    };
    if verified_hash.to_uppercase() != expected_hash_hex {
        drop(file_lock);
        let _ = std::fs::remove_file(temp_path);
        return Err(FetchFileError::Local(format!(
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
            Err(e) => {
                // 错误码取自 Err 自带的那份：`BOOL → Err` 转换时 crate 已经读过一次
                // last-error，这里再裸读一次是同一事实的重复表示，且依赖「中间没有别的
                // Win32 调用」这个无人承诺的前提。取不到裸码（None）落在下面的保守侧。
                if win32_code_from_hresult(e.code().0 as u32) == Some(ERROR_FILE_NOT_FOUND.0) {
                    return;
                }
                // 其余错误（含 ACCESS_DENIED）保守按「互斥量仍存在」继续等待，
                // 超时后交安装器 taskkill 兜底；只记首条，避免 50ms 轮询刷屏。
                if !logged_probe_error {
                    logged_probe_error = true;
                    log_event!(
                        "等待主进程退出: 互斥量仍存在或无权打开 (0x{:08X})，继续等待",
                        win32_error_code(&e)
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

/// 重新拉起常驻主程序（EXIT_MAIN 发出后安装未能继续，或安装器收场后确认组件不在跑）。
/// 携带一次性参数让新进程推迟首个自动检查冷却周期：更新确认框刚被用户
/// 决策过，立刻再弹同一版本的确认框属于骚扰；下个冷却周期恢复正常。
///
/// **失败必须留痕、并做有限次重试**：静默更新交接里本函数是唯一的拉起点（另一处
/// `[Run]` 只在安装成功收尾时执行），静默失败就等于「组件永久消失，而日志里查不到
/// 原因」——正是本次改动要消灭的现场。`ShellExecuteW` 返回值 ≤32 是它自己的 SE_ERR_*
/// 码（不是 last-error，故原样记录）；失败重试覆盖「exe 刚写完被杀软实时扫描占用」
/// 这类可自愈的瞬态，与安装器启动同一对常量。
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
    for attempt in 1..=INSTALLER_LAUNCH_MAX_ATTEMPTS {
        // SAFETY: 两个缓冲均含尾 NUL，ShellExecuteW 同步返回前存活。
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(path_wide.as_ptr()),
                PCWSTR(args_wide.as_ptr()),
                None,
                SW_SHOWNORMAL,
            )
        };
        // >32 即 shell 接受了请求（该 API 的返回值约定）。
        if result.0 as isize > 32 {
            return;
        }
        if attempt == INSTALLER_LAUNCH_MAX_ATTEMPTS {
            log_event!(
                "重新拉起主程序失败: ShellExecuteW 返回 {}（组件不会自动回来）",
                result.0 as isize
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(
            INSTALLER_LAUNCH_RETRY_DELAY_MS,
        ));
    }
}

/// 启动安装器，对「文件正被外部进程占用」类瞬态错误（典型为杀软实时扫描
/// 刚写完的安装包）做有限次重试；只读锁保持到最后一次尝试结束后才释放，
/// 它不与映像加载器冲突，重试只针对外部占用者。
///
/// 只覆盖「启动」这一段：成功后返回进程句柄，等待收场由调用方按
/// [`InstallerProcess::wait_for_exit_code`] 进行，因此重试判据里不含任何等待时长。
pub(super) fn launch_installer(
    verified: VerifiedInstaller,
) -> Result<InstallerProcess, InstallerLaunch> {
    let mut attempt = 1;
    let result = loop {
        match try_launch_installer(&verified.path) {
            Ok(process) => break Ok(process),
            Err(other) => {
                if !is_transient_launch_error(&other) || attempt >= INSTALLER_LAUNCH_MAX_ATTEMPTS {
                    break Err(other);
                }
            }
        }
        attempt += 1;
        std::thread::sleep(std::time::Duration::from_millis(
            INSTALLER_LAUNCH_RETRY_DELAY_MS,
        ));
    };
    // 影像加载器已在本进程创建成功时就拿到了文件，此后不必继续持有只读锁；
    // 尤其不把它带进接下来的「等安装器收场」——那段时间可能长达数秒到数分钟，
    // 没有理由让已校验的安装包在这期间被本进程锁住。
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

fn try_launch_installer(path: &std::path::Path) -> Result<InstallerProcess, InstallerLaunch> {
    let path_wide = os_to_wide(path.as_os_str());
    let params_wide = to_wide("/VERYSILENT /SUPPRESSMSGBOXES /NORESTART");

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        // NOCLOSEPROCESS：取回安装器进程句柄，等它收场并读退出码（见 InstallerProcess）。
        // 失败时抑制 Shell 自带错误框（如「另一个程序正在使用此文件」）：重试期间会连弹
        // 多个标准错误框，且与子进程的 show_error 形成双重弹窗；统一由子进程报告错误码。
        fMask: SEE_MASK_FLAG_NO_UI | SEE_MASK_NOCLOSEPROCESS,
        // 刻意不给 lpVerb（用默认动词）。"runas" 会让安装器从第一条指令起就是提权态，
        // Inno 的 SetupLdr 因此没有机会用原始凭据跑过任何代码，`[Run]` 的
        // `runasoriginaluser`（postinstall 的默认身份）随之失效——组件被安装器的管理员
        // token 拉起成 high IL，UIPI 切断它与 explorer 之间全部窗口消息交互
        // （TaskbarCreated 广播、WM_SETTINGCHANGE、--quit 的 PostMessageW），看门狗机制
        // 整体失效。默认动词下由 SetupLdr 自己提权（`installer.iss` 未设
        // PrivilegesRequired，取默认 admin；stub 的应用 manifest 是 asInvoker），它先以
        // 原始凭据跑过一段代码，`runasoriginaluser` 才真正生效。
        // 代价：UAC 取消不再由 ShellExecuteExW 同步返回 ERROR_CANCELLED——它发生在
        // SetupLdr 内部，改由「等进程退出 + 非 0 退出码」这条路径覆盖（见
        // update::complete_update_interaction）。
        lpVerb: PCWSTR::null(),
        lpFile: PCWSTR(path_wide.as_ptr()),
        lpParameters: PCWSTR(params_wide.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // SAFETY:
    // path_wide 和 params_wide 都是 NUL 终止的 UTF-16 缓冲区，并在 ShellExecuteExW
    // 同步读取 SHELLEXECUTEINFOW 期间保持存活。cbSize 与结构体实际大小一致；fMask 不含
    // 需要调用方提供额外指针的掩码，NOCLOSEPROCESS 要求调用方接管 hProcess，由
    // InstallerProcess 的 Drop 关闭。
    match unsafe { ShellExecuteExW(&mut sei) } {
        // 成功但不返回进程句柄（NOCLOSEPROCESS 下理论上只可能出现在「复用既有实例」的
        // 转发路径上）：不能等、也就无从判断收场结果，按启动失败归因并留痕；调用方对它的
        // 处置与其它启动失败一致（重新拉起主程序），不会留下「无人拉起」。
        Ok(()) if sei.hProcess.is_invalid() => {
            log_event!("安装器启动返回成功但未提供进程句柄，无法等待其收场");
            Err(InstallerLaunch::FailedWithoutCode)
        }
        Ok(()) => Ok(InstallerProcess(sei.hProcess)),
        // 失败分类读 Err 自带的码，不再裸读 last-error（见 classify_launch_hresult）。
        Err(e) => Err(classify_launch_hresult(e.code().0 as u32)),
    }
}

/// `ShellExecuteExW` 失败的唯一分类处（纯函数：入参是 `Err` 携带的 HRESULT）。
///
/// - `ERROR_CANCELLED` ⇒ `Cancelled`：UAC 被用户取消，调用方据此重新拉起主程序。
/// - 其余裸 Win32 码 ⇒ `Failed(code)`：原样交给调用方决定是否重试/弹框。
/// - 取不到裸码（HRESULT 非 `FACILITY_WIN32`）或裸码恰为 0 ⇒ `FailedWithoutCode`：
///   `Failed(0)` 会被更新协调器拼成「启动安装程序失败 (错误码: 0)」弹给用户——一句
///   没有信息、也无法据以行动的话。无码分支只写日志，重新拉起主程序的处置不变。
fn classify_launch_hresult(hresult: u32) -> InstallerLaunch {
    match win32_code_from_hresult(hresult) {
        Some(code) if code == ERROR_CANCELLED.0 => InstallerLaunch::Cancelled,
        Some(code) if code != 0 => InstallerLaunch::Failed(code),
        _ => InstallerLaunch::FailedWithoutCode,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // 「启动成功不重试」这一条不再在这里断言：它已由类型接管——`InstallerLaunch`
        // 只描述启动失败，启动成功返回的是 `InstallerProcess`，压根进不了本判据。
        assert!(!is_transient_launch_error(&InstallerLaunch::Cancelled));
        assert!(!is_transient_launch_error(&InstallerLaunch::Failed(5)));
        assert!(!is_transient_launch_error(&InstallerLaunch::Failed(2)));
        // 无码失败没有可重试的判据，必须与永久失败同侧。
        assert!(!is_transient_launch_error(
            &InstallerLaunch::FailedWithoutCode
        ));
    }

    #[test]
    fn test_launch_failure_classification_keeps_win32_codes() {
        // HRESULT_FROM_WIN32(ERROR_CANCELLED = 1223)：UAC 取消必须仍走 Cancelled。
        assert_eq!(
            classify_launch_hresult(0x8007_04C7),
            InstallerLaunch::Cancelled
        );
        // 其余 Win32 码折回裸码后原样保留，重试判据（占用/锁定）才能继续生效。
        assert_eq!(
            classify_launch_hresult(0x8007_0005),
            InstallerLaunch::Failed(5)
        );
        assert_eq!(
            classify_launch_hresult(0x8007_0020),
            InstallerLaunch::Failed(ERROR_SHARING_VIOLATION.0)
        );
    }

    #[test]
    fn test_launch_failure_without_win32_code_has_no_dialog_code() {
        // E_FAIL：非 FACILITY_WIN32，取不到裸码——必须落到无码分支，不能变成
        // Failed(0) 被拼成「错误码: 0」弹给用户。
        assert_eq!(
            classify_launch_hresult(0x8000_4005),
            InstallerLaunch::FailedWithoutCode
        );
        // 裸码恰为 0（ERROR_SUCCESS）也不是失败的诊断信息，同样归无码分支。
        assert_eq!(
            classify_launch_hresult(0x8007_0000),
            InstallerLaunch::FailedWithoutCode
        );
    }

    // ===== 安装器收场等待 =====

    /// 用真实进程钉死「等收场 + 读退出码」这条 API 路径。
    ///
    /// 安装器本体没法在单测里跑（要真机、要 UAC），但 `InstallerProcess` 的等待与取码
    /// 不依赖安装器：任意已退出进程都覆盖同一段代码，同时钉住两点——退出码**原样**取回
    /// （非 0 正是「未成功收场」那条日志分支的判据），以及句柄所有权真的归本类型
    /// （`into_raw_handle` 移交后由 Drop 唯一关闭）。
    #[test]
    fn test_wait_for_exit_code_reads_real_process_status() {
        use std::os::windows::io::IntoRawHandle;
        let child = std::process::Command::new("cmd")
            .args(["/C", "exit", "7"])
            .spawn()
            .expect("必须能启动 cmd.exe");
        let process = InstallerProcess(HANDLE(child.into_raw_handle()));
        assert_eq!(
            process.wait_for_exit_code(),
            Some(7),
            "已退出进程的退出码必须原样取回"
        );
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

    // ===== 安装器兜底强杀筛选器（installer/kill-remnant.ps1） =====

    /// PowerShell 单引号字符串字面量：内部单引号翻倍。路径可能含空格与中文。
    fn ps_single_quote(raw: &str) -> String {
        format!("'{}'", raw.replace('\'', "''"))
    }

    /// JSON 字符串字面量转义（只用 ASCII 构造数据，故只需处理反斜杠与引号）。
    fn json_escape(raw: &str) -> String {
        raw.replace('\\', "\\\\").replace('"', "\\\"")
    }

    /// 以安装器所用的同一可执行体（`powershell`，即 Windows PowerShell 5.1）执行一段
    /// 命令，返回 stdout 原始字节：断言只看 ASCII 数字，不依赖控制台代码页。
    fn powershell_stdout(args: &[&str]) -> Vec<u8> {
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass"])
            .args(args)
            .output()
            .expect("必须能启动 powershell：安装器的兜底强杀同样依赖它");
        assert!(
            output.status.success(),
            "powershell 退出码非 0：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn contains_ascii(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
    }

    /// 确定性筛选器验收：脚本不参与 cargo 构建，用构造记录直接跑 `-Processes` + `-DryRun`，
    /// 钉死「目标路径 + 当前会话 + 非更新协调者」三项收窄。旧实现在安装器内联 PowerShell，
    /// 没有任何一次真实执行的验收；本用例取代它，且不依赖「真的进入强杀分支」。
    ///
    /// 构造记录覆盖 6 个边界：(a) 三条全中 ⇒ 必须列出；(b) 命令行含 `--check-update`、
    /// (c) 异会话、(d) 命令行取不到、(e) 另一目录的同名 exe、(f) 可执行文件路径取不到
    /// ⇒ 都必须不列出。(f) 是 `提案` 3 明列的 `ExecutablePath` 为 `$null` 边界。
    /// 缺口（未实测，见提交说明）：`-DryRun` 之外的真杀路径与真实跨会话/跨目录进程
    /// 需要真机安装才能验证，本用例只钉死筛选判据。
    #[test]
    fn test_kill_remnant_selector_scope() {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("installer")
            .join("kill-remnant.ps1");
        assert!(
            script.is_file(),
            "兜底强杀筛选器必须存在：{}",
            script.display()
        );

        // 脚本用 (Get-Process -Id $PID).SessionId 取自身会话，构造记录必须以同一会话号
        // 喂入才算「同会话」；(c) 用 session + 1 构造异会话记录。
        let session: i64 = String::from_utf8_lossy(&powershell_stdout(&[
            "-Command",
            "(Get-Process -Id $PID).SessionId",
        ]))
        .trim()
        .parse()
        .expect("必须能取到当前会话号");

        let target = r"C:\verify-target\traffic-monitor.exe";
        let other_dir = r"C:\verify-other\traffic-monitor.exe";
        // 只用 ASCII 数字与 ASCII 路径构造记录，断言因此与输出编码无关。
        let records = format!(
            concat!(
                r#"[{{"ProcessId":424242,"ExecutablePath":"{0}","CommandLine":"\"{0}\"","SessionId":{2}}}"#,
                r#",{{"ProcessId":434343,"ExecutablePath":"{0}","CommandLine":"\"{0}\" --check-update","SessionId":{2}}}"#,
                r#",{{"ProcessId":444444,"ExecutablePath":"{0}","CommandLine":"\"{0}\"","SessionId":{3}}}"#,
                r#",{{"ProcessId":454545,"ExecutablePath":"{0}","CommandLine":null,"SessionId":{2}}}"#,
                r#",{{"ProcessId":464646,"ExecutablePath":"{1}","CommandLine":"\"{1}\"","SessionId":{2}}}"#,
                r#",{{"ProcessId":474747,"ExecutablePath":null,"CommandLine":"x","SessionId":{2}}}]"#
            ),
            json_escape(target),
            json_escape(other_dir),
            session,
            session + 1
        );

        let records_path = std::env::temp_dir().join(format!(
            "traffic-monitor-kill-remnant-{}.json",
            std::process::id()
        ));
        std::fs::write(&records_path, records.as_bytes()).expect("写构造记录失败");
        let command = format!(
            "& {} -Expected {} -Processes (Get-Content -Raw {} | ConvertFrom-Json) -DryRun",
            ps_single_quote(&script.to_string_lossy()),
            ps_single_quote(target),
            ps_single_quote(&records_path.to_string_lossy())
        );
        let stdout = powershell_stdout(&["-Command", &command]);
        let _ = std::fs::remove_file(&records_path);

        let listed = |pid: u32| contains_ascii(&stdout, &pid.to_string());
        assert!(
            listed(424242),
            "目标路径 + 当前会话 + 普通命令行必须被列出 (a)：{}",
            String::from_utf8_lossy(&stdout)
        );
        for (pid, why) in [
            (
                434343,
                "更新协调者（命令行含 --check-update）不得被列出 (b)",
            ),
            (444444, "异会话的同路径进程不得被列出 (c)"),
            (454545, "命令行取不到时不得被列出 (d)"),
            (464646, "另一目录的同名 exe 不得被列出 (e)"),
            (474747, "可执行文件路径取不到时不得被列出 (f)"),
        ] {
            assert!(!listed(pid), "{why}");
        }
    }
}
