# Agent Note：固定更新后实例的完整性等级——别让安装器的 [Run] 用管理员 token 拉起组件

Status: proposed

## 问题

（「问题」「明确不在本次范围」「为什么不保留」三节是改动前的现场记录，行号相对改动前的代码；本次改动的规范见"提案"。）

自动更新成功后，把组件重新拉起来的是 `installer.iss:46-49` 的 `[Run]` 条目，而它的**身份**由 Inno 的 `postinstall` 语义决定，不由是否手写 `runasoriginaluser` 决定。

Inno 官方文档的 `[Run]` 章节写明：`runasoriginaluser` 是「the default behavior when the postinstall flag is used」，而 `runascurrentuser` 是「the default behavior when the postinstall flag is not used」。所以本仓库的 `[Run]` 虽然没写 `runasoriginaluser`，默认**本该**是"以原始（非提权）用户身份启动"。

真正出问题的是**启动方式**：`src/update/installer.rs:285-288` 用 `ShellExecuteExW` 且 `lpVerb = "runas"` 启动安装器——这正是资源管理器右键"以管理员身份运行"背后的同一个 shell 动词（`src/update/installer.rs:317-321` 专门接 `ERROR_CANCELLED` 也印证了作者预期走 UAC 提示这条路）。而同一页文档紧接着的 caveat 逐字命中这条路径：「If a user launches Setup by right-clicking its EXE file and selecting "Run as administrator", then this flag, unfortunately, will have no effect, because Setup has no opportunity to run any code with the original user credentials. The same is true if Setup is launched from an already-elevated process.」也就是：安装器进程从第一条指令起就已是提权态，不存在"pre-UAC 凭据"可交回，于是 `[Run]` 用自己的管理员 token 拉起组件。

上面引的两句要一起读：判定条件不是"谁点了什么菜单"，而是"Setup 有没有机会在 UAC 之前跑过代码"。第一轮命中的是第一句——更新子进程是主进程 re-exec 自身得到的**非提权**进程（`src/update/protocol.rs:74-93` 的 `current_exe` → `Command::new` → `spawn`），是它用 `runas` 动词向系统请求提权，安装器因此从第一条指令起就是提权态；**从第二轮起第二句也命中**：本条问题一旦发生过，常驻组件自己就是提权进程，它 spawn 的更新子进程随之提权（`CreateProcess` 继承 token），于是"Setup 从已提权进程启动"成立。这是个会自续的问题，不是一次性的窗口。

生产消费者：`src/update/mod.rs:501-505`（成功路径打完日志就结束，拉起的身份由安装器决定）、`src/update/installer.rs:257-275`（重试后仍用同一 `runas` 启动）、`installer.iss:46-49`。

非生产消费者：`src/smoke.rs:21-24` 的人工验收清单（只到"安装器正常启动"，没有任何一条检查"装完之后拉起来的实例是什么身份"）；`docs/archive/rfc/2026-09-24-64bb2b/03-update-child-lifecycle.md`（它记载了"不做安装器退出码回报"的决定，但与本条无关——本条不是退出码问题）。

组件一旦以 high IL 运行，UIPI 会切断它与 medium IL 世界之间所有需要窗口消息的交互，而本仓库的看门狗机制**全部**建立在窗口消息上：`src/main.rs:280` 注册的 `TaskbarCreated` 收不到 ⇒ Explorer 重建后组件永久消失，而 AGENTS.md「任务栏嵌入与 Explorer 重建」要求"必须保留一个不嵌入、隐藏且比主窗口更稳定的顶层恢复接收者"，这个接收者此刻也收不到广播；`src/main.rs:694-702` 的 `WM_SETTINGCHANGE` 收不到 ⇒ 主题自适应失效；用户手敲 `traffic-monitor.exe --quit` 时 `src/main.rs:167` 的 `PostMessageW` 被 UIPI 拦下，且返回值被 `let _ =` 丢弃 ⇒ 静默无效，随后 `src/main.rs:175-181` 的 5 秒等待轮询纯属空转。

注意范围：`build.rs:2-16` 的应用 manifest 没有 `requestedExecutionLevel`（即 `asInvoker`），所以组件**不会自己提权**；手动双击运行安装包走向导、由 SetupLdr 自提权的那条路径上，`runasoriginaluser` 应当正常工作。**本条只覆盖"更新器用 runas 动词启动安装器"这一条路径。**

## 提案

1. **身份自检日志（已实施）**：组件启动早期读一次 `GetTokenInformation(TokenElevation)`（唯一实现：`util::current_process_is_elevated`），已提权时把结论用 `log_event!` 记下（需用户开 `EnableDebugLog`，见 `src/util.rs`）。这条日志是验收的可观测点。
   **刻意不以 `RELAUNCHED_BY_UPDATE_ARG` 为门**：`installer.iss` 的 `[Run]` 条目不带任何参数，被它拉起的提权实例身上没有这个标记——按标记判定会正好漏掉需要修复的那一批机器。因此自检无条件做，标记只用于「由更新拉起且未提权」这一条正向证据。
2. **应用侧自去提权（已实施）**：自检为已提权就经 explorer 中转重新以非提权身份启动自己，然后退出。机制与时序都要写对：
   - **不能**对自身直接 `ShellExecuteW("open", self)`——那会继承当前（提权）token；要经 explorer 中转（`ShellExecuteW(None, "open", "explorer.exe", "<自身路径>", ...)`），让 medium IL 的 shell 以交互登录用户身份启动它。这条依赖"交互登录用户 = 目标用户"：标准用户 + 管理员凭据提升的场景下，经 explorer 中转能回到交互用户，而"以当前用户身份直接重启"不能。
   - **中转只在「确定有未提权的 shell 可托付」时做**：任务栏窗口存在、且承载它的进程未提权，两个条件任一不满足就维持现状。这不只是保守：explorer 中转靠正在运行的 shell 代为启动，若那个 shell 自己就是提权的，中转不会降级，替代实例会继续以提权身份启动并再次触发自检——该判据从结构上排除了这条进程创建循环。
   - **调用点在单例锁之前**（`main()` 里早于 `init_single_instance`），比原笔记的"先 drop 单例互斥量守卫再拉起"更简单：那时本进程还没持有互斥量，替代实例可以直接拿到锁，"先拉起后放锁会让新实例撞上重复实例静默退出"这个坑在结构上不存在，也就不需要任何一次性参数来防重入。
   - **退出前确认替代实例真的接管**：`ShellExecuteW` 的返回值只说明 shell 接受了请求，不代表目标起来了；以「同名单例互斥量出现」为准（超时 5 秒，与另两处等待共用常量）。等不到就**继续以提权身份运行**，下次启动再试——所以"为了去提权把组件弄丢"在结构上不可能发生。
   - 代价：explorer 的命令行不把参数转交给目标程序，替代实例不带 `--relaunched-by-update`，这一次"推迟首个自动检查周期"的优惠会丢失；影响面仅限提权实例去提权的这一次启动。
3. **换掉 `runas` 动词（已实施）**：`update::installer::try_launch_installer` 改用默认动词（`lpVerb = NULL`）启动安装器，让 Inno 的 SetupLdr 自己提权（stub 的应用 manifest 是 `asInvoker`，`installer.iss` 未设 `PrivilegesRequired`、取默认 `admin`），于是它先以原始凭据跑过一段代码，`[Run]` 的 `runasoriginaluser` 才真正生效。代价明确：`ERROR_CANCELLED` 不再由 `ShellExecuteExW` 同步返回，替代机制正是 02 的提案 1（取进程句柄、等收场、读退出码），两篇同批实施，因此不存在"只换动词不补等待"的净退步。附带好处：拿到的句柄因此属于同一完整性等级的进程，省掉了跨等级等待与取退出码这一项不确定性。
4. **方案 2 与 3 一起实施（修正原文的"互斥，择一实施"）**：原笔记认为两者解决同一个问题、择一即可；但两者覆盖面不同、缺一不可——方案 3 只**防复发**（新路径不再把组件拉成提权身份），修不了**已经**被旧版 `runas` 安装器拉起的提权实例：那次交接跑的是旧代码，它落地的下一版组件必然是提权的，而旧 `[Run]` 又不传任何参数，方案 2 若按标记判定同样认不出它（见提案 1）。两者互不冲突：非提权路径下方案 2 是空操作，提权路径下"方案 3 拿不到原始凭据"这个洞由方案 2 兜住。
5. 无论走哪条路径，都在 `installer.iss` 的 `[Run]` 条目显式写出 `runasoriginaluser`——文档说它在 caveat 场景下无效，但写上不花钱，且能把"非 runas 启动"路径上的意图钉住。

## 明确不在本次范围

- **不要去掉 `postinstall`。** 官方文档对它唯一定义是"在 Setup Completed 向导页创建复选框"，它的另一个效果就是把默认身份定为 `runasoriginaluser`；去掉它会把默认翻成 `runascurrentuser`（安装器自己的管理员 token），正好加重本条问题。
- 不改 `installer.iss` 的 `AppMutex` / `RequestGracefulExit` / `ForceKillRemnant`（`installer.iss:5`、`:86-158`），它们与身份无关且已被归档笔记论证过。
- 不改 `update::installer::relaunch_main_app` 的语义（它服务安装交接结束后的重新拉起，见另一篇笔记）；自去提权是组件启动期自己做的事，不复用也不改它。
- 不动 `build.rs` 的 manifest（组件自己不该提权；在上面加 `requireAdministrator` 是把 bug 变成设计）。

## 为什么不保留？

1. **"`[Run]` 没写 `runasoriginaluser` 才会提权"**——文档说它是 `postinstall` 的默认值，补 flag 是空操作；必须改的是启动方式或加自去提权。
2. **"提权只是第一次启动，用户重开就好"**——更新是自动发生的，用户不会知道要重开；而且看门狗失效是**静默**的，用户看到的只是"某天 Explorer 重启后组件没了"或"点退出没反应"，没有任何线索指向提权。
3. **"UIPI 只挡 SendMessage，广播不受影响"**——错。UIPI 按完整性等级过滤窗口消息，medium IL 进程（explorer.exe）的 `HWND_BROADCAST` 投递不到 high IL 进程的窗口；`PostMessageW` 则直接返回拒绝。
4. **"那就不做自动更新"**——代价过大，且本条有低风险修法（方案 2 只是换一次进程）。
5. **"组件提权反而更稳"**——恰恰相反：它依赖与 explorer 的消息往返，提权把它从"能对话"变成"只能单方面画窗口"。

## 验收标准

- `grep -n "Flags:.*runasoriginaluser" installer.iss` 命中 1 次；`src/update/installer.rs` 的启动路径不再把 `lpVerb` 指向 `runas` 动词（现为 `lpVerb: PCWSTR::null()`，`runas` 只出现在解释理由的注释里）。
- `grep -n "提权" src/main.rs` 命中（自检与自去提权的判据、日志）；令牌读取的唯一实现在 `src/util.rs`（`current_process_is_elevated` / `window_process_is_elevated`）。
- **真机实测（本次交付未做，见风险）**：完整走一次"发现新版本 → 点是 → 主程序退出 → 安装器静默装完"，确认 ①组件自动回到任务栏 ②新实例是 **medium**（任务管理器"已提升"列，或对组件进程 `whoami /groups` 查完整性级别）。
- **真机实测（本次交付未做）**：从一个已被提权的组件出发（右击 exe"以管理员身份运行"，或从上一次提权状态出发），确认它经 explorer 中转后回到 medium，任务栏组件仍在（不是"两个都退"，也没有反复创建进程）。
- 现有回退路径不回归：安装器启动失败/UAC 取消仍重新拉起主程序；`src/smoke.rs` 的 4 个 `#[ignore]` 冒烟用例仍可跑通。
- `cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全绿。

## 风险

- 提案 1 是纯观测，无风险；但它依赖用户开日志，"没开日志就等于没验"，所以验收必须在开日志的机器上做一次。
- 提案 2 引入一次进程更替：托盘图标会有一次重建；`--relaunched-by-update` 在中转这一跳丢失（影响面见提案 2）。去提权依赖 explorer 在运行——中转判据显式要求"任务栏窗口存在且其进程未提权"，不满足就维持现状，不会把组件弄丢。
- 中转期间本进程尚未持有单例锁：那个窗口里第三方启动的实例可能抢到锁，本进程随后据"互斥量已出现"让位退出，因此不会出现两个常驻实例；代价是这次自去提权没生效（下次启动再试）。
- **未实测项（交付时的事实）**：explorer 中转是否在目标 Windows 11 版本上都落成 medium IL、"等替代实例接管"的 5 秒上限在冷启动 + 杀软扫描下是否够用、"旧 `[Run]` 拉起的提权实例能否被无条件自检修复"这整条首次升级路径，都只做过静态推理，需真机各验一次。
- **可证伪项**：若实测发现组件在 `runas` 启动的安装器下**仍是** medium IL，则提案 3 的前提被证伪（说明 Inno 在该路径上仍能拿到原始用户 token）。此时提案 3 退化为冗余，但提案 2 仍应保留——它防的是"任何原因导致的提权启动"，不只 `runas` 这一条。
