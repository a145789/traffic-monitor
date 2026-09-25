# Agent Note：给更新子进程定归属（父身份绑定 + 可取消下载 + 跨进程互斥）

Status: proposed

## 问题

**子进程无人负责。** `src/update/protocol.rs:59-70` 用 `std::process::Command::new(current_exe).arg("--check-update")` re-exec 自身，得到一个独立 OS 进程；`src/main.rs:535-545` 的 `begin_exit()` 只有 `remove_tray_icon()` + `PostQuitMessage(0)`，既不取消也不等待子进程；全仓 `grep -rn -e JobObject -e CreateJobObject -e TerminateProcess -e "\.kill(" src/` 命中 0 处。因此用户在检查/下载中途退出主程序后，子进程继续跑完 256 MiB 上限的下载（`src/config.rs:79`），并可能在主进程已消失之后弹出确认框（`src/update/mod.rs:340-348` 的 `show_yes_no`）。

**进程内标志挡不住跨进程。** `UPDATE_IN_PROGRESS` 是进程内原子（`src/state.rs:56`，读写在 `src/update/mod.rs:115`、`:139`、`:392`），而 `--check-update` 按 AGENTS.md 第 4 条必须在单例 Mutex 之前被拦截（`src/main.rs:187-190`），所以手工再跑一次 `traffic-monitor.exe --check-update`、或主进程崩溃后快速重启，可以同时存在多个更新子进程。它们共用同一个固定缓存路径（`src/update/cache.rs:10`、`:15-23`）。这条已被证伪为「不会损坏」：`create_new(true)` + `FILE_SHARE_READ_ONLY`（`src/update/cache.rs:13`、`:32-39`）让第二个进程打不开、删不掉（无 `FILE_SHARE_DELETE`），最终以 `FetchFailure::Local("创建安装包文件失败")` 退出——所以本 note 要收的是**进程残留与重复交互**，不是数据损坏。

**协议写入失败被静默吞掉。** `src/update/protocol.rs:189-192` 的 `emit_protocol_line` 返回 `()`，`write_all`/`flush` 的错误被 `let _ =` 丢弃；`src/update/mod.rs:352` 发完 `EXIT_MAIN` 后立刻 `wait_main_instance_gone()`（`src/update/installer.rs:169-206`）并启动提权安装器——父进程是否真的收到那条交接消息，子进程完全不知道。

**取消点没有落点。** 分块下载的循环不在 `src/update/installer.rs:87-162`（那只是调用方与文件锁/哈希校验），真正逐块消费数据的是 `src/update/http.rs:280-300` 的 `fetch_to_file` → `conn.for_each_chunk(max_response_bytes, consume)`；要加取消点必须把谓词透传进这个闭包，并给 `FetchFileError` 增加一个**不能与下载失败同义**的变体——因为 `src/update/mod.rs:274` 只在 `FetchFailure::Download` 上回落第三方代理，把「父进程没了」映射成 `Download` 会触发一次完整的代理重下（还有删了部分文件再下的副作用）。

**安装器结果无人接管。** `ShellExecuteExW` 成功即 `return InstallerLaunch::Started`（`src/update/installer.rs:291-293`），不持有句柄、不等退出码。这一点本 note 只记录、不修（理由见下）。

生产消费者：`--check-update` 子进程入口 `src/update/mod.rs:307-321`，父侧收割 `src/update/protocol.rs:45-118`；非生产消费者：`src/update/protocol.rs:230-387` 的协议扫描测试（用 `Cursor` 喂字符串，不涉及真实进程）。

## 提案

1. **父身份绑定到「进程 + 创建时刻」，并只等待那个句柄。** 父进程 spawn 时传 `--parent-pid <n> --parent-start <FILETIME>`（父侧用 `GetProcessTimes` 取自身创建时间）；子进程启动早期 `OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid)`，用 `GetProcessTimes` 复核创建时刻**完全相等**，然后持有该句柄到进程结束，之后的所有存活检查都用 `WaitForSingleObject(handle, 0)`。只传 PID 不足以承载这个机制：长时间开机的机器上 PID 必然被复用，子进程没有原始创建时刻可比，`OpenProcess` 会拿到另一个进程的句柄并误判「父还在」。`--parent-pid` 缺席时（手工调试）自检退化为空操作，不影响 `--check-update --manual` 的独立可用性。
2. **检查点与动作规则写死为两条**（不是「弹框前查一次」就够）：
   - R1 父已消失、且本次检查尚未走到「用户确认安装」⇒ 静默退出：不下载、不弹框、不启动安装器（发出协议行之前退出，父侧读到空流，`saw_valid_action` 为假，自然按失败处理）。
   - R2 用户**已在模态框里点了「是」** ⇒ 无论父进程是否还在都继续交接：这是用户明确表达过的意图，不再依赖 `EXIT_MAIN` 的送达。若此时父进程仍在而 `EXIT_MAIN` 写失败，则视为硬错误、**不得**启动安装器（否则父进程不会释放 exe 映像，必然撞上文件占用）；父已消失而写失败则继续（没有需要通知的对象）。
   - 检查点落点：进入下载前一次、`fetch_to_file` 的每个分块处（谓词）、以及**每个 `show_info` / `show_yes_no` 返回之后**——模态框可能在父进程退出期间一直开着，返回后才做决定，这是 R1/R2 的分界。
3. **`emit_protocol_line` 返回 `Result`**，`EXIT_MAIN` 的写入结果参与上面的 R2 分支判断；`DONE` 的写入失败只记日志（无人在等）。
4. **取消走独立错误变体，不触发代理回落。** `src/update/http.rs:270-278` 的 `FetchFileError` 增加 `Cancelled`；`fetch_to_file`（`src/update/http.rs:280`）增加一个 `should_continue: impl Fn() -> bool` 参数并在 `consume` 闭包内先判；`src/update/installer.rs:49-56` 的 `FetchFailure` 增加 `Cancelled`；`src/update/mod.rs:266-294` 的匹配里 `Cancelled` 直接返回（不回落代理、不重试），并沿用既有的「先释放写锁再删部分文件」（`src/update/installer.rs:121-130`）。
5. **跨进程命名互斥量 + `BUSY` 协议结果。** 子进程启动即 `CreateMutexW` 一个更新专用名字（常量入 `src/config.rs`，照 `MUTEX_NAME` 的风格；不带 `Global\` 前缀即会话级，与单例锁一致，跨会话并发的残留由缓存锁兜底，见「风险」）。`ERROR_ALREADY_EXISTS` ⇒ 输出新的协议行 `BUSY` 并退出。父侧（`src/update/protocol.rs:181-187` 的 `parse_update_action`、`src/update/mod.rs:185` 的收尾判定）必须把 `BUSY` 与 `DONE` 区分开：`BUSY` **不写 `NEXT_CHECK_TIME`、不推进一小时正常冷却**（沿用当前 deadline，或写入错误冷却值以便稍后重试），否则「因另一个更新进程占用而未执行」会被记账成一次成功检查。
6. **拦截次序不变**：父元数据抓取前先做一次 R1 检查；`--check-update` 仍在单例 Mutex 之前（`src/main.rs:187-190`，AGENTS.md 第 4 条）。

## 明确不在本次范围

- **不用「Job Object + KILL_ON_JOB_CLOSE」作为主手段。** 它看起来最省事，但会直接打断既定的更新成功路径：用户确认后主进程**必须**先退出释放 exe 映像（`src/update/mod.rs:350-354`），而主进程一退出 Job 就关，子进程会在 `wait_main_instance_gone()` 期间被杀，安装器永远起不来。
- **不用「父侧跟踪 Child 并在退出时 kill」替代子进程自检。** 父侧确实持有句柄（`src/update/protocol.rs:65` 的 `spawn`、`:97` 的 `wait`），但那个 `Child` 属于更新工作线程，父线程要在 `begin_exit` 里 kill 就得引入跨线程共享句柄 + 交接状态；而 kill 的正确条件恰好是「尚未收到 `EXIT_MAIN`」——与子进程本地自检同构，却多了一处在主进程退出路径上的写操作，且时序写错就会杀掉正当交接中的子进程（与上面否决 Job 的失败模式同源）。子进程自检不需要任何共享状态，也不会误杀交接。
- 不改缓存锁策略（已验证 fail-safe，收益为零）。
- 不做安装器退出码回报：主进程此时已经退出，要回报必须跨进程持久化（注册表/文件）再加一次启动期读取，成本远超收益；且 `installer.iss:137-147` 把终止逻辑钉在 `ssInstall`、并有 `[Run] postinstall` 重新拉起（`installer.iss:43-46`），当前失败面已经有兜底。
- 不改安装器侧的 `AppMutex` / `RequestGracefulExit` / `ForceKillRemnant`（`installer.iss:5`、`:80-135`）。
- 不把弹框改成可取消的自绘窗口：那是外观与交互改造，不是本 note 的归属问题。

## 为什么不保留？

1. 「用户点了退出，说明他不要这个更新了」——子进程在弹框之前并不知道父进程死了，弹框发生在主进程消失之后：用户对「程序关掉之后还要不要装」从未表达过意见，这不是他先前同意的那个决定。R1 正是把这条补上。
2. 「安装器有 AppMutex + taskkill 兜底，孤儿无害」——那是安装器侧的保护，防的是重复实例，不解决孤儿进程占用带宽、以及主界面已消失却冒出 UAC 弹窗的观感问题。
3. 「缓存有锁，多个孤儿不会互相破坏」——正确，但这条只证明数据安全，不证明进程与交互面安全；本 note 的提案不依赖缓存行为。
4. 「多开需要用户手工跑 `--check-update`，不是正常路径」——`--check-update` 正是父进程自己每次自动检查都会跑的路径，主进程崩溃/快速重启时天然会留下第二个；此外这个参数是公开入口，任何脚本都能触发。
5. 「写 stdout 失败不值得传播」——旧行为让「父进程已消失」与「父进程在但没收到」这两件后果完全不同的事看起来一样；R2 的分支判断正是靠这个返回值区分，所以必须传播。

## 验收标准

- `grep -rn "parent-pid\|parent_pid\|parent-start\|parent_start" src/` 命中 ≥ 4 处（父侧构造两参数、子侧解析两参数）；子进程侧存在 `GetProcessTimes` 与 `WaitForSingleObject` 的使用（人工核对：复核创建时刻 + 只等句柄，不接受「只 `OpenProcess` 就算」）。
- `grep -rn "CreateMutexW" src/` 只认**调用点** 2 处（单例锁、更新互斥；行数会连带 import 与 SAFETY 注释，不按行数判定），且更新互斥的申请在 `src/main.rs` 的 `--check-update` 分支内、而非其后。
- `grep -rn "BUSY" src/` 覆盖三处：子进程输出、`parse_update_action` 的解析分支、父侧「不推进正常冷却」的判定；`grep -rn "FetchFileError::Cancelled\|FetchFailure::Cancelled" src/` 命中 ≥ 2 处（产生处 + 分类处）。
- 协议测试（`src/update/protocol.rs:230-387`，用 `Cursor` 喂字符串）需要**扩充**而不是保持：新增 `BUSY` 行被识别为「有效动作但非成功完成」，并补一条「`Cancelled` 不得被分类成 `Download`」的判定测试。
- 手动场景 A：托盘菜单触发检查 → 下载中途用托盘退出主程序 → 任务管理器内无 `--check-update` 残留进程，且 30 秒内不出现任何更新弹框。
- 手动场景 B（同时验证互斥命门）：主程序运行时，手工连开两个 `traffic-monitor.exe --check-update --manual` → 第二个立即静默退出（退出码 0，无网络请求），若第一个仍在跑则第二个输出 `BUSY`；随后观察主进程**没有**把这次结果记成一次成功检查（冷却不被推进到 1 小时）。
- 手动场景 C（回归保护，最重要）：完整走一次「发现新版本 → 点是 → 主程序退出 → 安装器启动」成功路径不被父身份自检误杀；并补一条「模态框开着时退出主程序 → 再点『是』」的分支确认走 R2（安装器仍被拉起，不再有「父已消失却以为交接成功」的中间态）。
- `cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt -- --check` 全绿。

## 风险

- 检查与动作之间的竞态窗口仍然存在：`WaitForSingleObject` 报「还在」之后父进程可能立刻退出，随后子进程才弹框。窗口从「整个下载期」缩到「检查与弹框之间」，量级从分钟降到毫秒；彻底闭合需要把弹框改成可取消的自绘窗口（已列入「不在本次范围」）。
- 元数据抓取（`fetch_url`）**不带**取消谓词：检查点只覆盖安装包下载的每个数据块，这一次 4KiB 请求从建连到收完之间不可中断（`HttpGet::open` 同理），父进程若在该窗口内退出，「静默放弃」最坏要等这次抓取走完自身超时；该阶段无弹框、无写盘、不启动安装器，因此用户可见后果只是多一次无人消费的元数据请求，但这个延迟窗口确实存在，要在抓取之间保证不再有无人要的重试（失败重试前必须复判一次 R1）。彻底消除得把谓词一路透传到 `fetch_url` 的消费闭包，收益仅限收包阶段（建连阶段仍不可中断）。
- `--parent-pid`/`--parent-start` 属于公开 CLI 面，需在 `parse_cli_args`（`src/main.rs` 的测试已覆盖精确匹配语义，`src/main.rs:792-799`）里一并处理，避免被当成未知参数；参数缺失时必须按「无父可查」走退化路径，而不是按「父已退出」误杀手工调用。
- 会话级更新互斥无法阻止两个不同用户会话同时更新：它们会通过缓存锁的 `FetchFailure::Local("创建安装包文件失败")` 之一失败（现状即如此），本 note 不改变该结论，也不把互斥名升为 `Global\`（那会让异会话的更新互相阻塞到超时）。
- `BUSY` 是协议面的新增值，若旧版父进程（升级中途、新旧混跑）读到未知行会落入 `saw_valid_action == false` 并被当作失败——这是可接受的失败方向（退到错误冷却），但实施时要确认 `src/update/protocol.rs` 的扫描器不会因为未知行而提前中断读取。
- `for_each_chunk` 的取消检查是每个数据块一次，块越大取消延迟越长；实现时不要把块大小改大，也不要在取消路径上做额外的网络或磁盘操作。
