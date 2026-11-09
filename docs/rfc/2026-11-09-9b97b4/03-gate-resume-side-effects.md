# Agent Note：把系统恢复的副作用挂到真实暂停位边沿上

Status: proposed

## 问题

`resume_system`（`src/suspend.rs:80`-`src/suspend.rs:94`）对三个副作用不判位：`reset_network_backoff()`（`src/suspend.rs:91`）、`resync_monitoring_timers`（`src/suspend.rs:92`，内部先杀后建见 `src/suspend.rs:464`-`src/suspend.rs:474`）、`force_repaint`（`src/suspend.rs:93`）。唯一挂边沿的是基线重建（`src/suspend.rs:86`-`src/suspend.rs:89` + `should_rebuild_baseline` `src/suspend.rs:96`-`src/suspend.rs:104`）——也就是说，同一份纪律在同一个函数里只落实了四分之一。

「位没置位也会走到 `resume_system`」的通路有三条，都在本文件内：两个显示器电源订阅（`GUID_MONITOR_POWER_ON` 与 `GUID_CONSOLE_DISPLAY_STATE`，`src/suspend.rs:112`-`src/suspend.rs:114`）可能对同一次点亮各投一次；显示状态 `Data[0] == 2`（变暗）按「开」处理（`src/suspend.rs:131`-`src/suspend.rs:139`）；`PBT_APMRESUMEAUTOMATIC` 可能没有对应前驱。`src/suspend.rs:111` 把代价写成「只是一次多余的定时器同步」，低估了它：先杀后建会把健康定时器的倒计时归零——`src/suspend.rs:431`-`src/suspend.rs:435` 的注释已经明写这种模式会「把采样饿死」，两处描述的是同一机制的两个方向，一处当成代价可忽略，一处当成必须规避。

量级要诚实：网络采样间隔 1 秒（`TIMER_INTERVAL_NETWORK`，`src/config.rs:100`），零速退避阈值 `BACKOFF_ZERO_THRESHOLD` = 5（`src/config.rs:115`）。要让网络采样真被饿死，假 resume 的到达间隔必须小于 1 秒；上述三条通路都由用户/电源状态驱动，现实节奏是每小时数次。所以实际损失是「每次假 resume 白付一次倒计时重置 + 退避计数被清零」，不是「永久不采样」。这条笔记的立足点是行为语义与既有不变量不一致，不是高危缺陷。

状态属主处其实已经算出了需要的信息：`SuspendReasons::suspend` 返回置位前的位集（`src/state.rs:40`-`src/state.rs:43`），而 `SuspendReasons::resume` 返回 `()`（`src/state.rs:45`-`src/state.rs:48`）——「这一次调用是否真的构成边沿」这个事实在属主处被丢掉，调用方只能靠自己重算，于是重算了基线一项就停了。

生产消费者：`handle_power_broadcast`（`src/suspend.rs:116`-`src/suspend.rs:145`）、`handle_session_change`（`src/suspend.rs:147`-`src/suspend.rs:159`）、`heal_stale_suspend`（`src/suspend.rs:246`-`src/suspend.rs:270`）。非生产消费者：`test_baseline_rebuild_only_on_last_resume_edge`（`src/suspend.rs:638`-`src/suspend.rs:661`）、`src/state.rs:118`-`src/state.rs:169` 三条位协议用例。

## 提案

1. 让 `SuspendReasons::resume` 返回清除前的位集（`fetch_and` 的旧值），与 `suspend` 对称。
2. `resume_system` 首行取出旧值，`old & reason == 0` 时直接返回：位本来没置位 ⇒ 没有真实恢复边沿，不重建基线、不清退避、不同步定时器、不重绘。三个副作用因此全部挂到真实边沿上，与 `should_rebuild_baseline` 同一条纪律。
3. 调用点不动：`heal_stale_suspend` 三处调用前都判了 `is_set`（`src/suspend.rs:252`、`src/suspend.rs:258`、`src/suspend.rs:265`），`handle_power_broadcast` / `handle_session_change` 保持原样；副作用集合本身一个也不删。
4. 顺带处理 `suspend_system` 里那句 `MONITOR_FULLSCREEN.store(false, Ordering::Release)`（`src/suspend.rs:73`）：它是该事实的第二个写点（唯一另一个是 `on_fullscreen_edge` 里的 `src/suspend.rs:530`），同线程无竞态，但当前零注释，读者无法知道它为什么必须在这里清位。做法二选一：(a) 原地补注释，说明「挂起态下监测定时器集合全空、位若留真会让恢复后的 `timer_plan` 停在仅全屏检测分支，进而杀光网络与 CPU 采样」；(b) 在 `on_fullscreen_edge` 旁边加一个两行的 `clear_fullscreen_on_suspend()`，把语义与「为何不走完整收尾」集中到一处。**不要**改成调用 `on_fullscreen_edge`：那条路径会带出 `force_repaint` 与一次 `reset_network_baseline`/`reset_cpu_baseline`，而 `suspend_system` 紧接着自己还会 `resync_monitoring_timers` 一次，等于凭空多一轮先杀后建。

## 明确不在本次范围

- 不动 `on_fullscreen_edge`（`src/suspend.rs:526`-`src/suspend.rs:539`）的边沿语义：它已经正确地只在 `was != now` 时行动，正是本提案要照抄的范式。
- 不改「全屏边沿独立恢复采样、但遵守同一基线语义」这条现行不变量（`AGENTS.md` 的暂停/恢复条）。
- 不改 TTL / 探针 / 自证三分（`src/suspend.rs:246`-`src/suspend.rs:270`）：三种原因的证据强度不同是刻意设计（SYSTEM 可自证、SESSION 有文档化只读探针、MONITOR 只能保守超长 TTL），统一超时会把零成本的自证降级成猜测。
- 不改 `resync_monitoring_timers` 的「先杀后建」（`src/suspend.rs:464`-`src/suspend.rs:474`）：它是「定时器集合收敛到唯一正确集合」的实现，只增不删的补建路径是 `retry_missing_timers`（`src/suspend.rs:438`-`src/suspend.rs:453`）。本提案只让真边沿变少，不动收敛机制本身。
- 不在本笔记里修交接副本 exe 的 `copy` → `spawn` 窗口（`src/update/installer.rs:447`-`src/update/installer.rs:448`）：已复核属实，但属新增防护，另有笔记待立。

## 为什么不保留？

- **反方一：`resume` 是无条件收尾，简单且幂等，判位反而多一个分支。** 回应：幂等的是结果，不是代价。先杀后建的定时器同步不是零成本操作，且会顺带清掉退避计数；判位只是把状态属主已经算出来的事实（`fetch_and` 的旧值）传出来，没有新增任何状态。
- **反方二：假 resume 频率极低，不值得动。** 回应：频率低是当前事件源的事实，不是不变量。Modern Standby 下显示器订阅是唯一省电信号（见 `src/suspend.rs:106`-`src/suspend.rs:108` 的注释），驱动/固件重复投递电源设置不是可排除的场景。修复成本约 6 行加一条位协议用例，收益是让「恢复副作用挂边沿」与「基线重建挂边沿」成为同一条纪律，而不是一条纪律加三个例外。
- **反方三：早退会让 `force_repaint` 少跑，可能留下陈旧画面。** 回应：`force_repaint` 的存在理由是「挂起期间分层窗口表面可能被系统丢弃」（`src/suspend.rs:59`-`src/suspend.rs:65`）；位没置位说明本轮并没有经历挂起，没有需要自愈的表面。真边沿（含 `heal_stale_suspend` 的清位）仍会重绘。
- **反方四：这是行为变更，不是删减，不符合审计范围。** 回应：它删掉的是「三次无条件副作用」，保留语义不变；且它修的是一个已被本仓自己写进注释的风险（`src/suspend.rs:431`-`src/suspend.rs:435`）。若坚持纯删减口径，最小版本是只让 `reset_network_backoff` 与 `resync_monitoring_timers` 判位，`force_repaint` 留在无条件区——但那样又回到四分之一纪律，不建议。

## 验收标准

- `grep -n "pub fn resume" src/state.rs` 的返回类型是 `u32`。
- `src/state.rs` 的测试模块新增一条位协议用例：`resume` 返回清除前的位集；对已清位的原因再次调用返回 0 且不改变位集。形状与 `resume_idempotent_preserves_other_reasons`（`src/state.rs:156`-`src/state.rs:169`）一致。
- `test_baseline_rebuild_only_on_last_resume_edge`（`src/suspend.rs:638`-`src/suspend.rs:661`）保持绿且断言不放宽。
- 若把「是否构成边沿」提成纯函数（如 `should_apply_resume_side_effects(old, reason)`），必须有对应用例——这是 `hwnd` 依赖之外唯一能被单测钉住的部分，也是本提案可回归的核心。
- `src/suspend.rs:73` 那行要么带上说明第二个写点与「为何跳过重绘/基线重置」的注释，要么被新出口函数替代；`grep -n "MONITOR_FULLSCREEN.store" src/suspend.rs` 仍只有两处。
- 四道门禁全绿：`cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`。

## 风险

- 行为变化：重复/双订阅的点亮事件不再触发定时器重同步与强制重绘。若现场存在「靠每次点亮事件顺手自愈」的隐性依赖（例如某类显示器唤醒后主窗口表面陈旧、且没有其它重绘源），会退化成等下一次数值变化才重绘。证伪路径：真边沿（`MONITOR` 位由置位到清除）仍走完整副作用，且同 tick 的网络采样会写入新速率并触发重绘。若实测出现唤醒后陈旧画面，说明需要的是「点亮即重绘」而不是「点亮即重同步」——那时把 `force_repaint` 单独留在无条件区即可，不必回退整条。
- `heal_stale_suspend` 的 SYSTEM 自证分支在 `PBT_APMSUSPEND` 与真正入睡之间被恢复 tick 命中时，会先清位，醒来后不再构成边沿；但 S3 期间 QPC 不走、网卡计数器几乎不增，两侧同时接近零，既不会产生虚假峰值也不会长期冻结，实害接近零，只损失一次基线重建。本提案不改变该窗口宽度，也不试图修它（恢复 tick 基础间隔 60 秒，窗口通常 1-2 秒）。
- 若将来有新的恢复入口在「位未置位」时仍期望副作用（例如某处想借 `resume_system` 顺带清退避），早退会让它静默失效。规避：不要新增这类调用方；确实需要时显式调 `reset_network_backoff`，别借用恢复入口。
