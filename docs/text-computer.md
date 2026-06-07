# TrafficMonitor 任务栏文字尺寸与颜色逻辑分析报告

本报告针对 C++ 开源项目 [TrafficMonitor](file:///D:/work_space/fork/TrafficMonitor) 的任务栏窗口进行分析，详细阐述其**文字大小适配（DPI缩放）**和**文字颜色自动适应机制**，并为您的 Rust 任务栏项目提供相应的实现建议。

---

## 1. 任务栏文字尺寸适配经验

在 Windows 任务栏上显示文字，如果尺寸不对、文字偏小，主要是因为**忽略了系统的 DPI 缩放**，以及在调用 Windows 字体 API 时**混淆了字号（Points）与像素高度（Pixels）**。

### A. 基于 DPI 缩放的窗口尺寸设计

TrafficMonitor 在 [TaskBarDlg.h](file:///D:/work_space/fork/TrafficMonitor/TrafficMonitor/TaskBarDlg.h#L14) 中定义了任务栏窗口的基准高度：

```cpp
#define TASKBAR_WND_HEIGHT DPI(32) // 任务栏窗口的基准高度
```

- 基准高度以 **96 DPI**（即系统 100% 缩放）为标准，定义为 `32` 像素。
- 在实际运行时，程序会获取当前显示器屏幕的 DPI。
- 通过 [CTaskBarDlg::DPI](file:///D:/work_space/fork/TrafficMonitor/TrafficMonitor/TaskBarDlg.cpp#L640) 成员函数将基准像素转换为实际像素：
  $$\text{实际像素} = \frac{\text{当前 DPI} \times \text{基准像素}}{96}$$
  _例如：在 150% 缩放（144 DPI）下，高度会自动变为 $32 \times 144 / 96 = 48$ 像素。如果不做这个转换，在高分屏上窗口和文字都会显得非常窄小。_

### B. 字体大小换算为 `lfHeight`

Windows API（如 `CreateFont` 或 `CreateFontIndirect`）的 `lfHeight` 字段并不是直接填入字号（如 9 pt），而是必须转为**设备像素高度**。
在 [CommonData.h](file:///D:/work_space/fork/TrafficMonitor/TrafficMonitor/CommonData.h#L102) 中，TrafficMonitor 提供了 [FontSizeToLfHeight](file:///D:/work_space/fork/TrafficMonitor/TrafficMonitor/CommonData.h#L102) 函数：

```cpp
inline int FontSizeToLfHeight(int font_size, int dpi = 0)
{
    if (dpi == 0)
    {
        HDC hDC = ::GetDC(HWND_DESKTOP);
        dpi = GetDeviceCaps(hDC, LOGPIXELSY);
        ::ReleaseDC(HWND_DESKTOP, hDC);
    }
    // 计算公式
    int lfHeight = -MulDiv(font_size, dpi, 72);
    return lfHeight;
}
```

> [!IMPORTANT]
> **字体像素高度换算公式：**
> $$\text{lfHeight} = - \text{round}\left( \frac{\text{字号 (pt)} \times \text{当前 DPI}}{72} \right)$$
> 如果直接将 9 号字作为 `9` 传给字体创建 API，相当于在 96 DPI 下请求了一个大约 6.75 pt 的超小字体，字就会非常小。必须通过公式将其转为实际高度像素（96 DPI 下 9 pt 对应 `-12` 像素；150% 缩放下对应 `-18` 像素）。

### C. 双行渲染的排版布局

在 [CTaskBarDlg::CalculateWindowSize](file:///D:/work_space/fork/TrafficMonitor/TrafficMonitor/TaskBarDlg.cpp#L812) 中：

- 任务栏在顶部或底部且非水平排列时（即双行显示，如“上行网速、下行网速”），单行的高度上限会被平分为：`TASKBAR_WND_HEIGHT / 2`。
- 在 100% DPI 下，半高为 16 像素。
- 绘制文字的矩形限制在对应的半高内，文字高度设为 12 像素，在 GDI 绘制时使用对齐参数 `DT_VCENTER | DT_SINGLELINE`（或 DirectWrite 的 `DWRITE_PARAGRAPH_ALIGNMENT_CENTER`），使文字在 16 像素高的矩形正中间垂直居中，视觉效果极佳。

### D. 动态测量文字宽度

由于网速、CPU 占用是动态变化的，TrafficMonitor 在更新数据后会调用 `GetTextExtent`（GDI）或 DWrite 接口，计算文本实际渲染出来的像素宽度，并累加标签与数值之间的间距，最后动态调用 `SetWindowPos` 调整任务栏子窗口宽度。这可以避免窗口太小导致字被截断。

---

## 2. 文字颜色逻辑与深浅背景适配

TrafficMonitor 没有采用复杂的屏幕图像像素分析，而是利用 Windows 提供的**主题配置注册表键值**来实现极低成本、极高稳定性的自动颜色适应。

### A. 系统深浅色主题检测

在 [WindowsSettingHelper.cpp](file:///D:/work_space/fork/TrafficMonitor/TrafficMonitor/WindowsSettingHelper.cpp#L11) 中，[CheckWindows10LightTheme](file:///D:/work_space/fork/TrafficMonitor/TrafficMonitor/WindowsSettingHelper.cpp#L11) 每秒监测一次注册表值：

- **注册表路径**：`HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`
- **键名**：`SystemUsesLightTheme` (DWORD 类型)
  - `1` $\rightarrow$ 系统目前是**浅色主题**（亮色背景，白底任务栏）。
  - `0` $\rightarrow$ 系统目前是**深色主题**（暗色背景，黑底任务栏）。

### B. 样式预设机制 (Style Presets)

软件内置了多套颜色样式（即预设配置），分别代表不同的主题表现：

- **深色主题样式**（如默认预设 1）：
  - 背景色：黑色（`0`）或透明。
  - 文字颜色：**白色**（`RGB(255, 255, 255)`）。
- **浅色主题样式**（如默认预设 3）：
  - 背景色：浅灰色（`RGB(210, 210, 211)`）或透明。
  - 文字颜色：**黑色**（`RGB(0, 0, 0)`）。

### C. 自动适配流程

当检测到系统主题发生变化时：

1. 从注册表读取到新的 `SystemUsesLightTheme` 状态。
2. 依据新的状态，在 [TaskbarDefaultStyle.cpp](file:///D:/work_space/fork/TrafficMonitor/TrafficMonitor/TaskbarDefaultStyle.cpp#L85) 的 [ApplyDefaultStyle](file:///D:/work_space/fork/TrafficMonitor/TrafficMonitor/TaskbarDefaultStyle.cpp#L85) 方法中，将当前运行的全局颜色配置替换为对应的预设方案。
3. 重新绘制任务栏窗口，实现毫秒级的平滑过渡。

### D. 防冲突安全检查

为避免用户自定义颜色时，将文字颜色改得跟背景颜色一模一样导致“字隐形”，[IsTaskBarStyleDataValid](file:///D:/work_space/fork/TrafficMonitor/TrafficMonitor/TaskbarDefaultStyle.cpp#L133) 会进行校验：

```cpp
bool CTaskbarDefaultStyle::IsTaskBarStyleDataValid(const TaskBarStyleData& data)
{
    for (const auto& item : data.text_colors)
    {
        // 只要有任意一项文字的标签色或数值色不等于背景色，就说明颜色有效
        if (item.second.label != data.back_color || item.second.value != data.back_color)
            return true;
    }
    return false; // 如果完全相同，则该预设非法，拒绝应用并报错
}
```

---

## 3. 给您的 Rust 任务栏项目的实现建议

以下是您可以在 Rust 代码中参考并实现的逻辑框架：

### A. 获取系统 DPI 与计算字高 (Rust 示例)

在 Rust 中，您可以使用 `windows` 或 `winapi` crate。

```rust
use std::ptr;
use windows::Win32::Graphics::Gdi::{GetDC, ReleaseDC, GetDeviceCaps, LOGPIXELSY};
use windows::Win32::UI::WindowsAndMessaging::{GetDesktopWindow, MulDiv};
use windows::Win32::Graphics::Gdi::{CreateFontW, HFONT, FW_NORMAL, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, DEFAULT_QUALITY, DEFAULT_PITCH, FF_SWISS};

// 1. 获取当前系统/桌面的 DPI
pub unsafe fn get_dpi() -> i32 {
    let hwnd = GetDesktopWindow();
    let hdc = GetDC(hwnd);
    let dpi = GetDeviceCaps(hdc, LOGPIXELSY);
    ReleaseDC(hwnd, hdc);
    dpi
}

// 2. 将字号 pt 换算为 lfHeight
pub fn font_size_to_lf_height(font_size_pt: i32, dpi: i32) -> i32 {
    // 换算公式，结果为负数，代表绝对像素高度
    -MulDiv(font_size_pt, dpi, 72)
}

// 3. 在 Rust 中正确创建适配 DPI 的字体
pub unsafe fn create_taskbar_font(font_size_pt: i32) -> HFONT {
    let dpi = get_dpi();
    let lf_height = font_size_to_lf_height(font_size_pt, dpi);

    // 使用 Win32 API 创建字体
    CreateFontW(
        lf_height,           // nHeight (关键：传入换算后的像素高度)
        0,                   // nWidth
        0,                   // nEscapement
        0,                   // nOrientation
        FW_NORMAL.0 as i32,  // fnWeight
        0,                   // fdwItalic
        0,                   // fdwUnderline
        0,                   // fdwStrikeOut
        DEFAULT_CHARSET.0 as u32, // fdwCharSet
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        DEFAULT_QUALITY.0 as u32,
        (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
        windows::core::w!("Segoe UI"), // 字体名称
    )
}
```

### B. 检测 Windows 10/11 的深浅色主题

在 Rust 中读取注册表，以确定当前任务栏处于什么背景色中，从而决定使用黑色字还是白色字。
您可以引入 `winreg` crate：

```rust
use winreg::enums::*;
use winreg::RegKey;

#[derive(Debug, PartialEq)]
pub enum TaskbarTheme {
    Dark,  // 深色（黑色背景，应采用白色字）
    Light, // 浅色（白色背景，应采用黑色字）
}

pub fn detect_taskbar_theme() -> TaskbarTheme {
    let hkcu = RegKey::predefined(HKEY_CURRENT_USER);
    // 尝试打开注册表路径
    if let Ok(subkey) = hkcu.open_subkey_with_flags(
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        KEY_READ,
    ) {
        // 读取 SystemUsesLightTheme 键值
        if let Ok(value) = subkey.get_value::<u32, _>("SystemUsesLightTheme") {
            if value == 1 {
                return TaskbarTheme::Light;
            }
        }
    }
    // 默认返回深色模式（Windows 默认状态）
    TaskbarTheme::Dark
}
```

### C. 绘制时的颜色方案与排版

1. **获取主题**：程序初始化以及在后台定时器（比如每隔 1-2 秒）检测一次 `detect_taskbar_theme()`，判断状态。
2. **应用颜色**：
   - 如果是 `TaskbarTheme::Dark`，将文本画刷的颜色设置为 `RGB(255, 255, 255)`。
   - 如果是 `TaskbarTheme::Light`，将文本画刷的颜色设置为 `RGB(0, 0, 0)`。
3. **分配矩形范围**：
   - 任务栏在下方时，窗口高度大约为 `GetSystemMetrics(SM_CYSIZEFRAME) + ...`，或动态获取任务栏窗口本身的高度。
   - 在 Rust 中绘图时，确定文本绘制所处的 `RECT`（矩形框）。在绘制单行时，垂直居中对齐文字；绘制多行时，计算 `RECT.bottom = RECT.top + (wnd_height / 2)`，使得每一行都有一个明确的半高矩形界限，从而保证文字不会堆叠或偏小。
