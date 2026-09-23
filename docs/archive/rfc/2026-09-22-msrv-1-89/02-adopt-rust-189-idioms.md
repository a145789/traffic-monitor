# Agent Note：为 let-chain 收尾，并把 1.88–1.89 能力对账的结论登记在案

Status: implemented

> 实施补记：01 号落地实测发现，`rust-version` 抬到 1.89 当天，stable clippy 的 `collapsible_if` 即对本笔记「问题」里的两处嵌套强制报错（该 lint 感知 MSRV，≥1.88 时解锁 let-chain 建议），01 号「门禁全绿」的验收无法在不动 `src/` 的前提下达成。故提案（1）的两处塌缩已随 01 号的 PR 落地——「风险二」的不合并建议让位于门禁硬约束；本笔记其余项（2）（3）仍待办，验收时（1）的三条 grep 直接对已落地代码核对。

## 问题

`src/main.rs:130` 与 `src/update/mod.rs:121` 两行注释把「为什么这里不写成 let-chain」写进了代码，唯一原因是 MSRV 1.85：`src/main.rs:131-132` 是 `if let Ok(h) = hwnd {` 套 `if !h.is_invalid() {`，`src/update/mod.rs:122-123` 是 `if let Some(t) = *last {` 套 `if t.elapsed().as_secs() < AUTO_CHECK_COOLDOWN_SECS {`，两处外层 `if let` 都没有 else 分支（`src/update/mod.rs:124-126` 的 `return` 在内层块内）。同目录 `01-bump-msrv-1-89-and-ci.md` 一旦落地，这两行注释即成为假陈述——本仓正在清理的正是这类「与事实不符的注释」。

除此之外，本笔记还做了一次「1.88–1.89 新增语言/库能力 vs 本仓现行写法」的逐条对账，结论是**本仓只有 let-chains 一个落点**；1.89 之后的已知候选一律超出目标 MSRV，本文不实施，只连同各自的收益分析登记为「下一次提级候选」，检索记录逐条如下。

（1）嵌套合取形态：内置 grep `^\s*if let .*\{\s*$`（`src/`）命中 23 行，多行检索「`if let` 行紧跟一条 `if` 行」命中 3 处——`src/main.rs:131-132`、`src/update/mod.rs:122-123`，以及 `src/window.rs:158-159`（`get_taskbar_hwnd` 内的 `if let Some(hwnd) = TASKBAR_HWND.load() {` 套 `if unsafe { IsWindow(Some(hwnd)) }.as_bool() {`）。第 3 处**不可塌缩**：它的内层条件失败后有一个副作用 `TASKBAR_HWND.clear()`（`src/window.rs:162`），let-chain 写法要么把 clear 变成无条件执行（行为改变），要么改成二次 `load()` 判断（新增一次原子读并引入与并发 `store` 的竞态），故本文只处理前两处，并把第 3 处记为「同形但不可塌缩」。

（2）超出 1.89 的 match 守卫 `if let`（1.95 才稳定，`if let` guards on match arms）：多行检索「match 臂体首个语句是 `if let`」命中 1 处——`src/update/protocol.rs:155-156` 的 `Ok(_) => { if let Some(action) = parse_update_action(line.as_bytes()) {`。改成守卫 `Ok(_) if let Some(action) = ...` 并不更好：需要为「读到了行但解析失败」补一个显式的空 `Ok(_) => {}` 臂（当前语义由 if-let 的隐含 else 承担），可读性与行数都不赚，不采纳。

（3）超出 1.89 的 `cfg_select!`（1.95 才稳定）：内置 grep `#\[cfg\(`（`src/`）命中 17 行，其中 `#[cfg(test)]` 15 行、`#[cfg(debug_assertions)]` 与 `#[cfg(not(debug_assertions))]` 一对（`src/util.rs:253`、`src/util.rs:271`）；后者是成对函数定义而非表达式选择，宏替不了，不采纳。

（4）超出 1.89 的 `Atomic*::update` / `try_update`（1.95 才稳定）：内置 grep `compare_exchange|fetch_update`（`src/`）0 命中——本仓原子用法是 `swap`（`src/util.rs:109`、`src/util.rs:152` 的 take 语义）与位集的 `fetch_or` / `fetch_and`（`src/state.rs` 的 `SuspendReasons`），没有 CAS 循环可简化，不采纳。

（5）超出 1.89 的 `Duration::from_mins` / `from_hours`（1.91 才稳定）：本仓时间常量以 `_SECS` / `_MS` 命名并被整数比较消费（定义见 `src/config.rs:85`、`src/config.rs:115`；消费点如 `src/update/cache.rs:52` 的 `age.as_secs() > INSTALLER_CACHE_MAX_AGE_SECS`），换单位会牵动多处消费者并违反 `AGENTS.md:97` 的常量归位纪律，收益为零，不采纳。

（6）超出 1.89 的其余已知候选（1.96 `assert_matches!`、1.97 `dead_code_pub_in_binary`、1.98 `{integer}::format_into`；上列（2）-（5）各项同属超出 1.89）本文一律不采纳，登记为「下一次提级候选」，见「提案（3）」。

## 提案

（1）两处塌缩为 let-chain 并删注释：`src/main.rs:130-132` 的注释与嵌套改为 `if let Ok(h) = hwnd && !h.is_invalid() {`；`src/update/mod.rs:121-123` 的注释与嵌套改为 `if let Some(t) = *last && t.elapsed().as_secs() < AUTO_CHECK_COOLDOWN_SECS {`。两处都是纯塌缩：绑定作用域与第二个条件的求值时机逐字不变（外层 `if let` 无 else 分支时，let-chain 就是这两层嵌套的语法糖），两处 `return` 的归属不变。净删 2 行（两行注释），两段代码块整体减一级缩进（行数不变）。

（2）次要项（**与 MSRV 无关，可单独放弃而不影响（1）**）：两处 `#[allow]` 转 `#[expect]`，让「豁免理由消失」时编译器报警而不是静默保留——`src/util.rs:260` 的 `#[allow(unused_unsafe)]` 与 `src/tray.rs:46` 的 `#[allow(clippy::manual_dangling_ptr)]`（内置 grep `#\[allow` 在 `src/` 恰命中这 2 处）。两者的失效模式与本仓「注释不得与事实不符」的纪律同类：豁免一旦不再必要，`#[allow]` 会永远留下去。`#[expect]` 自 1.81 可用，故此条不依赖 01 号笔记的 MSRV 抬升。

（3）登记「下一次提级候选」，作为未来把 MSRV 继续往上抬的判决依据（本文不实施）：这些候选里只有 1.97 的 `dead_code_pub_in_binary` 有潜在对口点——本仓是 binary crate（`Cargo.toml` 无 `[lib]`，入口 `src/main.rs`），该 lint 能把「`pub` 项在二进制内无生产消费者」变成编译期信号（allow-by-default，需在 `Cargo.toml:50-53` 的 `[lints]` 表显式开启；是否真有命中要在开启当天实测，本文不预告数量）；1.96 的 `assert_matches!` 在本仓**无落点**（内置 grep `matches!\(` 仅 2 处：`src/update/mod.rs:314`、`src/update/installer.rs:259`，都是取值而非断言）；1.98 的 `{integer}::format_into` + `core::fmt::NumBuffer` 的对口点是 `src/renderer.rs:595` 的手写 `write_u32`（有专属测试 `test_write_u32`，见 `src/renderer.rs:668`，且该函数处于 `src/renderer.rs:525` 所述「渲染热路径零分配」纪律之下），换与不换需单独评估，不在本文承诺范围内。

## 明确不在本次范围

**不做风格类重构**：函数拆分、重复表示合并、测试合并等同类工作已有的审计路径与节奏，本文只做「因旧 MSRV 而不能写」的写法收复，加两处会静默变陈旧的 `#[allow]`；两处塌缩之外的 `if let` 一律不动。

**不动 `src/window.rs:158-159` 的同形嵌套**：理由见「问题（1）」——塌缩会改变 `TASKBAR_HWND.clear()` 的执行条件，或引入第二次原子读与竞态。

**不把 let-chain 推广到其余 21 处 `if let`**：它们内部没有第二个条件，链式写法不减少任何行，属于无收益的风格偏好。

**不引入任何需要 1.90+ 的能力**（超出 1.89 的候选登记见「提案（3）」），也不触碰 `Cargo.toml` 的 `[lints]` 表——那是提级当天的事。

**不改 `AGENTS.md` 第 9/10 条的既有风格约束**（`unsafe` 旁注只写不变量、原子序约定、跨消息事实的单点真值源）：两处塌缩不涉及这些；若实施中发现必须动它们，说明方案已超出本文范围。

## 为什么不保留？

最强反方：净删只有 2 行注释、代码块各减一级缩进，成本却是一篇笔记加一次 PR，不成比例；更省事的做法是只把那两行注释改写成新事实（例如「MSRV 已 ≥1.88，此处保留嵌套仅为可读性」），代码一行不动。回应：两条路都可行，选塌缩的理由有两条——(a) 那两行注释之所以存在，是因为 MSRV 与写法脱节，注释只是补丁；抬级之后继续保留「因为曾经不能写」的说明，是把历史包袱留在现场，而本仓近期两轮清理的目标恰是删除这类无消费者的表示；(b) let-chains 是本仓唯一被 MSRV 反复绊倒的语法（三次 CI 红的现场），把它收复为默认写法等于消除该类事故的复现条件——保留嵌套只会让下一次 agent 或开发者写出 let-chain 时又被绊住。若评审选「只改注释」方案，本文（1）的验收项须整体替换（见「风险」）。

反方二：塌缩后两处的可读性未必更好，尤其 `src/update/mod.rs` 把「取锁值」与「比较冷却时间」压成一行。回应：let-chain 的语义正是顺序短路，与嵌套逐字等价；本仓已有同向的现代表达先例——`src/window.rs:151-155` 的 `watchdog_hwnd` 用 `load()?` 加 `then_some` 把三行压成两行。若 review 认为 `src/update/mod.rs` 那处不宜压行，允许只塌缩 `src/main.rs`，验收项相应减一。

反方三：`#[expect]` 项与 MSRV 无关，混进本文是范围蔓延。回应：已在「提案（2）」显式标注「可单独放弃」，且它覆盖全仓 `#[allow]` 的 100%（恰 2 处）；若评审要严格按 MSRV 划界，删掉这一项不影响（1）成立。

## 验收标准

`grep -rn 'let-chain' src/` 0 命中；`src/main.rs` 的 `if let Ok(h) = hwnd$` 与 `&& !h.is_invalid()$` 各 1 命中，`src/update/mod.rs` 的 `if let Some(t) = \*last$` 与 `&& t.elapsed().as_secs() < AUTO_CHECK_COOLDOWN_SECS$` 各 1 命中（rustfmt 将 let-chain 拆为多行，单行写法过不了 `cargo fmt -- --check`，验收按拆行后形态核对）。

（2）采纳时：`grep -n '#\[allow' src/util.rs src/tray.rs` 0 命中，且 `grep -n '#\[expect' src/util.rs src/tray.rs` 2 命中；不采纳时不设验收项。

`git diff --stat` 只含 `src/main.rs`、`src/update/mod.rs`（以及（2）采纳时的 `src/util.rs`、`src/tray.rs`），不得出现其它文件。

门禁全绿（全部带 `--locked`）：`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`（此项是（2）的裁决点）、`cargo fmt -- --check`；MSRV 侧 `cargo +1.89 check --all-targets --locked` 通过。

行为一致性点名现有用例仍绿：`src/main.rs` 的 `exit_request_gate_accepts_only_first_request`、`stale_main_hwnd_is_rejected`、`cli_args_parses_each_flag_and_ignores_unknown`；`src/update/protocol.rs` 的 `test_should_reset_update_progress_matrix`、`test_reset_update_progress_clears_global_flag`、`test_scan_exit_main_forwards_exactly_once`、`test_scan_duplicate_exit_main_forward_only_once`、`test_scan_done_does_not_forward`、`test_scan_invalid_lines_do_not_block_later_exit_main`、`test_scan_dead_target_still_signals_but_not_forwarded`；`src/suspend.rs` 的 `auto_update_poll_interval_must_be_far_below_cooldown`。如实说明一处覆盖缺口：`src/update/mod.rs` 的冷却早退分支本身没有直接用例（`start_auto_check` 依赖 WinHTTP，不可单测），它的等价性论证是「匹配与短路顺序逐字不变」加上上述 protocol 用例与整套 `cargo test` 全绿。

打包与运行（与 `01-bump-msrv-1-89-and-ci.md` 同口径）：`bun scripts/package.ts dev` 产出安装包不报错；运行 `target\release\traffic-monitor.exe` 确认小组件嵌入、数据刷新、托盘菜单与 `--quit` 正常；须用户实机确认，CI 绿不算。

## 风险

风险一：若评审选择「只改注释、保留嵌套」的替代方案，本文（1）的验收项失效，须改写为注释措辞检查；实施前先确认走哪条分支，不要两条各改一半。

风险二：前置依赖成立性——let-chain 在 1.88 才稳定，若 `01-bump-msrv-1-89-and-ci.md` 未先落地，本文代码编译不过（E0658）。顺序不可颠倒，也不建议与 01 合并成同一笔提交：01 是纯声明改动、本文是代码改动，混在一起会让「MSRV 抬级是否破坏构建」与「写法改动是否改变行为」两个问题纠缠，回退时无法只退一半。

风险三：（2）在 rustc 与 clippy 两个执行者下对工具 lint 的期望判定存在实现细节差异（`clippy::` 命名空间的期望由 clippy 判定），若出现「clippy 绿而 build 红」或反之，回退（2）即可，不影响（1）。

风险四：塌缩后 clippy 可能给出新提示（例如对 `src/main.rs:132` 的 `is_invalid` 判断风格），以 `cargo clippy --all-targets --locked -- -D warnings` 为裁决；若必须加豁免，应走 `#[expect]` 而不是 `#[allow]`（与（2）同理），并在同一笔提交里说明豁免对象。
