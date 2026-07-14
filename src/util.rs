use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW,
};
use windows::core::PCWSTR;
use windows_registry::CURRENT_USER;

pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn show_error(msg: &str) {
    let title = to_wide("Traffic Monitor");
    let msg_wide = to_wide(msg);
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

#[cfg(test)]
mod tests {
    use super::*;

    // ===== to_wide =====

    #[test]
    fn test_to_wide_nul_terminated() {
        let w = to_wide("hello");
        // 必须以 NUL 结尾。
        assert_eq!(w.last(), Some(&0));
        // 不含 NUL 的前缀应与原始 UTF-16 一致。
        let without_nul = &w[..w.len() - 1];
        let expected: Vec<u16> = "hello".encode_utf16().collect();
        assert_eq!(without_nul, expected);
    }

    #[test]
    fn test_to_wide_empty() {
        let w = to_wide("");
        // 空字符串应产生单独的 NUL 终止符。
        assert_eq!(w, vec![0]);
    }

    #[test]
    fn test_to_wide_unicode() {
        // 箭头字符（网速显示常用）应正确编码。
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
