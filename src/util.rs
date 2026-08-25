use std::time::Instant;
use windows::Win32::System::Memory::{GetProcessHeaps, HEAP_FLAGS, HeapCompact};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{
    GetCurrentProcess, MEMORY_PRIORITY_INFORMATION, MEMORY_PRIORITY_LOW,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, ProcessMemoryPriority, ProcessPowerThrottling,
    SetProcessInformation, SetProcessWorkingSetSize,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, MessageBoxW,
};
use windows::core::PCWSTR;
use windows_registry::CURRENT_USER;

use crate::config::{
    APP_TITLE, WORKING_SET_TRIM_BASELINE_GROWTH_PCT, WORKING_SET_TRIM_COOLDOWN_SECS,
    WORKING_SET_TRIM_MIN_BYTES,
};
use crate::state::TRIM_BOOKKEEPING;

/// 业务字符串 → NUL 结尾 UTF-16。Win32 API 的标准入口。
///
/// `config` 中已含尾 NUL 的常量请直接 `encode_utf16().collect()`，勿再套本函数
/// （会多一个多余的 NUL，虽通常无害但语义不清晰）。
pub fn to_wide(s: &str) -> Vec<u16> {
    let mut v = Vec::with_capacity(s.len() + 1);
    push_wide(&mut v, s);
    v
}

/// 向已有缓冲区追加 NUL 结尾 UTF-16，供渲染热路径复用缓冲、避免逐帧分配。
pub fn push_wide(buf: &mut Vec<u16>, s: &str) {
    buf.extend(s.encode_utf16());
    buf.push(0);
}

/// 编译期把 ASCII 字符串展开为定长 UTF-16 数组。仅适用于 ASCII 输入；
/// 非 ASCII 字节会产生错误结果（调用方负责保证输入为 ASCII）。
pub const fn utf16<const N: usize>(s: &str) -> [u16; N] {
    let mut buf = [0u16; N];
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

/// 当前进程模块句柄（HINSTANCE），用于注册窗口类、加载内置资源。
pub fn module_instance() -> Result<windows::Win32::Foundation::HINSTANCE, String> {
    // SAFETY: GetModuleHandleW(None) 查询当前进程模块，无指针参数。
    unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
        .map(Into::into)
        .map_err(|e| format!("获取模块句柄失败: {e:?}"))
}

/// 统一的 MessageBoxW 入口：所有弹窗都经由本函数创建，避免各处重复拼装
/// 标题/正文宽字符串。`style` 直接透传 Win32 组合标志，返回用户选择结果。
pub fn message_box(msg: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let title = to_wide(APP_TITLE);
    let msg_wide = to_wide(msg);
    // SAFETY: title/msg_wide 含尾 NUL，在 MessageBoxW 返回前存活。
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(msg_wide.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
        )
    }
}

pub fn show_error(msg: &str) {
    message_box(msg, MB_OK | MB_ICONERROR);
}

pub fn show_info(msg: &str) {
    message_box(msg, MB_OK | MB_ICONINFORMATION);
}

/// 仅把当前进程的内存优先级调低：系统内存紧张时，OS 会优先回收本进程的
/// 代码/堆/栈页（退回 Standby），而不是与其他进程争抢物理内存。
///
/// 与 EcoQoS（ProcessPowerThrottling）不同，此设置不影响 CPU 调度与核心
/// 选择，常驻主进程可安全使用；1s 采样定时器触发时的软缺页代价为微秒级。
/// 内存优先级会被本进程创建的子进程继承。
///
/// 这是最佳努力设置：旧系统或策略限制导致设置失败时不影响功能。
pub fn set_low_memory_priority() {
    // SAFETY: MEMORY_PRIORITY_INFORMATION 为 Win32 API 要求的固定布局，
    // 指针只在同步调用期间有效；当前进程伪句柄无需关闭。
    unsafe {
        let memory = MEMORY_PRIORITY_INFORMATION {
            MemoryPriority: MEMORY_PRIORITY_LOW,
        };
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessMemoryPriority,
            &memory as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        );
    }
}

/// 将进程标记为低优先级后台工作（显式 EcoQoS + 低内存优先级）。
///
/// 仅用于 `--check-update` 短生命周期子进程：主进程是任务栏常显窗口，
/// 显式 EcoQoS 会把它钉进效率核/低频调度类并拖慢 1s 采样与 GDI 绘制。
/// 子进程的内存优先级本就继承自父进程，EcoQoS 则必须显式设置。
///
/// 这是最佳努力设置：旧系统或策略限制导致设置失败时不影响功能。
pub fn configure_background_process() {
    // SAFETY: PROCESS_POWER_THROTTLING_STATE 为 Win32 API 要求的固定布局，
    // 指针只在同步调用期间有效；当前进程伪句柄无需关闭。
    unsafe {
        let power = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &power as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }

    set_low_memory_priority();
}

pub fn reg_read_dword(subkey: &str, value_name: &str) -> Option<u32> {
    CURRENT_USER
        .open(subkey)
        .and_then(|key| key.get_u32(value_name))
        .ok()
}

pub fn reg_write_dword(subkey: &str, value_name: &str, value: u32) -> bool {
    CURRENT_USER
        .create(subkey)
        .and_then(|key| key.set_u32(value_name, value))
        .is_ok()
}

pub fn reg_read_string(subkey: &str, value_name: &str) -> Option<String> {
    CURRENT_USER
        .open(subkey)
        .and_then(|key| key.get_string(value_name))
        .ok()
}

pub fn reg_write_string(subkey: &str, value_name: &str, value: &str) -> bool {
    CURRENT_USER
        .create(subkey)
        .and_then(|key| key.set_string(value_name, value))
        .is_ok()
}

pub fn reg_remove_value(subkey: &str, value_name: &str) -> bool {
    CURRENT_USER
        .open(subkey)
        .and_then(|key| key.remove_value(value_name))
        .is_ok()
}

/// 修剪当前进程工作集（物理页面退到 Standby List），并更新共享 trim 簿记。
///
/// 与 `compact_and_trim` 的区别：本函数**不**压缩堆，适合挂起、初始化后等
/// 周期性/一次性场景调用，不会引发工作集反弹。
///
/// 所有调用方（维护定时器水位门、INIT_TRIM 一次性定时器、挂起路径、更新线程的
/// `compact_and_trim`）都经由本函数写入簿记，保证冷却时间戳与稳态基线全进程唯一。
pub fn trim_working_set() {
    // SAFETY: GetCurrentProcess() 返回当前进程伪句柄，不需关闭；
    // (usize::MAX, usize::MAX) 是系统约定的工作集修剪命令。
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }

    if let Ok(mut book) = TRIM_BOOKKEEPING.lock() {
        book.last_trim_at = Some(Instant::now());
        // 立即采样读到的是接近零的瞬时值，须等下个维护周期 fault-back 后再测稳态。
        book.pending_baseline = true;
    }
}

/// 实际触发阈值：max(绝对最低门槛, 稳态基线 × 增长系数)。
fn trim_threshold(steady_state_bytes: u64) -> u64 {
    let baseline_based =
        steady_state_bytes.saturating_mul(WORKING_SET_TRIM_BASELINE_GROWTH_PCT) / 100;
    baseline_based.max(WORKING_SET_TRIM_MIN_BYTES as u64)
}

/// 工作集超过自适应水位且距上次 trim 足够久时，归还冷物理页。
///
/// 使用 K32 前缀版本直接调用 Kernel32，避免加载旧版 Psapi 包装 DLL；水位与冷却
/// 双重限制避免将热页周期性踢出后再次 fault-in。阈值基于上次实测稳态基线校准：
/// 静态固定值若低于本进程稳态工作集，会造成每个冷却期的周期性全量清洗。
pub fn trim_working_set_if_needed() {
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    let counters_size = counters.cb;

    // SAFETY: 当前进程伪句柄有效；counters 是与 cb 匹配的栈上可写结构体。
    let working_set = unsafe {
        let success =
            K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters_size).as_bool();
        if success {
            counters.WorkingSetSize as u64
        } else {
            return;
        }
    };

    let Ok(mut book) = TRIM_BOOKKEEPING.lock() else {
        return;
    };
    let now = Instant::now();
    if book.last_trim_at.is_some_and(|t| {
        now.saturating_duration_since(t).as_secs() < WORKING_SET_TRIM_COOLDOWN_SECS
    }) {
        return;
    }

    // 冷却期内（含刚 trim 后的第一个维护 tick）优先补采稳态基线，
    // 此时 fault-back 已基本完成，读数代表进程实际需要的常驻页。
    if book.pending_baseline {
        book.pending_baseline = false;
        book.steady_state_bytes = working_set;
    }

    if working_set >= trim_threshold(book.steady_state_bytes) {
        drop(book);
        trim_working_set();
    }
}

/// 压缩进程**所有堆**并修剪工作集。
///
/// 与 `trim_working_set` 的区别：先遍历进程内所有堆（含 UCRT malloc 堆）
/// 调用 `HeapCompact` 将空闲页 decommit 归还 OS，再修剪工作集物理页面。
///
/// Rust 默认分配器走 UCRT 的 `malloc` 堆，与 `GetProcessHeap()` 返回的默认
/// 进程堆是**不同的堆句柄**。若只压缩默认堆，`Command::output()` 等 Rust
/// 代码路径在 UCRT 堆上释放的内存不会被 decommit，造成内存水位居高不下。
///
/// 仅在更新检查等「大量临时堆分配已全部释放」的场景中调用；**不可**用于常规
/// 周期性 trim，否则会因过度 decommit 导致后续正常分配反复 recommit 页面，
/// 造成工作集反弹到更高水位。
///
/// 经由 `trim_working_set` 收口写入共享簿记，与 UI 线程的水位门共享冷却时钟。
pub fn compact_and_trim() {
    // SAFETY:
    // 1. GetProcessHeaps(None) 返回进程堆数量，无副作用。
    // 2. 第二次调用传入足够大的缓冲区，OS 填充所有堆句柄。
    // 3. HeapCompact(flags=0) 使用默认序列化，对多线程安全。
    //    它合并空闲块并将整页空闲内存 decommit 归还 OS。
    unsafe {
        let count = GetProcessHeaps(&mut []);
        if count > 0 {
            let mut heaps = vec![windows::Win32::Foundation::HANDLE::default(); count as usize];
            let actual = GetProcessHeaps(&mut heaps);
            for heap in heaps.iter().take(actual as usize) {
                let _ = HeapCompact(*heap, HEAP_FLAGS(0));
            }
        }
    }
    trim_working_set();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_wide_nul_terminated() {
        let w = to_wide("hello");
        assert_eq!(w.last(), Some(&0));
        let without_nul = &w[..w.len() - 1];
        let expected: Vec<u16> = "hello".encode_utf16().collect();
        assert_eq!(without_nul, expected);
    }

    #[test]
    fn test_to_wide_empty() {
        let w = to_wide("");
        assert_eq!(w, vec![0]);
    }

    #[test]
    fn test_to_wide_unicode() {
        let w = to_wide("\u{2191}\u{2193}");
        let without_nul = &w[..w.len() - 1];
        assert_eq!(without_nul, &[0x2191u16, 0x2193u16]);
    }

    #[test]
    fn test_to_wide_roundtrip() {
        let original = "Traffic Monitor 监控";
        let w = to_wide(original);
        let without_nul = &w[..w.len() - 1];
        let rt = String::from_utf16(without_nul).unwrap();
        assert_eq!(rt, original);
    }

    #[test]
    fn test_push_wide_appends() {
        let mut buf = to_wide("A");
        push_wide(&mut buf, "B");
        // "A\0" + "B\0"
        assert_eq!(buf, vec![b'A' as u16, 0, b'B' as u16, 0]);
    }

    #[test]
    fn test_utf16_ascii() {
        let result = utf16::<5>("test");
        // 't'=0x74 'e'=0x65 's'=0x73 't'=0x74 + NUL padding
        assert_eq!(result, [0x74, 0x65, 0x73, 0x74, 0]);
    }

    #[test]
    fn test_utf16_exact_fit() {
        // 恰好填满缓冲区时不应溢出。
        let result = utf16::<3>("ab");
        assert_eq!(result, [b'a' as u16, b'b' as u16, 0]);
    }

    // ===== trim_threshold =====

    #[test]
    fn test_trim_threshold_without_baseline_uses_floor() {
        assert_eq!(trim_threshold(0), WORKING_SET_TRIM_MIN_BYTES as u64);
    }

    #[test]
    fn test_trim_threshold_baseline_below_floor_is_clamped() {
        // 基线 ×2 仍低于最低门槛时不放大缺页风险，取门槛值。
        let low = (WORKING_SET_TRIM_MIN_BYTES / 3) as u64;
        assert_eq!(trim_threshold(low), WORKING_SET_TRIM_MIN_BYTES as u64);
    }

    #[test]
    fn test_trim_threshold_grows_with_baseline() {
        let baseline = WORKING_SET_TRIM_MIN_BYTES as u64 * 4;
        assert_eq!(
            trim_threshold(baseline),
            baseline * WORKING_SET_TRIM_BASELINE_GROWTH_PCT / 100
        );
    }
}
