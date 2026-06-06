use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::Duration;
use hidapi::{HidApi, HidDevice};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_USER};

use crate::config::{
    MOUSE_BATTERY_LEVEL, MOUSE_DPI_VALUE, MOUSE_FAIL_THRESHOLD, MOUSE_IS_CHARGING,
    MOUSE_ONLINE, MOUSE_PID, MOUSE_POLL_INTERVAL_OFFLINE, MOUSE_POLL_INTERVAL_ONLINE,
    MOUSE_USAGE, MOUSE_USAGE_PAGE, MOUSE_VID, SUSPENDED,
};

pub const WM_USER_MOUSE_UPDATE: u32 = WM_USER + 1;
pub const WM_USER_MOUSE_STATUS: u32 = WM_USER + 2;

static FAIL_COUNT: AtomicU32 = AtomicU32::new(0);
static SHOULD_STOP: AtomicBool = AtomicBool::new(false);
static mut MAIN_HWND: HWND = HWND(std::ptr::null_mut());

const MOUSE_HID_CMD_BATTERY: [u8; 64] = {
    let mut cmd = [0u8; 64];
    cmd[0] = 0x01;
    cmd[1] = 0x0A;
    cmd
};

const MOUSE_HID_CMD_DPI: [u8; 64] = {
    let mut cmd = [0u8; 64];
    cmd[0] = 0x01;
    cmd[1] = 0x0E;
    cmd
};

pub fn init(hwnd: HWND) {
    unsafe {
        MAIN_HWND = hwnd;
    }
}

pub fn start_mouse_thread() -> thread::JoinHandle<()> {
    SHOULD_STOP.store(false, Ordering::Relaxed);
    thread::spawn(|| {
        mouse_worker_loop();
    })
}

fn mouse_worker_loop() {
    let api = match HidApi::new() {
        Ok(api) => api,
        Err(_) => return,
    };

    loop {
        if SHOULD_STOP.load(Ordering::Relaxed) {
            break;
        }

        if SUSPENDED.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_secs(5));
            continue;
        }

        let device = match find_mouse_device(&api) {
            Some(dev) => dev,
            None => {
                handle_mouse_offline();
                thread::sleep(Duration::from_secs(MOUSE_POLL_INTERVAL_OFFLINE));
                continue;
            }
        };

        let battery_result = query_mouse_battery(&device);
        let dpi_result = query_mouse_dpi(&device);

        match (battery_result, dpi_result) {
            (Ok((level, charging)), Ok(dpi)) => {
                MOUSE_BATTERY_LEVEL.store(level, Ordering::Relaxed);
                MOUSE_IS_CHARGING.store(charging, Ordering::Relaxed);
                MOUSE_DPI_VALUE.store(dpi, Ordering::Relaxed);

                FAIL_COUNT.store(0, Ordering::Relaxed);
                MOUSE_ONLINE.store(true, Ordering::Relaxed);

                unsafe {
                    let lparam = ((level & 0xFF) << 16) | (dpi & 0xFFFF);
                    let wparam = charging as usize;
                    let _ = PostMessageW(
                        MAIN_HWND,
                        WM_USER_MOUSE_UPDATE,
                        WPARAM(wparam),
                        LPARAM(lparam as isize),
                    );
                }
            }
            _ => {
                handle_mouse_offline();
                thread::sleep(Duration::from_secs(MOUSE_POLL_INTERVAL_OFFLINE));
                continue;
            }
        }

        thread::sleep(Duration::from_secs(MOUSE_POLL_INTERVAL_ONLINE));
    }
}

fn find_mouse_device(api: &HidApi) -> Option<HidDevice> {
    for device_info in api.device_list() {
        if device_info.vendor_id() == MOUSE_VID
            && device_info.product_id() == MOUSE_PID
            && device_info.usage_page() == MOUSE_USAGE_PAGE
            && device_info.usage() == MOUSE_USAGE
        {
            match device_info.open_device(api) {
                Ok(dev) => {
                    dev.set_blocking_mode(false).ok()?;
                    return Some(dev);
                }
                Err(_) => continue,
            }
        }
    }
    None
}

fn query_mouse_battery(device: &HidDevice) -> Result<(u32, bool), ()> {
    device.write(&MOUSE_HID_CMD_BATTERY).map_err(|_| ())?;

    thread::sleep(Duration::from_millis(100));

    let mut buf = [0u8; 64];
    match device.read_timeout(&mut buf, 500) {
        Ok(n) if n >= 3 => {
            let level = buf[2] as u32;
            let charging = buf[3] != 0;
            Ok((level.min(100), charging))
        }
        _ => Err(()),
    }
}

fn query_mouse_dpi(device: &HidDevice) -> Result<u32, ()> {
    device.write(&MOUSE_HID_CMD_DPI).map_err(|_| ())?;

    thread::sleep(Duration::from_millis(100));

    let mut buf = [0u8; 64];
    match device.read_timeout(&mut buf, 500) {
        Ok(n) if n >= 3 => {
            let dpi_raw = ((buf[2] as u32) << 8) | (buf[3] as u32);
            Ok(dpi_raw)
        }
        _ => Err(()),
    }
}

fn handle_mouse_offline() {
    let count = FAIL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

    if count >= MOUSE_FAIL_THRESHOLD {
        MOUSE_ONLINE.store(false, Ordering::Relaxed);
        MOUSE_BATTERY_LEVEL.store(0, Ordering::Relaxed);
        MOUSE_DPI_VALUE.store(0, Ordering::Relaxed);

        unsafe {
            let _ = PostMessageW(
                MAIN_HWND,
                WM_USER_MOUSE_STATUS,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

pub fn stop_mouse_thread() {
    SHOULD_STOP.store(true, Ordering::Relaxed);
}
