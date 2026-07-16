use windows::Win32::System::Memory::{GetProcessHeaps, HEAP_FLAGS, HeapCompact};
use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW,
};
use windows::core::PCWSTR;
use windows_registry::CURRENT_USER;

/// 业务字符串 → NUL 结尾 UTF-16。Win32 API 的标准入口。
///
/// `config` 中已含尾 NUL 的常量请直接 `encode_utf16().collect()`，勿再套本函数
/// （会多一个多余的 NUL，虽通常无害但语义不清晰）。
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn show_error(msg: &str) {
    let title = to_wide("Traffic Monitor");
    let msg_wide = to_wide(msg);
    // SAFETY: title/msg_wide 含尾 NUL，在 MessageBoxW 返回前存活。
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(msg_wide.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

pub fn show_info(msg: &str) {
    let title = to_wide("Traffic Monitor");
    let msg_wide = to_wide(msg);
    // SAFETY: 同上。
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(msg_wide.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
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

/// 压缩进程**所有堆**并修剪工作集。
///
/// 与 `collector::trim_working_set` 的区别：先遍历进程内所有堆（含 UCRT malloc 堆）
/// 调用 `HeapCompact` 将空闲页 decommit 归还 OS，再修剪工作集物理页面。
///
/// Rust 默认分配器走 UCRT 的 `malloc` 堆，与 `GetProcessHeap()` 返回的默认
/// 进程堆是**不同的堆句柄**。若只压缩默认堆，`Command::output()` 等 Rust
/// 代码路径在 UCRT 堆上释放的内存不会被 decommit，造成内存水位居高不下。
///
/// 仅在更新检查等「大量临时堆分配已全部释放」的场景中调用；**不可**用于常规
/// 周期性 trim，否则会因过度 decommit 导致后续正常分配反复 recommit 页面，
/// 造成工作集反弹到更高水位。
pub fn compact_and_trim() {
    // SAFETY:
    // 1. GetProcessHeaps(None) 返回进程堆数量，无副作用。
    // 2. 第二次调用传入足够大的缓冲区，OS 填充所有堆句柄。
    // 3. HeapCompact(flags=0) 使用默认序列化，对多线程安全。
    //    它合并空闲块并将整页空闲内存 decommit 归还 OS。
    // 4. GetCurrentProcess() 返回当前进程的伪句柄，安全且不需关闭。
    // 5. 将 (usize::MAX, usize::MAX) 传给 SetProcessWorkingSetSize 是系统约定的
    //    资源清理命令，将物理页面从工作集修剪至 Standby List。
    unsafe {
        let count = GetProcessHeaps(&mut []);
        if count > 0 {
            let mut heaps = vec![windows::Win32::Foundation::HANDLE::default(); count as usize];
            let actual = GetProcessHeaps(&mut heaps);
            for heap in heaps.iter().take(actual as usize) {
                let _ = HeapCompact(*heap, HEAP_FLAGS(0));
            }
        }
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
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
}
