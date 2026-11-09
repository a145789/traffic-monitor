# Agent Note：抽取码级瞬态判据，消除 installer 孪生占用错误判据的重复表示

Status: proposed

## 问题

`src/update/installer.rs:468` 的 `is_transient_sharing_error`（判 `io::Error::raw_os_error`）与 `src/update/installer.rs:601` 的 `is_transient_launch_error`（判 `InstallerLaunch::Failed(code)`）编码同一事实：裸码 ∈ {ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION}（`src/update/installer.rs:472` vs `:605`）。前者的文档（`src/update/installer.rs:464`-467）自认「与 `is_transient_launch_error` 同一语义」，但该承诺只靠注释维系，无编译期约束——未来新增瞬态占用码必须记得改两处。检索：`grep -rn "ERROR_SHARING_VIOLATION\|ERROR_LOCK_VIOLATION\|is_transient_sharing_error\|is_transient_launch_error" src/` 命中全部在 installer.rs：判码仅 `:472` 与 `:605` 两处，其余为 import、文档、调用点与测试构造，无第三份表示。

消费者分类：

- 生产消费者：`is_transient_sharing_error` 在 `src/update/installer.rs:181`（副本重验重试）、`src/update/installer.rs:452`（副本落盘/spawn 重试）；`is_transient_launch_error` 在 `src/update/installer.rs:584`（安装器启动重试）。
- 非生产消费者：`test_transient_launch_errors_are_retried`（`src/update/installer.rs:676`-683）与 `test_transient_sharing_errors_are_retried`（`src/update/installer.rs:686`-696）对同一码集各断言一遍；`test_permanent_launch_errors_are_not_retried`（`src/update/installer.rs:727`-737）覆盖 launch 侧永久变体（Cancelled、Failed(5)、Failed(2)、FailedWithoutCode）。

## 提案

新增码级判据 `fn is_transient_code(code: u32) -> bool` 作为两错误码的唯一判定位置；两个 wrapper 保留但各成一行委托（sharing 侧把 `raw_os_error()` 的 `Option<i32>` 映射后委托，launch 侧匹配 `Failed(code)` 后委托），调用点（`:181`、`:452`、`:584`）不动。两个瞬态测试合并为一个码级真值表测试：两码正例 + 拒绝访问（码 5）反例，同时经两个 wrapper 各走一遍以钉死委托适配；永久侧测试原样保留。预估净删约 15 行（判据 ~9 + 测试合并 ~6）。

## 明确不在本次范围

- 四套重试循环脚手架（`src/update/installer.rs:175`-191、`:364`-393、`:443`-461、`:579`-593）不抽公共泛型 helper：本次审计核算净删仅 ~3–8 行，却一次性改动四条受保护恢复路径，已否决；本提案只动判据、不动循环与重试次数/延迟。
- `InstallerLaunch` 枚举与 `classify_launch_hresult`（`src/update/installer.rs:663`）不改：`Cancelled`（UAC 取消恢复）与 `FailedWithoutCode`（无码只写日志、不弹「错误码: 0」）的区分有真实生产消费。
- 错误码集合本身不增不减：本次只消除重复表示，不改重试策略；「访问拒绝不重试」的语义由反例继续钉死。

## 为什么不保留？

最强反方：① 两个 wrapper 各自封装「裸码来源不同」的适配（`raw_os_error() -> Option<i32>` vs `Failed(u32)`），统一后适配逻辑仍在 wrapper 里，净删有限（~15 行），收益看起来主要是整洁；② 三处重试路径共用判据家族，改动回归面集中，且触碰 AGENTS.md 保护的恢复调度接缝内部。回应：① 收益核心不是行数而是事实单点化——「哪些码算瞬态占用」从两处口头承诺变成一个编译期唯一位置，新增码只改一处，真值表测试同时覆盖两侧委托；这正是「同一事实两份表示」判据针对的维护陷阱。② 循环形态、重试上限（`INSTALLER_LAUNCH_MAX_ATTEMPTS`）、延迟与只读锁释放时序全部不动，行为等价由合并后的真值表测试加现有永久侧测试钉死；接缝语义（瞬态可重试、永久立即上报）不变，变的只是判据的表示。残余语义：两种输入形态的适配层保留，共享的只有码集判定。

## 验收标准

- `grep -n "ERROR_SHARING_VIOLATION\|ERROR_LOCK_VIOLATION" src/update/installer.rs` 中码集判定只出现在 `is_transient_code` 一处，其余命中为 import、文档与测试构造。
- `cargo test --locked` 全绿；合并后的真值表测试覆盖两码正例、码 5 反例，且经两个 wrapper 分别断言；`test_permanent_launch_errors_are_not_retried` 与 `test_launch_failure_classification_keeps_win32_codes` 原样通过。
- `cargo fmt -- --check`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全过且无 warning。

## 风险

残留风险：若未来启动侧与交接侧的瞬态码集需要分叉（例如启动想多重试一个码而交接不想），共享判据要再拆开。证伪依据：当前两侧文档明认面对同一类瞬态（杀软实时扫描刚写完的文件，`src/update/installer.rs:118`、`:464`-467、`:570`-572），检索未发现任何分叉需求；真到分叉时拆开是自然演化，不构成现在保留两份表示的理由。