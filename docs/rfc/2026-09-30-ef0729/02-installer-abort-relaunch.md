# Agent Note：安装器没装完就没人再拉起组件——让更新子进程等安装器收场，并把"装完之后"写进验收

Status: proposed

## 问题

`installer.iss:46-49` 的 `[Run]` 条目带 `postinstall`。官方文档对 `postinstall` 的定义是"在 Setup Completed 向导页创建复选框，由用户勾选决定是否处理"，`[Run]` 章节开头也写明条目在 "the program has been successfully installed" 之后执行；而 `postinstall` 与 `skipifsilent` 是**两个互相独立的 flag**（Inno 官方文档自己的示例就写成 `Flags: postinstall nowait skipifsilent`，它自动生成的 isreadme 条目同样两个都带）——如果静默模式下 `postinstall` 会自动跳过，`skipifsilent` 就是多余的。所以成功路径成立：**安装成功收尾时组件会被拉起来**（"更新成功后小组件蒸发"因此不成立，别照着那个方向改）。本条要收的是它的另一半：**安装器启动成功之后、安装没能成功收尾**时 `[Run]` 不执行，而旧实例早已按 `src/update/mod.rs:479-499` 的约定退出并让出 exe 映像，更新子进程也在 `src/update/mod.rs:501-505` 打完一行日志就结束（`SubprocessEnd::ExitMain` → `subprocess_main` 返回 0，见 `src/update/mod.rs:365-388`）——没有任何人再拉起组件。

失败的触发面要按 /VERYSILENT 的真实形态写（`src/update/installer.rs:288`）：更新路径是静默安装，**没有可点击的取消入口**，现实情形是复制阶段失败、目标文件被杀软占用、安装器崩溃/回滚；至于 UAC 取消，它发生在启动器阶段、已被 `src/update/mod.rs:506-512` 覆盖。用户可见后果不是"永久消失"（开始菜单与桌面快捷方式仍在），而是：他刚在更新框点了"是"，程序就人间蒸发、没有任何提示，也不会自己回来。

生产消费者：`installer.iss:148-158` 的 `CurStepChanged(ssInstall)` 只做"先礼后兵"的终止（`RequestGracefulExit` → 超时 `ForceKillRemnant`），没有任何收尾拉起；`src/update/installer.rs:231-252` 的 `relaunch_main_app` 只被 `src/update/mod.rs:506-512`（`Cancelled`）与 `:513-518`（`Failed`）调用——两者覆盖的都是"**安装器没启动起来**"，覆盖不到"启动成功、随后失败"；`src/update/installer.rs:285-323` 的 `try_launch_installer` 在 `ShellExecuteExW` 成功即返回 `Started`，不持有句柄、不等退出码。

非生产消费者：`src/smoke.rs:21-24` 的人工验收清单，逐字是"更新确认框点'是'后主程序退出、安装器正常启动；安装器 UAC 取消后主程序被重新拉起"——停在"安装器正常启动"，"装完之后"正好是盲区。

## 提案

1. **让更新子进程等安装器收场（推荐，且顺带解决身份问题）**：`try_launch_installer`（`src/update/installer.rs:285-323`）加上 `SEE_MASK_NOCLOSEPROCESS` 取回进程句柄，用 `WaitForSingleObject` 等它退出、`GetExitCodeProcess` 读 Inno 退出码（官方 Setup Exit Codes：0 = 成功；非 0 = 初始化失败/用户取消/致命错误/回滚/非致命错误），据此决定：非 0 ⇒ `relaunch_main_app()`（回滚后 `{app}` 下是旧版可执行文件，拉起来即恢复，失败原因写进日志）；0 ⇒ 成功路径。关键优势是**身份**：更新子进程本身是非提权进程（`src/update/protocol.rs:74-93` 由 `asInvoker` 主进程 re-exec 得到），它拉起的新实例是非提权的——这顺带绕开 01-update-relaunch-integrity 那条"runas 启动的安装器用管理员 token 拉起组件"。API 路径不新：仓库已在 `src/update/protocol.rs:424-426` 用 `WaitForSingleObject(handle, 0)`。
2. **安装器侧显式收尾（备选）**：在 `installer.iss` 的 `[Code]` 加 `CurStepChanged(ssDone)`/`DeinitializeSetup` 收尾 `Exec('{app}\traffic-monitor.exe', ...)`，成功与失败两侧都拉起。它不用改更新子进程的寿命，但拉起身份受安装器 token 支配（会命中 01 那条），且管不到"安装器根本没起来"（UAC 取消）。
3. **拉起点只能留一个**：采用提案 1 时，`[Run]` 应加 `skipifsilent`（向导路径保留复选框 UX，静默路径交给子进程）或整体删除；绝不允许"子进程拉一次 + [Run] 再拉一次"。单例互斥（`src/main.rs:187-204`）会让第二个静默退出、无功能损害，但多一次进程创建与竞态窗口。
4. 把"装完之后"写进人工清单（见验收标准）。

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

- 提案 1：`grep -n "SEE_MASK_NOCLOSEPROCESS\|WaitForSingleObject\|GetExitCodeProcess" src/update/installer.rs` 三处都能命中；`src/update/mod.rs:501-519` 出现"成功/失败"两条分支且都覆盖重新拉起或明确不需要拉起。
- 真机两条：①安装成功 → 组件自动回到任务栏，且任务管理器"已提升"列为**否**（同时验证 01 的身份问题）；②安装中途用任务管理器结束 `TrafficMonitor-Setup-*.exe` → 组件在数秒内自动回来（而不是要手动启动）。
- 提案 3：`grep -n "postinstall\|skipifsilent" installer.iss` 能看出拉起点只有一处，且静默路径不会双拉起（任务管理器观察只有一个组件进程，无"闪一下"）。
- 人工清单（`src/smoke.rs:21-24`）新增两条：「更新成功后不手动启动，组件自动回到任务栏且非提权」与「安装器被强杀/失败后，组件自动回来」。
- 现有 UAC 取消兜底不回归：`src/update/mod.rs:506-512` 的路径仍生效，且不与新拉起点冲突。
- `cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全绿。

## 风险

- 提案 1 把更新子进程的寿命从"启动安装器即退"延长到"安装结束"（数秒；期间它持有已校验安装包的只读锁）。需要确认这与 AGENTS.md 第 4 条"可回收的短生命周期隔离边界"兼容；若判定不兼容，退到提案 2 + 01 的身份修法。
- `SEE_MASK_NOCLOSEPROCESS` 返回的句柄要跨完整性等级等待并取退出码，需实测（理论上等待只需 SYNCHRONIZE、`GetExitCodeProcess` 只需查询权限，且句柄由系统直接给出、不经过 `OpenProcess`）。
- 两个拉起点若没关干净，用户可能看到组件"闪一下"；单例互斥会挡下第二次，但要实测确认不出现"旧实例已退、新实例撞锁退出"的顺序（这与 01 提案 2 的互斥量时序是同一个坑）。
- 提案 2 的 `[Code]` 收尾必须放在**回滚完成之后**，否则可能拉起一个正被覆盖的 exe。
- **可证伪项**：若实测发现"安装器复制失败/被杀软挡下后 `[Run]` 仍被执行"，则本条问题不存在；验收时用一次真机强杀安装器来证伪，不要靠推断。
