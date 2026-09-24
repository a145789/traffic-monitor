# Agent Note：闭合三条恢复路径的失败环（嵌入后置断言 / DPI 事务 / 缺失定时器补建）

Status: proposed

## 问题

三处同源：**一次瞬态失败之后，没有状态位也没有重试点**，只能等下一次外部事件（下一次 DPI 变化、Explorer 重建、状态切换）才可能自愈。

**其一，`SetParent` 没有后置断言。** `src/window.rs:229` 用 `SetParent(hwnd, Some(h_taskbar)).map_err(...)?` 把 windows crate 的 `Result<HWND>` 当作成功判据，而 `src/window.rs:223-228` 的注释自己已承认 Win32 契约有歧义（NULL 既可能是「前一个父窗口」也可能表示失败，且不保证设置 last error）。嵌入成功的依据因此是「本机实测返回了桌面句柄」这条经验：五步序列走完直接 `EMBEDDED.store(true)`（`src/window.rs:275`）。恢复路径反而有校验——`parent_is_current_taskbar`（`src/window.rs:325-333`）会比对 `GetParent` 与当前任务栏——首轮嵌入没有。

**其二，`update_dpi` 失败后没有脏位、没有重试点，而且「自愈」路径本身是错的。** `src/main.rs:728-739` 在失败分支只调 `rollback_window_to_bitmap`（`src/main.rs:386-391` → `resize_embedded_window`）；失败时 `update_dpi` 不写任何状态（`src/renderer.rs:436-466` 的三条失败路径都在 `self.width/height` 赋值之前返回），全仓也没有 DPI 相关的位。更关键的是：`update_dpi` **只换位图与字体**（`src/renderer.rs:436-490`），真正改窗口物理尺寸的是 `embed_in_taskbar`（`src/window.rs:206-277`）——所以「回滚窗口 → 下个 tick 自愈」这条注释（`src/main.rs:737`）缺的正是「谁来把窗口提交到新尺寸」。`reembed_if_lost`（`src/window.rs:309-321`）只调 `embed_in_taskbar`，不重跑 `update_dpi`；`update_dpi` 全仓只有 2 个调用点（`src/main.rs:404` 启动/重建尾段、`src/main.rs:730` 的 `WM_DPICHANGED`）。
   现有 `update_taskbar_position`（`src/window.rs:335-376`）在移动成功后**无条件**把完整目标矩形（含宽高）写进 `LAST_RECT`（`src/window.rs:350`、`:373`），所以任何「只改位置不改尺寸」的中间态都会把这个未生效的尺寸记成已生效：下一个 tick 命中 `LAST_RECT == target` 直接 `return false`（`src/window.rs:351-353`），窗口永久停在旧尺寸、位图却是新 DPI 版式。

**其三，定时器同步失败没有重试点，而且最坏情形下没有可用的重试载体。** `src/suspend.rs:169-216` 先杀全部（`:176-183`）再按 `timer_plan` 重建，核心定时器失败只 `diag`（`:205-213`）并返回 false；失败后没有任何重试点，只能等下一次状态切换。3 个调用点直接丢弃返回值（`src/main.rs:707`、`src/main.rs:713`、`src/suspend.rs:245`，均为 `let _ = sync_monitoring_timers(hwnd);`）。其中 `TIMER_ID_FULLSCREEN` 失败会连带失去嵌入自愈（`src/main.rs:642-653` 是该 tick 的唯一消费者）与全屏检测——而 `timer_plan` 的全屏分支**只有**这一个定时器（`src/suspend.rs:143-150`），所以「把重试挂在常驻监测 tick 上」在最需要它的场景里根本没有 tick 可挂：这是旧稿的错误设计。同理「由其他 tick 反复调用完整的 `sync_monitoring_timers`」会不断执行「先杀后建」，把健康的网络/CPU 定时器倒计时反复重置（持续单点失败即饿死采样）。自动更新定时器刻意不计入返回值（`src/suspend.rs:166-168`、`:200-203`），此为既有裁定，本次不动。

生产消费者：三处都在正常路径上（`src/main.rs:248-252` 首轮嵌入、`src/main.rs:477` 重建、`src/main.rs:658` 每 1 秒的任务栏位置更新、每次状态切换的定时器同步）。非生产消费者：`src/suspend.rs:382-431` 的 `timer_plan` 测试只测计划集合，`src/window.rs` 无测试（该文件没有 `#[cfg(test)]`），`src/renderer.rs:608-683` 的格式测试与 DPI 无关。

## 提案

1. **`SetParent` 后置断言。** `embed_in_taskbar`（`src/window.rs:206-277`）在五步序列末尾、`EMBEDDED.store(true)` 之前加：`GetParent(hwnd).ok() == Some(h_taskbar)`；不成立则返回 `Err`、保持 `EMBEDDED` 为 false，交给既有周期重试（`src/window.rs:309-321`）。不要改五步顺序（AGENTS.md 第 1 条）。
2. **在看门狗上建立独立于监测定时器集合的恢复调度器**（本 note 的中枢，09 篇也依赖它）：
   - 新常量 `TIMER_ID_RECOVERY`（`src/config.rs:60` 一带，与 `TIMER_ID_REBUILD_RETRY` 并列）+ 间隔常量（建议 60 秒）。
   - 在 `create_watchdog_window` 成功后 `SetTimer(watchdog, TIMER_ID_RECOVERY, ...)`，在 `watchdog_wnd_proc` 的 `WM_TIMER` 分支里消费（既有 `src/main.rs:609-612` 是同范式的先例）。看门狗永不参与挂起、永不重建，因此这个 tick 在任何状态下都存在——这解决了「唯一的自愈 tick 被 `timer_plan` 一起杀掉」的死结，而**不需要**放宽挂起态的定时器集合。
   - 调度器只做三件事，全部通过 `live_main_hwnd()`（`src/main.rs:486-489`）取当前主窗口：重试 `DPI_DIRTY`、补建缺失定时器、按 09 篇的原因探针清陈旧挂起位。自身创建失败时 `diag` + `log_event` 退避（照 `arm_rebuild_retry` 的间隔翻倍写法，`src/main.rs:501-521`）。
3. **DPI 恢复收口成四步事务**（顺序不可拆）：
   1. 重试 `update_dpi(hwnd)`（`src/renderer.rs:436`）；
   2. 成功则**提交窗口几何**——`resize_embedded_window(hwnd, bitmap_size)` 或直接重跑 `embed_in_taskbar(hwnd)`（二选一由实施者按实测选定，前者更轻，后者顺带修复嵌入漂移）；
   3. **失效 `LAST_RECT`**：把 `src/window.rs:337` 的函数内 `thread_local` 提到模块作用域，并新增 `invalidate_last_rect()` 供事务第 3 步与 `embed_in_taskbar` 成功后调用；
   4. 前三步全部成功才清 `DPI_DIRTY` 并 `InvalidateRect` 重绘。
   - `DPI_DIRTY` 的置位点是**两处**：`src/main.rs:735-739` 的 `WM_DPICHANGED` 失败分支与 `src/main.rs:407-410` 的启动/重建尾段（`bind_display_and_timers`）失败分支。
   - 脏位期间 `update_taskbar_position` 追加 `SWP_NOSIZE`（只改位置），并且**跳过 `LAST_RECT` 提交**（否则第 2、3 步就白做了）；把标志选择抽成纯函数 `position_flags(dpi_dirty: bool) -> WINDOW_POS_FLAGS` 供单测。
4. **定时器同步失败改成「只补缺失、不重建全部」。** `sync_monitoring_timers`（`src/suspend.rs:169-216`）的返回值从 `bool` 改成「失败的核心定时器 ID 集合」（小结构体或位掩码均可）；新增 `create_missing_timers(hwnd, missing)`，**只**为缺失项调用 `set_coalescable_timer`，不执行任何 `KillTimer`（因此不会重置健康定时器的倒计时）。恢复调度器每个周期调用它一次；连续失败按调度器自身的退避间隔重试，不跨 tick 拆分。
   - 三个 `let _ =` 调用点（`src/main.rs:707`、`src/main.rs:713`、`src/suspend.rs:245`）统一改为走「失败即登记缺失」的封装函数，避免以后新增调用点再次静默丢弃。
   - 首次收敛仍走既有「先杀后建」的 `sync_monitoring_timers`（AGENTS.md 第 7 条的对称要求不变），本提案只在其**失败之后**补一层只增不删的重试。

## 明确不在本次范围

- **不删除 `rollback_window_to_bitmap`**：DPI 事务修好之后它仍是必要的即时兜底（尺寸与位图必须始终一致），只是不再需要承担「自愈」的责任。
- **不改 `sync_monitoring_timers` 的「先杀后建」结构**：它是「定时器集合永远收敛到唯一正确集合」的实现，符合 AGENTS.md 第 7 条；重试只补在失败之后。
- **不改挂起态定时器集合**（`timer_plan` 的挂起分支仍为全空，`test_timer_plan_suspended_has_no_timers` 不变）：恢复 tick 住在看门狗上，不需要占用监测定时器。
- **不给自动更新定时器加失败上报或重试**（`src/suspend.rs:166-168` 已裁定 best-effort，且它有独立的周期轮询兜底）。
- **不改 `WM_DPICHANGED` 里「失败不弹框」的决定**（`src/main.rs:732-733` 的理由是避免跨屏拖动时弹窗风暴，成立）。
- **不把 `reembed_if_lost` 从 `TIMER_ID_FULLSCREEN` 上搬走**（AGENTS.md 第 5 条明确要求挂在那里）；恢复调度器是新增的补充，不是替代。

## 为什么不保留？

1. 「GDI 位图/字体创建失败极罕见，不值得加状态」——罕见但代价不对称：失败后是**永久**错版式（下一次 `WM_DPICHANGED` 之前不会恢复），而修法只是增加一个原子 + 一个已有 tick 里的一次判断，与既有 `EMBEDDED` 同构。
2. 「`SetCoalescableTimer` 对有效 hwnd 不会失败」——恰恰在 Explorer 重建竞态里 hwnd 会失效，那正是最需要自愈的时刻；返回 false 却被 3 个调用点丢弃，等于把「需要重试」这一信息扔了。
3. 「恢复调度器挂在看门狗上会踩 AGENTS.md 第 5 条（看门狗不参与嵌入与显示）」——该条禁止的是「让看门狗参与嵌入或显示」，而它本来就是电源/会话通知、托盘与重建重试的稳定落点（`src/main.rs:576-631`）；新增一个只调「补建定时器 / 重试 DPI / 清陈旧位」的调度器不改变这条边界。
4. 「加位会让状态机更复杂，AGENTS.md 第 10 条要求指认唯一真值源」——`DPI_DIRTY` 的唯一写方是两处置位分支，唯一读方是恢复调度器与 `position_flags`，清位只在事务四步全成功之后；比 `EMBEDDED` 的收敛方式更简单，属范式内新增。
5. 「`SetParent` 已经实测没问题，加断言是浪费」——实测是本机 Win11 的一次观测；MS 文档同时写明「跨进程 `SetParent` 会强制重置子窗口进程的 DPI 感知」，加一个 3 行的后置断言是把经验固化成契约的最便宜方式。

## 验收标准

- `grep -rn "TIMER_ID_RECOVERY" src/` 命中 ≥ 3 处（常量定义、`SetTimer`、`WM_TIMER` 分支）；`grep -rn "KillTimer" src/suspend.rs` 的命中数不增加（补建路径不得带删除）。
- `grep -rn "DPI_DIRTY" src/`：定义 1 处、置位 2 处、清位 1 处、条件读 ≥ 1 处（`position_flags` 或其调用点）。
- `grep -rn "invalidate_last_rect" src/` 命中 ≥ 2 处（定义 + DPI 事务第 3 步）；`grep -rn "LAST_RECT" src/window.rs` 出现在模块作用域而非函数体内（人工核对）。
- `grep -rn "let _ = sync_monitoring_timers" src/` 命中 0 处；`grep -rn "sync_monitoring_timers" src/` 的每个调用点都处理了返回值。
- 新增纯函数测试：`position_flags(true)` 必须含 `SWP_NOSIZE`、`position_flags(false)` 必须不含；以及「缺失集合为空 ⇒ 补建函数不调用任何 `set_coalescable_timer`」的判定（把补建逻辑抽成可注入的纯判定即可，与 `src/suspend.rs:393-430` 的 `timer_plan` 测试同风格）。
- 既有用例全部保持通过：`test_timer_plan_suspended_has_no_timers`、`all_three_reasons_must_clear` 等（本 note 不改挂起集合与位集语义）。
- 实机：拖动窗口跨两个不同缩放比的显示器，组件尺寸与字号同步变化且不出现半幅透明/裁切；并在一次 Explorer 重启（任务管理器结束 explorer.exe，再手动启动）后确认数字在恢复 tick 周期内继续更新。
- `cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt -- --check` 全绿。
- 08 篇的集成冒烟在本 note 合入后才能绿（其 DPI 与重建用例断言 `DPI_DIRTY` 的清位结果），两篇的落地顺序不可颠倒。

## 风险

- 后置断言失败会在「`SetParent` 实际成功但返回 NULL」的机器上把嵌入判为失败，表现为面板持续不可见 + 每 2 秒重试。缓解办法是断言写成「`GetParent` 与任务栏一致」而不是「`SetParent` 返回值非空」（提案已按此写）；上线前应在一台以上的 Win11 机器上验证。
- 恢复调度器在每个周期都调用 `create_missing_timers`，若某个定时器因 hwnd 失效而持续失败，会形成每 60 秒一次的失败重试；靠调度器自身的间隔翻倍退避收敛，不要把它做成「失败即立刻重试」。
- 「DPI 事务第 2 步选 `resize_embedded_window` 还是 `embed_in_taskbar`」有实测差异：前者不重新计算任务栏几何，可能在新 DPI 下留下位置偏差；后者更彻底但会重复 `SetLayeredWindowAttributes`。实施时必须实机跨屏确认，不能只看编译通过。
- `LAST_RECT` 提到模块作用域后，多窗口场景（重建期间新旧句柄并存）会共享同一份缓存；`rebuild_main_window` 路径需在创建新窗口后显式失效（`src/main.rs:426-480`），否则新窗口会继承旧句柄的矩形而跳过首次定位。
