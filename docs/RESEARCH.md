# TrafficMonitor 任务栏文字显示实现调研

> 目标：用 Rust 实现一个轻量版，仅在任务栏用文字展示 CPU 占用、内存占用、上下行网速。

---

## 1. 核心机制：窗口嵌入任务栏

TrafficMonitor 的核心思路是**将自定义窗口通过 `SetParent` 嵌入到任务栏的窗口层级中**，然后在窗口上绘制文字。

### 1.1 找到任务栏窗口

```cpp
// 主任务栏
HWND hTaskbar = ::FindWindow(L"Shell_TrayWnd", NULL);

// 副显示器任务栏（通过 EnumWindows 枚举窗口类名为 "Shell_SecondaryTrayWnd" 的窗口）
EnumWindows(EnumWindowsProc, 0);  // TaskbarHelper.cpp:36-51
```

> **Rust 对应**: `winapi` crate 的 `FindWindowW("Shell_TrayWnd", null())` + `EnumWindows`

### 1.2 两种嵌入策略

#### Windows 10 及更早 (ClassicalTaskbarDlg)

文件: `ClassicalTaskbarDlg.cpp:70-84`

1. 找到任务栏内的 `ReBarWindow32` 子窗口（二级容器）
2. 找到 `MSTaskSwWClass` 子窗口（最小化窗口区域，即任务栏图标区）
3. 调用 `::MoveWindow` 把 `MSTaskSwWClass` 的宽度**缩小**，腾出空间
4. 调用 `::SetParent(m_hWnd, m_hBar)` 把自己设置为 `ReBarWindow32` 的子窗口
5. 把自己的窗口放在 `MSTaskSwWClass` 旁边

```
Shell_TrayWnd
├── ReBarWindow32          ← SetParent到这一层
│   ├── MSTaskSwWClass     ← 缩小它，腾空间
│   └── [自定义窗口]        ← 新增
└── TrayNotifyWnd
```

#### Windows 11 (Win11TaskbarDlg)

文件: `Win11TaskbarDlg.cpp:89-94`

Win11 的任务栏结构不同，不再有 `ReBarWindow32`，策略变为：

1. 找到任务栏内的 `TrayNotifyWnd`（通知区域）和 `Start`（开始按钮）
2. 调用 `::SetParent(m_hWnd, m_hTaskbar)` 直接设置为 `Shell_TrayWnd` 的子窗口
3. 根据 `TrayNotifyWnd` 和 `Start` 的位置计算自己的位置，放在通知区域左侧

```cpp
// Win11TaskbarDlg.cpp:91-93
m_hNotify = ::FindWindowEx(m_hTaskbar, 0, L"TrayNotifyWnd", NULL);
m_hStart = ::FindWindowEx(m_hTaskbar, nullptr, L"Start", NULL);
```

### 1.3 SetParent — 关键 API

```cpp
// TaskBarDlg.cpp:999
m_connot_insert_to_task_bar = !(::SetParent(this->m_hWnd, GetParentHwnd()));
```

`SetParent` 将一个窗口的父窗口改为任务栏（或其子容器），使得该窗口成为任务栏的一部分。

> **Rust 对应**: `SetParent(hwnd_my, hwnd_taskbar_container)`

### 1.4 窗口透明

```cpp
// TaskBarDlg.cpp:595-596
SetWindowLong(m_hWnd, GWL_EXSTYLE, GetWindowLong(m_hWnd, GWL_EXSTYLE) | WS_EX_LAYERED);
SetLayeredWindowAttributes(transparent_color, 0, LWA_COLORKEY);
```

使用 `WS_EX_LAYERED` + `SetLayeredWindowAttributes` 的 color key 模式实现透明背景。

> **Rust 对应**: `SetWindowLongPtrW` + `SetLayeredWindowAttributes`，或直接用 D2D 渲染到 layered window。

---

## 2. 文字绘制

### 2.1 绘制流程

文件: `TaskBarDlg.cpp:60-270`

1. `OnPaint` → `ShowInfo(CDC* pDC)`
2. 遍历 `m_item_widths`，计算每个显示项的矩形区域
3. 调用 `DrawDisplayItem` 绘制每个项目

### 2.2 DrawDisplayItem — 绘制单项

文件: `TaskBarDlg.cpp:272-379`

```cpp
void CTaskBarDlg::DrawDisplayItem(IDrawCommon& drawer, DisplayItem type, CRect rect, int label_width, bool vertical)
{
    // 1. 确定颜色（从配置读取）
    COLORREF label_color = ...;
    COLORREF text_color = ...;
    
    // 2. 划分 label 区域和 value 区域
    //    label: 显示"CPU: "等标签
    //    value: 显示"23 %"等数值
    
    // 3. 绘制标签文字
    drawer.DrawWindowText(rect_label, str_label.c_str(), label_color, alignment);
    
    // 4. 绘制数值文字  
    CString str_value = CommonDisplayItem(type).GetItemValueText(false);
    drawer.DrawWindowText(rect_value, str_value, text_color, value_alignment);
}
```

### 2.3 GDI 文字绘制 (默认)

文件: `DrawCommon.h:25` / `DrawCommon.cpp`

最终调用的是 Win32 的 `::DrawTextW`，通过双缓冲（内存 DC）避免闪烁：

```cpp
// DrawCommon.h:65-101
class CDrawDoubleBuffer : public IDrawBuffer
{
    // 创建兼容 DC 和兼容位图
    m_memDC.CreateCompatibleDC(NULL);
    m_memBitmap.CreateCompatibleBitmap(pDC, rect.Width(), rect.Height());
    m_pOldBit = m_memDC.SelectObject(&m_memBitmap);
    
    // 析构时把内存 DC 的内容 BitBlt 到目标 DC
    ~CDrawDoubleBuffer() {
        m_pDC->BitBlt(m_rect.left, m_rect.top, ...);
    }
};
```

> **Rust 简化方案**: 直接用 `BeginPaint` → `SetBkMode(TRANSPARENT)` → `SetTextColor` → `DrawTextW` → `EndPaint`  
> 可配合 `CreateCompatibleDC` + `CreateCompatibleBitmap` + `BitBlt` 做双缓冲防闪烁。

### 2.4 D2D 绘制（可选的透明模式）

启用了额外支持 D2D1 / DirectComposition 渲染，主要用于透明背景场景。对于轻量实现，**只需要 GDI 即可**。

---

## 3. 数据采集

### 3.1 网速采集

文件: `TrafficMonitorDlg.cpp:1171-1241`

```cpp
// 1. 获取接口表
GetIfTable(m_pIfTable, &m_dwSize, FALSE);

// 2. 遍历选中连接的入站/出站字节数
for (auto& connection : m_connections) {
    m_in_bytes  += table.dwInOctets;
    m_out_bytes += table.dwOutOctets;
}

// 3. 计算网速 = 本次字节数增量 / 时间间隔 * 1000
cur_in_speed = m_in_bytes - m_last_in_bytes;
cur_out_speed = m_out_bytes - m_last_out_bytes;
theApp.m_in_speed  = cur_in_speed * 1000 / time_span;
theApp.m_out_speed = cur_out_speed * 1000 / time_span;
```

**关键 API**: `GetIfTable` (iphlpapi.dll)

> **Rust 对应**: 
> - `windows` crate: `windows::Win32::NetworkManagement::IpHelper::GetIfTable2`
> - 或 `winapi` crate: `GetIfTable` / `GetIfEntry2`

### 3.2 CPU 占用采集

文件: `PdhHardwareQuery/CPUUsage.cpp:35-80`

```cpp
// 方式一：PDH 查询（优先）
// 计数器路径: "\\Processor Information(_Total)\\% Processor Utility" (Win10+)
//              "\\Processor Information(_Total)\\% Processor Time"     (Win7/8)
m_pdh_cup_usage_query.GetCPUUsage(cpu_usage);

// 方式二：GetSystemTimes 回退
GetSystemTimes(&idleTime, &kernelTime, &userTime);
cpu_usage = (kernel + user - idle) * 100 / (kernel + user);
```

> **Rust 对应**: 
> - PDH: `windows` crate 的 `PdhOpenQueryW` + `PdhAddCounterW` + `PdhCollectQueryData` + `PdhGetFormattedCounterValue`
> - 或更简单的 `GetSystemTimes` fallback

### 3.3 内存占用采集

文件: `TrafficMonitorDlg.cpp:1401-1407`

```cpp
MEMORYSTATUSEX statex;
statex.dwLength = sizeof(statex);
GlobalMemoryStatusEx(&statex);
theApp.m_memory_usage = statex.dwMemoryLoad;  // 内存使用百分比 0-100
theApp.m_used_memory  = (ullTotalPhys - ullAvailPhys) / 1024; // 已用内存 (KB)
theApp.m_total_memory = ullTotalPhys / 1024;                   // 总内存 (KB)
```

**关键 API**: `GlobalMemoryStatusEx`

> **Rust 对应**: `windows` crate 的 `GlobalMemoryStatusEx`

### 3.4 定时采集

文件: `TrafficMonitorDlg.cpp:1567` (OnTimer)

```cpp
SetTimer(TIMER_ID, 1000, NULL);  // 1秒定时器
```

> **Rust 对应**: `SetTimer` + `WM_TIMER` 消息处理

---

## 4. 窗口生命周期

### 4.1 创建

文件: `TrafficMonitorDlg.cpp:566-597`

```cpp
void CTrafficMonitorDlg::OpenTaskBarWnd()
{
    // 根据系统版本选择不同的实现
    if (theApp.IsWindows11Taskbar())
        m_tBarDlg = new CWin11TaskbarDlg();
    else
        m_tBarDlg = new CClassicalTaskbarDlg();
    
    m_tBarDlg->Create(IDD_TASK_BAR_DIALOG, this);
    m_tBarDlg->ShowWindow(SW_SHOW);
}
```

### 4.2 销毁时恢复

```cpp
// ClassicalTaskbarDlg.cpp:86-97
void CClassicalTaskbarDlg::ResetTaskbarPos()
{
    // 把 MSTaskSwWClass 的宽度恢复为原来的值
    ::MoveWindow(m_hMin, m_left_space, 0, m_rcMinOri.Width(), m_rcMinOri.Height(), TRUE);
}
```

---

## 5. Rust 实现方案建议

### 5.1 最小可行方案

```
┌─────────────────────────────────────────────────┐
│ 1. 创建隐藏的主窗口 (消息循环用)                    │
│ 2. 创建子窗口作为任务栏显示窗口                     │
│ 3. FindWindow("Shell_TrayWnd") 找到任务栏          │
│ 4. FindWindowEx 找到 ReBarWindow32 (Win10)        │
│ 5. SetParent 嵌入子窗口                            │
│ 6. SetTimer(1秒) 定时采集数据 + InvalidateRect     │
│ 7. WM_PAINT 中用 DrawTextW 绘制文字                │
│ 8. WM_DESTROY 时恢复任务栏布局                      │
└─────────────────────────────────────────────────┘
```

### 5.2 推荐 Rust crate

| 功能 | crate |
|------|-------|
| Windows API (窗口、GDI、系统调用) | `windows` (官方) |
| 或传统方式 | `winapi` |
| 网速 | `GetIfTable2` from `windows::Win32::NetworkManagement::IpHelper` |
| CPU | `Pdh*` 系列 或 `GetSystemTimes` |
| 内存 | `GlobalMemoryStatusEx` |
| 窗口创建 | `CreateWindowExW`, `RegisterClassW` |
| 文字绘制 | `DrawTextW` + 双缓冲 `BitBlt` |

### 5.3 关键 Windows API 清单

| API | 用途 |
|-----|------|
| `FindWindowW` | 查找 Shell_TrayWnd |
| `FindWindowExW` | 查找 ReBarWindow32 / TrayNotifyWnd / Start |
| `SetParent` | 嵌入到任务栏 |
| `MoveWindow` | 调整 MSTaskSwWClass 尺寸，给自己的窗口定位 |
| `GetWindowRect` | 获取窗口矩形 |
| `CreateWindowExW` | 创建自定义窗口 |
| `SetWindowLongPtrW` | 设置 WS_EX_LAYERED 等样式 |
| `SetLayeredWindowAttributes` | 透明色 keying |
| `BeginPaint` / `EndPaint` | 获取/释放 DC |
| `CreateCompatibleDC` | 创建内存 DC（双缓冲） |
| `CreateCompatibleBitmap` | 创建内存位图 |
| `SelectObject` | 选择字体/位图到 DC |
| `SetBkMode(TRANSPARENT)` | 文字背景透明 |
| `SetTextColor` | 设置文字颜色 |
| `DrawTextW` | 绘制文字 |
| `BitBlt` | 位图传输（双缓冲） |
| `SetTimer` / `KillTimer` | 定时器 |
| `RegisterClassW` | 注册窗口类 |
| `GetMessageW` / `DispatchMessageW` | 消息循环 |
| `PostQuitMessage` | 退出消息循环 |
| `GetIfTable2` / `GetIfTable` | 获取网络接口表 |
| `GlobalMemoryStatusEx` | 获取内存状态 |
| `GetSystemTimes` | 获取 CPU 时间（回退方案） |
| `PdhOpenQueryW` / `PdhAddCounterW` / `PdhCollectQueryData` / `PdhGetFormattedCounterValue` | PDH CPU 查询 |

### 5.4 简化后的窗口嵌入逻辑（伪代码）

```rust
// 1. 找到任务栏
let h_taskbar = FindWindowW("Shell_TrayWnd", None);

// 2. 判断 Win11 vs Win10（可检测 FindWindowEx(taskbar, "Start") 是否存在）
let h_start = FindWindowExW(h_taskbar, None, "Start", None);
let is_win11 = !h_start.is_null();

// 3. 找到父容器
let (h_parent, need_shrink_minwin) = if is_win11 {
    (h_taskbar, false)
} else {
    let h_bar = FindWindowExW(h_taskbar, None, "ReBarWindow32", None);
    (h_bar, true)
};

// 4. 如果 Win10，缩小 MSTaskSwWClass 腾空间
if need_shrink_minwin {
    let h_min = FindWindowExW(h_parent, None, "MSTaskSwWClass", None);
    GetWindowRect(h_min, &mut rc_min_original);
    MoveWindow(h_min, left, top, rc_min.width - MY_WIDTH, rc_min.height, TRUE);
}

// 5. 嵌入窗口
SetParent(h_my_wnd, h_parent);

// 6. 定位窗口
MoveWindow(h_my_wnd, x, y, MY_WIDTH, MY_HEIGHT, TRUE);
```

---

## 6. 项目文件对照表

| 功能 | 文件 | 行号 |
|------|------|------|
| 查找任务栏句柄 | `TaskBarDlg.cpp` | 670-696 |
| SetParent 嵌入 | `TaskBarDlg.cpp` | 999 |
| Win10 任务栏嵌入逻辑 | `ClassicalTaskbarDlg.cpp` | 70-84 |
| Win11 任务栏嵌入逻辑 | `Win11TaskbarDlg.cpp` | 89-94 |
| 窗口定位 | `ClassicalTaskbarDlg.cpp` | 4-67 (AdjustTaskbarWndPos) |
| Win11 窗口定位 | `Win11TaskbarDlg.cpp` | 5-87 (AdjustTaskbarWndPos) |
| 文字绘制 | `TaskBarDlg.cpp` | 272-379 (DrawDisplayItem) |
| 透明色设置 | `TaskBarDlg.cpp` | 568-603 (ApplyWindowTransparentColor) |
| 双缓冲绘制 | `DrawCommon.h` | 65-101 (CDrawDoubleBuffer) |
| GDI DrawText 接口 | `IDrawCommon.h` | 33 (DrawWindowText) |
| OnPaint 入口 | `TaskBarDlg.cpp` | 1294-1348 |
| OnTimer 刷新 | `TaskBarDlg.cpp` | 1230-1253 |
| 网速采集 | `TrafficMonitorDlg.cpp` | 1171-1241 |
| CPU 采集 | `CPUUsage.cpp` | 35-80 |
| 内存采集 | `TrafficMonitorDlg.cpp` | 1401-1407 |
| 窗口创建 | `TrafficMonitorDlg.cpp` | 566-597 (OpenTaskBarWnd) |
| 窗口销毁恢复 | `ClassicalTaskbarDlg.cpp` | 86-97 (ResetTaskbarPos) |
| 副显示器任务栏 | `TaskbarHelper.cpp` | 36-102 |
