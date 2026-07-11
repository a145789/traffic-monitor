use windows::Win32::System::Registry::{
    HKEY, KEY_READ, KEY_WRITE, REG_CREATE_KEY_DISPOSITION, REG_DWORD, RegCreateKeyExW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW,
};
use windows::core::PCWSTR;

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

pub fn reg_read_dword(hkey_root: HKEY, subkey: &str, value_name: &str) -> Option<u32> {
    let key_path = to_wide(subkey);
    let val_name = to_wide(value_name);
    let mut hkey = Default::default();

    // SAFETY: key_path 以 NUL 结尾，hkey 为栈上变量，成功后由 RegKey RAII 守卫接管释放。
    let open_ok = unsafe {
        RegOpenKeyExW(
            hkey_root,
            PCWSTR(key_path.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )
        .is_ok()
    };

    if open_ok {
        let _key_guard = crate::ffi_guard::RegKey::new(hkey);
        let mut dword: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;

        // SAFETY: hkey 有效（生命周期由 _key_guard 保护），val_name 以 NUL 结尾，
        // dword 和 size 为栈变量，size 与缓冲区大小匹配。
        let result = unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR(val_name.as_ptr()),
                None,
                None,
                Some(&mut dword as *mut u32 as *mut u8),
                Some(&mut size),
            )
        };
        if result.is_ok() {
            return Some(dword);
        }
    }

    None
}

pub fn reg_write_dword(hkey_root: HKEY, subkey: &str, value_name: &str, value: u32) -> bool {
    let key_path = to_wide(subkey);
    let val_name = to_wide(value_name);
    let mut hkey = Default::default();
    let mut disposition = REG_CREATE_KEY_DISPOSITION(0);

    // SAFETY: key_path 以 NUL 结尾，hkey 和 disposition 为栈上变量，
    // 成功后句柄由 RegKey RAII 守卫接管释放。
    let open_ok = unsafe {
        RegCreateKeyExW(
            hkey_root,
            PCWSTR(key_path.as_ptr()),
            None,
            None,
            Default::default(),
            KEY_WRITE,
            None,
            &mut hkey,
            Some(&mut disposition),
        )
        .is_ok()
    };

    if open_ok {
        let _key_guard = crate::ffi_guard::RegKey::new(hkey);
        // SAFETY: hkey 有效（生命周期由 _key_guard 保护），val_name 以 NUL 结尾，
        // from_raw_parts 将 &u32 转换为合法字节切片，长度正确。
        unsafe {
            RegSetValueExW(
                hkey,
                PCWSTR(val_name.as_ptr()),
                Some(0),
                REG_DWORD,
                Some(std::slice::from_raw_parts(
                    &value as *const u32 as *const u8,
                    std::mem::size_of::<u32>(),
                )),
            )
            .is_ok()
        }
    } else {
        false
    }
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
