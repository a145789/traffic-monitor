use std::sync::atomic::Ordering;
use windows::Win32::NetworkManagement::IpHelper::{
    GetIfTable, MIB_IFROW, MIB_IFTABLE,
};
use windows::Win32::System::SystemInformation::{
    GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};

use crate::config::{CPU_USAGE, MEM_USAGE, NET_SPEED_DOWN, NET_SPEED_UP};

const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
const IF_TYPE_IEEE80211: u32 = 71;

static mut PREV_IDLE_TIME: u64 = 0;
static mut PREV_KERNEL_TIME: u64 = 0;
static mut PREV_USER_TIME: u64 = 0;
static mut PREV_NET_IN: u64 = 0;
static mut PREV_NET_OUT: u64 = 0;

pub fn collect_cpu() {
    unsafe {
        let mut idle_time = 0u64;
        let mut kernel_time = 0u64;
        let mut user_time = 0u64;

        if windows::Win32::System::Threading::GetSystemTimes(
            Some(&mut idle_time as *mut u64 as *mut _),
            Some(&mut kernel_time as *mut u64 as *mut _),
            Some(&mut user_time as *mut u64 as *mut _),
        )
        .is_ok()
        {
            let idle_diff = idle_time - PREV_IDLE_TIME;
            let kernel_diff = kernel_time - PREV_KERNEL_TIME;
            let user_diff = user_time - PREV_USER_TIME;
            let total = kernel_diff + user_diff;

            if total > 0 {
                let usage = ((total - idle_diff) * 100 / total) as u32;
                CPU_USAGE.store(usage.min(100), Ordering::Relaxed);
            }

            PREV_IDLE_TIME = idle_time;
            PREV_KERNEL_TIME = kernel_time;
            PREV_USER_TIME = user_time;
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
            MEM_USAGE.store(mem_info.dwMemoryLoad as u32, Ordering::Relaxed);
        }
    }
}

pub fn collect_network() {
    unsafe {
        let mut buf_size: u32 = 0;
        GetIfTable(None, &mut buf_size, false);

        if buf_size == 0 {
            return;
        }

        let mut buf = vec![0u8; buf_size as usize];
        let table = buf.as_mut_ptr() as *mut MIB_IFTABLE;

        let result = GetIfTable(Some(table), &mut buf_size, false);
        if result == 0 {
            let table_ref = &*table;
            let num_entries = table_ref.dwNumEntries as usize;
            let row_ptr = table_ref.table.as_ptr();

            let mut total_in: u64 = 0;
            let mut total_out: u64 = 0;

            for i in 0..num_entries {
                let row = &*row_ptr.add(i);

                if !is_physical_interface(row) {
                    continue;
                }

                if row.dwOperStatus != windows::Win32::NetworkManagement::IpHelper::INTERNAL_IF_OPER_STATUS(1) {
                    continue;
                }

                total_in += row.dwInOctets as u64;
                total_out += row.dwOutOctets as u64;
            }

            if total_in >= PREV_NET_IN && total_out >= PREV_NET_OUT {
                let speed_down = (total_in - PREV_NET_IN) as u32;
                let speed_up = (total_out - PREV_NET_OUT) as u32;
                NET_SPEED_DOWN.store(speed_down, Ordering::Relaxed);
                NET_SPEED_UP.store(speed_up, Ordering::Relaxed);
            } else {
                NET_SPEED_DOWN.store(0, Ordering::Relaxed);
                NET_SPEED_UP.store(0, Ordering::Relaxed);
            }

            PREV_NET_IN = total_in;
            PREV_NET_OUT = total_out;
        }
    }
}

fn is_physical_interface(row: &MIB_IFROW) -> bool {
    let if_type = row.dwType;
    if if_type != IF_TYPE_ETHERNET_CSMACD && if_type != IF_TYPE_IEEE80211 {
        return false;
    }

    if row.dwPhysAddrLen == 0 {
        return false;
    }

    true
}

pub fn trim_working_set() {
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}
