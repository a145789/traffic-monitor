# Agent Note：广播消息统一由看门狗接收再分发

Status: implemented

## 问题

项目已因 `TaskbarCreated` 认识到“子窗口收不到顶层广播”（`src/main.rs:318` 注释与 `src/window.rs:114` 看门狗设计），但同一教训没有推广：`WM_SETTINGCHANGE` 主题处理只在主窗口过程 `src/main.rs:475`，而主窗口嵌入后是 `WS_CHILD`，`WM_SETTINGCHANGE` 作为 `HWND_BROADCAST` 顶层广播根本到不了它，表现为换主题后任务栏文字颜色不跟随，直到下次重绘或重启才恢复。与之对比需要保留现状的是两类定向通知：电源设置通知经 `src/main.rs:253` 的 `RegisterPowerSettingNotification(HANDLE(hwnd))` 绑定指定 HWND，锁屏通知经 `src/main.rs:275` 的 `WTSRegisterSessionNotification(hwnd)` 绑定指定 HWND，它们是发往注册句柄的定向消息而非广播，嵌入后仍可达，重建时逐一重绑（`src/main.rs:352`）的现有逻辑正确。检索证据：`Select-String -Pattern "WM_SETTINGCHANGE|is_immersive_color_set" src` 命中主窗口过程与 `suspend.rs:266` 定义共 2 处生产消费，无看门狗侧任何引用；`Select-String -Pattern "RegisterPowerSettingNotification|WTSRegisterSessionNotification" src` 命中注册、注销、重建三处，确认定向路径已有配对语义（`SESSION_NOTIFY_HWND` 先注销后销毁），不得连带搬迁。

## 提案

确立一条路由规则并只搬广播：凡 `HWND_BROADCAST` 类顶层广播（本篇先覆盖 `WM_SETTINGCHANGE` 的 `ImmersiveColorSet` 分支）统一由看门狗顶层窗口接收，再调用与主窗口相同的共享业务处理（`update_text_color` 加 `InvalidateRect`），生产消费者为主题文字颜色（`src/renderer.rs:279`）；非生产消费者为 `suspend.rs:287` 下的 `immersive_color_*` 五例（匹配语义不得变）。定向通知（电源、会话）保持注册到当前主窗口、重建时重绑的现状，本篇只补核查不断言改动。具体改动：看门狗过程新增 `WM_SETTINGCHANGE` 分支，复用 `is_immersive_color_set(lparam)` 判定后直接执行共享处理（与 `01` 的看门狗控制入口一样是看门狗侧直接执行，不引入转发——重建间隙主窗口可能不存在，转发反而多一个失效面）；`PBT_APMSUSPEND` 类电源广播与 `WTS_SESSION_*` 的“广播还是定向”逐条在注释中写明依据（一句话即可），避免后人把定向通知误搬到看门狗导致双重处理。

## 明确不在本次范围

电源与会话通知的注册对象、注销顺序（先 `WTSUnRegisterSessionNotification` 后 `DestroyWindow`）与重建重绑一字不动；`TaskbarCreated` 的重建职责不扩展，看门狗新增的只是无状态转发，不持有渲染器或托盘状态；Hyper-V 网卡关键字策略不在本次范围（见 `06` 的验证矩阵备注，不改关键字）；不新增自定义消息号，若需转发载荷只复用现有 `WM_USER` 偏移或直接调用共享函数。

## 为什么不保留？

最强的反方是“主题一天换几次，漏一次用户手动重启或等下次重绘即可，不值得动看门狗”。逐条回应：漏主题不是“少一次重绘”，而是嵌入后系统性失明（每次都漏），与项目“常驻任务栏”的定位直接冲突；且修复只是把已存在的两行处理（`update_text_color` 加 `InvalidateRect`）换一个入口调用，无新状态、无新线程，成本远低于它修复的“每次必现”。第二个反方是“把所有消息都搬到看门狗统一处理更干净”。不采纳的理由：定向通知的契约对象就是主窗口（电源 `GUID_MONITOR_POWER_ON`、会话锁屏的暂停/恢复要带 `hwnd` 重建定时器），搬过去会导致 `suspend_system(hwnd)` 与 `sync_monitoring_timers(hwnd)` 的目标窗口错位，广播与定向必须分开路由。

## 验收标准

`Select-String -Pattern "WM_SETTINGCHANGE" src/main.rs` 必须同时命中主窗口与看门狗两个过程（或命中共享转发函数被两处调用）；`Select-String -Pattern "WTSRegisterSessionNotification|RegisterPowerSettingNotification" src` 的注册目标仍为主窗口 HWND（注释写明定向依据）。现有测试全过：`cargo test --locked`（点名 `immersive_color_*` 五例、`timer_plan_*` 四例），`cargo clippy --all-targets --locked -- -D warnings`，`cargo fmt -- --check`。人工验证：在已嵌入状态下切换浅色/深色主题，任务栏文字颜色在 2 秒内跟随且无残留粉边；锁屏/解锁、显示器开关、休眠唤醒各一次，暂停与恢复行为与改前一致；Explorer 重启后主题切换仍跟随（看门狗未重建、主窗口已重建的组合态）。

## 风险

残留风险是广播与定向被误判导致双重处理（如 `WM_SETTINGCHANGE` 在主窗口与看门狗各执行一次 `update_text_color`）。实际语义已核实：只有「启动后、`SetParent` 之前」这段窗口期两者同为顶层，此时一次广播确实各处理一次；嵌入后主窗口是 `WS_CHILD`，只剩看门狗分支可达，因此不是每次必现。缓解是共享处理写成幂等的（重算颜色 + 置脏重绘）并在注释写明这是可接受的代价；若日后有人把定向通知也搬进看门狗，则会出现 `suspend_system`/`sync_monitoring_timers` 目标窗口错位，属功能性错误而非多余重绘。证伪依据：若实机出现主题切换后颜色闪烁往返（两次处理打架）或锁屏暂停失效，即判定本篇失败。
