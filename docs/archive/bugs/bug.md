# Traffic Monitor 全面审计报告

## 一、确认的 Bug 与内存泄漏

### 🔴 严重级 (High)

#### 1. `collector.rs` 网速溢出截断 — 数据错误

[collector.rs:L111-L112](file:///D:/work_space/life/traffic-monitor/src/collector.rs#L111-L112)

```rust
let speed_down = total_in.saturating_sub(PREV_NET_IN) as u32;  // u64 → u32 截断!
let speed_up = total_out.saturating_sub(PREV_NET_OUT) as u32;
```

`saturating_sub` 的结果是 `u64`，直接 `as u32` 会静默截断。当 1 秒内吞吐量超过 4GB（万兆网卡满载就能达到），差值会被截断为错误的小值。

> [!IMPORTANT]
> **修复**：在 `as u32` 前加 `.min(u32::MAX as u64)`，或者改用 `u64` 原子变量存储网速。考虑到实际场景中 `u32` 上限 ~4GB/s 对大多数用户足够，至少应该做饱和截断而非静默截断：
>
> ```rust
> let speed_down = total_in.saturating_sub(PREV_NET_IN).min(u32::MAX as u64) as u32;
> ```

---

#### 2. `renderer.rs` Drop 后使用已删除的 GDI 对象 — 逻辑错误

[renderer.rs:L336-L346](file:///D:/work_space/life/traffic-monitor/src/renderer.rs#L336-L346)

```rust
impl Drop for Renderer {
    fn drop(&mut self) {
        let _ = SelectObject(self.hdc_mem, self.old_bitmap);  // 恢复初始 bitmap
        let _ = SelectObject(self.hdc_mem, self.old_font);    // 恢复初始 font
        let _ = DeleteObject(self.hfont.into());   // ✓ 删除当前 font
        let _ = DeleteObject(self.hbitmap.into());  // ✓ 删除当前 bitmap
        let _ = DeleteObject(self.hbrush.into());   // ✓ 删除 brush
        let _ = DeleteDC(self.hdc_mem);             // ✓ 删除 DC
    }
}
```

这里的 Drop 实现本身**逻辑正确**（先恢复旧对象再删除新对象），但有一个隐患：`update_dpi` 中替换 bitmap/font 时：

```rust
// update_dpi (L313-L314):
let old_bitmap = SelectObject(self.hdc_mem, new_bitmap.into());
let _ = DeleteObject(old_bitmap.into());  // 删除了旧 bitmap
```

此时 `self.old_bitmap` 存的是 **最初始** 的 DC 默认 bitmap（创建时从 `SelectObject` 返回的），它在 `update_dpi` 里没有被更新。如果 `update_dpi` 被多次调用，Drop 时 `SelectObject(self.hdc_mem, self.old_bitmap)` 恢复的仍是最初始对象——**这是正确行为**，因为 DC 默认对象不需要我们管理。

> [!NOTE]
> 经仔细检查，Drop 的 GDI 资源管理**逻辑正确**。`old_bitmap`/`old_font` 保存的是 DC 自带的默认对象，始终有效，不会被 `update_dpi` 删除。✅ 无问题。

---

#### 3. `main.rs` WM_CLOSE 没有 join 鼠标线程 — 资源泄漏

[main.rs:L372-L377](file:///D:/work_space/life/traffic-monitor/src/main.rs#L372-L377)

```rust
WM_CLOSE => {
    remove_tray_icon();
    stop_mouse_thread();        // 只设置了 SHOULD_STOP 标志
    PostQuitMessage(0);         // 立即退出消息循环
    LRESULT(0)                  // 没有 join！线程可能还在运行
}
```

`stop_mouse_thread()` 只是设置了 `SHOULD_STOP = true`，但鼠标线程可能正在 `read_timeout(3000ms)` 阻塞中。`PostQuitMessage` 导致消息循环退出，`main()` 结束后进程退出，鼠标线程被 OS 强杀。

> [!WARNING]
> 虽然 OS 会回收资源，但 HID 设备的 file handle 可能没有被干净关闭，在极端情况下可能导致设备锁定（下次打开失败）。
>
> **修复**：改为 `stop_and_join_mouse_thread()`，但要注意 `join` 最多可能等待 3 秒（DPI 查询超时）。如果不想阻塞，可以在 `main()` 消息循环退出后再 join：
>
> ```rust
> // main.rs 消息循环结束后：
> stop_and_join_mouse_thread();
> let _ = WTSUnRegisterSessionNotification(hwnd);
> let _ = RENDERER.take();
> ```

---

#### 4. `mouse_hid.rs` `interruptible_sleep` 潜在 panic — duration 下溢

[mouse_hid.rs:L100-L109](file:///D:/work_space/life/traffic-monitor/src/mouse_hid.rs#L100-L109)

```rust
fn interruptible_sleep(dur: Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < dur {
        if SHOULD_STOP.load(Ordering::Relaxed) { return; }
        let remaining = dur - start.elapsed();  // ⚠️ 可能 panic!
        thread::sleep(remaining.min(Duration::from_millis(500)));
    }
}
```

在 `elapsed()` 检查和 `dur - start.elapsed()` 之间，时间在流逝。如果第二次 `elapsed()` 已经超过 `dur`，`Duration` 减法会 **panic**（Duration 不允许负值）。

> [!CAUTION]
> **修复**：使用 `saturating_sub` 或 `checked_sub`：
>
> ```rust
> let remaining = dur.saturating_sub(start.elapsed());
> if remaining.is_zero() { break; }
> thread::sleep(remaining.min(Duration::from_millis(500)));
> ```

---

### 🟡 中等级 (Medium)

#### 5. `collector.rs` static mut 非线程安全

[collector.rs:L16-L23](file:///D:/work_space/life/traffic-monitor/src/collector.rs#L16-L23)

```rust
static mut PREV_IDLE_TIME: u64 = 0;
static mut PREV_KERNEL_TIME: u64 = 0;
// ...
static mut NET_INITIALIZED: bool = false;
```

虽然当前所有 `collect_*` 函数只在主线程的定时器回调中调用，但 `static mut` 的使用本身是不安全的，且编译器无法做任何保证。Rust 2024 edition 将彻底禁止 `static mut`。

> [!TIP]
> **建议**：使用 `std::cell::Cell` 封装在不可变 static 中（单线程场景），或使用 `AtomicU64`。最简洁的方案是将状态封装到一个 `struct Collector` 中，持有在 main 中。

---

#### 6. `renderer.rs` `update_dpi` 未重新设置 `SetBkMode(TRANSPARENT)`

[renderer.rs:L299-L332](file:///D:/work_space/life/traffic-monitor/src/renderer.rs#L299-L332)

`new()` 中调用了 `SetBkMode(hdc_mem, TRANSPARENT)`，但 `update_dpi` 中没有重新设置。虽然 `SetBkMode` 作用于 DC 而非 bitmap，替换 bitmap 后 DC 的背景模式**应该保留**，但如果 DC 被重建则不保证。当前实现中 DC 没有被重建，所以**暂时安全**，但建议防御性地在 `update_dpi` 末尾补上。

---

#### 7. `mouse_hid.rs` 缓存的 `HidApi` 设备列表过期

[mouse_hid.rs:L112-L137](file:///D:/work_space/life/traffic-monitor/src/mouse_hid.rs#L112-L137)

`HidApi::new()` 在构造时枚举一次设备列表。缓存 `api_opt` 后，如果鼠标在运行中被插拔，`api.device_list()` 返回的仍是旧的设备列表。`hidapi` 2.x 提供了 `HidApi::refresh_devices()` 方法。

> [!TIP]
> **建议**：在 `find_mouse_device` 找不到设备时，调用 `api.refresh_devices()` 后重试一次，或在离线周期结束时 refresh。

---

#### 8. `config.rs` `DPI_SCALE_FACTOR` 硬编码可能不匹配其他鼠标型号

[config.rs:L23](file:///D:/work_space/life/traffic-monitor/src/config.rs#L23)

```rust
pub const DPI_SCALE_FACTOR: f64 = 1.173;
```

这是 MLOONG MX302 特有的 DPI 转换系数。如果将来更换鼠标，此常量需要同步修改。这不是 bug，但作为工程提醒记录在此。

---

### 🟢 低等级 (Low) / 代码质量

#### 9. `main.rs` 全局 `static mut` 过多

[main.rs:L40-L44](file:///D:/work_space/life/traffic-monitor/src/main.rs#L40-L44)

5 个 `static mut` 全局变量（`RENDERER`、`MOUSE_THREAD`、`TASKBAR_CREATED_MSG`、`H_TASKBAR`、`H_TRAY`）。虽然 Win32 GUI 程序的主线程单一消息循环保证了这些变量不会被并发访问，但 `#![allow(static_mut_refs)]` 是一个不好的信号。

> [!TIP]
> 更好的做法：用 `thread_local!` 或 `OnceLock`，或者把所有状态放进一个 `App` struct 通过 `GWLP_USERDATA` 关联到窗口。

---

#### 10. `tray.rs` TRAY_DATA 静态可变 — 潜在 UB

[tray.rs:L20](file:///D:/work_space/life/traffic-monitor/src/tray.rs#L20)

```rust
static mut TRAY_DATA: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
```

同第 9 点，属于代码风格问题。实际单线程无 data race，但形式上是 UB。

---

#### 11. `check_fullscreen` 缩进问题 — 可读性

[main.rs:L84-L129](file:///D:/work_space/life/traffic-monitor/src/main.rs#L84-L129)

```rust
fn check_fullscreen(hwnd: HWND) {
    unsafe {
        // ...
        if foreground.is_invalid() || ... {
        let was = FULLSCREEN.load(Ordering::Relaxed);  // ⚠️ 缩进不对！
        if was {
            // ...
        }
        return;
        }
```

`if` 块内的代码缩进和外面一样，容易误读。虽然功能正确，但**强烈建议修正缩进**。

---

## 二、文字排列优化方案

### 当前问题分析

当前布局用**硬编码像素偏移**计算每列位置，存在以下问题：

1. **网速列宽度固定 76px**，但 `"↓ 105.2 MB/s"` 和 `"↓ 0 B/s"` 宽度差距极大，导致视觉不对齐
2. **CPU/MEM 列宽度固定 64px**，但 `"CPU: 1%"` 和 `"CPU: 100%"` 宽度不同
3. **鼠标列宽度固定 62px**（声明注释写 52，实际代码用 62）
4. 列间距 `col_gap = 13px` 是均匀的，但由于各列文本长度不一致，视觉上间距不等

### 🎯 推荐方案：固定宽度格式化 + `GetTextExtentPoint32W` 动态测量

#### 核心思路

**不改变列的固定位置框架**（保持防抖），而是通过**格式化字符串使文本长度恒定**来消除跳动：

```rust
// === 第一列：CPU/MEM，统一占 7 字符 ===
// "CPU:  1%" → "CPU:  1%"  (padding space)
// "CPU: 12%" → "CPU: 12%"
// "CPU:100%" → "CPU:100%"
let cpu_text = format!("CPU:{:>3}%", cpu);   // 始终 8 字符
let mem_text = format!("MEM:{:>3}%", mem);   // 始终 8 字符

// === 第三列：网速，统一宽度 ===
// 关键：数字部分用固定宽度，单位用固定宽度
fn format_speed_fixed(bytes_per_sec: u32) -> String {
    if bytes_per_sec < 1024 {
        format!("{:>5} B/s", bytes_per_sec)       // "    0 B/s" ~ "1023 B/s"
    } else if bytes_per_sec < 1024 * 1024 {
        format!("{:>5.1}KB/s", bytes_per_sec as f64 / 1024.0)  // " 12.4KB/s"
    } else {
        format!("{:>5.1}MB/s", bytes_per_sec as f64 / 1048576.0) // "105.2MB/s"
    }
}
// "↑" + 空格 + format_speed_fixed = 固定前缀 + 固定宽度 = 稳定
```

#### 关键变化

| 改动         | 之前                              | 之后                                       |
| ------------ | --------------------------------- | ------------------------------------------ |
| CPU/MEM 文本 | `format!("CPU: {}%", cpu)` — 变长 | `format!("CPU:{:>3}%", cpu)` — 定长 8 字符 |
| 网速文本     | `format_speed` 变长               | `format_speed_fixed` — 数字部分定长 5 字符 |
| 网速箭头     | `"↑ "`                            | `"↑ "` 不变                                |
| 列定位       | 从右往左硬编码                    | 保持不变（固定位置框架是对的）             |

#### 列宽和间距的最佳值

通过 `GetTextExtentPoint32W` 在 `Renderer::new()` / `update_dpi` 时**测量一次**各列最大文本的像素宽度，然后动态计算：

```rust
// 在 update_dpi 或 new 中，测量各列最大宽度：
fn measure_text_width(hdc: HDC, text: &str) -> i32 {
    let wide = to_wide(text);
    let mut size = SIZE::default();
    unsafe { GetTextExtentPoint32W(hdc, &wide[..wide.len()-1], &mut size); }
    size.cx
}

// 最大宽度文本：
let col1_width = measure_text_width(hdc, "CPU:100%");        // 第一列
let col2_width = measure_text_width(hdc, "🖱️ 100%");         // 第二列(鼠标)
let col3_width = measure_text_width(hdc, "↑ 999.9MB/s");     // 第三列(网速)
let col_gap = measure_text_width(hdc, "  ");                  // 两个空格的间距
```

然后用测量值反算各 RECT：

```rust
// 从右往左排列：
let speed_right = self.width - right_margin;
let speed_left = speed_right - col3_width;

let mouse_right = speed_left - col_gap;
let mouse_left = mouse_right - col2_width;  // 仅 show_mouse 时

let cpu_right = (if show_mouse { mouse_left } else { speed_left }) - col_gap;
let cpu_left = cpu_right - col1_width;
```

#### 效果预览

```
无鼠标模式（两列紧凑排列）：
┌──────────────────────────────────────┐
│    CPU:  5%   ↑   0.5KB/s           │
│    MEM: 45%   ↓ 105.2MB/s           │
└──────────────────────────────────────┘

有鼠标模式（三列均匀排列）：
┌──────────────────────────────────────────────┐
│    CPU:  5%   🖱️  75%   ↑   0.5KB/s          │
│    MEM: 45%   DPI:1600   ↓ 105.2MB/s         │
└──────────────────────────────────────────────┘
```

- **CPU/MEM 列**：始终右对齐数字，视觉稳定
- **网速列**：左对齐但数字固定宽度，箭头始终在同一位置
- **各列间距**：通过测量两个空格宽度实现**自然等距**
- **右侧冗余**：网速文本 `DT_LEFT` 对齐，右侧自然留白（最长 `999.9MB/s` 刚好填满）

### 实施建议

> [!IMPORTANT]
> **最小改动路线**（推荐）：只改两个地方即可获得 90% 的效果提升：
>
> 1. **`renderer.rs` 的 `format_speed`** → 改为固定宽度格式化
> 2. **`renderer.rs` 的 `render` 中的 CPU/MEM 格式化** → `format!("CPU:{:>3}%", cpu)`
>
> 不需要改列定位逻辑，现有的固定像素框架配合固定宽度文本就足够稳定。

> [!TIP]
> **完美路线**（可选）：额外在 `update_dpi` 中用 `GetTextExtentPoint32W` 测量列宽，将硬编码的 `64.0`、`62.0`、`76.0` 替换为测量值。这样在任意字体/DPI 下都完美对齐，但改动量较大。

---

## 三、其他建议

| 类别           | 建议                                                                                                                                                                                                                                                                                                                                                                                              |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **内存**       | 当前 GDI 资源管理 ✅ 无泄漏。Drop 实现正确，`update_dpi` 也有释放旧对象                                                                                                                                                                                                                                                                                                                           |
| **字体渲染**   | `NONANTIALIASED_QUALITY (3)` 在高 DPI 下可能会让文字边缘毛糙，可考虑 `CLEARTYPE_QUALITY (5)` + 调整 COLOR_KEY 策略                                                                                                                                                                                                                                                                                |
| **原子序**     | 大量 `Ordering::Relaxed`。对于跨线程的 flag（如 `SHOULD_STOP`），发布端用 `Release`、消费端用 `Acquire` 更严谨。当前使用 `Release` 发布 `SHOULD_STOP` 是正确的 ✓，但读取端 [L103](file:///D:/work_space/life/traffic-monitor/src/mouse_hid.rs#L103)、[L114](file:///D:/work_space/life/traffic-monitor/src/mouse_hid.rs#L114) 使用 `Relaxed` 读取，理论上在弱序架构上可能延迟可见（x86 上无影响） |
| **emoji 渲染** | `🖱️` (U+1F5B1) 是双码元字符(surrogate pair)，GDI 的 `DrawTextW` 对 emoji 支持有限，在某些 Win11 版本上可能渲染为方框。如果出现问题可改用纯文本 "M:" 替代                                                                                                                                                                                                                                          |
