//! 子进程协议：stdout 单行动作扫描、EXIT_MAIN 转发、收尾复位判定、父身份绑定与 R1/R2 判定。
//! stdout 单行协议：
//! - `DONE`：子进程已处理完毕，主进程继续运行。
//! - `EXIT_MAIN`：用户确认安装。必须在子进程启动安装器**之前**发出——主进程
//!   看门狗收到并处理后，主进程退出并释放 exe 映像句柄；子进程等单实例互斥量消失后才提权
//!   运行安装器，从源头消除「文件正在使用」竞态；安装器内 taskkill 仅作兜底。
//! - `BUSY`：另一处更新子进程已持有更新互斥量，本次未执行任何检查。是「有效动作」
//!   但**不是**成功完成：父侧据此不推进一小时的正常冷却（见 `should_use_error_cooldown`）。
//!
//! 父身份绑定（[`ParentProbe`]）：父进程 spawn 时传 `--parent-pid <n> --parent-start <FILETIME>`，
//! 子进程用 `OpenProcess` + `GetProcessTimes` 复核创建时刻后持有句柄，之后所有存活检查
//! 都走该句柄的 `WaitForSingleObject`——只传 PID 在 PID 被复用时会误判「父还在」。
//!
//! R1/R2 动作规则（[`UpdateContext`]）：R1 = 父已消失且用户尚未确认安装 ⇒ 静默放弃
//! （不下载、不弹框、不启动安装器、不输出协议行）；R2 = 用户已在模态框点「是」⇒ 无论父
//! 是否还在都继续交接，但此时父仍在而 `EXIT_MAIN` 写失败按硬错误处理，不得启动安装器。

use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INVALID_PARAMETER, FILETIME, GetLastError, HANDLE,
    LPARAM, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateMutexW, GetCurrentProcess, GetProcessTimes, OpenProcess,
    PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZATION_SYNCHRONIZE,
    WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::PCWSTR;

use crate::config::{PARENT_PID_ARG, PARENT_START_ARG, UPDATE_MUTEX_NAME, WM_USER_UPDATE_ACTION};
use crate::state::UPDATE_IN_PROGRESS;
use crate::util::log_event;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UpdateAction {
    Done,
    ExitMain,
    /// `BUSY`：本次未执行检查（另一处更新子进程占用），非成功完成。
    Busy,
}

pub(super) struct SubprocessOutcome {
    pub(super) is_error: bool,
    /// 子进程报告 `BUSY`（本次没跑，另一处更新子进程在跑）。与 `is_error` 相互独立：
    /// `BUSY` 不是子进程失败，但绝不能被记成一次成功检查。
    pub(super) busy: bool,
    /// 已在 stdout 中读到 EXIT_MAIN（协议层事实，不代表通知已送达）。
    pub(super) exit_signalled: bool,
    /// EXIT_MAIN 已成功投递给看门狗（消息已入队，等待看门狗处理）。转发失败时主进程不会开始退出，
    /// worker 必须收尾复位进行中标志，否则后续检查被永久挡掉。
    pub(super) exit_forwarded: bool,
}

/// 主进程调用：re-exec 自身 `--check-update` 子进程，逐行解析其 stdout 协议。
///
/// winhttp/bcrypt 以及更新确认、网页打开和安装器启动等更新专属动作在子进程执行；
/// 主进程仍保留基础窗口/错误提示 API，只解析 `DONE/EXIT_MAIN` 最终动作。读到
/// `EXIT_MAIN` 时立即转发给看门狗，由看门狗直接执行退出语义而不等子进程退出——
/// 此时安装器尚未启动，主进程必须先行退出释放 exe 映像，子进程才会继续执行提权安装。
///
/// 此处使用 `spawn()` + 手动按行读取，而非 `output()`，避免后者为并发读取
/// stderr 创建一个使用默认 2MB 栈预留的隐藏线程。
pub(super) fn run_check_subprocess(is_manual: bool) -> SubprocessOutcome {
    let failed = || SubprocessOutcome {
        is_error: true,
        busy: false,
        exit_signalled: false,
        exit_forwarded: false,
    };
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => {
            log_event!("更新检查中止: 无法获取自身路径");
            return failed();
        }
    };

    let mut command = std::process::Command::new(exe);
    command.arg("--check-update");
    if is_manual {
        command.arg("--manual");
    }
    bind_parent_identity(&mut command);

    let mut child = match command
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            log_event!("更新子进程启动失败");
            return failed();
        }
    };

    let outcome_scan = match child.stdout.take() {
        Some(stdout) => {
            let mut reader = BufReader::new(stdout);
            scan_subprocess_protocol(&mut reader, post_update_action_to_watchdog)
        }
        None => ScanOutcome {
            saw_valid_action: false,
            busy: false,
            exit_signalled: false,
            read_failed: true,
            exit_forwarded: false,
        },
    };
    let ScanOutcome {
        saw_valid_action,
        busy,
        exit_signalled,
        read_failed,
        exit_forwarded,
    } = outcome_scan;

    let exit_status = match child.wait() {
        Ok(status) => status,
        Err(_) => {
            log_event!("等待更新子进程退出失败");
            return SubprocessOutcome {
                is_error: true,
                busy,
                exit_signalled,
                exit_forwarded,
            };
        }
    };

    if exit_signalled && !exit_forwarded {
        log_event!("EXIT_MAIN 已读到但未送达看门狗，主进程不会开始退出");
    }

    SubprocessOutcome {
        is_error: read_failed || !saw_valid_action || !exit_status.success(),
        busy,
        exit_signalled,
        exit_forwarded,
    }
}

/// 父侧：把自身身份写进子进程命令行。
///
/// 取不到自身创建时刻就两参数都不传：子进程必须能复核「打开的是不是同一个进程」，
/// 只传 PID 反而会让它拿到复用号段的陌生进程并把「父还在」判错（见 [`ParentProbe`]）。
/// 参数缺失时子进程退化为「无父可查」，不影响手工 `--check-update --manual`。
fn bind_parent_identity(command: &mut std::process::Command) {
    // SAFETY: GetCurrentProcess 只返回本进程的伪句柄，不失败、也不需要关闭。
    let this = unsafe { GetCurrentProcess() };
    let Some(start) = process_creation_time(this) else {
        log_event!("无法读取自身创建时刻，本次不绑定父身份");
        return;
    };
    command
        .arg(PARENT_PID_ARG)
        .arg(std::process::id().to_string())
        .arg(PARENT_START_ARG)
        .arg(start.to_string());
}

/// 子进程 stdout 协议扫描结果：[`scan_subprocess_protocol`] 的具名返回。
///
/// 替代 `(Option<UpdateAction>, bool, bool, bool)` 四元组——三个 `bool` 在位置上
/// 无法区分，调用点只能靠顺序记忆；字段名即文档，零运行时成本。
struct ScanOutcome {
    /// 是否读到过至少一个有效动作行（`DONE` / `EXIT_MAIN` / `BUSY`）；空流或全无效行时为 `false`。
    saw_valid_action: bool,
    /// 是否读到 `BUSY`（本次未执行检查）。
    busy: bool,
    /// 是否读到 `EXIT_MAIN`（协议层事实，不代表通知已送达）。
    exit_signalled: bool,
    /// 读取是否失败（I/O 错误，含遇到无效 UTF-8 行）。
    read_failed: bool,
    /// `EXIT_MAIN` 是否成功转发给看门狗（消息已入队，等待看门狗处理）。
    exit_forwarded: bool,
}

/// 逐行扫描子进程 stdout 协议，返回 [`ScanOutcome`]。
///
/// 不变量（由本模块 tests 以 Cursor 喂协议行钉死）：读到 `EXIT_MAIN` 即调用
/// `on_exit_main` 转发且仅转发一次（exit_signalled 守卫），转发发生在扫描期间、
/// 早于 `child.wait()`；调用方无补发路径。转发返回值独立于「是否读到」上报，
/// 使「消息没送出去」不再被当成「主进程即将退出」。
/// 未知行只被跳过，绝不中断读取：新旧版本混跑时旧父进程会读到不认识的 `BUSY`。
fn scan_subprocess_protocol(
    reader: &mut impl BufRead,
    mut on_exit_main: impl FnMut() -> bool,
) -> ScanOutcome {
    let mut saw_valid_action = false;
    let mut busy = false;
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
                    saw_valid_action = true;
                    if action == UpdateAction::Busy {
                        busy = true;
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
        saw_valid_action,
        busy,
        exit_signalled,
        read_failed,
        exit_forwarded,
    }
}

fn parse_update_action(stdout: &[u8]) -> Option<UpdateAction> {
    match std::str::from_utf8(stdout).ok()?.trim() {
        "DONE" => Some(UpdateAction::Done),
        "EXIT_MAIN" => Some(UpdateAction::ExitMain),
        "BUSY" => Some(UpdateAction::Busy),
        _ => None,
    }
}

/// 向父进程输出一行协议。返回值即「这行是否真的进了管道」：
/// `EXIT_MAIN` 的写入结果参与 R2 的分支判断（父仍在而写不通是硬错误），
/// 因此旧实现的 `let _ =` 静默丢弃必须去掉。
/// `pub(crate)`：`--check-update` 分支（`main.rs`）要用它输出 `BUSY`，
/// 故经 `update` 模块再导出后可见。
pub(crate) fn emit_protocol_line(line: &str) -> std::io::Result<()> {
    let mut stdout = std::io::stdout();
    stdout.write_all(format!("{line}\n").as_bytes())?;
    stdout.flush()
}

/// 通知看门狗执行「主进程退出并清理托盘」。单动作协议，消息无载荷。
///
/// 投递目标是看门狗窗口——全生命周期不重建的顶层窗口——由它直接执行收尾语义，
/// 不再经主窗口转发。返回是否成功投递：看门狗已消失时 `PostMessageW` 返回错误，
/// 调用方据此知道消息未入队，而不是把「消息丢了」当成「主进程即将退出」。
fn post_update_action_to_watchdog() -> bool {
    let Some(hwnd) = crate::window::watchdog_hwnd() else {
        return false;
    };
    // SAFETY: hwnd 已由 watchdog_hwnd 用 IsWindow 校验存活；PostMessageW 只向目标
    // 线程队列复制整数消息参数，不跨线程解引用 Rust 内存。
    unsafe { PostMessageW(Some(hwnd), WM_USER_UPDATE_ACTION, WPARAM(0), LPARAM(0)).is_ok() }
}

/// 子进程启动即申请跨进程更新互斥量：同一会话内只允许一个更新子进程。
///
/// `None` 有两种成因，调用方都按「已被占用」处理（输出 `BUSY` 并以 0 退出）：
/// 一是 `ERROR_ALREADY_EXISTS`（另一处更新子进程确实在跑），二是互斥量创建本身失败
/// （环境异常，已在 [`acquire_named_mutex`] 内记日志）。两者的处置方向相同，
/// 但排查时以日志区分。句柄须活到进程结束——它正是「本进程是唯一更新者」的存活证明。
pub(crate) fn acquire_update_mutex() -> Option<crate::ffi_guard::MutexGuard> {
    acquire_named_mutex(UPDATE_MUTEX_NAME)
}

/// 申请一个会话级命名互斥量的独占持有；同名已存在时返回 `None`。
///
/// 名字由调用方给出，便于用测试专属名字覆盖「第二次申请必须报占用」这条性质，
/// 而不去和真实运行中的更新子进程抢同一个会话级对象。
/// 互斥量创建本身失败同样按「已被占用」收尾：无法确认独占时继续空跑一次完整检查
/// （还可能弹框）比放弃本轮更糟，且错误冷却会在数分钟后重试；失败原因写日志留痕。
fn acquire_named_mutex(name: &str) -> Option<crate::ffi_guard::MutexGuard> {
    // 名字以 NUL 结尾：常量自带尾 NUL，测试用 format! 显式补上。
    let name_wide: Vec<u16> = name.encode_utf16().collect();
    // SAFETY: name_wide 以 NUL 结尾；成功时句柄交由 MutexGuard 关闭。
    let handle = match unsafe { CreateMutexW(None, true, PCWSTR(name_wide.as_ptr())) } {
        Ok(handle) => handle,
        Err(e) => {
            log_event!("创建更新互斥量失败: {e}");
            return None;
        }
    };
    // SAFETY: 紧接 CreateMutexW 读取 last-error，中间无其他可覆盖它的 Win32 调用。
    let last = unsafe { GetLastError() };
    if last == ERROR_ALREADY_EXISTS {
        // 重复路径：句柄不交给 MutexGuard，须在此自行关闭（与单例锁同一写法）。
        // SAFETY: handle 由紧邻的 CreateMutexW 成功返回，仅关闭一次。
        let _ = unsafe { CloseHandle(handle) };
        return None;
    }
    Some(crate::ffi_guard::MutexGuard(handle))
}

/// `GetProcessTimes` 的 `FILETIME` 转 `u64`（高 32 位在前）。
fn filetime_to_u64(time: FILETIME) -> u64 {
    ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64
}

/// 读取指定进程的创建时刻；读取失败返回 None（调用方按无法判定处理）。
fn process_creation_time(process: HANDLE) -> Option<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: process 是 OpenProcess/GetCurrentProcess 返回的进程句柄；四个 FILETIME
    // 输出缓冲区均为本函数栈上有效内存。
    unsafe {
        GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user).ok()?;
    }
    Some(filetime_to_u64(creation))
}

/// 子进程侧的父进程存活探针：父身份 = 进程号 **+ 创建时刻**。
///
/// 单点真值源：只有 [`ParentProbe::bind`] 能写状态，绑定后存活结论只由父进程句柄的
/// 等待结果派生，不存在第二处可写位置。
///
/// 为什么必须复核创建时刻：长时间开机的机器上 PID 必然被复用，只按 PID `OpenProcess`
/// 会打开到另一个进程并把「父还在」判错；创建时刻是唯一能把「同一个进程」与
/// 「复用了号段的陌生进程」区分开的依据（PID 相同不算边界，`FILETIME` 精确相等才算）。
pub(super) struct ParentProbe {
    state: ParentState,
}

enum ParentState {
    /// 无父可查（参数缺失，或探测降级）：存活检查恒为真，绝不误杀手工调用。
    Unbound,
    /// 已复核创建时刻并持有父进程句柄；存活与否由句柄的等待结果决定。
    Bound(HANDLE),
    /// 已确认父进程不在：进程不存在，或 PID 已被复用（创建时刻不符）。
    Gone,
}

impl ParentProbe {
    /// 按父身份参数绑定；任一参数缺失即退化为「无父可查」。
    pub(super) fn bind(pid: Option<u32>, start: Option<u64>) -> Self {
        let (Some(pid), Some(start)) = (pid, start) else {
            return Self {
                state: ParentState::Unbound,
            };
        };

        // SYNCHRONIZE 是通用对象同步权限（0x00100000），与 `PROCESS_QUERY_LIMITED_INFORMATION`
        // 同为最小权限组合：前者够 WaitForSingleObject，后者够 GetProcessTimes。
        // windows crate 只在 Storage::FileSystem 暴露同名常量（FILE_ACCESS_RIGHTS），
        // 故按位等值复用 SYNCHRONIZATION_SYNCHRONIZE 的数值。
        let access = PROCESS_ACCESS_RIGHTS(SYNCHRONIZATION_SYNCHRONIZE.0)
            | PROCESS_QUERY_LIMITED_INFORMATION;
        // SAFETY: pid 来自父进程命令行；OpenProcess 只按号段查询，失败返回 Err 且不产生句柄。
        let opened = unsafe { OpenProcess(access, false, pid) };
        let handle = match opened {
            Ok(handle) => handle,
            Err(_) => {
                // SAFETY: 紧接失败的 OpenProcess 读取 last-error，中间无其他 Win32 调用。
                let last = unsafe { GetLastError() };
                return Self {
                    state: if last == ERROR_INVALID_PARAMETER {
                        // 号段不存在：父进程已消失。
                        ParentState::Gone
                    } else {
                        // 无权限等原因无法判定：保守按「仍在」处理。判错方向的代价不对称——
                        // 误判「已消失」会让一次用户/系统已发起的更新被静默放弃。
                        log_event!("父身份探测失败 (0x{:08X})，本次不做父进程存活检查", last.0);
                        ParentState::Unbound
                    },
                };
            }
        };

        match process_creation_time(handle) {
            Some(actual) if actual == start => Self {
                state: ParentState::Bound(handle),
            },
            Some(_) => {
                // 创建时刻不相等：该号段已是别的进程，父进程其实早已退出。
                // SAFETY: handle 由刚才成功的 OpenProcess 返回，此处是它唯一持有者，仅关闭一次。
                unsafe {
                    let _ = CloseHandle(handle);
                }
                Self {
                    state: ParentState::Gone,
                }
            }
            None => {
                // 读不出创建时刻就无法复核身份：按「无父可查」降级（恒视为仍在），
                // 与上面 OpenProcess 非「进程不存在」失败同一处置方向——判错方向的
                // 代价不对称，宁可少一层保护，也不静默放弃一次正当更新。
                log_event!("读取父进程创建时刻失败，本次不做父进程存活检查");
                // SAFETY: handle 由刚才成功的 OpenProcess 返回，此处是它唯一持有者，仅关闭一次。
                unsafe {
                    let _ = CloseHandle(handle);
                }
                Self {
                    state: ParentState::Unbound,
                }
            }
        }
    }

    /// 父进程是否仍在运行。
    ///
    /// 只有 `WAIT_OBJECT_0`（进程已终止）才算消失：`WAIT_TIMEOUT` 与 `WAIT_FAILED`
    /// 都归入「仍在」——误判「已消失」会让一次用户已发起的更新被静默放弃，
    /// 误判「仍在」只是让本次检查白跑一趟。
    pub(super) fn is_alive(&self) -> bool {
        match &self.state {
            ParentState::Unbound => true,
            ParentState::Gone => false,
            // SAFETY: 句柄由 OpenProcess 成功返回且尚未关闭；0 毫秒超时只做一次状态查询，不阻塞。
            ParentState::Bound(handle) => unsafe {
                WaitForSingleObject(*handle, 0) != WAIT_OBJECT_0
            },
        }
    }
}

impl Drop for ParentProbe {
    fn drop(&mut self) {
        if let ParentState::Bound(handle) = &self.state {
            // SAFETY: 句柄来自成功的 OpenProcess，且本类型是它唯一持有者。
            unsafe {
                let _ = CloseHandle(*handle);
            }
        }
    }
}

/// 子进程侧的 R1/R2 动作规则与唯一判定处。
///
/// 单点真值源：`user_confirmed` 只在用户于模态框点「是」时置位一次、此后不清除；
/// 父进程是否仍在每次都向 [`ParentProbe`] 现取、不缓存——缓存会把「检查」与
/// 「动作」之间的窗口重新拉大。
pub(super) struct UpdateContext {
    probe: ParentProbe,
    user_confirmed: bool,
}

impl UpdateContext {
    pub(super) fn new(pid: Option<u32>, start: Option<u64>) -> Self {
        Self {
            probe: ParentProbe::bind(pid, start),
            user_confirmed: false,
        }
    }

    /// R1：父已消失、且用户尚未确认安装 ⇒ 本次更新必须静默放弃
    /// （不下载、不弹框、不启动安装器、不输出协议行）。
    ///
    /// 分界为什么是「用户确认」：用户点「是」之前从未对「主程序已经关掉之后还要不要装」
    /// 表达过意见，父进程消失即撤销其意图；点过「是」之后继续交接是用户刚刚表达的
    /// 明确意图（R2），不再依赖父进程还在。
    pub(super) fn abandoned(&self) -> bool {
        !self.user_confirmed && !self.probe.is_alive()
    }

    /// R2 分界：记录用户已在模态框里点「是」。
    pub(super) fn mark_user_confirmed(&mut self) {
        self.user_confirmed = true;
    }

    /// 父进程当前是否仍在（R2 中 `EXIT_MAIN` 写失败的处置依据）。
    pub(super) fn parent_alive(&self) -> bool {
        self.probe.is_alive()
    }
}

/// worker 收尾判定：返回 true 表示本次检查已结束、调用方还需压缩内存。
///
/// 判定本身抽成纯函数（[`should_reset_update_progress`]）以便单测；
/// 本函数只负责把结论落到全局标志上。
pub(super) fn reset_update_progress_after_check(outcome: &SubprocessOutcome) -> bool {
    if !should_reset_update_progress(outcome.exit_signalled, outcome.exit_forwarded) {
        return false;
    }
    UPDATE_IN_PROGRESS.store(false, Ordering::Release);
    true
}

/// 本次检查结束后是否必须复位进行中标志。
///
/// 只有「读到 EXIT_MAIN」且「通知已入队」同时成立才免复位：此时看门狗随后会处理退出请求，
/// 复位反而与退出竞态。其余情况（含「读到 EXIT_MAIN 但消息未入队」）都必须复位：
/// 主进程仍在运行，不复位会让后续一切自动/手动检查被 `swap(true)` 永久挡掉，
/// 直到用户重启进程。
fn should_reset_update_progress(exit_signalled: bool, exit_forwarded: bool) -> bool {
    !(exit_signalled && exit_forwarded)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(parse_update_action(b"BUSY"), Some(UpdateAction::Busy));
        assert_eq!(parse_update_action(b"  BUSY\r\n"), Some(UpdateAction::Busy));
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

    fn scan(data: &[u8]) -> (ScanOutcome, usize) {
        let mut reader = std::io::Cursor::new(data);
        let mut forwards = 0usize;
        let outcome = scan_subprocess_protocol(&mut reader, || {
            forwards += 1;
            true
        });
        (outcome, forwards)
    }

    fn scan_with_dead_target(data: &[u8]) -> ScanOutcome {
        let mut reader = std::io::Cursor::new(data);
        scan_subprocess_protocol(&mut reader, || false)
    }

    #[test]
    fn test_scan_exit_main_forwards_exactly_once() {
        // forwards（回调次数）与 outcome.exit_forwarded（送达结果）是两个独立观测通道：
        // 前者证明"只转发一次"，后者证明"转发成功被正确上报"，互不可推导。
        let (outcome, forwards) = scan(b"EXIT_MAIN\n");
        assert!(outcome.saw_valid_action);
        assert!(!outcome.busy);
        assert!(outcome.exit_signalled);
        assert!(!outcome.read_failed);
        assert_eq!(forwards, 1);
        assert!(outcome.exit_forwarded);
    }

    #[test]
    fn test_scan_duplicate_exit_main_forward_only_once() {
        let (outcome, forwards) = scan(b"EXIT_MAIN\nEXIT_MAIN\nEXIT_MAIN\n");
        assert!(outcome.exit_signalled);
        assert_eq!(forwards, 1);
    }

    #[test]
    fn test_scan_done_does_not_forward() {
        let (outcome, forwards) = scan(b"DONE\n");
        assert!(outcome.saw_valid_action);
        assert!(!outcome.busy);
        assert!(!outcome.exit_signalled);
        assert!(!outcome.read_failed);
        assert_eq!(forwards, 0);
    }

    #[test]
    fn test_scan_busy_is_valid_action_without_completion() {
        let (outcome, forwards) = scan(b"BUSY\n");
        assert!(outcome.saw_valid_action);
        assert!(outcome.busy);
        assert!(!outcome.exit_signalled);
        assert!(!outcome.read_failed);
        assert_eq!(forwards, 0);
    }

    #[test]
    fn test_scan_unknown_line_before_busy_does_not_stop_reading() {
        let (outcome, _) = scan(b"NO_UPDATE\nBUSY\n");
        assert!(outcome.busy);
        assert!(outcome.saw_valid_action);
        assert!(!outcome.read_failed);
    }

    #[test]
    fn test_scan_invalid_lines_do_not_block_later_exit_main() {
        let (outcome, forwards) = scan(b"NO_UPDATE\nEXIT_MAIN|extra\nEXIT_MAIN\n");
        assert!(outcome.saw_valid_action);
        assert!(!outcome.busy);
        assert!(outcome.exit_signalled);
        assert!(!outcome.read_failed);
        assert_eq!(forwards, 1);
    }

    #[test]
    fn test_scan_empty_stream() {
        let (outcome, forwards) = scan(b"");
        assert!(!outcome.saw_valid_action);
        assert!(!outcome.busy);
        assert!(!outcome.exit_signalled);
        assert!(!outcome.read_failed);
        assert_eq!(forwards, 0);
    }

    #[test]
    fn test_scan_invalid_utf8_marks_read_failed() {
        let (outcome, forwards) = scan(&[0xFF, 0xFE, b'\n']);
        assert!(!outcome.saw_valid_action);
        assert!(!outcome.exit_signalled);
        assert!(outcome.read_failed);
        assert_eq!(forwards, 0);
    }

    #[test]
    fn test_scan_dead_target_still_signals_but_not_forwarded() {
        let outcome = scan_with_dead_target(b"EXIT_MAIN\n");
        assert!(outcome.exit_signalled);
        assert!(!outcome.exit_forwarded);
    }

    #[test]
    fn test_should_reset_update_progress_matrix() {
        assert!(!should_reset_update_progress(true, true));
        assert!(should_reset_update_progress(true, false));
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
            busy: false,
            exit_signalled: true,
            exit_forwarded: false,
        };
        assert!(reset_update_progress_after_check(&outcome), "仍需收尾");
        assert!(!UPDATE_IN_PROGRESS.load(Ordering::Acquire));
    }

    fn own_identity() -> (u32, u64) {
        // SAFETY: GetCurrentProcess 只返回本进程伪句柄，不失败、无需关闭。
        let this = unsafe { GetCurrentProcess() };
        let start = process_creation_time(this).expect("读取自身创建时刻");
        (std::process::id(), start)
    }

    #[test]
    fn test_parent_probe_binds_own_process_and_reports_alive() {
        let (pid, start) = own_identity();
        let probe = ParentProbe::bind(Some(pid), Some(start));
        assert!(
            matches!(probe.state, ParentState::Bound(_)),
            "身份完整且创建时刻相符时必须持有句柄"
        );
        assert!(probe.is_alive(), "绑定自身身份的探针必须报告存活");
    }

    #[test]
    fn test_parent_probe_rejects_reused_pid_by_creation_time() {
        // 同一 PID、不同创建时刻（PID 复用场景）：必须判为已消失，
        // 而不是「OpenProcess 成功就算父还在」。
        let (pid, start) = own_identity();
        let probe = ParentProbe::bind(Some(pid), Some(start ^ 0xFFFF));
        assert!(!probe.is_alive(), "创建时刻不符即父进程已消失");
    }

    #[test]
    fn test_parent_probe_degrades_without_full_identity() {
        // 参数缺失 ⇒ 无父可查，恒报存活：手工 `--check-update --manual` 绝不能被误杀。
        assert!(ParentProbe::bind(None, None).is_alive());
        let (pid, _) = own_identity();
        assert!(ParentProbe::bind(Some(pid), None).is_alive());
        assert!(ParentProbe::bind(None, Some(1)).is_alive());
    }

    #[test]
    fn test_update_context_r1_r2_boundary() {
        // 无父可查时 R1 永不成立。
        let mut ctx = UpdateContext::new(None, None);
        assert!(!ctx.abandoned(), "无父可查不得判为放弃");

        // 父已消失（创建时刻不符）⇒ R1 成立。
        let (pid, start) = own_identity();
        let mut gone = UpdateContext::new(Some(pid), Some(start ^ 0xFFFF));
        assert!(gone.abandoned(), "父已消失且未确认时应放弃");

        // 用户确认后（R2 分界）不再受父进程是否还在影响。
        gone.mark_user_confirmed();
        assert!(!gone.abandoned(), "用户已确认后不得因父进程消失而放弃");

        ctx.mark_user_confirmed();
        assert!(!ctx.abandoned());
    }

    #[test]
    fn test_named_mutex_reports_busy_then_releases() {
        // 用测试专属名字：真实运行中的更新子进程（本机可能正在跑小组件）持有的是
        // UPDATE_MUTEX_NAME，用同一个名字会让本用例偶发假红。
        let name = format!("TrafficMonitor_Mutex_Test_{}\0", std::process::id());

        let first = acquire_named_mutex(&name).expect("首次申请应成功");
        assert!(
            acquire_named_mutex(&name).is_none(),
            "同名互斥量已存在时必须报占用（这就是 BUSY 的来源）"
        );

        // 最后一个句柄关闭即销毁命名对象，同一名字可再次独占。
        drop(first);
        assert!(acquire_named_mutex(&name).is_some(), "释放后应可重新取得");
    }
}
