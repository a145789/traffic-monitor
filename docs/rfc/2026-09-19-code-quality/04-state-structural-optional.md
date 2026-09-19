# Agent Note：状态组织收敛与可选健壮项及暂缓项裁决

Status: proposed

## 问题

本篇收容需逐项评审的结构收敛（E1、E3、D3、B4）与按需可选的健壮项（A4、A5、F1、G），并对 E2、D1 给出暂缓裁决：E1 是 `src/collector/cpu_mem.rs:8-11` 四个全局原子表达“CPU 基线”一个概念，且只在 UI 线程 `WM_TIMER` 读写，原子与内存序无真实保护；E3 是 `src/main.rs:110-267` 的 155 行九阶段编排（参数、单例、IME、类注册、看门狗、主窗口、绑定、消息循环、清理）；D3 是 `src/window.rs:54-63` 七参 `create_window` 加 allow；B4 是 `src/suspend.rs:216` 的 `check_fullscreen` 全屏退出边沿写两遍；A4 是 `src/tray.rs:64-65` 的 `NIM_ADD/NIM_SETVERSION` 失败被吞（v0 语义下菜单静默失灵）；A5 是 `src/renderer.rs:427` 的 `update_dpi` 半失败态（窗口新尺寸加位图旧尺寸）；F1 是全仓 `let _ =` 无诊断通道（原 `eprintln!` 方案已证伪，见下）；G1/G2/G4 是百纳秒级微项。

## 提案

逐项独立提交、可单独回滚：E1 收敛为 `thread_local! { CPU_BASELINE: Cell<Option<CpuTimes>> }`（`None` 即无基线），`collect_cpu` 一次 `with` 加 `match`，`reset_cpu_baseline` 变 `set(None)`；E3 只切两刀（`main.rs:235-253` 提 `run_message_loop()` 顺带收 E4 残留的全限定路径，`main.rs:124-142` 提 `init_single_instance() -> Option<MutexGuard>`），阶段顺序与错误早退路径不变；D3 收敛为 `WindowSpec { class_name, title, width, height, style, ex_style }` 且错误文案由 `create_window` 按类名统一生成，调用方不再传 `err`；B4 抽 `on_fullscreen_edge(hwnd, was, now)` 并由两分支调用，`timer_plan` 系列测试不受影响；A4 让 `create_tray_icon` 返回 `bool`、失败不写 `TRAY_DATA`，`NIM_SETVERSION` 失败走一次 v0 兼容重试；A5 在位图或字体创建失败时维持旧尺寸嵌入（回滚本轮窗口改动，下个成功周期自愈）；F1 采用 `OutputDebugStringW` 的 `diag!` 宏（`#[cfg(debug_assertions)]` 门控，release 空展开，需在 `Cargo.toml` 补 `Win32_System_Diagnostics_Debug` feature），埋点限托盘失败、`sync_monitoring_timers` 失败、`embed_in_taskbar` 失败等十余关键静默点，不引入 tracing/log；G 只顺手做 G1（`Layout` 缓存到 `Renderer`）、G2（箭头 `const [u16; 2]`）、G4（KB 满 `10240` 进位 MB 含测试更新），G3（`GetIfTable2`）与 G5（`opt-level = "z"`）明确保持；E2（`src/collector/network.rs:88-131` 三层闭包并为单一 `SamplerState`）暂缓，落地前置条件为 PR 内写出无嵌套借用证明（`with_virtual_blacklist` 唯一调用方为 `collect_network`、`CURRENT_DATA` 只在闭包内、`INTERFACE_HISTORY` 只在采样与测试中、持借用下无二次 `with`），且承认维持现状完全可辩护；D1（`src/state.rs:15-19` 裸 `u32`）降 P3，重评触发条件为新增第 4 个暂停原因或 API 对外暴露。

## 明确不在本次范围

UI 采集线程化、`with_renderer` 改 `GWLP_USERDATA`、引入 clap/thiserror/log、AGENTS.md 第 1 至 7 条 Win32 时序、E2 在证明写出前的任何代码改动、D1 在触发条件满足前的新类型，全部不做；`suspend.rs:299` 低频编码维持现状。

## 为什么不保留？

最强反方是“E1 原子改 `Cell` 是为将来多线程关门”。回应：当前生产消费者全在同一 UI 线程（`TIMER_ID_CPU_MEM` tick），`Cell` 与 x86 Relaxed 同价且删掉误导性内存序注释，若将来真要线程化，那时连同定时器重建一起设计，现在留原子只是虚假安全感；次强反方是“F1 的 ODS 无人看等于白埋”。回应：白埋零成本（release 空展开），且托盘失灵、定时器未起这类现场问题今天只能靠读代码猜，debug 加 DebugView 是唯一不污染发布行为的通道；E2 暂缓的最强反方已在报告 v1.1 回应：今天借用域本就链式拉满（`src/collector/network.rs:265-267` 的 `borrow()` 横跨整个采样闭包），合并只把三个分散锁域并为一个，证明义务仍在提议方。

## 验收标准

`cargo test --locked`（含 `timer_plan` 系列、CPU 基线首轮行为、托盘与 DPI 相关用例）全绿，`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt` 全绿；`grep -n "PREV_IDLE_TIME\|PREV_KERNEL_TIME\|PREV_USER_TIME\|CPU_INITIALIZED" src/collector/cpu_mem.rs` 零命中且 `grep -n "CPU_BASELINE" src/collector/cpu_mem.rs` 命中；`grep -n "too_many_arguments" src/window.rs` 零命中；`grep -rn "SamplerState" src/collector/network.rs` 在 E2 满足前置条件前零命中（暂缓即不出现）；`grep -rn "OutputDebugStringW" src` 仅出现在 `diag!` 定义与调用点。

## 风险

真实残留风险三处：一是 E1 的 `thread_local` 在测试线程与 UI 线程各一份，测试若沿用全局断言会串味，证伪为现有 CPU 相关测试必须逐个确认线程归属；二是 E3 提函数时 `MutexGuard` 生命周期（`_mutex_guard` 须活到消息循环结束）被缩短，证伪为 guard 由 `init_single_instance` 返回并在 `main` 栈帧持有，diff 必须可见该绑定；三是 F1 的 `OutputDebugStringW` 宽字符串尾 NUL 与 feature 缺失导致编译失败，证伪即编译器本身，不猜测，一次过门禁为准。
