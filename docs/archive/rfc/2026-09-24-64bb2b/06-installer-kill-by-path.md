# Agent Note：安装器兜底强杀收窄到「目标路径 + 当前会话 + 可识别命令行」

Status: implemented

## 问题

（本段三处行号 `installer.iss:118-135`、`:124-127`、`:112-117` 指向本 note **实施前**的内联实现；实施后该内联筛选已外置为 `installer/kill-remnant.ps1`，本段保留作为缺陷记录，不再指向现行代码。）

`installer.iss:118-135` 的 `ForceKillRemnant` 以管理员身份执行 PowerShell 兜底强杀，筛选条件是 `$_.Name -eq 'traffic-monitor.exe' -and $_.CommandLine -notlike '*--check-update*'`（`installer.iss:124-127`），没有任何**完整路径 / 用户 / Session** 约束。`installer.iss:112-117` 的注释声称「不用映像名全杀以免误伤同名进程」——但按 `Name` 过滤仍然覆盖**全机器所有叫这个名字的进程**，只是从「按映像名批量杀」收窄成「逐个 PID 杀同名的」，误伤面并没有真正收窄到「本次要升级的那个安装」之外。

这条兜底为什么会被触发、触发时覆盖面有多大：`RequestGracefulExit`（`installer.iss:86-96`）先用 `--quit` 请旧实例自己退，`WaitForSingleInstanceGone`（`installer.iss:100-116`）轮询单例互斥量 `TrafficMonitor_Mutex_Instance`（`installer.iss:107`），超过 `GracefulWaitTimeoutMs` 才进入 `ForceKillRemnant`。而该互斥量名没有 `Global\` 前缀（`src/config.rs:26` 是裸名），即它是**会话级**对象：另一个用户 Session 或另一份便携副本可以在同名进程被互斥量挡不住的情况下并存，然后被这次强杀命中。安装器此时是提权进程，`taskkill /F /PID` 没有会话边界，因此这不是「理论上可能」而是执行权限允许的行为。

**旧稿的三个落地缺陷**（本稿据此收紧）：其一，验收场景 A 写成「在另一个目录放一份同名 exe 并运行它」——同一会话里那份副本会被单例互斥量直接挡掉退出（`src/config.rs:26`），根本构造不出「两个同名进程并存」，这条验收永远测不了；其二，只加路径比较仍不阻止**同路径、异会话**的进程被杀，而 note 的目标是「少杀」，所以必须连会话一起约束；其三，`$_.CommandLine` 为 `$null` 时 `-notlike` 的结果是「不排除」（PowerShell 把 `$null` 当空串比较），方向恰好是危险的「照样杀」，而路径比较一加上，更新协调者进程（`--check-update` 是安装目录下同一个 exe 的 re-exec）反而会被路径匹配命中，命令行排除从此变成**必需**而不是可选。

`installer.iss:95` 的 `Exec(..., ewWaitUntilTerminated, ResultCode)` 没有外层超时，这条被记录但**不需要修**：它等的是被拉起的 `--quit` 助手进程，而该助手自身有上限——`src/main.rs:223-250` 的 `quit_existing_instance()` 只轮询 `MAIN_EXIT_WAIT_TIMEOUT_MS`（5 秒）后必然返回并退出。

生产消费者：`installer.iss:152-158` 在 `ssInstall` 前调用；非生产消费者：`installer.iss` 自身无测试、本仓库也没有对 iss 的静态检查，这正是下面要外置出可测筛选器的原因；外置出的 `installer/kill-remnant.ps1` 由 `src/update/installer.rs` 的 `test_kill_remnant_selector_scope` 覆盖。

## 提案

1. **把筛选逻辑从内联字符串外置成可测的 `.ps1`**，用 `[Files]` 的 `Flags: dontcopy` 打进安装包、运行时 `ExtractTemporaryFile` 到 `{tmp}`，再用 `Exec(ExpandConstant('{cmd}'), '/C powershell -NoProfile -ExecutionPolicy Bypass -File "<tmp>\kill-remnant.ps1" -Expected ' + AddQuotes(ExpandConstant('{app}\traffic-monitor.exe')), ...)` 调用。这一步同时解决两个问题：路径不再拼进 PowerShell 源码（消灭引号、`%FOO%`、中文、8.3 短路径多层转义这一类缺陷），筛选逻辑可以被离线喂数据测试。
2. **筛选条件改成三个必要条件**（全部满足才终止，路径取不到一律跳过）：
   - `$_.ExecutablePath -eq $Expected`（`$Expected` 由 `{app}` 派生，即本次正在升级的那个安装目录）；
   - `$_.SessionId -eq $CurrentSession`（`$CurrentSession = (Get-Process -Id $PID).SessionId`，即安装器所在会话，挡掉异会话同路径进程）；
   - `$_.CommandLine -ne $null -and $_.CommandLine -notlike '*--check-update*'`（命令行取不到时**不杀**：保守方向；更新协调者必须被排除，因为它是同目录同路径的 re-exec）。
3. **脚本支持 `-Processes <object[]>` 与 `-DryRun`**：`-Processes` 缺省时才走 `Get-CimInstance Win32_Process`，`-DryRun` 只打印「将终止哪些 PID」而不执行 `taskkill`。这让验收可以喂构造的进程对象（含 `ExecutablePath` 为 `$null`、`CommandLine` 为 `$null`、异会话三个边界），不再依赖「真的能进入强杀分支」。
4. **生产调用路径仍走真杀**：`-DryRun` 只在验收/演练时使用；`installer.iss:139-145` 的 `ForceKillMaxAttempts` 循环与「每轮后复查互斥量」保持不变。
5. 把 `installer.iss:118-126` 的注释改准：说明按名筛选仍覆盖全机器同名进程，真正的收窄来自「路径 + 会话 + 命令行」三项；并写明「本 note 只做少杀，不做跨会话清理」。

## 明确不在本次范围

- 不改 `RequestGracefulExit` 的 `ewWaitUntilTerminated`（等待对象是短命助手，见上；加外层超时需另起线程或改 Inno 脚本结构，收益不成立）。
- 不删除 `ForceKillRemnant` 兜底本身：`--quit` 在旧版 exe 损坏、或进程卡在模态路径时可能无效，没有兜底会直接失败在文件占用上。
- 不改 `installer.iss:148-158` 的 `ssInstall` 时序与 `AppMutex` 声明（`installer.iss:5`）。
- **不引入跨会话清理逻辑**：异会话的残留进程占着安装目录文件时，退回安装器原生的「文件被占用」提示，属预期行为而非回归。本 note 的会话约束是「少杀」，不是「杀得更全」。
- 不改 `MUTEX_NAME`（`src/config.rs:26`）：把它升成 `Global\` 会让异会话的实例互相阻塞退出，超出本 note 范围。

## 为什么不保留？

1. 「同一个会话里单例 Mutex 已经保证只有一个实例，按名杀等价于按路径杀」——不成立：互斥量是会话级对象，多用户 / RDP / 另起会话的便携副本都不受它约束；而且**路径比较一加上，同目录的更新协调者进程就会命中**，命令行排除从可选变成必需。
2. 「用户自己在跑安装器，误杀自己的副本是可以接受的」——一个提权进程按名字跨会话终止进程不该是默认行为；加路径 + 会话比较的成本是脚本里两个 `-and`。
3. 「路径比较会漏杀真正需要杀的那个进程」——不会：要杀的目标就是安装目录下、当前会话里、命令行不是 `--check-update` 的那个 exe，三个条件恰好是最精确的匹配；漏杀的唯一情形是 `ExecutablePath` 或 `CommandLine` 取不到，此时退回原生文件占用提示，属于可接受的保守失败。
4. 「外置 .ps1 是为了可测性改结构」——顺带收益更大：现在这条逻辑连一次真实执行都没有验收（旧稿的「编译过就算」正是被点名的缺陷），外置后可以用构造数据覆盖空路径、空命令行、异会话三个边界，这是把「不敢改的脚本」变成「能验证的脚本」。

## 验收标准

- `grep -n "ExecutablePath" installer.iss` 命中 0 处（路径比较已移入 `.ps1`）；`grep -n "ExecutablePath" installer/kill-remnant.ps1` 命中 1 处，且比较对象是 `-Expected` 参数。
- `grep -n "SessionId" installer/kill-remnant.ps1` 命中 2 处：1 处取安装器自身会话（`(Get-Process -Id $PID).SessionId`），1 处比较候选进程；比较本身只有后一处（原文写「命中 1 处」与同一节的提案自相矛盾——取会话与比会话各需一次，无法压到 1 处）。`grep -n "Name -eq" installer/kill-remnant.ps1` 不得作为**唯一**筛选条件（可保留为附加条件；本次实现完全不用它，故命中 0 处）。
- **确定性筛选器验收（新增，替代旧稿的场景 A）**：直接运行脚本的 `-Processes` + `-DryRun` 模式，喂入构造记录——(a) 目标路径 + 当前会话 + 正常命令行 ⇒ 应被列出；(b) 目标路径 + 当前会话 + 含 `--check-update` ⇒ 不应列出；(c) 目标路径 + **其它会话** ⇒ 不应列出；(d) 目标路径 + 当前会话 + `CommandLine = $null` ⇒ 不应列出；(e) 另一目录同名 exe ⇒ 不应列出；(f) `ExecutablePath = $null`（`提案` 3 明列的边界，原验收漏列）⇒ 不应列出。本仓库唯一进 CI 的执行体是 `cargo test`，故该验收落在 `src/update/installer.rs` 的 `test_kill_remnant_selector_scope`（用 `powershell` 驱动脚本，断言只看 PID 数字，不依赖控制台代码页）。
- 端到端 A（跨会话）：通过 RDP 或在第二个用户会话里启动**同目录**的一份 `traffic-monitor.exe`，再在当前会话跑安装器并进入强杀分支 ⇒ 该异会话进程存活。
- 端到端 B（路径隔离）：用一份临时把 `MUTEX_NAME`（`src/config.rs:26`）改名后构建的 scratch exe，放在**另一个目录**并在当前会话运行 ⇒ 单例锁不再互相阻挡，两个同名进程可并存，安装器强杀分支只终止安装目录里的那一个。
- 端到端 C（回归保护）：正常升级安装版，确认安装目录下的旧进程仍被正确终止、升级成功。**必须真跑一次**（旧稿只要求「编译出安装包」是明确的缺口），并在真跑后用 `Get-CimInstance Win32_Process` 复核没有留下孤儿进程。
- 本地 `bun scripts/package.ts` 能编译出安装包，且 `{tmp}` 下的 `.ps1` 在运行时确实被释放（在脚本首行写一行 `log` 或在 `-DryRun` 下打印自身路径以确认）。

## 风险

- `Get-CimInstance Win32_Process` 与随后的 `taskkill /F /PID` 之间仍存在 PID 复用窗口（TOCTOU）：路径与会话校验发生在快照时刻，终止发生在之后。窗口是毫秒级且目标已收窄到单一路径，属**已知残留**；彻底闭合需要在 PowerShell 里 `OpenProcess` + `TerminateProcess`（把句柄校验与终止合成一步），代价与复杂度不成比例，本次不做，实施时在脚本注释里写明这个取舍。
- `AddQuotes`/`-File` 传参在路径含空格、中文、`&` 等字符时的行为必须用真实安装目录验证一次（把包装到 `C:\Program Files\道路 Monitor\` 这类路径下跑端到端 C）；这是本 note 唯一无法用构造数据覆盖的部分。
- `.ps1` 从 `{tmp}` 释放后执行，可能被杀软拦截或受 `-ExecutionPolicy` 策略影响（已用 `Bypass`）；若企业环境禁止脚本执行，后果是强杀分支失效并退回文件占用提示——方向安全（少杀），但需要在实施说明里记录，不能当作「一定生效」。
- 会话约束会让「安装器在会话 0 / 提权后会话归属变化」的环境下漏杀（例如某些远程管理工具以服务方式拉起安装器）；届时表现为文件占用提示而非误杀。这是刻意选择的方向。
