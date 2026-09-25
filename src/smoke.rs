//! Windows 窗口过程与重建冒烟（opt-in，需真实 Explorer/任务栏）。
//!
//! 只覆盖能自动化的部分：建窗与嵌入、重建路径（含旧绑定的迁移断言）、
//! 定时器收敛与挂起对称、DPI 脏位由恢复事务清位。渲染像素比对、真实跨屏 DPI、
//! 故障注入不做自动化。残留缺口（已如实记录）：电源句柄值域不透明，
//! “注销先于 DestroyWindow 的顺序”只由代码评审覆盖，详见重建用例内注释。
//!
//! 运行（必须串行，全局状态跨用例共享）：
//! `cargo test --locked -- --ignored --test-threads=1`
//!
//! Fixture 契约（用例间不互相污染靠它）：单个串行 RAII fixture，`setup()` 依次
//! `ImmDisableIME`（首个顶层窗口之前，生产在 `main` 同样要求）、注册窗口类、
//! `Renderer::new` + `set_renderer`、建看门狗与主窗口并登记 `CURRENT_MAIN_HWND`；
//! `Drop` 按生产同样的顺序清理：先 `unregister_session_notification()`、
//! `unregister_power_notifications()`（注销先于 `DestroyWindow`），再
//! `remove_tray_icon()`、`DestroyWindow` 两个窗口、`take_renderer()`，最后把
//! `DPI_DIRTY` / `MONITOR_FULLSCREEN` / 全局挂起位复位到干净态。
//! 预处理条件不满足（本机无 Explorer/任务栏）时显式 panic 并给出可操作信息，
//! 不用提前 `return` 的假跳过（Rust 无原生 skip，`return` 仍记 passed）。
//!
//! 人工清单（发布前过一遍）：合盖睡眠→唤醒后网络数值 1–2 秒内、CPU/内存一个
//! 5 秒周期内恢复更新；跨两个不同缩放比显示器的拖动；断开网络后手动检查更新的
//! 失败提示；更新确认框点“是”后主程序退出、安装器正常启动；安装器 UAC 取消后
//! 主程序被重新拉起。

use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::Ime::ImmDisableIME;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, GetParent, IsWindow, KillTimer, SendMessageW, WM_DPICHANGED,
};

/// 串行 RAII fixture：建窗与清理配对，`Drop` 不 panic。
struct Fixture {
    main: HWND,
    watchdog: HWND,
}

impl Fixture {
    fn setup() -> Self {
        // SAFETY: 只改本进程输入法状态；首个顶层窗口之前调用，与生产一致。
        unsafe {
            let _ = ImmDisableIME(u32::MAX);
        }
        // 同一进程内串行跑多个用例时类已注册，忽略“已存在”失败；
        // 真缺类时后续建窗会 loud 失败，不会静默通过。
        let _ = crate::window::register_window_class();
        let _ = crate::window::register_watchdog_class();
        let renderer =
            crate::renderer::Renderer::new().expect("冒烟前置失败：Renderer::new 建 GDI 资源失败");
        crate::renderer::set_renderer(renderer);
        let watchdog =
            crate::window::create_watchdog_window().expect("冒烟前置失败：看门狗窗口创建失败");
        let main = crate::window::create_main_window().expect("冒烟前置失败：主窗口创建失败");
        super::CURRENT_MAIN_HWND.store(main);
        Self { main, watchdog }
    }

    fn main(&self) -> HWND {
        self.main
    }

    fn watchdog(&self) -> HWND {
        self.watchdog
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        super::unregister_session_notification();
        super::unregister_power_notifications();
        crate::tray::remove_tray_icon();
        // SAFETY: 两句柄同属本测试线程创建；KillTimer 对不存在的 ID 仅返回错误。
        unsafe {
            KillTimer(Some(self.watchdog), crate::config::TIMER_ID_RECOVERY).ok();
            KillTimer(Some(self.watchdog), crate::config::TIMER_ID_REBUILD_RETRY).ok();
        }
        // 重建用例会替换 CURRENT_MAIN_HWND：先清理当前登记值，再兜底 fixture 自持值；
        // IsWindow 守卫 + 同线程无他者并发，保证不 double-close。
        // SAFETY: IsWindow 纯查询；DestroyWindow 只发给仍存活的本线程窗口。
        if let Some(cur) = super::CURRENT_MAIN_HWND.take()
            && unsafe { IsWindow(Some(cur)) }.as_bool()
        {
            unsafe {
                let _ = DestroyWindow(cur);
            }
        }
        if unsafe { IsWindow(Some(self.main)) }.as_bool() {
            unsafe {
                let _ = DestroyWindow(self.main);
            }
        }
        if unsafe { IsWindow(Some(self.watchdog)) }.as_bool() {
            unsafe {
                let _ = DestroyWindow(self.watchdog);
            }
        }
        crate::window::invalidate_last_rect();
        crate::renderer::take_renderer();
        // 复位可变全局，下一串行用例从干净态开始（Drop 在断言之后跑，不掩盖结果）。
        crate::state::DPI_DIRTY.store(false, Ordering::Release);
        crate::state::MONITOR_FULLSCREEN.store(false, Ordering::Release);
        crate::state::SUSPEND_REASONS.resume(crate::state::SUSPEND_REASON_SYSTEM);
        crate::state::SUSPEND_REASONS.resume(crate::state::SUSPEND_REASON_SESSION);
        crate::state::SUSPEND_REASONS.resume(crate::state::SUSPEND_REASON_MONITOR);
    }
}

fn require_taskbar() -> HWND {
    crate::window::get_taskbar_hwnd()
        .expect("冒烟前置失败：本机无 Explorer/任务栏，跳过前请先启动 explorer.exe")
}

#[test]
#[ignore]
fn build_and_embed_attaches_to_current_taskbar() {
    let fx = Fixture::setup();
    let taskbar = require_taskbar();
    crate::window::embed_in_taskbar(fx.main())
        .expect("嵌入任务栏失败：请确认 explorer.exe 正在运行");
    assert!(
        crate::window::is_embedded(),
        "embed_in_taskbar 返回 Ok 后 EMBEDDED 必须为真"
    );
    // SAFETY: fx.main 刚走完嵌入序列；GetParent 只查询父子关系，跨进程同样有效。
    let parent = unsafe { GetParent(fx.main()) }.ok();
    assert_eq!(parent, Some(taskbar), "嵌入后父窗口必须等于当前任务栏");
}

#[test]
#[ignore]
fn rebuild_rebinds_new_main_window() {
    let fx = Fixture::setup();
    // 重建内含嵌入，失败会弹框：先确认任务栏存在，让缺环境 loud 失败在断言处。
    require_taskbar();
    let old = super::current_main_hwnd().expect("fixture 应已登记主窗口");
    // 与生产一致的旧绑定（main 启动尾段同顺序）：电源订阅 → 嵌入 → 托盘/定时器 → 会话通知。
    // 没有这一步，重建内的 unregister_*/remove_tray_icon 对旧窗口全是空操作，
    // 用例只能证明“新建成功”，不能证明“旧资源正确交接”。
    super::register_power_notifications(old);
    crate::window::embed_in_taskbar(old).expect("旧窗口嵌入失败：请确认 explorer.exe 正在运行");
    assert!(
        super::bind_display_and_timers(old),
        "旧绑定尾段核心定时器须建起"
    );
    super::register_session_notification(old);
    assert_eq!(
        super::session_notify_hwnd(),
        Some(old),
        "旧窗口会话通知须已登记"
    );
    assert!(super::power_notify_handle().is_some(), "旧窗口订阅须已登记");
    assert!(
        super::display_notify_handle().is_some(),
        "旧窗口订阅须已登记"
    );
    assert!(
        super::suspend_notify_handle().is_some(),
        "旧窗口订阅须已登记"
    );
    assert_eq!(crate::tray::tray_owner(), Some(old), "旧窗口托盘须已绑定");
    assert!(
        !crate::state::DPI_DIRTY.load(Ordering::Acquire),
        "旧绑定成功后脏位须为干净"
    );
    // 刻意绕过 TaskbarCreated 路由，只测重建函数本身（路由依赖真实广播，属人工验收）。
    super::rebuild_main_window(fx.watchdog());
    let new = super::current_main_hwnd().expect("重建后 CURRENT_MAIN_HWND 必须换成新句柄");
    assert_ne!(old.0, new.0, "重建必须换句柄，旧句柄不得复用");
    assert!(crate::window::is_embedded(), "重建后 EMBEDDED 必须为真");
    // 迁移断言：值域是窗口句柄本身的状态必须精确落到新柄（残留旧柄即失败）；
    // 电源句柄值域不透明（OS 会回收复用数值），只断言重建后在册——
    // “注销先于 DestroyWindow 的顺序”仍由代码评审覆盖。
    assert_eq!(
        super::session_notify_hwnd(),
        Some(new),
        "会话通知必须从旧柄迁移到新柄"
    );
    assert_eq!(
        crate::tray::tray_owner(),
        Some(new),
        "托盘绑定必须从旧柄迁移到新柄"
    );
    assert!(
        super::power_notify_handle().is_some(),
        "重建后 legacy 显示器订阅必须重绑"
    );
    assert!(
        super::display_notify_handle().is_some(),
        "重建后控制台显示状态订阅必须重绑"
    );
    assert!(
        super::suspend_notify_handle().is_some(),
        "重建后休眠定向订阅必须重绑"
    );
    // SAFETY: new 为重建刚登记的新主窗口；GetParent 只查询父子关系。
    let parent = unsafe { GetParent(new) }.ok();
    assert_eq!(
        parent,
        crate::window::get_taskbar_hwnd(),
        "重建后新窗口父级必须等于当前任务栏"
    );
    assert!(
        crate::suspend::retry_missing_timers(new),
        "重建后须无缺失定时器残留"
    );
    // 托盘无残留由 fixture 的 Drop 负责（remove_tray_icon），此处不断言系统托盘状态。
}

#[test]
#[ignore]
fn timers_converge_across_suspend_resume() {
    let fx = Fixture::setup();
    let missing = crate::suspend::sync_monitoring_timers(fx.main());
    assert!(
        missing.is_empty(),
        "正常态定时器应全部建起，缺失集合须为空：{missing:?}"
    );
    crate::suspend::suspend_system(fx.main(), crate::state::SUSPEND_REASON_SESSION);
    crate::suspend::resume_system(fx.main(), crate::state::SUSPEND_REASON_SESSION);
    let missing = crate::suspend::sync_monitoring_timers(fx.main());
    assert!(
        missing.is_empty(),
        "挂起→恢复对称后定时器仍须收敛为空：{missing:?}"
    );
}

#[test]
#[ignore]
fn dpi_dirty_cleared_by_recovery_transaction() {
    let fx = Fixture::setup();
    // 处理器用 GetDpiForWindow 取当前 DPI，手工发消息不会改变它：
    // 消息分支只更新资源并重嵌入，从不清脏位（清位点唯一归恢复事务所有）；
    // 真正的跨屏 DPI 行为见本文件头部人工清单。
    require_taskbar();
    // 前置脏位：模拟一次已失败的 DPI 更新。干净初值会让末尾断言空洞
    // （成功分支根本不碰该位），预置是本用例承重的必要条件。
    crate::state::DPI_DIRTY.store(true, Ordering::Release);
    // SAFETY: fx.main 为本线程有效窗口；WM_DPICHANGED 分支忽略 wparam/lparam，
    // 不解引用任何指针；SendMessageW 同步投递、无悬垂。
    unsafe {
        SendMessageW(fx.main(), WM_DPICHANGED, Some(WPARAM(0)), Some(LPARAM(0)));
    }
    assert!(
        crate::state::DPI_DIRTY.load(Ordering::Acquire),
        "消息分支成功时不得清位：清位权只归恢复事务"
    );
    // 驱动真正的清位 owner（恢复调度器的 DPI 事务），断言它收敛并清位。
    assert!(super::recover_dpi(fx.main()), "同 DPI 下恢复事务须成功");
    assert!(
        !crate::state::DPI_DIRTY.load(Ordering::Acquire),
        "恢复事务成功后脏位必须被清"
    );
}
