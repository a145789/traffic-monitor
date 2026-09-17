# Agent Note：恢复时重建采样基线，修正首个读数语义

Status: proposed

## 问题

`src/suspend.rs:52` 的 `resume_system` 只复位网络退避（`reset_network_backoff`）并重建定时器，但 CPU 与网络的差分基线仍是暂停前的旧值：网络侧 `src/collector/rate.rs:22` 的 `select_winner_interface` 用 `now.saturating_duration_since(prev_time)` 做归一化，暂停期间无采样导致下一次 `elapsed` 包含整个暂停区间，算出的是暂停区间平均速率而非恢复后的当前速率；CPU 侧 `src/collector/cpu_mem.rs:36` 的 `PREV_*` 同理，恢复后首个差分覆盖整个锁屏/息屏/全屏区间。锁屏、显示器关闭、全屏期间机器仍在产生流量与 CPU 占用（与真休眠不同），因此该首个读数会系统性偏离用户对“当前负载”的预期。检索证据：`Select-String -Pattern "resume_system|suspend_system" src` 命中 `suspend.rs:43`、`suspend.rs:52` 定义与 `handle_power_broadcast`、`handle_session_change`、`check_fullscreen` 三处调用，确认恢复统一收敛于一处，改一处即覆盖全部唤醒路径；`Select-String -Pattern "INTERFACE_HISTORY|CPU_INITIALIZED|PREV_" src/collector` 确认基线分别寄宿在 `network.rs:27` 的 `INTERFACE_HISTORY` 与 `cpu_mem.rs:8` 的 `PREV_*`，且当前没有任何重置入口（`reset_network_backoff` 只清退避计数，不清历史）。

## 提案

先定语义再改代码：对外语义明确为“恢复后的当前负载”（而非“暂停区间平均”），生产消费者为任务栏上的网速与 CPU 文字（`src/renderer.rs:77` 的 `DisplayValues::load` 读取的 `NET_SPEED_*` 与 `CPU_USAGE`）；非生产消费者为 `rate.rs:65` 与 `suspend.rs:287` 下的 timers/归一化单测（语义不得变）。具体改动：在 `collector` 侧新增两个无害重置入口（网络历史清空或用恢复时刻重打时间戳、CPU 基线重采或置 `CPU_INITIALIZED=false` 后由下一 tick 重建），`resume_system` 在 `sync_monitoring_timers` 之前调用它们，恢复后的第一个有效周期只建基线不显示速率（与 `src/collector/cpu_mem.rs:15` 注释的“首轮仅建立基线”语义对齐），第二个周期起恢复正常显示；启动阶段可顺手提前建立一次 CPU 基线，使首个有效值早一个周期出现（可选，不阻塞本篇）。暂停与恢复的定时器销毁重建对称性（`timer_plan` 单测钉死的集合）不得改变。

## 明确不在本次范围

退避阈值（`BACKOFF_ZERO_THRESHOLD`）、采样间隔（1s/15s）、`timer_plan` 的集合划分一字不动；渲染判脏与显示格式不在本次范围；主题与广播路由不在本次范围（另见 `05`）；不为基线引入跨线程共享可变状态，重置入口只在 UI 线程的恢复路径调用，与采集同线程，无需新同步原语。

## 为什么不保留？

最强的反方是“暂停区间平均也是有意义的数据，首个读数偏差一个周期无所谓，用户看不出”。逐条回应：若产品语义真是“区间平均”，则休眠（计数器冻结）与锁屏（计数器继续）会算出两种不可比的值，同一块 UI 文字在两种恢复路径下含义不同，这本身就是语义 bug；偏差不是“一个周期显示旧值”，而是把几小时的累积摊成一个看似合理的新值（如锁屏一夜后恢复瞬间显示一个中等网速），误导性比空白更强。第二个反方是“归一化已经除了 `elapsed`，数学上自洽”。回应：自洽的是“平均”，不是“当前”，而用户把任务栏数字理解为当前，数学自洽不能替代语义正确。

## 验收标准

`Select-String -Pattern "reset.*baseline|clear.*history|CPU_INITIALIZED" src/collector src/suspend.rs` 必须命中恢复路径调用的新基线入口。现有测试必须全过：`cargo test --locked`（点名 `timer_plan_*` 四例、`normalize_*` 五例、`select_winner_interface_*` 五例、`lock_then_monitor_off_*` 状态机三例行为不变），`cargo clippy --all-targets --locked -- -D warnings`，`cargo fmt -- --check`。新增单测：以“暂停 1 小时后恢复”的 `Instant` 间隔驱动 `select_winner_interface`，断言恢复后首个输出为建基线零速而非区间平均；以交错 `suspend(SYSTEM)` 加 `suspend(SESSION)` 后逐个 `resume` 断言基线只在最后一次真正恢复时重建（位集语义不变）。人工验证：锁屏 5 分钟后解锁，首个网速/CPU 不出现与锁屏期间下载量对应的虚假峰值或虚假 idle。

## 风险

残留风险是恢复后会有一个采样周期显示 0 或旧值（建基线窗口），用户可能误读为“唤醒后断网”。缓解是该窗口仅一个网络周期（1s）加一个 CPU 周期（5s），且 `force_repaint` 语义不变；若实机出现恢复后长时间零速（基线被反复清空，如 `resume` 被重复调用），即判定失败。证伪依据：重复 `resume` 同一原因必须幂等（`SuspendReasons::resume` 已是 `fetch_and` 幂等，基线重置必须挂在“从暂停态回到运行态”的边沿上，而非每次 `resume` 调用上）。
