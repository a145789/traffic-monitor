use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{COLORREF, HWND, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW, CreateSolidBrush,
    DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW,
    FONT_QUALITY, FillRect, GetTextExtentPoint32W, GetWindowDC, HBITMAP, HBRUSH, HDC, HFONT,
    HGDIOBJ, LOGFONTW, ReleaseDC, SRCCOPY, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::Registry::HKEY_CURRENT_USER;

use crate::config::{
    COLOR_DARK_TEXT, COLOR_KEY, COLOR_LIGHT_TEXT, COLOR_LOW_BATTERY, CPU_USAGE, DISPLAY_HEIGHT,
    DISPLAY_WIDTH, FONT_BASE_SIZE, MEM_USAGE, MOUSE_BATTERY_LEVEL, MOUSE_DPI_VALUE,
    MOUSE_IS_CHARGING, MOUSE_ONLINE, NET_SPEED_DOWN, NET_SPEED_UP, SHOW_MOUSE_INFO,
};
use crate::util::{reg_read_dword, to_wide};

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
}

impl Renderer {
    pub fn new() -> Self {
        // SAFETY: null_mut 句柄获取整个屏幕的临时 HDC。
        let hdc_screen = unsafe { GetWindowDC(Some(HWND(std::ptr::null_mut()))) };

        // SAFETY: hdc_screen 有效，创建兼容内存 DC。
        let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };

        // SAFETY: hdc_screen 有效，创建兼容位图。
        let hbitmap = unsafe { CreateCompatibleBitmap(hdc_screen, DISPLAY_WIDTH, DISPLAY_HEIGHT) };

        // SAFETY: hdc_mem 和 hbitmap 有效，选入并备份旧位图。
        let old_bitmap = unsafe { SelectObject(hdc_mem, hbitmap.into()) };

        let hfont = create_font(FONT_BASE_SIZE);

        // SAFETY: hfont 有效，选入并备份旧字体。
        let old_font = unsafe { SelectObject(hdc_mem, hfont.into()) };

        // SAFETY: 创建透明背景纯色刷子。
        let hbrush = unsafe { CreateSolidBrush(COLORREF(COLOR_KEY)) };

        // SAFETY: 设置背景模式为透明。
        unsafe {
            let _ = SetBkMode(hdc_mem, TRANSPARENT);
        }

        let arrow_width = {
            let arrow_text = to_wide("\u{2191} ");
            let mut size = SIZE::default();
            // SAFETY: hdc_mem 有效，arrow_text 以 NUL 结尾，size 在栈上。
            unsafe {
                let _ =
                    GetTextExtentPoint32W(hdc_mem, &arrow_text[..arrow_text.len() - 1], &mut size);
            }
            size.cx
        };

        // SAFETY: 释放临时屏幕 HDC。
        unsafe {
            let _ = ReleaseDC(Some(HWND(std::ptr::null_mut())), hdc_screen);
        }

        Self {
            hdc_mem,
            hbitmap,
            hfont,
            old_bitmap,
            old_font,
            hbrush,
            text_color: COLORREF(COLOR_LIGHT_TEXT),
            font_size: FONT_BASE_SIZE,
            width: DISPLAY_WIDTH,
            height: DISPLAY_HEIGHT,
            arrow_width,
        }
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

    fn format_percent_wide(buf: &mut Vec<u16>, value: u32) -> &mut [u16] {
        buf.clear();
        write_u32(buf, value);
        push_ascii(buf, "%");
        buf.push(0);
        buf
    }

    fn format_mouse_battery_wide(buf: &mut Vec<u16>, value: u32) -> &mut [u16] {
        buf.clear();
        // U+1F5B1 (🖱️) 的 UTF-16 代理对
        buf.push(0xD83D);
        buf.push(0xDDB1);
        buf.push(b' ' as u16);
        write_u32(buf, value);
        push_ascii(buf, "%");
        buf.push(0);
        buf
    }

    fn format_dpi_wide(buf: &mut Vec<u16>, value: u32) -> &mut [u16] {
        buf.clear();
        push_ascii(buf, "DPI: ");
        write_u32(buf, value);
        buf.push(0);
        buf
    }

    fn format_speed_wide(buf: &mut Vec<u16>, bytes_per_sec: u32) -> &mut [u16] {
        buf.clear();
        if bytes_per_sec < 1024 {
            write_u32(buf, bytes_per_sec);
            push_ascii(buf, " B/s");
        } else if bytes_per_sec < 1024 * 1024 {
            write_fixed1(buf, bytes_per_sec as f64 / 1024.0);
            push_ascii(buf, " KB/s");
        } else {
            write_fixed1(buf, bytes_per_sec as f64 / (1024.0 * 1024.0));
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
        let show_mouse = SHOW_MOUSE_INFO.load(Ordering::Relaxed);
        let mouse_online = MOUSE_ONLINE.load(Ordering::Relaxed);
        let battery = MOUSE_BATTERY_LEVEL.load(Ordering::Relaxed);
        let charging = MOUSE_IS_CHARGING.load(Ordering::Relaxed);
        let dpi = MOUSE_DPI_VALUE.load(Ordering::Relaxed);

        let half_height = self.height / 2;
        let scale = self.width as f64 / DISPLAY_WIDTH as f64;

        let mut buf = Vec::with_capacity(32);

        // 1. 绘制第三列 (网速) - 最右列
        // 箭头左对齐，数值右对齐 — 表格效果
        let col_gap = (13.0 * scale).round() as i32;
        let speed_right = self.width - (4.0 * scale).round() as i32;
        let speed_left = speed_right - (76.0 * scale).round() as i32;
        let arrow_right = speed_left + self.arrow_width;

        // SAFETY: hdc_mem、hbrush 均有效，DrawTextW 的 RECT 和字符串在栈上分配。
        unsafe {
            let _ = FillRect(self.hdc_mem, &rect, self.hbrush);
            SetTextColor(self.hdc_mem, self.text_color);
            let hdc_mem = self.hdc_mem;

            // 上行箭头
            let mut rc_up_arrow = RECT {
                left: speed_left,
                top: 0,
                right: arrow_right,
                bottom: half_height,
            };
            let up_arrow = Self::wide(&mut buf, "\u{2191}");
            let _ = DrawTextW(
                hdc_mem,
                up_arrow,
                &mut rc_up_arrow,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
            );

            // 上行数值
            let mut rc_up_val = RECT {
                left: arrow_right,
                top: 0,
                right: speed_right,
                bottom: half_height,
            };
            let up_val = Self::format_speed_wide(&mut buf, speed_up);
            let _ = DrawTextW(
                hdc_mem,
                up_val,
                &mut rc_up_val,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_RIGHT,
            );

            // 下行箭头
            let mut rc_down_arrow = RECT {
                left: speed_left,
                top: half_height,
                right: arrow_right,
                bottom: self.height,
            };
            let down_arrow = Self::wide(&mut buf, "\u{2193}");
            let _ = DrawTextW(
                hdc_mem,
                down_arrow,
                &mut rc_down_arrow,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
            );

            // 下行数值
            let mut rc_down_val = RECT {
                left: arrow_right,
                top: half_height,
                right: speed_right,
                bottom: self.height,
            };
            let down_val = Self::format_speed_wide(&mut buf, speed_down);
            let _ = DrawTextW(
                hdc_mem,
                down_val,
                &mut rc_down_val,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_RIGHT,
            );

            // 2. 绘制第二列 (鼠标信息) - 中列
            if show_mouse {
                let mouse_right = speed_left - col_gap;
                let mouse_left = mouse_right - (62.0 * scale).round() as i32;

                if mouse_online {
                    // 第一行：鼠标电量
                    if battery < 20 && !charging {
                        // 画图标 🖱️
                        let mut rc_mouse = RECT {
                            left: mouse_left,
                            top: 0,
                            right: mouse_right,
                            bottom: half_height,
                        };
                        let mouse_wide = Self::wide(&mut buf, "\u{1F5B1}");
                        let _ = DrawTextW(
                            hdc_mem,
                            mouse_wide,
                            &mut rc_mouse,
                            DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
                        );

                        // 用红色画电量数字，左侧相对偏移 16 像素
                        let battery_color = COLORREF(COLOR_LOW_BATTERY);
                        SetTextColor(hdc_mem, battery_color);
                        let battery_wide = Self::format_percent_wide(&mut buf, battery);
                        let mut rc_bat = RECT {
                            left: mouse_left + (16.0 * scale).round() as i32,
                            top: 0,
                            right: mouse_right,
                            bottom: half_height,
                        };
                        let _ = DrawTextW(
                            hdc_mem,
                            battery_wide,
                            &mut rc_bat,
                            DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
                        );

                        // 恢复颜色
                        SetTextColor(hdc_mem, self.text_color);
                    } else {
                        let mouse_wide = Self::format_mouse_battery_wide(&mut buf, battery);
                        let mut rc_mouse = RECT {
                            left: mouse_left,
                            top: 0,
                            right: mouse_right,
                            bottom: half_height,
                        };
                        let _ = DrawTextW(
                            hdc_mem,
                            mouse_wide,
                            &mut rc_mouse,
                            DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
                        );
                    }

                    // 第二行：DPI
                    let h = self.height;
                    let dpi_wide = Self::format_dpi_wide(&mut buf, dpi);
                    let mut rc_dpi = RECT {
                        left: mouse_left,
                        top: half_height,
                        right: mouse_right,
                        bottom: h,
                    };
                    let _ = DrawTextW(
                        hdc_mem,
                        dpi_wide,
                        &mut rc_dpi,
                        DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
                    );
                } else {
                    // 鼠标离线，画 --
                    let mouse_text = "\u{1F5B1} --";
                    let mut rc_mouse = RECT {
                        left: mouse_left,
                        top: 0,
                        right: mouse_right,
                        bottom: half_height,
                    };
                    let mouse_wide = Self::wide(&mut buf, mouse_text);
                    let _ = DrawTextW(
                        hdc_mem,
                        mouse_wide,
                        &mut rc_mouse,
                        DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
                    );

                    let dpi_text = "DPI: --";
                    let mut rc_dpi = RECT {
                        left: mouse_left,
                        top: half_height,
                        right: mouse_right,
                        bottom: self.height,
                    };
                    let dpi_wide = Self::wide(&mut buf, dpi_text);
                    let _ = DrawTextW(
                        hdc_mem,
                        dpi_wide,
                        &mut rc_dpi,
                        DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
                    );
                }
            }

            // 3. 绘制第一列 (CPU & MEM) - 最左列
            let cpu_right = if show_mouse {
                speed_left - col_gap - (62.0 * scale).round() as i32 - col_gap
            } else {
                speed_left - col_gap
            };
            let cpu_left = cpu_right - (76.0 * scale).round() as i32;

            let cpu_wide = Self::format_cpu_mem_wide(&mut buf, "CPU", cpu);
            let mut rc_cpu = RECT {
                left: cpu_left,
                top: 0,
                right: cpu_right,
                bottom: half_height,
            };
            let _ = DrawTextW(
                hdc_mem,
                cpu_wide,
                &mut rc_cpu,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_RIGHT,
            );

            let h = self.height;
            let mem_wide = Self::format_cpu_mem_wide(&mut buf, "MEM", mem);
            let mut rc_mem = RECT {
                left: cpu_left,
                top: half_height,
                right: cpu_right,
                bottom: h,
            };
            let _ = DrawTextW(
                hdc_mem,
                mem_wide,
                &mut rc_mem,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_RIGHT,
            );

            SetTextColor(hdc_mem, self.text_color);

            let _ = BitBlt(
                hdc,
                0,
                0,
                self.width,
                self.height,
                Some(hdc_mem),
                0,
                0,
                SRCCOPY,
            );
        }
    }

    pub fn update_dpi(&mut self, hwnd: HWND) {
        // SAFETY: hwnd 是在当前进程上下文中有效且处于活动状态的窗口句柄，调用 GetDpiForWindow 是安全的，无跨进程非法访问问题。
        let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
        let scale = dpi as f64 / 96.0;
        let width = (DISPLAY_WIDTH as f64 * scale).round() as i32;
        let height = (DISPLAY_HEIGHT as f64 * scale).round() as i32;

        // 1. 创建符合新大小的 Compatible Bitmap
        // SAFETY: null_mut 句柄获取临时屏幕 HDC。
        let hdc_screen = unsafe { GetWindowDC(Some(HWND(std::ptr::null_mut()))) };

        // SAFETY: hdc_screen 有效，创建兼容位图。
        let new_bitmap = unsafe { CreateCompatibleBitmap(hdc_screen, width, height) };

        // SAFETY: 释放临时屏幕 HDC。
        unsafe {
            let _ = ReleaseDC(Some(HWND(std::ptr::null_mut())), hdc_screen);
        }

        // 2. 将新位图选入内存 DC，销毁旧位图
        if !new_bitmap.is_invalid() {
            // SAFETY: hdc_mem 和 new_bitmap 有效，选入新位图并销毁旧位图。
            let old_bitmap = unsafe { SelectObject(self.hdc_mem, new_bitmap.into()) };
            unsafe {
                let _ = DeleteObject(old_bitmap);
            }
            self.hbitmap = new_bitmap;
        }

        // 3. 重新创建并选择字体（不设上限）
        let font_size = (FONT_BASE_SIZE as f64 * scale).round() as i32;
        let new_font = create_font(font_size);
        if !new_font.is_invalid() {
            // SAFETY: hdc_mem 和 new_font 有效，选入新字体并销毁旧字体。
            let old_font = unsafe { SelectObject(self.hdc_mem, new_font.into()) };
            unsafe {
                let _ = DeleteObject(old_font);
            }
            self.hfont = new_font;
        }

        // 4. 更新相关属性
        self.font_size = font_size;
        self.width = width;
        self.height = height;

        // SAFETY: hdc_mem 有效，设置背景模式为透明。
        unsafe {
            let _ = SetBkMode(self.hdc_mem, TRANSPARENT);
        }

        let arrow_width = {
            let hdc_mem = self.hdc_mem;
            let arrow_text = to_wide("\u{2191} ");
            let arrow_len = arrow_text.len() - 1;
            let mut size = SIZE::default();
            // SAFETY: hdc_mem 有效，arrow_text 以 NUL 结尾，size 在栈上。
            unsafe {
                let _ = GetTextExtentPoint32W(hdc_mem, &arrow_text[..arrow_len], &mut size);
            }
            size.cx
        };
        self.arrow_width = arrow_width;
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY:
        // 1. self.hdc_mem 是有效持有的内存设备上下文，还原最初选入上下文的备用 GDI 对象 self.old_bitmap 和 self.old_font 能避免还原默认 GDI 对象时的泄露。
        // 2. 所持有的 HFONT、HBITMAP、HBRUSH 句柄均由当前结构体独占，使用 DeleteObject 和 DeleteDC 销毁它们可安全归还系统图形资源。
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

fn create_font(size: i32) -> HFONT {
    let mut lf = LOGFONTW {
        lfHeight: -size,
        lfWeight: 400,
        lfQuality: FONT_QUALITY(3), // NONANTIALIASED_QUALITY, 彻底斩断 Layered 窗口上 GDI 渲染的半透明粉红毛边
        ..Default::default()
    };
    let font_name: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    lf.lfFaceName[..font_name.len()].copy_from_slice(&font_name);
    // SAFETY:
    // 1. lf 已经过完整的零初始化且 lfFaceName 被安全地写入了以 NUL 结尾的 "Segoe UI" 宽字符序列，避免了非法内存溢出。
    // 2. 传入 LOGFONTW 结构体的指针给 CreateFontIndirectW 调用是内存安全的，返回的 HFONT 句柄所有权将被返回并交由外部进行清理。
    unsafe { CreateFontIndirectW(&lf) }
}

pub fn is_system_light_theme() -> bool {
    reg_read_dword(
        HKEY_CURRENT_USER,
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

fn write_fixed1(buf: &mut Vec<u16>, val: f64) {
    let mut int_part = val as u32;
    let mut frac = ((val - int_part as f64) * 10.0).round() as u32;
    if frac >= 10 {
        int_part += 1;
        frac = 0;
    }
    write_u32(buf, int_part);
    buf.push(b'.' as u16);
    buf.push((b'0' + frac as u8) as u16);
}
