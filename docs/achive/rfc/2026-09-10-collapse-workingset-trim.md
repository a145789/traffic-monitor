# Agent Note：收敛工作集修剪的自适应水位机，保留挂起/初始化修剪

Status: implemented

## 问题

工作集修剪是全仓生产代码量最大的防御性子系统：[`config.rs`](../../../src/config.rs) 有 3 个水位常量（最低门槛、基线放大百分比、冷却秒数）加 2 套定时器 ID/间隔（`MEMORY_MAINTENANCE` 每 60s、`INIT_TRIM` 一次性 10s），[`state.rs`](../../../src/state.rs) 有跨线程 `Mutex<TrimBookkeeping>`（`last_trim_at` / `pending_baseline` / `steady_state_bytes` 三字段），[`util.rs`](../../../src/util.rs) 有 `trim_working_set`、`trim_working_set_if_needed`、`trim_threshold`、`compact_and_trim` 四个入口加 3 个水位单测，[`suspend.rs`](../../../src/suspend.rs) 的 `TimerPlan` 与 `sync_monitoring_timers` 为它单独保留 `memory_maintenance` 分支，[`main.rs`](../../../src/main.rs) 为它保留两个 `WM_TIMER` 分支，更新子进程的 `compact_and_trim` 还要与 UI 线程共享同一冷却时钟。本体只是一个常驻任务栏、每秒一次 GDI 文本绘制的小组件，且主进程已设置低内存优先级、更新重活全在短命子进程里，水位机的“基线校准 → 百分比放大 → 冷却抖动抑制”三段逻辑成本明显高于它能省下的几 MB Standby 页。

## 提案

把自适应水位机降级为两条确定性规则并删除其余机件：保留挂起入口（`suspend_system` 内）与启动后 10 秒一次性 `TIMER_ID_INIT_TRIM` 定时器原样（该延迟是 load-bearing 的：等 GDI 位图、字体、DLL 页全部 fault-in 后才修剪初始峰值，不可并入消息循环前直接调用），保留更新子进程结束前的 `compact_and_trim`（其压缩 UCRT 堆的语义与普通修剪不同，见 `util.rs` 注释）；删除 `TrimBookkeeping` 结构体与 `TRIM_BOOKKEEPING` 全局锁、`trim_threshold`、`trim_working_set_if_needed`、`TIMER_ID_MEMORY_MAINTENANCE` / `TIMER_INTERVAL_MEMORY_MAINTENANCE` / 三个水位常量、`TimerPlan::memory_maintenance` 分支及对应单测；同步简化 `trim_working_set` 本体去掉簿记写入（`util.rs:183-185`）并更新 `compact_and_trim` 的“收口写入共享簿记”注释。`trim_working_set` 本体（`SetProcessWorkingSetSize(MAX, MAX)` 一行）保留。

## 为什么不保留？

最强的反方理由是“长稳态运行后堆碎片与 GDI 位图会把工作集推高，周期水位门是唯一自动回收”。但现有水位门恰恰为避免“静态阈值误伤稳态基线、反复 trim 制造缺页”才引入基线校准与 15 分钟冷却，结果是大多数周期都在“采样 → 发现未超阈 → 什么都不做”， occasionally 的一次全量 trim 又与挂起修剪语义重复；挂起/锁屏/显示器关闭是本机每天必然发生的事件，其附带的无条件修剪已覆盖 24h 级回收，剩下的极端长开机场景可用任务管理器一次观测验证，而不必为它常驻一个跨线程锁加一个每分钟唤醒的定时器（定时器本身就与低功耗常驻目标相悖）。

## 验收标准

`rg -n "TrimBookkeeping|TRIM_BOOKKEEPING|trim_threshold|trim_working_set_if_needed|MEMORY_MAINTENANCE|WORKING_SET_TRIM" src/` 无命中（`TIMER_ID_INIT_TRIM` / `TIMER_INTERVAL_INIT_TRIM` 常量与一次性定时器分支保留，不在删除范围）；`timer_plan` 结构体失去 `memory_maintenance` 字段且暂停/恢复对称单测仍全绿；连续运行 24h（含至少一次挂起或锁屏）后任务管理器工作集与改动前同量级（±20% 内），且 1s 采样无肉眼可感知的缺页卡顿。

## 风险

若用户机器从不挂起、从不锁屏、连续开机数周，水位可能缓慢上漂。缓解：启动后一次性修剪已压住初始峰值；真出现可复现的上漂报告时，恢复“固定间隔无条件修剪”（无基线、无冷却，10 行以内）即可，比恢复整套自适应机便宜一个数量级；本次删除不触碰 `set_low_memory_priority`、`configure_background_process` 与 `/DELAYLOAD` 隔离，更新子进程的 DLL 回收语义不变。
