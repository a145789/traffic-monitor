use std::cell::{Cell, RefCell};
use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{COLORREF, HWND, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW, CreateSolidBrush,
    DRAW_TEXT_FORMAT, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteDC,
    DeleteObject, DrawTextW, FillRect, GetTextExtentPoint32W, GetWindowDC, HBITMAP, HBRUSH, HDC,
    HFONT, HGDIOBJ, InvalidateRect, LOGFONTW, NONANTIALIASED_QUALITY, ReleaseDC, SRCCOPY,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};

use crate::config::{
    COLOR_DARK_TEXT, COLOR_KEY, COLOR_LIGHT_TEXT, DISPLAY_HEIGHT, DISPLAY_WIDTH, FONT_BASE_SIZE,
    FONT_FACE_NAME, FONT_WEIGHT_NORMAL, LAYOUT_COL_GAP, LAYOUT_COL_WIDTH, LAYOUT_SPEED_MARGIN,
    REG_PATH_PERSONALIZE,
};
use crate::state::{CPU_USAGE, MEM_USAGE, NET_SPEED_DOWN, NET_SPEED_UP};
use crate::util::{copy_wide_truncated, diag, dpi_scaled, log_event, reg_read_dword, to_wide};

const ARROW_UP: [u16; 2] = [0x2191, 0];
const ARROW_DOWN: [u16; 2] = [0x2193, 0];

thread_local! {
    static RENDERER: RefCell<Option<Renderer>> = const { RefCell::new(None) };
    static LAST_RENDERED_VALUES: Cell<Option<DisplayValues>> = const { Cell::new(None) };
}

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
        let Ok(mut borrowed) = r.try_borrow_mut() else {
            log_event!("渲染器重入被跳过");
            return;
        };
        if let Some(renderer) = borrowed.as_mut() {
            f(renderer);
        }
    });
}

pub fn take_renderer() {
    RENDERER.with(|r| {
        let _ = r.borrow_mut().take();
    });
}

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
    width: i32,
    height: i32,
    arrow_width: i32,
    layout: Layout,
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
    /// 本值为跨 tick 拼接快照，禁止基于「快照一致」写更强的去重逻辑。
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

/// 把 GDI 对象选入 DC，返回被替换出的旧对象；失败返回 `Err(())`。
///
/// `SelectObject` 失败时返回 NULL 或 HGDI_ERROR（两者均满足 `is_invalid`），
/// 此时 DC 中的选中对象不变。调用方必须把已完成的交换回滚到本调用之前的
/// 状态（见 `swap_dpi_objects`）：失败本身只说得出「选入没成功」这一件事实，
/// 该换上什么上下文文案由调用方补足，故不带负载。
///
/// # Safety
/// `hdc` 与 `obj` 均须为有效句柄，且 `obj` 未被任何 DC 选中（新建独占对象）。
unsafe fn select_or_fail(hdc: HDC, obj: HGDIOBJ) -> Result<HGDIOBJ, ()> {
    // SAFETY: 前提由调用方保证；SelectObject 为同步调用，不保留参数指针。
    let old = unsafe { SelectObject(hdc, obj) };
    // HGDIOBJ::is_invalid 覆盖 NULL(0) 与 HGDI_ERROR(-1) 两种失败返回
    // （windows 0.62 Gdi/mod.rs:5410），这是本判别成立的关键前提。
    if old.is_invalid() { Err(()) } else { Ok(old) }
}

/// 把新位图与新字体依次换入内存 DC，返回换出的两个旧对象；两者都换成才算成功。
///
/// `Renderer::new` 的首次选入与 `Renderer::update_dpi` 的换新共用本函数，是「四处
/// 交换」唯一的判别与回滚实现：任一步失败都回滚已完成的交换——把刚换出的旧位图
/// 选回 DC（顺带把新位图挤回未选中状态）——使 DC 状态回到调用前，`self.hbitmap` /
/// `self.hfont`（启动期则是 stock 默认对象）无需改动即可继续成立，新对象也能被
/// 调用方的 `OwnedGdi` 守卫删掉。回滚方向不可调换：必须先让旧对象回到 DC，新对象
/// 才脱离 DC、`DeleteObject` 才可能生效。
///
/// # Safety
/// `hdc` 须为有效内存 DC；两个新对象须为有效独占对象（未被任何 DC 选中）。
unsafe fn swap_dpi_objects(
    hdc: HDC,
    new_bitmap: HGDIOBJ,
    new_font: HGDIOBJ,
) -> Result<(HGDIOBJ, HGDIOBJ), &'static str> {
    let old_bitmap = match unsafe { select_or_fail(hdc, new_bitmap) } {
        Ok(old) => old,
        Err(()) => return Err("选入位图失败"),
    };

    let old_font = match unsafe { select_or_fail(hdc, new_font) } {
        Ok(old) => old,
        Err(()) => {
            // 回滚：把刚换出的旧位图选回 DC（尚未删除、仍有效），新位图随调用方的
            // OwnedGdi 守卫删除；DC 回到调用前的状态。
            // SAFETY: old_bitmap 是刚换出、尚未删除的有效对象。
            // 注意：回滚与被回滚的交换共用同一 hdc，回滚失败等价于本进程不再持有
            // 可信 DC 状态（此时 DC 里选中的是 new_bitmap，而字段仍指向 old_bitmap
            // ——这正是本函数要消灭的分叉），故只留痕不掩盖。
            // 判别标准与 select_or_fail 相同：is_invalid 覆盖 NULL 与 HGDI_ERROR。
            unsafe {
                if SelectObject(hdc, old_bitmap).is_invalid() {
                    diag!("对象交换失败: 回滚位图交换失败，DC 状态不可信");
                }
            }
            return Err("选入字体失败");
        }
    };

    Ok((old_bitmap, old_font))
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
    pub fn new() -> Result<Self, String> {
        let screen_dc = ScreenDcGuard::acquire().ok_or("无法获取屏幕设备上下文".to_string())?;

        // SAFETY: screen_dc.hdc 有效。
        let dc = OwnedGdi::new(unsafe { CreateCompatibleDC(Some(screen_dc.hdc)) })
            .ok_or("无法创建兼容内存 DC".to_string())?;

        // 3. 兼容位图。必须使用屏幕 DC 而非内存 DC，以匹配屏幕颜色格式。
        // SAFETY: screen_dc.hdc 有效；尺寸为正常量。
        let bitmap = OwnedGdi::new(unsafe {
            CreateCompatibleBitmap(screen_dc.hdc, DISPLAY_WIDTH, DISPLAY_HEIGHT)
        })
        .ok_or("无法创建兼容位图".to_string())?;

        let font = OwnedGdi::new(create_font(FONT_BASE_SIZE)).ok_or("无法创建字体".to_string())?;

        let brush = OwnedGdi::new(unsafe { CreateSolidBrush(COLORREF(COLOR_KEY)) })
            .ok_or("无法创建背景刷子".to_string())?;

        // ── 资源创建至此全部成功；选入仍可能失败（见 `select_or_fail`），故与
        //    `update_dpi` 共用 `swap_dpi_objects` 的回滚序，失败即整体早退。──

        // 6. 选入位图与字体，备份被替换出的 stock 默认对象（1x1 位图 / 系统字体）。
        // SAFETY: dc.0 为刚创建的有效内存 DC；bitmap.0 / font.0 为刚创建的有效独占对象。
        let (old_bitmap, old_font) =
            match unsafe { swap_dpi_objects(dc.0, bitmap.0.into(), font.0.into()) } {
                Ok(olds) => olds,
                // 失败时 DC 已被回滚到 stock 默认对象，两个新对象随各自 OwnedGdi
                // Drop 删除，DC 仍可安全 DeleteDC。
                Err(e) => return Err(e.to_string()),
            };

        // 7. 设置背景模式为透明，便于 DrawTextW 与位图 blit 保留透明色键。
        // SAFETY: dc.0 有效。
        unsafe {
            let _ = SetBkMode(dc.0, TRANSPARENT);
        }

        let arrow_width = measure_arrow_width(dc.0);

        drop(screen_dc);

        Ok(Self {
            hdc_mem: dc.into_raw(),
            hbitmap: bitmap.into_raw(),
            hfont: font.into_raw(),
            old_bitmap,
            old_font,
            hbrush: brush.into_raw(),
            text_color: COLORREF(COLOR_LIGHT_TEXT),
            width: DISPLAY_WIDTH,
            height: DISPLAY_HEIGHT,
            arrow_width,
            layout: Layout::new(DISPLAY_WIDTH, DISPLAY_HEIGHT),
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

    fn format_cpu_mem_wide<'a>(buf: &'a mut Vec<u16>, label: &str, value: u32) -> &'a mut [u16] {
        buf.clear();
        buf.extend(label.encode_utf16());
        buf.extend(": ".encode_utf16());
        write_u32(buf, value);
        buf.extend("%".encode_utf16());
        buf.push(0);
        buf
    }

    fn format_speed_wide(buf: &mut Vec<u16>, bytes_per_sec: u32) -> &mut [u16] {
        buf.clear();
        if bytes_per_sec < 1024 {
            write_u32(buf, bytes_per_sec);
            buf.extend(" B/s".encode_utf16());
        } else {
            // 先算 KB 十分位定点值；满 1024.0 KB/s（x >= 10240，十分位即 1.0 MB/s）
            // 落入 MB 分支按 MB 重新舍入，避免两条分支各自重复格式化。
            let mut x = ((bytes_per_sec as u64 * 10 + 512) / 1024) as u32;
            let unit: &str = if x >= 10240 {
                x = ((bytes_per_sec as u64 * 10 + 524288) / (1024 * 1024)) as u32;
                " MB/s"
            } else {
                " KB/s"
            };
            write_u32(buf, x / 10);
            buf.push(b'.' as u16);
            buf.push((b'0' + (x % 10) as u8) as u16);
            buf.extend(unit.encode_utf16());
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

        let layout = self.layout;
        let arrow_right = layout.speed_left + self.arrow_width;

        // 填充画布背景为透明色键，并设置文字颜色。
        // SAFETY: self.hdc_mem 为本结构体独占的内存 DC，self.hbrush 为本结构体独占的刷子，
        // 两者在调用期间均存活；rect 在栈上且调用期间有效；FillRect 为同步调用，不保留指针。
        unsafe {
            let _ = FillRect(self.hdc_mem, &rect, self.hbrush);
        }
        // SAFETY: self.hdc_mem 有效。
        unsafe {
            SetTextColor(self.hdc_mem, self.text_color);
        }

        let mut rc_up_arrow = RECT {
            left: layout.speed_left,
            top: 0,
            right: arrow_right,
            bottom: layout.half_height,
        };
        let mut up_arrow = ARROW_UP;
        draw_text(self.hdc_mem, &mut up_arrow, &mut rc_up_arrow, DT_LEFT);

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
        let mut down_arrow = ARROW_DOWN;
        draw_text(self.hdc_mem, &mut down_arrow, &mut rc_down_arrow, DT_LEFT);

        let mut rc_down_val = RECT {
            left: arrow_right,
            top: layout.half_height,
            right: layout.speed_right,
            bottom: self.height,
        };
        let down_val = Self::format_speed_wide(&mut self.buf, values.speed_down);
        draw_text(self.hdc_mem, down_val, &mut rc_down_val, DT_RIGHT);

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
        // SAFETY: hdc 为调用方在本次同步调用期间有效的目标 DC，self.hdc_mem 为本结构体独占的内存 DC；
        // 坐标与尺寸基于 self.width / self.height，与当前选入位图一致；BitBlt 不保留指针。
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

    /// 按窗口当前 DPI 重建位图/字体并缓存新布局。返回 false 表示位图/字体创建失败，
    /// 或新对象选入内存 DC 失败（此时已回滚到调用前的 DC 状态），两种情况都维持旧
    /// 尺寸不变（调用方须把窗口回滚到 [`bitmap_size`]，否则「窗口新尺寸 + 位图旧尺寸」
    /// 会让 BitBlt 只覆盖旧位图区域、露出色键底色）。
    pub fn update_dpi(&mut self, hwnd: HWND) -> bool {
        // SAFETY: hwnd 是在当前进程上下文中有效且处于活动状态的窗口句柄，调用
        // GetDpiForWindow 是纯查询 API，无跨进程非法访问问题。
        let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
        let width = dpi_scaled(DISPLAY_WIDTH, dpi);
        let height = dpi_scaled(DISPLAY_HEIGHT, dpi);
        let font_size = dpi_scaled(FONT_BASE_SIZE, dpi);

        let Some(screen_dc) = ScreenDcGuard::acquire() else {
            diag!("DPI 更新失败: 无法获取屏幕 DC");
            return false;
        };

        // SAFETY: screen_dc.hdc 有效。
        let Some(new_bitmap) =
            OwnedGdi::new(unsafe { CreateCompatibleBitmap(screen_dc.hdc, width, height) })
        else {
            diag!("DPI 更新失败: 无法创建 {width}x{height} 兼容位图");
            return false;
        };

        drop(screen_dc);

        let Some(new_font) = OwnedGdi::new(create_font(font_size)) else {
            diag!("DPI 更新失败: 无法创建字号 {font_size} 字体");
            return false;
        };

        // 4. 新资源均已就绪：两次交换都成功才删除旧对象并提交新状态；任一步失败则
        //    回滚已完成的交换，保证 DC 状态与 self 记录始终一致（见 swap_dpi_objects）。
        // SAFETY: self.hdc_mem 有效；new_bitmap/new_font 为刚创建的独占对象；
        // swap_dpi_objects 验证了两次交换都成功，故 old_bitmap/old_font 是已脱离 DC 的
        // self.hbitmap/self.hfont，可安全 DeleteObject。
        let (old_bitmap, old_font) =
            match unsafe { swap_dpi_objects(self.hdc_mem, new_bitmap.0.into(), new_font.0.into()) }
            {
                Ok(old) => old,
                Err(e) => {
                    diag!("DPI 更新失败: {e}");
                    return false;
                }
            };

        unsafe {
            let _ = DeleteObject(old_bitmap);
            let _ = DeleteObject(old_font);
        }
        self.hbitmap = new_bitmap.into_raw();
        self.hfont = new_font.into_raw();

        self.width = width;
        self.height = height;
        self.layout = Layout::new(width, height);

        // hdc_mem 常驻：背景模式只需 new() 设一次，此处仅换位图/字体。
        self.arrow_width = measure_arrow_width(self.hdc_mem);
        true
    }

    /// 当前位图尺寸（物理像素）。DPI 资源重建失败时窗口须回滚到该尺寸，
    /// 保证 BitBlt 源（位图）与目标（窗口）一致。
    pub fn bitmap_size(&self) -> (i32, i32) {
        (self.width, self.height)
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
///
/// `width` 必须取已舍入的实际宽度（`dpi_scaled(DISPLAY_WIDTH, dpi)` 的输出），
/// 禁止经整数 DPI 中转二次舍入（96–384 实测 77 处差一像素）；改任一侧舍入
/// 策略都要重新全范围对账。纯值类型：随 DPI 更新整体重算并缓存（`Renderer::layout`），
/// 渲染热路径零分配纪律不变。
#[derive(Clone, Copy)]
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
        lfWeight: FONT_WEIGHT_NORMAL,
        // NONANTIALIASED_QUALITY：避免 Layered 窗口上 GDI 半透明粉红毛边。
        lfQuality: NONANTIALIASED_QUALITY,
        ..Default::default()
    };
    let font_name = to_wide(FONT_FACE_NAME);
    copy_wide_truncated(&mut lf.lfFaceName, &font_name);
    // SAFETY: lfFaceName 经上式截断后必含尾 NUL；返回的 HFONT 由调用方独占释放。
    unsafe { CreateFontIndirectW(&lf) }
}

fn is_system_light_theme() -> bool {
    reg_read_dword(REG_PATH_PERSONALIZE, "SystemUsesLightTheme")
        .map(|v| v == 1)
        .unwrap_or(false)
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
    use windows::Win32::Graphics::Gdi::{GetCurrentObject, OBJ_BITMAP};
    use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

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
            wide_to_string(Renderer::format_speed_wide(&mut buf, 1024 * 1024 - 102)),
            "1023.9 KB/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(&mut buf, 1024 * 1024 - 1)),
            "1.0 MB/s"
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

    #[test]
    fn test_write_u32() {
        for (input, expected) in [
            (0, "0"),
            (9, "9"),
            (10, "10"),
            (99, "99"),
            (100, "100"),
            (u32::MAX, "4294967295"),
        ] {
            let mut buf = Vec::new();
            write_u32(&mut buf, input);
            assert_eq!(wide_to_string(&buf), expected);
        }
    }

    #[test]
    fn test_dpi_swap_rejects_failed_select() {
        // 以失效 DC 触发交换失败：update_dpi 必须返回 false 且位图尺寸不变。
        let hwnd = unsafe { GetDesktopWindow() };
        let mut renderer = Renderer::new().expect("测试环境 GDI 不可用");
        // 前置自证：用例的 false 必须来自「选入判别」，而不是更早的创建步骤。`Renderer::new`
        // 成功已证明屏幕 DC、兼容位图与字体创建在本环境可用，而 `update_dpi` 的创建阶段
        // 用的是同一批原语（位图取自 screen_dc，不是 hdc_mem），故创建阶段不会提前失败。
        // 显式断言这一前提，避免在拿不到屏幕 DC 的环境里用例静默退化成空转（两条断言
        // 照样成立，却什么都没测）。
        assert!(
            ScreenDcGuard::acquire().is_some(),
            "测试环境拿不到屏幕 DC，本用例会退化为空转"
        );
        let size_before = renderer.bitmap_size();
        let real_dc = renderer.hdc_mem;
        // 置无效 DC：SelectObject 对无效 hdc 返回 NULL/HGDI_ERROR。
        renderer.hdc_mem = HDC::default();
        let updated = renderer.update_dpi(hwnd);
        let size_after = renderer.bitmap_size();
        // 先还原真实 DC 再断言：这样无论断言是否失败，Drop 都在真实 DC 上跑完释放序
        // （还原 stock → 删独占对象 → DeleteDC），不会把释放序留在无效 DC 上。
        renderer.hdc_mem = real_dc;
        assert!(!updated);
        assert_eq!(size_after, size_before);

        // 失败出口的核心不变量：DC 里选中的位图仍与 self 记录一致，不得分叉。
        // 判别 + 回滚的承重承诺就是这一条，单独断言它而不只看返回值。
        // SAFETY: real_dc 是本测试持有且有效的内存 DC，GetCurrentObject 为查询型调用。
        let selected_bitmap = unsafe { GetCurrentObject(real_dc, OBJ_BITMAP) };
        assert_eq!(selected_bitmap.0, renderer.hbitmap.0);
    }

    #[test]
    fn test_dpi_swap_rolls_back_bitmap_when_second_select_fails() {
        // 直击本 PR 新增的「第二次交换失败 → 回滚」分支：位图换入成功、第二次交换失败，
        // 必须把旧位图选回 DC，不留半交换状态。
        // 构造手段：第一个对象给有效位图，第二个给已被 DeleteObject 的失效句柄——
        // SelectObject 对无效对象返回 NULL/HGDI_ERROR，于是第二次交换失败。
        let dc = OwnedGdi::new(unsafe { CreateCompatibleDC(None) }).expect("测试环境 GDI 不可用");
        let old_bitmap = OwnedGdi::new(unsafe { CreateCompatibleBitmap(dc.0, 1, 1) })
            .expect("测试环境 GDI 不可用");
        let new_bitmap = OwnedGdi::new(unsafe { CreateCompatibleBitmap(dc.0, 4, 4) })
            .expect("测试环境 GDI 不可用");

        // 基线：先把 old_bitmap 换入 DC，本函数须把它换出、再回滚换回。
        // SAFETY: dc.0 有效，old_bitmap 为有效独占对象。
        let stock = unsafe { SelectObject(dc.0, old_bitmap.0.into()) };
        assert!(!stock.is_invalid(), "基线换入应成功");

        // 制造失效句柄：创建后立即删除，得到一个「曾经有效」但已失效的句柄值。
        let dead_raw = OwnedGdi::new(unsafe { CreateCompatibleBitmap(dc.0, 1, 1) })
            .expect("测试环境 GDI 不可用")
            .into_raw();
        unsafe {
            let _ = DeleteObject(dead_raw.into());
        }

        // SAFETY: dc.0 有效；new_bitmap 为有效独占对象；dead_raw 是刚被删除的失效句柄，
        // 正是本用例要注入的失败源。
        let result = unsafe { swap_dpi_objects(dc.0, new_bitmap.0.into(), dead_raw.into()) };
        assert!(result.is_err(), "失效句柄必须让第二次交换失败");

        // 回滚后 DC 仍选中 old_bitmap —— 这正是「DC 状态不得与 self 记录分叉」的具体形态。
        // SAFETY: dc.0 有效，GetCurrentObject 为查询型调用。
        let selected = unsafe { GetCurrentObject(dc.0, OBJ_BITMAP) };
        assert_eq!(selected.0, old_bitmap.0.0);

        // 收尾：把 stock 选回 DC 让 old_bitmap 脱离 DC，OwnedGdi 的 Drop 才删得掉。
        // SAFETY: dc.0 有效；stock 是上面换出的原选中对象，仍然有效。
        assert!(!unsafe { SelectObject(dc.0, stock) }.is_invalid());
    }
}
