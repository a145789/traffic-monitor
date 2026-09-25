//! 跨模块复用的通用 Win32 句柄 RAII 守卫。
//!
//! 仅收口「裸句柄 → CloseHandle/DestroyMenu」且无业务构造逻辑的类型。
//! 业务专属守卫（GDI、WinHTTP、BCrypt、MibTable）留在各自模块。

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// 测试专用释放计数：`Drop` 恰好执行一次的可观测手段。
/// 不用句柄重操作作探针——句柄槽复用会带来假红（互斥量探针在默认并行下约 2/16 轮误报），
/// 而二次关闭/销毁在复用时还会关掉他人的活对象。计数只在 `#[cfg(test)]` 编译，
/// release 产物无此符号、无额外开销。
#[cfg(test)]
pub(crate) static MUTEX_GUARD_DROPS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
pub(crate) static MENU_GUARD_DROPS: AtomicUsize = AtomicUsize::new(0);

pub struct MutexGuard(pub windows::Win32::Foundation::HANDLE);

impl Drop for MutexGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: self.0 若有效则必为已移入本守卫的 CreateMutexW 成功句柄；
            // 本守卫是该句柄的唯一释放者，Drop 只执行一次，CloseHandle 与创建配对且仅调用一次。
            // 字段为公开的 Copy 句柄（类型不禁止别名），安全性依赖“移入后无第二条释放路径、
            // 句柄在 Drop 前保持有效”这一调用纪律，生产侧构造点均满足。
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
            #[cfg(test)]
            MUTEX_GUARD_DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }
}

pub struct MenuGuard(pub windows::Win32::UI::WindowsAndMessaging::HMENU);

impl Drop for MenuGuard {
    fn drop(&mut self) {
        // SAFETY: self.0 为已移入本守卫的 CreatePopupMenu 成功句柄；
        // 本守卫是该菜单的唯一释放者，Drop 只执行一次，DestroyMenu 与创建配对且仅调用一次。
        // 守卫存活期内的借用（菜单项插入与弹出）不释放句柄，句柄在 Drop 前保持有效。
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyMenu(self.0);
        }
        #[cfg(test)]
        MENU_GUARD_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_menu_guard_drop_runs_exactly_once() {
        // 本用例是默认门禁下 MenuGuard 的唯一构造点（生产侧 show_context_menu 无单测覆盖），
        // 计数断言精确，不受并行测试干扰。
        // SAFETY: 本测试线程创建空弹出菜单，成功后所有权移入守卫。
        let drops_before = MENU_GUARD_DROPS.load(Ordering::SeqCst);
        let hmenu = unsafe { windows::Win32::UI::WindowsAndMessaging::CreatePopupMenu() }.unwrap();
        let guard = MenuGuard(hmenu);
        assert_eq!(
            MENU_GUARD_DROPS.load(Ordering::SeqCst),
            drops_before,
            "持有期间不得发生释放"
        );
        drop(guard);
        assert_eq!(
            MENU_GUARD_DROPS.load(Ordering::SeqCst),
            drops_before + 1,
            "MenuGuard::drop 必须恰好执行一次"
        );
    }
}
