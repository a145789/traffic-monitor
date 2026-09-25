//! 跨模块复用的通用 Win32 句柄 RAII 守卫。
//!
//! 仅收口「裸句柄 → CloseHandle/DestroyMenu」且无业务构造逻辑的类型。
//! 业务专属守卫（GDI、WinHTTP、BCrypt、MibTable）留在各自模块。

pub struct MutexGuard(pub windows::Win32::Foundation::HANDLE);

impl Drop for MutexGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: self.0 若有效则必为已移入本守卫的 CreateMutexW 成功句柄，本守卫是唯一持有者；
            // Drop 只执行一次，CloseHandle 与创建配对且仅调用一次，无别名句柄。
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

pub struct MenuGuard(pub windows::Win32::UI::WindowsAndMessaging::HMENU);

impl Drop for MenuGuard {
    fn drop(&mut self) {
        // SAFETY: self.0 为已移入本守卫的 CreatePopupMenu 成功句柄，本守卫是唯一持有者；
        // Drop 只执行一次，DestroyMenu 与创建配对且仅调用一次。
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyMenu(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::PCWSTR;

    #[test]
    fn test_mutex_guard_drop_closes_handle() {
        // SAFETY: 无名互斥量、无初始持有者；调用线程拥有该句柄，成功后所有权移入守卫。
        let handle = unsafe {
            windows::Win32::System::Threading::CreateMutexW(None, false, PCWSTR(std::ptr::null()))
        }
        .unwrap();
        let raw = handle;
        assert!(!raw.is_invalid());
        let guard = MutexGuard(handle);
        assert!(!guard.0.is_invalid());
        drop(guard);
        // SAFETY: raw 是刚被守卫关闭的句柄值；再次 CloseHandle 仅作关闭探针，
        // 成功即说明 Drop 漏关，失败即证明已关闭，不解引用任何资源。
        let second = unsafe { windows::Win32::Foundation::CloseHandle(raw) };
        assert!(
            second.is_err(),
            "MutexGuard::drop 未关闭句柄：二次 CloseHandle 仍成功"
        );
    }

    #[test]
    fn test_menu_guard_drop_destroys_menu() {
        // SAFETY: 本测试线程创建空弹出菜单，成功后所有权移入守卫。
        let hmenu = unsafe { windows::Win32::UI::WindowsAndMessaging::CreatePopupMenu() }.unwrap();
        let raw = hmenu;
        let guard = MenuGuard(hmenu);
        assert_eq!(guard.0.0, raw.0);
        drop(guard);
        // SAFETY: raw 是刚被守卫销毁的菜单句柄值；再次 DestroyMenu 仅作销毁探针，
        // 成功即说明 Drop 漏销毁，失败即证明已销毁，不解引用任何资源。
        let second = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyMenu(raw) };
        assert!(
            second.is_err(),
            "MenuGuard::drop 未销毁菜单：二次 DestroyMenu 仍成功"
        );
    }
}
