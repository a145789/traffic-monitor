use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::thread;
use std::time::Duration;
use hidapi::{HidApi, HidDevice};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_USER};

use crate::config::{
    DPI_SCALE_FACTOR, MOUSE_BATTERY_LEVEL, MOUSE_DPI_VALUE, MOUSE_FAIL_THRESHOLD, MOUSE_IS_CHARGING,
    MOUSE_ONLINE, MOUSE_PID, MOUSE_POLL_INTERVAL_OFFLINE, MOUSE_POLL_INTERVAL_ONLINE,
    MOUSE_USAGE, MOUSE_USAGE_PAGE, MOUSE_VIDS, SUSPENDED,
};

pub const WM_USER_MOUSE_UPDATE: u32 = WM_USER + 1;
pub const WM_USER_MOUSE_STATUS: u32 = WM_USER + 2;

static FAIL_COUNT: AtomicU32 = AtomicU32::new(0);
static SHOULD_STOP: AtomicBool = AtomicBool::new(false);
static MAIN_HWND: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

const BATTERY_CMD: [u8; 64] = {
    let mut cmd = [0u8; 64];
    cmd[0] = 0x55;
    cmd[1] = 0x30;
    cmd[2] = 0xA5;
    cmd[3] = 0x0B;
    cmd[4] = 0x2E;
    cmd[5] = 0x01;
    cmd[6] = 0x01;
    cmd[7] = 0x01;
    cmd
};

const DPI_SYNC_CMD: [u8; 64] = {
    let mut cmd = [0u8; 64];
    cmd[0] = 0x55;
    cmd[1] = 0xED;
    cmd
};

const DPI_CMD: [u8; 64] = {
    let mut cmd = [0u8; 64];
    cmd[0] = 0x55;
    cmd[1] = 0x61;
    cmd[2] = 0xA5;
    cmd[3] = 0x0B;
    cmd[4] = 0x1B;
    cmd[5] = 0x01;
    cmd[6] = 0x01;
    cmd[7] = 0x01;
    cmd
};

fn send_packet(device: &HidDevice, packet: &[u8; 64]) -> Result<(), ()> {
    if device.send_feature_report(packet).is_err() {
        let mut write_buf = [0u8; 65];
        write_buf[0] = 0x00;
        write_buf[1..65].copy_from_slice(packet);
        device.write(&write_buf).map_err(|_| ())?;
    }
    Ok(())
}

fn find_base_offset(buf: &[u8]) -> Option<usize> {
    if buf.is_empty() {
        return None;
    }
    if buf[0] == 0x55 || buf[0] == 0xAA {
        return Some(0);
    }
    if buf.len() > 1 && (buf[1] == 0x55 || buf[1] == 0xAA) {
        return Some(1);
    }
    None
}

pub fn init(hwnd: HWND) {
    MAIN_HWND.store(hwnd.0, Ordering::Relaxed);
}

pub fn start_mouse_thread() -> thread::JoinHandle<()> {
    SHOULD_STOP.store(false, Ordering::Release);
    thread::spawn(|| {
        interruptible_sleep(Duration::from_secs(2));
        mouse_worker_loop();
    })
}

pub fn stop_mouse_thread() {
    SHOULD_STOP.store(true, Ordering::Release);
}

pub fn check_mouse_available() -> bool {
    match HidApi::new() {
        Ok(api) => find_mouse_device(&api).is_some(),
        Err(_) => false,
    }
}

fn interruptible_sleep(dur: Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < dur {
        if SHOULD_STOP.load(Ordering::Relaxed) {
            return;
        }
        let remaining = dur.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(Duration::from_millis(500)));
    }
}

fn mouse_worker_loop() {
    let mut api_opt: Option<HidApi> = None;
    loop {
        if SHOULD_STOP.load(Ordering::Relaxed) {
            break;
        }

        if SUSPENDED.load(Ordering::Relaxed) || crate::config::FULLSCREEN.load(Ordering::Relaxed) {
            interruptible_sleep(Duration::from_secs(5));
            continue;
        }

        let api = match &api_opt {
            Some(api) => api,
            None => {
                match HidApi::new() {
                    Ok(api) => {
                        api_opt = Some(api);
                        api_opt.as_ref().unwrap()
                    }
                    Err(_) => {
                        interruptible_sleep(Duration::from_secs(MOUSE_POLL_INTERVAL_OFFLINE));
                        continue;
                    }
                }
            }
        };

        let device = match find_mouse_device(api) {
            Some(dev) => dev,
            None => {
                handle_mouse_offline();
                interruptible_sleep(Duration::from_secs(MOUSE_POLL_INTERVAL_OFFLINE));
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
                    let hwnd = HWND(MAIN_HWND.load(Ordering::Relaxed));
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_USER_MOUSE_UPDATE,
                        WPARAM(wparam),
                        LPARAM(lparam as isize),
                    );
                }
            }
            _ => {
                handle_mouse_offline();
                interruptible_sleep(Duration::from_secs(MOUSE_POLL_INTERVAL_OFFLINE));
                continue;
            }
        }

        interruptible_sleep(Duration::from_secs(MOUSE_POLL_INTERVAL_ONLINE));
    }
}

fn find_mouse_device(api: &HidApi) -> Option<HidDevice> {
    for device_info in api.device_list() {
        if MOUSE_VIDS.contains(&device_info.vendor_id())
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
    send_packet(device, &BATTERY_CMD)?;

    thread::sleep(Duration::from_millis(100));

    let mut buf = [0u8; 65];
    match device.read_timeout(&mut buf, 500) {
        Ok(n) if n >= 10 => {
            let base = find_base_offset(&buf[..n]).ok_or(())?;
            if buf.len() < base + 10 || buf[base + 1] != 0x30 {
                return Err(());
            }
            let level = buf[base + 8] as u32;
            let charging = buf[base + 9] != 0;
            Ok((level.min(100), charging))
        }
        _ => Err(()),
    }
}

fn query_mouse_dpi(device: &HidDevice) -> Result<u32, ()> {
    let _ = send_packet(device, &DPI_SYNC_CMD);
    let mut dummy = [0u8; 65];
    let _ = device.read_timeout(&mut dummy, 200);

    send_packet(device, &DPI_CMD)?;

    thread::sleep(Duration::from_millis(100));

    let mut buf = [0u8; 65];
    match device.read_timeout(&mut buf, 3000) {
        Ok(n) if n >= 35 => {
            let base = find_base_offset(&buf[..n]).ok_or(())?;
            if buf.len() < base + 35 {
                return Err(());
            }
            let d = &buf[base..];

            if d[1] != 0x61 && d[1] != 0x60 {
                return Err(());
            }

            let active_mode: u32 = if d[8] == 0 { 2 } else { 1 };
            let active_stage = (d[10] as usize).saturating_sub(1);

            let raw_dpi = if active_mode == 1 {
                let offset = 11 + active_stage * 2;
                (d[offset] as u16) | ((d[offset + 1] as u16) << 8)
            } else {
                let offset = 23 + active_stage * 2;
                (d[offset] as u16) | ((d[offset + 1] as u16) << 8)
            };

            let mut dpi = (raw_dpi as f64 / DPI_SCALE_FACTOR).round() as u32;

            if active_mode == 2 {
                dpi = ((dpi as f64 / 100.0).round() as u32) * 100;
            }

            Ok(dpi)
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
            let hwnd = HWND(MAIN_HWND.load(Ordering::Relaxed));
            let _ = PostMessageW(
                Some(hwnd),
                WM_USER_MOUSE_STATUS,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}