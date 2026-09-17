# Agent Note：看门狗成为稳定控制入口，修复退出与更新交接

Status: implemented

## 问题

主窗口经 `src/window.rs:207` 的 `SetWindowLongPtrW(GWL_STYLE, WS_CHILD|WS_VISIBLE)` 成为任务栏子窗口后，`src/main.rs:67` 的 `quit_existing_instance` 仍用 `FindWindowW(WINDOW_CLASS)` 查找它，而 `FindWindowW` 只检索顶层窗口，因此正常嵌入状态下 `--quit` 第一次查找即 miss、直接什么都不做，且轮询消失的第二次查找也会立刻误判为“已退出”。同一根因的第二条失效链在更新交接：`src/update/mod.rs:176` 在 `spawn_update_worker` 时快照 `hwnd_raw`，`src/update/mod.rs:640` 的 `post_update_action` 向该旧句柄 `PostMessageW(WM_USER_UPDATE_ACTION)` 且忽略失败；若检查、下载或确认弹窗期间发生 Explorer 重建（`src/main.rs:325` 的 `rebuild_main_window` 替换主窗口），该消息发往已销毁的旧 HWND 并静默丢失。此时 `src/update/mod.rs:207` 因 `exit_signalled=true` 直接返回而不复位 `UPDATE_IN_PROGRESS`，而本应复位的 `src/update/mod.rs:651` 的 `handle_update_action` 永远收不到消息，后续一切自动/手动检查都被 `swap(true)` 挡掉直到重启；重拉起的实例又被单例 Mutex 挡住，形成死锁。第三条同源缺口是 `src/main.rs:343` 重建时 `create_main_window` 失败直接 return，此时旧窗口已销毁、`CURRENT_MAIN_HWND=0` 且无任何重试，只能等下一次任务栏重建或人工重启。检索证据：`Select-String -Pattern "--quit"` 全仓仅命中 `src/main.rs:92` 一处生产调用方；`Select-String -Pattern "WATCHDOG_CLASS"` 命中 `src/config.rs:19` 定义与 `src/window.rs:106`、`src/window.rs:122` 注册创建、`src/main.rs:376` 看门狗过程（仅处理 `TaskbarCreated`，无退出/更新分支），确认看门狗常驻顶层但尚未承担控制职责。

## 提案

把看门狗窗口提升为唯一的稳定控制消息入口，生产消费者与非生产消费者划分如下：生产消费者为 `--quit` 流程（`src/main.rs:92`）、更新工作线程的退出转发（`src/update/mod.rs:562` 的 `scan_subprocess_protocol` 回调）、`TaskbarCreated` 重建（`src/main.rs:376`）；非生产消费者为 `scan_subprocess_protocol` 的 Cursor 协议单测（仅覆盖文本解析，不覆盖 HWND 生命周期）。具体改动：`quit_existing_instance` 改查 `WATCHDOG_CLASS`（顶层，`FindWindowW` 可达），看门狗过程新增退出分支，直接执行与 `src/main.rs:501` 等价的托盘清理加 `PostQuitMessage`（不转发给主窗口：转发只能证明消息已入队，主窗口被 Explorer 崩溃级联销毁时该消息随之消失，而幂等门已置位会让后续请求被吞掉，进程再也退不出；直接执行使幂等门恰好对应「退出序列已执行」这一不可逆事实，且该操作幂等）；`spawn_update_worker` 改为由工作线程 `PostMessageW` 给看门狗（不再携带主窗口 HWND 快照），再由看门狗直接执行 `handle_update_action` 语义（同样不转发，理由同上），同时把“读到 EXIT_MAIN”与“成功通知 UI”拆成两个返回值/标志，通知未送达时工作线程必须复位 `UPDATE_IN_PROGRESS` 而不是按 `exit_signalled` 直接返回（判定抽为纯函数，全局标志只留一处断言以免并行用例互相覆盖）；`rebuild_main_window` 的创建失败路径改为由看门狗侧带退避的重试（定时器驱动，首次失败提示、重试静默），而不是一次性 `show_error` 后丢空窗口状态；主窗口 `WM_CLOSE` 与看门狗退出请求共用同一份退出序列实现（`begin_exit`），避免语义两处漂移。

## 明确不在本次范围

安装器 `.iss` 的强杀推迟与收窄不在本次范围（另见 `03-installer-graceful-handoff`，且它依赖本篇先提供可用的优雅退出入口）；安装包哈希与锁的绑定方式不在本次范围（另见 `02-verified-installer-pipeline`）；主题与电源广播是否搬迁不在本次范围（另见 `05-watchdog-broadcast`，定向注册通知保持不动）；不引入新进程、新 Mutex 或跨进程 IPC，新控制消息若需新增 `WM_USER` 偏移必须在 `src/config.rs:35` 附近保持唯一并注释。

## 为什么不保留？

最强的反方是“保持现状：用 `EnumWindows`/`EnumChildWindows` 按类名枚举子窗口找主窗口，更新线程加失败重试循环”。不采纳的理由：枚举子窗口仍以“主窗口存在且类名不变”为前提，重建间隙（`CURRENT_MAIN_HWND=0`）与半嵌入态（`EMBEDDED=false`）下依然无稳定锚点，而看门狗是全生命周期唯一不重建的顶层窗口，用它做锚点把三条失效链收敛到一处比分头打补丁更少状态；重试循环若仍以旧 HWND 为目标只是把静默丢失推迟，不能解决“发错对象”的本质错误。第二个反方是“更新失败靠安装器 taskkill 兜底即可”。不采纳的理由：兜底只能保证文件拷贝不冲突，不能恢复 `UPDATE_IN_PROGRESS` 死锁，也不能让 UAC 取消后优雅重拉起，正常路径的正确性不能外包给强杀。

## 验收标准

`Select-String -Pattern "WATCHDOG_CLASS" src/main.rs` 必须命中退出入口与看门狗过程的退出/更新分支；`Select-String -Pattern "FindWindowW" src/main.rs` 不得再以 `WINDOW_CLASS` 作为 `--quit` 的查找类名。现有测试必须全过：`cargo test --locked`（重点点名 `scan_exit_main_forwards_exactly_once`、`scan_duplicate_exit_main_forward_only_once`、`scan_done_does_not_forward` 不得改行为），`cargo clippy --all-targets --locked -- -D warnings`，`cargo fmt -- --check`。新增或改动的单测必须覆盖：重建间隙的退出请求幂等（调两次只退一次且不 panic）；通知未送达时 `UPDATE_IN_PROGRESS` 被复位（用内存队列加返回 false 的转发回调模拟看门狗已消失，断言判定为需复位、且全局标志回到 false）。实机矩阵（人工）：正常嵌入后 `--quit` 进程在 5 秒内退出；Explorer 重启后 `--quit` 仍有效；在更新确认框弹出的 30 秒内重启 Explorer，确认安装后主进程仍能优雅退出且不残留托盘图标。

## 风险

看门狗成为单点控制入口后，其窗口过程的任何 panic 或阻塞都会同时影响退出、更新与重建三条路径，残留风险是消息处理引入重入或错线程 `PostQuitMessage`（`handle_update_action` 注释要求必须在 UI 线程执行，看门狗与主窗口同属 `GetMessageW(None)` 同一线程是前提，若未来拆分线程该前提即失效）。证伪依据：改动前后分别跑 `cargo test --locked` 与上述实机矩阵，若任一单测改行为或实机出现“看门狗被关但主进程仍在”，即判定本篇失败并回滚。
