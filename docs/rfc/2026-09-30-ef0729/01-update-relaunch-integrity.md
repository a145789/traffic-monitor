# Agent Note：固定更新后实例的完整性等级——别让安装器的 [Run] 用管理员 token 拉起组件

Status: proposed

## 问题

自动更新成功后，把组件重新拉起来的是 `installer.iss:46-49` 的 `[Run]` 条目，而它的**身份**由 Inno 的 `postinstall` 语义决定，不由是否手写 `runasoriginaluser` 决定。

Inno 官方文档的 `[Run]` 章节写明：`runasoriginaluser` 是「the default behavior when the postinstall flag is used」，而 `runascurrentuser` 是「the default behavior when the postinstall flag is not used」。所以本仓库的 `[Run]` 虽然没写 `runasoriginaluser`，默认**本该**是"以原始（非提权）用户身份启动"。

真正出问题的是**启动方式**：`src/update/installer.rs:285-288` 用 `ShellExecuteExW` 且 `lpVerb = "runas"` 启动安装器——这正是资源管理器右键"以管理员身份运行"背后的同一个 shell 动词（`src/update/installer.rs:317-321` 专门接 `ERROR_CANCELLED` 也印证了作者预期走 UAC 提示这条路）。而同一页文档紧接着的 caveat 逐字命中这条路径：「If a user launches Setup by right-clicking its EXE file and selecting "Run as administrator", then this flag, unfortunately, will have no effect, because Setup has no opportunity to run any code with the original user credentials. The same is true if Setup is launched from an already-elevated process.」也就是：安装器进程从第一条指令起就已是提权态，不存在"pre-UAC 凭据"可交回，于是 `[Run]` 用自己的管理员 token 拉起组件。

上面引的两句要一起读：判定条件不是"谁点了什么菜单"，而是"Setup 有没有机会在 UAC 之前跑过代码"。第一轮命中的是第一句——更新子进程是主进程 re-exec 自身得到的**非提权**进程（`src/update/protocol.rs:74-93` 的 `current_exe` → `Command::new` → `spawn`），是它用 `runas` 动词向系统请求提权，安装器因此从第一条指令起就是提权态；**从第二轮起第二句也命中**：本条问题一旦发生过，常驻组件自己就是提权进程，它 spawn 的更新子进程随之提权（`CreateProcess` 继承 token），于是"Setup 从已提权进程启动"成立。这是个会自续的问题，不是一次性的窗口。

生产消费者：`src/update/mod.rs:501-505`（成功路径打完日志就结束，拉起的身份由安装器决定）、`src/update/installer.rs:257-275`（重试后仍用同一 `runas` 启动）、`installer.iss:46-49`。

非生产消费者：`src/smoke.rs:21-24` 的人工验收清单（只到"安装器正常启动"，没有任何一条检查"装完之后拉起来的实例是什么身份"）；`docs/archive/rfc/2026-09-24-64bb2b/03-update-child-lifecycle.md`（它记载了"不做安装器退出码回报"的决定，但与本条无关——本条不是退出码问题）。

组件一旦以 high IL 运行，UIPI 会切断它与 medium IL 世界之间所有需要窗口消息的交互，而本仓库的看门狗机制**全部**建立在窗口消息上：`src/main.rs:280` 注册的 `TaskbarCreated` 收不到 ⇒ Explorer 重建后组件永久消失，而 AGENTS.md「任务栏嵌入与 Explorer 重建」要求"必须保留一个不嵌入、隐藏且比主窗口更稳定的顶层恢复接收者"，这个接收者此刻也收不到广播；`src/main.rs:694-702` 的 `WM_SETTINGCHANGE` 收不到 ⇒ 主题自适应失效；用户手敲 `traffic-monitor.exe --quit` 时 `src/main.rs:167` 的 `PostMessageW` 被 UIPI 拦下，且返回值被 `let _ =` 丢弃 ⇒ 静默无效，随后 `src/main.rs:175-181` 的 5 秒等待轮询纯属空转。

注意范围：`build.rs:2-16` 的应用 manifest 没有 `requestedExecutionLevel`（即 `asInvoker`），所以组件**不会自己提权**；手动双击运行安装包走向导、由 SetupLdr 自提权的那条路径上，`runasoriginaluser` 应当正常工作。**本条只覆盖"更新器用 runas 动词启动安装器"这一条路径。**

## 提案

1. **先量后改（建议作为第一步）**：在组件启动早期读一次 `GetTokenInformation(TokenElevation)`，当命令行带 `RELAUNCHED_BY_UPDATE_ARG`（`src/config.rs:142`）时用 `log_event!` 记下"本次是否提权"（需用户开 `EnableDebugLog`，见 `src/util.rs:375-381`）。这条日志把本笔记的前提从"文档推断"变成"实测事实"，同时也给验收提供可观测点。
2. **应用侧自去提权**：若自检为"已提权 + 由更新拉起"，重新以非提权身份启动自己（带同一 `--relaunched-by-update`）然后退出。机制与时序都要写对：**不能**对自身直接 `ShellExecuteW("open", self)`——那会继承当前（提权）token；要经 explorer 中转（例如 `ShellExecuteW(None, "open", "explorer.exe", "<自身路径>", ...)`），让 medium IL 的 shell 以交互登录用户身份启动它。时序上必须**先 `drop` 单例互斥量守卫**（`src/main.rs:253-256` 的 `_mutex_guard`）再拉起，否则新实例会撞上 `src/main.rs:195-203` 的"重复实例静默退出"（那是刻意的设计决定，不能绕过），结果是"两个都退"、组件反而没了；先放锁再拉起会留下一个极小的第三方抢跑窗口（另一实例恰好在此刻启动），这个取舍必须明写在代码注释里。注意这条依赖"交互登录用户 = 目标用户"：标准用户 + 管理员凭据提升的场景下，经 explorer 中转能回到交互用户，而"以当前用户身份直接重启"不能。
3. **或：换掉 runas 动词**，让 Inno 的 SetupLdr 自己提权（stub 是 `asInvoker`，自提权时才有 pre-UAC 凭据）。代价明确：`src/update/installer.rs:317-321` 依赖的 `ERROR_CANCELLED` 检测会失效（默认动词下 UAC 取消发生在 stub 内部，`ShellExecuteExW` 会返回成功），因此选它之前必须先给出替代机制，而不是"顺手补一下"：默认动词下 stub 会自己提权、并**在安装结束后以 Inno 退出码收场**（0 = 成功；非 0 = 失败/取消/回滚），所以可行的替代是"让更新子进程持有 stub 的进程句柄并等它退出"——`SEE_MASK_NOCLOSEPROCESS` + `WaitForSingleObject` + `GetExitCodeProcess`（见 02-installer-abort-relaunch 的提案 1），它同时把"安装失败后谁拉起"一并解决，代价是更新子进程的寿命从"启动安装器即退"延长到"安装结束"，需要确认这与 AGENTS.md 第 4 条"短生命周期隔离边界"的兼容性。只换动词而不补等待：UAC 取消与安装失败都会变成"没人拉起"，比现状更糟。
4. 方案 2 与 3 **互斥**，择一实施；无论选哪个，都在 `installer.iss:47-49` 显式写出 `runasoriginaluser`——文档说它在 caveat 场景下无效，但写上不花钱，且能把"非 runas 启动"路径上的意图钉住。

## 明确不在本次范围

- **不要去掉 `postinstall`。** 官方文档对它唯一定义是"在 Setup Completed 向导页创建复选框"，它的另一个效果就是把默认身份定为 `runasoriginaluser`；去掉它会把默认翻成 `runascurrentuser`（安装器自己的管理员 token），正好加重本条问题。
- 不改 `installer.iss` 的 `AppMutex` / `RequestGracefulExit` / `ForceKillRemnant`（`installer.iss:5`、`:86-158`），它们与身份无关且已被归档笔记论证过。
- 不改 `src/update/installer.rs:231-252` 的 `relaunch_main_app` 语义（它服务失败路径，见另一篇笔记）。
- 不动 `build.rs` 的 manifest（组件自己不该提权；在上面加 `requireAdministrator` 是把 bug 变成设计）。

## 为什么不保留？

1. **"`[Run]` 没写 `runasoriginaluser` 才会提权"**——文档说它是 `postinstall` 的默认值，补 flag 是空操作；必须改的是启动方式或加自去提权。
2. **"提权只是第一次启动，用户重开就好"**——更新是自动发生的，用户不会知道要重开；而且看门狗失效是**静默**的，用户看到的只是"某天 Explorer 重启后组件没了"或"点退出没反应"，没有任何线索指向提权。
3. **"UIPI 只挡 SendMessage，广播不受影响"**——错。UIPI 按完整性等级过滤窗口消息，medium IL 进程（explorer.exe）的 `HWND_BROADCAST` 投递不到 high IL 进程的窗口；`PostMessageW` 则直接返回拒绝。
4. **"那就不做自动更新"**——代价过大，且本条有低风险修法（方案 2 只是换一次进程）。
5. **"组件提权反而更稳"**——恰恰相反：它依赖与 explorer 的消息往返，提权把它从"能对话"变成"只能单方面画窗口"。

## 验收标准

- `grep -n "runasoriginaluser" installer.iss` 命中 1 处；`grep -rn "runas" src/update/installer.rs` 的用法与所选方案一致（选方案 3 则不再出现 `to_wide("runas")`）。
- 真机实测（方案 2 或 3 都必须做）：完整走一次"发现新版本 → 点是 → 主程序退出 → 安装器静默装完"，用任务管理器的"已提升"列（或对组件进程 `whoami /groups` 查完整性级别）确认新实例是 **medium**，不是 high。
- 身份自检日志可 grep（`grep -n "提权\|TokenElevation" src/main.rs` 命中 1 处）。
- 现有回退路径不回归：`installer.iss` 的 UAC 取消仍由 `src/update/mod.rs:506-512` 重新拉起主程序；`src/smoke.rs` 的 4 个 `#[ignore]` 冒烟用例仍可跑通。
- `cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全绿。

## 风险

- 方案 1 是纯观测，无风险；但它依赖用户开日志，"没开日志就等于没验"，所以验收必须在开日志的机器上做一次。
- 方案 2 引入一次进程更替：需要处理"新实例起的瞬间旧实例还没退、单例互斥量已存在"的窗口（应当先退出再拉起，或让新实例的互斥量等待逻辑兜住），并接受托盘图标有一次重建。去提权依赖 explorer 在运行（任务栏组件本来就依赖它）。
- 方案 3 的 UAC 取消兜底如上所述必须同批替换，否则是净退步。
- **可证伪项**：若实测发现组件在 runas 启动的安装器下**仍是** medium IL，则本条前提被证伪（说明 Inno 在该路径上仍能拿到原始用户 token），此时只保留"显式写 `runasoriginaluser`"与那条自检日志即可，不做方案 2/3。
