//! 子进程协议：stdout 单行动作扫描、EXIT_MAIN 转发、收尾复位判定。
//!
//! stdout 单行协议：
//! - `DONE`：子进程已处理完毕，主进程继续运行。
//! - `EXIT_MAIN`：用户确认安装。必须在子进程启动安装器**之前**发出——主进程
//!   收到后立即退出并释放 exe 映像句柄，子进程等单实例互斥量消失后才提权
//!   运行安装器，从源头消除「文件正在使用」竞态；安装器内 taskkill 仅作兜底。

use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::config::WM_USER_UPDATE_ACTION;
use crate::state::UPDATE_IN_PROGRESS;
use crate::util::log_event;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UpdateAction {
    Done,
    ExitMain,
}

pub(super) struct SubprocessOutcome {
    pub(super) is_error: bool,
    /// 已在 stdout 中读到 EXIT_MAIN（协议层事实，不代表通知已送达）。
    pub(super) exit_signalled: bool,
    /// EXIT_MAIN 已成功投递给看门狗（UI 侧接到通知）。转发失败时主进程不会退出，
    /// worker 必须收尾复位进行中标志，否则后续检查被永久挡掉。
    pub(super) exit_forwarded: bool,
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
pub(super) fn run_check_subprocess(is_manual: bool) -> SubprocessOutcome {
    let failed = || SubprocessOutcome {
        is_error: true,
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
            log_event!("等待更新子进程退出失败");
            return SubprocessOutcome {
                is_error: true,
                exit_signalled,
                exit_forwarded,
            };
        }
    };

    if exit_signalled && !exit_forwarded {
        log_event!("EXIT_MAIN 已读到但未送达看门狗，主进程不会退出");
    }

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

pub(super) fn emit_protocol_line(line: &str) {
    let _ = std::io::stdout().write_all(format!("{line}\n").as_bytes());
    let _ = std::io::stdout().flush();
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
/// 只有「读到 EXIT_MAIN」且「通知已送达 UI」同时成立才免复位：此时主进程随即退出，
/// 复位反而与退出竞态。其余情况（含「读到 EXIT_MAIN 但通知未送达」）都必须复位：
/// 主进程仍在运行，不复位会让后续一切自动/手动检查被 `swap(true)` 永久挡掉，
/// 直到用户重启进程。
fn should_reset_update_progress(exit_signalled: bool, exit_forwarded: bool) -> bool {
    !(exit_signalled && exit_forwarded)
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
}
