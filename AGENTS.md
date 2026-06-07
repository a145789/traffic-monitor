# Traffic Monitor - Agent 指南

## 项目概述

Windows 11 任务栏小组件，纯 Rust 实现，无配置文件。嵌入在任务栏系统托盘左侧，以双行文字展示 CPU、内存、网速、鼠标电量/DPI。

## 功能要求

### 展示内容与格式

- 双行文字，右对齐嵌入在 `Shell_TrayWnd` 内、`TrayNotifyWnd` 左侧
- 第一行（鼠标信息隐藏时）：`CPU: 12%   ↑ 12.4 KB/s`
- 第一行（鼠标信息显示时）：`CPU: 12%   🖱️ 75%   ↑ 12.4 KB/s`
- 第二行（鼠标信息隐藏时）：`MEM: 45%   ↓ 105.2 MB/s`
- 第二行（鼠标信息显示时）：`MEM: 45%   DPI: 1600   ↓ 105.2 MB/s`
- 鼠标信息默认隐藏，通过托盘菜单「显示鼠标信息」切换
- 开启时先快速扫描 HID 设备，未检测到则弹框提示「未检测到物理鼠标或鼠标不支持」，并且不予开启（不打勾，不启动鼠标线程）
- 鼠标信息隐藏时，鼠标 HID 轮询线程也会停止，节省资源
- 鼠标在线勾选状态下，若中途断电休眠或者检测不到，无需展示空白，而是展示「🖱️ --」和「DPI: --」占位符以确保防抖，行为逻辑与之前的五分钟检测一致（托盘保持勾选）
- 鼠标电量 <20% 且未充电时，电量部分文字变色为红色（#FF4444）提示

### 暗色/亮色主题自适应

- 读取注册表 `SystemUsesLightTheme` 判断系统主题，自动切换文字颜色（深灰/白色）
- 监听 `WM_SETTINGCHANGE` + `ImmersiveColorSet` 实时响应主题变化

### 系统托盘

- `Shell_NotifyIconW` 创建托盘图标，右键菜单：**开机自启**（勾选/取消）/ **显示鼠标信息**（勾选/取消）/ **退出**
- 开机自启通过 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 注册表实现

### 智能挂起（省资源）

以下情况停止所有定时器采集、停止鼠标轮询、`SetProcessWorkingSetSize` trim 内存：

- **全屏检测**：前台窗口尺寸 == 屏幕分辨率（全屏游戏/视频）
- **系统睡眠**：收到 `PBT_APMSUSPEND`
- **锁屏**：收到 `WM_WTSSESSION_CHANGE` + `WTS_SESSION_LOCK`
- 唤醒/退出全屏/解锁后自动恢复

### 性能指标

- 二进制体积 < 250KB（release, opt-level="z", lto, strip）
- 稳定运行时物理内存 < 2.5MB
- 挂起 trim 后物理内存 < 150KB
- 所有参数硬编码在 `config.rs`，零磁盘 I/O
- 仅主显示器、仅 Win11

## 构建与运行

```bash
cargo build --release          # 编译优化版（约200KB）
cargo build --release 2>&1     # 检查警告
Start-Process "target\release\traffic-monitor.exe" -WindowStyle Hidden  # 后台运行
Stop-Process -Name "traffic-monitor" -Force  # 停止进程
```

## 架构（6个文件）

- `main.rs` - 窗口创建、消息循环、任务栏嵌入、wnd_proc、全局 Renderer 退出清理
- `config.rs` - 所有常量（尺寸、定时器、HID ID、颜色、原子变量、低电量位置常量）
- `collector.rs` - CPU/内存用 `GetSystemTimes`/`GlobalMemoryStatusEx`，网速用对齐安全且无溢出的 `GetIfTable2` 与 `FreeMibTable` 释放
- `renderer.rs` - GDI 双缓冲渲染，容错检验新句柄，`hdc_mem` → `BitBlt` 到窗口 hdc
- `mouse_hid.rs` - HID 工作线程轮询 MLOONG MX302（缓存 HidApi 避免重建，支持优雅停止与 Join，`check_mouse_available` 快速扫描）
- `tray.rs` - 系统托盘图标（带 uVersion 4 版本控制）、右键菜单、加双引号的开机自启、`WM_COMMAND` 处理

## 关键实现细节

### 窗口生命周期

- 窗口创建时用 `WS_POPUP | WS_VISIBLE`（独立弹出窗口），然后 `SetParent` 到任务栏
- `embed_in_taskbar` 中 `SetParent` 后**直接覆盖** `GWL_STYLE` 为 `WS_CHILD | WS_VISIBLE`，不做位运算叠加，避免残留顶级窗口样式冲突
- `GetMessageW` 必须用 `None` 作为 hwnd（不能传具体窗口），否则收不到 `WM_QUIT`
- 菜单点击通过 `WM_COMMAND` 传递，ID 为 1001/1002/1003，不是 `WM_USER+100`

### 渲染管线

- 所有绘制操作作用于 `self.hdc_mem`，最后 `BitBlt` 到真实 hdc（双缓冲）
- 背景用 `CreateSolidBrush(COLOR_KEY)` 填充实现透明
- 字体在创建时就选入 `hdc_mem`，不是在窗口 hdc 上
- `LOGFONTW.lfHeight` 必须用**负值**（如 `-14`），按字符高度创建字体；正值按单元格高度，会导致字号偏大
- 用 `DrawTextW` + `DT_VCENTER | DT_SINGLELINE` 绘制文字，比 `TextOutW` 能精确控制垂直居中
- Renderer 初始化及 DPI 变化时调用 `update_dpi(hwnd)` 重新配置位图与字号，创建新句柄前进行有效性校验（如 `!new_bitmap.is_invalid()`）以防止异常失效
- 低电量文字定位使用中列鼠标区域内的右起相对 RECT 偏移，在各种 DPI 下均能与图标完美对齐且不重叠

### 线程安全与生命周期同步

- `SHOULD_STOP` 在 `start_mouse_thread()` 启动前必须重置为 `false`
- 为了避免在全屏切换、系统休眠/唤醒、锁屏/解锁交错收到消息时造成的线程泄漏与 HID 设备冲突，在任何地方启动新鼠标线程前，都必须先调用 `stop_and_join_mouse_thread()` 阻塞等待旧线程完全 Join 销毁后再启动新线程
- 鼠标 HID 轮询：在线 3分钟，离线 5分钟，连续2次失败判定离线，循环外部缓存 `HidApi` 对象
- `SUSPENDED` 标志在全屏/锁屏时暂停所有采集
- `SHOW_MOUSE_INFO` 控制鼠标信息显示与线程：默认 `false`，切换时启动/停止鼠标 HID 线程，全屏/休眠/锁屏恢复时仅在 `SHOW_MOUSE_INFO` 为 `true` 时重启线程

### 任务栏集成

- 查找 `Shell_TrayWnd` → `TrayNotifyWnd`，定位在托盘左侧
- `RegisterWindowMessageW("TaskbarCreated")` 用于 explorer.exe 重启恢复
- `embed_in_taskbar` 内的调用顺序**严格固定**：
  1. `SetParent(hwnd, h_taskbar)` — 会剥离 `WS_EX_LAYERED`
  2. `SetWindowLongPtrW(GWL_STYLE, WS_CHILD | WS_VISIBLE)` — 覆盖，不叠加
  3. `SetWindowLongPtrW(GWL_EXSTYLE, ... | WS_EX_LAYERED)` — 恢复被剥离的样式
  4. `SetWindowPos(..., SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED)` — `SWP_FRAMECHANGED` 让样式变更生效
  5. `SetLayeredWindowAttributes(hwnd, COLOR_KEY, 0, LWA_COLORKEY)` — 必须在样式生效之后调用
- 位置计算用任务栏客户区坐标：`display_x = rc_tray.left - rc_taskbar.left - gap - display_width`

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

## 要求

- 如无特殊要求，不要阅读 `docs/` 下的任何文档
