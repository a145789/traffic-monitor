# RFC 0011: 更新动作协议收敛与残留重复状态清理

- **状态**: Implemented (已实施)
- **创建时间**: 2026-09-01
- **更新记录**:
  - 2026-09-01: 初稿。对当前 HEAD（4420f79）全部 17 个源文件 + build.rs / installer.iss / scripts / .github / README 做逐符号消费方审计后，仍可验证的剩余候选。
  - 2026-09-01: 复核修正（依据外部逐条复核意见，全部采纳）：2.1 补发分支条件引述更正为 `!outcome.is_error`，并补「对 post 失败场景同样无效」论据；2.5 删去理论化的竞态论据，改为镜像状态消除 + 数据流显式化；2.3 复位对补计 network.rs 第三处、`reset_network_backoff()` 归属改 state.rs；§1 补行号基线与符号定位提醒。
  - 2026-09-01: 已实施于 71b4cf2（分支 `refactor/rfc-0011-update-protocol-cleanup`），2.1–2.6 全部落地；另将 2.1 读取循环抽为 `scan_subprocess_protocol`，把「读到 EXIT_MAIN 即转发且仅转发一次」不变量升级为测试钉死。
- **关联文件**: `src/update/mod.rs`, `src/main.rs`, `src/suspend.rs`, `src/collector/network.rs`, `src/collector/mod.rs`, `src/window.rs`, `src/update/crypto.rs`

---

## 1. 背景与现状 (Context)

审计方法：通读全部生产语料后，对每个候选符号用 `rg` 区分生产消费方（src / 脚本 / CI / 安装器）与测试、文档消费方，并逐条核对调用点数据流；config.rs 全部常量逐一核对均有真实消费方，无死常量。上一轮（2026-08-25）审计的删除与收敛项已在当前代码全部落地，本轮收编的是其后仍存留、或随实现演进新引入的面。全部条目互相独立、可各自成 PR；除 2.2 为纯内部协议载荷收窄外均零行为变化。文中行号以审计基线 4420f79 为准；实施时若已有新提交落地，请按符号定位而非行号。

## 2. 核心重构项

### 2.1 `update/mod.rs`：不可达的 EXIT_MAIN 补发分支与 `SubprocessOutcome.action` 死字段

**现状**：`run_check_subprocess` 的读取循环（551-585 行）内 `parsed_action` 与 `exit_signalled` 满足不变量：`parsed_action == Some(ExitMain) ⟹ exit_signalled == true`——二者在同一迭代内先后置位（565-573 行）。`update_check_worker` 211 行 `if outcome.exit_signalled { return; }` 先于 215-220 行的补发分支执行；补发分支自身的条件是 `outcome.action == UpdateAction::ExitMain && !outcome.is_error`，但控制流根本到不了这里——带 `ExitMain` 的 outcome 必然 `exit_signalled == true`、必然在 211 行提前返回，故该分支**不可达**（三处 `SubprocessOutcome` 构造——523、590、598 行——的 `action` 均取自 `parsed_action`）。同时 `action` 字段除这个不可达分支外全仓零读者；`post_update_action` 的 bool 返回值在 573 行调用处被丢弃，唯一读者同样是死分支里的 217 行。

**方案**：删除 215-220 行补发分支；`SubprocessOutcome` 收为 `{ is_error, exit_signalled }` 两字段；`post_update_action` 返回 `()`。`parsed_action` 局部变量保留（600 行 `is_error` 判定依赖 `parsed_action.is_none()`）。

**放弃什么**：一个「万一没转发就补发」的冗余兜底。需写明：这个兜底连 `PostMessageW` 失败都兜不住——573 行丢弃返回值、且 `exit_signalled` 在 post **之前**置位，转发失败时 worker 同样在 211 行提前返回，补发分支不会执行。它从未兜住过任何场景，删除零损失；转发失败路径的真实兜底是子进程 5s 超时后照常启动安装器 + 安装器内 taskkill（既有设计）。若未来把「读到即转发」挪出读取循环（如改为子进程退出后统一转发），需重新设计转发时机并补测试——届时也不应依赖一个当前不可达的分支充当保险。

### 2.2 单动作协议的载荷收窄

**现状**：`WM_USER_UPDATE_ACTION` 的 wparam 全仓只有一个取值 `UPDATE_ACTION_EXIT_MAIN = 1`（49 行定义；573 行与死分支 217 行两个发送点同值）。`handle_update_action` 623 行 `if action != UPDATE_ACTION_EXIT_MAIN { return; }` 在防御一个不存在的多动作协议；main.rs 442 行调用点透传 `wparam.0`。

**方案**：删除 `UPDATE_ACTION_EXIT_MAIN` 常量；`post_update_action(hwnd)` 去掉动作参数；`handle_update_action()` 无参化——收到该消息即语义「主进程退出并清理托盘」。消息号 `WM_USER_UPDATE_ACTION` 与 wnd_proc 路由不动。

**放弃什么**：将来新增第二种更新动作需重新引入载荷或新消息号。WM_USER 偏移空间充足（+3/+4/+5 已用、+100 托盘），重新引入成本一行。

### 2.3 `suspend.rs`：`resume_system` 的 `reset_backoff` 死参数与退避复位双写

**现状**：`resume_system(hwnd, reason, reset_backoff)`（53 行）的全部 3 个调用点（70 / 80 / 99 行，对应系统唤醒、显示器点亮、解锁）恒传 `true`，`false` 分支无任何生产或测试消费方。`NETWORK_BACKOFF = false` + `CONSECUTIVE_ZERO_COUNT = 0` 这对复位共手写**三处**：main.rs 434-435 行（`WM_USER_NETWORK_RECONNECTED` 处理器）、suspend.rs 56-57 行（`resume_system` 内）、network.rs 132 / 134 行（重连检测分支，投递消息前先复位一遍——处理器内的复位因此是冗余的保险写法）。main.rs 与另两处的 store 顺序相反（语义等价，均在 `sync_monitoring_timers` 读状态前完成）——同一对状态多种写法、顺序不一，正是双写漂移的早期形态。

**方案**：删除 `reset_backoff` 参数（恢复即复位，与现状逐调用点等价）；抽 `pub fn reset_network_backoff()` 放 **state.rs**——两个原子量的家，与 `SuspendReasons` 把位协议封装在状态属主处的既有做法一致；main.rs 与 suspend.rs 均已依赖 state，不新增依赖边（退避状态机的置位/自增属主虽在 network.rs，但 suspend.rs 反向调用 collector 会新增依赖边，state.rs 是零新边的中立归属，理由写入 helper 注释）。main.rs 处理器与 `resume_system` 改调 helper；network.rs 132 / 134 行的条件投递逻辑保持原样（其「先探测再条件复位」与纯复位不同型，不强行并入）。

**放弃什么**：假想中的「恢复但保持退避」场景（如息屏唤醒时仍离线，可省去重新进入退避前的 5 个 1s 采样 tick）——当前从未启用过该分支；真有需求时以显式参数重新引入，比保留一个恒真参数更清晰。

### 2.4 `update/mod.rs`：`start_auto_check` / `start_manual_check` 的生成样板复制

**现状**：两函数（139-169 / 171-189 行）各复制同一段 64KB 栈 `Builder::spawn` worker + spawn 失败复位 `UPDATE_IN_PROGRESS` 的样板（160-168 / 180-188 行），差异仅在 auto 版的两道前置门（`ENABLE_AUTO_UPDATE`、`LAST_CHECK_TIME` 冷却）。调用方：main.rs 199 / 399 / 437 行（auto）与 tray.rs 222 行（manual），外部签名不受影响。

**方案**：抽私有 `fn spawn_update_worker(hwnd: HWND, is_manual: bool)`（仅 spawn + 失败复位）；`start_auto_check` 保留两道门与占坑后调用，`start_manual_check` 占坑后调用。占坑（`UPDATE_IN_PROGRESS.swap`）与冷却门保持在各入口原位置不动，避免改变门序。

**放弃什么**：无。纯复制面收敛。

### 2.5 主窗口句柄双静态：删除 `MAIN_HWND_NETWORK` 与 `init_network_listener`

**现状**：main.rs 65 行 `CURRENT_MAIN_HWND` 与 network.rs 28 行 `MAIN_HWND_NETWORK` 两个 `AtomicIsize` 镜像同一事实（当前主窗口句柄）。`init_network_listener`（36-38 行）已退化为纯 setter，两个调用点（main.rs 169 / 325 行）都紧随 `CURRENT_MAIN_HWND.store` 之后；函数名承自早期地址变更监听线程设计，现无任何「监听」语义（重连检测实际由 `collect_network` 轮询内完成）。`MAIN_HWND_NETWORK` 唯一读者是 `post_to_main`（144 行），而 `collect_network` 的唯一调用点 main.rs 381 行 `handle_timer` 手里就有当前 hwnd。

**方案**：`collect_network(hwnd: HWND)` 改收参数；删除 `MAIN_HWND_NETWORK` 静态、`init_network_listener` 函数、collector/mod.rs 12 行的再导出、main.rs 的 import（36 行）与两处调用。`CURRENT_MAIN_HWND` 保持 main.rs 私有（退出清理与 Explorer 重建仍用）。本项的真实收益是**消除镜像状态**（两个静态承载同一事实，迟早漂移）+ 删掉 setter / 再导出样板 + 把「采样结果投递给谁」的数据流显式化。需说明：`collect_network` 与 `rebuild_main_window` 同在 UI 线程串行执行，并不存在「参数比静态更即时」的交错窗口——参数传递只是不劣于现状的附带属性，不作为论据。

**放弃什么**：`collect_cpu` / `collect_memory` / `collect_network` 三者签名不再齐整（仅 network 收 hwnd）。以显式数据流换签名一致性，值得。

### 2.6 顺带小项

- **`check_fullscreen` 的 `MONITORINFOEXW` → `MONITORINFO`**（suspend.rs 236-242 行）：只读 `rcMonitor`，`szDevice`（32 个 WCHAR）每 2s 白初始化一次，239 行的双重指针转型 `as *mut MONITORINFOEXW as *mut _` 随换型一并删除。
- **`window::calc_widget_rect` 降私有**：消费方（167、251 行）全在 window.rs 内部。
- **`crypto::Sha256` 降私有**：消费点（75、83、146 行）全在 crypto.rs 内部；对外只保留 `compute_sha256_hex` / `compute_sha256_hex_file`。

## 3. 验收标准

- `rg` 零命中：`reset_backoff`、`UPDATE_ACTION_EXIT_MAIN`、`init_network_listener`、`MAIN_HWND_NETWORK`、`MONITORINFOEXW`（src 内）、`SubprocessOutcome` 的 `action` 字段访问。
- `handle_update_action` / `post_update_action` 均无动作参数；`collect_network` 收 hwnd 参数；两个更新入口的公开签名不变。
- 更新全流程行为不变：自动/手动检查、`EXIT_MAIN` 即时转发、主进程退出让出 exe 映像、UAC 取消后 relaunch，逐路径手测。
- 断网退避 → 重连恢复、锁屏/息屏 → 唤醒路径行为不变（退避复位时序与现状等价）。
- `cargo test`、`cargo build --release`、`cargo clippy -- -D warnings`、`cargo fmt` 全部通过。

## 4. 实施检查清单

- [x] **2.1** 删 215-220 行补发分支；`SubprocessOutcome` 去 `action` 字段；`post_update_action` 返回 `()`；在读取循环处补一行注释钉死「读到 EXIT_MAIN 即转发且仅转发一次」的不变量
- [x] **2.2** 删 `UPDATE_ACTION_EXIT_MAIN`；`post_update_action(hwnd)` / `handle_update_action()` 无参化；main.rs 442 行调用点同步
- [x] **2.3** 删 `reset_backoff` 参数；在 state.rs 抽 `reset_network_backoff()`（注释写明归属理由）并在 main.rs / suspend.rs 两处调用
- [x] **2.4** 抽 `spawn_update_worker`；两个入口改调，门序不动
- [x] **2.5** `collect_network(hwnd)` 参数化；删静态 / setter / 再导出 / import / 两处调用点
- [x] **2.6** `MONITORINFO` 换型并删转型；`calc_widget_rect`、`Sha256` 降私有
- [x] 全量 `cargo test` + `cargo build --release` + `cargo clippy -- -D warnings` + `cargo fmt`
- [ ] 真机验证：更新检查（含手动与 UAC 取消）、断网退避恢复、息屏唤醒、Explorer 重启重建

## 5. 非目标与保留项（附当前代码理由）

- **`--quit` / `quit_existing_instance`（main.rs 67-88 行）**：README「命令行参数」节文档化的可用用户面（README.md:59）；安装器与脚本虽未调用，但功能真实可用，删除属产品决策而非清理。
- **UI 线程独占状态使用原子量**（`NET_SPEED_*`、`CPU_USAGE`、`MONITOR_FULLSCREEN` 等十余个 static）：`static` 要求 `Sync`，原子量是安全全局可变状态的最简形式；真正跨线程的仅 `UPDATE_IN_PROGRESS`、`LAST_CHECK_TIME`、`TRIM_BOOKKEEPING` 三处。降级 thread_local 平添 `.with()` 样板、无净删除，不动。
- **WinHTTP / BCrypt / 版本解析不换第三方 crate**：AGENTS.md 决策 4 的 `/DELAYLOAD` + re-exec 子进程隔离红线；注册表场景已由 `windows-registry` 覆盖。
- **`ffi_guard` 模块存续、`OwnedGdi<T>` 泛型、`rate.rs` 独立文件、两套窗口类注册不合并**：AGENTS.md 决策 8 与架构表既定的归属规则与注释契约。
- **`LAST_RENDERED_VALUES` 增量重绘门、`TimerPlan` 状态机、`TrimBookkeeping` 自适应水位、`SuspendReasons` 位集**：各自承担已注释记录的省电 / 防抖 / 暂停对称性职责（AGENTS.md 决策 7），非投机面。
- **哈希比对处 `to_uppercase()` 双重归一化、http.rs 15000ms 超时未入 config.rs**：零删除收益的约定一致性微调，不立条目；若顺手处理，以一行 TODO 标注即可。
- **Cargo.toml feature 瘦身**：feature 门控存在非直觉交叉（Cargo.toml 27 行注释：`ShellExecuteExW` 实际由 `Win32_System_Registry` 门控），逐项移除需 `cargo check` 编译实验背书，收益仅编译时间，不值得开条目。

## 6. 风险

- **2.1 / 2.2**：删除的是不可达的冗余兜底，实施时须按 2.1 的不变量逐行复核（尤其 590 行 `child.wait()` Err 分支的 `unwrap_or(Done)`），并靠新增注释防止未来重构悄悄改变不变量。
- **2.5**：签名变化属编译期可发现风险；行为等价性依赖「WM_TIMER 随窗口销毁而消失、tick 携带的 hwnd 即当前窗口」这一 Win32 语义，不存在陈旧句柄 tick。
- **2.3**：恒真参数内联后逐调用点等价；helper 内两个 store 的先后顺序任选（三处现状顺序不一且语义等价），加注释说明即可。
- **2.4 / 2.6**：纯收敛 / 可见性降级 / 换型，编译器全程把关，低风险。
