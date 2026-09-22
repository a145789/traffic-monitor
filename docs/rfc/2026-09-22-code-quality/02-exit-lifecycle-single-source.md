# Agent Note：退出与启动生命周期的单点真值收口

Status: proposed

## 问题

报告的独立观察指出本仓真正的架构风格是「每个跨消息/跨线程的事实收口到一个可指认的真值源」，并建议写进 AGENTS 作为第 10 条。按这条判据复核，退出与启动两条生命周期路径上现有四处不符：

1. **退出幂等门只覆盖三条入口中的一条，且 claim 悬在调用链中间。** `src/main.rs:71-73` 写明「退出序列（托盘清理 + `PostQuitMessage`）只应执行一次」，但 `claim_exit_request`（`src/main.rs:470-472`）只被 `route_exit_request`（`src/main.rs:528-533`）调用；而这条路径是全仓唯一的三层调用链——`WM_USER_QUIT_REQUEST`（`src/main.rs:571-572`）→ `route_exit_request` 先 claim → `finish_exit_from_watchdog`（`src/main.rs:517-520`，负责 `disarm_rebuild_retry`）→ `begin_exit`（`src/main.rs:505-512`）。另外两条入口完全不过门：`WM_CLOSE`（`src/main.rs:737-740`）直接调 `begin_exit`，`handle_update_action`（`src/update/mod.rs:806-816`）直接执行收尾。并发到达时清理与 `PostQuitMessage` 会执行两遍——两操作本身幂等、无实际危害，但「只执行一次」这条不变量在文档与实现之间已不成立；且 claim 悬在链中间，使「把门收到 `begin_exit`」这一步极易写成「两处 claim 并存」（后果见提案 1）。
2. **退出收尾有两份实现。** `begin_exit`（`src/main.rs:505-511`）的注释写着「两个调用方…共用这一份实现，避免退出语义在两处漂移」，实际是全仓 `PostQuitMessage` 的非导入、非注释命中恰好两处：`src/main.rs:510` 与 `src/update/mod.rs:814`；`remove_tray_icon` 同样两处。
3. **启动失败早退不清理已建资源。** `bind_display_and_timers`（`src/main.rs:369-394`）的顺序是「先建托盘图标（`src/main.rs:371`），最后才返回 `sync_monitoring_timers`（`src/main.rs:393`）」；`main()` 在 `src/main.rs:255-258` 收到 false 时弹框后直接 `return`，托盘图标未 `remove_tray_icon`，任务栏会留下悬空图标直到进程退出，渲染器也未归还。`SetTimer` 失败概率极低，但这是「退出序列只有一份实现」原则的直接反例。
4. **托盘状态与事实脱钩。** `remove_tray_icon`（`src/tray.rs:89-98`）删除图标后不清空 `TRAY_DATA`，靠「重复 `NIM_DELETE` 无害」兜底；同一原因使 `create_tray_icon` 的失败早退（`src/tray.rs:35-38`、`:69-72`）可能让 `TRAY_DATA` 残留旧句柄，后续移除/重建路径据此误判图标仍在。

另有**两处测试触碰进程级全局**：`src/main.rs:832` 的 `CURRENT_MAIN_HWND.store_raw` 与 `src/update/mod.rs:1082` 的 `UPDATE_IN_PROGRESS`；两处注释都已自知并声明「只留一处」，但在 `cargo test` 并行执行下仍是地雷。

## 提案

1. **把 claim 迁移到 `begin_exit`（收敛为唯一门），而不是新增一处。** 具体做法：`begin_exit` 开头 `if !claim_exit_request(&EXIT_REQUESTED) { return; }`，**同时删除 `route_exit_request` 里现有的那句 `claim_exit_request`**，使该函数退化为「`finish_exit_from_watchdog`（`disarm_rebuild_retry` + `begin_exit`）」。⚠️ 若不删旧 claim，就会形成两处 claim 并存：`--quit` 路径已在 `route_exit_request` 置位，`begin_exit` 的第二次 `swap` 必返回 `true` → 提前 return → **托盘不清理、`PostQuitMessage` 不投递、进程根本不退出**，子进程只能白等 5 秒后落到安装器 taskkill 兜底。本项把「只执行一次」从注释承诺变成代码事实；`src/main.rs:520-527` 的论证（「直接执行让幂等门恰好对应『退出序列已执行』这一不可逆事实」）随之移写到 `begin_exit` 上方。
2. `handle_update_action`（`src/update/mod.rs:806-816`）收敛为两步：`UPDATE_IN_PROGRESS.store(false, Ordering::Release)` + 调 `crate::main::begin_exit()`（可见性提到 `pub(crate)`），删掉与 `begin_exit` 逐行重复的 `remove_tray_icon` 与 `PostQuitMessage` 尾段。该路径不触碰 `EXIT_REQUESTED`，因此 `begin_exit` 的门会正常放行；`src/update/mod.rs` 已在反向引用 `crate::tray` / `crate::util`，不引入新的依赖方向类型。本项须排在提案 1 之后。
3. 启动失败分支（`src/main.rs:255-258`）补 `remove_tray_icon()`；是否补 `renderer::take_renderer()` 由实施者定——进程随后即退出、OS 会回收 GDI，补它只为语义一致，建议补（一行）。
4. `src/tray.rs:89-98` 在 `NIM_DELETE` 后把 `TRAY_DATA` 置 `None`；`create_tray_icon` 的两条失败早退同样保证 `TRAY_DATA` 为空，让「`TRAY_DATA` 非空 ⟺ 图标存在」重新成为可依赖的不变量。
5. （文档）`AGENTS.md` 增第 10 条：「新增跨消息/跨线程事实时，必须指认唯一真值源并写明竞争处理」。现有 6 个真值源（`EMBEDDED`、`EXIT_REQUESTED`、`UPDATE_IN_PROGRESS`、`LAST_CHECK_TIME`、`SUSPEND_REASONS`、`LAST_RENDERED_VALUES`）已按此风格写注释，这条是把既有隐性惯例显性化，符合 AGENTS 自己的更新指南（引入新的隐式设计约束）。

## 明确不在本次范围

- `--quit` 的轮询与重试语义不变：`claim_exit_request` 的 `swap` 语义与 `exit_request_gate_accepts_only_first_request`（`src/main.rs:817-826`）原样保留。
- `EMBEDDED` / `SUSPEND_REASONS` / `LAST_RENDERED_VALUES` 等既有真值源设计不动——它们是本笔记要对齐的正例。
- `installer.iss` 的强杀兜底（`installer.iss:113-130`）属更新交接链路，见 `03-release-chain-and-observability.md`。
- 不改 `begin_exit` 的触发时机（`WM_CLOSE` 由托盘菜单 `src/tray.rs:240-242` 投递、`EXIT_MAIN` 由子进程协议触发），只收敛清理动作的归属。
- 不去解耦测试对进程级全局的触碰（`src/main.rs:832` 的 `CURRENT_MAIN_HWND.store_raw`、`src/update/mod.rs:1082` 的 `UPDATE_IN_PROGRESS`）：收益小、需重构测试骨架，且这两个全局的读本身是设计的一部分。改注释即止。

## 为什么不保留？

反方其一：当前双执行无实际危害（`Shell_NotifyIconW(NIM_DELETE)` 对已删图标只是失败返回，`PostQuitMessage` 对已退出线程再投一次也无副作用），改 `begin_exit` 签名与 `handle_update_action` 会碰到 AGENTS 第 4 条保护的更新交接路径，属「为洁癖冒回归风险」。其二：让 `remove_tray_icon` 清状态会使「重复删除」这条兜底消失。回应：AGENTS 第 4 条保护的接缝是「`EXIT_MAIN` 必须在启动安装器之前发出、随后轮询互斥量消失」这一时序，本笔记只动收尾清理的归属、不动时序，且 `src/update/mod.rs` 里钉时序的用例保持绿即为证据；至于兜底消失，清理后语义从「无害空转」变为「无操作」，二者等价，换来的是状态与事实一致。保留旧形态的成本是每新增一个退出入口都要重新判断「要不要过门」，而 AGENTS 自己把「文档与实现漂移」列为最危险的一类。

## 验收标准

- `grep -n 'PostQuitMessage' src/` 的非导入、非注释命中只剩 `begin_exit` 一处；`grep -n 'remove_tray_icon' src/` 的非定义命中只剩 `begin_exit`。
- `grep -n 'claim_exit_request' src/` 的命中恰好两处，且都在 `src/main.rs`：定义（`src/main.rs:470`）与 `begin_exit` 内的**唯一**调用；`route_exit_request` **不得**再出现该调用——出现即说明落成了「两处 claim 并存」的错误实现。
- `grep -n 'TRAY_DATA' src/tray.rs` 的写入点在「成功创建」与「成功删除」两侧对称出现。
- `exit_request_gate_accepts_only_first_request`、`test_reset_update_progress_clears_global_flag` 保持绿；`cargo test --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt` 全绿。
- 人工三条路径：托盘菜单「退出」后图标即时消失；`--quit` 后进程**确实退出**（`src/main.rs:121-144` 的 `quit_existing_instance` 是「投递后轮询看门狗消失」，若清理未执行它会白等到超时）——这条是本笔记最容易写错的地方，现有单测覆盖不到；`--quit` 与菜单退出并发投递时清理恰好一次（可临时加 `diag!` 观察，验证后移除）。

## 风险

- **本笔记最大的回归面**：claim 迁移若只做一半（`begin_exit` 加了门、`route_exit_request` 的旧 claim 没删），`--quit` 会变成「进程不退出」，而现有单测**不会**报红（`exit_request_gate_accepts_only_first_request` 只测 `claim_exit_request` 本身）。因此实施顺序固定为「先删旧 claim、再加新门」，并在 PR 描述里贴出 grep 结果与上面那条人工验证。
- `begin_exit` 过门后存在理论窗口：「已置位但 `PostQuitMessage` 尚未生效」期间到达的新请求会被吞掉。既有论证（`src/main.rs:520-527`）认为该窗口不存在（置位即序列已开始、退出不可逆），实施时须把同一论证落到 `begin_exit` 注释，否则后人无法审计这条取舍。
- `update` 调 `main` 引入新的调用方向；若审查不接受，替代方案是把 `begin_exit` 提到双方共依赖的小模块，但那是第三条路径，与「收口」目标相反。
- 第 4 项使「重复 `NIM_DELETE` 无害」的隐性兜底消失，需确认没有路径依赖「删除后 `TRAY_DATA` 仍可复用」——Explorer 重启的重建序列会先 `create_tray_icon` 再绑定，重读一遍 `src/main.rs` 的重建函数即可证伪该风险。

