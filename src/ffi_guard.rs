pub struct MutexGuard(pub windows::Win32::Foundation::HANDLE);

impl Drop for MutexGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: self.0 是由 CreateMutexW 成功创建的有效互斥量句柄，在生命周期结束时自动关闭。
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

pub struct RegKey(pub windows::Win32::System::Registry::HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: self.0 是由 RegOpenKeyExW 成功打开的有效注册表句柄，在析构时安全关闭。
        unsafe {
            let _ = windows::Win32::System::Registry::RegCloseKey(self.0);
        }
    }
}

pub struct MenuGuard(pub windows::Win32::UI::WindowsAndMessaging::HMENU);

impl Drop for MenuGuard {
    fn drop(&mut self) {
        // SAFETY: self.0 是有效的菜单句柄，销毁它防止内存泄漏。
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyMenu(self.0);
        }
    }
}
