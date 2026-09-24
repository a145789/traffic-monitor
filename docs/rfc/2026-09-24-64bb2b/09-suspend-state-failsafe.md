# Agent Note：给挂起位补按原因自证的自愈（不设统一超时、不无条件清零）

Status: proposed

**前置依赖：04 篇的看门狗恢复调度器**（本 note 的检查必须挂在一个「挂起时仍存在」的 tick 上）。04 未合时本 note 不可实施。

## 问题

**其一，挂起态没有任何自愈入口。** `src/suspend.rs:133-141` 在挂起时把 `TimerPlan` 全部置空，包含 `TIMER_ID_FULLSCREEN`；而那个 tick 正是本仓库唯一的周期自愈入口（`src/main.rs:642-653`：`reembed_if_lost` + `check_fullscreen`，AGENTS.md 第 5 条指定）。清位的唯一途径是 `resume_system`（`src/suspend.rs:56-69`），它只被 `src/suspend.rs:88`（唤醒）与 `src/suspend.rs:117`（解锁）调用，没有任何兜底。

**其二，能置位的位一旦漏收 resume 通知就永久卡住。** `SUSPEND_REASON_MONITOR`（`GUID_MONITOR_POWER_ON`，`src/main.rs:339-343`→`src/suspend.rs:95-101`）与 `SUSPEND_REASON_SESSION`（`WTSRegisterSessionNotification`，`src/main.rs:358`→`src/suspend.rs:113-118`）都是**定向**通知、能到达子窗口，所以它们会真的置位；一旦对应的 resume 通知（显示器重开/解锁）丢失，位就永久留着，定时器永久不再创建，组件表现为「假活」——数字停在旧值，用户只能重启程序。

**其三，挂起起始时刻这个状态没有任何载体，且不可能是原子。** 旧稿提议「新增一个原子」记录挂起起始时刻，但 Rust 没有 `AtomicInstant`，`Instant` 也不能按内部布局塞进 `AtomicU64`。好在：挂起位集与它的全部处理器都跑在 UI 消息循环线程上（`src/main.rs:540-541`、`src/main.rs:601-602` 明确「看门狗与主窗口同属 UI 消息循环线程」），所以这个状态**不需要跨线程原子**，用 UI 线程专属的 `Cell` 即可。

后果分级：三原因里 `SYSTEM` 可以零 API 自证（见提案），`SESSION` 有文档化只读探针，`MONITOR` 没有可靠只读路径——三者的处理强度必须不同，这是本 note 与旧稿「统一 6–12 小时超时后清空全部位」的核心区别。

生产消费者：`SUSPEND_REASONS` 位集（`src/state.rs:46`）、`resume_system`（`src/suspend.rs:56-69`）的边沿语义、各 tick 的 `is_suspended()` 门控（`src/main.rs:643/656/670/679`）。非生产消费者：`src/suspend.rs:382-431` 的 `timer_plan` 测试与 `src/state.rs:94-146` 的位集测试只覆盖纯逻辑，钉不住「位是否与真实状态一致」。

## 提案

1. **承接 tick 用 04 篇在看门狗上建立的恢复调度器**（`SetTimer(watchdog, TIMER_ID_RECOVERY, ...)`，与既有 `TIMER_ID_REBUILD_RETRY` 同范式：`src/config.rs:60`、`src/main.rs:501-521`、`src/main.rs:609-612`）。看门狗永不参与挂起、也永不重建，所以它在挂起态仍然存在——这解决了「唯一自愈 tick 被一起杀掉」的死结，且不需要放宽 `timer_plan` 的挂起分支。
2. **按原因判定「位是否仍然真实」，判定结果只用于决定是否调用既有的 `resume_system(hwnd, reason)`**，不新增状态机、不改位协议：
   - `SUSPEND_REASON_SYSTEM`：**自证**。本进程正在执行 tick，就说明系统正在运行，休眠位必为陈旧（挂起的机器不会跑定时器）⇒ 直接 `resume_system(hwnd, SUSPEND_REASON_SYSTEM)`。零 API、零超时、零误判。
   - `SUSPEND_REASON_SESSION`：**探针**。`WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTSSessionInfoEx)` 读 `WTSINFOEX_LEVEL1_W.SessionFlags`，为 UNLOCK 才清位（见 MS 文档 `WTSINFOEX_LEVEL1_A` / `WTS_SESSIONSTATE_LOCK`；注意 Win7/2008R2 的 `WTS_SESSIONSTATE_LOCK`/`UNLOCK` 语义反转缺陷，本仓库目标平台不受影响）。`Win32_System_RemoteDesktop` feature 已启用（`Cargo.toml:42`），但 `windows` 0.62 是否导出 `WTSSessionInfoEx` / `WTSINFOEX_LEVEL1_W` 需实施时确认；若符号缺失，本项降级为「按位超长 TTL」。
   - `SUSPEND_REASON_MONITOR`：**没有可靠的只读真值源**。本仓库 feature 集内没有一个「读当前显示器开关状态」的文档化 API（`GetSystemPowerStatus` 只报 AC/电池；`GUID_CONSOLE_DISPLAY_STATE` 只在变化时推送）。因此取**保守超长按位 TTL**：建议 12 小时，明确长于「整夜息屏/锁屏」这一最常见的合法长挂起场景；到期只清 `MONITOR` 这一位，并立即重新注册订阅（保证下一次真实通知不会被漏收），同时 `log_event!` 留痕。
3. **三原因一律走既有 `resume_system`，不自己清位**：位协议（`src/state.rs:26-46`）与「只有清掉最后一位的那次才重建基线」的边沿语义（`src/suspend.rs:56-69` 的 `should_rebuild_baseline`）已经实现，本 note 只是给它补一个真值源。绝不允许出现「直接 `SUSPEND_REASONS.resume()` + 自己重建基线」的第二套实现。
4. **时间戳载体**（仅 `MONITOR` 的 TTL 用到）：`thread_local! { static SUSPEND_SINCE: std::cell::Cell<Option<Instant>> }`，与 `src/window.rs:337` 的 `LAST_RECT`、`src/main.rs:80` 的 `REBUILD_RETRY_INTERVAL_MS` 同范式；`SYSTEM`/`SESSION` 两条路径不需要时间戳。不引入原子、不引入 `Mutex`。
5. **Modern Standby (S0) 如实交代**：S0 下 `PBT_APMSUSPEND` 根本不发（02 篇已述），本 note 不假装覆盖；S0 的省电覆盖由 02 篇的 `GUID_CONSOLE_DISPLAY_STATE` 订阅承担（息屏即置 `MONITOR` 位，点亮即清）。

## 明确不在本次范围

- **不无条件清空 `SUSPEND_REASONS`**（旧稿的错误做法）：8 小时的合法锁屏或夜间息屏会被误判成「丢通知」，随后在仍锁屏时重启网络/CPU/更新定时器，把省电语义反向破坏掉。
- **不把挂起分支改成「留一个监测 tick」**：挂起态定时器集合保持全空（`test_timer_plan_suspended_has_no_timers` 保持不变），自愈 tick 住在看门狗上，AGENTS.md 第 7 条的「销毁与恢复对称」因此不被触碰。
- **不做统一超时**：三个原因的真值源强度不同，用同一个数字表达是把 `SYSTEM` 的零成本自证降级成猜测。
- **不新增 `SUSPEND_REASON_*` 位、不改位集语义**（`src/state.rs:94-146` 的四个用例全部保持不变）。
- **不把电源/会话处理器搬到看门狗**（理由与正确退路见 02 篇）。

## 为什么不保留？

1. 「有 tick 在跑就说明没挂起，那 `SYSTEM` 位根本不用清」——不对：位卡住会让 `is_suspended()` 恒真，而 tick 门控（`src/main.rs:643/656/670/679`）读的正是这个位，所以「有 tick 在跑」这个事实必须被**主动转写**成一次清位动作，否则位永远没人动它。
2. 「超时上限是拍脑袋，越短越好」——越短越会把合法长挂起误恢复。`MONITOR` 的 12 小时不是最优值而是**明确偏保守**的选择：误恢复的代价是显示关闭期间多跑 1 Hz 采样，且下一次显示器点亮通知必然到达（可自愈）；误冻结的代价是组件永久假活、必须重启程序。两者不对称，所以宁可容忍窗口内的省电损失。
3. 「直接清位比重入 `resume_system` 简单」——那会绕过 `should_rebuild_baseline` 与 `reset_network_backoff`（`src/suspend.rs:59-64`），产生「位清了但基线没重建」的静默错误，正是 AGENTS.md 第 10 条禁止的「同一事实多处可写」。
4. 「用一个 `AtomicU64` 存毫秒数就行」——可以绕过 `Instant` 的不可原子性，但那是把一个 UI 线程私有的状态提升成跨线程可见的共享状态，换来的只是「写起来像原子」；本仓库已有 `Cell` 范式，没有必要。

## 验收标准

- `grep -rn "WTSSessionInfoEx\|WTSINFOEX" src/` 命中 ≥ 1 处（若因符号缺失降级，则该裁决与原因写进实施说明，不允许静默跳过）。
- `grep -rn "SUSPEND_SINCE" src/` 命中 ≥ 2 处，且**只在** `MONITOR` 的 TTL 路径上被读写；`grep -rn "AtomicInstant" src/` 命中 0 处。
- 判定逻辑抽成纯函数可单测（与 `should_rebuild_baseline` 同风格，放 `src/suspend.rs` 的 `#[cfg(test)]`）：`should_resume_for_reason(reason, probe) -> bool`。用例至少覆盖：`SYSTEM` 在 tick 上下文中恒为真；`SESSION` 在探针报 UNLOCK 时为真、报 LOCK 时为假；`MONITOR` 未到 TTL 为假、超 TTL 为真。
- 故障注入（本 note 的关键实机验收）：临时注释掉 `src/suspend.rs:116-118` 的 `WTS_SESSION_UNLOCK` 分支构建一个「漏收解锁」的 exe → 锁屏 30 秒后解锁 → 断言下一个恢复周期（≤ 恢复 tick 间隔）内网络数值恢复更新，而不是永久停在旧值；验证完恢复代码。
- 恢复路径必须复用既有实现：`grep -rn "resume_system" src/` 的调用点从 2 处（`src/suspend.rs:88`、`:117`）增加到 ≥ 4 处，且新增调用点不出现裸的 `SUSPEND_REASONS.resume(`。
- 既有纯逻辑用例**全部保持不变**并通过：`test_timer_plan_suspended_has_no_timers`（`src/suspend.rs:394-404`）、`src/state.rs:94-148` 的四个位集用例。
- `cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt -- --check` 全绿。

## 风险

- 看门狗上的恢复 tick 在挂起态仍会周期性唤醒（建议 60 秒一次）：这是「挂起时不空转」的让步，量级相对 1 Hz 网络采样可忽略，但它是本 note 必须写进注释的实测事实，不能靠「大约没影响」。
- `MONITOR` 位在 12 小时窗口内仍可能冻结；且一旦探针不可用（`SESSION` 降级路径），窗口同样存在。这两个窗口是**已知残留**，不是「已验证不存在」。
- `WTSQuerySessionInformationW` 的锁屏探针在「快速用户切换」「RDP 断开但会话保留」等形态下的返回值语义需要实测确认（`WTSSessionInfoEx` 报的是会话锁状态，不是连接状态）；若实测发现 RDP 断连时误报 UNLOCK，则 `SESSION` 也必须退回 TTL，本 note 的探针部分应重新评审。
- 若 04 篇的恢复调度器最终没有落在看门狗上（例如实施时改挂主窗口），本 note 立刻失去承接 tick，会退化成旧稿那个「最坏情形恰好吃不上」的方案——所以两篇的先后顺序不能颠倒，实施时不得各自为政。
