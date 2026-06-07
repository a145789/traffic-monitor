# Traffic Monitor

Windows 11 任务栏小组件，纯 Rust，无配置文件。嵌入任务栏系统托盘左侧，双行文字展示 CPU、内存、网速、鼠标电量/DPI。

## 构建

```bash
cargo build --release
cargo build --release 2>&1     # 检查警告
Start-Process "target\release\traffic-monitor.exe" -WindowStyle Hidden
Stop-Process -Name "traffic-monitor" -Force
```

- Rust edition 2024，依赖 `windows` crate v0.62 和 `hidapi` v2.6
- `build.rs` 通过 `winresource` 嵌入 DPI-aware manifest（PerMonitorV2）
- Release 配置：`opt-level="z"`, `lto=true`, `codegen-units=1`, `strip=true`

## 安装包

使用 Inno Setup 7 构建，脚本 `installer.iss`，输出到 `Output/`。

```bash
bun release.ts 0.2.0    # 一键发布：更新版本号 → 编译 → 打包 → git tag
```

- 安装路径：`Program Files\Traffic Monitor`
- 包含开始菜单/桌面快捷方式、开机自启选项、标准卸载程序
- 依赖：[Inno Setup 7](https://jrsoftware.org/isinfo.php)、[Bun](https://bun.sh)
- 版本号需同步更新 `Cargo.toml` 和 `installer.iss`，详见 `VERSIONING.md`

## 架构（6 个源文件，约 1600 行）

| 文件 | 行数 | 职责 |
|---|---|---|
| `main.rs` | 442 | 窗口创建、消息循环、任务栏嵌入、wnd_proc |
| `config.rs` | 47 | 常量定义、原子变量 |
| `collector.rs` | 142 | CPU/内存/网速采集 |
| `renderer.rs` | 418 | GDI 双缓冲渲染 |
| `mouse_hid.rs` | 288 | HID 鼠标轮询线程 |
| `tray.rs` | 278 | 系统托盘、菜单、开机自启 |

## config.rs 常量

- 窗口：`DISPLAY_WIDTH=240`, `DISPLAY_HEIGHT=32`, `GAP=-3`
- 定时器：`TIMER_ID_NETWORK=1`（1000ms）, `TIMER_ID_CPU_MEM=2`（5000ms）
- 鼠标 HID：VID `[0xA8A4, 0xA8A5]`（MLOONG）, PID `0x2255`（MX302）, usage page `0xFF01`, usage `0x0010`
- 轮询：在线 180s，离线 300s，连续 2 次失败判定离线
- 字体：Segoe UI, `FONT_BASE_SIZE=13`, `DPI_SCALE_FACTOR=1.173`
- 颜色：`COLOR_KEY=0x00FF00FF`（透明色）, 暗色文字 `0x00282828`, 亮色文字 `0x00FFFFFF`, 低电量 `0x004444FF`（BGR 的 #FF4444）

## 功能行为

### 显示布局

三列布局（左到右：CPU/MEM, 鼠标信息, 网速），各列内部 `DT_LEFT` 对齐，整体 widget 右对齐到托盘左侧。

- 鼠标列隐藏时（默认）：第一行 `CPU: 12%   ↑ 12.4 KB/s`，第二行 `MEM: 45%   ↓ 105.2 MB/s`
- 鼠标列显示时：第一行 `CPU: 12%   🖱️ 75%   ↑ 12.4 KB/s`，第二行 `MEM: 45%   DPI: 1600   ↓ 105.2 MB/s`
- 鼠标离线时显示 `🖱️ --` 和 `DPI: --` 占位符
- 鼠标电量 <20% 且未充电时，电量文字变为红色
- 列宽：CPU 列 64px，鼠标列 62px，网速列 76px，列间距 13px

### 托盘菜单

三个菜单项：开机自启（1001）、显示鼠标信息（1003）、退出（1002）。通过 `WM_COMMAND` 传递。右键菜单用 `InsertMenuItemW` 创建。托盘回调消息为 `WM_APP_TRAY`（`WM_USER + 100`）。

开机自启通过 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 写入带双引号的 exe 路径。

### 主题自适应

读取注册表 `SystemUsesLightTheme` 判断主题。`WM_SETTINGCHANGE` 时检查 lparam 指向的字符串是否为 `ImmersiveColorSet` 来响应主题变化。

### 智能挂起

以下情况会 kill 定时器、停止鼠标线程、trim 工作集：

- 全屏：前台窗口尺寸 == 主显示器分辨率（`GetForegroundWindow` + `GetWindowRect` vs `SM_CXSCREEN`/`SM_CYSCREEN`）
- 睡眠：`PBT_APMSUSPEND`
- 锁屏：`WM_WTSSESSION_CHANGE` 的 `WTS_SESSION_LOCK`（0x7）
- 恢复对应 `PBT_APMRESUMEAUTOMATIC` 和 `WTS_SESSION_UNLOCK`（0x8）

## 窗口与任务栏嵌入

窗口创建为 `WS_POPUP | WS_VISIBLE`，extended style 包含 `WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`。

`embed_in_taskbar` 调用顺序：

1. `SetParent(hwnd, h_taskbar)` — 会剥离 `WS_EX_LAYERED`
2. `SetWindowLongPtrW(GWL_STYLE, WS_CHILD | WS_VISIBLE)` — 直接覆盖样式
3. `SetWindowLongPtrW(GWL_EXSTYLE, ... | WS_EX_LAYERED)` — 恢复
4. `SetPos(SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED)`
5. `SetLayeredWindowAttributes(hwnd, COLOR_KEY, 0, LWA_COLORKEY)`

位置：`display_x = rc_tray.left - rc_taskbar.left - GAP - display_width`，垂直居中。

`GetMessageW` 的 hwnd 参数用 `None`。`RegisterWindowMessageW("TaskbarCreated")` 处理 explorer 重启。`WM_DPICHANGED` 时重新嵌入。

## 渲染

- 双缓冲：所有绘制操作在 `self.hdc_mem`，最后 `BitBlt` 到窗口 hdc
- 背景用 `CreateSolidBrush(COLOR_KEY)` 填充实现透明
- 字体用负 `lfHeight`（字符高度），`NONANTIALIASED_QUALITY`（值 3）防止 layered window 粉色伪影
- `update_dpi(hwnd)` 重建位图和字体，创建前校验句柄有效性
- `Drop` 实现恢复原始 GDI 对象并释放所有句柄

## 鼠标 HID 线程

- 按需启动，`start_mouse_thread()` 前重置 `SHOULD_STOP`，启动后 2 秒延迟进入轮询
- 任何启动新线程前先 `stop_and_join_mouse_thread()` 等待旧线程 Join
- 缓存 `HidApi` 实例避免重复创建
- 通过 `PostMessageW` 发送 `WM_USER + 1`（数据更新）或 `WM_USER + 2`（离线）到主线程
- `check_mouse_available()` 快速扫描设备是否存在
- `SUSPENDED` / `FULLSCREEN` 标志为 true 时跳过轮询

## 数据采集

- CPU：`GetSystemTimes`，首次调用初始化基线，后续计算差值
- 内存：`GlobalMemoryStatusEx` 取 `dwMemoryLoad`
- 网速：`GetIfTable2` + `FreeMibTable`，仅统计 Ethernet（type 6）和 Wi-Fi（type 71）且 `OperStatusUp` 的接口
- `trim_working_set`：`SetProcessWorkingSetSize(usize::MAX, usize::MAX)`

## 共享状态

所有跨线程状态为 `AtomicBool` / `AtomicU32`（`config.rs` 中定义），无锁。`Renderer` 为线程局部 `RefCell<Option<Renderer>>`，鼠标线程句柄同理。

## 调试

- `show_error("msg")` 弹 MessageBoxW
- `println!` 输出到控制台
- `Get-Process -Name "traffic-monitor"` 检查进程

## 要求

如无特殊要求，不要阅读 `docs/` 下的文档
