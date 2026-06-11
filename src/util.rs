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
