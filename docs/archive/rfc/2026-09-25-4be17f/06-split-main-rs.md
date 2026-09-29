# Agent Note：把 main.rs 的两组状态机搬出主文件

Status: implemented

## 问题

`src/main.rs` 当前 1470 行，是 `src/` 最大的文件（对照：`src/suspend.rs` 802 行、`src/renderer.rs` 800 行、`src/update/protocol.rs` 743 行）。它同时承载至少六类职责，各自有独立的行段：

CLI 解析与参数（`src/main.rs:177-214`）；单实例互斥量与退出请求（`src/main.rs:216-278`、`src/main.rs:769-771`、`src/main.rs:1003-1023`）；电源/会话/显示三类订阅的注册与配对注销（`src/main.rs:456-655`，其中 `src/main.rs:461`、`src/main.rs:472`、`src/main.rs:489` 是三个近同构函数）；启动与 Explorer 重建编排（`src/main.rs:670-756`）；恢复调度状态机（`src/main.rs:141-152` 的 `thread_local!`、`src/main.rs:657-666`、`src/main.rs:773-990`）；消息路由与定时器分发（`src/main.rs:1032-1261`，其中 `fn handle_timer` 在 `src/main.rs:1106`、`pub extern "system" fn wnd_proc` 在 `src/main.rs:1162` 起 100 行）。`fn main` 自身占 `src/main.rs:280-433`，154 行。

问题不在行数本身，而在于**同一个文件里并存两套互相独立的状态机**：启动/重建编排与恢复调度。任何新增恢复项都要改这个文件里的同一段（例如恢复日程表在 `src/main.rs:887 run_recovery`），冲突面随恢复项数量线性增长。

两组职责之间只通过 `HWND`、既有原子（`src/main.rs:76`、`82`、`87`、`92` 的订阅句柄与 `src/state.rs` 的 `DPI_DIRTY`）与 `src/config.rs` 的常量传递，没有共享私有数据结构——切面已经天然存在。

检索证据：`grep -n "^fn \|^pub fn \|^pub extern" src/main.rs` 共 37 个顶层函数，横跨上述六类；`grep -rn "mod " src/main.rs` 显示 `main.rs` 已用子模块承载 `collector`、`renderer`、`update` 等，本次沿用同一形式而非新建 crate。

## 提案

把两组职责搬到同 crate 内的新模块，只搬不改：

其一，电源/会话/显示订阅组（`src/main.rs:456-655`，含三个近同构的 `register_*` 与配对注销）搬入 `src/power.rs`，对外只保留 `main.rs` 真正需要的入口（注册、注销、补注册三件）。

其二，恢复调度组（`src/main.rs:141-152` 的 `thread_local!`、`src/main.rs:657-666`、`src/main.rs:773-990`，含重试间隔、定时器编排、`recovery_tick`、`run_recovery`、`recover_dpi` 与 DPI 回滚）搬入 `src/recovery.rs`。

搬迁后 `src/main.rs` 只保留：CLI、单实例、启动编排、消息路由与窗口过程。目标行数：**1470 → 911 行**（净减 559 行，其中仍含 8 个用例的 `mod tests`）。原「不超过 700 行」的目标在「只搬两组、其余六类职责与测试模块原地保留」的范围内不可达，故改为按上列实测净减量核对。

## 明确不在本次范围

**不合并 `register_monitor_power_on`（`src/main.rs:472`）与 `register_console_display_state`（`src/main.rs:489`）**：两者订阅不同 GUID（`GUID_MONITOR_POWER_ON` 与 `GUID_CONSOLE_DISPLAY_STATE`），`src/main.rs:485-488` 已载明「S0 息屏只发这一个，而 legacy 是否仍投递属未验证行为」，两者写同一个 `SUSPEND_REASON_MONITOR` 位、由位集幂等吸收。任何「同构即可合并或删其一」的简化都会删掉一条真实的订阅点。

**不改任何消息 ID、定时器 ID 或 `src/config.rs` 常量**：`WM_USER_*`、`TIMER_ID_*` 与 `src/config.rs` 的对应关系是跨模块契约。

**不做行为变更**：本次是纯搬迁。搬迁过程中若发现行为差异，记为独立候选，不在本篇顺带修改。

**不扩大可见性**：搬迁后的函数保持 `pub(crate)` 或私有，只暴露 `main.rs` 真正调用的入口，避免产生「接缝上每个实现都必须支持、却没有消费者使用」的方法。

**不拆 crate**：本仓库是 bin-only、无 lib target，`src/smoke.rs` 依赖 `main.rs` 的内部符号，拆 crate 会破坏这套冒烟用例，属架构级改动。

## 为什么不保留？

最强的反方理由是：拆文件不改变行为，属「看着复杂」的重构；1470 行对 bin-only crate 不构成灾难，而搬迁会打断 git blame、扩大 review 面积，收益只是审美。

逐条回应：第一，缺陷不是行数而是**两套状态机共存于一个文件**，这有可验证的代价——恢复日程表的每次扩展都落在 `src/main.rs:887` 附近，而重建编排就在同文件 `src/main.rs:705` 附近，两者的评审与冲突无法隔离。第二，本次搬迁是**搬不删**：净行数不变、无新依赖、无可见性扩张，按简化审计的判据不构成「可删表面积」的增加；搬出的每个函数都保留原有调用链（订阅组被启动与重建路径调用，恢复组被看门狗定时器调用），不会出现无处调用的公开方法。第三，搬迁可整体回退，是纯机械提交，不含语义变更，风险面比任何行为改动都小。

结论：本篇的判据是「两套状态机的评审与冲突面必须可隔离」，不是美学。若未来审计认为该拆分无价值，可整体回退——它不含语义变更。

## 验收标准

默认门禁通过且数量完全不变：`cargo test --locked` 输出 `113 passed; 0 failed; 4 ignored`。

符号集合前后一致：搬迁前 `grep -n "^fn \|^pub fn \|^pub extern" src/main.rs` 命中 37 处，搬迁后 `src/main.rs` 命中 15 处、`grep -rn "fn " src/power.rs src/recovery.rs` 命中 30 处，并集覆盖搬迁前全部符号（人工核对，`grep` 只能定位）。无符号丢失；**新增 2 个接缝**：`recovery::rebuild_retry_idle`（`main.rs` 的重建编排读取「重试序列是否空闲」的唯一只读判据，替代原先对 `REBUILD_RETRY_INTERVAL_MS` 的直接读，使该状态保持单属主）与 `power::store_session_notify_raw`（`#[cfg(test)]` 预置句柄，供随实现搬入 `recovery` 的能力表用例）。

真 Explorer 冒烟全绿：`cargo test --locked -- --ignored --test-threads=1` 四条，尤其 `src/smoke.rs:132 rebuild_rebinds_new_main_window` 与 `src/smoke.rs:215`、`src/smoke.rs:233` 两条依赖定时器与挂起状态的用例。

`src/main.rs` 行数下降并记录在提交信息中（实测 1470 → 911，净减 559 行）。

四条门禁按 `AGENTS.md` 逐条执行，`cargo fmt` 先行。

## 风险

搬迁恢复调度组会改变 `thread_local!` 状态的作用域：重建重试间隔与恢复间隔的持有者随调度组搬进 `src/recovery.rs`，而调用点仍留在 `src/main.rs`（`handle_timer` 在 `src/main.rs:609`、`wnd_proc` 在 `src/main.rs:665`、看门狗过程与 `rebuild_main_window` 在 `src/main.rs:426`）。这些状态有「唯一属主」语义，必须整体移动，不得在两个模块各留一份，否则会引入同一事实两份表示。跨模块只暴露一个只读判据 `recovery::rebuild_retry_idle()`，武装/撤销入口都留在 `recovery` 内。

搬运订阅组要求保持「窗口销毁前先注销」的顺序约束（原先的载明处 `src/main.rs:611-614` 已随实现搬走）：注销必须在 `DestroyWindow` 之前完成，搬迁后调用点顺序不变（`src/main.rs:429-432`），但该约束从「同一文件内可见」变成「跨模块」，因此在 `src/power.rs` 的模块头写明了它，否则后续改动容易破坏它。

本仓库无 lib target，`src/smoke.rs` 通过 `super::` 访问 `main.rs` 内部符号；搬迁后这些路径需要同步调整（订阅组与恢复组的观测入口改为 `crate::power::*` / `crate::recovery::*`，`super::CURRENT_MAIN_HWND`、`super::rebuild_main_window`、`super::bind_display_and_timers` 等留在 `main` 的符号不动），冒烟用例的编译本身就是该调整的验证手段（若漏改会编译失败，不会静默通过）。
