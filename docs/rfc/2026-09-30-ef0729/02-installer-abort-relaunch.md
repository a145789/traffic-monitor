# Agent Note：安装器没装完就没人再拉起组件——让更新子进程等安装器收场，并把"装完之后"写进验收

Status: proposed

## 问题

（「问题」「明确不在本次范围」「为什么不保留」三节是改动前的现场记录，行号相对改动前的代码；本次改动的规范见"提案"。）

`installer.iss:46-49` 的 `[Run]` 条目带 `postinstall`。官方文档对 `postinstall` 的定义是"在 Setup Completed 向导页创建复选框，由用户勾选决定是否处理"，`[Run]` 章节开头也写明条目在 "the program has been successfully installed" 之后执行；而 `postinstall` 与 `skipifsilent` 是**两个互相独立的 flag**（Inno 官方文档自己的示例就写成 `Flags: postinstall nowait skipifsilent`，它自动生成的 isreadme 条目同样两个都带）——如果静默模式下 `postinstall` 会自动跳过，`skipifsilent` 就是多余的。所以成功路径成立：**安装成功收尾时组件会被拉起来**（"更新成功后小组件蒸发"因此不成立，别照着那个方向改）。本条要收的是它的另一半：**安装器启动成功之后、安装没能成功收尾**时 `[Run]` 不执行，而旧实例早已按 `src/update/mod.rs:479-499` 的约定退出并让出 exe 映像，更新子进程也在 `src/update/mod.rs:501-505` 打完一行日志就结束（`SubprocessEnd::ExitMain` → `subprocess_main` 返回 0，见 `src/update/mod.rs:365-388`）——没有任何人再拉起组件。

失败的触发面要按 /VERYSILENT 的真实形态写（`src/update/installer.rs:288`）：更新路径是静默安装，**没有可点击的取消入口**，现实情形是复制阶段失败、目标文件被杀软占用、安装器崩溃/回滚；至于 UAC 取消，它发生在启动器阶段、已被 `src/update/mod.rs:506-512` 覆盖。用户可见后果不是"永久消失"（开始菜单与桌面快捷方式仍在），而是：他刚在更新框点了"是"，程序就人间蒸发、没有任何提示，也不会自己回来。

生产消费者：`installer.iss:148-158` 的 `CurStepChanged(ssInstall)` 只做"先礼后兵"的终止（`RequestGracefulExit` → 超时 `ForceKillRemnant`），没有任何收尾拉起；`src/update/installer.rs:231-252` 的 `relaunch_main_app` 只被 `src/update/mod.rs:506-512`（`Cancelled`）与 `:513-518`（`Failed`）调用——两者覆盖的都是"**安装器没启动起来**"，覆盖不到"启动成功、随后失败"；`src/update/installer.rs:285-323` 的 `try_launch_installer` 在 `ShellExecuteExW` 成功即返回 `Started`，不持有句柄、不等退出码。

非生产消费者：`src/smoke.rs:21-24` 的人工验收清单，逐字是"更新确认框点'是'后主程序退出、安装器正常启动；安装器 UAC 取消后主程序被重新拉起"——停在"安装器正常启动"，"装完之后"正好是盲区。

## 提案

1. **让更新子进程等安装器收场（已实施）**：`update::installer::try_launch_installer` 加上 `SEE_MASK_NOCLOSEPROCESS` 取回进程句柄（新类型 `InstallerProcess`，Drop 关闭），`launch_installer` 只负责启动与"文件被占用"类瞬态重试、成功后把句柄交出去；`InstallerProcess::wait_for_exit_code` 用 `WaitForSingleObject` 等它退出、`GetExitCodeProcess` 读 Inno 退出码（官方 Setup Exit Codes：**0 = 成功跑完；其余任何值都表示没跑完**——初始化失败/取消/致命错误/回滚）。`update::complete_update_interaction` 据此分「成功 / 未成功 / 结果未知」三条日志分支，**三条都重新拉起主程序**：静默路径的 `[Run]` 已按提案 3 跳过，本子进程是这次更新唯一的拉起点，退出码只承担日志措辞，不承担"谁负责拉起"的判定。非 0 说明安装已回滚，`{app}` 下仍是旧版可执行文件，拉起来即恢复。
   - **别把"子进程是非提权进程"当身份保证**：更新子进程由主进程 re-exec 得到，`CreateProcess` 继承 token，因此组件已被提权时子进程同样是提权的，它拉起的新实例也就是提权的（这正是 01 的问题陈述）。这条身份保证由 01 的提案 2（组件启动期无条件自去提权）承担，不由子进程承担；两篇必须一起实施才成立。
   - 不变量提示：安装包只读锁在 `ShellExecuteExW` 返回后即释放（影像加载器此时已拿到文件），**不**把它带进"等安装器收场"那一段，所以不存在"整段安装期间锁住已校验安装包"这条风险。
2. **安装器侧显式收尾（备选，未采用）**：在 `installer.iss` 的 `[Code]` 加 `CurStepChanged(ssDone)`/`DeinitializeSetup` 收尾 `Exec('{app}\traffic-monitor.exe', ...)`，成功与失败两侧都拉起。它不用改更新子进程的寿命，但拉起身份受安装器 token 支配（会命中 01 那条），且管不到"安装器根本没起来"（UAC 取消）。
3. **拉起点只能留一个（已实施）**：`installer.iss` 的 `[Run]` 条目加 `skipifsilent`——向导路径保留 `postinstall` 复选框 UX，静默路径整条跳过、交给子进程；绝不允许"子进程拉一次 + `[Run]` 再拉一次"。单例互斥会让第二个静默退出、无功能损害，但多一次进程创建与竞态窗口。
   - **副产品**：Inno 在静默模式下是否处理 `postinstall` 条目**不再是本条的承重前提**——静默路径的拉起点由 `skipifsilent` 与子进程唯一确定，与那条语义怎么解释无关。
4. 把"装完之后"写进人工清单（已实施，见验收标准）。

## 明确不在本次范围

- **不把安装器退出码回报给主进程**：主进程此时已经退出，那需要跨进程持久化（注册表/文件）+ 启动期读取——正是 `docs/archive/rfc/2026-09-24-64bb2b/03-update-child-lifecycle.md` 否决的方向。提案 1 是在**更新子进程内部**就地消费退出码，不需要任何持久化，与该否决不冲突；实施时不要把"等安装器"写成"回报给主进程"。
- 不动 `ssInstall` 的终止逻辑（`installer.iss:148-158`）：它解决的是"旧实例占用目标 exe"，挪到 `ssDone` 会让文件占用重新暴露。
- 不改 EXIT_MAIN 顺序与 R1/R2 判定（`src/update/mod.rs:479-499`）：那是"主进程先退、再提权启动安装器"的前提。R1（`src/update/protocol.rs:466-468`）此时也不会介入——用户已点过"是"（`user_confirmed`），子进程绝不会因父进程消失而放弃交接。
- 不在组件里加"开机自启兜底"：`installer.iss:44` 的 HKCU Run 是可选任务，依赖它等于把恢复推迟到下次登录。

## 为什么不保留？

1. **"有 `[Run] postinstall` 兜底，失败面已经有人管"**——它在 Inno 语义上确实兜住了"成功收尾"（归档笔记这点没错），但失败路径的前提恰恰是"没能成功收尾"，所以这条兜底在失败路径上不成立；而组件的空窗期正好被压在"旧实例已退出"之后。
2. **"失败时旧实例还在，用户没损失"**——不在。旧实例在启动安装器**之前**就退出了（`src/update/mod.rs:479-480` 的注释说明了这是为了释放 exe 映像）。
3. **"用户自己重开就行"**——他刚点了"是"，会认为程序正在升级，不会去检查任务栏；组件是常驻挂件，"自动更新一次就消失"是不可接受的产品后果。
4. **"失败概率极低"**——触发面不是"用户取消"（静默路径没有取消入口），而是复制阶段失败/杀软实时扫描占用/安装器崩溃；`src/update/installer.rs:114-137` 与 `installer.iss:118-146` 的存在正说明"目标文件被占用"在这条链路上是常见故障。
5. **"用 `[Run]` 加 `skipifsilent` 就够了"**——那只抑制静默路径的重复拉起，不改变"未成功收尾就不执行"这一性质。

## 验收标准

- 提案 1：`grep -n "SEE_MASK_NOCLOSEPROCESS\|WaitForSingleObject\|GetExitCodeProcess" src/update/installer.rs` 三个 API 名都能命中；`src/update/mod.rs` 的安装交接出现"成功 / 未成功 / 结果未知"三条分支，且三条都重新拉起主程序。
- 提案 3：`grep -n "Flags:.*skipifsilent" installer.iss` 命中 1 次（Flags 行为 `nowait postinstall skipifsilent runasoriginaluser`），静默路径的拉起点不经过 `[Run]`，向导路径仍保留复选框。
- **真机两条（本次交付未做，见风险）**：①安装成功 → 组件自动回到任务栏，且任务管理器"已提升"列为**否**（同时验证 01 的身份问题）；②安装中途用任务管理器结束 `TrafficMonitor-Setup-*.exe` → 组件在数秒内自动回来（而不是要手动启动）。
- 人工清单（`src/smoke.rs`）已新增两条：「更新成功后不手动启动，组件自动回到任务栏且非提权」与「安装器被强杀/失败后，组件自动回来」。
- 现有 UAC 取消兜底不回归：默认动词下 UAC 取消表现为安装器非 0 退出码（走提案 1 那条），`InstallerLaunch::Cancelled` 分支保留给 `ShellExecuteExW` 自身仍返回 `ERROR_CANCELLED` 的场合，两条互不冲突。
- `cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全绿。

## 风险

- 提案 1 把更新子进程的寿命从"启动安装器即退"延长到"安装结束"（数秒，异常情况下更久）。与 AGENTS.md 第 4 条"可回收的短生命周期隔离边界"的兼容性判定：边界仍在，且没有把网络/加密/更新 UI 带回常驻主进程。等待期间子进程仍持有跨进程更新互斥量，此期间新的更新检查会得到 `BUSY` 并静默跳过（有日志），安装器收场后子进程退出、锁随之释放。若判定不兼容，退到提案 2 + 01 的身份修法。
- **已消解的原风险**：`SEE_MASK_NOCLOSEPROCESS` 的句柄不再需要跨完整性等级等待与取码——01 的提案 3 改用默认动词使安装器以同一等级启动。
- 退出码语义没有逐字保证：官方退出码表写的是"Setup program 可能返回的码"（其中 `/HELP` 一项由 SetupLdr 处理，是"这张表覆盖 SetupLdr"的间接证据），但没有一句明说 SetupLdr 会把内层 Setup 的退出码原样返回。因为三条分支都重新拉起，判错的最坏后果只是日志措辞与"组件晚几秒回来"，不会丢组件；仍需真机确认。
- 两个拉起点若没关干净，用户可能看到组件"闪一下"；`skipifsilent` 从源头只留一个，单例互斥还会挡下第二个。
- 提案 2 的 `[Code]` 收尾必须放在**回滚完成之后**，否则可能拉起一个正被覆盖的 exe（未采用，留档）。
- **未实测项（交付时的事实）**：退出码在"安装成功 / 安装器被强杀 / UAC 取消"三种情形下的实际取值、`WaitForSingleObject(INFINITE)` 在安装器被强杀时是否立即返回、以及"组件在数秒内自动回来"这条时序，都只做过文档比对与静态推理。
- **可证伪项**：若真机上"安装器复制失败/被杀软挡下"时 `[Run]` 仍被执行，则本条的原始问题描述不成立；不过本 PR 已用 `skipifsilent` 把静默路径的拉起点收敛到子进程，该问题不再依赖这层语义。
