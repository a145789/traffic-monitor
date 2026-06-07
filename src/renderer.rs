use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW,
    CreateSolidBrush, DeleteDC, DeleteObject, FillRect, GetWindowDC, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, HDC, HFONT, HBITMAP, HGDIOBJ, HBRUSH,
    SRCCOPY, FONT_QUALITY, LOGFONTW, TRANSPARENT,
    DrawTextW, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER
};

use crate::config::{
    COLOR_DARK_TEXT, COLOR_KEY, COLOR_LIGHT_TEXT, COLOR_LOW_BATTERY, DISPLAY_HEIGHT,
    DISPLAY_WIDTH, FONT_BASE_SIZE, LOW_BATTERY_TEXT_X, MOUSE_ONLINE, CPU_USAGE, MEM_USAGE,
    NET_SPEED_DOWN, NET_SPEED_UP, MOUSE_BATTERY_LEVEL, MOUSE_DPI_VALUE, MOUSE_IS_CHARGING,
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
}

impl Renderer {
    pub fn new() -> Self {
        unsafe {
            let hdc_screen = GetWindowDC(HWND(std::ptr::null_mut()));
            let hdc_mem = CreateCompatibleDC(hdc_screen);
            let hbitmap = CreateCompatibleBitmap(hdc_screen, DISPLAY_WIDTH, DISPLAY_HEIGHT);
            let old_bitmap = SelectObject(hdc_mem, hbitmap);

            let hfont = create_font(FONT_BASE_SIZE);
            let old_font = SelectObject(hdc_mem, hfont);

            let hbrush = CreateSolidBrush(COLORREF(COLOR_KEY));

            let _ = SetBkMode(hdc_mem, TRANSPARENT);

            let _ = ReleaseDC(HWND(std::ptr::null_mut()), hdc_screen);

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
            let mouse_online = MOUSE_ONLINE.load(Ordering::Relaxed);
            let battery = MOUSE_BATTERY_LEVEL.load(Ordering::Relaxed);
            let charging = MOUSE_IS_CHARGING.load(Ordering::Relaxed);
            let dpi = MOUSE_DPI_VALUE.load(Ordering::Relaxed);

            let line1 = format!(
                "\u{2191} {}   CPU: {}%   MEM: {}%",
                format_speed(speed_up),
                cpu,
                mem
            );

            let line2 = if mouse_online {
                if battery < 20 && !charging {
                    format!(
                        "\u{2193} {}   \u{1F5B1}       DPI: {}",
                        format_speed(speed_down),
                        dpi
                    )
                } else {
                    format!(
                        "\u{2193} {}   \u{1F5B1} {}%    DPI: {}",
                        format_speed(speed_down),
                        battery,
                        dpi
                    )
                }
            } else {
                format!("\u{2193} {}   \u{1F5B1} --    DPI: --", format_speed(speed_down))
            };

            let half_height = self.height / 2;
            let scale = self.width as f64 / DISPLAY_WIDTH as f64;

            SetTextColor(self.hdc_mem, self.text_color);
            let mut rc1 = RECT {
                left: 4,
                top: 0,
                right: self.width,
                bottom: half_height,
            };
            let mut line1_wide = to_wide(&line1);
            let _ = DrawTextW(
                self.hdc_mem,
                &mut line1_wide,
                &mut rc1,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
            );
            
            let mut rc2 = RECT {
                left: 4,
                top: half_height,
                right: self.width,
                bottom: self.height,
            };
            let mut line2_wide = to_wide(&line2);
            let _ = DrawTextW(
                self.hdc_mem,
                &mut line2_wide,
                &mut rc2,
                DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
            );

            if mouse_online && battery < 20 && !charging {
                let battery_color = COLORREF(COLOR_LOW_BATTERY);
                SetTextColor(self.hdc_mem, battery_color);
                let battery_text = format!("{}%", battery);
                let mut rc_bat = RECT {
                    left: (LOW_BATTERY_TEXT_X * scale).round() as i32,
                    top: half_height,
                    right: self.width,
                    bottom: self.height,
                };
                let mut battery_wide = to_wide(&battery_text);
                let _ = DrawTextW(
                    self.hdc_mem,
                    &mut battery_wide,
                    &mut rc_bat,
                    DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
                );
            }

            SetTextColor(self.hdc_mem, self.text_color);

            let _ = BitBlt(
                hdc,
                0,
                0,
                self.width,
                self.height,
                self.hdc_mem,
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
            let hdc_screen = GetWindowDC(HWND(std::ptr::null_mut()));
            let new_bitmap = CreateCompatibleBitmap(hdc_screen, width, height);
            let _ = ReleaseDC(HWND(std::ptr::null_mut()), hdc_screen);

            // 2. 将新位图选入内存 DC，销毁旧位图
            if !new_bitmap.is_invalid() {
                let old_bitmap = SelectObject(self.hdc_mem, new_bitmap);
                let _ = DeleteObject(old_bitmap);
                self.hbitmap = new_bitmap;
            }

            // 3. 重新创建并选择字体（不设上限）
            let font_size = (FONT_BASE_SIZE as f64 * scale).round() as i32;
            let new_font = create_font(font_size);
            if !new_font.is_invalid() {
                let old_font = SelectObject(self.hdc_mem, new_font);
                let _ = DeleteObject(old_font);
                self.hfont = new_font;
            }

            // 4. 更新相关属性
            self.font_size = font_size;
            self.width = width;
            self.height = height;
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.hdc_mem, self.old_bitmap);
            let _ = SelectObject(self.hdc_mem, self.old_font);

            let _ = DeleteObject(self.hfont);
            let _ = DeleteObject(self.hbitmap);
            let _ = DeleteObject(self.hbrush);
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
    use windows::Win32::System::Registry::{
        RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ,
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
        0,
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
        let _ = windows::Win32::System::Registry::RegCloseKey(hkey);
        if result.is_ok() {
            return value == 1;
        }
    }
    false // 默认使用深色模式
}