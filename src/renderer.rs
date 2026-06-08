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
use crate::ffi_guard::RegKey;

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
        // SAFETY:
        // 传入 null_mut 句柄用于获取整个主屏幕的临时设备上下文句柄（HDC）。
        let hdc_screen = unsafe { GetWindowDC(Some(HWND(std::ptr::null_mut()))) };
        
        // SAFETY:
        // hdc_screen 是有效的屏幕设备上下文句柄，由系统临时分配。
        let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
        
        // SAFETY:
        // hdc_screen 是有效句柄。创建与 hdc_screen 格式兼容的 HBITMAP 资源。
        let hbitmap = unsafe { CreateCompatibleBitmap(hdc_screen, DISPLAY_WIDTH, DISPLAY_HEIGHT) };
        
        // SAFETY:
        // hdc_mem 是有效的内存上下文句柄，hbitmap 是有效的位图句柄。
        // 将新位图选入设备上下文并返回旧有的 GDI 备份对象。
        let old_bitmap = unsafe { SelectObject(hdc_mem, hbitmap.into()) };

        let hfont = create_font(FONT_BASE_SIZE);
        
        // SAFETY:
        // 将有效的新字体对象选入内存上下文中。
        let old_font = unsafe { SelectObject(hdc_mem, hfont.into()) };

        // SAFETY:
        // 创建指定背景透明的纯色刷子对象。
        let hbrush = unsafe { CreateSolidBrush(COLORREF(COLOR_KEY)) };

        // SAFETY:
        // 设置指定内存上下文的背景混合模式为透明模式。
        unsafe {
            let _ = SetBkMode(hdc_mem, TRANSPARENT);
        }

        let arrow_width = {
            let arrow_text = to_wide("\u{2191} ");
            let mut size = SIZE::default();
            // SAFETY:
            // hdc_mem 是有效内存上下文，arrow_text 是以 NUL 结尾的有效宽字符数组。
            // 写入 size 是合法的栈内存地址，操作在其调用期间安全。
            unsafe {
                let _ = GetTextExtentPoint32W(hdc_mem, &arrow_text[..arrow_text.len() - 1], &mut size);
            }
            size.cx
        };

        // SAFETY:
        // 释放先前由 GetWindowDC 获取的屏幕设备上下文。
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

    pub fn render(&self, hdc: HDC) {
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

        // 1. 绘制第三列 (网速) - 最右列
        // 箭头左对齐，数值右对齐 — 表格效果
        let col_gap = (13.0 * scale).round() as i32;
        let speed_right = self.width - (4.0 * scale).round() as i32;
        let speed_left = speed_right - (76.0 * scale).round() as i32;
        let arrow_right = speed_left + self.arrow_width;

        // SAFETY:
        // 在这里我们对当前结构体持有的有效句柄进行 GDI 渲染。
        // hdc_mem 和 hbrush 均在所有者生存期内保证合法。
        // 调用 DrawTextW 传入的 RECT 指针和 wide 字符切片均在当前栈帧内分配且有效。
        // 目标句柄 hdc 是由 Windows 传入的有效设备上下文。
        unsafe {
            let _ = FillRect(self.hdc_mem, &rect, self.hbrush);
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
        // SAFETY: hwnd 是在当前进程上下文中有效且处于活动状态的窗口句柄，调用 GetDpiForWindow 是安全的，无跨进程非法访问问题。
        let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
        let scale = dpi as f64 / 96.0;
        let width = (DISPLAY_WIDTH as f64 * scale).round() as i32;
        let height = (DISPLAY_HEIGHT as f64 * scale).round() as i32;

        // 1. 创建符合新大小的 Compatible Bitmap
        // SAFETY: 传入 NULL 句柄给 GetWindowDC 用于临时获取整个主屏幕屏幕设备的 HDC 上下文，这在 GDI 绘图中是标准且安全的常规操作。
        let hdc_screen = unsafe { GetWindowDC(Some(HWND(std::ptr::null_mut()))) };
        
        // SAFETY: hdc_screen 是先前成功获取的屏幕设备有效 HDC，使用其兼容格式和合理宽高参数创建位图，不会引发越界分配。
        let new_bitmap = unsafe { CreateCompatibleBitmap(hdc_screen, width, height) };
        
        // SAFETY: 临时获取的屏幕 hdc_screen 不再需要使用，调用 ReleaseDC 归还系统资源以防止 HDC 资源泄露。
        unsafe {
            let _ = ReleaseDC(Some(HWND(std::ptr::null_mut())), hdc_screen);
        }

        // 2. 将新位图选入内存 DC，销毁旧位图
        if !new_bitmap.is_invalid() {
            // SAFETY: self.hdc_mem 是在 Renderer 初始化时成功创建的内存设备上下文，new_bitmap 是通过 CreateCompatibleBitmap 分配成功的有效句柄，在此将新位图选入设备上下文是内存安全的。
            let old_bitmap = unsafe { SelectObject(self.hdc_mem, new_bitmap.into()) };
            // SAFETY: 选出内存上下文的旧位图 old_bitmap 不再需要使用，调用 DeleteObject 释放其底层 GDI 资源是内存安全的。
            unsafe {
                let _ = DeleteObject(old_bitmap.into());
            }
            self.hbitmap = new_bitmap;
        }

        // 3. 重新创建并选择字体（不设上限）
        let font_size = (FONT_BASE_SIZE as f64 * scale).round() as i32;
        let new_font = create_font(font_size);
        if !new_font.is_invalid() {
            // SAFETY: new_font 是新分配的合法字体对象，选入当前内存设备上下文 self.hdc_mem 不会发生资源冲突。
            let old_font = unsafe { SelectObject(self.hdc_mem, new_font.into()) };
            // SAFETY: 替换出的旧字体对象已无其他地方引用，在此销毁它以防止内存和 GDI 资源泄漏。
            unsafe {
                let _ = DeleteObject(old_font.into());
            }
            self.hfont = new_font;
        }

        // 4. 更新相关属性
        self.font_size = font_size;
        self.width = width;
        self.height = height;

        // SAFETY: self.hdc_mem 为当前实例中持有的有效内存设备上下文，设置其背景模式为 TRANSPARENT 可使得 TextOut 等绘制不填充背景，是无副作用的安全调用。
        unsafe {
            let _ = SetBkMode(self.hdc_mem, TRANSPARENT);
        }

        let arrow_width = {
            let arrow_text = to_wide("\u{2191} ");
            let mut size = SIZE::default();
            // SAFETY:
            // hdc_mem 是有效内存上下文，arrow_text 是以 NUL 结尾的有效宽字符数组。
            // 写入 size 是合法的栈内存地址，操作在其调用期间安全。
            unsafe {
                let _ = GetTextExtentPoint32W(self.hdc_mem, &arrow_text[..arrow_text.len() - 1], &mut size);
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


pub fn is_system_light_theme() -> bool {
    use windows::Win32::System::Registry::{
        RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ,
    };
    use windows::core::PCWSTR;

    let key_path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0"
        .encode_utf16()
        .collect();
    let value_name: Vec<u16> = "SystemUsesLightTheme\0".encode_utf16().collect();
    let mut hkey = Default::default();

    // SAFETY:
    // 1. HKEY_CURRENT_USER 是预定义且系统保证有效的注册表根键。
    // 2. key_path 已转换为以 NUL 结尾的 UTF-16 宽字符数组，确保传入 RegOpenKeyExW 的路径在调用期间内存合法。
    // 3. 栈上 hkey 变量的地址合法，成功打开的句柄将通过 RegKey 进行 RAII 自动生命周期释放。
    let open_ok = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )
        .is_ok()
    };

    if open_ok {
        let _key_guard = RegKey(hkey);
        let mut value: u32 = 0;
        let mut value_size = std::mem::size_of::<u32>() as u32;

        // SAFETY:
        // 1. hkey 是先前成功打开的合法键句柄。
        // 2. value_name 指向以 NUL 结尾的合法宽字符数组指针。
        // 3. value_size 初始化为 value (u32) 的大小 4 字节，系统在写入时不会发生缓冲区溢出，内存访问是完全合法的。
        let query_ok = unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR(value_name.as_ptr()),
                None,
                None,
                Some(&mut value as *mut u32 as *mut u8),
                Some(&mut value_size),
            )
            .is_ok()
        };

        if query_ok {
            return value == 1;
        }
    }
    false
}