# Agent Note：安装器推迟强杀并优先优雅退出

Status: implemented

## 问题

`installer.iss:49` 的 `InitializeSetup` 在向导出现之前就循环 `taskkill /F /T /IM traffic-monitor.exe`，导致三个实际伤害：用户只是双击打开安装器随后取消，应用也已被杀；按映像名匹配把同名的 `--check-update` 更新子进程（与主进程同一 exe 名）也纳入击杀范围，而该子进程正是刚发出 `EXIT_MAIN`、正在执行 `wait_main_instance_gone` 加 `launch_installer` 的协调者；`AppMutex=TrafficMonitor_Mutex_Instance`（`installer.iss:5`）本可用于识别目标实例，却被无差别的映像名强杀绕过。检索证据：`Select-String -Pattern "taskkill" installer.iss` 仅命中 `InitializeSetup` 一处且位于拷贝阶段之前；`Select-String -Pattern "EXIT_MAIN" src/update/mod.rs` 确认 `emit_protocol_line("EXIT_MAIN")`（`src/update/mod.rs:447`）发生在 `launch_installer` 之前，安装器启动瞬间子进程必然存活，恰好落在本次 taskkill 的匹配窗口内。

## 提案

把终止动作从“初始化即强杀”改为“准备安装时先礼后兵”，生产消费者为 Inno 安装流程的拷贝阶段与更新子进程的 `wait_main_instance_gone`（`src/update/mod.rs:482`，5 秒超时后才放行给安装器兜底）；非生产消费者无（`.iss` 无单测，验收靠人工与安装日志）。具体改动：删除或收缩 `InitializeSetup` 中的 taskkill 循环，将终止逻辑移到真正准备覆写文件的前一刻（如 `CurStepChanged(ssInstall)`），先走本系列 `01` 修复后的优雅退出入口（`--quit` 经看门狗），等待目标进程或单例互斥量消失（复用与 `MAIN_EXIT_WAIT_TIMEOUT_MS` 同量级的超时），仅在超时后对目标实例做有限次强制终止，并检查终止结果；收窄匹配（优先按窗口/互斥量/进程路径定位目标实例，避免裸 `/IM` 全杀），且更新子进程自身必须排除在击杀范围外。强杀保留为兜底而不是首选，文案保持中文。

## 明确不在本次范围

优雅退出入口本身的修复不在本次范围（另见 `01-stable-control-window`，本篇以前提为它已可用为前置条件，若它未合入本篇不得合入）；安装包哈希与流式改动不在本次范围（另见 `02`）；安装目录、注册表 `Run` 键、桌面快捷方式等安装语义一字不动；不引入安装器与主进程之间的新 IPC。

## 为什么不保留？

最强的反方是“强杀最可靠，优雅退出可能被用户取消或超时，安装时文件被占用会弹错误框，不如一杀了之”。逐条回应：可靠性恰恰是反对早杀的理由——早杀发生在用户尚未确认安装之前，把“打开看看”变成破坏性操作；`wait_main_instance_gone` 已有 5 秒超时加安装器兜底的设计，说明原设计本就把 taskkill 定位为兜底而非首选，现在的 `.iss` 与该设计自相矛盾；映像名全杀在单用户单实例下看似无害，但在更新子进程存活窗口内自杀协调者属于明确的误伤。第二个反方是“加优雅等待会拖慢安装”。回应：等待只发生在旧实例确实存活时，且上限秒级，相对整包下载与 UAC 提权可忽略；无残留时直接放行，不增加任何延迟。

## 验收标准

`Select-String -Pattern "taskkill" installer.iss` 不得再命中 `InitializeSetup` 函数体；拷贝阶段前必须有“优雅退出等待→超时才强杀”的顺序证据（函数名或注释可 grep）。`cargo test --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt -- --check` 保持全过（本篇不改 Rust，但门禁仍须跑）。人工验收四条：双击安装器后直接取消，任务栏小组件进程仍在；确认安装时旧实例托盘图标干净消失而非被强杀闪断；更新子进程发起的安装在 UAC 取消后能重拉起主程序（`relaunch_main_app` 路径）；连续两次“打开又取消”不留下半写安装状态。

## 风险

残留风险是优雅退出超时（如主窗口卡死不处理 `WM_CLOSE`）时安装仍会被阻塞到超时上限，用户观感是“点了安装没反应几秒”。缓解是超时上限保持秒级且安装器给出中文等待提示；若实机出现“取消安装后进程仍消失”，即判定本篇失败。另一个残留风险是收窄匹配后多开/改名实例漏杀导致文件占用弹框，届时以错误码与安装日志为准回退为兜底强杀，但不得回退到初始化即全杀。
