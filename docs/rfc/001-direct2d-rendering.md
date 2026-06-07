# RFC: 渲染引擎向 Direct2D 与 DirectWrite 演进

## 背景
目前 Traffic Monitor 的 GUI 渲染完全基于经典的 Win32 GDI 技术（位于 `renderer.rs`）。采用了双缓冲策略（`CreateCompatibleBitmap` + `BitBlt`）来解决闪烁问题，并通过指定 `NONANTIALIASED_QUALITY` 以去除在 `LWA_COLORKEY` (Layered Window) 下常见的字体边缘粉色伪影。

随着 Windows 系统对高分辨率（High-DPI）的支持日益完善以及各种现代字体（如带彩色的 Emoji）、次像素抗锯齿 (ClearType) 的普及，GDI 越来越难以提供与系统原生组件（如 Windows 11 任务栏自带组件）一致的视觉效果。
具体表现为：
1. 无法渲染彩色 Emoji（如目前的 `🖱️` 显示为单色线框）。
2. 在某些非整数缩放倍率下，字体边缘依然不够平滑。

## 提议方案
废弃现有的 GDI 渲染管线，引入 **Direct2D (D2D1)** 进行图形绘制，引入 **DirectWrite (DWrite)** 进行排版与高级字体渲染。

### 架构变更
1. **渲染器初始化**：
   引入 `ID2D1Factory`、`IDWriteFactory` 进行全局渲染工厂初始化。
2. **设备资源管理 (Device-Dependent Resources)**：
   创建 `ID2D1HwndRenderTarget` 绑定到主窗口，并根据主题模式创建所需的 `ID2D1SolidColorBrush`。在渲染循环中必须妥善捕获并处理 `D2DERR_RECREATE_TARGET`（设备丢失/显卡重启）的错误，按需重建所有设备相关资源。
3. **文本布局 (Text Layout)**：
   抛弃 GDI `DrawTextW` 中基于标志位的粗放排版模式。转而使用 `IDWriteTextFormat` 定义字体样式，并对各个独立的显示区块（CPU、内存、网速、鼠标）生成各自的 `IDWriteTextLayout` 对象，以实现精确的排版对齐和绘制。

## 具体修改内容（结构示意）

在 `renderer.rs` 中：
```rust
struct D2DRenderer {
    factory: ID2D1Factory,
    write_factory: IDWriteFactory,
    render_target: Option<ID2D1HwndRenderTarget>,
    text_format: IDWriteTextFormat,
    // ...
}

impl D2DRenderer {
    pub fn render(&mut self, hwnd: HWND) {
        let rt = self.get_or_create_render_target(hwnd);
        rt.BeginDraw();
        rt.Clear(...);

        // 绘制彩色 Emoji 和使用硬件级抗锯齿的字体
        let layout = self.create_text_layout("🖱️ 100%", ...);
        rt.DrawTextLayout(..., layout, self.brush_text);

        if let Err(e) = rt.EndDraw() {
            // 处理设备丢失的情况，卸载渲染目标以便在下一帧重建
            self.render_target = None;
        }
    }
}
```

## 工程量与风险评估
- **工程量**：中偏大。整个 `renderer.rs` （近 500 行代码）将面临大规模重构。由于底层从简单的句柄直接绘制过渡到了面向对象的 COM 接口，开发者需要较好地掌握并处理 `windows-rs` 关于 COM 对象生命周期的隐式管理。
- **风险**：
  1. **构建体积增大**：需要启用 `windows` crate 的更多特性依赖（如 `Win32_Graphics_Direct2D` 和 `Win32_Graphics_DirectWrite`），这会微量增加最终 Release 二进制文件的体积及编译耗时。
  2. **状态维护成本**：设备丢失（GPU 驱动重置、深度休眠唤醒时）的处理较为隐蔽，若处理遗漏将直接导致界面永远冻结不再绘制。
- **收益**：全面提升小组件的图形渲染质感，从视觉上彻底解决高 DPI 下字体发虚、边缘锯齿及特殊字符（Emoji）无法多彩显示的问题，甚至可以借此引入极其平滑的高帧率动画。
