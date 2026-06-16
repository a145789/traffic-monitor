use hidapi::{HidApi, HidDevice};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::thread;
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_USER};

use crate::config::{
    DPI_SCALE_FACTOR, HID_BATTERY_READ_TIMEOUT_MS, HID_CMD_SETTLE_MS, HID_DPI_READ_TIMEOUT_MS,
    HID_DPI_SYNC_SETTLE_MS, HID_DRAIN_MAX_ITERATIONS, MOUSE_BATTERY_LEVEL,
    MOUSE_BATTERY_WARMUP_SENTINEL, MOUSE_DPI_VALUE, MOUSE_FAIL_THRESHOLD,
    MOUSE_FAST_RETRY_INTERVAL, MOUSE_IS_CHARGING, MOUSE_ONLINE, MOUSE_PID,
    MOUSE_POLL_INTERVAL_OFFLINE, MOUSE_POLL_INTERVAL_ONLINE, MOUSE_STARTUP_GRACE_PERIOD_SECS,
    MOUSE_SUSPENDED_POLL_INTERVAL, MOUSE_THREAD_START_DELAY, MOUSE_USAGE, MOUSE_USAGE_PAGE,
    MOUSE_VIDS, MOUSE_WARMUP_POLL_INTERVAL, MOUSE_WARMUP_SUCCESS_THRESHOLD, SKIP_WARMUP, SUSPENDED,
    VID_WIRELESS,
};

pub const WM_USER_MOUSE_UPDATE: u32 = WM_USER + 1;
pub const WM_USER_MOUSE_STATUS: u32 = WM_USER + 2;

static FAIL_COUNT: AtomicU32 = AtomicU32::new(0);
static SHOULD_STOP: AtomicBool = AtomicBool::new(false);
static MAIN_HWND: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

const RESP_BATTERY: u8 = 0x30;
const RESP_DPI_MODE1: u8 = 0x61;
const RESP_DPI_MODE2: u8 = 0x60;

const OFFSET_RESPONSE_TYPE: usize = 1;
const OFFSET_BATTERY_LEVEL: usize = 8;
const OFFSET_BATTERY_CHARGING: usize = 9;
const OFFSET_DPI_ACTIVE_MODE: usize = 8;
const OFFSET_DPI_ACTIVE_STAGE: usize = 10;
const DPI_MODE1_OFFSET: usize = 11;
const DPI_MODE2_OFFSET: usize = 23;

const ACTIVE_MODE_1: u32 = 1;
const ACTIVE_MODE_2: u32 = 2;

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

struct FoundDevice {
    device: HidDevice,
    vid: u16,
}

pub fn init(hwnd: HWND) {
    MAIN_HWND.store(hwnd.0, Ordering::Relaxed);
}

pub fn start_mouse_thread() -> thread::JoinHandle<()> {
    SHOULD_STOP.store(false, Ordering::Release);
    FAIL_COUNT.store(0, Ordering::Relaxed);
    let skip_warmup = SKIP_WARMUP.swap(false, Ordering::AcqRel);
    thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            if !skip_warmup {
                interruptible_sleep(Duration::from_secs(MOUSE_THREAD_START_DELAY));
            }
            mouse_worker_loop(skip_warmup);
        })
        .expect("Failed to spawn mouse thread")
}

pub fn stop_mouse_thread() {
    SHOULD_STOP.store(true, Ordering::Release);
}

pub fn check_mouse_available() -> bool {
    match HidApi::new() {
        Ok(api) => !find_mouse_devices(&api).is_empty(),
        Err(_) => false,
    }
}

fn interruptible_sleep(dur: Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < dur {
        if SHOULD_STOP.load(Ordering::Acquire) {
            return;
        }
        let remaining = dur.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(Duration::from_millis(500)));
    }
}

struct MouseData {
    battery: Option<(u32, bool)>,
    dpi: Option<u32>,
}

fn poll_mouse() -> Result<MouseData, ()> {
    let api = HidApi::new().map_err(|_| ())?;
    let devices = find_mouse_devices(&api);
    if devices.is_empty() {
        return Err(());
    }

    let primary = &devices[0];

    // 电量和 DPI 独立查询，一个失败不影响另一个。
    let mut battery = query_mouse_battery(&primary.device).ok();

    // USB 充电 fallback：有线设备在充电时固件可能返回硬编码的 100%，
    // 若 2.4G 接收器同时在线，通过它获取真实电量。
    if let Some((level, true)) = battery
        && level >= 100
        && let Some(wireless) = devices.iter().find(|d| d.vid == VID_WIRELESS)
        && let Ok((real_level, _)) = query_mouse_battery(&wireless.device)
    {
        battery = Some((real_level, true));
    }

    let dpi = query_mouse_dpi(&primary.device).ok();

    // 电量和 DPI 均失败才视为整体失败（设备不可达）。
    if battery.is_none() && dpi.is_none() {
        return Err(());
    }

    Ok(MouseData { battery, dpi })
}

fn mouse_worker_loop(skip_warmup: bool) {
    // skip_warmup 时从 1 开始：第一次成功后 +1 变为 2，满足 > 1 的信任条件，
    // 从而跳过预热丢弃，立即展示电量数据。
    let warmup_base: u32 = if skip_warmup { 1 } else { 0 };
    let mut success_count: u32 = warmup_base;
    let start_time = std::time::Instant::now();
    loop {
        if SHOULD_STOP.load(Ordering::Acquire) {
            break;
        }

        if SUSPENDED.load(Ordering::Acquire) || crate::config::FULLSCREEN.load(Ordering::Acquire) {
            interruptible_sleep(Duration::from_secs(MOUSE_SUSPENDED_POLL_INTERVAL));
            success_count = warmup_base;
            continue;
        }

        match poll_mouse() {
            Ok(data) => {
                FAIL_COUNT.store(0, Ordering::Relaxed);
                MOUSE_ONLINE.store(true, Ordering::Relaxed);

                success_count = success_count.saturating_add(1);
                // 只有当 success_count > 1 时（即从第二次成功查询开始），才信任电量值，
                // 从而避开启动/唤醒后第一次查询可能得到的固件硬编码默认值（例如 80%）。
                // skip_warmup 场景下 success_count 从 1 起步，首次成功即满足 > 1。
                let trusted = success_count > 1;

                // 电量：仅在有数据时更新原子变量，DPI 失败不会清空已有电量。
                if let Some((level, charging)) = data.battery {
                    let display_level = if trusted {
                        level
                    } else {
                        MOUSE_BATTERY_WARMUP_SENTINEL
                    };
                    let display_charging = trusted && charging;
                    MOUSE_BATTERY_LEVEL.store(display_level, Ordering::Relaxed);
                    MOUSE_IS_CHARGING.store(display_charging, Ordering::Relaxed);
                }

                // DPI：仅在有数据时更新原子变量，电量失败不会清空已有 DPI。
                if let Some(dpi) = data.dpi {
                    MOUSE_DPI_VALUE.store(dpi, Ordering::Relaxed);
                }

                // 从原子变量读取当前值构造消息参数，保证 PostMessage 内容一致。
                let current_level = MOUSE_BATTERY_LEVEL.load(Ordering::Relaxed);
                let current_charging = MOUSE_IS_CHARGING.load(Ordering::Relaxed);
                let current_dpi = MOUSE_DPI_VALUE.load(Ordering::Relaxed);
                let display_lparam_level = if current_level == MOUSE_BATTERY_WARMUP_SENTINEL {
                    0
                } else {
                    current_level
                };
                let lparam = ((display_lparam_level & 0xFF) << 16) | (current_dpi & 0xFFFF);
                let wparam = current_charging as usize;
                let hwnd = HWND(MAIN_HWND.load(Ordering::Relaxed));
                // SAFETY:
                // hwnd 句柄是由主线程初始化并存储在原子指针中的有效窗口句柄。
                // PostMessageW 是线程安全的 Windows API，能安全地跨线程投递自定义的鼠标更新消息。
                unsafe {
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_USER_MOUSE_UPDATE,
                        WPARAM(wparam),
                        LPARAM(lparam as isize),
                    );
                }

                let sleep_secs = if !skip_warmup && success_count <= MOUSE_WARMUP_SUCCESS_THRESHOLD
                {
                    // 冷启动预热期采用短轮询，给刚唤醒的设备足够时间稳定数值。
                    MOUSE_WARMUP_POLL_INTERVAL
                } else {
                    MOUSE_POLL_INTERVAL_ONLINE
                };
                interruptible_sleep(Duration::from_secs(sleep_secs));
            }
            Err(()) => {
                let count = handle_mouse_offline();
                // 只有真正判定为离线（连续失败达到阈值）时才重置预热计数，
                // 避免单次偶发通信抖动导致电量显示重新闪回 "--"。
                if count >= MOUSE_FAIL_THRESHOLD {
                    success_count = warmup_base;
                }
                // 线程刚启动（如解锁、退出全屏）时 HID 栈可能尚未就绪，
                // 在初始化宽限期内即使失败也坚持快速重试，避免在系统就绪慢时误判离线而进入 300s 离线等待。
                let is_grace_period =
                    start_time.elapsed().as_secs() < MOUSE_STARTUP_GRACE_PERIOD_SECS;
                let retry_interval = if count >= MOUSE_FAIL_THRESHOLD && !is_grace_period {
                    MOUSE_POLL_INTERVAL_OFFLINE
                } else {
                    MOUSE_FAST_RETRY_INTERVAL
                };
                interruptible_sleep(Duration::from_secs(retry_interval));
            }
        }
    }
}

fn find_mouse_devices(api: &HidApi) -> Vec<FoundDevice> {
    let mut devices = Vec::new();
    for &target_vid in &MOUSE_VIDS {
        for device_info in api.device_list() {
            if device_info.vendor_id() == target_vid
                && device_info.product_id() == MOUSE_PID
                && device_info.usage_page() == MOUSE_USAGE_PAGE
                && device_info.usage() == MOUSE_USAGE
            {
                match device_info.open_device(api) {
                    Ok(dev) => {
                        if dev.set_blocking_mode(false).is_ok() {
                            devices.push(FoundDevice {
                                device: dev,
                                vid: target_vid,
                            });
                        }
                    }
                    Err(_) => continue,
                }
            }
        }
    }
    devices
}

fn query_mouse_battery(device: &HidDevice) -> Result<(u32, bool), ()> {
    // 循环非阻塞读取以彻底排空 HID 队列中积压的所有陈旧响应（例如系统挂起/恢复期间积压的电量报告），
    // 防止首次 read 命中缓存旧值导致显示错误电量。
    let mut stale = [0u8; 65];
    for _ in 0..HID_DRAIN_MAX_ITERATIONS {
        match device.read(&mut stale) {
            Ok(n) if n > 0 => {}
            _ => break,
        }
    }

    send_packet(device, &BATTERY_CMD)?;

    thread::sleep(Duration::from_millis(HID_CMD_SETTLE_MS as u64));

    let mut buf = [0u8; 65];
    match device.read_timeout(&mut buf, HID_BATTERY_READ_TIMEOUT_MS) {
        Ok(n) if n >= 10 => parse_battery_response(&buf[..n]),
        _ => Err(()),
    }
}

fn parse_battery_response(buf: &[u8]) -> Result<(u32, bool), ()> {
    let base = find_base_offset(buf).ok_or(())?;
    if buf.len() < base + 10 || buf[base + OFFSET_RESPONSE_TYPE] != RESP_BATTERY {
        return Err(());
    }
    let level = buf[base + OFFSET_BATTERY_LEVEL] as u32;
    let charging = buf[base + OFFSET_BATTERY_CHARGING] != 0;
    Ok((level.min(100), charging))
}

fn query_mouse_dpi(device: &HidDevice) -> Result<u32, ()> {
    let _ = send_packet(device, &DPI_SYNC_CMD);
    let mut dummy = [0u8; 65];
    for _ in 0..HID_DRAIN_MAX_ITERATIONS {
        match device.read(&mut dummy) {
            Ok(n) if n > 0 => {}
            _ => break,
        }
    }

    // 修复 DPI 异步时序回归：追加一次短 timeout 读取以丢弃因系统延迟可能较晚到达的 DPI_SYNC_CMD 响应包。
    let _ = device.read_timeout(&mut dummy, HID_DPI_SYNC_SETTLE_MS);

    send_packet(device, &DPI_CMD)?;

    thread::sleep(Duration::from_millis(HID_CMD_SETTLE_MS as u64));

    let mut buf = [0u8; 65];
    match device.read_timeout(&mut buf, HID_DPI_READ_TIMEOUT_MS) {
        Ok(n) if n >= 35 => parse_dpi_response(&buf[..n]),
        _ => Err(()),
    }
}

fn parse_dpi_response(buf: &[u8]) -> Result<u32, ()> {
    let base = find_base_offset(buf).ok_or(())?;
    if buf.len() < base + 35 {
        return Err(());
    }
    let d = &buf[base..];

    if d[OFFSET_RESPONSE_TYPE] != RESP_DPI_MODE1 && d[OFFSET_RESPONSE_TYPE] != RESP_DPI_MODE2 {
        return Err(());
    }

    let active_mode: u32 = if d[OFFSET_DPI_ACTIVE_MODE] == 0 {
        ACTIVE_MODE_2
    } else {
        ACTIVE_MODE_1
    };
    let stage_raw = d[OFFSET_DPI_ACTIVE_STAGE];
    if stage_raw == 0 {
        return Err(());
    }
    let active_stage = stage_raw as usize - 1;

    let raw_dpi = if active_mode == ACTIVE_MODE_1 {
        let offset = DPI_MODE1_OFFSET + active_stage * 2;
        if d.len() <= offset + 1 {
            return Err(());
        }
        (d[offset] as u16) | ((d[offset + 1] as u16) << 8)
    } else {
        let offset = DPI_MODE2_OFFSET + active_stage * 2;
        if d.len() <= offset + 1 {
            return Err(());
        }
        (d[offset] as u16) | ((d[offset + 1] as u16) << 8)
    };

    let mut dpi = (raw_dpi as f64 / DPI_SCALE_FACTOR).round() as u32;

    if active_mode == ACTIVE_MODE_2 {
        dpi = ((dpi as f64 / 100.0).round() as u32) * 100;
    }

    Ok(dpi)
}

fn handle_mouse_offline() -> u32 {
    let count = FAIL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

    if count >= MOUSE_FAIL_THRESHOLD {
        MOUSE_ONLINE.store(false, Ordering::Relaxed);
        MOUSE_BATTERY_LEVEL.store(0, Ordering::Relaxed);
        MOUSE_DPI_VALUE.store(0, Ordering::Relaxed);

        let hwnd = HWND(MAIN_HWND.load(Ordering::Relaxed));
        // SAFETY:
        // hwnd 句柄是由主线程初始化并存储在原子指针中的有效窗口句柄。
        // PostMessageW 是线程安全的 Windows API，能安全地跨线程投递自定义的鼠标离线状态消息。
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_USER_MOUSE_STATUS, WPARAM(0), LPARAM(0));
        }
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_base_offset() {
        assert_eq!(find_base_offset(&[0x55, 0x30]), Some(0));
        assert_eq!(find_base_offset(&[0xAA, 0x30]), Some(0));
        assert_eq!(find_base_offset(&[0x00, 0x55, 0x30]), Some(1));
        assert_eq!(find_base_offset(&[0x00, 0xAA, 0x30]), Some(1));
        assert_eq!(find_base_offset(&[0x00, 0x00, 0x55]), None);
        assert_eq!(find_base_offset(&[]), None);
    }

    #[test]
    fn test_parse_battery_response() {
        let buf = [0x55, RESP_BATTERY, 0, 0, 0, 0, 0, 0, 75, 1];
        assert_eq!(parse_battery_response(&buf), Ok((75, true)));

        let mut buf2 = buf;
        buf2[9] = 0;
        assert_eq!(parse_battery_response(&buf2), Ok((75, false)));

        let mut buf3 = buf;
        buf3[8] = 150;
        assert_eq!(parse_battery_response(&buf3), Ok((100, true)));

        let mut bad_type = buf;
        bad_type[1] = 0xFF;
        assert_eq!(parse_battery_response(&bad_type), Err(()));

        assert_eq!(
            parse_battery_response(&[0x55, RESP_BATTERY, 0, 0, 0, 0, 0, 0, 75]),
            Err(())
        );
    }

    #[test]
    fn test_parse_dpi_response_mode1() {
        let mut buf = [0u8; 35];
        buf[0] = 0x55;
        buf[1] = RESP_DPI_MODE1;
        buf[8] = 1;
        buf[10] = 1;
        // raw_dpi = 1173 => 1173 / 1.173 = 1000
        buf[11] = 0x95;
        buf[12] = 0x04;
        assert_eq!(parse_dpi_response(&buf), Ok(1000));
    }

    #[test]
    fn test_parse_dpi_response_mode2() {
        let mut buf = [0u8; 35];
        buf[0] = 0x55;
        buf[1] = RESP_DPI_MODE2;
        buf[8] = 0;
        buf[10] = 1;
        // raw_dpi = 1232 => 1232 / 1.173 ≈ 1050 => rounded to 1100
        buf[DPI_MODE2_OFFSET] = 0xD0;
        buf[DPI_MODE2_OFFSET + 1] = 0x04;
        assert_eq!(parse_dpi_response(&buf), Ok(1100));
    }

    #[test]
    fn test_parse_dpi_response_base_offset() {
        let mut buf = [0u8; 36];
        buf[1] = 0x55;
        buf[2] = RESP_DPI_MODE1;
        buf[9] = 1;
        buf[11] = 1;
        buf[12] = 0x95;
        buf[13] = 0x04;
        assert_eq!(parse_dpi_response(&buf), Ok(1000));
    }

    #[test]
    fn test_parse_dpi_response_errors() {
        // Too short
        assert_eq!(parse_dpi_response(&[0x55; 34]), Err(()));

        // Wrong response type
        let mut buf = [0u8; 35];
        buf[0] = 0x55;
        buf[1] = 0xFF;
        buf[8] = 1;
        buf[10] = 1;
        assert_eq!(parse_dpi_response(&buf), Err(()));

        // Stage 0
        let mut buf = [0u8; 35];
        buf[0] = 0x55;
        buf[1] = RESP_DPI_MODE1;
        buf[8] = 1;
        buf[10] = 0;
        assert_eq!(parse_dpi_response(&buf), Err(()));

        // Stage out of bounds: MODE2 stage_raw = 10 => active_stage = 9,
        // offset 23 + 9*2 = 41 exceeds the 35-byte buffer.
        let mut buf = [0u8; 35];
        buf[0] = 0x55;
        buf[1] = RESP_DPI_MODE2;
        buf[8] = 0;
        buf[10] = 10;
        assert_eq!(parse_dpi_response(&buf), Err(()));
    }
}
