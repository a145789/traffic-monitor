# Agent Note：让收尾拉起不再依赖交接副本的存活

Status: rejected（复核裁决：目标场景需「副本被杀+安装未收场」双重稀有事件，且范围内改动经复核不覆盖该场景，ROI 不足，不实施）

## 问题

「安装收场后要不要把组件拉回来」这个职责，现在完全落在**一个可以被外部杀死的短命进程**上。链条是：协调者发 `EXIT_MAIN`、主程序退出、等单例互斥量消失，然后把整套收尾逻辑移交给 `%LOCALAPPDATA%` 里的自身副本（`src/update/mod.rs:585-624`，移交点 `:613`）；「谁负责拉起」的裁决随移交一起搬进副本，实际执行在 `src/update/installer.rs:478-521`。

副本的生命周期由 `wait_for_exit_code` 决定，它可以停留到 `INSTALLER_EXIT_WAIT_TIMEOUT_MS`（`src/config.rs:168-175`，30 分钟），期间是一个无窗口的 `CREATE_NO_WINDOW` 进程（`src/update/installer.rs:424`）。它若在这段时间里被任务管理器结束、被杀软隔离、或自身异常终止，而安装又**未成功收尾**，组件就不会回来：`installer.iss:57-59` 的 `[Run]` 只在 Setup 成功收尾时执行（`src/update/installer.rs:63-68` 的注释逐字说明了这一点），而主程序此时已按 `EXIT_MAIN` 退出（`src/update/mod.rs:585-605`）。

现有兜底只有「下次登录的自启项」，而且它还不是必然存在——它取决于用户当初是否勾选了安装器的 `startup` 任务（`installer.iss:39-44`）。`install_handoff_main`（`src/update/mod.rs:410-494`）只覆盖「载荷残缺」与「安装包重验失败」两条失败路径，没有覆盖「副本自身被杀」。全仓检索 `main_instance_exists` 命中 3 处（`src/util.rs:224` 定义、`:253` 轮询、`src/main.rs:266` 自去提权判重），没有一处服务于这条兜底。

## 提案

本次实施 **B 与 C**，A 留作独立后续项（理由见「明确不在本次范围」）。三条各自的取舍：

- **A. 把「收尾后是否要拉起」移出副本的存活期。** 副本在启动安装器**之前**把补拉起目标写进安装器读得到的位置（例如 `HKLM\Software\Traffic Monitor` 下的 `PendingRelaunch`），`installer.iss` 的 `[Code]` 在收尾阶段（`CurStepChanged(ssDone)` 或 `DeinitializeSetup`）读该值、按 `main_instance_exists` 等价判据决定是否 `Exec` 组件，然后清除该值。这样「拉起」不再取决于副本是否活着，只剩「副本在启动安装器之前就死」这一小窗口。注意本仓现有注册表辅助函数只支持 `HKCU`（`src/util.rs:570-621` 全部走 `CURRENT_USER`，见 `src/util.rs:19`），HKLM 需要新增一对读写函数；用 `HKCU` 的话，提权安装器会读到提权账户的 hive，多用户场景有歧义，故这里选 HKLM。
- **B. 缩短单点暴露窗口。** `InstallerProcess::wait_for_exit_code`（`src/update/installer.rs:79-91`）现在是一次 `WaitForSingleObject(30min)`；改为分段等待（每段 N 秒），每段之后用 `main_instance_exists()`（`src/util.rs:224-239`，与 `src/update/installer.rs:482-486` 的接管判据同源）判断组件是否已经回来，一旦在跑就提前退出、尽快归还更新互斥量。分段轮询在本仓已有同款实现可参照（`src/util.rs:250-261`）。
- **C. 把这条残留记账。** 在 `src/smoke.rs:22-27` 的人工清单里补一条「交接副本在等待窗口内被结束后，安装失败场景组件是否回来」，避免它以「偶发、无法复现」的形态长期无主。

## 明确不在本次范围

- 不引入常驻看护进程或服务：与「主进程不得为更新常驻」（AGENTS.md 不变量 1）及「协调者必须让出 exe 映像」（`src/update/installer.rs:6-12` 的模块头）两条冲突。
- **本次不做 A**（把收尾拉起移交给安装器）：它需要在 `installer.iss` 新增 `[Code]` 分支与一对 HKLM 读写（本仓现有注册表辅助函数只支持 HKCU，`src/util.rs:570-621`），且**只对新版安装器生效**；它的收益（副本已死时仍能拉起）已被 B（提前退出、缩短暴露窗口）与安装器原生 `[Run]`（`installer.iss:57-59`）覆盖大半。留作独立后续项，待 B 上线并真机观察后重新评估。
- 不改 `EXIT_MAIN` 的语义与主进程退出时序（`src/update/mod.rs:364-368`）。
- 不改 `[Run]` 的 `nowait postinstall runasoriginaluser`（`installer.iss:57-59`）：它的身份语义是既有裁定（见 `installer.iss:46-59` 的注释）。
- 不用计划任务/服务做兜底：超出本产品形态。
- 不改安装器入口处的优雅退出与强杀链（`installer.iss:102-165`）：那是「安装前清旧实例」，与本节「安装后拉新实例」是两回事。

## 为什么不保留？

1. 「副本被杀是用户或杀软的显式动作，不该为它设计。」—— 代价不对称：这一支的后果是「组件消失到下次登录」，而 A 的成本是一个注册表值加 `[Code]` 十几行；B 的成本是把一次等待改成轮询。
2. 「A 会让安装器承担拉起职责，破坏『拉起判据是单例互斥量』的既有设计。」—— A 只在副本缺席时兜底，判据仍是单例互斥量（`main_instance_exists`）；安装成功时 `[Run]` 已拉起组件，判据为真即跳过，不会重复拉起（与 `src/update/installer.rs:482-489` 的判据同源）。
3. 「B 会把 30 分钟上界架空。」—— 分段只改变等待方式，deadline 不变；提前退出只发生在「组件已经在跑」这一确定事实下。
4. 「副本很稳定，没必要。」—— 稳定性不是设计依据。本节真正的问题是「拉起责任与一个可被外部杀死的进程同生共死」，与本仓「恢复能力不得依赖外部事件次数」的取向同类，属结构性缺陷而非概率问题。

## 验收标准

- A：`grep -n "PendingRelaunch" src installer.iss` 命中 3 处（写入、读取、清除各 1）；真机做一次「安装器启动后立即用任务管理器结束交接副本」，断言 ①安装失败场景组件仍回来 ②安装成功场景不出现双实例。
- B：`grep -n "main_instance_exists" src/update/installer.rs` 命中 ≥1 处；新增一条用例覆盖分段等待的 deadline 推进（可参照 `src/update/installer.rs:733-746` 用真实进程钉 API 路径的写法）。
- C：`src/smoke.rs` 人工清单含该条。
- 四条门禁全绿；`installer.iss` 的改动需在真有 ISCC 的环境编译通过（本机无 ISCC，是已知验证缺口）。

## 风险

- A 需要安装器读写 HKLM，而安装器当前只用 `HKCU`（`installer.iss:43-44`）；把「机器级瞬态状态」放进注册表必须明确键位与清理时机，否则会留下永久残留。
- A 只对新版安装器生效：已装机上的旧安装器没有这段 `[Code]`，因此升级到含本项的版本之后才开始受保护（与 v1.7.3 那次「安装器侧一层救已装机」的双层结构同类）。
- B 的分段轮询若间隔过小会引入无谓唤醒；建议复用 `MAIN_EXIT_POLL_INTERVAL_MS`（`src/config.rs:154`）量级的间隔，并在真机确认 CPU 占用无变化。
- 未覆盖：副本被杀的触发面（杀软隔离策略、任务管理器结束）本机无法构造，以上为机制推理加代码事实；A 的 `[Code]` 分支也无法在本机编译验证。
