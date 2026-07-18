# RFC 0004: 性能优化与资源管理

- **状态**: Draft (草案)
- **创建时间**: 2026-06-08
- **关联文件**: `mouse_hid.rs`, `renderer.rs`, `collector.rs`, `main.rs`

---

## 1. 背景与现状 (Context)

本项目作为任务栏常驻小组件，对 CPU、内存和 I/O 的占用要求极为严格。当前实现中存在多处可优化的性能瓶颈和资源浪费。

---

## 2. 优化项

### 2.1 热路径堆分配消除（#2, #3, #17）

`renderer.rs` 的 `format_speed` 每次渲染都 `format!()` 分配 String，`to_wide` 每次创建 `Vec<u16>`。`render()` 每秒调用至少一次，涉及约 10+ 次 `format!` + `to_wide` 调用。

`collector.rs` 的 `is_virtual_friendly_name` 每次 `to_lowercase()` 分配新 String，且 `contains` 被调用 14 次。

**方案**：
- `format_speed`：用 `write!` 写入栈上 `[u8; 32]` 缓冲区避免堆分配
- `to_wide`：在 `Renderer` 内部维护可复用的 `Vec<u16>` 缓冲区，并加 `#[inline]`
- `is_virtual_friendly_name`：改用 `to_ascii_lowercase()` 避免 Unicode 处理开销

### 2.2 多显示器全屏检测（#19）

`check_fullscreen` 使用 `SM_CXSCREEN`/`SM_CYSCREEN` 仅取主显示器分辨率。多显示器场景下，副屏全屏不会触发挂起。

**方案**：改用 `MonitorFromWindow` + `GetMonitorInfo` 获取前台窗口所在显示器分辨率，仅在该窗口覆盖 taskbar 所在显示器时才挂起。

### 2.3 `trim_working_set` 过度调用（#14）

`trim_working_set()` 在 `stop_and_join_mouse_thread()` 中每次都调用，而后者被 6 处位置调用。频繁 trim 会导致页面在 trim 后立即被重新加载（抖动），反而增加 I/O 开销。

**方案**：仅在挂起/锁屏时 trim，正常运行中不要 trim。

### 2.4 鼠标线程启停统一（#7）

`stop_and_join → start` 模式在 `main.rs` 中至少出现 6 次。

**方案**：提取 `restart_mouse_thread()` 统一调用。

```rust
fn restart_mouse_thread() {
    mouse_hid::stop_and_join_mouse_thread();
    if SHOW_MOUSE_INFO.load(Ordering::Relaxed) {
        mouse_hid::start_mouse_thread();
    }
}
```

---

## 3. 优先级排序

| 优先级 | 项 | 收益 |
|--------|-----|------|
| 🔴 高 | #7 restart_mouse_thread | 消除 ~30 行重复代码，降低维护成本 |
| 🔴 高 | #14 trim_working_set | 减少 I/O 抖动，提升实际内存效率 |
| 🟡 中 | #2/3/17 热路径堆分配 | 每秒减少 10+ 次堆分配 |
| 🟡 中 | #19 多显示器全屏检测 | 多屏用户体验正确性 |
