# Agent Note：三处失败的 Win32 调用改用返回的 Error——同时把另外六处"看上去一样"的 GetLastError 钉成禁区

Status: proposed

## 问题

`windows-rs` 在把 `BOOL`/`Result` 变成 `Err` 时，内部已经读过一次 last-error 并把它封进 `windows::core::Error`。本仓库有三处在拿到 `Err` 之后**丢弃它**，改为裸调 `GetLastError()` 重新读一遍：

- `src/update/installer.rs:308-322`：`ShellExecuteExW` 返回 `Err` 后于 `:317` 读 last-error，再在 `:318` 比 `ERROR_CANCELLED`。
- `src/update/installer.rs:198-214`：`OpenMutexW` 返回 `Err` 后于 `:201` 读 last-error，再在 `:202` 比 `ERROR_FILE_NOT_FOUND`。
- `src/update/protocol.rs:367-372`：`OpenProcess` 返回 `Err` 后于 `:369` 读 last-error，再在 `:371` 比 `ERROR_INVALID_PARAMETER`。

三处今天都能工作：每处中间确实没有插入其它 Win32 调用（每处 SAFETY 注释都声明了这一点），而 `Error::from_win32()` 本身只读不清 last-error。但这条正确性依赖"crate 内部不会插入别的调用"——一个没有任何人承诺过的前提。

仓库里已有正确姿势可抄：`src/update/http.rs:41-48` 的 `win32_code_from_hresult` 把 `err.code().0` 从 `HRESULT_FROM_WIN32` 折回裸 Win32 码（`0x8007_0000` 前缀 + 低 16 位），紧接着在 `:47-48` 用 `err.code()` 而非 `GetLastError()`。注意**不能**直接拿 `err.code().0` 去比 `ERROR_FILE_NOT_FOUND` 这类常量——它们不是同一个数值空间，这正是上面那个折返函数存在的理由。

生产消费者：这三处分别决定 `InstallerLaunch::Cancelled/Failed`（`src/update/installer.rs:37-42`，直接决定 UAC 取消后是否 `relaunch_main_app`）、主进程退出等待的早退、协议层"父进程句柄无效"的判定。

非生产消费者：`src/update/installer.rs:329-361` 的三条 `InstallerLaunch` 分类用例（`test_cancelled_is_not_classified_as_download` / `test_transient_launch_errors_are_retried` / `test_permanent_launch_errors_are_not_retried`）——它们钉的是分类结果，不是取码方式，改法必须保持它们全绿。

## 提案

1. 把 `win32_code_from_hresult` 从 `src/update/http.rs:41-44` 提到共用位置（`src/util.rs` 或 `src/update/` 内的一个共用单元；它与 `http` 无耦合），保持签名与 `Option<u32>` 语义不变。
2. 三处改成 `match api(...) { Ok(x) => ..., Err(e) => { let code = win32_code_from_hresult(e.code().0 as u32); ... } }`，分支常量与顺序（`ERROR_CANCELLED` → `Cancelled`、`ERROR_FILE_NOT_FOUND` → 早退、`ERROR_INVALID_PARAMETER` → 无效句柄）逐字保持。
3. **为"取不到 Win32 码"（`win32_code_from_hresult` 返回 `None`）逐处定死落点**，三处都必须落在保守侧，且不得制造用户看不懂的提示：

- `src/update/installer.rs:317`：**不要直接落到 `InstallerLaunch::Failed(0)`**——`src/update/mod.rs:513-518` 会把 `code` 原样拼进弹框（"启动安装程序失败 (错误码: 0)"），用户拿不到任何信息。要么新增一个不带错误码的变体（只 `log_event!` + `relaunch_main_app`），要么在该分支对 `code == 0` 抑制弹框；无论哪种，"取不到码"仍然要触发重新拉起主程序。
- `src/update/installer.rs:201`：落在"继续等待"侧——只有 `ERROR_FILE_NOT_FOUND` 代表互斥量已消失，其余（含 `ACCESS_DENIED`）按仍在处理（`src/update/installer.rs:205-206` 的注释就是这个意思）。
- `src/update/protocol.rs:369`：落在 `ParentState::Unbound`。这不是新设计：`src/update/protocol.rs:374-379` 现在就写着"无权限等原因无法判定：保守按『仍在』处理"，而 `abandoned()`（`src/update/protocol.rs:466-468`）只在"父已消失且用户未确认"时为真，因此 `Unbound` 恰好等于"不放弃本次更新"，零成本。

## 明确不在本次范围

**另外六处 `GetLastError` 不要改**——它们不是同一个模式，"顺手统一"会把它们改错：

- `src/update/protocol.rs:297-299` 与 `src/main.rs:195-197`：读的是**成功**的 `CreateMutexW` 的 `ERROR_ALREADY_EXISTS`。last-error 是这条 API **文档指定的带外输出通道**，`Ok` 分支上根本没有 `Err` 可用。
- `src/main.rs:281-283`：`RegisterWindowMessageW` 返回 `u32`，不以 `Result` 表达失败（0 表示失败）。
- `src/main.rs:375-376`：`GetMessageW` 返回 `BOOL`，失败以 `-1` 表达。
- `src/window.rs:288-295` 与 `:298-309`：`SetWindowLongPtrW` 返回 `isize`，0 既可能是前值也可能是失败，因此**必须先 `SetLastError(WIN32_ERROR(0))` 再调用**；这是该 API 的正确协议，改成 `Result` 反而失去判别能力。

另外不改 `src/update/protocol.rs:424-426` 的 `WaitForSingleObject(handle, 0)` 轮询（它读的是等待结果，不是 last-error），也不改 `src/update/http.rs:47-48` 的既有用法。

## 为什么不保留？

1. **"SAFETY 注释已经声明了'中间没有别的 Win32 调用'"**——注释是断言，不是保证；它约束不了未来的 `windows-rs` 版本、编译器优化或某个被加在中间的探测调用。用返回的 `Error` 是把断言变成类型事实。
2. **"裸读 last-error 是 Win32 传统写法"**——在 windows-rs 里不是：`Err` 已经带码，读两次是重复表示，而重复表示正是这类错最经典的来源（两处取值可以不一致，且不一致时没有任何人会发现）。
3. **"改动面太小，不值得"**——改动面确实小（3 处 + 1 个函数搬家），但收益是消掉一类"只在特定 crate 版本上偶发"的失败，代价几乎为零。
4. **"那就把整个仓库的 GetLastError 全换掉"**——不行，见上节：其中 6 处靠 last-error 的带外语义工作，换掉是引入 bug。本笔记的价值有一半在"钉死这 6 处不要动"。

## 验收标准

- `grep -rn "GetLastError" src/` 的**调用点**从 9 处降到 6 处，且剩下的命中与上节列的 6 处逐一对上（不允许出现第 7 处）。
- `grep -rn "win32_code_from_hresult" src/` 命中数增加，且 `src/update/http.rs` 不再各自持有一份实现。
- 行为等价：`test_cancelled_is_not_classified_as_download`、`test_transient_launch_errors_are_retried`、`test_permanent_launch_errors_are_not_retried`（`src/update/installer.rs:329-361`）全绿；协议层 `parse_update_action` 相关用例（`src/update/protocol.rs:505-560` 区间）全绿。
- 真机各走一次：UAC 取消 → 仍走 `InstallerLaunch::Cancelled` 并重新拉起主程序；互斥量不存在 → `wait_main_instance_gone` 仍立刻返回；`--parent-pid` 传不存在 pid → 仍按"父进程句柄无效"处理。
- `cargo test --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全绿。

## 风险

- `win32_code_from_hresult` 只处理 `FACILITY_WIN32`（`0x8007xxxx`）的 code。若某个 `Err` 的 code 不在该 facility（例如 `E_HANDLE`、`E_INVALIDARG` 这类 HRESULT），折返得到 `None`。此时若把 `None` 当作 0 或某个具体常量，会误判分支——所以"取不到码"必须有独立的落点，且三处都要，不能只改一处。
- 三处改动会让代码路径从"读全局 last-error"变成"读局部 `Err`"，语义上等价但**顺序敏感**：`installer.rs:318` 的分支还带着"UAC 取消后要重新拉起"的产品语义，改完必须真机验证一次 UAC 取消，不能只靠单测。
- 若将来有人按"统一风格"把剩下 6 处一起改（本笔记第一节列的就是它），会破坏 `CreateMutexW` 的 `ERROR_ALREADY_EXISTS` 判定与 `SetWindowLongPtrW` 的 0 判别——`02`/`01` 这类笔记的"明确不在本次范围"无法覆盖它，所以本节内容应当随实施一起写进对应位置的注释。
