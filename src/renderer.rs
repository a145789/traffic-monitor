use std::cell::{Cell, RefCell};
use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{COLORREF, HWND, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW, CreateSolidBrush,
    DRAW_TEXT_FORMAT, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteDC,
    DeleteObject, DrawTextW, FONT_QUALITY, FillRect, GetTextExtentPoint32W, GetWindowDC, HBITMAP,
    HBRUSH, HDC, HFONT, HGDIOBJ, InvalidateRect, LOGFONTW, ReleaseDC, SRCCOPY, SelectObject,
    SetBkMode, SetTextColor, TRANSPARENT,
};

use crate::config::{
    COLOR_DARK_TEXT, COLOR_KEY, COLOR_LIGHT_TEXT, DISPLAY_HEIGHT, DISPLAY_WIDTH, FONT_BASE_SIZE,
    LAYOUT_COL_GAP, LAYOUT_COL_WIDTH, LAYOUT_SPEED_MARGIN, REG_PATH_PERSONALIZE,
};
use crate::state::{CPU_USAGE, MEM_USAGE, NET_SPEED_DOWN, NET_SPEED_UP};
use crate::util::{push_wide, reg_read_dword, to_wide};

thread_local! {
    static RENDERER: RefCell<Option<Renderer>> = const { RefCell::new(None) };
    static LAST_RENDERED_VALUES: Cell<Option<DisplayValues>> = const { Cell::new(None) };
}

/// 安装渲染器（启动时调用一次）。
pub fn set_renderer(renderer: Renderer) {
    RENDERER.with(|r| *r.borrow_mut() = Some(renderer));
}

/// 在 UI 线程上访问渲染器；未初始化时静默跳过。
///
/// 重入安全：闭包执行期间持有 `RefCell` 可变借用，闭包内（及其调用链）禁止
/// 再次调用本函数。此处刻意用 `try_borrow_mut` 使重入退化为跳过而非 panic——
/// release 构建为 `panic = "abort"`，`borrow_mut` 双重借用会直接中止进程。
pub fn with_renderer(f: impl FnOnce(&mut Renderer)) {
    RENDERER.with(|r| {
        if let Ok(mut borrowed) = r.try_borrow_mut()
            && let Some(renderer) = borrowed.as_mut()
        {
            f(renderer);
        }
    });
}

/// 销毁渲染器（退出前调用，归还 GDI 资源）。
pub fn take_renderer() {
    RENDERER.with(|r| {
        let _ = r.borrow_mut().take();
    });
}

/// 仅在展示数据变化后请求重绘，避免空闲时每秒执行完整 GDI 绘制。
pub fn invalidate_if_values_changed(hwnd: HWND) {
    let values = DisplayValues::load();
    let changed = LAST_RENDERED_VALUES.with(|last| last.get() != Some(values));
    if changed {
        // SAFETY: hwnd 是当前 UI 线程拥有的主窗口句柄；InvalidateRect 只投递重绘请求。
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

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

#[derive(Clone, Copy, PartialEq, Eq)]
struct DisplayValues {
    speed_up: u32,
    speed_down: u32,
    cpu: u32,
    mem: u32,
}

impl DisplayValues {
    fn load() -> Self {
        Self {
            speed_up: NET_SPEED_UP.load(Ordering::Relaxed),
            speed_down: NET_SPEED_DOWN.load(Ordering::Relaxed),
            cpu: CPU_USAGE.load(Ordering::Relaxed),
            mem: MEM_USAGE.load(Ordering::Relaxed),
        }
    }
}

// ===== 模块私有 RAII 资源守卫 =====
//
// 构造 `Renderer` 时所有 GDI 对象按依赖顺序创建，任何一步失败由局部守卫的
// Drop 自动清理已申请的资源。成功路径通过 `into_raw` 把所有权移交给
// `Renderer`，由 `Renderer::drop` 负责“还原默认对象 → 销毁独占对象 → 释放 DC”
// 的标准释放序。

/// 可由 `OwnedGdi` 统一托管的 GDI 独占句柄。
///
/// 实现约定：`destroy` 必须与句柄的创建 API 配对；`OwnedGdi` 保证只调用一次。
trait GdiHandle: Copy {
    fn is_valid(&self) -> bool;
    fn destroy(self);
}

/// GDI 独占对象守卫：创建失败返回 None；Drop 时销毁；`into_raw` 移交所有权。
struct OwnedGdi<T: GdiHandle>(T);

impl<T: GdiHandle> OwnedGdi<T> {
    fn new(handle: T) -> Option<Self> {
        if handle.is_valid() {
            Some(Self(handle))
        } else {
            None
        }
    }

    fn into_raw(self) -> T {
        let raw = self.0;
        std::mem::forget(self);
        raw
    }
}

impl<T: GdiHandle> Drop for OwnedGdi<T> {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

impl GdiHandle for HDC {
    fn is_valid(&self) -> bool {
        !self.is_invalid()
    }
    fn destroy(self) {
        // SAFETY: 句柄由 CreateCompatibleDC 创建且被独占；构造失败的早期路径上
        // DC 中尚未选入独占对象，DeleteDC 是配对释放。
        unsafe {
            let _ = DeleteDC(self);
        }
    }
}

impl GdiHandle for HBITMAP {
    fn is_valid(&self) -> bool {
        !self.is_invalid()
    }
    fn destroy(self) {
        // SAFETY: 句柄由 CreateCompatibleBitmap 创建且被独占。
        unsafe {
            let _ = DeleteObject(self.into());
        }
    }
}

impl GdiHandle for HFONT {
    fn is_valid(&self) -> bool {
        !self.is_invalid()
    }
    fn destroy(self) {
        // SAFETY: 句柄由 CreateFontIndirectW 创建且被独占。
        unsafe {
            let _ = DeleteObject(self.into());
        }
    }
}

impl GdiHandle for HBRUSH {
    fn is_valid(&self) -> bool {
        !self.is_invalid()
    }
    fn destroy(self) {
        // SAFETY: 句柄由 CreateSolidBrush 创建且被独占。
        unsafe {
            let _ = DeleteObject(self.into());
        }
    }
}

/// 临时屏幕 DC 守卫：构造时通过 `GetWindowDC(null)` 获取，`Drop` 时 `ReleaseDC`。
/// 与 `OwnedGdi` 分离：它是借来的 DC，配对释放 API 是 `ReleaseDC` 而非 `DeleteDC`。
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

impl Renderer {
    /// 事务式构造：所有 GDI 资源按依赖顺序创建，任何一步失败由局部 RAII 守卫
    /// 自动清理；全部成功后选入对象并通过 `into_raw` 移交所有权。错误信息为中文。
    pub fn new() -> Result<Self, String> {
        // 1. 临时屏幕 DC（Drop 时 ReleaseDC）。
        let screen_dc = ScreenDcGuard::acquire().ok_or("无法获取屏幕设备上下文".to_string())?;

        // 2. 兼容内存 DC（守卫托管，失败时 DeleteDC）。
        // SAFETY: screen_dc.hdc 有效。
        let dc = OwnedGdi::new(unsafe { CreateCompatibleDC(Some(screen_dc.hdc)) })
            .ok_or("无法创建兼容内存 DC".to_string())?;

        // 3. 兼容位图。必须使用屏幕 DC 而非内存 DC，以匹配屏幕颜色格式。
        // SAFETY: screen_dc.hdc 有效；尺寸为正常量。
        let bitmap = OwnedGdi::new(unsafe {
            CreateCompatibleBitmap(screen_dc.hdc, DISPLAY_WIDTH, DISPLAY_HEIGHT)
        })
        .ok_or("无法创建兼容位图".to_string())?;

        // 4. 字体。
        let font = OwnedGdi::new(create_font(FONT_BASE_SIZE)).ok_or("无法创建字体".to_string())?;

        // 5. 背景刷子。
        // SAFETY: COLOR_KEY 是合法的 COLORREF 常量。
        let brush = OwnedGdi::new(unsafe { CreateSolidBrush(COLORREF(COLOR_KEY)) })
            .ok_or("无法创建背景刷子".to_string())?;

        // ── 至此所有可失败步骤均已成功。后续选入/测量/配置均不会失败。──

        // 6. 选入位图并备份原默认位图（stock 1x1 位图）。
        // SAFETY: dc.0 与 bitmap.0 均为刚创建的有效独占句柄。
        let old_bitmap = unsafe { SelectObject(dc.0, bitmap.0.into()) };

        // 7. 选入字体并备份原默认字体（stock 系统字体）。
        // SAFETY: dc.0 与 font.0 均为有效独占句柄。
        let old_font = unsafe { SelectObject(dc.0, font.0.into()) };

        // 8. 设置背景模式为透明，便于 DrawTextW 与位图 blit 保留透明色键。
        // SAFETY: dc.0 有效。
        unsafe {
            let _ = SetBkMode(dc.0, TRANSPARENT);
        }

        // 9. 测量箭头宽度（依赖已选入的字体）。
        let arrow_width = measure_arrow_width(dc.0);

        // 10. 释放临时屏幕 DC（其使命仅限于提供创建上下文）。
        drop(screen_dc);

        // 11. 移交独占资源所有权，杜绝守卫 Drop 误释放仍在使用的对象。
        Ok(Self {
            hdc_mem: dc.into_raw(),
            hbitmap: bitmap.into_raw(),
            hfont: font.into_raw(),
            old_bitmap,
            old_font,
            hbrush: brush.into_raw(),
            text_color: COLORREF(COLOR_LIGHT_TEXT),
            font_size: FONT_BASE_SIZE,
            width: DISPLAY_WIDTH,
            height: DISPLAY_HEIGHT,
            arrow_width,
            buf: Vec::with_capacity(32),
        })
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
        push_wide(buf, s);
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

        let values = DisplayValues::load();

        let layout = Layout::new(self.width, self.height);
        let arrow_right = layout.speed_left + self.arrow_width;

        // 填充画布背景为透明色键，并设置文字颜色。
        // SAFETY: self.hdc_mem、self.hbrush 均为有效 GDI 句柄；rect 在栈上。
        unsafe {
            let _ = FillRect(self.hdc_mem, &rect, self.hbrush);
        }
        // SAFETY: self.hdc_mem 有效。
        unsafe {
            SetTextColor(self.hdc_mem, self.text_color);
        }

        // 网速列（右列）：箭头左对齐，数值右对齐 — 表格效果。
        let mut rc_up_arrow = RECT {
            left: layout.speed_left,
            top: 0,
            right: arrow_right,
            bottom: layout.half_height,
        };
        let up_arrow = Self::wide(&mut self.buf, "\u{2191}");
        draw_text(self.hdc_mem, up_arrow, &mut rc_up_arrow, DT_LEFT);

        let mut rc_up_val = RECT {
            left: arrow_right,
            top: 0,
            right: layout.speed_right,
            bottom: layout.half_height,
        };
        let up_val = Self::format_speed_wide(&mut self.buf, values.speed_up);
        draw_text(self.hdc_mem, up_val, &mut rc_up_val, DT_RIGHT);

        let mut rc_down_arrow = RECT {
            left: layout.speed_left,
            top: layout.half_height,
            right: arrow_right,
            bottom: self.height,
        };
        let down_arrow = Self::wide(&mut self.buf, "\u{2193}");
        draw_text(self.hdc_mem, down_arrow, &mut rc_down_arrow, DT_LEFT);

        let mut rc_down_val = RECT {
            left: arrow_right,
            top: layout.half_height,
            right: layout.speed_right,
            bottom: self.height,
        };
        let down_val = Self::format_speed_wide(&mut self.buf, values.speed_down);
        draw_text(self.hdc_mem, down_val, &mut rc_down_val, DT_RIGHT);

        // CPU/内存列（左列）：数值右对齐。
        let cpu_wide = Self::format_cpu_mem_wide(&mut self.buf, "CPU", values.cpu);
        let mut rc_cpu = RECT {
            left: layout.cpu_left,
            top: 0,
            right: layout.cpu_right,
            bottom: layout.half_height,
        };
        draw_text(self.hdc_mem, cpu_wide, &mut rc_cpu, DT_RIGHT);

        let mem_wide = Self::format_cpu_mem_wide(&mut self.buf, "MEM", values.mem);
        let mut rc_mem = RECT {
            left: layout.cpu_left,
            top: layout.half_height,
            right: layout.cpu_right,
            bottom: self.height,
        };
        draw_text(self.hdc_mem, mem_wide, &mut rc_mem, DT_RIGHT);

        // 把内存 DC 内容一次性 blit 到目标窗口 DC。
        // SAFETY: hdc 与 self.hdc_mem 均为有效 DC；坐标与尺寸基于 self.width / self.height，
        // 与位图选择时的尺寸一致。
        let copied = unsafe {
            BitBlt(
                hdc,
                0,
                0,
                self.width,
                self.height,
                Some(self.hdc_mem),
                0,
                0,
                SRCCOPY,
            )
            .is_ok()
        };
        if copied {
            LAST_RENDERED_VALUES.with(|last| last.set(Some(values)));
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
        // SAFETY: screen_dc.hdc 有效。
        let Some(new_bitmap) =
            OwnedGdi::new(unsafe { CreateCompatibleBitmap(screen_dc.hdc, width, height) })
        else {
            return;
        };

        // 位图创建后不再需要屏幕 DC，提前释放。
        drop(screen_dc);

        // 3. 创建新尺寸的字体。失败时由守卫自动释放位图。
        let Some(new_font) = OwnedGdi::new(create_font(font_size)) else {
            return;
        };

        // 4. 新资源均已就绪：原子替换并向后清理旧对象，确保 BitBlt 源/目标尺寸一致。
        // SAFETY: self.hdc_mem 有效；new_bitmap 为刚创建的独占位图；SelectObject 返回的
        // old_bitmap 是此前选入并被替换的 self.hbitmap，已脱离 DC 可安全 DeleteObject。
        let old_bitmap = unsafe { SelectObject(self.hdc_mem, new_bitmap.0.into()) };
        unsafe {
            let _ = DeleteObject(old_bitmap);
        }
        self.hbitmap = new_bitmap.into_raw();

        // SAFETY: self.hdc_mem 有效；new_font 为独占字体；old_font 是被替换的 self.hfont。
        let old_font = unsafe { SelectObject(self.hdc_mem, new_font.0.into()) };
        unsafe {
            let _ = DeleteObject(old_font);
        }
        self.hfont = new_font.into_raw();

        self.font_size = font_size;
        self.width = width;
        self.height = height;

        // SAFETY: self.hdc_mem 有效。
        unsafe {
            let _ = SetBkMode(self.hdc_mem, TRANSPARENT);
        }

        self.arrow_width = measure_arrow_width(self.hdc_mem);
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

/// 双列布局（物理像素），随窗口宽度按 96-DPI 基准缩放。
struct Layout {
    speed_left: i32,
    speed_right: i32,
    cpu_left: i32,
    cpu_right: i32,
    half_height: i32,
}

impl Layout {
    fn new(width: i32, height: i32) -> Self {
        let scale = width as f64 / DISPLAY_WIDTH as f64;
        let speed_right = width - (LAYOUT_SPEED_MARGIN as f64 * scale).round() as i32;
        let speed_left = speed_right - (LAYOUT_COL_WIDTH as f64 * scale).round() as i32;
        let cpu_right = speed_left - (LAYOUT_COL_GAP as f64 * scale).round() as i32;
        let cpu_left = cpu_right - (LAYOUT_COL_WIDTH as f64 * scale).round() as i32;
        Self {
            speed_left,
            speed_right,
            cpu_left,
            cpu_right,
            half_height: height / 2,
        }
    }
}

/// 向内存 DC 绘制单行文本（垂直居中、单行、不解析前缀符）。
fn draw_text(hdc: HDC, text: &mut [u16], rect: &mut RECT, align: DRAW_TEXT_FORMAT) {
    // SAFETY: hdc 有效；rect 在栈上；text 为 NUL 结尾的 UTF-16 缓冲区。
    unsafe {
        let _ = DrawTextW(
            hdc,
            text,
            rect,
            DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | align,
        );
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
    reg_read_dword(REG_PATH_PERSONALIZE, "SystemUsesLightTheme")
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
