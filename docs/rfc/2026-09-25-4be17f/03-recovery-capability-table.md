# Agent Note：把托盘图标与会话通知纳入既有的恢复能力表

Status: proposed

## 问题

`src/main.rs:856` 的 `run_recovery` 每周期只做三件事：DPI 事务（`src/main.rs:862`）、补建缺失定时器（`src/main.rs:865`）、陈旧挂起位自愈与电源订阅补注册（`src/main.rs:869-877`）。它由看门狗上的定时器驱动（`src/config.rs:77` 的 `TIMER_ID_RECOVERY`，编排见 `src/main.rs:836`），是「恢复调度必须独立于外部事件次数」这一不变量的唯一执行体。

有两类能力是一次性注册、失败后除 Explorer 重建外无人重试：

其一，托盘图标。`src/tray.rs:71-76` 在 `NIM_ADD` 失败时清空 `TRAY_DATA` 并返回 `false`；唯一调用点是 `src/main.rs:641`，它属于 `src/main.rs:639 bind_display_and_timers`，而该函数只被 `src/main.rs:395`（启动）与 `src/main.rs:722`（Explorer 重建）调用。失败时只写 `diag!` 与 `log_event!`（`src/main.rs:642-643`），注释自陈「本会话无图标」。托盘图标是全程序唯一的 UI 入口：退出、开机自启、自动更新开关、手动检查更新全在右键菜单（`src/tray.rs:124-167`、`src/tray.rs:252-259`），没有图标这些能力全部不可达，用户只能去任务管理器结束进程。

其二，会话（锁屏）通知。`src/main.rs:601-607` 注册失败只 `show_error` 一次，之后无人重试。失败意味着 `SUSPEND_REASON_SESSION`（`src/state.rs:17`）在该进程内永远没有生产者，`src/main.rs:1166` 的 `WM_WTSSESSION_CHANGE` 分支永不触发，锁屏不再暂停采样，且无任何后续提示。

检索证据：`grep -rn "create_tray_icon|register_session_notification" src` 的调用点只有 `src/main.rs:395`、`src/main.rs:641`、`src/main.rs:722`；`grep -rn "TRAY_DATA" src` 的写点只有 `src/tray.rs:41`、`74`、`88`，没有任何周期性读方。

与 `AGENTS.md` 的冲突：不变量要求「重建或嵌入的瞬态失败必须有**独立于外部事件次数**的恢复调度并最终收敛」，并要求重建后托盘、电源/会话通知不得退化为假活。当前这两项依赖「下一次 Explorer 重建」这一外部事件，不满足该条。

## 提案

沿用仓库里已经存在的模式：`src/main.rs:563-575` 的 `ensure_power_notifications` 就是「句柄原子为空即补注册」，且已被 `run_recovery`（`src/main.rs:876`）每周期调用。把同一条判据扩展到另两项：托盘看 `TRAY_DATA`（`src/tray.rs:31`，已是唯一真值源），会话通知看 `SESSION_NOTIFY_HWND`（`src/main.rs:92`，写点 `src/main.rs:604`、清点 `src/main.rs:619`），为空则补做一次。

补做失败计入 `run_recovery` 的 `all_ok`，参与既有退避（`src/main.rs:836-847`），与 `retry_missing_timers` 同语义，**不新增退避机制**。

静默：补做成功与失败都不弹框；启动路径首轮失败的那一次 `show_error` 语义保持不变，符合 `AGENTS.md`「同一失败序列只提示一次，周期重试必须静默」。

## 明确不在本次范围

**不把 `TIMER_ID_AUTO_UPDATE` 加进 `MissingTimers`**：自动更新定时器由 `src/suspend.rs:420` 的 `resync_monitoring_timers` 在每次息屏/锁屏/全屏/网络状态切换时随计划集合重建（`src/suspend.rs:482`），已有周期性重建入口；再纳入恢复表会造成同一事实两份表示，正是简化审计要删的冗余。

**不新增 `SUSPEND_REASON_*` 位、不改 `src/state.rs` 的位协议**：本篇只修「注册不重试」。

**不改配对注销纪律**：`src/main.rs:609-624` 的「销毁前注销、句柄即真值源」必须保留，补注册路径不得绕过 `unregister_session_notification`。

**不删 `src/main.rs:114-138` 的四个 `#[cfg(test)]` 访问器**（`power_notify_handle`、`display_notify_handle`、`suspend_notify_handle`、`session_notify_hwnd`）：它们是 `src/smoke.rs:152-193` 四条真 Explorer 冒烟用例的唯一观测手段，而 `AGENTS.md` 明确要求保留这套 opt-in 真集成验证。它们的「唯一消费者是测试」是有意为之，不是无生产消费者的表面积。

**不给托盘加「重试 N 次后弹框」**：会违反静默重试不变量。

## 为什么不保留？

最强的反方理由是：这两个原子为空的状态实务上极少出现（`Shell_NotifyIconW` 失败主因是 Explorer 未就绪），而 `src/main.rs:722` 的重建路径已经会重试，再加调度属「永不触发的兜底」；且恢复调度每周期多两次原子读是净开销。

逐条回应：第一，「Explorer 重建会重试」不满足 `AGENTS.md` 的「独立于外部事件次数」——Explorer 可能整场会话都不重启，此时托盘图标再也回不来，用户失去的是**退出程序**的能力，代价与概率不成比例；会话通知同理，锁屏暂停能力永久退化且无可见提示。第二，每周期开销是两次原子读（`TRAY_DATA` 借用判断加入 `SESSION_NOTIFY_HWND.load()`，后者已有现成形态见 `src/main.rs:619`），与同函数内的 `recover_dpi`、`retry_missing_timers` 相比可忽略。第三，本项不新增符号、不新增状态，只新增两处读与两处补做调用，删掉它不会让任何调用链变短。

结论：判据不是「失败概率低就该删」，而是 `AGENTS.md` 明写的恢复调度必须独立于外部事件次数。若未来审计要删，必须先证伪「Explorer 在一场会话内不重建时托盘图标可自行恢复」。

## 验收标准

`grep -n "run_recovery" -A 30 src/main.rs` 中可读到托盘与会话通知两项补做（`grep` 只能定位，需人工核对）。

新增用例名 `test_recovery_rearms_tray_and_session`，位于 `src/main.rs` 的 `#[cfg(test)]`：清空 `TRAY_DATA` 与 `SESSION_NOTIFY_HWND` 后调用恢复动作，断言补做被触发且返回值反映结果（无需真 Explorer）。

弱测试自查：删掉恢复表里的托盘那一项，上述用例必须变红。

真 Explorer 冒烟不受影响：`cargo test --locked -- --ignored --test-threads=1` 四条全绿，尤其 `src/smoke.rs:131 rebuild_rebinds_new_main_window`。

默认门禁数量不变（`108 passed; 0 failed; 4 ignored`），四条门禁全绿。

## 风险

若用户把托盘图标拖入 Windows 11 的溢出区，`NIM_ADD` 成功但图标不可见，`TRAY_DATA` 非空，本项不会介入——该场景属产品设置，不在范围内，如实登记为残留。

补注册与 Explorer 重建路径同属一个 UI 线程、实际串行；仍须确认不出现「两次注册、一次注销」的配对失衡。实施时以 `SESSION_NOTIFY_HWND` 为唯一真值源，注册前先判空，失败时不得写入句柄（沿用 `src/main.rs:603-604` 的既有纪律）。
