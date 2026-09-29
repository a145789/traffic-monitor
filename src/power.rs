//! 电源 / 会话 / 显示三类订阅的注册与配对注销。
//!
//! 电源 / 会话 / 显示三类订阅共四条订阅点，各自独立、互不可替代（各有独立的注册
//! 句柄，见本模块的四个原子）：
//! - `RegisterSuspendResumeNotification`：休眠/唤醒（APM）事件的**定向**生产者。
//!   APM 事件本身是顶层广播，主窗口嵌入任务栏成为跨进程子窗口后收不到，只有定向
//!   订阅让休眠位在嵌入后仍有可达的生产者。
//! - legacy 显示器开关（`GUID_MONITOR_POWER_ON`）与控制台显示状态
//!   （`GUID_CONSOLE_DISPLAY_STATE`）：两个订阅点 GUID 不同，都写同一个
//!   `SUSPEND_REASON_MONITOR` 位，由位集幂等吸收重复事件。S0 息屏只发后者，而
//!   legacy 是否仍投递属未验证行为——不得因为「同构」而合并或删掉其中一条。
//! - `WTSRegisterSessionNotification`：锁屏/解锁会话通知。
//!
//! **顺序不变量（本模块对外最重要的调用契约）**：注销必须先于窗口 `DestroyWindow`。
//! 四条通知都绑定注册时的 HWND，窗口销毁后句柄即失效，那时再注销已无意义（且会留下
//! 悬空注册）。启动失败早退路径与 `rebuild_main_window` 因此都在销毁旧窗口**之前**
//! 调用本模块的注销入口。该约束原先靠「同一文件内可见」维持，搬迁后由本注释承载，
//! 改动调用点顺序前请先确认这一点。
//!
//! **句柄即「该项订阅是否还在」的唯一真值源**：为空 ⇔ 不在。缺失时由恢复调度器的
//! 周期 tick 静默补注册（`ensure_*`，不弹框），不能等下一次同源事件——那些事件正是
//! 靠这条订阅才会到达。

use windows::Win32::Foundation::{HANDLE, HWND};
#[cfg(test)]
use windows::Win32::System::Power::HPOWERNOTIFY;
use windows::Win32::System::Power::{
    RegisterPowerSettingNotification, RegisterSuspendResumeNotification,
    UnregisterPowerSettingNotification, UnregisterSuspendResumeNotification,
};
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::System::SystemServices::{GUID_CONSOLE_DISPLAY_STATE, GUID_MONITOR_POWER_ON};
use windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_WINDOW_HANDLE;

use crate::util::{AtomicHwnd, AtomicPowerNotify, log_event, show_error};

/// legacy 显示器开关订阅（`GUID_MONITOR_POWER_ON`）的注册句柄。
static POWER_NOTIFY_HANDLE: AtomicPowerNotify = AtomicPowerNotify::new();
/// 控制台显示状态订阅（`GUID_CONSOLE_DISPLAY_STATE`）的注册句柄。
///
/// 与 `POWER_NOTIFY_HANDLE` 并列而不是共用一个槽：两个订阅点各有独立的注册/注销
/// 配对，共用一个原子会让「后注册的覆盖先注册的」变成注销泄漏。两者都处理同一个
/// `SUSPEND_REASON_MONITOR` 位，位集幂等吸收重复事件。
static DISPLAY_NOTIFY_HANDLE: AtomicPowerNotify = AtomicPowerNotify::new();
/// 休眠/唤醒定向订阅（`RegisterSuspendResumeNotification`）的注册句柄。
///
/// 主窗口嵌入任务栏后是跨进程 `WS_CHILD`，接收不到 `PBT_APMSUSPEND` 这类顶层广播；
/// 定向注册让休眠位在嵌入后仍有可达的生产者。
static SUSPEND_NOTIFY_HANDLE: AtomicPowerNotify = AtomicPowerNotify::new();
/// 已注册会话通知的窗口句柄；`None` 表示当前无注册。重建路径据此在销毁
/// 旧窗口前配对注销，避免每次 Explorer 重启留下悬空注册。
static SESSION_NOTIFY_HWND: AtomicHwnd = AtomicHwnd::new();

/// 只读观测 legacy 显示器开关订阅句柄（`#[cfg(test)]` 冒烟测试用，不暴露写入口）。
///
/// 写方：`register_monitor_power_on`（存）、`unregister_power_notifications` /
/// `rearm_display_notify`（取）；读方：`ensure_power_notifications`（为空即补注册）
/// 与冒烟测试。收敛：缺失由恢复调度器周期静默补齐。
#[cfg(test)]
pub(crate) fn power_notify_handle() -> Option<HPOWERNOTIFY> {
    POWER_NOTIFY_HANDLE.load()
}

/// 只读观测控制台显示状态订阅句柄（`#[cfg(test)]` 冒烟测试用，不暴露写入口）。
/// 写读收敛与 [`power_notify_handle`] 同构（02 篇增订的第二个显示器订阅点）。
#[cfg(test)]
pub(crate) fn display_notify_handle() -> Option<HPOWERNOTIFY> {
    DISPLAY_NOTIFY_HANDLE.load()
}

/// 只读观测休眠/唤醒定向订阅句柄（`#[cfg(test)]` 冒烟测试用，不暴露写入口）。
/// 写读收敛与 [`power_notify_handle`] 同构（02 篇补的定向生产者）。
#[cfg(test)]
pub(crate) fn suspend_notify_handle() -> Option<HPOWERNOTIFY> {
    SUSPEND_NOTIFY_HANDLE.load()
}

/// 只读观测已注册会话通知的窗口句柄（`#[cfg(test)]` 冒烟测试用，不暴露写入口）。
///
/// 写方：`register_session_notification`（成功才记位）、`unregister_session_notification`
///（取走并清零）；读方：注销路径本身与冒烟测试（断言重建后重绑）。
/// 收敛：注册失败不记位，重建路径在 `DestroyWindow` 旧窗口前配对注销。
#[cfg(test)]
pub(crate) fn session_notify_hwnd() -> Option<HWND> {
    SESSION_NOTIFY_HWND.load()
}

/// 仅测试可用的写入口：预置「会话通知已注册」这一状态。
///
/// 生产路径没有写入口——注册成功才记位是唯一记位纪律，写入口只此一处且只在测试
/// 构建里存在（手法与 `AtomicHwnd::store_raw` 一致）。供恢复能力表的用例断言
/// 「句柄非空 ⇒ 本轮不重注册、也不得改写句柄」。
#[cfg(test)]
pub(crate) fn store_session_notify_raw(hwnd: HWND) {
    SESSION_NOTIFY_HWND.store(hwnd);
}

/// 注册休眠/唤醒定向订阅。句柄存入 [`SUSPEND_NOTIFY_HANDLE`] 供配对注销。
///
/// 用 user32 的 `RegisterSuspendResumeNotification`（Win8+）把 APM 事件定向投递到
/// 本窗口：APM 事件本身是顶层广播，主窗口嵌入任务栏成为跨进程子窗口后收不到，
/// 只有定向订阅才让休眠位在嵌入后仍有可达的生产者。
fn register_suspend_resume(hwnd: HWND) -> Result<(), String> {
    // SAFETY: hwnd 是当前主窗口句柄；注册只登记「把通知投递到该窗口」，不解引用
    // 调用方内存；返回句柄存入具名原子供配对注销。
    unsafe {
        RegisterSuspendResumeNotification(HANDLE(hwnd.0), DEVICE_NOTIFY_WINDOW_HANDLE)
            .map(|handle| SUSPEND_NOTIFY_HANDLE.store(handle))
            .map_err(|e| format!("休眠/唤醒: {e:?}"))
    }
}

/// 注册 legacy 显示器开关订阅（`GUID_MONITOR_POWER_ON`）。
fn register_monitor_power_on(hwnd: HWND) -> Result<(), String> {
    // SAFETY: 同 `register_suspend_resume`。
    unsafe {
        RegisterPowerSettingNotification(
            HANDLE(hwnd.0),
            &GUID_MONITOR_POWER_ON,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
        .map(|handle| POWER_NOTIFY_HANDLE.store(handle))
        .map_err(|e| format!("显示器开关: {e:?}"))
    }
}

/// 注册控制台显示状态订阅（`GUID_CONSOLE_DISPLAY_STATE`）。
///
/// 与 legacy 订阅点并存：S0 息屏只发这一个，而 legacy 是否仍投递属未验证行为；
/// 两者写同一个 `SUSPEND_REASON_MONITOR` 位，重复事件被位集幂等吸收。
fn register_console_display_state(hwnd: HWND) -> Result<(), String> {
    // DEVICE_NOTIFY_WINDOW_HANDLE = 0；误用 SERVICE_HANDLE(1) 会把 HWND 当服务句柄，
    // 返回 ERROR_SERVICE_NOT_IN_EXE (0x8007043B)。SAFETY: 同 `register_suspend_resume`。
    unsafe {
        RegisterPowerSettingNotification(
            HANDLE(hwnd.0),
            &GUID_CONSOLE_DISPLAY_STATE,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
        .map(|handle| DISPLAY_NOTIFY_HANDLE.store(handle))
        .map_err(|e| format!("显示状态: {e:?}"))
    }
}

/// 注册全部电源/显示器订阅到指定窗口（启动与 Explorer 重建路径）。失败非致命：
/// 分别退化为失去对应的省电触发。
///
/// 三项失败合并成**一条**提示：三项都失败时逐项弹框会变成连点三次模态框，而它们
/// 是同一段启动/重建动作里的同一次失败序列。
///
/// 周期路径不调用本函数（会弹框），改用 [`ensure_power_notifications`]。
pub(crate) fn register_power_notifications(hwnd: HWND) {
    let failures: Vec<String> = [
        register_suspend_resume(hwnd),
        register_monitor_power_on(hwnd),
        register_console_display_state(hwnd),
    ]
    .into_iter()
    .filter_map(Result::err)
    .collect();

    if !failures.is_empty() {
        show_error(&format!(
            "注册电源/显示器通知失败（{} 项，失败项对应的省电触发将不生效）: {}",
            failures.len(),
            failures.join("; ")
        ));
    }
}

/// 配对注销全部电源/显示器订阅。
///
/// 与 `unregister_session_notification` 同一纪律：必须在 `DestroyWindow` **之前**
/// 调用——通知绑定的是窗口句柄，窗口销毁后句柄即失效，那时再注销已无意义。
/// 仅由 UI 线程调用（启动收尾与 `rebuild_main_window`），与注册调用之间无并发写者。
pub(crate) fn unregister_power_notifications() {
    if let Some(handle) = SUSPEND_NOTIFY_HANDLE.take() {
        // SAFETY: handle 由 RegisterSuspendResumeNotification 成功返回且只取走一次。
        unsafe {
            let _ = UnregisterSuspendResumeNotification(handle);
        }
    }
    if let Some(handle) = POWER_NOTIFY_HANDLE.take() {
        // SAFETY: handle 由 RegisterPowerSettingNotification 成功返回且只取走一次。
        unsafe {
            let _ = UnregisterPowerSettingNotification(handle);
        }
    }
    if let Some(handle) = DISPLAY_NOTIFY_HANDLE.take() {
        // SAFETY: 同上，两个订阅点各有独立句柄，不会重复注销。
        unsafe {
            let _ = UnregisterPowerSettingNotification(handle);
        }
    }
}

/// 静默补注册缺失的电源/显示器订阅（恢复调度器每个周期调用一次）。
///
/// **句柄本身就是「该项订阅是否还在」的唯一真值源**：为空 ⇔ 不在。订阅不在就再也
/// 收不到对应通知——休眠位失去生产者、显示器开关再也不会置 `MONITOR` 位——省电语义
/// 会静默失效到下一次 Explorer 重建或进程重启。因此必须按句柄在周期 tick 上补，
/// 而不能只在「别的同源事件」里重注册：那些事件正是靠这条订阅才会到达。
///
/// 失败只留 release 日志，绝不弹框（周期路径）；下个周期继续补。
pub(crate) fn ensure_power_notifications(hwnd: HWND) {
    if SUSPEND_NOTIFY_HANDLE.load().is_none()
        && let Err(e) = register_suspend_resume(hwnd)
    {
        log_event!("补注册休眠/唤醒订阅失败: {e}");
    }
    if POWER_NOTIFY_HANDLE.load().is_none()
        && let Err(e) = register_monitor_power_on(hwnd)
    {
        log_event!("补注册显示器开关订阅失败: {e}");
    }
    if DISPLAY_NOTIFY_HANDLE.load().is_none()
        && let Err(e) = register_console_display_state(hwnd)
    {
        log_event!("补注册显示状态订阅失败: {e}");
    }
}

/// 清掉 MONITOR 挂起位后重新武装显示器订阅（恢复调度器在 TTL 清位那一轮调用）。
///
/// TTL 清位意味着本进程承认可能漏掉了一次点亮通知：先注销再重新订阅，把这条通道
/// 重新武装一遍。**重新注册走 [`ensure_power_notifications`]**，因此即使这里注册
/// 失败（句柄保持为空）也不会静默失效——下个恢复周期会继续补。
pub(crate) fn rearm_display_notify(hwnd: HWND) {
    for handle in [POWER_NOTIFY_HANDLE.take(), DISPLAY_NOTIFY_HANDLE.take()]
        .into_iter()
        .flatten()
    {
        // SAFETY: 两个句柄各来自一次成功注册且只取走一次。
        unsafe {
            let _ = UnregisterPowerSettingNotification(handle);
        }
    }
    ensure_power_notifications(hwnd);
}

/// 注册会话锁屏通知，并记录句柄供 [`unregister_session_notification`] 配对注销。
/// 失败非致命：锁屏暂停失效，显示器关闭仍由电源通知覆盖。
pub(crate) fn register_session_notification(hwnd: HWND) {
    if let Err(e) = wts_register_session_notification(hwnd) {
        show_error(&format!("注册会话通知失败: {e:?}"));
    }
}

/// `WTSRegisterSessionNotification` 的唯一调用点：成功才记位。
///
/// 只有注册成功才记位：失败时无配对可注销，记位会让状态位谎报存在注册。
/// 启动/重建路径与恢复调度的静默补注册共用本函数，注册纪律只有一份实现。
fn wts_register_session_notification(hwnd: HWND) -> windows::core::Result<()> {
    // SAFETY: hwnd 是当前主窗口句柄；注册只登记「把会话切换通知投递到该窗口」，
    // 不解引用调用方内存，也不要求调用方内存存活到注册之后；注册结果以 HWND 值
    // 存入 SESSION_NOTIFY_HWND，由 unregister_session_notification 配对注销。
    unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) }
        .map(|()| SESSION_NOTIFY_HWND.store(hwnd))
}

/// 静默补注册缺失的会话通知（恢复调度器每个周期调用一次）。
///
/// 句柄即真值源（同 [`ensure_power_notifications`]）：为空 ⇔ 无注册。注册缺失意味着
/// `SUSPEND_REASON_SESSION` 在本进程内永远没有生产者——锁屏不再暂停采样，且此后
/// 不会再有第二次提示。因此按真值源在周期 tick 上补，而不是等下一次 Explorer 重建。
///
/// 失败只留 release 日志，绝不弹框（周期路径：启动/重建那次失败已经提示过）；
/// 返回 false 让本轮计入退避，下个周期继续补。
pub(crate) fn ensure_session_notification(hwnd: HWND) -> bool {
    if SESSION_NOTIFY_HWND.load().is_some() {
        return true;
    }
    match wts_register_session_notification(hwnd) {
        Ok(()) => true,
        Err(e) => {
            log_event!("补注册会话通知失败: {e:?}");
            false
        }
    }
}

/// 配对注销会话通知并在成功取出句柄时清零状态位。
///
/// WTS 契约要求每个 `WTSRegisterSessionNotification` 都有对应的
/// `WTSUnRegisterSessionNotification`，且必须在窗口**销毁之前**调用；
/// 故重建路径在 `DestroyWindow` 旧窗口前调用本函数。窗口销毁后句柄即失效，
/// 那时再注销已无意义，这也是不能在销毁之后补做的原因。
///
/// 仅由 UI 线程调用（启动失败早退路径与 `rebuild_main_window`），
/// 与同一线程上的注册调用之间无并发写者。
pub(crate) fn unregister_session_notification() {
    if let Some(registered) = SESSION_NOTIFY_HWND.take() {
        // SAFETY: registered 由 WTSRegisterSessionNotification 成功返回且被 take 只取走
        // 一次，不存在重复注销；本调用须在窗口销毁前完成，此时该 HWND 仍然有效。
        unsafe {
            let _ = WTSUnRegisterSessionNotification(registered);
        }
    }
}
