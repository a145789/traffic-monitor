# Agent Note：把缺失定时器集合与恢复武装状态降级为具名结构

Status: implemented

## 问题

三处同一形状的过度表示，都在暂停/恢复链路上，都是「集合或状态被编码得比它实际承载的事实更宽」。

**其一，`MissingTimers` 是服务三个固定定时器的手写位掩码。**（`src/suspend.rs:321`-`src/suspend.rs:369`）「哪些定时器应当存在」的唯一真值源是 `timer_plan`（`src/suspend.rs:281`-`src/suspend.rs:310`），本类型只承载「上一轮先杀后建里哪几个没建起来」这一瞬态事实，而它服务的集合永远是全屏检测、网络、CPU/内存这三个。`bit()`（`src/suspend.rs:332`-`src/suspend.rs:339`）把 `TIMER_ID_*` 映射到位，但 `_ => None` 分支不可达：唯一生产者 `from_results`（`src/suspend.rs:341`-`src/suspend.rs:353`）只喂这三个 ID，另一个写点 `with` 也只收到 `missing_timer_plan` 产出的缺失清单里的三个 ID（`src/suspend.rs:445`-`src/suspend.rs:449`）。于是 `has`/`with` 的 `Option` 包装整条是死路径，「未知 ID 静默忽略」这个语义没有任何可达输入。生产消费者：`register_missing_timers`（`src/suspend.rs:384`-`src/suspend.rs:392`）、`missing_timer_plan`（`src/suspend.rs:415`-`src/suspend.rs:429`）、`retry_missing_timers`（`src/suspend.rs:438`-`src/suspend.rs:453`）、`sync_monitoring_timers` 的返回值（`src/suspend.rs:505`）、启动尾段的布尔判定（`src/main.rs:588` 只取 `.is_empty()`）。非生产消费者：`src/suspend.rs:726`-`src/suspend.rs:755` 的用例、`src/smoke.rs:214`-`src/smoke.rs:233` 的冒烟用例（opt-in，不进必跑门禁）。

**其二，该类型的 Debug 输出把现场日志变成了乱码。** `register_missing_timers` 的 release 日志打的是 `{missing:?}`（`src/suspend.rs:389`），而 `MissingTimers(u32)` 派生出的 Debug 是 `MissingTimers(2)` 这种裸位掩码——维护者拿这条日志无法判断缺的是网络还是 CPU/内存定时器。这是本条的实际收益之一，不只是省行数。

**其三，`RECOVERY_ARM` 里的失败计数没有读者。**（`src/recovery.rs:43`-`src/recovery.rs:46`）`Option<u32>` 的计数值只被读一次：`src/recovery.rs:125` 取出后仅用于 `failures == 0` 判「本次是否失败序列的首条日志」（`src/recovery.rs:132`-`src/recovery.rs:135`），`saturating_add` 之后的数值再无任何消费方；`ensure_recovery_timer`（`src/recovery.rs:157`-`src/recovery.rs:164`）只判 `is_none()`。真正被使用的只有「已武装 / 未武装且已记过日志 / 未武装且未记过日志」三态。

**其四，`RECOVERY_INTERVAL_MS` 的注释与代码不符。**（`src/recovery.rs:40`-`src/recovery.rs:42`）注释写「0 表示尚未武装」，但该变量的唯一读者是 `recovery_tick`（`src/recovery.rs:186`-`src/recovery.rs:190`），那里 `.max(TIMER_INTERVAL_RECOVERY)` 把 0 直接吸收掉，没有任何代码把 0 当武装判据——武装判据在 `RECOVERY_ARM`。这条注释会让后来者去找一条不存在的耦合，或误以为改 `RECOVERY_INTERVAL_MS` 会动到武装语义。

## 提案

1. `MissingTimers` 换成三个具名字段的私有 struct（`fullscreen` / `network` / `cpu_mem: bool`），保留 `const EMPTY` 与 `is_empty()`；`missing_timer_plan` 直接读字段，删掉 `bit()` 与 ID→位映射；`register_missing_timers` 的日志由具名分量的 Debug 列出（`MissingTimers { fullscreen: true, network: false, cpu_mem: false }`），不再是 `MissingTimers(2)` 这种裸位掩码。对外 API 形状不变：`resync_monitoring_timers` 仍返回该类型、`retry_missing_timers` 仍返回 `bool`、`main.rs:588` 仍用 `.is_empty()`。
2. `RECOVERY_ARM` 换成三态 enum（`Armed` / `DisarmedUnlogged` / `DisarmedLogged`），删掉 `saturating_add` 与 `unwrap_or(0)`；`set_recovery_interval` 的日志去重改为读该状态经纯函数 `arm_attempt_outcome` 得出的「本次是否首条」。
3. 删掉 `RECOVERY_INTERVAL_MS` 注释里「0 表示尚未武装」半句（该变量的唯一读者是 `recovery_tick` 的退避计算，武装判据在 `RECOVERY_ARM`）。
4. 净值实测（实施后核对）：非测试代码 +107/−70（净 +37），测试 +66/−8（净 +58），源码合计 +173/−78（净 +95）。原稿估计的「约 35-40 行（减少）」不成立，省下的位掩码与计数算术被两处新增抵消：`CoreTimer`（把「定时器 ID ↔ 缺失分量」这层一一对应显式化，否则写回方向只能退回 `usize` 的 `_ =>` 死分支），以及把「持续失败只记首条日志」从真实 `SetTimer` 失败路径解耦出来的纯函数用例。本条收益在判据面（删掉 `Option` 包装、`_ => None` 与失败计数三个无消费者/不可达判据）与现场日志可读性，不在行数。

## 明确不在本次范围

- **不动 `CapabilityRearm`**（`src/recovery.rs:238`-`src/recovery.rs:249`）。生产侧确实只消费 `all_ok()`，但那两个 bool 是测试可观测面而不是重复表示：合并成单 bool 后，从恢复表里删掉「托盘」或「会话通知」任一项，都会被另一项的成功掩盖，`src/recovery.rs:416`-`src/recovery.rs:440` 的断言就钉不住表里到底有哪几项。十二行换「删一项就变红」，是正收益。
- 不动 `TimerPlan` 与 `timer_plan`（`src/suspend.rs:272`-`src/suspend.rs:310`）：它是计划集合的唯一真值源，四个字段各自承重（网络间隔是 `Option<u32>` 而不是 bool，因为间隔随退避变化）。本提案只动「缺失集合」这一瞬态事实的表示。
- 不动 `retry_missing_timers` 的「绝不 `KillTimer`」语义（`src/suspend.rs:431`-`src/suspend.rs:453`）：那是恢复贪婪重同步会饿死采样的直接对策。
- 不改 `RECOVERY_ARM` 的消费点拓扑（三处补武装入口见 `src/recovery.rs:144`-`src/recovery.rs:164`）：本提案只换表示，不换谁在什么时候武装。

## 为什么不保留？

- **反方一：位掩码是为将来加定时器预留的扩展点。** 回应：真加一个定时器时，`timer_plan`、`missing_timer_plan`、`set_coalescable_timer` 的调用处、`TIMER_ID_*` 常量都要一起改，位掩码只是其中最小的一块；三元 bool 改四元 bool 的成本与当年加位映射持平。若确实要保留 struct，最小让步是删掉 `Option` 包装与 `_ => None`，让 `has`/`with` 返回 `Self`/`bool`。
- **反方二：失败计数将来可能用于指数退避。** 回应：当前恢复调度的退避由 `recovery_tick` 的间隔翻倍承担（`src/recovery.rs:181`-`src/recovery.rs:193`），与失败计数无关。为一个没有归属者的想象需求保留计数值，是把维护成本留在现在。
- **反方三：改日志格式会破坏可解析性。** 回应：该行是 release 日志文本，全仓没有任何消费者（`grep -rn "监测定时器待补建集合" src` 只命中产生处）。
- **反方四：三态 enum 比 `Option<u32>` 更啰嗦。** 回应：啰嗦换来的是「日志只记一次」这条纪律从数值里显式化。真正要避免的是用 `Option<bool>` 的 `true`/`false` 去猜语义——若采用 `Option<bool>`，变体命名与注释必须写死每一侧的含义。

## 验收标准

- `grep -n "MissingTimers::bit\|=> None" src/suspend.rs` 在本类型范围内零命中。
- `grep -n "saturating_add" src/recovery.rs` 不再出现在武装路径上。
- `grep -n "0 表示尚未武装" src` 零命中。
- 测试 `test_missing_timer_plan_only_covers_wanted_and_missing`（`src/suspend.rs:726`-`src/suspend.rs:755`）改写后四条断言的全部语义不变：空集 ⇒ 空清单；挂起态不补建；正常态只补缺失项且网络间隔随退避取值；全屏态只补全屏检测定时器。
- `MissingTimers` 分量与判空由 `test_missing_timers_components_round_trip` 钉住：`mark` 只点亮对应分量，任一分量缺失都必须判为非空。
- `arm_attempt_outcome` 的纯用例钉住「每条失败序列只在首条留痕」：从 `Armed` 或 `DisarmedUnlogged` 失败要留痕、`DisarmedLogged` 之后静默、成功即回到 `Armed` 并重置该档。
- 冒烟用例 `src/smoke.rs:214`-`src/smoke.rs:233` 仍绿（它钉的是 `retry_missing_timers` 的返回语义而非表示）。
- 四道门禁全绿：`cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`。

## 风险

- 现场日志文本会与历史日志不同构。排查旧版本时需知道这一点；不影响任何自动化消费方。
- 若用三态 enum 而变体名不精确（例如把「未记过日志」编码进 `Armed`），「持续失败只记首条日志」这条纪律会重新埋回布尔里，且现有测试覆盖不到（该路径依赖真实 `SetTimer` 失败）。落地时用显式变体名，不要靠 `Option<bool>` 的字面值猜。本次落地用三态具名变体，并把该纪律收进 `arm_attempt_outcome` 的纯用例，这条风险已收敛。
- 删掉 `RECOVERY_INTERVAL_MS` 的死注释后，若将来真要做「0 = 未武装」判定，必须先改 `recovery_tick` 的 `.max()` 吸收逻辑，否则会造成反向误导。本提案只删死语义，不新增耦合。
- `MissingTimers` 换成具名 struct 后，`{missing:?}` 的 Debug 形状改变；若有人曾按 `MissingTimers(` 前缀 grep 日志，需要同步更新排查手册（本仓无此类手册，此风险仅提醒）。
