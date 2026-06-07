use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{COLORREF, HWND, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW,
    CreateSolidBrush, DeleteDC, DeleteObject, FillRect, GetTextExtentPoint32W,
    GetWindowDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    HDC, HFONT, HBITMAP, HGDIOBJ, HBRUSH,
    SRCCOPY, FONT_QUALITY, LOGFONTW, TRANSPARENT,
    DrawTextW, DT_LEFT, DT_RIGHT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER
};

use crate::config::{
    COLOR_DARK_TEXT, COLOR_KEY, COLOR_LIGHT_TEXT, COLOR_LOW_BATTERY, DISPLAY_HEIGHT,
    DISPLAY_WIDTH, FONT_BASE_SIZE, MOUSE_ONLINE, CPU_USAGE, MEM_USAGE,
    NET_SPEED_DOWN, NET_SPEED_UP, MOUSE_BATTERY_LEVEL, MOUSE_DPI_VALUE, MOUSE_IS_CHARGING,
    SHOW_MOUSE_INFO,
};

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
        unsafe {
            let hdc_screen = GetWindowDC(Some(HWND(std::ptr::null_mut())));
            let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
            let hbitmap = CreateCompatibleBitmap(hdc_screen, DISPLAY_WIDTH, DISPLAY_HEIGHT);
            let old_bitmap = SelectObject(hdc_mem, hbitmap.into());

            let hfont = create_font(FONT_BASE_SIZE);
            let old_font = SelectObject(hdc_mem, hfont.into());

            let hbrush = CreateSolidBrush(COLORREF(COLOR_KEY));

            let _ = SetBkMode(hdc_mem, TRANSPARENT);

            let arrow_width = {
                let arrow_text = to_wide("\u{2191} ");
                let mut size = SIZE::default();
                let _ = GetTextExtentPoint32W(hdc_mem, &arrow_text[..arrow_text.len() - 1], &mut size);
                size.cx
            };

            let _ = ReleaseDC(Some(HWND(std::ptr::null_mut())), hdc_screen);

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
    }

    pub fn update_text_color(&mut self) {
        unsafe {
            if is_system_light_theme() {
                self.text_color = COLORREF(COLOR_DARK_TEXT);
            } else {
                self.text_color = COLORREF(COLOR_LIGHT_TEXT);
            }
        }
    }

    pub fn render(&self, hdc: HDC) {
        unsafe {
            let rect = RECT {
                left: 0,
                top: 0,
                right: self.width,
                bottom: self.height,
            };

            let _ = FillRect(self.hdc_mem, &rect, self.hbrush);

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

            // 1. 绘制第三列 (网速) - 最右列
            // 箭头左对齐，数值右对齐 — 表格效果
            let col_gap = (13.0 * scale).round() as i32;
            let speed_right = self.width - (4.0 * scale).round() as i32;
            let speed_left = speed_right - (76.0 * scale).round() as i32;
            let arrow_right = speed_left + self.arrow_width;

            SetTextColor(self.hdc_mem, self.text_color);

            // 上行箭头
            let mut rc_up_arrow = RECT {
                left: speed_left,
                top: 0,
                right: arrow_right,
                bottom: half_height,
            };
            let mut up_arrow = to_wide("\u{2191}");
            let _ = DrawTextW(
                self.hdc_mem,
                &mut up_arrow,
                &mut rc_up_arrow,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
            );

            // 上行数值
            let speed_up_text = format_speed(speed_up);
            let mut rc_up_val = RECT {
                left: arrow_right,
                top: 0,
                right: speed_right,
                bottom: half_height,
            };
            let mut up_val = to_wide(&speed_up_text);
            let _ = DrawTextW(
                self.hdc_mem,
                &mut up_val,
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
            let mut down_arrow = to_wide("\u{2193}");
            let _ = DrawTextW(
                self.hdc_mem,
                &mut down_arrow,
                &mut rc_down_arrow,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
            );

            // 下行数值
            let speed_down_text = format_speed(speed_down);
            let mut rc_down_val = RECT {
                left: arrow_right,
                top: half_height,
                right: speed_right,
                bottom: self.height,
            };
            let mut down_val = to_wide(&speed_down_text);
            let _ = DrawTextW(
                self.hdc_mem,
                &mut down_val,
                &mut rc_down_val,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_RIGHT,
            );

            // 2. 绘制第二列 (鼠标信息) - 中列
            // 宽度 52。右界与网速列左界相距 col_gap
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
                        let mut mouse_wide = to_wide("\u{1F5B1}");
                        let _ = DrawTextW(
                            self.hdc_mem,
                            &mut mouse_wide,
                            &mut rc_mouse,
                            DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
                        );

                        // 用红色画电量数字，左侧相对偏移 16 像素
                        let battery_color = COLORREF(COLOR_LOW_BATTERY);
                        SetTextColor(self.hdc_mem, battery_color);
                        let battery_text = format!("{}%", battery);
                        let mut rc_bat = RECT {
                            left: mouse_left + (16.0 * scale).round() as i32,
                            top: 0,
                            right: mouse_right,
                            bottom: half_height,
                        };
                        let mut battery_wide = to_wide(&battery_text);
                        let _ = DrawTextW(
                            self.hdc_mem,
                            &mut battery_wide,
                            &mut rc_bat,
                            DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
                        );

                        // 恢复颜色
                        SetTextColor(self.hdc_mem, self.text_color);
                    } else {
                        let mouse_text = format!("\u{1F5B1} {}%", battery);
                        let mut rc_mouse = RECT {
                            left: mouse_left,
                            top: 0,
                            right: mouse_right,
                            bottom: half_height,
                        };
                        let mut mouse_wide = to_wide(&mouse_text);
                        let _ = DrawTextW(
                            self.hdc_mem,
                            &mut mouse_wide,
                            &mut rc_mouse,
                            DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
                        );
                    }

                    // 第二行：DPI
                    let dpi_text = format!("DPI: {}", dpi);
                    let mut rc_dpi = RECT {
                        left: mouse_left,
                        top: half_height,
                        right: mouse_right,
                        bottom: self.height,
                    };
                    let mut dpi_wide = to_wide(&dpi_text);
                    let _ = DrawTextW(
                        self.hdc_mem,
                        &mut dpi_wide,
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
                    let mut mouse_wide = to_wide(mouse_text);
                    let _ = DrawTextW(
                        self.hdc_mem,
                        &mut mouse_wide,
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
                    let mut dpi_wide = to_wide(dpi_text);
                    let _ = DrawTextW(
                        self.hdc_mem,
                        &mut dpi_wide,
                        &mut rc_dpi,
                        DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
                    );
                }
            }

            // 3. 绘制第一列 (CPU & MEM) - 最左列
            // 宽度 54。右界与下一列相距 col_gap
            let cpu_right = if show_mouse {
                speed_left - col_gap - (52.0 * scale).round() as i32 - col_gap
            } else {
                speed_left - col_gap
            };
            let cpu_left = cpu_right - (76.0 * scale).round() as i32;

            let cpu_text = format!("CPU: {}%", cpu);
            let mut rc_cpu = RECT {
                left: cpu_left,
                top: 0,
                right: cpu_right,
                bottom: half_height,
            };
            let mut cpu_wide = to_wide(&cpu_text);
            let _ = DrawTextW(
                self.hdc_mem,
                &mut cpu_wide,
                &mut rc_cpu,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_RIGHT,
            );

            let mem_text = format!("MEM: {}%", mem);
            let mut rc_mem = RECT {
                left: cpu_left,
                top: half_height,
                right: cpu_right,
                bottom: self.height,
            };
            let mut mem_wide = to_wide(&mem_text);
            let _ = DrawTextW(
                self.hdc_mem,
                &mut mem_wide,
                &mut rc_mem,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_RIGHT,
            );

            SetTextColor(self.hdc_mem, self.text_color);

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
        unsafe {
            let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd);
            let scale = dpi as f64 / 96.0;
            let width = (DISPLAY_WIDTH as f64 * scale).round() as i32;
            let height = (DISPLAY_HEIGHT as f64 * scale).round() as i32;

            // 1. 创建符合新大小的 Compatible Bitmap
            let hdc_screen = GetWindowDC(Some(HWND(std::ptr::null_mut())));
            let new_bitmap = CreateCompatibleBitmap(hdc_screen, width, height);
            let _ = ReleaseDC(Some(HWND(std::ptr::null_mut())), hdc_screen);

            // 2. 将新位图选入内存 DC，销毁旧位图
            if !new_bitmap.is_invalid() {
                let old_bitmap = SelectObject(self.hdc_mem, new_bitmap.into());
                let _ = DeleteObject(old_bitmap.into());
                self.hbitmap = new_bitmap;
            }

            // 3. 重新创建并选择字体（不设上限）
            let font_size = (FONT_BASE_SIZE as f64 * scale).round() as i32;
            let new_font = create_font(font_size);
            if !new_font.is_invalid() {
                let old_font = SelectObject(self.hdc_mem, new_font.into());
                let _ = DeleteObject(old_font.into());
                self.hfont = new_font;
            }

            // 4. 更新相关属性
            self.font_size = font_size;
            self.width = width;
            self.height = height;

            let _ = SetBkMode(self.hdc_mem, TRANSPARENT);

            let arrow_width = {
                let arrow_text = to_wide("\u{2191} ");
                let mut size = SIZE::default();
                let _ = GetTextExtentPoint32W(self.hdc_mem, &arrow_text[..arrow_text.len() - 1], &mut size);
                size.cx
            };
            self.arrow_width = arrow_width;
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
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
    unsafe {
        let mut lf = LOGFONTW {
            lfHeight: -size,
            lfWeight: 400,
            lfQuality: FONT_QUALITY(3), // NONANTIALIASED_QUALITY, 彻底斩断 Layered 窗口上 GDI 渲染的半透明粉红毛边
            ..Default::default()
        };
        let font_name: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
        lf.lfFaceName[..font_name.len()].copy_from_slice(&font_name);
        CreateFontIndirectW(&lf)
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn format_speed(bytes_per_sec: u32) -> String {
    if bytes_per_sec < 1024 {
        format!("{} B/s", bytes_per_sec)
    } else if bytes_per_sec < 1024 * 1024 {
        format!("{:.1} KB/s", bytes_per_sec as f64 / 1024.0)
    } else {
        format!("{:.1} MB/s", bytes_per_sec as f64 / (1024.0 * 1024.0))
    }
}

pub unsafe fn is_system_light_theme() -> bool {
    unsafe {
        use windows::Win32::System::Registry::{
            RegOpenKeyExW, RegQueryValueExW, RegCloseKey, HKEY_CURRENT_USER, KEY_READ,
        };
        use windows::core::PCWSTR;

        let key_path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0"
            .encode_utf16()
            .collect();
        let value_name: Vec<u16> = "SystemUsesLightTheme\0".encode_utf16().collect();
        let mut hkey = Default::default();

        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )
        .is_ok()
        {
            let mut value: u32 = 0;
            let mut value_size = std::mem::size_of::<u32>() as u32;
            let result = RegQueryValueExW(
                hkey,
                PCWSTR(value_name.as_ptr()),
                None,
                None,
                Some(&mut value as *mut u32 as *mut u8),
                Some(&mut value_size),
            );
            let _ = RegCloseKey(hkey);
            if result.is_ok() {
                return value == 1;
            }
        }
        false
    }
}