# RFC 0005: 代码质量、健壮性与用户体验

- **状态**: Draft (草案)
- **创建时间**: 2026-06-08
- **关联文件**: `main.rs`, `tray.rs`, `mouse_hid.rs`, `renderer.rs`, `collector.rs`, `Cargo.toml`

---

## 1. 背景与现状 (Context)

随着功能迭代，部分代码的可维护性、健壮性和用户体验仍有提升空间。

---

## 2. 优化项

### 2.1 `wnd_proc` 拆分（#6）

`main.rs` 的 `wnd_proc` 约 255 行，所有消息处理逻辑集中在一个函数中。

**方案**：按消息类型拆分成独立函数，`wnd_proc` 只做分发：

```rust
unsafe extern "system" fn wnd_proc(...) -> LRESULT {
    match msg {
        WM_TIMER => handle_timer(hwnd, wparam),
        WM_POWERBROADCAST => handle_power(hwnd, wparam, lparam),
        WM_WTSSESSION_CHANGE => handle_session_change(hwnd, wparam),
        WM_COMMAND => handle_command(hwnd, wparam),
        WM_APP_TRAY => { tray::show_context_menu(hwnd); LRESULT(0) }
        // ...
    }
}
```

### 2.2 `SHOW_MOUSE_INFO` 持久化（#8）

当前 `SHOW_MOUSE_INFO` 默认 `false`，每次重启都会重置。用户偏好应持久化到注册表（类似 autostart），重启后保持选择状态。

**方案**：在 `tray.rs` 中增加注册表读写，菜单切换时同步写入，启动时读取。

### 2.3 HID 协议常量命名（#9）

`mouse_hid.rs` 中 `BATTERY_CMD`、`DPI_CMD`、`DPI_SYNC_CMD` 使用裸字节数组，报文解析硬编码偏移量。

**方案**：为协议字段添加 `const` 命名常量：

```rust
const OFFSET_BATTERY_LEVEL: usize = 8;
const OFFSET_DPI_X: usize = 8;
const OFFSET_DPI_Y: usize = 10;
const ACTIVE_MODE_1: u8 = 0x01;
const ACTIVE_MODE_2: u8 = 0x02;
```

---

## 3. 优先级排序

| 优先级 | 项 | 收益 |
|--------|-----|------|
| 🟡 中 | #6 wnd_proc 拆分 | 可读性和可维护性大幅提升 |
| 🟡 中 | #8 SHOW_MOUSE_INFO 持久化 | 用户体验提升 |
| 🟡 中 | #9 HID 协议常量命名 | 协议代码可维护性提升 |
