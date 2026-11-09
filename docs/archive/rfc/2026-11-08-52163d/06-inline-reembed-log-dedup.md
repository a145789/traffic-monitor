# Agent Note：内联 should_log_reembed_failure，删除只复述 std 语义的专用测试

Status: implemented

## 问题

`src/window.rs:421`-423 的 `should_log_reembed_failure(previous, current)` 实现全文是 `previous != Some(current)`——一行 std `Option<&str>` 相等比较的具名包装。检索 `grep -rn "should_log_reembed_failure" src/` 命中 10 处：定义 1、状态注释 1（`src/window.rs:57`）、唯一生产调用 1（`src/window.rs:430`，`report_reembed_failure` 内）、测试 import 1（`:517`）、测试断言 5（`:562`-568），模块外零命中。专用测试 `reembed_failure_log_dedup`（`src/window.rs:559`-569，11 行）的五条断言全部在复述 std PartialEq 语义（`None != Some(x)` 为真、`Some(x) != Some(x)` 为假、`Some(x) != Some(y)` 为真），没有覆盖任何本仓库自有逻辑；其注释（`:561`）声称守护「同一失败原因连续 N 次只产出 1 条记录」，但测试从未调用 `report_reembed_failure`——即使把 report 里的去重守卫整个删掉，该测试依然全绿，声称的性质与实际守护脱节（弱测试）。

消费者分类：

- 生产消费者：唯一调用点 `src/window.rs:430`。
- 非生产消费者：`reembed_failure_log_dedup`（`src/window.rs:559`-569）；状态级测试 `reembed_failure_state_is_cleared_on_recovery`（`src/window.rs:571`-583）消费的是 `report_reembed_failure`/`clear_reembed_failure` 而非本函数，保留。

## 提案

把 `src/window.rs:430` 改为 `if last.as_deref() == Some(reason) { return; }`，删除具名函数及其文档（`src/window.rs:420`-423）、`reembed_failure_log_dedup` 测试与对应 import（`:517`），更新 `src/window.rs:57` 的状态注释（读方变为 report_reembed_failure 自身）。预估净删约 15 行。

## 明确不在本次范围

- `report_reembed_failure` 的「日志在状态提交后写」顺序（`src/window.rs:425`-426 文档、`:433`-434 实现）不动：它保证写日志失败不影响去重状态。
- `clear_reembed_failure` 与 `LAST_REEMBED_FAILURE` 状态（`src/window.rs:438`-447）不动：「恢复后再失败仍要记一条」依赖共用状态的清空，由 `reembed_failure_state_is_cleared_on_recovery` 钉死。
- AGENTS.md「同一失败序列只提示一次、周期重试静默」的模态弹窗纪律不动：本提案只涉及 log_event 日志去重，不涉及任何用户提示路径。
- `diag!` 与 `log_event!` 双通道（`src/window.rs:412`-414）不动。

## 为什么不保留？

最强反方：① AGENTS.md 要求同一失败序列只提示一次，本函数是日志去重的判据锚点，删掉直接单测后若有人把比较写反，现有测试不再变红；② 具名函数让去重意图可读。回应：① 逐条核对保护面：状态级测试 `reembed_failure_state_is_cleared_on_recovery` 从清空态调用 `report_reembed_failure("嵌入失败")` 并断言状态写入为 Some——若守卫比较写反（`!=` 误为 `==`），首次失败会早退、状态保持 None、该测试变红，写反路径有保护；而「同因只记一条」的日志条数性质在删除前后都没有自动化守护（旧测试只测纯函数、不测 report，report 删守卫它照样绿），删除不减保护。② 内联后是一目了然的一行比较，处于 9 行函数体内，去重意图由 `report_reembed_failure` 的既有文档（`:425`-426）承载，不需要独立命名。残余语义（去重纪律本身）计一张反对票，但其出错面是一行比较，且首写路径有状态测试兜底。

## 验收标准

- `grep -rn "should_log_reembed_failure" src/` 0 命中。
- `cargo test --locked` 全绿：`reembed_failure_state_is_cleared_on_recovery` 原样通过；`invalidate_last_rect_clears_committed_cache` 等 window 内联测试不受影响。
- `cargo fmt -- --check`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全过且无 warning。

## 风险

残留风险：未来若有人整个删掉内联守卫（同因每次都记日志），状态测试不变红——该性质在删除前同样无自动化守护（旧纯函数测试不测 report），属既有缺口而非本次引入；日志重复不是模态弹窗风暴，不触碰 AGENTS.md 提示纪律。证伪依据：守卫是 `report_reembed_failure` 体内一行早退，代码审查可直接核对；若要补强，可以另加状态级用例断言二次同因调用不改变状态写入次数，但那超出本次删除提案的范围。