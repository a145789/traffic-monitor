# Agent Note：处理嵌入子窗口的 DPI 变化通知（WM_DPICHANGED_AFTERPARENT）

Status: rejected（复核裁决：触发依赖缩放变更这一低频事件且可能被任务栏重建掩盖，感知/频率不足，不实施）

## 问题

组件嵌入任务栏后是**另一个进程顶层窗口的子窗口**（`src/window.rs:292` 的 `SetParent(hwnd, h_taskbar)`）。而 Per-Monitor v2 下，DPI 变化的通知形态对子窗口是不同的：`WM_DPICHANGED_AFTERPARENT` 的官方定义是「对 PMv2 **顶层窗口**，本消息发给正在经历 DPI 变化的那个窗口的**整个子 HWND 树**，发生在顶层窗口收到 `WM_DPICHANGED` 之后」，并且 `DefWindowProc` 对它**没有默认处理**。也就是说：任务栏（Explorer 的顶层窗口）原地经历 DPI 变化时，本组件很可能只收到 `WM_DPICHANGED_AFTERPARENT`。

全仓检索：`grep -rn "DPICHANGED" src` 命中 `src/main.rs:26`（导入 `WM_DPICHANGED`）、`src/main.rs:878`（唯一处理器）、`src/main.rs:885`（日志）、`src/smoke.rs:35` 与 `:240-268`（手工 `SendMessageW(WM_DPICHANGED)` 的用例）以及若干注释；**`AFTERPARENT` 全仓 0 命中**。而 windows 0.62.2 已导出该常量（`WM_DPICHANGED_AFTERPARENT = 739`，见该 crate 的 `Windows/Win32/UI/WindowsAndMessaging/mod.rs:6935`），取用成本为零。

后果链是确定的，不依赖时序运气：`Renderer::update_dpi`（`src/renderer.rs:493-550`）是全仓唯一重建位图/字体的入口，其生产调用点只有 3 处 —— `src/main.rs:574`（启动与 Explorer 重建尾段）、`src/main.rs:880`（`WM_DPICHANGED`）、`src/recovery.rs:272`（DPI 恢复事务）；而恢复事务的入口门是 `DPI_DIRTY`（`src/recovery.rs:197`），该位的置位点只有 `src/main.rs:582` 与 `src/main.rs:900` 两处、**都挂在 `update_dpi` 失败上**。于是通知一旦不到达：位图与字体停在旧 DPI，恢复事务永远不会被唤醒。

与此同时，定位路径每个 1s tick 都用 `GetDpiForWindow(hwnd)`（`src/window.rs:253`）的**当前** DPI 重算 `display_width/height` 并提交（`src/window.rs:471-508`；`SWP_NOSIZE` 只在 `dpi_dirty` 时追加，见 `src/window.rs:466-469`），而 `BitBlt` 用的是位图尺寸 `self.width/self.height`（`src/renderer.rs:471-481`）。两边一旦分叉，就是 AGENTS.md 不变量 6 明令禁止的「窗口物理宽高与位图不一致」，且**不会自愈**：`LAST_RECT` 提交后同一矩形被跳过（`src/window.rs:484-486`）。

现有兜底为何不覆盖：`reembed_if_lost` 只在 `EMBEDDED == false` 或父窗口不是当前任务栏时才动手（`src/window.rs:399-405`），这两个条件在「已嵌入 + 任务栏原地改 DPI」下都不成立。

## 提案

1. 把 `src/main.rs:878` 的匹配臂扩为 `WM_DPICHANGED | WM_DPICHANGED_AFTERPARENT`，共用同一处理器。处理器现在**不读 wparam/lparam**（只用 `GetDpiForWindow` + `update_dpi` + `embed_in_taskbar`），而 AFTERPARENT 的两个参数都是 0（官方定义），因此共用是安全的；注释里要显式写明「不得读这两个参数，它们无载荷」。
2. 在定位路径加一条**档位一致性判据**作为兜底：`update_taskbar_position` 已经拿到 `display_width`，把它与 `Renderer::bitmap_size().0`（`src/renderer.rs:554-556`）比对，不一致时置 `DPI_DIRTY` 并按 `position_flags(true)`（只改位置、不改尺寸）提交。这条让「通知没到」这类未知形态也能收敛，且不新增状态属主（`DPI_DIRTY` 的属主说明见 `src/state.rs:58-70`，语义仍是「窗口几何与位图可能不一致，尺寸提交权归 DPI 事务」）。
3. 把 `src/smoke.rs:267` 那条断言（`WM_DPICHANGED` 后窗口物理尺寸必须等于渲染器位图尺寸）扩到 AFTERPARENT 分支，两条消息共用同一后置条件。

## 明确不在本次范围

- 不改 DPI 恢复事务的四步序与清位条件（`src/recovery.rs:268-296`）：它是另一个出口，本次只是保证它**会被唤醒**。
- 不动 `rollback_window_to_bitmap`（`src/recovery.rs:56-61`）与 `WM_DPICHANGED` 失败分支的既有分工（`src/main.rs:895-901`）：后者处理「资源没换成功」，与本节处理的「通知没到达」是两回事。
- 不动 `src/window.rs:258-259` 的几何推导（位置用任务栏客户区坐标、尺寸用窗口 DPI）；几何推导本身另有既有裁定。
- 不把「DPI 值」引入成新的全局事实（现在没有第二处需要比较 DPI，先不加）。

## 为什么不保留？

1. 「Explorer 在缩放变化时会重建任务栏，子窗口被级联销毁 → `TaskbarCreated` → 完整重建，所以本项是空担忧。」—— 这是唯一可能让本项退化为纯加固的理由，但本仓没有任何证据支持它，且 `TaskbarCreated` 只覆盖任务栏**重建**，不覆盖任务栏**原地**改 DPI。因此提案把「真机验证」写进验收标准的第一步，而不是当既成事实。
2. 「子窗口也会收到 `WM_DPICHANGED`。」—— 官方把子树通知定义成 AFTERPARENT，本条若被真机日志证伪就是纯冗余（证伪依据见风险第 1 条），代价是常量加一个匹配臂。
3. 「干脆每 tick 重算 DPI 并重建位图，最省事。」—— 那会把 GDI 重分配（`CreateCompatibleBitmap`/`CreateFontIndirectW`）搬进 1s 热路径，与 `src/recovery.rs` 的事务化设计直接冲突，也违反「恢复动作幂等、失败退避」的既有取向。
4. 「加一条判据是新增状态。」—— 判据读的是既有的 `bitmap_size()` 与既有的 `DPI_DIRTY`，没有新事实、没有新属主。

## 验收标准

- 真机（必须先做）：组件已嵌入任务栏时把主屏缩放从 100% 改到 150%（或反向），开 `EnableDebugLog`（`src/util.rs:476-482`），断言 ①文字按新 DPI 重建（清晰、无错版）②组件物理尺寸等于位图尺寸 ③任务栏位置仍正确。
- 可证伪判据：若在嵌入态测得 `src/main.rs:885` 的 `WM_DPICHANGED` 路径被触发（日志留痕），说明通知本就到达 ⇒ 本项降级为「只做提案 2 的兜底判据」。
- 代码：`grep -n "WM_DPICHANGED_AFTERPARENT" src/main.rs` 命中 1 处；`grep -n "DPI_DIRTY" src/window.rs` 命中（新增判据）；`src/main.rs` 的 `WM_DPICHANGED` 臂注释写明「两参数无载荷、不得读」。
- `cargo test --locked -- --ignored --test-threads=1` 四条仍绿，其中 `smoke::dpi_dirty_cleared_by_recovery_transaction` 与 `smoke.rs:267` 的断言不回归；四条常规门禁全绿。

## 风险

- 最大风险是**真机证伪**：若子窗口确实也收 `WM_DPICHANGED`，本项提案 1 为冗余（不是错误），提案 2 仍应保留。证伪依据：在 `src/main.rs:878` 分支加一行 `log_event!` 后做一次缩放变化，看日志走哪个臂。
- AFTERPARENT 无载荷这一点若被未来改动忽略（有人想用 `lParam` 的建议矩形），会静默拿到 0 尺寸；提案要求在代码注释里把这条钉死。
- 定位路径新增一次 `bitmap_size()` 读取（`src/renderer.rs:554-556`，纯字段读取、无分配），位于 1s 定时器路径，无可测成本。
- 未覆盖：多显示器 + 任务栏不在主屏时的首帧档位（`calc_widget_rect` 在 `SetParent` 之前取 DPI，`src/window.rs:253` 早于 `:292`）不在本项范围内，它由「下一秒定位路径用新 DPI 收敛」兜住；若未来要修，应在 `calc_widget_rect` 之前完成 reparent 或改用任务栏窗口的 DPI。
