use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::time::Instant;
use windows::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GetIfTable2, FreeMibTable, IP_ADAPTER_ADDRESSES_LH,
    MIB_IF_TABLE2, MIB_IF_ROW2, GET_ADAPTERS_ADDRESSES_FLAGS,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::System::SystemInformation::{
    GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, HWND, LPARAM, WPARAM};
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

static NET_INITIALIZED: AtomicBool = AtomicBool::new(false);

static MAIN_HWND_NETWORK: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

static INTERFACE_HISTORY: LazyLock<Mutex<HashMap<u64, (u64, u64)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static VIRTUAL_BLACKLIST: LazyLock<Mutex<Option<(Vec<u64>, Instant)>>> = LazyLock::new(|| Mutex::new(None));
const BLACKLIST_REFRESH_SECS: u64 = 30;

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

            let virtual_blacklist = get_virtual_blacklist();
            let mut current_data: HashMap<u64, (u64, u64)> = HashMap::new();
            let mut has_up_interface = false;

            for i in 0..num_entries {
                let row = &*row_ptr.add(i);

                if !is_valid_interface(row) {
                    continue;
                }

                let luid = row.InterfaceLuid.Value;
                if virtual_blacklist.contains(&luid) {
                    continue;
                }

                if row.OperStatus == IfOperStatusUp {
                    has_up_interface = true;
                    current_data.insert(luid, (row.InOctets, row.OutOctets));
                }
            }

            if !NET_INITIALIZED.load(Ordering::Acquire) {
                let mut history = INTERFACE_HISTORY.lock().unwrap();
                *history = current_data;
                NET_INITIALIZED.store(true, Ordering::Release);
                FreeMibTable(table as *const _);
                return;
            }

            let mut history = INTERFACE_HISTORY.lock().unwrap();

            let mut max_total: u64 = 0;
            let mut best_speed_down: u32 = 0;
            let mut best_speed_up: u32 = 0;

            for (luid, (in_octets, out_octets)) in &current_data {
                if let Some(&(prev_in, prev_out)) = history.get(luid) {
                    let speed_down = in_octets.saturating_sub(prev_in).min(u32::MAX as u64) as u32;
                    let speed_up = out_octets.saturating_sub(prev_out).min(u32::MAX as u64) as u32;
                    let total = speed_down as u64 + speed_up as u64;

                    if total > max_total {
                        max_total = total;
                        best_speed_down = speed_down;
                        best_speed_up = speed_up;
                    }
                }
            }

            *history = current_data;
            drop(history);

            NET_SPEED_DOWN.store(best_speed_down, Ordering::Release);
            NET_SPEED_UP.store(best_speed_up, Ordering::Release);

            if best_speed_down == 0 && best_speed_up == 0 && !has_up_interface {
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

            FreeMibTable(table as *const _);
        }
    }
}

fn is_valid_interface(row: &MIB_IF_ROW2) -> bool {
    let if_type = row.Type;
    if if_type != IF_TYPE_ETHERNET_CSMACD && if_type != IF_TYPE_IEEE80211 {
        return false;
    }

    if row.PhysicalAddressLength == 0 {
        return false;
    }

    // 注意：此处故意不检查 HardwareInterface 标志位。
    // 在 Hyper-V / WSL2 / Docker Desktop 环境下，物理网卡绑定到虚拟交换机后，
    // 外网流量实际由 vEthernet 等虚拟网口承载，其 HardwareInterface 为 false。
    // 若保留该检查，这些环境下网速将始终显示为 0。
    // 虚拟网口的过滤现已交由 is_virtual_friendly_name 黑名单完成。
    true
}

fn is_virtual_friendly_name(name: &str) -> bool {
    let name_lower = name.to_lowercase();
    name_lower.contains("virtual")
        || name_lower.contains("vbox")
        || name_lower.contains("vmware")
        || name_lower.contains("hyper-v")
        || name_lower.contains("wsl")
        || name_lower.contains("tap")
        || name_lower.contains("vpn")
        || name_lower.contains("loopback")
        || name_lower.contains("teredo")
        || name_lower.contains("isatap")
        || name_lower.contains("6to4")
        || name_lower.contains("ppp")
        || name_lower.contains("kvm")
        || name_lower.contains("xen")
}

unsafe fn read_wide_string(ptr: *mut u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0;
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }
}

unsafe fn build_virtual_blacklist() -> Option<Vec<u64>> {
    unsafe {
        let mut buf_size: u32 = 0;
        let ret = GetAdaptersAddresses(0, GET_ADAPTERS_ADDRESSES_FLAGS(0), None, None, &mut buf_size);
        if ret != ERROR_BUFFER_OVERFLOW.0 {
            return None;
        }

        let mut buf: Vec<u64> = vec![0u64; (buf_size as usize).div_ceil(8)];
        let adapter_ptr = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;

        let ret = GetAdaptersAddresses(0, GET_ADAPTERS_ADDRESSES_FLAGS(0), None, Some(adapter_ptr), &mut buf_size);
        if ret != 0 {
            return None;
        }

        let mut blacklist = Vec::new();
        let mut current = adapter_ptr;
        while !current.is_null() {
            let adapter = &*current;

            let friendly = read_wide_string(adapter.FriendlyName.0);
            let desc = read_wide_string(adapter.Description.0);

            if is_virtual_friendly_name(&friendly) || is_virtual_friendly_name(&desc) {
                blacklist.push(adapter.Luid.Value);
            }

            current = adapter.Next;
        }

        Some(blacklist)
    }
}

fn get_virtual_blacklist() -> Vec<u64> {
    {
        let cache = VIRTUAL_BLACKLIST.lock().unwrap();
        if let Some((list, last_refresh)) = cache.as_ref() {
            if last_refresh.elapsed().as_secs() < BLACKLIST_REFRESH_SECS {
                return list.clone();
            }
        }
    }

    let new_list = unsafe { build_virtual_blacklist() };
    match new_list {
        Some(list) => {
            *VIRTUAL_BLACKLIST.lock().unwrap() = Some((list.clone(), Instant::now()));
            list
        }
        None => {
            let cache = VIRTUAL_BLACKLIST.lock().unwrap();
            cache.as_ref().map(|(l, _)| l.clone()).unwrap_or_default()
        }
    }
}

pub fn trim_working_set() {
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}
