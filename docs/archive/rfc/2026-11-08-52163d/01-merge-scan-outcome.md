# Agent Note：合并 ScanOutcome 进 SubprocessOutcome，消除协议扫描结果的镜像表示

Status: implemented

## 问题

`src/update/protocol.rs:172` 的私有 struct `ScanOutcome` 与 `src/update/protocol.rs:45` 的 `SubprocessOutcome` 是同一事实的两份表示：`busy` / `exit_signalled` / `exit_forwarded` 三字段在两个 struct 间逐字复制（解构 `src/update/protocol.rs:114`-120，wait 失败臂重建 `src/update/protocol.rs:126`-131，最终重建 `src/update/protocol.rs:139`-144）；`saw_valid_action` 与 `read_failed` 的唯一生产消费点是 `src/update/protocol.rs:140` 折叠进 `is_error`，此后再无读取。检索：`grep -rn "saw_valid_action\|read_failed\|ScanOutcome" src/` 命中 34 处全部在 protocol.rs 内；`grep -rn "exit_signalled\|exit_forwarded" src/` 确认生产读取点仅 `src/update/protocol.rs:135` 与 `src/update/protocol.rs:494`，其余为构造、搬运与测试。

消费者分类：

- 生产消费者：`ScanOutcome` 不出 protocol.rs——构造 `src/update/protocol.rs:106`（stdout 不可用臂）、解构 `:114`、返回类型 `:195`、构造 `:228`。下游 `SubprocessOutcome` 跨模块消费：`src/update/mod.rs:189`（`.busy`）、`src/update/mod.rs:206`（`.is_error`）、`src/update/mod.rs:211`（传 `&outcome` 给 `reset_update_progress_after_check`，在 `src/update/protocol.rs:494` 读 `exit_signalled`/`exit_forwarded`）。
- 非生产消费者：测试 helper `src/update/protocol.rs:544`、`:554` 以 `ScanOutcome` 为返回类型；`src/update/protocol.rs:564`-640 逐字段断言，其中 `saw_valid_action`/`read_failed` 的区分（如 `test_scan_empty_stream` 断言两者皆 false、`test_scan_invalid_utf8_marks_read_failed` 断言后者 true）只服务测试诊断，无生产行为差异。

## 提案

让 `scan_subprocess_protocol` 直接返回 `SubprocessOutcome`：扫描内部把 `read_failed || !saw_valid_action` 折进 `is_error`；stdout 不可用臂（`src/update/protocol.rs:106`）直接构造 `is_error: true`；`run_check_subprocess` 在 `child.wait()` 成功后仅做 `outcome.is_error |= !exit_status.success()`，wait 失败臂置 `is_error = true` 并原样返回。删除 `ScanOutcome` struct 及其文档（`src/update/protocol.rs:168`-183）、解构与两处重建。测试 helper 与断言改用 `is_error` 口径，转发次数（`forwards`）与送达结果（`exit_forwarded`）的双通道断言原样保留。预估净删 30–38 行。

## 明确不在本次范围

- `busy` 的独立字段地位与「BUSY 不是错误但绝不能记成成功检查」语义（`src/update/protocol.rs:47`-49、`src/update/mod.rs:189`-207 的 `should_use_error_cooldown`）有真实生产消费者，不得折叠进 `is_error`。
- `exit_signalled`/`exit_forwarded` 双通道与 `should_reset_update_progress`（`src/update/protocol.rs:494`-508）是退出交接不变量的核心，不得合并或删减。
- 「读到 EXIT_MAIN 即转发、早于 child.wait()、仅转发一次」的时序不变量（`src/update/protocol.rs:187`-190 文档与 `:213`-217 实现）与返回类型无关，不得改动。
- `http.rs:278` 的 `FetchFileError` 零消费者 derive 是另一处独立冗余（净删仅 1 行，低于立笔记门槛），不并入本次。

## 为什么不保留？

最强反方：① `ScanOutcome` 文档（`src/update/protocol.rs:168`-171）明言它是为取代「位置不可辨的四元组」刻意引入的具名返回，两 struct 代表「扫描原始事实」与「最终记账」两个阶段，分离有文档背书；② 合并后测试失去 `saw_valid_action` 与 `read_failed` 的区分，`test_scan_empty_stream` 与 `test_scan_invalid_utf8_marks_read_failed` 的断言都收敛成 `is_error = true`，诊断精度下降。回应：① `SubprocessOutcome` 同样是具名字段 struct，具名返回的收益完整保留——被删的是三字段镜像搬运，不是命名；「两阶段」的唯一实质差异就是两个 bool 的折叠时机，从 wait 边界移到扫描边界不改变任何生产取值（逐臂核对：正常路径 `is_error = read_failed || !saw_valid_action || !exit_status.success()` 不变，wait 失败与 stdout 缺失臂仍恒为 true）。② 该区分在生产路径零消费者（唯一折叠点 `src/update/protocol.rs:140` 对两者一视同仁），合并后两个用例仍分别钉死「空流判错」「无效 UTF-8 判错」，对生产行为的保护不减；损失的只是测试内部的归因标签。

## 验收标准

- `grep -rn "ScanOutcome" src/` 0 命中。
- `cargo test --locked` 全绿；`test_scan_exit_main_forwards_exactly_once`、`test_scan_duplicate_exit_main_forward_only_once`、`test_scan_busy_is_valid_action_without_completion`、`test_scan_unknown_line_before_busy_does_not_stop_reading`、`test_scan_dead_target_still_signals_but_not_forwarded`、`test_should_reset_update_progress_matrix` 的转发/送达/重置断言原样通过；`test_scan_empty_stream` 与 `test_scan_invalid_utf8_marks_read_failed` 改断言 `is_error` 后仍各自存在。
- `cargo fmt -- --check`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全过且无 warning。

## 风险

残留风险：未来排查「子进程没输出」与「管道读失败」时无法从返回值归因（都是 `is_error = true`）。缓解：`run_check_subprocess` 现有日志路径不变，真需要归因时可在扫描内部补日志，成本一行；两个输入形态仍各有独立测试用例钉死行为。证伪依据：若未来出现需要区分两者的生产消费者（例如按归因走不同冷却），本提案的前提即失效，应恢复具名中间态——当前检索确认不存在该消费者。