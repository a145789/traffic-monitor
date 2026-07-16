//! 跨模块复用的通用 Win32 句柄 RAII 守卫。
//!
//! 职责边界：本模块只收口那些在多个业务模块间共享、且无业务专属构造/释放逻辑的
//! 「裸句柄 → CloseHandle/DestroyMenu」配对。业务专属守卫（如 `Renderer` 的事务式
//! `ScreenDcGuard`/`OwnedDc`/`OwnedBitmap`、`update` 的 `WinHttpHandles`/`BcryptHandles`、
//! `collector` 的 `MibTable`）保留在各自业务文件中，因为它们的创建/释放与具体业务
//! 不变量强耦合（构造顺序、所有权移交、配对 API 等），强行集中反而割裂上下文。
//!
//! 新增守卫时遵循以下归属规则：
//! - 仅做单一 `Close*`/`Destroy*` 释放、无业务构造逻辑的通用句柄 → 入此模块。
//! - 创建/选入/配对/事务移交等含业务语义的 → 留在业务文件并就近注释生命周期。

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

pub struct MenuGuard(pub windows::Win32::UI::WindowsAndMessaging::HMENU);

impl Drop for MenuGuard {
    fn drop(&mut self) {
        // SAFETY: self.0 是有效的菜单句柄，销毁它防止内存泄漏。
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyMenu(self.0);
        }
    }
}
