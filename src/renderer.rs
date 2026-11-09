use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
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
    FONT_FACE_NAME, FONT_WEIGHT_NORMAL, LAYOUT_ARROW_GAP, LAYOUT_LEFT_MARGIN, LAYOUT_SPEED_MARGIN,
    LAYOUT_VALUE_GAP, REG_PATH_PERSONALIZE,
};
use crate::state::{CPU_USAGE, MEM_USAGE, NET_SPEED_DOWN, NET_SPEED_UP};
use crate::util::{copy_wide_truncated, diag, dpi_scaled, log_event, reg_read_dword, to_wide};

const ARROW_UP: [u16; 2] = [0x2191, 0];
const ARROW_DOWN: [u16; 2] = [0x2193, 0];

const KIB: u64 = 1024;
const MIB: u64 = KIB * 1024;
const GIB: u64 = MIB * 1024;

/// 网速列宽度按**最宽可能文本**预留，不再用固定像素列宽：网速列宽 = 箭头列 + 这几个
/// 档位边界输入现场格式化后的最大字宽，既不会被右对齐裁掉高位，也压不到箭头列。
///
/// `format_speed_wide` 每个档位只有一个最宽形态（值的上界是档位上限，无小数位），
/// 所以取各档上界的格式化结果即文本上界。探针只给**输入**，文本形态由格式化器
/// 唯一产出——改档位、单位或量级都不需要同步这里。`speed_column_fits_every_tier`
/// 用真实格式化结果复验布局合身。
const SPEED_WIDTH_SAMPLES: [u32; 4] = [1023, (MIB - 1) as u32, (GIB - 1) as u32, u32::MAX];

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
    layout: Layout,
    buf: Vec<u16>,
    /// 十进制文本的复用缓冲区，与 `buf` 同属渲染器：每帧四次格式化都在此摊还分配，
    /// 不得改成每次新建（渲染热路径零分配纪律）。
    scratch: String,
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

        let layout = Layout::measure(dc.0, DISPLAY_WIDTH, DISPLAY_HEIGHT);

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
            layout,
            buf: Vec::with_capacity(32),
            scratch: String::with_capacity(8),
        })
    }

    pub fn update_text_color(&mut self) {
        if is_system_light_theme() {
            self.text_color = COLORREF(COLOR_DARK_TEXT);
        } else {
            self.text_color = COLORREF(COLOR_LIGHT_TEXT);
        }
    }

    /// 标签段（`CPU:` / `MEM:`）：静态文本，左对齐绘制，两行共用同一右缘（冒号同列）。
    fn format_label_wide<'a>(buf: &'a mut Vec<u16>, label: &str) -> &'a mut [u16] {
        buf.clear();
        buf.extend(label.encode_utf16());
        buf.push(b':' as u16);
        buf.push(0);
        buf
    }

    /// 数值段（`0%`~`100%`）：右对齐绘制，`%` 永远停在同一列。
    ///
    /// 上界钳到 100：数值列宽按 `100%` 的实测字宽预留，钳制让「字段一定装得下」
    /// 由格式化本身保证，而不是依赖采集侧永不越界——越界会被右对齐裁掉高位数字。
    fn format_percent_wide<'a>(
        buf: &'a mut Vec<u16>,
        scratch: &mut String,
        value: u32,
    ) -> &'a mut [u16] {
        buf.clear();
        push_decimal_wide(buf, scratch, u64::from(value.min(100)));
        buf.push(b'%' as u16);
        buf.push(0);
        buf
    }

    /// 网速文本：整数 + 单位，不带小数位。
    ///
    /// 档位判定与取整全程整数（`+ divisor/2` 是「四舍五入」的定点写法）：渲染路径
    /// 要能在测试里逐字节断言，不引入浮点。每档的值再钳到 1023，避免舍入把界内值
    /// 顶到下一档的量级——`1023.9 KB/s` 显示 `1024 KB/s` 会紧接着跳到 `1 MB/s`，
    /// 读起来像倒退；钳制后换档单调（`1023 KB/s` → `1 MB/s`）。
    fn format_speed_wide<'a>(
        buf: &'a mut Vec<u16>,
        scratch: &mut String,
        bytes_per_sec: u32,
    ) -> &'a mut [u16] {
        buf.clear();
        let bytes = bytes_per_sec as u64;
        let (value, unit): (u64, &str) = if bytes < KIB {
            (bytes, " B/s")
        } else if bytes < MIB {
            (((bytes + KIB / 2) / KIB).min(1023), " KB/s")
        } else if bytes < GIB {
            (((bytes + MIB / 2) / MIB).min(1023), " MB/s")
        } else {
            // 万兆链路满速是 1.25e9 B/s：没有这一档就只能显示成 4 位数的 MB。
            ((bytes + GIB / 2) / GIB, " GB/s")
        };
        push_decimal_wide(buf, scratch, value);
        buf.extend(unit.encode_utf16());
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

        // 网速列：箭头左对齐，速率从箭头列右边界起右对齐——速率矩形按最宽文本预留，
        // 任何量级的数字都只在矩形内向左长，碰不到箭头。
        let mut rc_up_arrow = RECT {
            left: layout.arrow_left,
            top: 0,
            right: layout.arrow_right,
            bottom: layout.half_height,
        };
        let mut up_arrow = ARROW_UP;
        draw_text(self.hdc_mem, &mut up_arrow, &mut rc_up_arrow, DT_LEFT);

        let mut rc_up_val = RECT {
            left: layout.arrow_right,
            top: 0,
            right: layout.speed_right,
            bottom: layout.half_height,
        };
        let up_val = Self::format_speed_wide(&mut self.buf, &mut self.scratch, values.speed_up);
        draw_text(self.hdc_mem, up_val, &mut rc_up_val, DT_RIGHT);

        let mut rc_down_arrow = RECT {
            left: layout.arrow_left,
            top: layout.half_height,
            right: layout.arrow_right,
            bottom: self.height,
        };
        let mut down_arrow = ARROW_DOWN;
        draw_text(self.hdc_mem, &mut down_arrow, &mut rc_down_arrow, DT_LEFT);

        let mut rc_down_val = RECT {
            left: layout.arrow_right,
            top: layout.half_height,
            right: layout.speed_right,
            bottom: self.height,
        };
        let down_val = Self::format_speed_wide(&mut self.buf, &mut self.scratch, values.speed_down);
        draw_text(self.hdc_mem, down_val, &mut rc_down_val, DT_RIGHT);

        // CPU/MEM 列：标签从各自实测起点左对齐画到同一个右缘（冒号同列），数值在
        // 固定宽度字段内右对齐——位数变化只向左长，`%` 永远停在同一列。
        let cpu_label = Self::format_label_wide(&mut self.buf, "CPU");
        let mut rc_cpu_label = RECT {
            left: layout.cpu_label_left,
            top: 0,
            right: layout.label_right + 1,
            bottom: layout.half_height,
        };
        draw_text(self.hdc_mem, cpu_label, &mut rc_cpu_label, DT_LEFT);

        let cpu_value = Self::format_percent_wide(&mut self.buf, &mut self.scratch, values.cpu);
        let mut rc_cpu_value = RECT {
            left: layout.value_left,
            top: 0,
            right: layout.value_right + 1,
            bottom: layout.half_height,
        };
        draw_text(self.hdc_mem, cpu_value, &mut rc_cpu_value, DT_RIGHT);

        let mem_label = Self::format_label_wide(&mut self.buf, "MEM");
        let mut rc_mem_label = RECT {
            left: layout.mem_label_left,
            top: layout.half_height,
            right: layout.label_right + 1,
            bottom: self.height,
        };
        draw_text(self.hdc_mem, mem_label, &mut rc_mem_label, DT_LEFT);

        let mem_value = Self::format_percent_wide(&mut self.buf, &mut self.scratch, values.mem);
        let mut rc_mem_value = RECT {
            left: layout.value_left,
            top: layout.half_height,
            right: layout.value_right + 1,
            bottom: self.height,
        };
        draw_text(self.hdc_mem, mem_value, &mut rc_mem_value, DT_RIGHT);

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
        self.layout = Layout::measure(self.hdc_mem, width, height);

        // hdc_mem 常驻：背景模式只需 new() 设一次，此处仅换位图/字体。
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

/// 双列布局（物理像素），三处边界的锚点各不相同，改之前先读这里：
///
/// - **标签**（`CPU:` / `MEM:`）以 `label_right` 为**右缘**、各自从实测起点
///   （`cpu_label_left` / `mem_label_left`）**左对齐**起画：两个冒号因此同列，
///   而 `CPU` 比 `MEM` 窄的那几像素自然表现为首字母不齐（冒号对齐的必然代价）；
/// - **数值**（`0%`~`100%`）在 [`value_left`, `value_right`] 字段内**右对齐**，
///   字段宽度按最宽数值 `100%` 预留：数字位数变化只往左长，`%` 永远停在同一列；
///   个位数时数字与冒号之间会有字段内的留白，这是右对齐的必然代价；
/// - **网速列**左锚在 `arrow_left`、右锚在 `speed_right`，列宽 = 箭头列 + 最宽速率
///   文本（[`SPEED_WIDTH_SAMPLES`] 经 `format_speed_wide` 现算），故速率矩形恰好装得下
///   `1023 MB/s`，压不到箭头。
///
/// 标签块左锚、网速列右锚，两列之间剩下的空间全部留在中间：它随 DPI 变化是因为
/// 「位图宽度取整」与「字号取整」不同步（字号先跳一档、宽度只涨几个百分点）。96-DPI
/// 基准下中间距 18px；下限由 `layout_has_no_overlap_across_dpi` 在整个 DPI 范围上钉死。
///
/// `width` 必须取已舍入的实际宽度（`dpi_scaled(DISPLAY_WIDTH, dpi)` 的输出），
/// 禁止经整数 DPI 中转二次舍入（96–384 实测 77 处差一像素）；改任一侧舍入策略
/// 都要重新全范围对账。纯值类型：随 DPI 更新整体重算并缓存（`Renderer::layout`），
/// 渲染热路径零分配纪律不变。
#[derive(Clone, Copy)]
struct Layout {
    cpu_label_left: i32,
    mem_label_left: i32,
    label_right: i32,
    value_left: i32,
    value_right: i32,
    arrow_left: i32,
    arrow_right: i32,
    speed_right: i32,
    half_height: i32,
}

impl Layout {
    /// 在已选入目标字体的 `hdc` 上实测字宽并算出整套边界。
    ///
    /// 矩形右边界额外放 1px：`DT_RIGHT` 要求矩形宽度不小于字形 advance，一旦量宽
    /// 与绘制取整差一像素，字形左缘就会被矩形裁掉（`MEM:` 的 `M` 会缺左竖）。
    /// 标签按**左对齐 + 实测起点**绘制，越界只可能发生在右侧，1px 余量足够；
    /// 这样两个标签共用同一右边界（冒号同列）又不会互相牵连。数值与网速字段
    /// 宽度按各自最宽文本预留，同样靠这 1px 余量兜住取整差。
    fn measure(hdc: HDC, width: i32, height: i32) -> Self {
        let scale = width as f64 / DISPLAY_WIDTH as f64;

        let cpu_label_width = measure_str(hdc, "CPU:");
        let mem_label_width = measure_str(hdc, "MEM:");
        let label_width = cpu_label_width.max(mem_label_width);
        let value_width = measure_str(hdc, "100%");
        // 箭头列宽 = 字形实测宽 + 固定间距：间距算进列宽，右对齐的速率字段才不会碰到箭头。
        let arrow_width =
            measure_wide(hdc, &ARROW_UP) + (LAYOUT_ARROW_GAP as f64 * scale).round() as i32;
        let speed_width = {
            // 各档上界喂给格式化器，取其真实文本的最宽形态——文本形态只由格式化器定义。
            let mut speed_buf = Vec::with_capacity(16);
            let mut scratch = String::with_capacity(8);
            SPEED_WIDTH_SAMPLES
                .iter()
                .map(|bytes| {
                    let text = Renderer::format_speed_wide(&mut speed_buf, &mut scratch, *bytes);
                    measure_wide(hdc, text)
                })
                .max()
                .unwrap_or(0)
        };

        let label_right = (LAYOUT_LEFT_MARGIN as f64 * scale).round() as i32 + label_width;
        let value_left = label_right + (LAYOUT_VALUE_GAP as f64 * scale).round() as i32;
        let value_right = value_left + value_width;
        let speed_right = width - (LAYOUT_SPEED_MARGIN as f64 * scale).round() as i32;
        let arrow_left = speed_right - (arrow_width + speed_width);

        Self {
            cpu_label_left: label_right - cpu_label_width,
            mem_label_left: label_right - mem_label_width,
            label_right,
            value_left,
            value_right,
            arrow_left,
            arrow_right: arrow_left + arrow_width,
            speed_right,
            half_height: height / 2,
        }
    }
}

fn draw_text(hdc: HDC, text: &mut [u16], rect: &mut RECT, align: DRAW_TEXT_FORMAT) {
    // windows crate 按切片长度传 cchText：尾 NUL 会被当成一个有 advance 的字符，
    // DT_RIGHT 下整条文本左移一个默认字形宽、最宽文本左缘被裁。与 measure_wide
    // 同一口径截掉尾 NUL，量宽与绘制才一致。
    let len = text.len().saturating_sub(1);
    // SAFETY: hdc 有效；rect 在栈上；text 为 NUL 结尾的 UTF-16 缓冲区，截掉尾 NUL
    // 后仍落在同一缓冲区内，DrawTextW 不保留指针。
    unsafe {
        let _ = DrawTextW(
            hdc,
            &mut text[..len],
            rect,
            DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | align,
        );
    }
}

/// 用 `hdc` 当前选入的字体测量文本宽度（物理像素）。
///
/// `text` 与 `DrawTextW` 的入参同形（NUL 结尾），测量时去掉尾 NUL：与渲染共用同一个
/// DC 和字体，量出来的就是实际绘制宽度（`Layout` 的全部列边界都以此为准）。
fn measure_wide(hdc: HDC, text: &[u16]) -> i32 {
    let len = text.len().saturating_sub(1);
    let mut size = SIZE::default();
    // SAFETY: hdc 有效且已选入被测量的字体；text 为 NUL 结尾的宽串，截掉尾 NUL 后
    // 仍落在同一缓冲区内；size 在栈上，GetTextExtentPoint32W 不保留指针。
    unsafe {
        let _ = GetTextExtentPoint32W(hdc, &text[..len], &mut size);
    }
    size.cx
}

fn measure_str(hdc: HDC, text: &str) -> i32 {
    let wide = to_wide(text);
    measure_wide(hdc, &wide)
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

/// 把 `value` 的十进制文本追加到 `buf`（不含尾 NUL），十进制形态由 std 的 `Display`
/// 唯一产出：格式必须与 `u32`/`u64` 的 `Display` 一致，否则宽度探针与绘制文本会对不上。
///
/// `scratch` 是调用方复用的缓冲区，写入失败只可能来自 `fmt::Error`（`String` 不会产生），
/// 故忽略返回值；`scratch` 在返回前已清空复写，容量随渲染器生命周期摊还。
fn push_decimal_wide(buf: &mut Vec<u16>, scratch: &mut String, value: u64) {
    scratch.clear();
    let _ = write!(scratch, "{value}");
    buf.extend(scratch.encode_utf16());
}
#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::{GetCurrentObject, GetPixel, OBJ_BITMAP};
    use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

    fn wide_to_string(wide: &[u16]) -> String {
        String::from_utf16_lossy(wide.strip_suffix(&[0]).unwrap_or(wide))
    }

    #[test]
    fn test_format_speed_wide_boundaries() {
        let mut buf = Vec::with_capacity(32);
        let mut scratch = String::new();

        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(&mut buf, &mut scratch, 0)),
            "0 B/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(&mut buf, &mut scratch, 512)),
            "512 B/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(&mut buf, &mut scratch, 1023)),
            "1023 B/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(&mut buf, &mut scratch, 1024)),
            "1 KB/s"
        );
        // 档内四舍五入：1.5 KiB/s 进位到 2。
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(&mut buf, &mut scratch, 1536)),
            "2 KB/s"
        );
        // 档内上界钳制：1023.9 KB/s 舍入后是 1024，但 1024 已是 MB 档的量级，
        // 显示成 "1024 KB/s" 会紧接 "1 MB/s" 像倒退；钳到 1023 保持单调。
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(
                &mut buf,
                &mut scratch,
                1024 * 1024 - 1
            )),
            "1023 KB/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(
                &mut buf,
                &mut scratch,
                1024 * 1024
            )),
            "1 MB/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(
                &mut buf,
                &mut scratch,
                1024 * 1024 * 10 + 1024 * 512
            )),
            "11 MB/s"
        );
        // 同样钳制在 MB 档：差 1 字节到 1 GiB 时显示 1023 而不是 1024。
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(
                &mut buf,
                &mut scratch,
                1024 * 1024 * 1024 - 1
            )),
            "1023 MB/s"
        );
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(
                &mut buf,
                &mut scratch,
                1024 * 1024 * 1024
            )),
            "1 GB/s"
        );
        // 万兆链路满速（1.25e9 B/s）：没有 GB 档时这里是 4 位数的 "1192 MB/s"。
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(
                &mut buf,
                &mut scratch,
                1_250_000_000
            )),
            "1 GB/s"
        );
        // u32 上限 = 4 GiB/s - 1 字节，四舍五入后正好 4（不是溢出）。
        assert_eq!(
            wide_to_string(Renderer::format_speed_wide(
                &mut buf,
                &mut scratch,
                u32::MAX
            )),
            "4 GB/s"
        );
    }

    /// 换档处显示值必须单调：任何速率都不该显示成下一档的量级（`1024 KB/s` / `1024 MB/s`）。
    #[test]
    fn test_speed_display_is_monotonic_across_tiers() {
        let limits = [
            (1024u32, "KB/s"),
            (1024 * 1024, "MB/s"),
            (1024 * 1024 * 1024, "GB/s"),
        ];
        let mut buf = Vec::with_capacity(32);
        let mut scratch = String::new();
        for (boundary, _) in limits {
            let below = wide_to_string(Renderer::format_speed_wide(
                &mut buf,
                &mut scratch,
                boundary - 1,
            ));
            let at = wide_to_string(Renderer::format_speed_wide(
                &mut buf,
                &mut scratch,
                boundary,
            ));
            let number =
                |s: &str| -> u32 { s.split(' ').next().unwrap_or("").parse().unwrap_or(0) };
            assert!(
                number(&below) < 1024,
                "{boundary} B/s 下方显示 {below}，越过了档位量级"
            );
            assert_eq!(number(&at), 1, "{boundary} B/s 处应显示 1：{at}");
        }
    }

    #[test]
    fn test_format_label_and_percent_wide() {
        let mut buf = Vec::new();
        let mut scratch = String::new();
        assert_eq!(
            wide_to_string(Renderer::format_label_wide(&mut buf, "CPU")),
            "CPU:"
        );
        assert_eq!(
            wide_to_string(Renderer::format_label_wide(&mut buf, "MEM")),
            "MEM:"
        );
        for (value, expected) in [(0, "0%"), (5, "5%"), (50, "50%"), (100, "100%")] {
            assert_eq!(
                wide_to_string(Renderer::format_percent_wide(&mut buf, &mut scratch, value)),
                expected
            );
        }
        // 越界钳制：数值列宽按 `100%` 预留，超出只显示 100%，不裁掉高位数字。
        assert_eq!(
            wide_to_string(Renderer::format_percent_wide(&mut buf, &mut scratch, 101)),
            "100%"
        );
        assert_eq!(
            wide_to_string(Renderer::format_percent_wide(
                &mut buf,
                &mut scratch,
                u32::MAX
            )),
            "100%"
        );
    }

    /// 在给定字号的独立内存 DC 上执行 `f`，退出前把 stock 对象换回 DC。
    ///
    /// 换回是必须的：`DeleteObject` 对仍被选中的对象无效，只有换回后字体才随
    /// `OwnedGdi` 的 Drop 真正释放（与 `Renderer::drop` 同一释放序）。
    fn with_font<T>(font_size: i32, f: impl FnOnce(HDC) -> T) -> T {
        let dc = OwnedGdi::new(unsafe { CreateCompatibleDC(None) }).expect("测试环境 GDI 不可用");
        let font = OwnedGdi::new(create_font(font_size)).expect("测试环境 GDI 不可用");
        // SAFETY: dc.0 为刚创建的有效内存 DC；font.0 为未被任何 DC 选中的独占对象。
        let old = unsafe { SelectObject(dc.0, font.0.into()) };
        assert!(!old.is_invalid(), "测试环境无法选入字体");
        let result = f(dc.0);
        // SAFETY: old 是本函数换出的原选中对象，仍然有效；换回后 font 才脱离 DC。
        assert!(
            !unsafe { SelectObject(dc.0, old) }.is_invalid(),
            "测试环境无法还原 DC 的选中对象"
        );
        result
    }

    /// 跨 DPI 的布局不变量：静态标签与动态数值互不重叠、两列之间始终有间距、
    /// 网速列整体不越出位图。
    ///
    /// 96–384 DPI 逐档扫（步长 1），因为这些边界是「位图宽度取整」与「字号取整」
    /// 两套取整共同算出来的，极值只出现在个别 DPI 上（最紧的是 dpi 100）。
    #[test]
    fn layout_has_no_overlap_across_dpi() {
        for dpi in 96..=384u32 {
            with_font(dpi_scaled(FONT_BASE_SIZE, dpi), |hdc| {
                let width = dpi_scaled(DISPLAY_WIDTH, dpi);
                let height = dpi_scaled(DISPLAY_HEIGHT, dpi);
                let layout = Layout::measure(hdc, width, height);

                assert!(
                    layout.mem_label_left >= 0,
                    "dpi {dpi}: 标签越出左边界（mem_label_left={}）",
                    layout.mem_label_left
                );
                assert!(
                    layout.cpu_label_left > layout.mem_label_left,
                    "dpi {dpi}: CPU 不比 MEM 窄，冒号对齐全靠右缘，这里只钉住两者的相对位置"
                );
                assert!(
                    layout.value_left > layout.label_right,
                    "dpi {dpi}: 数值矩形压到标签矩形"
                );
                assert!(
                    layout.arrow_left - layout.value_right >= 6,
                    "dpi {dpi}: CPU/MEM 列与网速列之间只剩 {}px",
                    layout.arrow_left - layout.value_right
                );
                assert!(
                    layout.arrow_right > layout.arrow_left,
                    "dpi {dpi}: 箭头列为空"
                );
                assert!(
                    layout.speed_right <= width,
                    "dpi {dpi}: 网速列越过位图右边界"
                );
                assert_eq!(layout.half_height, height / 2, "dpi {dpi}: 半高错位");
            });
        }
    }

    /// 网速列必须装得下**任何**取值格式化出来的文本（四位数、GB 档、各档上界）。
    /// 用真实格式化结果复验，不做常量之间的自证。
    #[test]
    fn speed_column_fits_every_tier() {
        let mut samples: Vec<u32> = vec![
            0,
            1,
            1023,
            1024,
            1025,
            1024 * 1024 - 1,
            1024 * 1024,
            1024 * 1024 * 1024 - 1,
            1024 * 1024 * 1024,
            u32::MAX,
        ];
        // 全域粗扫：步长取素数，避开档位边界的整数倍，才扫得到档内最宽形态。
        samples.extend((0..=u32::MAX).step_by(65_537));

        with_font(FONT_BASE_SIZE, |hdc| {
            let layout = Layout::measure(hdc, DISPLAY_WIDTH, DISPLAY_HEIGHT);
            let field_width = layout.speed_right - layout.arrow_right;
            let mut buf = Vec::with_capacity(32);
            let mut scratch = String::new();
            for value in samples {
                let text = Renderer::format_speed_wide(&mut buf, &mut scratch, value);
                let shown = wide_to_string(text);
                let text_width = measure_wide(hdc, text);
                assert!(
                    text_width <= field_width,
                    "网速 {value} B/s 显示为 {shown}（{text_width}px）超过速率列宽 {field_width}px"
                );
            }
        });
    }

    /// 真实渲染像素回归：跑一遍 `Renderer::render`，用像素断言字形没被矩形裁掉、
    /// 也没贴到位图边缘。
    ///
    /// 这条用例存在的原因：只验「矩形互不重叠」查不出裁字。1.7.6 把标签用 `DT_RIGHT`
    /// 对齐到一个正好等于量宽的矩形里，量宽与绘制取整差一像素时字形左缘被削掉
    /// （`MEM:` 的 `M` 缺左竖），而几何断言全绿、模拟图也看不出来——必须在真 DC 上
    /// 读回像素才算验证过。
    #[test]
    fn render_keeps_glyphs_inside_bitmap() {
        let mut renderer = Renderer::new().expect("测试环境 GDI 不可用");
        let layout = renderer.layout;
        let (width, height) = renderer.bitmap_size();

        let screen_dc = ScreenDcGuard::acquire().expect("测试环境拿不到屏幕 DC");
        // SAFETY: screen_dc.hdc 有效；尺寸取渲染器位图的实际尺寸。
        let target = OwnedGdi::new(unsafe { CreateCompatibleDC(Some(screen_dc.hdc)) })
            .expect("测试环境 GDI 不可用");
        let target_bitmap =
            OwnedGdi::new(unsafe { CreateCompatibleBitmap(screen_dc.hdc, width, height) })
                .expect("测试环境 GDI 不可用");
        // SAFETY: target.0 有效；target_bitmap.0 未被任何 DC 选中。
        let old_bitmap = unsafe { SelectObject(target.0, target_bitmap.0.into()) };
        assert!(!old_bitmap.is_invalid(), "无法把目标位图选入 DC");

        renderer.render(target.0);

        // 按列扫描两行的墨迹范围。背景是色键，凡不是色键的像素都算墨迹。
        let row_ink = |y0: i32, y1: i32| -> Option<(i32, i32)> {
            let mut lo = i32::MAX;
            let mut hi = -1;
            for y in y0..y1 {
                for x in 0..width {
                    // SAFETY: target.0 有效，坐标落在位图内。
                    if unsafe { GetPixel(target.0, x, y) }.0 != COLOR_KEY {
                        lo = lo.min(x);
                        hi = hi.max(x);
                    }
                }
            }
            (hi >= 0).then_some((lo, hi))
        };

        let cpu_row = row_ink(0, layout.half_height).expect("CPU 行没有画出任何内容");
        let mem_row = row_ink(layout.half_height, height).expect("MEM 行没有画出任何内容");

        // 左边界：标签必须从各自实测起点开始，不能贴边（贴边说明被裁）。
        assert!(
            mem_row.0 >= layout.mem_label_left,
            "MEM 行墨迹从 x={} 起，早于实测起点 {}",
            mem_row.0,
            layout.mem_label_left
        );
        assert!(mem_row.0 > 0, "MEM 行贴到了位图左边缘，说明字形被裁");
        assert!(cpu_row.0 > 0, "CPU 行贴到了位图左边缘，说明字形被裁");

        // 标签宽度：墨迹跨度不得明显小于 advance（被裁时跨度会整块变短）。
        let mem_advance = measure_str(renderer.hdc_mem, "MEM:");
        let mem_label_span = mem_row.1.min(layout.label_right) - mem_row.0 + 1;
        assert!(
            mem_label_span >= mem_advance - 2,
            "MEM 标签被裁：墨迹跨度 {mem_label_span}px，实测 advance {mem_advance}px"
        );

        // 右边界：网速文本右对齐在 speed_right 上，不得越过位图右侧留白。
        assert!(
            mem_row.1 <= width - 2,
            "MEM 行墨迹到 x={}，越过右侧留白（位图宽 {width}）",
            mem_row.1
        );

        // 收尾：把原选中位图换回，target_bitmap 才能随 OwnedGdi 真正释放。
        // SAFETY: old_bitmap 是本用例换出的原选中对象，仍然有效。
        assert!(!unsafe { SelectObject(target.0, old_bitmap) }.is_invalid());
    }

    /// 百分比右对齐回归：同一帧里个位数（`5%`）与三位数（`100%`）的墨迹右缘必须
    /// 同列并顶到字段右界，且个位数左缘更靠右。退回左对齐时右缘随位数变化，
    /// 这些断言都会失败。
    #[test]
    fn percent_values_are_right_aligned() {
        use crate::state::{CPU_USAGE, MEM_USAGE};

        CPU_USAGE.store(5, Ordering::Relaxed);
        MEM_USAGE.store(100, Ordering::Relaxed);

        let mut renderer = Renderer::new().expect("测试环境 GDI 不可用");
        let layout = renderer.layout;
        let (width, height) = renderer.bitmap_size();

        let screen_dc = ScreenDcGuard::acquire().expect("测试环境拿不到屏幕 DC");
        // SAFETY: screen_dc.hdc 有效；尺寸取渲染器位图的实际尺寸。
        let target = OwnedGdi::new(unsafe { CreateCompatibleDC(Some(screen_dc.hdc)) })
            .expect("测试环境 GDI 不可用");
        let target_bitmap =
            OwnedGdi::new(unsafe { CreateCompatibleBitmap(screen_dc.hdc, width, height) })
                .expect("测试环境 GDI 不可用");
        // SAFETY: target.0 有效；target_bitmap.0 未被任何 DC 选中。
        let old_bitmap = unsafe { SelectObject(target.0, target_bitmap.0.into()) };
        assert!(!old_bitmap.is_invalid(), "无法把目标位图选入 DC");

        renderer.render(target.0);

        // 只扫数值字段窗口内的墨迹：标签在窗口左侧外，箭头在右侧外。
        let value_ink = |y0: i32, y1: i32| -> (i32, i32) {
            let mut lo = i32::MAX;
            let mut hi = -1;
            for y in y0..y1 {
                for x in layout.value_left..=layout.value_right {
                    // SAFETY: target.0 有效，坐标落在位图内。
                    if unsafe { GetPixel(target.0, x, y) }.0 != COLOR_KEY {
                        lo = lo.min(x);
                        hi = hi.max(x);
                    }
                }
            }
            (lo, hi)
        };
        let cpu = value_ink(0, layout.half_height);
        let mem = value_ink(layout.half_height, height);
        assert!(cpu.1 >= 0 && mem.1 >= 0, "数值列没有画出墨迹");
        assert!(
            (cpu.1 - mem.1).abs() <= 1,
            "两行百分比右缘不同列：cpu={} mem={}",
            cpu.1,
            mem.1
        );
        assert!(
            cpu.0 > mem.0,
            "个位数左缘应更靠右：cpu={} mem={}",
            cpu.0,
            mem.0
        );
        assert!(
            mem.1 >= layout.value_right - 2,
            "百分比右缘未顶到字段右界：{} vs {}",
            mem.1,
            layout.value_right
        );

        // SAFETY: old_bitmap 是本用例换出的原选中对象，仍然有效。
        assert!(!unsafe { SelectObject(target.0, old_bitmap) }.is_invalid());
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
