# Win11 任务栏状态监视器 — 实现文档

> **目标**: 用 Rust 实现一个轻量级桌面小组件，在 Windows 11 任务栏右侧（系统托盘左侧）以文字形式展示：CPU 占用、内存占用、上行/下行网速、鼠标电量、鼠标 DPI。  
> **核心约束**: 仅兼容 Windows 11、仅主显示器、纯硬编码配置（方案 A，无配置文件与设置界面，通过托盘右键菜单操作）、超低内存占用（目标 < 3MB）。  
> **交互方式**: 系统托盘图标 → 右键菜单：切换开机自启 / 退出程序。

---

## 1. 总体架构

小组件采用**双线程**架构，确保 HID 通信超时不会阻塞主 UI 线程的消息响应。

```
┌─────────────────────────────────────────────────────────────┐
│                       主线程 (UI 与渲染)                     │
│                                                             │
│  ┌──────────┐  ┌────────────────┐  ┌─────────────────────┐  │
│  │ 窗口管理 │  │ 文字渲染 (GDI) │  │  智能挂起与内存释放 │  │
│  │Embedder  │  │   Renderer     │  │   Suspend & Trim    │  │
│  └────┬─────┘  └───────┬────────┘  └──────────┬──────────┘  │
│       │                │                      │             │
│  ┌────┴────────────────┴──────────────────────┴────────────┐  │
│  │  定时器 (1s) → 采集网速/CPU/内存 → 触发 InvalidateRect   │  │
│  └────────────────────────────────────────────────────────┘  │
│                                                             │
│  ┌────────────────────────────────────────────────────────┐  │
│  │ 显示窗口 → SetParent → Shell_TrayWnd                    │  │
│  │ 位置：TrayNotifyWnd (系统托盘) 左侧（右对齐固定）       │  │
│  └────────────────────────────────────────────────────────┘  │
└───────────────────────────────▲─────────────────────────────┘
                                │ PostMessage (异步数据通知)
┌───────────────────────────────┴─────────────────────────────┐
│                    鼠标采集线程 (Worker Thread)             │
│                                                             │
│  ┌─────────────────┐        ┌────────────────────────────┐  │
│  │   HID 设备读写  │ ─────> │ 动态轮询：在线状态 (3分钟)  │  │
│  │ (MLOONG MX302)  │        │           退避状态 (5分钟)  │  │
│  └─────────────────┘        └────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

**设计原则**:
- **双线程隔离** — 鼠标 HID 读取放在后台工作线程，主 UI 线程负责定时器刷新和 GDI 绘制，消除 HID 读写超时引发的界面卡顿。
- **纯硬编码配置 (方案 A)** — 不引入外部配置文件及任何解析库，参数全部定义在 `config.rs` 中，实现零磁盘 I/O 损耗和极小二进制体积。
- **动态亮度自适应** — 实时读取任务栏背景像素颜色并计算亮度，自适应切换文字颜色，保证在各种主题或透明任务栏软件（如 TranslucentTB）下均清晰可见。
- **双行紧凑布局** — 纵向利用 Win11 任务栏 48px 的高度，横向缩减排版宽度，节约宝贵的任务栏空间。
- **智能挂起与零内存占用** — 锁屏或全屏游戏时挂起采集并强制回收工作集内存，使物理内存占用降至近乎 0MB。

---

## 2. 任务栏窗口嵌入 (Win11 右对齐)

### 2.1 嵌入位置与对齐策略

小组件嵌入在主任务栏 `Shell_TrayWnd` 中，并固定在右侧系统通知区域 `TrayNotifyWnd`（即隐藏托盘图标箭头的左侧）。这种右对齐策略能够完美避开 Win11 图标“靠左”或“居中”对齐方式的冲突。

### 2.2 嵌入与坐标计算

```rust
// 1. 定位主任务栏与系统托盘
let h_taskbar = FindWindowW("Shell_TrayWnd", None);
let h_tray = FindWindowExW(h_taskbar, None, "TrayNotifyWnd", None);

let rc_tray = get_window_rect(h_tray);
let rc_taskbar = get_window_rect(h_taskbar);

// 2. 双行布局尺寸 (高度约占任务栏 2/3)
let display_width = 220; 
let display_height = 32; 

// 3. 计算右对齐位置：托盘左边界 - 间距 - 小组件宽度
let display_x = rc_tray.left - GAP - display_width;
let display_y = (rc_taskbar.bottom - rc_taskbar.top - display_height) / 2;

// 4. 嵌入并设置位置
SetParent(h_my_wnd, h_taskbar);
SetWindowPos(h_my_wnd, HWND_TOP, display_x, display_y, display_width, display_height, SWP_NOACTIVATE);
```

### 2.3 窗口属性

| 属性 | 值 / 说明 |
|------|------|
| 窗口样式 | `WS_CHILD \| WS_VISIBLE`，无边框 |
| 扩展样式 | `WS_EX_LAYERED \| WS_EX_TOOLWINDOW \| WS_EX_NOACTIVATE` |
| 透明背景 | `SetLayeredWindowAttributes(color_key, 0, LWA_COLORKEY)` |
| 重绘机制 | 任务栏重启或重新布局（如显示器分辨率变化）时，重新计算坐标并刷新 |

---

## 3. 系统托盘与退出机制

### 3.1 托盘图标

使用 `Shell_NotifyIconW` 在通知区域创建一个代表 Traffic Monitor 的托盘图标，并在隐藏的顶层窗口上接收 `WM_APP_TRAY` 消息。

### 3.2 右键菜单

响应 `WM_RBUTTONUP`，在鼠标光标位置弹出极简右键菜单：
- **开机自启**：勾选/取消勾选状态。
- **退出**：清理托盘图标、销毁子窗口并调用 `PostQuitMessage(0)` 退出程序。

### 3.3 开机自启实现

写入注册表当前用户启动项：
- 注册表键：`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
- 键值名：`TrafficMonitor`
- 键值数据：当前运行的 `.exe` 绝对路径。

---

## 4. 数据采集

### 4.1 CPU 与内存占用
- **CPU 占用**：使用 `GetSystemTimes` 获取系统时间差值，避免引入 PDH 造成库依赖和初始化开销。
- **内存占用**：通过 `GlobalMemoryStatusEx` 获取 `MEMORYSTATUSEX.dwMemoryLoad`。
- **采集频率**：每 **5 秒** 采集一次。

### 4.2 网速统计（物理网卡过滤）
- **实现方式**：使用 `GetIfTable2` 遍历所有网卡接口，计算在 1s 间隔内的流量差值。
- **过滤机制**：为避免 VPN 虚拟网卡、VMware/WSL 虚拟网卡等造成的网速统计翻倍，仅累加符合以下条件的接口：
  1. 接口类型为 `IF_TYPE_ETHERNET_CSMACD`（以太网）或 `IF_TYPE_IEEE80211`（无线网）。
  2. 接口状态为 `IfOperStatusUp`（联机状态）。
  3. 物理地址（MAC 地址）不为空。
- **采集频率**：每 **1 秒** 采集一次。

### 4.3 鼠标电量与 DPI (双线程轮询)
- **物理协议**：通过 `hidapi` 精准连接 MLOONG MX302（指定 VID/PID/Usage），使用固定 64 字节包交互。
- **双线程隔离**：
  - 在主程序启动时创建一个 Worker 线程。
  - Worker 线程在循环中执行 HID 发送和读取。由于 `read_timeout` 设置为 500ms，其产生的阻塞完全被隔离在 Worker 线程中。
  - 读取成功后，Worker 线程调用 `PostMessageW(h_main_wnd, WM_USER_MOUSE_UPDATE, battery_info, dpi_info)` 异步通知主线程。
- **动态频率退避**：
  * **在线状态**：每 **3 分钟 (180s)** 轮询一次。
  * **离线/休眠状态**（连续 2 次读取失败或找不到设备）：自动将采集间隔降至每 **5 分钟 (300s)** 尝试一次，大幅减少休眠状态下的无效通信。一旦连接/读取成功，立刻恢复 3 分钟的正常采集频率。
- **离线展示**：当鼠标处于离线或休眠状态时，界面展示为 `鼠标: --` 和 `DPI: --`，保持占位空间不变，防止布局抖动。

---

## 5. 文字渲染与智能自适应

### 5.1 双行紧凑排版格式

文字渲染采用双行格式展示：
```
↑ 12.4 KB/s   CPU: 12%   MEM: 45%
↓ 105.2 MB/s  🖱️ 75%     DPI: 1600
```
- 使用鼠标 Emoji 图标 `🖱️` 明确提示鼠标数据。
- 鼠标电量低于 20% 且不在充电状态时，该部分的文字及图标颜色动态切换为**红色**。

### 5.2 任务栏背景色自适应检测

为实现透明背景下文字的良好可读性，程序不读取系统暗黑模式注册表，而是通过“像素点颜色亮度计算”来实现智能自适应。

```rust
// 1. 获取任务栏 DC
let hdc = GetWindowDC(h_taskbar);

// 2. 读取托盘左侧相邻位置的任务栏背景像素点颜色
let pixel_x = rc_tray.left - 10;
let pixel_y = (rc_taskbar.bottom - rc_taskbar.top) / 2;
let color_ref = GetPixel(hdc, pixel_x, pixel_y); 
ReleaseDC(h_taskbar, hdc);

// 3. 提取 RGB 分量
let r = (color_ref & 0xFF) as f64;
let g = ((color_ref >> 8) & 0xFF) as f64;
let b = ((color_ref >> 16) & 0xFF) as f64;

// 4. 心理学亮度公式计算 (Luminance)
let luminance = 0.299 * r + 0.587 * g + 0.114 * b;

// 5. 决定文字前景色
let text_color = if luminance > 125.0 {
    RGB(40, 40, 40)        // 浅色任务栏背景 -> 使用深灰字
} else {
    RGB(255, 255, 255)     // 深色任务栏背景 -> 使用白字
};
```

- 该检测在程序启动时、检测到系统主题更改消息（`WM_SETTINGCHANGE`，且参数为 `"ImmersiveColorSet"`）时重新执行。

### 5.3 渲染管线与资源管理
- 渲染使用双缓冲技术（内存 DC + 内存位图），杜绝文字重画时产生的闪烁。
- 响应 `WM_DPICHANGED` 时重新计算字体大小。在重新创建 `HFONT` 之前，必须显式调用 `DeleteObject(h_old_font)` 销毁旧字体句柄，防止 GDI 资源泄露。

---

## 6. 智能挂起与内存整理 (Working Set Trim)

为达到极致的“小巧低内存”自用目标，引入系统状态监控与主动内存回收：

### 6.1 智能挂起条件
1. **全屏独占检测**：检测当前活动窗口（`GetForegroundWindow`）的尺寸是否等于屏幕分辨率（如全屏游戏或视频），若是，则进入挂起模式。
2. **锁屏/睡眠状态**：监听 `WM_POWERBROADCAST` 消息，若收到 `PBT_APMSUSPEND` 或锁屏状态变化通知，进入挂起模式。

### 6.2 挂起期行为
* 停止所有网速、CPU、内存的采集定时器，关闭 Worker 线程 of 鼠标轮询。
* 调用 Windows API 强制物理内存工作集回收：
  ```rust
  SetProcessWorkingSetSize(GetCurrentProcess(), -1, -1);
  // 或调用 EmptyWorkingSet(GetCurrentProcess())
  ```
  这会指引 Windows 将程序所有的物理内存页移入虚拟内存，使小组件的物理内存占用瞬间归零（降至 ~100KB）。
* 当从睡眠唤醒或退出全屏状态后，重新注册定时器，恢复正常采集，物理内存按需平滑加载。

---

## 7. Cargo.toml 依赖

采用无外部配置文件方案（方案 A），保持依赖的极端纯净：

```toml
[package]
name = "traffic-monitor"
version = "0.1.0"
edition = "2021"

[profile.release]
opt-level = "z"        # 最小体积优化
lto = true             # 编译期全局链接时优化
codegen-units = 1      # 最小化并行编译单元，提升优化空间
strip = true           # 去除全部符号信息

[dependencies]
windows = { version = "0.58", features = [
    "Win32_Foundation",
    "Win32_System_SystemInformation",
    "Win32_System_SystemServices",
    "Win32_System_Threading",
    "Win32_System_Registry",
    "Win32_System_Power",
    "Win32_NetworkManagement_IpHelper",
    "Win32_UI_WindowsAndMessaging",
    "Win32_UI_Shell",
    "Win32_UI_Controls",
    "Win32_UI_Input",
    "Win32_Graphics_Gdi",
]}
hidapi = "2.6"
```

---

## 8. 性能与内存预期指标

- **可执行文件体积**：`< 250 KB`（Release 编译加 `opt-level = "z"` 压缩）。
- **运行期内存 (工作物理内存)**：
  - 稳定采集时：`< 2.5 MB`。
  - 系统挂起/游戏挂起时（经过 Trim 后）：`< 150 KB`。
- **磁盘 I/O**：零（所有配置常量编译期决定，无外部文件读取）。

---

## 9. 潜在 Bug 与防御性编程规范

为确保小组件长期挂机无需人工维护（无卡死、无闪烁、无数据异常），在开发中必须遵循以下防御性规范：

### 9.1 资源管理器崩溃重启应对机制 (Explorer.exe Crash)
- **隐患**：当 `explorer.exe` 崩溃或因 Windows Update 重启时，原任务栏窗口句柄将变为无效死句柄，小组件将从任务栏上彻底消失。
- **防御**：
  1. 调用 `RegisterWindowMessageW("TaskbarCreated")` 向系统注册“任务栏创建”消息。
  2. 在窗口过程 `WndProc` 中拦截此消息。一旦接收，代表资源管理器已重启。
  3. 此时主动销毁旧的子窗口，重新调用 `FindWindowW` 寻找新的托盘，重新执行嵌入定位逻辑，即可让小组件自动“复活”。

### 9.2 网络重连或网卡禁用导致的网速突变 (Arithmetic Overflow)
- **隐患**：网卡禁用后重新启用（或 Wi-Fi 断开重连）时，网卡的字节计数器（`InOctets`/`OutOctets`）会置零。如果直接用“当前流量 - 上次流量”计算网速，会因为数值下溢（Underflow）而计算出一个高达数万 GB/s 的异常突变网速。
- **防御**：
  * 在计算网速时，先判断当前值是否小于上次值。若 `current < prev`，则说明网卡发生了复位。
  * 遇到此情况，丢弃当次的网速计算，强行将网速置为 `0`，并把 `prev` 直接同步为当前值，实现平滑过渡。

### 9.3 睡眠唤醒后的 USB/HID 句柄失效 (Power States Change)
- **隐患**：电脑睡眠（S3/S4）或长时间休眠唤醒后，USB 总线重新供电，原 `hidapi` 打开的设备句柄通常会失效。若继续对其调用 read/write，会导致 Worker 线程陷入无限阻塞或导致程序直接 Panic。
- **防御**：
  1. 主窗口监听 `WM_POWERBROADCAST` 的 `PBT_APMRESUMEAUTOMATIC`（系统自动唤醒消息）。
  2. 收到唤醒消息后，向后台 Worker 线程发送通知，强制关闭旧的 HID 设备句柄，并延迟 1~2 秒等待系统硬件就绪后，重新执行设备发现和 open 操作。

### 9.4 开启分层透明后的文字边缘“黑边”/“杂色” (ClearType Bleeding)
- **隐患**：启用 `WS_EX_LAYERED` 且用 `LWA_COLORKEY` 扣除背景透明色时，如果字体渲染使用了系统的 `ClearType`（子像素反走样），文字边缘的像素会与背景色混合。当背景色被扣除后，文字边缘会残留一圈灰黑色或彩色的杂边（俗称“锯齿黑边”），使文字看起来非常模糊、廉价。
- **防御**：
  1. 保证内存 DC 的填充背景色与实际任务栏底色接近，或者在创建逻辑字体（`CreateFontIndirectW`）时，将 `dwQuality` 参数设为 `NONANTIALIASED_QUALITY`（无反走样）或 `ANTIALIASED_QUALITY`（常规反走样），避开 ClearType 渲染。
  2. 强制使用 `SetBkMode(TRANSPARENT)` 绘制。

### 9.5 多显示器或高分屏下的文字模糊 (High DPI Blur)
- **隐患**：若未向操作系统声明 DPI 自适应，Windows 11 会在高分屏（如 150% 或 200% 缩放）上强行拉伸小组件的像素，导致字体边缘发虚、模糊。
- **防御**：
  * 在 `build.rs` 引入的 XML 清单（manifest）中，除了兼容性声明外，必须加入 `<dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/PM</dpiAware>` 以及 `<dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>`，声明完美的每显示器 DPI 感知。

