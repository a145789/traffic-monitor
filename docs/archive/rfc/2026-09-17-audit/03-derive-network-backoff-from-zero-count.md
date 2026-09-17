# Agent Note：以零速计数派生退避状态，删除 NETWORK_BACKOFF 双份表示
Status: proposed

## 问题

同一事实有两份表示：`NETWORK_BACKOFF: AtomicBool`（`src/state.rs:65`）与 `CONSECUTIVE_ZERO_COUNT: AtomicU32`（`src/state.rs:68`）在可观察点上恒满足 `BACKOFF == (count >= BACKOFF_ZERO_THRESHOLD)`，后者可由前者完全派生。证据（`grep -rn "NETWORK_BACKOFF\|CONSECUTIVE_ZERO_COUNT\|reset_network_backoff" src` 共 23 命中，逐条归类；检索须区分大小写，`timer_plan` 的形参 `network_backoff`（`src/suspend.rs:112,133`）不计入）：唯一写路径是 `collect_network`（`src/collector/network.rs:107-119`），零接口 tick 做 `fetch_add` 自增，`count >= BACKOFF_ZERO_THRESHOLD` 且 bool 为假时置 bool并发 `DISCONNECTED`（`src/collector/network.rs:108-112`），恢复 tick 清零且 bool 为真时清 bool并发 `RECONNECTED`（`src/collector/network.rs:114-118`）；其余写点只有 `reset_network_backoff` 的双清（`src/state.rs:77-80`），调用方仅两处——`resume_system`（`src/suspend.rs:55`）与 `RECONNECTED` 处理器（`src/main.rs:463-468`，注意该处理器在 `collect_network` 已清过一次的基础上再清一次，今日即是双写）；写路径之外唯一的跨函数读点是 `sync_monitoring_timers` 经 `timer_plan` 选定时器间隔（`src/suspend.rs:149-153,131-137`）；`timer_plan` 的四个单测（`src/suspend.rs:348-385`）用 `bool` 参数传入，从不触及该静态量，故没有测试钉死这个 `static` 本身；`update/` 与 `tray.rs` 对两符号零命中，`collect_network` 的唯一生产调用是 UI 线程 `TIMER_ID_NETWORK` tick（`src/main.rs:412-424`），`sync`/`reset` 的全部调用方（`src/main.rs:191,371` 经 `bind_display_and_timers` 落到 `src/main.rs:315`；`src/main.rs:459,464,465` 为断网/恢复消息处理器；`src/suspend.rs:46,55,210,255`，其中 210/255 在 `check_fullscreen` 内）同样在 UI 线程上下文，因此 `NETWORK_BACKOFF` 上的 `Acquire/Release` 握手没有跨线程对象（`src/state.rs:64` 的握手注释属保守表述），`fetch_add` 与置位之间的瞬态在单线程内无观察点，不变量成立。

## 提案

删除 `NETWORK_BACKOFF` 静态量及其文档（`src/state.rs:64-65`），`reset_network_backoff` 只保留计数清零并同步更新其归属注释（`src/state.rs:70-80`，今日“两个 store”的表述改为单个清零说明）；`sync_monitoring_timers` 改读派生谓词 `CONSECUTIVE_ZERO_COUNT.load(Relaxed) >= BACKOFF_ZERO_THRESHOLD`（`src/suspend.rs:152` 一行，import 从 `NETWORK_BACKOFF` 换为 `CONSECUTIVE_ZERO_COUNT` 加 `config::BACKOFF_ZERO_THRESHOLD`，此即 `src/state.rs` 模块头约定的“同一线程定时器读写用 Relaxed”）；`collect_network` 进入路径收敛为边沿 `count == BACKOFF_ZERO_THRESHOLD` 发 `DISCONNECTED`（省掉 `!BACKOFF.load` 查询与 `BACKOFF.store(true)` 两行），恢复路径用 `swap(0)` 取旧值、`旧值 >= BACKOFF_ZERO_THRESHOLD` 则发 `RECONNECTED`（发完计数已归零，后续 tick 不再满足条件，仍是边沿触发）；`timer_plan(bool,bool,bool)` 签名与全部定时器挂载逻辑不动；逐 tick 行为等价：持续断网时计数单调增、`== TH` 只成立一次（与今日“bool 防重发”同效，含 `DISCONNECTED` 处理前又来 tick 的 `PostMessageW` 异步窗口）；恢复时处理器侧 `reset` 保持幂等清零；净删约 7 行（state 减静态量加文档收缩约 5 行，network 收敛约 2 行，suspend 改一行基本持平）。

## 明确不在本次范围

`BACKOFF_ZERO_THRESHOLD`、正常与退避两档采样间隔常量、`timer_plan` 真值表及其四例单测不动（退避策略本身是产品行为）；挂起/恢复对称性与定时器挂在 `TIMER_ID_FULLSCREEN` 做周期兜底（受保护接缝，`src/main.rs:399-411`）不动；单网卡锁定与逐 LUID 差分（受保护接缝，`src/collector/network.rs:76-102`）不动；`CONSECUTIVE_ZERO_COUNT` 仅在“零速率且无接口”（`src/collector/network.rs:107` 的 `current_data.is_empty()` 条件）时自增的语义不动——有接口但空闲的 tick 走恢复分支清零是既有行为，本提案不评判该条件是否过严；`reset_network_backoff` 作为中立归属点的存在本身不动，只删其内部对 bool 的 store。

## 为什么不保留？

反方一：bool 是“已通知”闩锁，删后 `RECONNECTED` 会从边沿触发退化为电平触发、每 15s 重发导致定时器反复重建。回应：派生方案同样是边沿触发——恢复分支以 `swap` 返回的旧值是否 `>= TH` 决定是否发消息，发完计数已归零，后续 tick 不再满足条件；进入侧 `== TH` 在计数单调增时恰成立一次，与今日语义逐 tick 等价，且消除了今日“`network.rs:114-116` 与 `main.rs:464` 对同一事实双写”的窗口。反方二：`src/state.rs:64` 明确写了 `Acquire/Release（与定时器重建握手）`，改 Relaxed 是内存序降级。回应：上文调用链枚举证明读写双方全在 UI 线程（`collect` 唯一调用 `src/main.rs:416`，`sync`/`reset` 调用方全为窗口过程、全屏检测 tick 与挂起恢复路径），符合模块头“同一线程定时器读写用 Relaxed”的既有约定（`NET_SPEED_*`、`CPU_USAGE` 皆如此），降级之名不成立。反方三：省约 7 行却要动原子量与消息边沿，心智负担不合算。回应：删的是一个跨三文件的全局可变静态量（含其文档与双写协调），不是 7 行普通代码；且 `timer_plan` 的纯函数签名与测试桩原样保留，定时器决策的可测试性不受损。

## 验收标准

`grep -rnw "NETWORK_BACKOFF" src` 零命中（必须用词边界口径：常量 `TIMER_INTERVAL_NETWORK_BACKOFF` 含同名子串，`src/config.rs:48`、`src/suspend.rs:22,134,374` 共 4 处子串命中不在断言范围；子串口径当前 14 处，实施后仍剩这 4 处，按子串断言必然假红）；`grep -rn "CONSECUTIVE_ZERO_COUNT" src` 仅剩 `src/state.rs` 定义加清零、`src/collector/network.rs` 自增加边沿判定、`src/suspend.rs` 派生谓词读取；`cargo test --locked` 全绿，点名 `timer_plan` 四例（`test_timer_plan_suspended_has_no_timers`、`test_timer_plan_fullscreen_only_keeps_detection_timer`、`test_timer_plan_normal_backoff_uses_slow_network_interval`、`test_timer_plan_normal_online_uses_regular_network_interval`）与 `state.rs` 位协议五例行为不变；手动验证：断网持续超过阈值后采样切到 15s 间隔且只切一次，恢复后回到 1s 间隔且 `start_auto_check`（`src/main.rs:466`）恰触发一次；`sync_monitoring_timers` 返回值语义（核心定时器 fail-fast）不变。

## 风险

真实残留风险只有一项：上述等价性依赖“读写全在同一 UI 线程”这一前提，若未来有人从工作线程调用 `collect_network` 或直接读写计数，Relaxed 派生读会出现竞态；缓解是今日调用点唯一（`src/main.rs:412-424` 的 UI tick），提案要求在 `collect_network` 文档上加“仅由 UI 线程调用（与 `bind_display_and_timers` 同约束）”一行断言性注释，把前提写死为后来人的显式卡点。`PostMessageW` 异步投递的乱序极端情况（先发的 `DISCONNECTED` 后于后采样的恢复到达）与旧方案完全同等，旧方案同样只依赖消息顺序而无序列号，不属本次新增风险。
