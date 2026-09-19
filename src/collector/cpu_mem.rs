//! CPU 与内存使用率采集。

use std::cell::Cell;
use std::sync::atomic::Ordering;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

use crate::state::{CPU_USAGE, MEM_USAGE};

// 上次 CPU 采样基线。`None` 即无基线：数据结构本身表达「基线存在与否」，
// 不另设初始化标志。仅 UI 线程读写（`TIMER_ID_CPU_MEM` tick 与
// `reset_cpu_baseline`，后者也来自同一消息循环），`Cell` 已足够。
thread_local! {
    static CPU_BASELINE: Cell<Option<CpuTimes>> = const { Cell::new(None) };
}

#[derive(Clone, Copy)]
struct CpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}

pub fn reset_cpu_baseline() {
    CPU_BASELINE.with(|baseline| baseline.set(None));
}

/// `FILETIME`（高低 32 位）拼为 100ns 滴答计数的 `u64`。
fn filetime_to_u64(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

/// 采样 `GetSystemTimes`，更新 `CPU_USAGE`。
///
/// 由 `TIMER_ID_CPU_MEM` 调用；首轮仅建立基线，不产生有效差分。
/// API 调用失败或本周期差分为 0 时保持上次展示值不变。
pub fn collect_cpu() {
    let mut idle_time = FILETIME::default();
    let mut kernel_time = FILETIME::default();
    let mut user_time = FILETIME::default();

    // SAFETY: 传入的指针均指向当前栈帧的有效 FILETIME；API 仅在调用期间写入。
    let ok = unsafe {
        windows::Win32::System::Threading::GetSystemTimes(
            Some(std::ptr::addr_of_mut!(idle_time)),
            Some(std::ptr::addr_of_mut!(kernel_time)),
            Some(std::ptr::addr_of_mut!(user_time)),
        )
        .is_ok()
    };

    if !ok {
        return;
    }

    let idle_time = filetime_to_u64(idle_time);
    let kernel_time = filetime_to_u64(kernel_time);
    let user_time = filetime_to_u64(user_time);

    CPU_BASELINE.with(|baseline| {
        // 首轮（或基线被重置后）只采样建基线：无历史即无有效差分。
        let Some(prev) = baseline.get() else {
            baseline.set(Some(CpuTimes {
                idle: idle_time,
                kernel: kernel_time,
                user: user_time,
            }));
            return;
        };

        let idle_diff = idle_time.saturating_sub(prev.idle);
        let kernel_diff = kernel_time.saturating_sub(prev.kernel);
        let user_diff = user_time.saturating_sub(prev.user);

        baseline.set(Some(CpuTimes {
            idle: idle_time,
            kernel: kernel_time,
            user: user_time,
        }));

        // GetSystemTimes 的 kernel 时间包含 idle，total = kernel + user 为全部时钟滴答。
        let total = kernel_diff + user_diff;
        if total == 0 {
            return;
        }

        let usage = ((total - idle_diff) * 100 / total).min(100) as u32;
        CPU_USAGE.store(usage, Ordering::Relaxed);
    });
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
