use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{COLORREF, HWND, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW, CreateSolidBrush,
    DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW,
    FONT_QUALITY, FillRect, GetTextExtentPoint32W, GetWindowDC, HBITMAP, HBRUSH, HDC, HFONT,
    HGDIOBJ, LOGFONTW, ReleaseDC, SRCCOPY, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};

use crate::config::{
    COLOR_CRIT_TEXT, COLOR_DARK_TEXT, COLOR_HOT_TEXT, COLOR_KEY, COLOR_LIGHT_TEXT, DISPLAY_HEIGHT,
    DISPLAY_WIDTH, FONT_BASE_SIZE,
};
use crate::state::{CPU_USAGE, MEM_USAGE, NET_SPEED_DOWN, NET_SPEED_UP, THERMAL_STATE};
use crate::util::{reg_read_dword, to_wide};

/// `Renderer::new()` 失败的具体原因。所有失败均发生在 GDI 资源创建阶段，
/// 一旦创建全部成功，后续选入、设置背景模式与测量都无法失败。
#[derive(Debug)]
pub enum RendererError {
    ScreenDcUnavailable,
    MemoryDcCreationFailed,
    BitmapCreationFailed,
    FontCreationFailed,
    BrushCreationFailed,
}

impl std::fmt::Display for RendererError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            Self::ScreenDcUnavailable => "无法获取屏幕设备上下文",
            Self::MemoryDcCreationFailed => "无法创建兼容内存 DC",
            Self::BitmapCreationFailed => "无法创建兼容位图",
            Self::FontCreationFailed => "无法创建字体",
            Self::BrushCreationFailed => "无法创建背景刷子",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for RendererError {}

pub struct Renderer {
    hdc_mem: HDC,
    hbitmap: HBITMAP,
    hfont: HFONT,
    old_bitmap: HGDIOBJ,
    old_font: HGDIOBJ,
    hbrush: HBRUSH,
    text_color: COLORREF,
    font_size: i32,
    width: i32,
    height: i32,
    arrow_width: i32,
    buf: Vec<u16>,
}

// ===== 模块私有 RAII 资源守卫 =====
//
// 设计目标：构造 `Renderer` 时所有 GDI 对象按依赖顺序创建，任何一步失败时
// 由局部守卫的 Drop 自动清理已申请的资源，无需手写多分支释放。
// 成功路径在最后一刻通过 `mem::forget` 把所有权移交给 `Renderer`，由
// `Renderer::drop` 负责“还原默认对象 → 销毁独占对象 → 释放 DC”的标准释放序。

/// 临时屏幕 DC 守卫：构造时通过 `GetWindowDC(null)` 获取，`Drop` 时 `ReleaseDC`。
struct ScreenDcGuard {
    hdc: HDC,
}

impl ScreenDcGuard {
    fn acquire() -> Option<Self> {
        // SAFETY: 传入 nullptr 句柄获取整个桌面屏幕的 HDC，是 Win32 获取
        // 兼容 GDI 资源所需源 DC 的标准方式。失败时返回无效句柄。
        let hdc = unsafe { GetWindowDC(Some(HWND(std::ptr::null_mut()))) };
        if hdc.is_invalid() {
            None
        } else {
            Some(Self { hdc })
        }
    }
}

impl Drop for ScreenDcGuard {
    fn drop(&mut self) {
        // SAFETY: self.hdc 来自 `GetWindowDC(HWND=null)`，配对释放 API 是
        // `ReleaseDC` 且必须传入相同的 HWND（桌面 nullptr）。
        unsafe {
            let _ = ReleaseDC(Some(HWND(std::ptr::null_mut())), self.hdc);
        }
    }
}

/// 内存兼容 DC 守卫：由 `CreateCompatibleDC` 创建，`Drop` 时 `DeleteDC`。
struct OwnedDc {
    hdc: HDC,
}

impl OwnedDc {
    fn new(hdc_screen: HDC) -> Option<Self> {
        // SAFETY: hdc_screen 由调用方传入且已验证为有效屏幕 HDC。
        let hdc = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
        if hdc.is_invalid() {
            None
        } else {
            Some(Self { hdc })
        }
    }
}

impl Drop for OwnedDc {
    fn drop(&mut self) {
        // SAFETY: self.hdc 由 `CreateCompatibleDC` 创建。构造期间若选入了对象，
        // 其所有权已由上层管理；本守卫仅出现在构造失败的早期阶段——此时 DC 中尚未
        // 选入任何独占对象——`DeleteDC` 是其正确释放 API。
        unsafe {
            let _ = DeleteDC(self.hdc);
        }
    }
}

/// 位图守卫：由 `CreateCompatibleBitmap` 创建，`Drop` 时 `DeleteObject`。
struct OwnedBitmap {
    hb: HBITMAP,
}

impl OwnedBitmap {
    fn new(hdc_screen: HDC, width: i32, height: i32) -> Option<Self> {
        // SAFETY: 必须使用屏幕 DC 以匹配屏幕颜色格式；width/height 为正像素尺寸。
        let hb = unsafe { CreateCompatibleBitmap(hdc_screen, width, height) };
        if hb.is_invalid() {
            None
        } else {
            Some(Self { hb })
        }
    }
}

impl Drop for OwnedBitmap {
    fn drop(&mut self) {
        // SAFETY: self.hb 由 `CreateCompatibleBitmap` 创建并被当前进程独占。
        unsafe {
            let _ = DeleteObject(self.hb.into());
        }
    }
}

/// 字体守卫：由 `CreateFontIndirectW` 创建，`Drop` 时 `DeleteObject`。
struct OwnedFont {
    hf: HFONT,
}

impl OwnedFont {
    fn new(size: i32) -> Option<Self> {
        let hf = create_font(size);
        if hf.is_invalid() {
            None
        } else {
            Some(Self { hf })
        }
    }
}

impl Drop for OwnedFont {
    fn drop(&mut self) {
        // SAFETY: self.hf 由 `CreateFontIndirectW` 创建并被独占。
        unsafe {
            let _ = DeleteObject(self.hf.into());
        }
    }
}

/// 刷子守卫：由 `CreateSolidBrush` 创建，`Drop` 时 `DeleteObject`。
struct OwnedBrush {
    hbr: HBRUSH,
}

impl OwnedBrush {
    fn new() -> Option<Self> {
        // SAFETY: COLOR_KEY 是合法的 COLORREF 常量。
        let hbr = unsafe { CreateSolidBrush(COLORREF(COLOR_KEY)) };
        if hbr.is_invalid() {
            None
        } else {
            Some(Self { hbr })
        }
    }
}

impl Drop for OwnedBrush {
    fn drop(&mut self) {
        // SAFETY: self.hbr 由 `CreateSolidBrush` 创建并被独占。
        unsafe {
            let _ = DeleteObject(self.hbr.into());
        }
    }
}

impl Renderer {
    /// 事务式构造：所有 GDI 资源按依赖顺序创建，任何一步失败由局部 RAII 守卫
    /// 自动清理；全部成功后选入对象并移交所有权给 `Renderer`。
    pub fn new() -> Result<Self, RendererError> {
        // 1. 临时屏幕 DC（Drop 时 ReleaseDC）。
        let screen_dc = ScreenDcGuard::acquire().ok_or(RendererError::ScreenDcUnavailable)?;

        // 2. 兼容内存 DC（Drop 时 DeleteDC）。
        let dc = OwnedDc::new(screen_dc.hdc).ok_or(RendererError::MemoryDcCreationFailed)?;

        // 3. 兼容位图（Drop 时 DeleteObject）。必须使用屏幕 DC 而非内存 DC。
        let bitmap = OwnedBitmap::new(screen_dc.hdc, DISPLAY_WIDTH, DISPLAY_HEIGHT)
            .ok_or(RendererError::BitmapCreationFailed)?;

        // 4. 字体（Drop 时 DeleteObject）。
        let font = OwnedFont::new(FONT_BASE_SIZE).ok_or(RendererError::FontCreationFailed)?;

        // 5. 背景刷子（Drop 时 DeleteObject）。
        let brush = OwnedBrush::new().ok_or(RendererError::BrushCreationFailed)?;

        // ── 至此所有可失败步骤均已成功。后续选入/测量/配置均不会失败。──

        // 6. 选入位图并备份原默认位图（stock 1x1 位图）。
        // SAFETY: dc 与 bitmap 均为刚创建的有效独占句柄。
        let old_bitmap = unsafe { SelectObject(dc.hdc, bitmap.hb.into()) };

        // 7. 选入字体并备份原默认字体（stock 系统字体）。
        // SAFETY: dc 与 font 均为有效独占句柄。
        let old_font = unsafe { SelectObject(dc.hdc, font.hf.into()) };

        // 8. 设置背景模式为透明，便于 DrawTextW 与位图 blit 保留透明色键。
        // SAFETY: dc.hdc 有效。
        unsafe {
            let _ = SetBkMode(dc.hdc, TRANSPARENT);
        }

        // 9. 测量箭头宽度（依赖已选入的字体）。
        let arrow_width = measure_arrow_width(dc.hdc);

        // 10. 释放临时屏幕 DC（其使命仅限于提供创建上下文）。
        drop(screen_dc);

        // 11. 把独占资源所有权移交给 Renderer，杜绝守卫 Drop 误释放仍在使用的对象。
        let renderer = Self {
            hdc_mem: dc.hdc,
            hbitmap: bitmap.hb,
            hfont: font.hf,
            old_bitmap,
            old_font,
            hbrush: brush.hbr,
            text_color: COLORREF(COLOR_LIGHT_TEXT),
            font_size: FONT_BASE_SIZE,
            width: DISPLAY_WIDTH,
            height: DISPLAY_HEIGHT,
            arrow_width,
            buf: Vec::with_capacity(32),
        };
        std::mem::forget(dc);
        std::mem::forget(bitmap);
        std::mem::forget(font);
        std::mem::forget(brush);

        Ok(renderer)
    }

    pub fn update_text_color(&mut self) {
        if is_system_light_theme() {
            self.text_color = COLORREF(COLOR_DARK_TEXT);
        } else {
            self.text_color = COLORREF(COLOR_LIGHT_TEXT);
        }
    }

    fn wide<'a>(buf: &'a mut Vec<u16>, s: &str) -> &'a mut [u16] {
        buf.clear();
        buf.extend(s.encode_utf16());
        buf.push(0);
        buf
    }

    fn format_cpu_mem_wide<'a>(buf: &'a mut Vec<u16>, label: &str, value: u32) -> &'a mut [u16] {
        buf.clear();
        push_ascii(buf, label);
        push_ascii(buf, ": ");
        write_u32(buf, value);
        push_ascii(buf, "%");
        buf.push(0);
        buf
    }

    fn format_speed_wide(buf: &mut Vec<u16>, bytes_per_sec: u32) -> &mut [u16] {
        buf.clear();
        if bytes_per_sec < 1024 {
            write_u32(buf, bytes_per_sec);
            push_ascii(buf, " B/s");
        } else if bytes_per_sec < 1024 * 1024 {
            // 整数定点：乘 10 后加半除数四舍五入，得到十分位精度值。
            let x = ((bytes_per_sec as u64 * 10 + 512) / 1024) as u32;
            write_u32(buf, x / 10);
            buf.push(b'.' as u16);
            buf.push((b'0' + (x % 10) as u8) as u16);
            push_ascii(buf, " KB/s");
        } else {
            let x = ((bytes_per_sec as u64 * 10 + 524288) / (1024 * 1024)) as u32;
            write_u32(buf, x / 10);
            buf.push(b'.' as u16);
            buf.push((b'0' + (x % 10) as u8) as u16);
            push_ascii(buf, " MB/s");
        }
        buf.push(0);
        buf
    }

    pub fn render(&mut self, hdc: HDC) {
        let rect = RECT {
            left: 0,
            top: 0,
            right: self.width,
            bottom: self.height,
        };

        let speed_up = NET_SPEED_UP.load(Ordering::Relaxed);
        let speed_down = NET_SPEED_DOWN.load(Ordering::Relaxed);
        let cpu = CPU_USAGE.load(Ordering::Relaxed);
        let mem = MEM_USAGE.load(Ordering::Relaxed);
        let thermal_state = THERMAL_STATE.load(Ordering::Relaxed);

        let half_height = self.height / 2;
        let scale = self.width as f64 / DISPLAY_WIDTH as f64;

        // 1. 绘制第二列 (网速) - 最右列
        // 箭头左对齐，数值右对齐 — 表格效果
        let col_gap = (13.0 * scale).round() as i32;
        let speed_right = self.width - (4.0 * scale).round() as i32;
        let speed_left = speed_right - (76.0 * scale).round() as i32;
        let arrow_right = speed_left + self.arrow_width;

        // 填充画布背景为透明色键，并设置文字颜色。
        // SAFETY: self.hdc_mem、self.hbrush 均为已选入且有效的 GDI 句柄；rect 在栈上。
        unsafe {
            let _ = FillRect(self.hdc_mem, &rect, self.hbrush);
        }
        // SAFETY: self.hdc_mem 有效。
        unsafe {
            SetTextColor(self.hdc_mem, self.text_color);
        }

        // 上行箭头
        let mut rc_up_arrow = RECT {
            left: speed_left,
            top: 0,
            right: arrow_right,
            bottom: half_height,
        };
        let up_arrow = Self::wide(&mut self.buf, "\u{2191}");
        // SAFETY: hdc_mem 有效；rc_up_arrow 在栈上；up_arrow 是 NUL 结尾的字段缓冲区切片。
        unsafe {
            let _ = DrawTextW(
                self.hdc_mem,
                up_arrow,
                &mut rc_up_arrow,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
            );
        }

        // 上行数值
        let mut rc_up_val = RECT {
            left: arrow_right,
            top: 0,
            right: speed_right,
            bottom: half_height,
        };
        let up_val = Self::format_speed_wide(&mut self.buf, speed_up);
        // SAFETY: hdc_mem 有效；rc_up_val 在栈上；up_val 是 NUL 结尾的字段缓冲区切片。
        unsafe {
            let _ = DrawTextW(
                self.hdc_mem,
                up_val,
                &mut rc_up_val,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_RIGHT,
            );
        }

        // 下行箭头
        let mut rc_down_arrow = RECT {
            left: speed_left,
            top: half_height,
            right: arrow_right,
            bottom: self.height,
        };
        let down_arrow = Self::wide(&mut self.buf, "\u{2193}");
        // SAFETY: 同上。
        unsafe {
            let _ = DrawTextW(
                self.hdc_mem,
                down_arrow,
                &mut rc_down_arrow,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
            );
        }

        // 下行数值
        let mut rc_down_val = RECT {
            left: arrow_right,
            top: half_height,
            right: speed_right,
            bottom: self.height,
        };
        let down_val = Self::format_speed_wide(&mut self.buf, speed_down);
        // SAFETY: 同上。
        unsafe {
            let _ = DrawTextW(
                self.hdc_mem,
                down_val,
                &mut rc_down_val,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_RIGHT,
            );
        }

        // 2. 绘制第一列 (CPU & MEM) - 最左列
        let cpu_right = speed_left - col_gap;
        let cpu_left = cpu_right - (76.0 * scale).round() as i32;

        // 仅 CPU 行根据热风险状态变色，其余行保持默认色。
        let thermal_color = match thermal_state {
            2 => COLORREF(COLOR_HOT_TEXT),
            3 => COLORREF(COLOR_CRIT_TEXT),
            _ => self.text_color,
        };
        // SAFETY: hdc_mem 有效。
        unsafe {
            SetTextColor(self.hdc_mem, thermal_color);
        }

        let cpu_wide = Self::format_cpu_mem_wide(&mut self.buf, "CPU", cpu);
        let mut rc_cpu = RECT {
            left: cpu_left,
            top: 0,
            right: cpu_right,
            bottom: half_height,
        };
        // SAFETY: 同上；cpu_wide 是 NUL 结尾的字段缓冲区切片。
        unsafe {
            let _ = DrawTextW(
                self.hdc_mem,
                cpu_wide,
                &mut rc_cpu,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_RIGHT,
            );
        }

        // SAFETY: hdc_mem 有效。
        unsafe {
            SetTextColor(self.hdc_mem, self.text_color);
        }

        let h = self.height;
        let mem_wide = Self::format_cpu_mem_wide(&mut self.buf, "MEM", mem);
        let mut rc_mem = RECT {
            left: cpu_left,
            top: half_height,
            right: cpu_right,
            bottom: h,
        };
        // SAFETY: 同上。
        unsafe {
            let _ = DrawTextW(
                self.hdc_mem,
                mem_wide,
                &mut rc_mem,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_RIGHT,
            );
        }

        // 3. 把内存 DC 内容一次性 blit 到目标窗口 DC。
        // SAFETY: hdc 与 self.hdc_mem 均为有效 DC；坐标与尺寸基于 self.width / self.height，
        // 与位图选择时的尺寸一致。
        unsafe {
            let _ = BitBlt(
                hdc,
                0,
                0,
                self.width,
                self.height,
                Some(self.hdc_mem),
                0,
                0,
                SRCCOPY,
            );
        }
    }

    pub fn update_dpi(&mut self, hwnd: HWND) {
        // SAFETY: hwnd 是在当前进程上下文中有效且处于活动状态的窗口句柄，调用
        // GetDpiForWindow 是纯查询 API，无跨进程非法访问问题。
        let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
        let scale = dpi as f64 / 96.0;
        let width = (DISPLAY_WIDTH as f64 * scale).round() as i32;
        let height = (DISPLAY_HEIGHT as f64 * scale).round() as i32;
        let font_size = (FONT_BASE_SIZE as f64 * scale).round() as i32;

        // 1. 取得临时屏幕 DC。
        let Some(screen_dc) = ScreenDcGuard::acquire() else {
            return;
        };

        // 2. 创建新尺寸的兼容位图（必须用屏幕 DC）。失败时保持旧尺寸与旧位图。
        let Some(new_bitmap) = OwnedBitmap::new(screen_dc.hdc, width, height) else {
            return;
        };

        // 位图创建后不再需要屏幕 DC，提前释放。
        drop(screen_dc);

        // 3. 创建新尺寸的字体。失败时由 OwnedBitmap 守卫自动释放位图。
        let Some(new_font) = OwnedFont::new(font_size) else {
            return;
        };

        // 4. 新资源均已就绪：原子替换并向后清理旧对象，确保 BitBlt 源/目标尺寸一致。
        // SAFETY: self.hdc_mem 有效；new_bitmap 为刚创建的独占位图；SelectObject 返回的
        // old_bitmap 是此前选入并被替换的 self.hbitmap，已脱离 DC 可安全 DeleteObject。
        let old_bitmap = unsafe { SelectObject(self.hdc_mem, new_bitmap.hb.into()) };
        unsafe {
            let _ = DeleteObject(old_bitmap);
        }
        self.hbitmap = new_bitmap.hb;

        // SAFETY: self.hdc_mem 有效；new_font 为独占字体；old_font 是被替换的 self.hfont。
        let old_font = unsafe { SelectObject(self.hdc_mem, new_font.hf.into()) };
        unsafe {
            let _ = DeleteObject(old_font);
        }
        self.hfont = new_font.hf;

        self.font_size = font_size;
        self.width = width;
        self.height = height;

        // SAFETY: self.hdc_mem 有效。
        unsafe {
            let _ = SetBkMode(self.hdc_mem, TRANSPARENT);
        }

        self.arrow_width = measure_arrow_width(self.hdc_mem);

        // 移交独占资源，避免守卫 Drop 误销毁仍在使用的对象。
        std::mem::forget(new_bitmap);
        std::mem::forget(new_font);
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY:
        // 1. self.hdc_mem 是有效持有的内存设备上下文。还原最初选入上下文的 stock 默认
        //    位图 self.old_bitmap 与 stock 字体 self.old_font，避免 DeleteDC 因仍持有
        //    独占对象而拒绝释放。
        // 2. self.hfont、self.hbitmap、self.hbrush 均由本结构体独占，已被还原出 DC，
        //    可用 DeleteObject 安全归还系统图形资源。DeleteDC 最后释放 DC 本身。
        unsafe {
            let _ = SelectObject(self.hdc_mem, self.old_bitmap);
            let _ = SelectObject(self.hdc_mem, self.old_font);

            let _ = DeleteObject(self.hfont.into());
            let _ = DeleteObject(self.hbitmap.into());
            let _ = DeleteObject(self.hbrush.into());
            let _ = DeleteDC(self.hdc_mem);
        }
    }
}

fn measure_arrow_width(hdc: HDC) -> i32 {
    let arrow_text = to_wide("\u{2191} ");
    let mut size = SIZE::default();
    // SAFETY: hdc 有效；arrow_text 以 NUL 结尾；size 在栈上分配。
    unsafe {
        let _ = GetTextExtentPoint32W(hdc, &arrow_text[..arrow_text.len() - 1], &mut size);
    }
    size.cx
}

fn create_font(size: i32) -> HFONT {
    let mut lf = LOGFONTW {
        lfHeight: -size,
        lfWeight: 400,
        // NONANTIALIASED_QUALITY：避免 Layered 窗口上 GDI 半透明粉红毛边。
        lfQuality: FONT_QUALITY(3),
        ..Default::default()
    };
    let font_name = to_wide("Segoe UI");
    let copy_len = font_name.len().min(lf.lfFaceName.len());
    lf.lfFaceName[..copy_len].copy_from_slice(&font_name[..copy_len]);
    // SAFETY: lfFaceName 含尾 NUL；返回的 HFONT 由调用方独占释放。
    unsafe { CreateFontIndirectW(&lf) }
}

pub fn is_system_light_theme() -> bool {
    reg_read_dword(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize",
        "SystemUsesLightTheme",
    )
    .map(|v| v == 1)
    .unwrap_or(false)
}

fn push_ascii(buf: &mut Vec<u16>, s: &str) {
    for b in s.bytes() {
        buf.push(b as u16);
    }
}

fn write_u32(buf: &mut Vec<u16>, mut n: u32) {
    if n == 0 {
        buf.push(b'0' as u16);
        return;
    }
    let start = buf.len();
    while n > 0 {
        buf.push((b'0' + (n % 10) as u8) as u16);
        n /= 10;
    }
    buf[start..].reverse();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide_to_string(wide: &[u16]) -> String {
        String::from_utf16_lossy(wide.strip_suffix(&[0]).unwrap_or(wide))
    }

    #[test]
    fn test_format_speed_wide_boundaries() {
        let mut buf = Vec::with_capacity(32);

        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(&mut buf, 0)),
            "0 B/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(&mut buf, 512)),
            "512 B/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(&mut buf, 1023)),
            "1023 B/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(&mut buf, 1024)),
            "1.0 KB/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(&mut buf, 1024 * 1024 - 1)),
            "1024.0 KB/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(&mut buf, 1024 * 1024)),
            "1.0 MB/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(
                &mut buf,
                1024 * 1024 * 10 + 1024 * 512
            )),
            "10.5 MB/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(&mut buf, u32::MAX)),
            "4096.0 MB/s"
        );
    }

    // ===== write_u32 =====

    #[test]
    fn test_write_u32_zero() {
        let mut buf = Vec::new();
        write_u32(&mut buf, 0);
        assert_eq!(wide_to_string(&buf), "0");
    }

    #[test]
    fn test_write_u32_digit_boundaries() {
        let mut buf = Vec::new();
        // 1 位 → 2 位边界
        write_u32(&mut buf, 9);
        assert_eq!(wide_to_string(&buf), "9");
        buf.clear();
        write_u32(&mut buf, 10);
        assert_eq!(wide_to_string(&buf), "10");

        // 2 位 → 3 位边界
        buf.clear();
        write_u32(&mut buf, 99);
        assert_eq!(wide_to_string(&buf), "99");
        buf.clear();
        write_u32(&mut buf, 100);
        assert_eq!(wide_to_string(&buf), "100");
    }

    #[test]
    fn test_write_u32_max() {
        let mut buf = Vec::new();
        write_u32(&mut buf, u32::MAX);
        assert_eq!(wide_to_string(&buf), "4294967295");
    }

    // ===== push_ascii =====

    #[test]
    fn test_push_ascii_roundtrip() {
        let mut buf = Vec::new();
        push_ascii(&mut buf, "CPU: ");
        push_ascii(&mut buf, "100");
        push_ascii(&mut buf, "%");
        assert_eq!(wide_to_string(&buf), "CPU: 100%");
    }
}
