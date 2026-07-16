//! 跨模块复用的通用 Win32 句柄 RAII 守卫。
//!
//! 仅收口「裸句柄 → CloseHandle/DestroyMenu」且无业务构造逻辑的类型。
//! 业务专属守卫（GDI、WinHTTP、BCrypt、MibTable）留在各自模块。

pub struct MutexGuard(pub windows::Win32::Foundation::HANDLE);

impl Drop for MutexGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: CreateMutexW 成功创建的互斥量句柄。
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

pub struct MenuGuard(pub windows::Win32::UI::WindowsAndMessaging::HMENU);

impl Drop for MenuGuard {
    fn drop(&mut self) {
        // SAFETY: CreatePopupMenu 成功创建的菜单句柄。
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyMenu(self.0);
        }
    }
}
