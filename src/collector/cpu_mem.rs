//! CPU 与内存使用率采集。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

use crate::state::{CPU_USAGE, MEM_USAGE};

static PREV_IDLE_TIME: AtomicU64 = AtomicU64::new(0);
static PREV_KERNEL_TIME: AtomicU64 = AtomicU64::new(0);
static PREV_USER_TIME: AtomicU64 = AtomicU64::new(0);
static CPU_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// 采样 `GetSystemTimes`，更新 `CPU_USAGE`。
///
/// 由 `TIMER_ID_CPU_MEM` 调用；首轮仅建立基线，不产生有效差分。
/// API 调用失败或本周期差分为 0 时保持上次展示值不变。
pub fn collect_cpu() {
    let mut idle_time = 0u64;
    let mut kernel_time = 0u64;
    let mut user_time = 0u64;

    // SAFETY: 传入的指针均指向当前栈帧的有效可变 u64；API 仅在调用期间写入。
    let ok = unsafe {
        windows::Win32::System::Threading::GetSystemTimes(
            Some(&mut idle_time as *mut u64 as *mut _),
            Some(&mut kernel_time as *mut u64 as *mut _),
            Some(&mut user_time as *mut u64 as *mut _),
        )
        .is_ok()
    };

    if !ok {
        return;
    }

    if !CPU_INITIALIZED.load(Ordering::Acquire) {
        PREV_IDLE_TIME.store(idle_time, Ordering::Relaxed);
        PREV_KERNEL_TIME.store(kernel_time, Ordering::Relaxed);
        PREV_USER_TIME.store(user_time, Ordering::Relaxed);
        CPU_INITIALIZED.store(true, Ordering::Release);
        return;
    }

    let idle_diff = idle_time.saturating_sub(PREV_IDLE_TIME.load(Ordering::Relaxed));
    let kernel_diff = kernel_time.saturating_sub(PREV_KERNEL_TIME.load(Ordering::Relaxed));
    let user_diff = user_time.saturating_sub(PREV_USER_TIME.load(Ordering::Relaxed));

    PREV_IDLE_TIME.store(idle_time, Ordering::Relaxed);
    PREV_KERNEL_TIME.store(kernel_time, Ordering::Relaxed);
    PREV_USER_TIME.store(user_time, Ordering::Relaxed);

    // GetSystemTimes 的 kernel 时间包含 idle，total = kernel + user 为全部时钟滴答。
    let total = kernel_diff + user_diff;
    if total == 0 {
        return;
    }

    let usage = ((total - idle_diff) * 100 / total).min(100) as u32;
    CPU_USAGE.store(usage, Ordering::Relaxed);
}

pub fn collect_memory() {
    let mut mem_info = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };

    // SAFETY: dwLength 已按 API 要求设置；mem_info 为栈上独占结构体，API 仅在调用期间填充。
    let ok = unsafe { GlobalMemoryStatusEx(&mut mem_info).is_ok() };

    if ok {
        MEM_USAGE.store(mem_info.dwMemoryLoad as u32, Ordering::Relaxed);
    }
}
