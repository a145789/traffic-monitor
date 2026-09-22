# Agent Note：panic=abort 语境下的异常路径防御收口

Status: proposed

## 问题

release 构建为 `panic = "abort"`（`Cargo.toml:12`），进程没有兜底 UI、没有 unwind、release 期也没有日志（见 `03-release-chain-and-observability.md`）：一次 panic 等于任务栏小组件无声消失，且现场不可复现。以下四处的共同点是——都落在 AGENTS 第 9 条靠注释与人肉纪律守护的本地状态机侧（而非更新链路那侧），修正成本都在一到三行之间。严重度如实分级：第 1 条触发条件近乎不可构造（防御性对齐，不是缺陷修复），第 2 条是真实的维护性地雷，第 3 条是编译期契约，第 4 条是注释固化。

1. **CPU 使用率存在无保护减法（防御性对齐）。** `src/collector/cpu_mem.rs:87` 为 `((total - idle_diff) * 100 / total).min(100) as u32`，而三个差分在 `src/collector/cpu_mem.rs:71-73` 各自 `saturating_sub`——彼此独立饱和，不保证 `idle_diff <= total`（其中 `total = kernel_diff + user_diff`）。若该组合成立：release 下 u64 减法回绕成天文数字、乘 100 再回绕，`.min(100)` 只截上界救不回来；debug 下直接 panic → abort。`.min(100)` 的存在说明作者已考虑「结果超界」，漏的只是下界。**如实说明严重度**：该组合近乎不可构造——三个计数器出自同一次 `GetSystemTimes` 的同一组内核快照，异常复位时三者同时回退、差分一并饱和为 0 并走 `total == 0` 早退（`src/collector/cpu_mem.rs:83-85`）；即便命中，基线每 tick 覆盖（`:75-79`），影响也只有一个 5 秒显示周期，不是长期错乱。
2. **`RefCell` 借用依赖微妙作用域规则（真实地雷）。** `src/collector/network.rs:262-263` 是 `if blacklist_needs_refresh(&cell.borrow(), now) { let mut cache = cell.borrow_mut(); ... }`：今天正确（if 条件表达式的临时借用先于块体 drop），任何「把条件提成 `let stale = ...`」的等价重构都会变成运行时双重借用，而在 `panic = "abort"` 下没有 unwind、直接终止进程。同仓 `src/renderer.rs:37-45` 已用 `try_borrow_mut` 把重入降级为「跳过」以避免同类 abort，两处谨慎程度不一致。
3. **一条常量关系未钉死。** `src/update/mod.rs:204` 是 `AUTO_CHECK_COOLDOWN_SECS - AUTO_CHECK_ERROR_COOLDOWN_SECS`（`src/config.rs:76-77`，当前 3600 与 300）。u64 下溢无编译期兜底：把错误冷却调得比正常冷却大，release 会回绕出约 584 年的 `Duration`，随后 `Instant - Duration` panic → abort。本仓已有把常量关系钉成测试的先例（`src/suspend.rs:405` 的 `auto_update_poll_interval_must_be_far_below_cooldown`），此处没有。
4. **`SetParent` 返回值歧义与同函数内的判别标准不一致（只做注释）。** `src/window.rs:223` 只做 `map_err`，而 `src/window.rs:225-234` 与 `:237-247` 对 `SetWindowLongPtrW` 精心做了 `SetLastError(0)` 加事后判别。Win32 对 `SetParent` 有同样的「返回 NULL 既是『前值』也是失败」歧义；本机 Win11 实测（审查期间以 P/Invoke 复现）该路径返回的是桌面句柄而非 NULL，故现状不误报。**本项不改判定逻辑**——把 `Err` 在 last error 为 0 时当成功继续，会把失败态变成「未 reparent 却继续后续序列」，比现状的安全侧重试更糟，理由见「明确不在本次范围」。

## 提案

四组独立提交，顺序即「先消灭会 abort 的算术与借用，再补契约与注释」（PR 怎么切由实施阶段判断）：

1. `src/collector/cpu_mem.rs:87` 改为 `total.saturating_sub(idle_diff) * 100 / total` 之后再 `.min(100)`；行上方补一句「两处饱和各自独立，故减法侧也必须饱和」。
2. `src/collector/network.rs:260-272` 把借用显式化：先 `let stale = { let cache = cell.borrow(); blacklist_needs_refresh(cache, now) };` 再在 `stale` 为真时 `borrow_mut`；或更彻底——`rebuild_virtual_blacklist`（`src/collector/network.rs:281` 起的纯函数）本就接受 `&mut`，可在同一个 `borrow_mut` 作用域内完成判定与重建，把两个锁域并成一个（推荐后者）。
3. `src/update/mod.rs` 内加 `const _: () = assert!(AUTO_CHECK_ERROR_COOLDOWN_SECS <= AUTO_CHECK_COOLDOWN_SECS);`，放在冷却常量使用处旁，把常量关系变成编译期契约。
4. `src/window.rs:223` 只补注释：写明该返回值歧义、同函数内 `SetWindowLongPtrW` 用的是 `SetLastError(0)` 加事后判别、以及本机实测返回桌面句柄的结论；**不加 `GetLastError` 分支**。
5. （文档）`src/renderer.rs:89-98` 的 `DisplayValues::load()` 依次读 4 个 Relaxed 原子、是跨 tick 拼接快照；`src/renderer.rs:55-64` 的去重比较会因此偶发漏画一帧（下次变化自愈）。在 `DisplayValues::load` 上方补一行「本值为跨 tick 拼接快照，禁止基于『快照一致』写更强的去重逻辑」——`src/state.rs` 有成文内存序约定，读侧语义也应有一句。

## 明确不在本次范围

- 不改 `panic = "abort"` 本身：它的前提（无兜底 UI、无 unwind、进程死亡即功能消失）正是本笔记成立的理由，改它会牵动体积与既有失败语义，属独立决策。
- `src/renderer.rs:37-45` 的 `with_renderer` 静默跳过保持不动——它是本笔记要推广的既有正例，改成 panic 或返回 `Result` 会波及全部调用点；它「吞掉调用方意图」的可诊断性问题在 `03-release-chain-and-observability.md` 的日志开关里解决。
- `src/suspend.rs:318` 的兜底 `return true`、速率显示粒度、`src/window.rs:326-364` 的 `LAST_RECT` 冗余等项归 `04-constants-reuse-and-doc-alignment.md`（注意：前者**不可删除**，理由见该笔记）。
- `src/update/mod.rs:606` 的 `MUTEX_ALL_ACCESS`（另一处「`Err` 即判定」的同类写法）不在这里：它属更新交接链路，见 03 笔记。
- **子进程协议不改字节级读取**（原报告的 A6 不采纳）：子进程就是自身同一个 exe，协议行是固定短 ASCII，非 UTF-8 或超长行不会出现，收益不可构造；成本却被低估——要同时改 `scan_subprocess_protocol` 与 `parse_update_action`（`src/update/mod.rs:779-785` 现为 `str::from_utf8(...).ok()?.trim()`），且 `read_failed` 是生产消费者（`src/update/mod.rs:710` 的 `is_error: read_failed || parsed_action.is_none() || !exit_status.success()` 参与错误提示），字节化会把「非 UTF-8 行」从「读失败即报错」变成「跳过」，改变用户可见行为——不是「净简化」。该接缝还受 AGENTS 第 4 条保护。
- **`SetParent` 不引入 `GetLastError` 分支**：Win32 不保证所有失败都 set last error；若某版本真的失败而 last error 恰为 0，忽略 `Err` 会让后续 `SetWindowLongPtrW`/`SetWindowPos`/`SetLayeredWindowAttributes` 作用在未 reparent 的窗口上，得到一个位置错乱的一等公民窗口，比现状「静默重试到成功」更糟。现状的假阳性只会多一次重试，落在安全侧。

## 为什么不保留？

最强反方有三条。其一，「加 `saturating_` 会掩盖真实异常，把崩溃换成静默错值」——在 `panic = "abort"` 且 release 无日志的前提下两者代价并不对等：崩溃让组件消失且现场不可复现，错值最多在一个刷新周期内偏低（`total == 0` 时现有代码本就选择「保持上次值」）；而 `.min(100)` 已表明作者接受了「结果钳制」策略，本项只是补齐下界。其二，「这几项收益都是纸面上的，纯属给不会发生的情况加保险」——承认：第 1 条确属防御性对齐、收益接近零，第 4 条只是注释；但第 2 条不同，它消除的是一个「后人顺手重构即进程级 abort」的活雷，第 3 条把一条静默依赖变成编译期契约，两者都是真实的维护性收益。其三，「`network.rs` 的借用今天是对的，动了反而有机会引入 bug」——正因为它对，改动才是纯机械的（把不可变借用显式限定到一个 `let`），而它一旦被后人顺手重构就是进程级后果。

## 验收标准

- `grep -n 'saturating_sub(idle_diff)' src/collector/cpu_mem.rs` 有命中（本笔记**不含**协议读取侧改动，`src/update/mod.rs` 的 `read_line` 保持原样）。
- `grep -n 'const _: () = assert' src/update/mod.rs` 有命中；把 `src/config.rs:77` 临时改为 `4000` 后 `cargo build --release --locked` 必须失败（证伪依据），还原后通过。
- `grep -n 'borrow()' src/collector/network.rs` 的命中不再出现在 `if` 条件内。
- `cargo test --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt` 全绿（本次不触碰 `src/update/mod.rs` 的 9 条 `test_scan_*`，它们应原样通过）。

## 风险

- 第 1 项修法使 `idle_diff > total` 时写入 0%（瞬时归零），与 `src/collector/cpu_mem.rs:36` 声明的「失败保持上次值」不完全一致；若要一致，应在该条件下提前 `return` 不写原子量——二选一并同步注释，不要两处口径不一。
- 第 2 项若选「合并为单一锁域」，`build_virtual_blacklist`（内部调 `GetAdaptersAddresses`）的运行区间从「不可变借用持有中」变为「可变借用持有中」，需在 PR 描述里贴出「该闭包不访问 `CURRENT_DATA` / `INTERFACE_HISTORY`」的调用图检查；重入风险与现状等价（两条路径都会 panic），无新增面。
- 第 4 项（注释项）无法在 CI 回归：它依赖 OS 返回值行为。注释必须写成「已评估、采取现状」的结论，避免后人误以为这里漏了防御。

