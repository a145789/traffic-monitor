use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW,
    CreateSolidBrush, DeleteDC, DeleteObject, FillRect, GetWindowDC, GetPixel, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, TextOutW, HDC, HFONT, HBITMAP,
    SRCCOPY, FONT_QUALITY, LOGFONTW, TRANSPARENT,
};

use crate::config::{
    COLOR_DARK_TEXT, COLOR_KEY, COLOR_LIGHT_TEXT, COLOR_LOW_BATTERY, DISPLAY_HEIGHT,
    DISPLAY_WIDTH, FONT_BASE_SIZE, LUMINANCE_THRESHOLD, MOUSE_ONLINE, CPU_USAGE, MEM_USAGE,
    NET_SPEED_DOWN, NET_SPEED_UP, MOUSE_BATTERY_LEVEL, MOUSE_DPI_VALUE, MOUSE_IS_CHARGING,
};

pub struct Renderer {
    hdc_mem: HDC,
    hbitmap: HBITMAP,
    hfont: HFONT,
    text_color: COLORREF,
}

impl Renderer {
    pub fn new() -> Self {
        unsafe {
            let hdc_screen = GetWindowDC(HWND(std::ptr::null_mut()));
            let hdc_mem = CreateCompatibleDC(hdc_screen);
            let hbitmap = CreateCompatibleBitmap(hdc_screen, DISPLAY_WIDTH, DISPLAY_HEIGHT);
            SelectObject(hdc_mem, hbitmap);

            let hfont = create_font(FONT_BASE_SIZE);
            SelectObject(hdc_mem, hfont);

            SetBkMode(hdc_mem, TRANSPARENT);

            ReleaseDC(HWND(std::ptr::null_mut()), hdc_screen);

            Self {
                hdc_mem,
                hbitmap,
                hfont,
                text_color: COLORREF(COLOR_LIGHT_TEXT),
            }
        }
    }

    pub fn update_text_color(&mut self, tray_hwnd: HWND, taskbar_hwnd: HWND) {
        unsafe {
            let hdc = GetWindowDC(taskbar_hwnd);
            let mut tray_rc = RECT::default();
            let mut taskbar_rc = RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(tray_hwnd, &mut tray_rc);
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(taskbar_hwnd, &mut taskbar_rc);
            let pixel_x = tray_rc.left - 10 - taskbar_rc.left;
            let pixel_y = (taskbar_rc.bottom - taskbar_rc.top) / 2;
            let color = GetPixel(hdc, pixel_x, pixel_y);
            ReleaseDC(taskbar_hwnd, hdc);

            let r = (color.0 & 0xFF) as f64;
            let g = ((color.0 >> 8) & 0xFF) as f64;
            let b = ((color.0 >> 16) & 0xFF) as f64;
            let luminance = 0.299 * r + 0.587 * g + 0.114 * b;

            self.text_color = if luminance > LUMINANCE_THRESHOLD {
                COLORREF(COLOR_DARK_TEXT)
            } else {
                COLORREF(COLOR_LIGHT_TEXT)
            };
        }
    }

    pub fn render(&self, hdc: HDC) {
        unsafe {
            let rect = RECT {
                left: 0,
                top: 0,
                right: DISPLAY_WIDTH,
                bottom: DISPLAY_HEIGHT,
            };

            let hbrush = CreateSolidBrush(COLORREF(COLOR_KEY));
            FillRect(self.hdc_mem, &rect, hbrush);
            DeleteObject(hbrush);

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
                        "\u{2193} {}   --     DPI: {}",
                        format_speed(speed_down),
                        dpi
                    )
                } else {
                    format!(
                        "\u{2193} {}   {}%     DPI: {}",
                        format_speed(speed_down),
                        battery,
                        dpi
                    )
                }
            } else {
                format!("\u{2193} {}   --     DPI: --", format_speed(speed_down))
            };

            SetTextColor(self.hdc_mem, self.text_color);
            TextOutW(self.hdc_mem, 4, 2, &to_wide(&line1));
            TextOutW(self.hdc_mem, 4, 16, &to_wide(&line2));

            if mouse_online && battery < 20 && !charging {
                let battery_color = COLORREF(COLOR_LOW_BATTERY);
                SetTextColor(self.hdc_mem, battery_color);
                let battery_text = format!("{}%", battery);
                TextOutW(self.hdc_mem, 110, 16, &to_wide(&battery_text));
            }

            SetTextColor(self.hdc_mem, self.text_color);

            BitBlt(
                hdc,
                0,
                0,
                DISPLAY_WIDTH,
                DISPLAY_HEIGHT,
                self.hdc_mem,
                0,
                0,
                SRCCOPY,
            );
        }
    }

    pub fn recreate_font(&mut self, hwnd: HWND) {
        unsafe {
            let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd);
            let font_size = (FONT_BASE_SIZE as f64 * dpi as f64 / 96.0).round() as i32;
            let new_font = create_font(font_size);
            SelectObject(self.hdc_mem, new_font);
            DeleteObject(self.hfont);
            self.hfont = new_font;
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.hfont);
            DeleteObject(self.hbitmap);
            DeleteDC(self.hdc_mem);
        }
    }
}

fn create_font(size: i32) -> HFONT {
    unsafe {
        let mut lf = LOGFONTW {
            lfHeight: size,
            lfWeight: 400,
            lfQuality: FONT_QUALITY(4),
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