use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use windows::Win32::NetworkManagement::IpHelper::{
    GetIfTable2, FreeMibTable, MIB_IF_TABLE2, MIB_IF_ROW2,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::System::SystemInformation::{
    GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_USER};

use crate::config::{
    CPU_USAGE, MEM_USAGE, NET_SPEED_DOWN, NET_SPEED_UP,
    BACKOFF_ZERO_THRESHOLD, NETWORK_BACKOFF, CONSECUTIVE_ZERO_COUNT,
};

pub const WM_USER_NETWORK_DISCONNECTED: u32 = WM_USER + 3;
pub const WM_USER_NETWORK_RECONNECTED: u32 = WM_USER + 4;

const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
const IF_TYPE_IEEE80211: u32 = 71;

static PREV_IDLE_TIME: AtomicU64 = AtomicU64::new(0);
static PREV_KERNEL_TIME: AtomicU64 = AtomicU64::new(0);
static PREV_USER_TIME: AtomicU64 = AtomicU64::new(0);
static CPU_INITIALIZED: AtomicBool = AtomicBool::new(false);

static PREV_NET_IN: AtomicU64 = AtomicU64::new(0);
static PREV_NET_OUT: AtomicU64 = AtomicU64::new(0);
static NET_INITIALIZED: AtomicBool = AtomicBool::new(false);

static MAIN_HWND_NETWORK: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

pub fn init_network_listener(hwnd: HWND) {
    MAIN_HWND_NETWORK.store(hwnd.0, Ordering::Release);
}

pub fn collect_cpu() {
    let mut idle_time = 0u64;
    let mut kernel_time = 0u64;
    let mut user_time = 0u64;

    unsafe {
        if windows::Win32::System::Threading::GetSystemTimes(
            Some(&mut idle_time as *mut u64 as *mut _),
            Some(&mut kernel_time as *mut u64 as *mut _),
            Some(&mut user_time as *mut u64 as *mut _),
        )
        .is_ok()
        {
            if !CPU_INITIALIZED.load(Ordering::Acquire) {
                PREV_IDLE_TIME.store(idle_time, Ordering::Release);
                PREV_KERNEL_TIME.store(kernel_time, Ordering::Release);
                PREV_USER_TIME.store(user_time, Ordering::Release);
                CPU_INITIALIZED.store(true, Ordering::Release);
                return;
            }

            let idle_diff = idle_time.saturating_sub(PREV_IDLE_TIME.load(Ordering::Acquire));
            let kernel_diff = kernel_time.saturating_sub(PREV_KERNEL_TIME.load(Ordering::Acquire));
            let user_diff = user_time.saturating_sub(PREV_USER_TIME.load(Ordering::Acquire));
            let total = kernel_diff + user_diff;

            if total > 0 {
                let usage = ((total - idle_diff) * 100 / total) as u32;
                CPU_USAGE.store(usage.min(100), Ordering::Release);
            }

            PREV_IDLE_TIME.store(idle_time, Ordering::Release);
            PREV_KERNEL_TIME.store(kernel_time, Ordering::Release);
            PREV_USER_TIME.store(user_time, Ordering::Release);
        }
    }
}

pub fn collect_memory() {
    unsafe {
        let mut mem_info = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };

        if GlobalMemoryStatusEx(&mut mem_info).is_ok() {
            MEM_USAGE.store(mem_info.dwMemoryLoad as u32, Ordering::Release);
        }
    }
}

pub fn collect_network() {
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        let result = GetIfTable2(&mut table);
        if result.0 == 0 && !table.is_null() {
            let table_ref = &*table;
            let num_entries = table_ref.NumEntries as usize;
            let row_ptr = table_ref.Table.as_ptr();

            let mut total_in: u64 = 0;
            let mut total_out: u64 = 0;
            let mut has_up_interface = false;

            for i in 0..num_entries {
                let row = &*row_ptr.add(i);

                if !is_valid_interface(row) {
                    continue;
                }

                if row.OperStatus == IfOperStatusUp {
                    has_up_interface = true;
                    total_in += row.InOctets;
                    total_out += row.OutOctets;
                }
            }

            if !NET_INITIALIZED.load(Ordering::Acquire) {
                PREV_NET_IN.store(total_in, Ordering::Release);
                PREV_NET_OUT.store(total_out, Ordering::Release);
                NET_INITIALIZED.store(true, Ordering::Release);
                FreeMibTable(table as *const _);
                return;
            }

            let speed_down = total_in.saturating_sub(PREV_NET_IN.load(Ordering::Acquire)).min(u32::MAX as u64) as u32;
            let speed_up = total_out.saturating_sub(PREV_NET_OUT.load(Ordering::Acquire)).min(u32::MAX as u64) as u32;

            NET_SPEED_DOWN.store(speed_down, Ordering::Release);
            NET_SPEED_UP.store(speed_up, Ordering::Release);

            if speed_down == 0 && speed_up == 0 && !has_up_interface {
                let count = CONSECUTIVE_ZERO_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                if count >= BACKOFF_ZERO_THRESHOLD && !NETWORK_BACKOFF.load(Ordering::Acquire) {
                    NETWORK_BACKOFF.store(true, Ordering::Release);
                    let hwnd = HWND(MAIN_HWND_NETWORK.load(Ordering::Acquire));
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_USER_NETWORK_DISCONNECTED,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            } else {
                CONSECUTIVE_ZERO_COUNT.store(0, Ordering::Release);
                if NETWORK_BACKOFF.load(Ordering::Acquire) {
                    NETWORK_BACKOFF.store(false, Ordering::Release);
                    let hwnd = HWND(MAIN_HWND_NETWORK.load(Ordering::Acquire));
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_USER_NETWORK_RECONNECTED,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }

            PREV_NET_IN.store(total_in, Ordering::Release);
            PREV_NET_OUT.store(total_out, Ordering::Release);

            FreeMibTable(table as *const _);
        }
    }
}

// 注意：此处故意不检查 HardwareInterface 标志位。
// 在 Hyper-V / WSL2 / Docker Desktop 环境下，物理网卡绑定到虚拟交换机后，
// 外网流量实际由 vEthernet 等虚拟网口承载，其 HardwareInterface 为 false。
// 若保留该检查，这些环境下网速将始终显示为 0。
// 因此仅保留接口类型（Ethernet / Wi-Fi）和 PhysicalAddressLength > 0 的过滤。
fn is_valid_interface(row: &MIB_IF_ROW2) -> bool {
    let if_type = row.Type;
    if if_type != IF_TYPE_ETHERNET_CSMACD && if_type != IF_TYPE_IEEE80211 {
        return false;
    }

    if row.PhysicalAddressLength == 0 {
        return false;
    }

    true
}

pub fn trim_working_set() {
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}
