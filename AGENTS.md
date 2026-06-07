# Traffic Monitor - Agent 指南

## 项目概述

Windows 11 任务栏小组件，显示 CPU、内存、网速、鼠标电量/DPI。纯 Rust 实现，无配置文件。

## 功能要求

低内存、高性能、无闪烁。

## 构建与运行

```bash
cargo build --release          # 编译优化版（约200KB）
cargo build --release 2>&1     # 检查警告
Start-Process "target\release\traffic-monitor.exe" -WindowStyle Hidden  # 后台运行
Stop-Process -Name "traffic-monitor" -Force  # 停止进程
```

## 架构（6个文件）

- `main.rs` - 窗口创建、消息循环、任务栏嵌入、wnd_proc
- `config.rs` - 所有常量（尺寸、定时器、HID ID、颜色、原子变量）
- `collector.rs` - CPU/内存用 `GetSystemTimes`/`GlobalMemoryStatusEx`，网速用 `GetIfTable`
- `renderer.rs` - GDI 双缓冲渲染，`hdc_mem` → `BitBlt` 到窗口 hdc
- `mouse_hid.rs` - HID 工作线程轮询 MLOONG MX302（VID:04D9 PID:A02A）
- `tray.rs` - 系统托盘图标、右键菜单、`WM_COMMAND` 处理

## 关键实现细节

### 窗口生命周期

- 窗口创建时用 `WS_POPUP | WS_VISIBLE`（独立弹出窗口），然后 `SetParent` 到任务栏
- `embed_in_taskbar` 中 `SetParent` 后**直接覆盖** `GWL_STYLE` 为 `WS_CHILD | WS_VISIBLE`，不做位运算叠加，避免残留顶级窗口样式冲突
- `GetMessageW` 必须用 `None` 作为 hwnd（不能传具体窗口），否则收不到 `WM_QUIT`
- 菜单点击通过 `WM_COMMAND` 传递，ID 为 1001/1002，不是 `WM_USER+100`

### 渲染管线

- 所有绘制操作作用于 `self.hdc_mem`，最后 `BitBlt` 到真实 hdc（双缓冲）
- 背景用 `CreateSolidBrush(COLOR_KEY)` 填充实现透明
- 字体在创建时就选入 `hdc_mem`，不是在窗口 hdc 上
- `LOGFONTW.lfHeight` 必须用**负值**（如 `-14`），按字符高度创建字体；正值按单元格高度，会导致字号偏大
- 用 `DrawTextW` + `DT_VCENTER | DT_SINGLELINE` 绘制文字，比 `TextOutW` 能精确控制垂直居中
- Renderer 初始化后立即调用 `recreate_font(hwnd)` 根据窗口 DPI 校正字号
- 低电量文字放在 x=110 位置，避免与网速文字重叠

### 线程安全

- `SHOULD_STOP` 在 `start_mouse_thread()` 启动前必须重置为 `false`
- 鼠标 HID 轮询：在线 3分钟，离线 5分钟，连续2次失败判定离线
- `SUSPENDED` 标志在全屏/锁屏时暂停所有采集

### 任务栏集成

- 查找 `Shell_TrayWnd` → `TrayNotifyWnd`，定位在托盘左侧
- `RegisterWindowMessageW("TaskbarCreated")` 用于 explorer.exe 重启恢复
- `embed_in_taskbar` 内的调用顺序**严格固定**：
  1. `SetParent(hwnd, h_taskbar)` — 会剥离 `WS_EX_LAYERED`
  2. `SetWindowLongPtrW(GWL_STYLE, WS_CHILD | WS_VISIBLE)` — 覆盖，不叠加
  3. `SetWindowLongPtrW(GWL_EXSTYLE, ... | WS_EX_LAYERED)` — 恢复被剥离的样式
  4. `SetWindowPos(..., SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED)` — `SWP_FRAMECHANGED` 让样式变更生效
  5. `SetLayeredWindowAttributes(hwnd, COLOR_KEY, 0, LWA_COLORKEY)` — 必须在样式生效之后调用
- 位置计算用任务栏客户区坐标：`display_x = rc_tray.left - rc_taskbar.left - GAP - DISPLAY_WIDTH`

## Windows API 注意事项

- `HWND` 是 `*mut c_void`，用 `HWND(std::ptr::null_mut())` 不是 `HWND(0)`
- 大多数 API 不接受 `Option<HWND>`，直接传 `HWND`
- 注册表操作需要 UTF-16 转换：`str.encode_utf16().chain(once(0))`
- `PostMessageW` 参数类型：`WPARAM(usize)`、`LPARAM(isize)`，不是原始整数

## 调试方法

- 添加 `show_error("msg")` 调用弹出 MessageBoxW 诊断信息
- 用 `println!` 输出到控制台（从终端运行时）
- 用 `Get-Process -Name "traffic-monitor"` 检查进程是否运行

## 发布优化配置

```toml
[profile.release]
opt-level = "z"    # 体积优化
lto = true         # 链接时优化
codegen-units = 1  # 更好的优化
strip = true       # 去除符号
```
