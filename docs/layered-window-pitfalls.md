# Windows Layered Window 踩坑经验

本文档记录在将 GDI 窗口嵌入 Windows 11 任务栏过程中遇到的关键问题及解决方案。

## SetParent 会剥离 WS_EX_LAYERED

`SetParent(hwnd, taskbar)` 之后窗口的 `WS_EX_LAYERED` 扩展样式会被 Windows 自动移除。必须在 `SetParent` 后手动恢复：

```rust
SetParent(hwnd, h_taskbar);
// 1. 覆盖 GWL_STYLE（不要 OR，直接赋值）
SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_CHILD.0 | WS_VISIBLE.0) as isize);
// 2. 恢复 WS_EX_LAYERED
let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | (WS_EX_LAYERED.0 as isize));
// 3. SetWindowPos 必须带 SWP_FRAMECHANGED，否则样式不生效
SetWindowPos(hwnd, ..., SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED);
// 4. 最后才设透明（必须在样式生效之后）
SetLayeredWindowAttributes(hwnd, COLORREF(COLOR_KEY), 0, LWA_COLORKEY);
```

## SWP_FRAMECHANGED 不是可选的

`SetWindowLongPtrW` 修改 `GWL_STYLE` / `GWL_EXSTYLE` 后，必须调用 `SetWindowPos` 并带上 `SWP_FRAMECHANGED` 标志。不带这个标志，样式变更只写入内部状态，Windows 不会重新应用窗口框架，`WS_EX_LAYERED` 不会生效。

## GWL_STYLE 用覆盖而不是 OR

`SetParent` 后窗口可能残留各种顶级窗口样式（`WS_OVERLAPPED`、`WS_CAPTION`、`WS_BORDER` 等）。用 `current_style | WS_CHILD` 做位运算叠加会保留这些冲突样式。正确做法是直接覆盖为 `WS_CHILD | WS_VISIBLE`：

```rust
// 错误：叠加，残留样式可能导致渲染异常
SetWindowLongPtrW(hwnd, GWL_STYLE, current_style | (WS_CHILD.0 as isize));
// 正确：覆盖，只保留需要的
SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_CHILD.0 | WS_VISIBLE.0) as isize);
```

## 窗口创建用 WS_POPUP

`CreateWindowExW` 创建窗口时用 `WS_POPUP | WS_VISIBLE` 而不是单独的 `WS_VISIBLE`。`WS_POPUP` 明确告诉 Windows 这是一个独立弹出窗口，和 `WS_EX_LAYERED` 配合更可靠。后续 `embed_in_taskbar` 会统一改为 `WS_CHILD`。

## SetLayeredWindowAttributes 的调用时机

这个函数**不能**在 `embed_in_taskbar` 之前调用。必须在 `SetParent` -> 恢复 `WS_EX_LAYERED` -> `SetWindowPos(SWP_FRAMECHANGED)` 全部完成之后才能调用，否则透明不生效。`SetLayeredWindowAttributes` 应放在 `embed_in_taskbar` 函数内部末尾。

## LOGFONTW.lfHeight 用负值

`CreateFontIndirectW` 的 `lfHeight` 字段：正值 = 单元格高度（含 internal leading），负值 = 字符高度。用正值会导致实际字号偏大。正确做法：

```rust
lfHeight: -size  // 负值，按字符高度
```

## DrawTextW 优于 TextOutW

`TextOutW` 不支持垂直对齐，y 坐标是文字顶部位置。`DrawTextW` 配合 `DT_VCENTER | DT_SINGLELINE` 可以在指定 RECT 内垂直居中，适合任务栏这种固定高度的场景：

```rust
DrawTextW(hdc, &mut text, &mut rect, DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT);
```
