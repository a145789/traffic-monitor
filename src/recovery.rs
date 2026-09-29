//! 看门狗定时器上的恢复调度状态机：重建退避重试、恢复调度 tick 的武装与退避，
//! 以及一轮恢复动作（DPI 事务、缺失定时器/托盘/通知的静默补做、陈旧挂起位自愈）。
//!
//! 与本模块并列的「启动与 Explorer 重建编排」留在 `main.rs`。两者原先同处一个文件，
//! 恢复日程表的每次扩展都要改到与重建编排相邻的同一段代码，评审与冲突面无法隔离；
//! 搬迁后恢复日程表只落在本模块的 [`run_recovery`]，重建编排只落在
//! `main.rs::rebuild_main_window`。
//!
//! **状态属主**：本模块的三个 `thread_local!` 事实（重建重试间隔、恢复间隔、武装
//! 状态）只由 UI 线程访问，唯一写方是 [`arm_rebuild_retry`] / [`disarm_rebuild_retry`]
//! 与 [`set_recovery_interval`]。`main.rs` 只能通过 [`rebuild_retry_idle`] 这一只读判据
//! 观测重试间隔，不得在两处各持一份表示。

use std::sync::atomic::Ordering;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};

use crate::config::{
    TIMER_ID_REBUILD_RETRY, TIMER_ID_RECOVERY, TIMER_INTERVAL_REBUILD_RETRY_MAX,
    TIMER_INTERVAL_REBUILD_RETRY_MIN, TIMER_INTERVAL_RECOVERY, TIMER_INTERVAL_RECOVERY_MAX,
};
use crate::live_main_hwnd;
use crate::power::{ensure_power_notifications, ensure_session_notification, rearm_display_notify};
use crate::state::{DPI_DIRTY, SUSPEND_REASON_MONITOR};
use crate::suspend::{heal_stale_suspend, retry_missing_timers};
use crate::tray::ensure_tray_icon;
use crate::util::{diag, log_event};
use crate::window::{
    align_window_size_to, embed_in_taskbar, invalidate_last_rect, resize_embedded_window,
    watchdog_hwnd,
};

thread_local! {
    /// 主窗口重建重试的当前间隔（毫秒）；0 表示不在重试序列中。
    /// 仅 UI 线程（看门狗过程与重建路径）读写。
    static REBUILD_RETRY_INTERVAL_MS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// 恢复调度 tick 的当前间隔（毫秒）；0 表示尚未武装。
    /// 仅 UI 线程（看门狗过程与恢复调度路径）读写。
    static RECOVERY_INTERVAL_MS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// 恢复调度 tick 的武装状态：`None` = 已武装；`Some(n)` = 未武装且已连续失败 n 次
    /// （`Some(0)` = 本进程还没试过）。唯一写方是 [`set_recovery_interval`]，
    /// 唯一读方是 [`ensure_recovery_timer`]。仅 UI 线程访问。
    static RECOVERY_ARM: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(Some(0)) };
}

/// 把窗口回滚到渲染器当前的位图尺寸。
///
/// DPI 更新失败时渲染器维持旧尺寸，而窗口已按新 DPI 改过：不拉回同一尺寸，
/// BitBlt 只覆盖旧位图区域、边缘露出色键底色。
///
/// 这是 `WM_DPICHANGED` / 启动绑定路径失败当时的**即时兜底**，与把窗口几何和位图一起
/// 推到新 DPI 的提交动作（[`recover_dpi`]）配对；两者都归本模块，避免「窗口 == 位图」
/// 这条不变量的两个出口分散在两处。
pub(crate) fn rollback_window_to_bitmap(hwnd: HWND) {
    crate::renderer::with_renderer(|r| {
        let (width, height) = r.bitmap_size();
        resize_embedded_window(hwnd, width, height);
    });
}

/// 主窗口重建重试序列是否空闲（间隔为 0，即不在重试序列中）。
///
/// 迁出 `main.rs` 的只读判据：重建编排据此判定「这是不是本失败序列的首次失败」——
/// 只有首次才提示用户，后续重试静默（否则退化成弹窗风暴）。判据必须在
/// [`arm_rebuild_retry`] **之前**读取，原因有二：武装会改写间隔；且提示框是模态的，
/// 先武装会让重试 tick 在框未关闭时重入重建。间隔状态的唯一属主仍是本模块。
pub(crate) fn rebuild_retry_idle() -> bool {
    REBUILD_RETRY_INTERVAL_MS.with(|c| c.get()) == 0
}

/// 主窗口重建失败后的退避重试：间隔从 `TIMER_INTERVAL_REBUILD_RETRY_MIN` 翻倍
/// 至 `TIMER_INTERVAL_REBUILD_RETRY_MAX`，直到重建成功。
///
/// `SetTimer` 复用同一 ID 会重设倒计时，因此连续失败无需先 `KillTimer`。
pub(crate) fn arm_rebuild_retry(watchdog: HWND) {
    let next = match REBUILD_RETRY_INTERVAL_MS.with(|c| c.get()) {
        0 => TIMER_INTERVAL_REBUILD_RETRY_MIN,
        current => (current * 2).min(TIMER_INTERVAL_REBUILD_RETRY_MAX),
    };
    REBUILD_RETRY_INTERVAL_MS.with(|c| c.set(next));
    // SAFETY: watchdog 是看门狗窗口句柄，与调用方同属 UI 线程；不使用回调函数。
    unsafe {
        if SetTimer(Some(watchdog), TIMER_ID_REBUILD_RETRY, next, None) == 0 {
            diag!("重建重试定时器({TIMER_ID_REBUILD_RETRY}) 创建失败");
        }
    }
}

/// 结束重建重试：复位退避档位并移除定时器。
pub(crate) fn disarm_rebuild_retry(watchdog: HWND) {
    REBUILD_RETRY_INTERVAL_MS.with(|c| c.set(0));
    // SAFETY: watchdog 是看门狗窗口句柄，与调用方同属 UI 线程；KillTimer 对不存在的
    // 定时器 ID 只返回错误，无副作用。
    unsafe {
        KillTimer(Some(watchdog), TIMER_ID_REBUILD_RETRY).ok();
    }
}

/// 在看门狗上武装/重置恢复调度 tick（恢复调度器唯一的定时器入口）。
///
/// 看门狗**永不参与挂起、永不重建**，因此这个 tick 在任何状态下都存在——这解决了
/// 「唯一的自愈 tick 会被 `timer_plan` 的挂起分支一起杀掉」的死结，且不需要放宽
/// 挂起态的监测定时器集合（挂起分支必须保持全空，销毁与恢复对称）。
///
/// 与 `arm_rebuild_retry` 同范式：`SetTimer` 复用同一 ID 会重设倒计时，因此连续
/// 失败无需先 `KillTimer`。创建失败**不能**只留痕了事——没有 WM_TIMER 就再也没有
/// 机会调用本函数，一次瞬态失败会永久摘掉整条恢复路径；因此这里只登记失败状态，
/// 重试交给 [`ensure_recovery_timer`]（挂到仍然活着的周期 tick 上）。
fn set_recovery_interval(watchdog: HWND, interval: u32) {
    RECOVERY_INTERVAL_MS.with(|c| c.set(interval));
    // SAFETY: watchdog 是看门狗窗口句柄，与调用方同属 UI 线程；不使用回调函数。
    let armed = unsafe { SetTimer(Some(watchdog), TIMER_ID_RECOVERY, interval, None) != 0 };
    let failures = RECOVERY_ARM.with(|c| c.get()).unwrap_or(0);
    if armed {
        RECOVERY_ARM.with(|c| c.set(None));
    } else {
        RECOVERY_ARM.with(|c| c.set(Some(failures.saturating_add(1))));
        // 只在失败序列首次留痕：`ensure_recovery_timer` 会在每个周期 tick 上重试，
        // 持续失败不得把日志刷成噪声。
        if failures == 0 {
            diag!("恢复调度定时器({TIMER_ID_RECOVERY}) 创建失败，将在下个周期 tick 重试");
            log_event!("恢复调度定时器({TIMER_ID_RECOVERY}) 创建失败，将在下个周期 tick 重试");
        }
    }
}

/// 首次武装恢复调度 tick（启动路径）。
pub(crate) fn arm_recovery_timer(watchdog: HWND) {
    set_recovery_interval(watchdog, TIMER_INTERVAL_RECOVERY);
}

/// 恢复调度的自愈武装（幂等）：未武装时才重试一次。
///
/// 恢复路径的存续不能取决于启动期那一次 `SetTimer` 是否成功（失败后没有任何
/// WM_TIMER 能再调用 [`set_recovery_interval`]）。因此把「重新武装」挂到仍然存在的
/// 周期入口上：主窗口的监测 tick（`main.rs` 的 `handle_timer`）、看门狗的重建重试
/// tick，以及状态切换（`suspend::resync_monitoring_timers`——它覆盖「进入挂起」这一
/// 临界点，恰好是监测定时器全部消失之前最后一次能补武装的机会）。
///
/// 覆盖边界：只要状态还会变化、或主窗口还有任一周期 tick 在跑，就会再试一次；唯一
/// 补不上的情形是 `SetTimer` 本身持续失败（那时没有可用手段，属 API 级故障）。
///
/// **已武装时必须直接返回**：`SetTimer` 复用同一 ID 会重设倒计时，每个 tick 都武装
/// 一次会让恢复周期永远到不了。
pub(crate) fn ensure_recovery_timer() {
    if RECOVERY_ARM.with(|c| c.get()).is_none() {
        return;
    }
    if let Some(watchdog) = watchdog_hwnd() {
        arm_recovery_timer(watchdog);
    }
}

/// 退出在即：撤销恢复调度 tick，并标记为已武装以免退出序列被处理前被周期 tick
/// 重新武装。
pub(crate) fn disarm_recovery_timer(watchdog: HWND) {
    RECOVERY_ARM.with(|c| c.set(None));
    // SAFETY: watchdog 是看门狗窗口句柄，与调用方同属 UI 线程；KillTimer 对不存在的
    // 定时器 ID 只返回错误，无副作用。
    unsafe {
        KillTimer(Some(watchdog), TIMER_ID_RECOVERY).ok();
    }
}

/// 恢复调度 tick 的入口：跑一轮恢复动作，并按结果决定下个周期的间隔。
///
/// 全部成功即回到基础间隔；出现失败则翻倍至上限——恢复动作幂等，周期只为最终
/// 收敛服务，失败时拉长间隔避免在持续故障下反复空转。
pub(crate) fn recovery_tick(watchdog: HWND) {
    let all_ok = run_recovery();
    let next = if all_ok {
        TIMER_INTERVAL_RECOVERY
    } else {
        RECOVERY_INTERVAL_MS
            .with(|c| c.get())
            .max(TIMER_INTERVAL_RECOVERY)
            .saturating_mul(2)
            .min(TIMER_INTERVAL_RECOVERY_MAX)
    };
    set_recovery_interval(watchdog, next);
}

/// 一轮恢复动作，全部通过 `live_main_hwnd()` 取当前主窗口：
/// 重试 DPI 事务 → 补建缺失定时器 → 补做缺失的托盘/会话通知 → 按原因探针清陈旧挂起位。
///
/// 返回本轮是否全部成功（决定下个周期是否退避）。重建间隙（无主窗口）视为
/// 「无事可做」而不是失败：主窗口重建自有 `arm_rebuild_retry` 的独立退避，
/// 让恢复 tick 一起退避只会拖慢重建成功后的首轮自愈。
fn run_recovery() -> bool {
    let Some(hwnd) = live_main_hwnd() else {
        return true;
    };
    let mut all_ok = true;

    if DPI_DIRTY.load(Ordering::Acquire) && !recover_dpi(hwnd) {
        all_ok = false;
    }
    if !retry_missing_timers(hwnd) {
        all_ok = false;
    }
    // 托盘图标与会话通知都是一次性注册：失败后除 Explorer 重建外无人重试，而
    // Explorer 可能整场会话都不重启。真值源为空即静默补做一次，失败计入退避。
    if !rearm_tray_and_session_capabilities(hwnd).all_ok() {
        all_ok = false;
    }

    if heal_stale_suspend(hwnd) & SUSPEND_REASON_MONITOR != 0 {
        // TTL 清位等价于本进程承认「可能漏掉了一次点亮通知」：先撤销再重新订阅。
        rearm_display_notify(hwnd);
    } else {
        // 订阅句柄是「是否已注册」的真值源：缺失就静默补，绝不等下一次同源事件
        // （那正需要这条订阅才可能到达）。本项失败**不**计入退避：补注册要尽快
        // 恢复，不能让退避把下个周期推到 10 分钟后。
        ensure_power_notifications(hwnd);
    }

    all_ok
}

/// 补做「一次性注册」能力（托盘图标、会话通知）的结果；逐项报告。
///
/// 两项失败互不蕴含：合并成一个 `bool` 后，恢复表里删掉任一项都会被另一项的失败
/// 掩盖，测试就钉不住表里到底有哪几项。`all_ok` 才是 `run_recovery` 用的退避判据。
#[derive(Debug)]
struct CapabilityRearm {
    tray: bool,
    session: bool,
}

impl CapabilityRearm {
    /// 本轮两项都已就位或补做成功。
    fn all_ok(&self) -> bool {
        self.tray && self.session
    }
}

/// 补做缺失的托盘图标与会话通知（恢复调度器每个周期调用一次）。
///
/// 判据与 [`ensure_power_notifications`] 同构：**真值源为空 ⇔ 该项不在**——托盘看
/// `TRAY_DATA`（`tray::ensure_tray_icon`），会话通知看 `power::SESSION_NOTIFY_HWND`。
/// 两项都只做一次注册尝试，注册前先判空，因此不会出现「两次注册、一次注销」。
///
/// 与电源订阅不同，本项失败计入 `run_recovery` 的退避（同 [`retry_missing_timers`]）：
/// 两者同属「注册/创建失败后留在缺失集合里」的一次性动作，语义一致，不新增退避机制。
fn rearm_tray_and_session_capabilities(hwnd: HWND) -> CapabilityRearm {
    CapabilityRearm {
        tray: ensure_tray_icon(hwnd),
        session: ensure_session_notification(hwnd),
    }
}

/// DPI 恢复事务（四步顺序不可拆，第 2 步内部再分「先对齐尺寸、再提交完整几何」）。
///
/// 1. 重试 `Renderer::update_dpi`（只换位图/字体，不动窗口几何）；
/// 2. **先无条件把窗口尺寸对齐到新位图**（`align_window_size_to`，只改尺寸），
///    再重跑嵌入序列提交位置与分层属性——这一步不能省也不能挪到失败分支里：
///    `update_dpi` 一旦成功，位图就已经是新尺寸，而接下来的嵌入序列可能在任何一步
///    瞬态失败，且它中途失败会把 `EMBEDDED` 清成 false，使带嵌入门回滚的
///    `rollback_window_to_bitmap` 拒绝动作。先对齐尺寸把「新位图 + 旧窗口」这一错配
///    的窗口期压到零，剩下没提交的只是位置与可见性，由本事务或 `reembed_if_lost` 重试；
/// 3. 失效位置缓存，避免刚提交的几何被旧缓存判为「已到位」；
/// 4. 前三步全部成功才清 `DPI_DIRTY` 并整幅重绘。
///
/// 任一步失败即保持脏位，由下一个恢复周期重试。`rollback_window_to_bitmap` 仍是
/// `WM_DPICHANGED` / 启动路径失败当时的即时兜底（那时位图未换、窗口尺寸必须跟着旧位图）。
pub(crate) fn recover_dpi(hwnd: HWND) -> bool {
    let mut dpi_updated = false;
    let mut bitmap_size = (0, 0);
    crate::renderer::with_renderer(|r| {
        dpi_updated = r.update_dpi(hwnd);
        bitmap_size = r.bitmap_size();
    });
    if !dpi_updated {
        diag!("DPI 恢复: 重建位图/字体失败，保持脏位等待下一轮");
        return false;
    }

    align_window_size_to(hwnd, bitmap_size.0, bitmap_size.1);

    if let Err(e) = embed_in_taskbar(hwnd) {
        diag!("DPI 恢复: 提交窗口几何失败: {e}");
        log_event!("DPI 恢复: 提交窗口几何失败: {e}");
        return false;
    }

    invalidate_last_rect();
    DPI_DIRTY.store(false, Ordering::Release);
    // SAFETY: hwnd 是本线程有效的主窗口句柄；InvalidateRect 只把该窗口标记为待重绘，
    // 不转移所有权，也不要求调用方内存在返回后继续存活。
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    true
}

#[cfg(test)]
mod tests {
    //! 恢复能力表的语义测试：只覆盖不依赖真 Explorer 的补做失败路径与真值源纪律，
    //! 不创建真实窗口（真窗口路径见 `src/smoke.rs`）。

    use super::rearm_tray_and_session_capabilities;

    #[test]
    fn test_recovery_rearms_tray_and_session() {
        use windows::Win32::Foundation::HWND;

        // 清空两个真值源：模拟「一次性注册失败、此后除 Explorer 重建外无人重试」。
        crate::tray::remove_tray_icon();
        crate::power::unregister_session_notification();

        // 两项缺失时恢复动作必须真的去补做一次，而不是看一眼真值源就返回：无效句柄上
        // 两项都必然失败（`NIM_ADD` 拒绝无效窗口，`WTSRegisterSessionNotification`
        // 对无效窗口返回 E_INVALIDARG），失败必须逐项反映到返回值里——删掉恢复表里的
        // 托盘那一项，`rearmed.tray` 断言即变红。
        let bogus = HWND(0x0BAD_F00D as *mut _);
        let rearmed = rearm_tray_and_session_capabilities(bogus);
        assert!(
            !rearmed.tray,
            "托盘缺失时本轮必须尝试补建并如实报告失败: {rearmed:?}"
        );
        assert!(
            !rearmed.session,
            "会话通知缺失时本轮必须尝试补注册并如实报告失败: {rearmed:?}"
        );
        assert!(
            !rearmed.all_ok(),
            "任一项补做失败都不得报告为全部就位: {rearmed:?}"
        );

        // 补做失败不得写入真值源（沿用「注册成功才记位」的纪律），否则下个周期会
        // 误判能力已就位而不再补做。
        assert_eq!(
            crate::tray::tray_owner(),
            None,
            "补建失败后 TRAY_DATA 必须保持为空"
        );
        assert_eq!(
            crate::power::session_notify_hwnd(),
            None,
            "补注册失败后 SESSION_NOTIFY_HWND 必须保持为空"
        );

        // 已就位（句柄非空）即无需补做，也不得改写句柄：值域是 HWND 的能力一旦
        // 出现「两次注册、一次注销」就会留下悬空注册。预置句柄用
        // `power::store_session_notify_raw`（与 `AtomicHwnd::store_raw` 同手法，
        // 仅在测试构建里存在）。
        crate::power::store_session_notify_raw(bogus);
        let rearmed = rearm_tray_and_session_capabilities(bogus);
        assert!(
            rearmed.session,
            "句柄非空 ⇒ 本轮无需补注册，应报告为已就位: {rearmed:?}"
        );
        assert_eq!(
            crate::power::session_notify_hwnd(),
            Some(bogus),
            "已就位时不得改写会话通知句柄"
        );
        assert!(
            !rearmed.all_ok(),
            "托盘仍缺失时不得因会话通知已就位而报告全部就位: {rearmed:?}"
        );
        // 收尾清掉预置句柄：注销对未真实注册的句柄只会失败并返回，无副作用。
        crate::power::unregister_session_notification();
    }
}
