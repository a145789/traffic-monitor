# 代码审计报告

> 审计日期: 2025-06-07  
> 对照文档: `IMPLEMENTATION.md`, `reference/text-computer.md`, `reference/layered-window-pitfalls.md`

---

## 一、已确认的 Bug

### 1. 菜单句柄泄漏 — `tray.rs:107`

`show_context_menu()` 中 `CreatePopupMenu()` 创建的 `HMENU` 从未被 `DestroyMenu()` 释放。

```rust
// tray.rs:107 — 当前代码
let hmenu = CreatePopupMenu().unwrap();
// ... 添加菜单项 ...
TrackPopupMenu(hmenu, ...);
// 缺失: DestroyMenu(hmenu)
```

**影响**: 每次右键弹出菜单泄漏一个 GDI 句柄。长期运行后可能触及进程 10,000 GDI 对象上限。

**修复**: 在 `TrackPopupMenu` 返回后调用 `DestroyMenu(hmenu)`。

---

### 2. 托盘菜单退出时鼠标线程未被停止 — `tray.rs:149-158`

`MENU_ID_EXIT` 处理逻辑：

```rust
// tray.rs:149-158
MENU_ID_EXIT => {
    remove_tray_icon();
    unsafe {
        DestroyWindow(hwnd).ok();   // 发送 WM_DESTROY/WM_NCDESTROY
        PostQuitMessage(0);
    }
}
```

`DestroyWindow` 发送的是 `WM_DESTROY` 和 `WM_NCDESTROY`，**不是** `WM_CLOSE`。而 `stop_mouse_thread()` 仅存在于 `WM_CLOSE` 分支：

```rust
// main.rs:331-336 — 不会被执行
WM_CLOSE => {
    remove_tray_icon();
    stop_mouse_thread();   // <-- 托盘退出时走不到这里
    PostQuitMessage(0);
    LRESULT(0)
}
```

**影响**: `SHOULD_STOP` 保持 `false`，鼠标线程在窗口已销毁后继续运行，向已失效的 `HWND` 发送 `PostMessageW`。

**修复**: 在 `MENU_ID_EXIT` 分支中增加 `stop_mouse_thread()` + `join()`，或将退出逻辑统一。

---

## 二、代码异味

### 3. `static mut MAIN_HWND` 跨线程无同步 — `mouse_hid.rs:19`

```rust
static mut MAIN_HWND: HWND = HWND(std::ptr::null_mut());
```

- **写**: `init()` — 主线程
- **读**: `mouse_worker_loop()` → `PostMessageW(MAIN_HWND, ...)` — 鼠标线程

Rust 安全模型中 `static mut` 的多线程读写属于 UB。实际运行时安全（写先于读），但编译器不会保证这一点。建议改为 `AtomicPtr` 或确保 `MAIN_HWND` 在 `start_mouse_thread()` 之前完成写入。

### 4. Renderer Drop 中 GDI 删除顺序不当 — `renderer.rs:211-218`

```rust
impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.hfont);    // 仍选入 hdc_mem
            DeleteObject(self.hbitmap);  // 仍选入 hdc_mem
            DeleteDC(self.hdc_mem);      // 内含以上两个对象的引用
        }
    }
}
```

GDI 规范要求先选回原对象再删除。当前无实际影响（`Renderer` 是 `static`，仅在进程退出时 drop，OS 会回收所有 GDI 句柄），但写法不符合规范。

### 5. `update_text_color` 死参数 — `renderer.rs:54`

```rust
pub fn update_text_color(&mut self, _tray_hwnd: HWND, _taskbar_hwnd: HWND) {
```

`_tray_hwnd` 和 `_taskbar_hwnd` 从未被使用。调用处传入了有效值，函数内部却走了注册表方案（`is_system_light_theme()`）。

### 6. 每帧创建/销毁画刷 — `renderer.rs:73-75`

```rust
let hbrush = CreateSolidBrush(COLORREF(COLOR_KEY));
let _ = FillRect(self.hdc_mem, &rect, hbrush);
let _ = DeleteObject(hbrush);
```

每个 `WM_PAINT` 都创建新画刷。可将画刷缓存到 `Renderer` 结构体中，在 `Drop` 时一并清理。

---

## 三、与 IMPLEMENTATION.md 的偏离（已知&已评估）

| Spec 要求 | 实际实现 | 评估 |
|---|---|---|
| 像素采样任务栏背景色（5.2 节） | 注册表 `SystemUsesLightTheme` | **正确。** `text-computer.md` 验证了注册表方案更稳定可靠 |
| `GetIfTable2`（4.2 节） | `GetIfTable`（32-bit 计数器） | 可接受。下溢保护兜底，正常带宽下不会出问题 |
| 锁屏挂起（6.1 节） | 仅实现 `PBT_APMSUSPEND` | **缺失。** Win11 Modern Standby 下锁屏不触发 `PBT_APMSUSPEND`，需 `WTS_SESSION_LOCK` |
| 唤醒后等 1-2 秒再重连 HID（9.3 节） | `start_mouse_thread()` 内 sleep 2s | 已满足（线程启动时 2s sleep） |

---

## 四、`layered-window-pitfalls.md` 合规检查

| 要点 | 检查结果 |
|---|---|
| `SetParent` 后恢复 `WS_EX_LAYERED` | ✅ `main.rs:204-205` |
| `GWL_STYLE` 覆盖而非 OR | ✅ `main.rs:202` |
| `SWP_FRAMECHANGED` 携带 | ✅ `main.rs:215` |
| `SetLayeredWindowAttributes` 最后调用 | ✅ `main.rs:219` |
| `lfHeight` 负值 | ✅ `renderer.rs:224` |
| `DrawTextW` + `DT_VCENTER` | ✅ `renderer.rs:123,137` |
| 窗口创建用 `WS_POPUP` | ✅ `tray.rs:55` |
| DPI Awareness manifest | ✅ `build.rs:11-12` |

---

## 五、优先级建议

| 优先级 | 编号 | 项 |
|--------|------|----|
| P0 | #1 | 菜单句柄泄漏 |
| P0 | #2 | 退出时线程未停 |
| P1 | #3 | `static mut MAIN_HWND` 改为 `AtomicPtr` |
| P2 | #5 | 删除 `update_text_color` 死参数 |
| P2 | #6 | 画刷缓存 |
| P3 | #4 | Renderer Drop 顺序 |
| P3 | 锁屏挂起 | 添加 `WTS_SESSION_LOCK` 监听 |
