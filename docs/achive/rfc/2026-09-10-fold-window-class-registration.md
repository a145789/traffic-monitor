# Agent Note：折叠主窗口与看门狗窗口的重复注册/创建代码

Status: implemented

## 问题

[`register_window_class`](../../../src/window.rs) 与 [`register_watchdog_class`](../../../src/window.rs) 除类名（`WINDOW_CLASS` / `WATCHDOG_CLASS`）、窗口过程（`wnd_proc` / `watchdog_wnd_proc`）与错误文案外逐行相同；[`create_main_window`](../../../src/window.rs) 与 `create_watchdog_window` 同样共享“宽串类名 → `module_instance` → `CreateWindowExW` → `map_err`”骨架。生产调用方只有 [`main.rs`](../../../src/main.rs) 启动序列中的四处相邻调用（注册×2、创建×2，重建路径复用 `create_main_window`），重复体的真实成本是分叉风险：任一分支修了 `cbSize`、`hInstance` 或错误处理，另一分支极易漏改，而两个类在语义上必须保持“除过程与可见性外一致”。

## 提案

新增一个私有 `register_class(class_name: &str, proc, err: &str)` 与一个私有 `create_window(class_name, title: &[u16], width, height, style, ex_style, err)`（或等价的二参泛化），让现有四个公有函数各剩 3~5 行转发，保持公有签名、`WATCHDOG_CLASS` 常量、两套窗口过程与现有中文错误文案不变；其中尺寸参数不可少（主窗口 `DISPLAY_WIDTH/HEIGHT` vs 看门狗 `0,0`），标题统一为 `&[u16]`（主窗口现有 `Vec<u16>` 与看门狗 `w!("")` 的 `PCWSTR` 归一）；看门狗“永不嵌入、永不显示、唯一 `TaskbarCreated` 接收者”的设计约束（AGENTS.md 第 5 条）不受任何影响，重建路径仍调用 `create_main_window`。同步检查 `quit_existing_instance` 中手拼 `WINDOW_CLASS` 宽串的一处，复用同一转换入口。

## 为什么不保留？

反方理由是“两个窗口语义不同（任务栏子窗口 vs 隐藏顶层），显式重复比抽象更清晰”。但本提案不合并语义，只收敛完全一致的 Win32 样板（`WNDCLASSEXW` 填充、`RegisterClassExW` 返回值检查、`CreateWindowExW` 错误映射），调用点仍保留两个具名公有函数，阅读时意图不丢失；真正的语义差异（样式标志、父窗口、显示方式）本就写在调用参数里，抽象后反而更醒目。

## 验收标准

`window.rs` 中 `RegisterClassExW` 与 `CreateWindowExW` 各只出现一次（测试注释除外）；四个公有函数签名与行为不变；Explorer 重启重建、看门狗 `TaskbarCreated` 恢复、多 DPI 嵌入路径手工验证通过；`cargo test`、`cargo build --release`、`cargo clippy -- -D warnings` 全绿。

## 风险

几乎纯重构风险：泛化参数若选错（如把主窗口的 `WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` 与看门狗的样式搞混）会导致窗口不可见或无法接收广播。缓解：泛化函数只收敛完全相同的字段赋值，把样式/扩展样式/标题作为调用方显式参数，diff 应显示行为零变化；`embed_in_taskbar` 的严格 API 顺序（AGENTS.md 第 1 条）不在本次改动范围内。
