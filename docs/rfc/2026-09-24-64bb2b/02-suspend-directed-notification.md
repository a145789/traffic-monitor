# Agent Note：把休眠与显示器状态通知改成定向订阅（让挂起位有真实生产者）

Status: proposed

## 问题

**休眠位没有可达的生产者。** 休眠/唤醒走 `src/suspend.rs:84-89` 的 `PBT_APMSUSPEND` / `PBT_APMRESUMEAUTOMATIC`，落在主窗口过程 `src/main.rs:748` 上；而主窗口嵌入任务栏后是跨进程 `WS_CHILD`（`src/window.rs:229` 的 `SetParent`、`src/window.rs:236` 覆盖 `GWL_STYLE` 为 `WS_CHILD | WS_VISIBLE`），APM 事件是顶层广播，不投递子窗口。这条规则本仓库自己在两处依赖：`src/main.rs:557-559`（主题广播「SetParent 之后它是 WS_CHILD，收不到 HWND_BROADCAST 顶层广播」）与 `src/main.rs:614-616`。而 `src/main.rs:746-747` 的注释用「电源设置通知是定向消息……嵌入为子窗口后仍直达」为由把整个 `WM_POWERBROADCAST` 分支留在主窗口——这句话对 `PBT_POWERSETTINGCHANGE`（`RegisterPowerSettingNotification` 定向到注册 HWND，`src/main.rs:338-344`）成立，对同一 match 臂里的 APM 事件不成立，两类通知被混同了。

**显示器订阅用的是退化形态。** `src/main.rs:339-343` 只订阅 `GUID_MONITOR_POWER_ON`，这是被 `GUID_CONSOLE_DISPLAY_STATE` 取代的 legacy 订阅点。在 Modern Standby（S0 低功耗空闲，今天笔记本的默认形态）下 `PBT_APMSUSPEND` **根本不发**，而显示器息屏正是 S0 最典型的省电场景——只订阅 legacy 显示器事件意味着 S0 息屏时省电语义完全没被触发。

检索证据：`grep -rn -e RegisterSuspendResumeNotification -e PowerRegisterSuspendResumeNotification -e GUID_CONSOLE_DISPLAY_STATE src/` 命中 0 处；`WM_POWERBROADCAST` 的处理器只有 `src/main.rs:748`（主窗口）一处，看门狗过程 `src/main.rs:576-631` 无该臂、也不转发（落到 `src/main.rs:629` 的 `DefWindowProcW`）。

生产消费者：`SUSPEND_REASONS`（`src/state.rs:46`）的写方就是这两处处理器；读方是 `is_suspended`（`src/suspend.rs:33-35`）与 `src/main.rs:643/656/670/679` 的各 tick 门控。非生产消费者：`src/state.rs:94-146`、`src/suspend.rs:318-431` 的测试只测位集与 `timer_plan` 纯函数，钉不住「通知是否送达」。

本 note 只做「补生产者」。位丢失后的自愈另有 09 篇；本 note 是它的前置。

## 提案

1. 给主窗口补定向的休眠/唤醒订阅：`RegisterSuspendResumeNotification(HANDLE(hwnd.0), DEVICE_NOTIFY_WINDOW_HANDLE)` + 配对 `UnregisterSuspendResumeNotification`，完全照既有 `POWER_NOTIFY_HANDLE` 的范式（注册 `src/main.rs:335-353`、注销 `src/main.rs:301-306`、重建时重绑 `src/main.rs:461-467`），并遵守「注销先于 `DestroyWindow`」的配对纪律（`src/main.rs:429-431`）。这样 `PBT_APMSUSPEND` / `PBT_APMRESUMEAUTOMATIC` 变成定向消息，`SUSPEND_REASON_SYSTEM` 才有嵌入后仍可达的生产者。
2. 把显示器订阅从 `GUID_MONITOR_POWER_ON` 换成 `GUID_CONSOLE_DISPLAY_STATE`（或在保留旧句柄之外**增订**一个，实施时以实测哪个在 S0 息屏时真的投递为准）。若采用两个句柄，本仓库「电源通知句柄原子存储」的单句柄假设（`src/util.rs` 的原子存储、`src/main.rs:301-306` 的注销点）要相应扩成两个具名原子（例如 `POWER_NOTIFY_HANDLE` / `DISPLAY_NOTIFY_HANDLE`），并按 AGENTS.md 第 10 条指认唯一真值源，不要塞进一个 `Vec` 让注销顺序变成隐式约定。
3. 修正 `src/main.rs:746-747` 与 `src/main.rs:750-751` 的注释：说明 `PBT_POWERSETTINGCHANGE` 是注册定向、APM 事件是顶层广播；并写明「处理器必须留在主窗口侧」的真实理由是 `handle_power_broadcast` 会把 hwnd 交给 `sync_monitoring_timers`（`src/suspend.rs:48`、`src/suspend.rs:65`），定时器住在主 hwnd 上。

## 明确不在本次范围

- **不做超时/兜底清位**：那是 09 篇。本 note 只让通知到达，不改挂起态的收敛策略。
- **不把 `WM_POWERBROADCAST` 搬到看门狗**：`handle_power_broadcast` 把 hwnd 传进 `sync_monitoring_timers`，定时器由 `WM_TIMER` tick 携带主 hwnd 投递；搬过去会让定时器挂到看门狗上，与 `src/main.rs:633-687` 的 tick 分发错位。若将来要复用看门狗，必须走「转发给 `live_main_hwnd()`」而不是直接处理（`src/main.rs:570-574` 已有该范式的先例）。
- **不删除 `SUSPEND_REASON_SYSTEM` 位与其处理器**：定向化之后它才有稳定生产者，删掉会把「休眠省电」整条语义删掉。
- 不改全屏暂停（`MONITOR_FULLSCREEN`）与 `src/suspend.rs:236-249` 的边沿语义。
- 不改 `timer_plan` 的挂起分支（仍为全空）：承接兜底检查的 tick 在**看门狗**上，不占用监测定时器集合，见 09 篇。

## 为什么不保留？

1. 「漏收是双向对称的，SYSTEM 位根本不会置，所以不用管」——这条本仓库自己的代码就反驳了：启动期与嵌入失败期主窗口还是顶层，`PBT_APMSUSPEND` 会被正常收到并置位（`src/main.rs:746-748`）；若此后嵌入成功、休眠期间 resume 广播却因已成为 `WS_CHILD` 而漏收，位就卡在置位状态。**两个窗口期不对称**，所以「对称所以无害」不成立，这正是 09 篇要做按原因自证的原因。
2. 「`GUID_MONITOR_POWER_ON` 一直能用」——它在现代机器上是否仍随显示器开关投递需要实测；`GUID_CONSOLE_DISPLAY_STATE` 是文档化的替代，且是 S0 息屏唯一的省电信号来源。保留 legacy 而不同时验证，等于把 S0 的省电覆盖押在未验证行为上。
3. 「`RegisterSuspendResumeNotification` 有版本门槛」——Win8+ 才有，本仓库目标平台是 Windows 11（`build.rs:6` 的 supportedOS GUID 之一即 Win10/11），无兼容负担；注册失败按既有范式降级为非致命（`show_error` + 不记位），不退化为崩溃。
4. 「多订一个电源通知句柄不值」——它带来的是 S0 息屏的省电覆盖与 09 篇可用的真值源；成本是第二个具名原子 + 第二处注销，与既有 `POWER_NOTIFY_HANDLE` 完全同构。

## 验收标准

- `grep -rn "RegisterSuspendResumeNotification\|UnregisterSuspendResumeNotification" src/` 命中 2 处（注册 + 注销），且注销与 `unregister_session_notification()`（`src/main.rs:431` 的调用点）同批次、在 `DestroyWindow` 之前；重建路径（`src/main.rs:459-467`）同样重绑。
- `grep -rn "GUID_CONSOLE_DISPLAY_STATE" src/` 命中 ≥ 1 处；`grep -rn "GUID_MONITOR_POWER_ON" src/` 的处置在实施说明里写明（替换或并存），不允许两个句柄共用一个原子槽。
- `src/main.rs:746-747` 不再出现「电源设置通知是定向消息」这种把两类通知混同的表述（人工核对）。
- 位集幂等性用例仍然通过：`src/state.rs:135-148` 的 `resume_idempotent_preserves_other_reasons`、`all_three_reasons_must_clear` 不改也不放宽；`src/suspend.rs:394-404` 的 `test_timer_plan_suspended_has_no_timers` **保持不变**（本 note 不动挂起态定时器集合）。
- 实机（含机器能力前置检查）：先 `powercfg /a` 确认本机支持 S3 —— 支持则合盖睡眠→唤醒一次；不支持（S0-only）则本 note 的休眠位路径**无法实机验证**，如实记录，并用「息屏/点亮显示器」验证显示器订阅确实投递。
- `cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt -- --check` 全绿。

## 风险

- `RegisterSuspendResumeNotification` 在跨进程子窗口上的实机行为尚未验证（Windows 的定向通知以窗口句柄投递，理论上不受父子关系影响，但本仓库的嵌入是跨进程 `SetParent`，属未验证组合）。若实测仍收不到，退路是让看门狗接收后转发给 `live_main_hwnd()`；本 note 的「明确不在本次范围」已写明该退路的正确形态（转发而非直接处理）。
- 补上定向订阅后，启动期与嵌入失败期主窗口会**同时**通过广播臂与定向臂各收到一次 APM 事件；位集幂等（`src/state.rs:26-46`）且 `suspend_system` 的 `previous == 0` 守卫（`src/suspend.rs:51-53`）使重复调用不重复 trim，代价只是一次多余的 `sync_monitoring_timers`，可接受。
- 换/增 `GUID_CONSOLE_DISPLAY_STATE` 会改变置位时机：部分机器在启动时会先收到一次「显示器已开」的初始通知，若在该通知与初始 `SUSPEND_REASONS == 0` 状态之间处理不当，可能产生一次无意义的恢复调用（幂等、无观测代价），但实现时不要把它写成「复位位集」。
