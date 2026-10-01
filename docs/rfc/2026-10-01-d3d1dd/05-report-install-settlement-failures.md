# Agent Note：把安装未收场的失败报告给用户

Status: proposed

## 问题

安装交接的收场判定在 `src/update/installer.rs:478-521` 的 `run_install_handoff`。它共有 7 个去向，其中只有**启动失败且带错误码**这一支给用户可见提示（`:511` 的 `show_error`，也是全文件唯一一处调用）：

- 安装成功且组件已在跑：`:487-489` 静默（正确，不该提示）；
- 安装成功但组件未在跑：`:491` 只 `log_event!`；
- 退出码非 0：`:492-494` 只 `log_event!`，随后 `relaunch_main_app_at(app_exe)`（`:497`）；
- 退出码取不到（等待超时/取码失败）：`:495` 只写日志；
- `InstallerLaunch::Cancelled`：`:500-506` 只写日志；
- `InstallerLaunch::Failed(code)`：`:507-512` 先拉起、再 `show_error`（`:511`）；
- `InstallerLaunch::FailedWithoutCode`：`:513-519` 只写日志（本提案有意不为它加提示：取不到码时弹「错误码: 0」没有信息量，理由见 `:514-516` 的既有注释）。

而**用户最可能遇到的那两类失败恰好落在只写日志的一侧**：默认动词下 UAC 取消不再由 `ShellExecuteExW` 同步返回 `ERROR_CANCELLED`，而是表现为安装器的非 0 退出码（`src/update/installer.rs:580-582` 的注释逐字说明了这次语义迁移；一手佐证：仓库内构建产物 `Output\TrafficMonitor-Setup-1.5.0-devtlswl5.exe` 内嵌 manifest 实测为 `requestedExecutionLevel level="asInvoker"`、`assemblyIdentity name="JR.Inno.Setup"`——asInvoker 的 stub 在进程创建阶段不触发 UAC，本次 `ShellExecuteExW` 拿不到 `ERROR_CANCELLED`），静默回滚同样表现为非 0 退出码。「非 0 ⇒ Setup 没跑完」有官方兜底句（Inno 退出码表末句逐字：Any non-zero exit code indicates that Setup was not run to completion），具体码值官方未列明（社区证据为 2），本提案只依赖「非 0」。

后果：用户点了「是」→ 主程序按 `EXIT_MAIN` 退出（`src/update/mod.rs:585-605`）→ 安装没装成 → 组件被悄悄拉回来，**全程没有任何可见提示**。唯一线索是用户自己开过 `EnableDebugLog`（`src/config.rs:54`）才能看到的 `%LOCALAPPDATA%\Traffic Monitor\debug.log`（拼路径见 `src/util.rs:506-515`）。`src/smoke.rs:21-29` 的人工清单里也只有「UAC 取消后主程序被重新拉起」（`:25-26`），没有「失败时有可见提示」。

对照：安装包重验失败那条路径是**有**提示的（`src/update/mod.rs:447-458`，`show_error` 在 `:456`），副本启动失败也有（`src/update/mod.rs:613-624` 的 `Err` 分支，`show_error` 在 `:622`）。即同一个交接流程里，提示的有无取决于失败发生在哪一步，而不是用户是否该知道。

## 提案

1. `relaunch_main_app_at`（`src/update/installer.rs:356-386`）改为返回 `bool`：它现在只在最后一次尝试失败时 `log_event!`（`:375-381`），返回值即「shell 是否接受了拉起请求」（`>32` 判据在 `:371-374`）。调用点共 8 处——`run_install_handoff` 内 4 处（`:497`、`:505`、`:510`、`:518`）、`src/update/mod.rs` 3 处（`:430`、`:435`、`:455`）、`relaunch_main_app` 转发 1 处（`installer.rs:344`）——新增返回值后各调用点显式消费或以 `let _ =` 显式忽略，不得静默依赖旧签名。
2. 在 `src/update/installer.rs:492-495` 的「非 0 / 结果未知」分支补一次可见提示，文案只陈述可证明的事实与已做的动作，例如「安装器未跑完（退出码 N）。已尝试重新启动组件，如未出现请手动打开」；拉起失败时改成「安装器未跑完（退出码 N），且组件未能自动重新启动，请手动打开」。
3. `InstallerLaunch::Cancelled`（`:500-506`）保持静默：默认动词 + asInvoker stub 下这一支近乎不可达（UAC 取消实际走非 0 那支并拿到中性提示），保留静默只是不改变现状、防语义不同的启动期取消。
4. `Some(0)` 且组件已在跑的成功路径（`:487-489`）继续保持完全静默，不新增任何提示。

## 明确不在本次范围

- 不给成功路径加提示，也不给自动检查的「无更新」加提示（那是 `src/update/mod.rs:513-522` 的既有语义，另属检查侧）。
- 不改「更新相关提示必须由短生命周期子进程显示」这条架构决定（`src/update/mod.rs:142-143` 的注释）：本节的提示都在副本进程内弹，副本本身就是隔离边界，不是常驻主进程。
- 不改 `wait_for_exit_code` 的 30 分钟上界（`src/update/installer.rs:79-91`、`src/config.rs:168-175`）。
- 不新增持久状态/注册表项来记录「上次安装未完成」：需要单独立项，本篇不做。
- 不改退出码语义映射（`src/update/installer.rs:616-622` 的 `classify_launch_hresult`）。

## 为什么不保留？

1. 「UAC 取消是用户自己的动作，不需要提示。」—— 非 0 退出码这一支**同时**覆盖用户取消与安装器静默回滚（`src/update/installer.rs:580-582`），后者用户完全不知情；默认动词下两者无法区分，因此中性提示是唯一能覆盖回滚的形态。
2. 「更新提示不得回流常驻主进程。」—— 提示由副本弹（`src/main.rs:402-408` 的 `--update-install` 分支在该进程内），主进程此刻已退出，不违反 AGENTS.md「更新、下载与安装交接」首条（长期主进程不得因更新功能初始化更新专属 UI）。
3. 「弹框会挡住交接收尾。」—— 它是交接的最后一步动作，`show_error` 返回后进程即 `std::process::exit`（`src/main.rs:402-408`）。但**真实残留**是：框存活期间副本仍持有跨进程更新互斥量（`src/update/mod.rs:487`），未点掉之前下一次检查会以 BUSY 静默退出（`src/update/mod.rs:189-198`）。这条必须在实施时取舍（见风险第 1 条），不能假装不存在。
4. 「反正日志里有。」—— 需要用户先知道并手动打开 `EnableDebugLog`（`src/config.rs:54`、`src/util.rs:476-482`）才有日志；对一个「点了是却什么都没发生」的用户，这不是可发现渠道。

## 验收标准

- `grep -n "show_error(" src/update/installer.rs` 命中数由 1 处（`:511`）增至 ≥2 处，且新增处在 `:492-495` 的 `Some(code)` / `None` 分支内。
- `grep -n "fn relaunch_main_app_at" src/update/installer.rs` 的签名返回 `bool`，且 `run_install_handoff` 内 4 处调用点（`:497`、`:505`、`:510`、`:518`）都消费了返回值。
- 真机两例：①点「是」后在 UAC 上取消 → 组件回来，且有中性提示；②制造一次静默回滚（安装期间用外部进程占住 `{app}\traffic-monitor.exe`）→ 组件回来且提示写明未跑完。
- 文案审查：不得出现「安装未完成/没装成/已恢复成功/已修复」这类不可证明的断言；`src/smoke.rs:21-29` 的人工清单补一条「安装未跑完时有可见提示」。
- 四条门禁全绿（`cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`）。

## 风险

- **自动更新路径也会弹这个框，本节决定就按这个来**，理由是一个容易被忽略的事实：安装流程不可能在没有用户参与的情况下开始——`src/update/mod.rs:570-581` 的确认框（「新版本 vX 已准备就绪。是否立即关闭程序并安装？」）必须先被点「是」，失败只可能发生在用户点完的几秒之后。因此「用户不在机器前」这一情形极罕见；即便他走开，代价也只到「手动检查在框未关闭期间以 BUSY 静默退出」（`src/update/mod.rs:189-198`），与今天「检查占线时静默退出」的行为完全一致；何况被重新拉起的实例已把首次自动检查推迟了一个完整冷却周期（`src/main.rs:515-517`、`src/update/mod.rs:225-228`），这段时间本来就不会有自动检查。
- 被否决的两个替代方案：**(b) 只在有交互会话时弹**——需要新增判据，而它要解决的问题在上一段的事实下几乎不存在，属净增复杂度；**(c) 改为写持久标记、由下次启动的主程序提示**——把提示搬到常驻主进程，与 AGENTS.md「更新、下载与安装交接」首条（长期主进程不得因更新功能初始化更新专属 UI）的既有取舍相悖，且需要额外持久状态。本节因此不引入任何新开关（不把「手动/自动」传进交接副本），实现保持最小。
- 若安装器非 0 而实际已装成功（退出码与 `[Run]` 语义不同步的边角），提示会与实际不符；因此文案**不得断言「没装成」**，只陈述「未跑完」（可由非 0 退出码证明）与已做的动作，接管判据仍以「组件是否在跑」为准（`src/update/installer.rs:482-486`）。
- 未覆盖：真机无法在本会话内验证（无 ISCC、不能真实触发回滚），以上均为机制推理 + 代码事实。
