//! 系统暂停/恢复、全屏检测与主题变更检测。
//!
//! 负责管理定时器的启停（休眠/锁屏/显示器关闭时暂停），以节省 CPU 资源；
//! 电源广播（WM_POWERBROADCAST）与锁屏（WM_WTSSESSION_CHANGE）消息在此处理。
//!
//! 挂起位没有「统一超时」：三个原因的证据强度不同（`SYSTEM` 可自证、`SESSION`
//! 有文档化只读探针、`MONITOR` 只能按保守超长 TTL），自愈判定见
//! [`heal_stale_suspend`]；承接 tick 是看门狗上的恢复调度器（挂起态下监测定时器
//! 集合全空，它是唯一仍存在的周期入口）。

use std::cell::Cell;
use std::sync::atomic::Ordering;
use std::time::Instant;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::System::Power::POWERBROADCAST_SETTING;
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_UNLOCK, WTSFreeMemory,
    WTSINFOEXW, WTSQuerySessionInformationW, WTSSessionInfoEx,
};
use windows::Win32::System::SystemServices::{GUID_CONSOLE_DISPLAY_STATE, GUID_MONITOR_POWER_ON};
use windows::Win32::UI::WindowsAndMessaging::{
    GetDesktopWindow, GetForegroundWindow, GetShellWindow, GetWindowRect, KillTimer,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE, SetCoalescableTimer,
    WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};
use windows::core::PCWSTR;

use crate::collector::{reset_cpu_baseline, reset_network_baseline};
use crate::config::{
    BACKOFF_ZERO_THRESHOLD, CPU_MEM_INTERVAL, SUSPEND_MONITOR_TTL_SECS,
    TIMER_COALESCING_TOLERANCE_MS, TIMER_ID_AUTO_UPDATE, TIMER_ID_CPU_MEM, TIMER_ID_FULLSCREEN,
    TIMER_ID_NETWORK, TIMER_INTERVAL_AUTO_UPDATE, TIMER_INTERVAL_FULLSCREEN,
    TIMER_INTERVAL_NETWORK, TIMER_INTERVAL_NETWORK_BACKOFF,
};
use crate::state::{
    CONSECUTIVE_ZERO_COUNT, MONITOR_FULLSCREEN, SUSPEND_REASON_MONITOR, SUSPEND_REASON_SESSION,
    SUSPEND_REASON_SYSTEM, SUSPEND_REASONS, SuspendReasons, reset_network_backoff,
};
use crate::util::{diag, log_event, trim_working_set};
use crate::window::get_taskbar_hwnd;

thread_local! {
    /// `SUSPEND_REASON_MONITOR` 的置位时刻。**只服务 MONITOR 的 TTL**：
    /// `SYSTEM` / `SESSION` 两条路径不读也不写它。
    ///
    /// 唯一写方是 [`suspend_system`] 的 MONITOR 分支（首次置位时记时），
    /// 唯一清方是 [`resume_system`] 的 MONITOR 分支；读者只有 [`monitor_suspended_secs`]。
    /// 位集与全部处理器都跑在 UI 消息循环线程上，故用 `Cell` 而非原子。
    static SUSPEND_SINCE: Cell<Option<Instant>> = const { Cell::new(None) };
}

pub fn is_suspended() -> bool {
    SUSPEND_REASONS.is_suspended()
}

/// 挂起/全屏期间分层窗口表面可能被系统丢弃（显示模式变化、RDP 重连、DWM 重置），
/// 而数值未变时增量重绘不会触发；恢复后强制整幅重绘以自愈陈旧画面。
fn force_repaint(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

pub fn suspend_system(hwnd: HWND, reason: u32) {
    let previous = SUSPEND_REASONS.suspend(reason);
    // TTL 起点只记首次置位：重复置位若刷新起点，持续的重复通知会把 TTL 无限推后。
    if reason == SUSPEND_REASON_MONITOR && previous & SUSPEND_REASON_MONITOR == 0 {
        SUSPEND_SINCE.with(|c| c.set(Some(Instant::now())));
    }
    MONITOR_FULLSCREEN.store(false, Ordering::Release);
    resync_monitoring_timers(hwnd);
    if previous == 0 {
        trim_working_set();
    }
}

pub fn resume_system(hwnd: HWND, reason: u32) {
    let was_suspended = SUSPEND_REASONS.is_suspended();
    SUSPEND_REASONS.resume(reason);
    if reason == SUSPEND_REASON_MONITOR {
        SUSPEND_SINCE.with(|c| c.set(None));
    }
    if should_rebuild_baseline(was_suspended, &SUSPEND_REASONS) {
        reset_network_baseline();
        reset_cpu_baseline();
    }
    // 恢复即复位网络退避：唤醒/解锁后立即回到快速采样节奏。
    reset_network_backoff();
    resync_monitoring_timers(hwnd);
    force_repaint(hwnd);
}

/// 是否构成"从暂停态回到运行态"的边沿。
///
/// 基线重建必须挂在这条边沿上而非每次 `resume` 调用上：交错
/// suspend(SYSTEM)+suspend(SESSION) 后逐个 resume，只有清掉最后一个
/// 原因位的那次才真正恢复采集；已在运行态时的重复 `resume`（was=false）
/// 不得重建，否则基线被反复清空、恢复后长期显示零速。
fn should_rebuild_baseline(was_suspended: bool, reasons: &SuspendReasons) -> bool {
    was_suspended && !reasons.is_suspended()
}

/// 显示器开关的两个电源设置订阅点：`GUID_MONITOR_POWER_ON`（legacy）与
/// `GUID_CONSOLE_DISPLAY_STATE`（文档化替代，也是 Modern Standby 息屏的唯一省电
/// 信号来源——S0 下 `PBT_APMSUSPEND` 根本不发）。
///
/// 两个都订阅、两个都处理：哪个真的投递由机器与电源模型决定，任一投递即可置/清
/// `SUSPEND_REASON_MONITOR`；重复事件被位集幂等吸收（代价只是一次多余的定时器同步）。
fn is_display_power_setting(setting: &windows::core::GUID) -> bool {
    *setting == GUID_MONITOR_POWER_ON || *setting == GUID_CONSOLE_DISPLAY_STATE
}

/// WM_POWERBROADCAST 处理：系统休眠/唤醒、显示器开关。
pub fn handle_power_broadcast(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match wparam.0 as u32 {
        PBT_APMSUSPEND => {
            suspend_system(hwnd, SUSPEND_REASON_SYSTEM);
        }
        PBT_APMRESUMEAUTOMATIC => {
            resume_system(hwnd, SUSPEND_REASON_SYSTEM);
        }
        PBT_POWERSETTINGCHANGE => {
            let setting = lparam.0 as *const POWERBROADCAST_SETTING;
            if !setting.is_null() {
                // SAFETY: PBT_POWERSETTINGCHANGE 时 OS 保证 lparam 指向有效结构。
                let setting_ref = unsafe { &*setting };
                // 数值含义：0 表示显示器关闭，其余（点亮/变暗）都按「可用」处理。
                if is_display_power_setting(&setting_ref.PowerSetting)
                    && setting_ref.DataLength >= 1
                {
                    if setting_ref.Data[0] != 0 {
                        resume_system(hwnd, SUSPEND_REASON_MONITOR);
                    } else {
                        suspend_system(hwnd, SUSPEND_REASON_MONITOR);
                    }
                }
            }
        }
        _ => {}
    }
    LRESULT(0)
}

/// WM_WTSSESSION_CHANGE 处理：锁屏/解锁。
pub fn handle_session_change(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    match wparam.0 as u32 {
        WTS_SESSION_LOCK => {
            suspend_system(hwnd, SUSPEND_REASON_SESSION);
        }
        WTS_SESSION_UNLOCK => {
            resume_system(hwnd, SUSPEND_REASON_SESSION);
        }
        _ => {}
    }
    LRESULT(0)
}

/// 恢复调度的原因探针：三个挂起原因的证据强度不同，判定强度必须不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasonProbe {
    /// 本进程正在执行恢复 tick：挂起的机器不会跑定时器 ⇒ `SYSTEM` 位必为陈旧。
    /// 零 API、零超时、零误判。
    RunningTick,
    /// 会话锁状态探针结果。探针不可用时调用方根本不构造本变体——保守方向是保持位不变。
    Session { unlocked: bool },
    /// `SUSPEND_REASON_MONITOR` 位已置位的秒数。
    Monitor { elapsed_secs: u64 },
}

/// 纯判定：一个已置位的挂起原因是否已可自证/已超 TTL，应当走 [`resume_system`] 清位。
///
/// 本函数只决定「是否清位」，不实现清位：位协议与恢复边沿归 `SuspendReasons` /
/// `resume_system` 唯一所有，绕过它们会漏掉基线重建与网络退避复位。
fn should_resume_for_reason(reason: u32, probe: ReasonProbe) -> bool {
    match (reason, probe) {
        (SUSPEND_REASON_SYSTEM, _) => true,
        (SUSPEND_REASON_SESSION, ReasonProbe::Session { unlocked }) => unlocked,
        (SUSPEND_REASON_MONITOR, ReasonProbe::Monitor { elapsed_secs }) => {
            elapsed_secs >= SUSPEND_MONITOR_TTL_SECS
        }
        _ => false,
    }
}

/// `SUSPEND_REASON_MONITOR` 已置位的秒数。
///
/// 位在而起点缺失理论上不可达（置位与记时同处）；真出现时以本刻为起点并返回 0，
/// 让该位在一个完整 TTL 之后才可能被清——保守方向是「晚清」，而不是「永不清理」。
fn monitor_suspended_secs() -> u64 {
    SUSPEND_SINCE.with(|c| match c.get() {
        Some(start) => start.elapsed().as_secs(),
        None => {
            c.set(Some(Instant::now()));
            0
        }
    })
}

/// 会话锁状态探针：读 `WTSSessionInfoEx` 的 `SessionFlags`。
///
/// 返回 `None` 表示探针不可用（查询失败、缓冲过小、`Level` 不为 1），调用方据此
/// 保持位不变——锁屏期间误清位会把省电语义反向破坏掉。
///
/// 语义边界：本探针报的是**会话锁状态**，不是连接状态；「快速用户切换」「RDP 断开
/// 但会话保留」等形态下的返回值属未实测组合——若实测发现 RDP 断连时误报解锁，
/// `SESSION` 必须退回按位 TTL，而不是假设这里读到的一定是锁状态。
fn probe_session_unlocked() -> Option<bool> {
    let mut buffer = windows::core::PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: 成功时 OS 分配缓冲并通过出参给出指针与字节数，指针由紧随其后的
    // WTSFreeMemory 成对释放；失败时不分配，buffer 保持 null。
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes,
        )
        .ok()?;
    }
    if buffer.is_null() {
        return None;
    }
    let unlocked = if (bytes as usize) >= std::mem::size_of::<WTSINFOEXW>() {
        // SAFETY: 缓冲区已按 WTSINFOEXW 大小校验；`Level` 决定联合体的有效成员，
        // 只有 Level == 1 才读 WTSInfoExLevel1（否则该成员未初始化，不得求值）。
        unsafe {
            let info = &*buffer.0.cast::<WTSINFOEXW>();
            if info.Level == 1 {
                Some(info.Data.WTSInfoExLevel1.SessionFlags == WTS_SESSIONSTATE_UNLOCK as i32)
            } else {
                None
            }
        }
    } else {
        None
    };
    // SAFETY: buffer 来自上面成功的 WTSQuerySessionInformationW，只释放一次。
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    unlocked
}

/// 挂起位自愈：按原因各自的真值源判断「已置位的位是否仍然真实」，只对判定为陈旧的
/// 原因调用既有的 [`resume_system`]。
///
/// 承接 tick 是看门狗上的恢复调度器（`main::recovery_tick`）：挂起态下监测定时器
/// 集合全空，它是唯一仍存在的周期入口。三原因的处理强度**刻意不同**：
/// `SYSTEM` 自证、`SESSION` 探针、`MONITOR` 保守超长 TTL；不做统一超时，那会把
/// 零成本的 `SYSTEM` 自证降级成猜测。
///
/// 返回本轮真正清掉的位集；调用方据此决定是否需要重新注册显示器订阅。
pub fn heal_stale_suspend(hwnd: HWND) -> u32 {
    let mut healed = 0;

    if SUSPEND_REASONS.is_set(SUSPEND_REASON_SYSTEM)
        && should_resume_for_reason(SUSPEND_REASON_SYSTEM, ReasonProbe::RunningTick)
    {
        resume_system(hwnd, SUSPEND_REASON_SYSTEM);
        healed |= SUSPEND_REASON_SYSTEM;
    }

    if SUSPEND_REASONS.is_set(SUSPEND_REASON_SESSION)
        && let Some(unlocked) = probe_session_unlocked()
        && should_resume_for_reason(SUSPEND_REASON_SESSION, ReasonProbe::Session { unlocked })
    {
        resume_system(hwnd, SUSPEND_REASON_SESSION);
        healed |= SUSPEND_REASON_SESSION;
    }

    if SUSPEND_REASONS.is_set(SUSPEND_REASON_MONITOR)
        && should_resume_for_reason(
            SUSPEND_REASON_MONITOR,
            ReasonProbe::Monitor {
                elapsed_secs: monitor_suspended_secs(),
            },
        )
    {
        log_event!("显示器挂起位超过 {SUSPEND_MONITOR_TTL_SECS}s 未见点亮通知，按 TTL 清位");
        resume_system(hwnd, SUSPEND_REASON_MONITOR);
        healed |= SUSPEND_REASON_MONITOR;
    }

    healed
}

#[derive(Debug, PartialEq, Eq)]
struct TimerPlan {
    fullscreen: bool,
    network_interval: Option<u32>,
    cpu_mem: bool,
    auto_update: bool,
}

/// 纯函数决定当前状态下应存在的定时器集合，供状态机测试覆盖暂停/恢复对称性。
fn timer_plan(suspended: bool, fullscreen: bool, network_backoff: bool) -> TimerPlan {
    if suspended {
        return TimerPlan {
            fullscreen: false,
            network_interval: None,
            cpu_mem: false,
            auto_update: false,
        };
    }

    if fullscreen {
        return TimerPlan {
            fullscreen: true,
            network_interval: None,
            cpu_mem: false,
            auto_update: false,
        };
    }

    TimerPlan {
        fullscreen: true,
        network_interval: Some(if network_backoff {
            TIMER_INTERVAL_NETWORK_BACKOFF
        } else {
            TIMER_INTERVAL_NETWORK
        }),
        cpu_mem: true,
        auto_update: true,
    }
}

/// 当前状态下的定时器计划。状态切换的每个入口都在改动状态之后调用它。
fn current_timer_plan() -> TimerPlan {
    timer_plan(
        is_suspended(),
        MONITOR_FULLSCREEN.load(Ordering::Acquire),
        CONSECUTIVE_ZERO_COUNT.load(Ordering::Relaxed) >= BACKOFF_ZERO_THRESHOLD,
    )
}

/// 核心监测定时器的缺失集合（位掩码）。
///
/// 「哪些定时器应当存在」的唯一真值源仍是 [`timer_plan`] 推导出的计划集合；本类型
/// 只承载「上一轮『先杀后建』同步里哪几个没建起来」这一瞬态事实，供看门狗上的
/// 恢复调度器补建。定时器 ID 与位的映射收在 [`MissingTimers::bit`] 一处。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MissingTimers(u32);

impl MissingTimers {
    pub const EMPTY: Self = Self(0);

    fn bit(timer_id: usize) -> Option<u32> {
        match timer_id {
            TIMER_ID_FULLSCREEN => Some(1 << 0),
            TIMER_ID_NETWORK => Some(1 << 1),
            TIMER_ID_CPU_MEM => Some(1 << 2),
            _ => None,
        }
    }

    fn from_results(fullscreen_ok: bool, network_ok: bool, cpu_mem_ok: bool) -> Self {
        let mut missing = Self::EMPTY;
        for (timer_id, ok) in [
            (TIMER_ID_FULLSCREEN, fullscreen_ok),
            (TIMER_ID_NETWORK, network_ok),
            (TIMER_ID_CPU_MEM, cpu_mem_ok),
        ] {
            if !ok {
                missing = missing.with(timer_id);
            }
        }
        missing
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn has(self, timer_id: usize) -> bool {
        Self::bit(timer_id).is_some_and(|b| self.0 & b != 0)
    }

    fn with(self, timer_id: usize) -> Self {
        match Self::bit(timer_id) {
            Some(b) => Self(self.0 | b),
            None => self,
        }
    }
}

thread_local! {
    /// 最近一轮 `sync_monitoring_timers` 未能创建的核心定时器集合。
    ///
    /// 唯一写方 [`register_missing_timers`]（同步失败与恢复补建后各写一次），
    /// 唯一读方 [`retry_missing_timers`]。任何一轮同步都会整体覆盖它：同步成功即写
    /// 空集，因此不会累积历史失败。仅 UI 线程访问，故用 `Cell` 而非原子。
    static MISSING_TIMERS: Cell<MissingTimers> = const { Cell::new(MissingTimers::EMPTY) };
}

/// 登记缺失集合；写空集表示「上一轮同步已全部就位」。
///
/// 只在集合**变化**时留 release 日志：恢复调度每个周期都会重写同一个集合，持续
/// 单点失败不得把日志刷成噪声（周期重试必须静默，AGENTS.md 的恢复收敛要求）。
fn register_missing_timers(missing: MissingTimers) {
    MISSING_TIMERS.with(|c| {
        let previous = c.get();
        c.set(missing);
        if previous != missing && !missing.is_empty() {
            log_event!("监测定时器待补建集合: {missing:?}");
        }
    });
}

/// 状态切换后的定时器同步：失败即登记缺失集合，由看门狗上的恢复调度器补建。
///
/// 状态切换的全部调用点都必须走这里——直接调 [`sync_monitoring_timers`] 会把
/// 「需要重试」这一信息丢在半路（旧实现逐个 `let _ =` 丢弃返回值正是这个问题）。
/// 返回值仍然给出本次缺失集合，启动尾段据此判定「核心定时器是否都建起来了」。
///
/// 顺带确保看门狗上的恢复调度 tick 处于武装状态：这里是「监测定时器集合可能变全空」
/// 的临界点（挂起/全屏分支），过了这一步可能就没有别的周期入口来补武装了。
pub fn resync_monitoring_timers(hwnd: HWND) -> MissingTimers {
    crate::ensure_recovery_timer();
    let missing = sync_monitoring_timers(hwnd);
    register_missing_timers(missing);
    missing
}

/// 纯判定：把「当前计划要求的定时器」与「缺失集合」映射为补建清单
/// `(timer_id, interval)`。
///
/// 两个条件必须同时满足：计划要求存在 **且** 在缺失集合里。计划不要求的缺失项
/// 视为已解决——计划是唯一真值源，例如失败之后状态已切到挂起/全屏，此时绝不能把
/// 监测定时器补回来。清单为空 ⇒ 补建函数不调用任何 `set_coalescable_timer`。
fn missing_timer_plan(plan: &TimerPlan, missing: MissingTimers) -> Vec<(usize, u32)> {
    let mut pending = Vec::new();
    if plan.fullscreen && missing.has(TIMER_ID_FULLSCREEN) {
        pending.push((TIMER_ID_FULLSCREEN, TIMER_INTERVAL_FULLSCREEN));
    }
    if let Some(interval) = plan.network_interval
        && missing.has(TIMER_ID_NETWORK)
    {
        pending.push((TIMER_ID_NETWORK, interval));
    }
    if plan.cpu_mem && missing.has(TIMER_ID_CPU_MEM) {
        pending.push((TIMER_ID_CPU_MEM, CPU_MEM_INTERVAL));
    }
    pending
}

/// 恢复调度器的一步：只为缺失的定时器调用 `set_coalescable_timer`。
///
/// **绝不调用 `KillTimer`**：完整重同步会执行「先杀后建」，把健康的网络/CPU
/// 定时器倒计时一并重置；若某个定时器持续单点失败，反复重同步会一直重置健康
/// 定时器的倒计时，把采样饿死（这正是本函数存在的理由）。
///
/// 返回是否已全部补齐；未补齐的部分留在登记处，由下个恢复周期按退避间隔重试。
pub fn retry_missing_timers(hwnd: HWND) -> bool {
    let registered = MISSING_TIMERS.with(|c| c.get());
    if registered.is_empty() {
        return true;
    }
    let plan = current_timer_plan();
    let mut still_missing = MissingTimers::EMPTY;
    for (timer_id, interval) in missing_timer_plan(&plan, registered) {
        if !set_coalescable_timer(hwnd, timer_id, interval) {
            diag!("恢复调度: 补建监测定时器({timer_id}) 失败");
            still_missing = still_missing.with(timer_id);
        }
    }
    register_missing_timers(still_missing);
    still_missing.is_empty()
}

/// 依据暂停原因、全屏状态和网络退避状态，将所有周期任务定时器收敛到唯一正确集合。
///
/// 返回**核心监测定时器**（全屏检测/网络/CPU 内存）中未能创建的集合；空集表示全部
/// 就位。辅助定时器（自动更新）为 best-effort，失败被刻意忽略——它不影响监测主
/// 功能，也不进缺失集合（它有自己的周期轮询兜底）。
///
/// 本函数保持「先杀后建」：它是「定时器集合永远收敛到唯一正确集合」的实现，符合
/// 销毁与恢复对称的要求。失败后的重试不在这里，而是由 [`retry_missing_timers`]
/// 只增不删地补建。
pub fn sync_monitoring_timers(hwnd: HWND) -> MissingTimers {
    let plan = current_timer_plan();

    // 先统一移除，再按当前状态重建，避免调用方各自维护不完整的定时器子集。
    // SAFETY: hwnd 是主窗口句柄；移除不存在的定时器只会返回错误，不会破坏状态。
    unsafe {
        KillTimer(Some(hwnd), TIMER_ID_NETWORK).ok();
        KillTimer(Some(hwnd), TIMER_ID_CPU_MEM).ok();
        KillTimer(Some(hwnd), TIMER_ID_FULLSCREEN).ok();
        KillTimer(Some(hwnd), TIMER_ID_AUTO_UPDATE).ok();
    }

    let fullscreen_ok = if plan.fullscreen {
        set_coalescable_timer(hwnd, TIMER_ID_FULLSCREEN, TIMER_INTERVAL_FULLSCREEN)
    } else {
        true
    };

    let network_ok = plan
        .network_interval
        .is_none_or(|interval| set_coalescable_timer(hwnd, TIMER_ID_NETWORK, interval));
    let cpu_mem_ok = if plan.cpu_mem {
        set_coalescable_timer(hwnd, TIMER_ID_CPU_MEM, CPU_MEM_INTERVAL)
    } else {
        true
    };

    if plan.auto_update {
        let _ = set_coalescable_timer(hwnd, TIMER_ID_AUTO_UPDATE, TIMER_INTERVAL_AUTO_UPDATE);
    }

    if !fullscreen_ok {
        diag!("同步监测定时器失败: 全屏检测定时器({TIMER_ID_FULLSCREEN}) 未创建");
    }
    if !network_ok {
        diag!("同步监测定时器失败: 网络采样定时器({TIMER_ID_NETWORK}) 未创建");
    }
    if !cpu_mem_ok {
        diag!("同步监测定时器失败: CPU/内存采样定时器({TIMER_ID_CPU_MEM}) 未创建");
    }

    MissingTimers::from_results(fullscreen_ok, network_ok, cpu_mem_ok)
}

fn set_coalescable_timer(hwnd: HWND, timer_id: usize, interval: u32) -> bool {
    // SAFETY: hwnd 由当前 UI 线程拥有；定时器 ID/间隔为受控常量；不使用回调函数。
    unsafe {
        SetCoalescableTimer(
            Some(hwnd),
            timer_id,
            interval,
            None,
            TIMER_COALESCING_TOLERANCE_MS,
        ) != 0
    }
}

/// 全屏状态跃迁后的统一收尾，两个判定分支共用。边沿未变直接返回；
/// 离开全屏（was=true → now=false）时重建差分基线：停采期间基线已陈旧，
/// 不重建会把累计流量/CPU 摊成恢复瞬间的虚假速率（只动基线，不动退避与
/// 定时器计划集合，与 `resume_system` 同理）；随后同步监测定时器，
/// 离开时强制重绘（挂起期间分层窗口表面可能被系统丢弃，须整幅自愈）。
fn on_fullscreen_edge(hwnd: HWND, was: bool, now: bool) {
    if was == now {
        return;
    }
    MONITOR_FULLSCREEN.store(now, Ordering::Release);
    if !now {
        reset_network_baseline();
        reset_cpu_baseline();
    }
    resync_monitoring_timers(hwnd);
    if !now {
        force_repaint(hwnd);
    }
}

pub fn check_fullscreen(hwnd: HWND) {
    let foreground = unsafe { GetForegroundWindow() };
    let is_invalid = foreground.is_invalid();
    let is_desktop_or_shell =
        unsafe { GetDesktopWindow() == foreground || GetShellWindow() == foreground };

    if is_invalid || is_desktop_or_shell || foreground == hwnd {
        on_fullscreen_edge(hwnd, MONITOR_FULLSCREEN.load(Ordering::Acquire), false);
        return;
    }

    let mut rect = RECT::default();
    let _ = unsafe { GetWindowRect(foreground, &mut rect) };

    // 前台窗口所在显示器 vs 任务栏所在显示器，仅同屏全屏才暂停。
    let hmon_fg = unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) };
    let mut mi_fg = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: cbSize 已设；GetMonitorInfoW 写入 mi_fg。
    let fg_ok = unsafe { GetMonitorInfoW(hmon_fg, &mut mi_fg) };

    let is_full = if fg_ok.as_bool() {
        let mon_rect = mi_fg.rcMonitor;
        rect.left == mon_rect.left
            && rect.top == mon_rect.top
            && rect.right == mon_rect.right
            && rect.bottom == mon_rect.bottom
    } else {
        false
    };

    let same_monitor = match get_taskbar_hwnd() {
        Some(h_taskbar) => {
            let hmon_tb = unsafe { MonitorFromWindow(h_taskbar, MONITOR_DEFAULTTONEAREST) };
            hmon_fg == hmon_tb
        }
        // 任务栏暂不存在（Explorer 重启竞态）：拿不到比对基准，谈不上"不同屏"。
        // 若按 false 处理会误判成"未全屏"并恢复监测定时器，把上次状态直接丢掉；
        // 这里保持上次状态直接返回，交给下一次 tick 在任务栏就绪后重新判定。
        None => return,
    };

    let was = MONITOR_FULLSCREEN.load(Ordering::Acquire);
    let should_suspend = is_full && same_monitor;
    on_fullscreen_edge(hwnd, was, should_suspend);
}

/// # Safety
///
/// 调用者必须保证 `lparam` 指向一个有效的、以 NUL 结尾的 UTF-16 宽字符序列。
/// 由 `WM_SETTINGCHANGE` 消息传入时 OS 保证此条件成立。
///
/// 判定必须是**精确相等**：更长的 `ImmersiveColorSetFoo`、仅同前缀的
/// `ImmersiveColor` 都不得判为主题变更，禁止改成前缀或定长切片匹配。
pub unsafe fn is_immersive_color_set(lparam: LPARAM) -> bool {
    let ptr = lparam.0 as *const u16;
    if ptr.is_null() {
        return false;
    }
    // SAFETY: 调用者保证 ptr 指向有效的 NUL 结尾 UTF-16 序列；非法 UTF-16 只返回 Err，
    // 与「不等于字面量」同判 false。
    unsafe { PCWSTR(ptr).to_string() }.is_ok_and(|s| s == "ImmersiveColorSet")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AUTO_CHECK_COOLDOWN_SECS;
    use windows::Win32::Foundation::LPARAM;

    #[test]
    fn test_immersive_color_null_pointer() {
        // SAFETY: LPARAM(0) 表示 null 指针，函数应安全返回 false。
        let result = unsafe { is_immersive_color_set(LPARAM(0)) };
        assert!(!result);
    }

    #[test]
    fn test_immersive_color_valid_string() {
        let valid: Vec<u16> = "ImmersiveColorSet\0".encode_utf16().collect();
        // SAFETY: valid 在栈上，指针在调用期间有效。
        let result = unsafe { is_immersive_color_set(LPARAM(valid.as_ptr() as isize)) };
        assert!(result);
    }

    #[test]
    fn test_immersive_color_longer_name_is_rejected() {
        // 含尾 NUL 精确匹配：更长的 "ImmersiveColorSetFoo" 必须拒绝。
        // 若改用无尾 NUL 的 17 元素切片比较，此例会被误判为主题变更。
        let longer: Vec<u16> = "ImmersiveColorSetFoo\0".encode_utf16().collect();
        // SAFETY: longer 在栈上，指针在调用期间有效。
        let result = unsafe { is_immersive_color_set(LPARAM(longer.as_ptr() as isize)) };
        assert!(!result);
    }

    #[test]
    fn test_baseline_rebuild_only_on_last_resume_edge() {
        let state = SuspendReasons::new();
        state.suspend(SUSPEND_REASON_SYSTEM);
        state.suspend(SUSPEND_REASON_SESSION);

        let was = state.is_suspended();
        state.resume(SUSPEND_REASON_SYSTEM);
        assert!(
            !should_rebuild_baseline(was, &state),
            "SESSION 仍暂停，中途 resume 不得触发基线重建"
        );

        let was = state.is_suspended();
        state.resume(SUSPEND_REASON_SESSION);
        assert!(
            should_rebuild_baseline(was, &state),
            "清掉最后一个原因位才构成恢复边沿"
        );

        let was = state.is_suspended();
        state.resume(SUSPEND_REASON_SESSION);
        assert!(!should_rebuild_baseline(was, &state));
    }

    #[test]
    fn auto_update_poll_interval_must_be_far_below_cooldown() {
        // sync_monitoring_timers 每次状态切换（息屏/锁屏/全屏/网络事件）都会销毁重建
        // 全部定时器并使倒计时归零。若轮询周期≈自动检查冷却时长，事件频繁的机器上
        // 检查会被无限推迟；因此周期必须显著小于冷却，让 NEXT_CHECK_TIME 冷却门
        // 成为唯一权威。此处以 1/10 冷却为上界钉死该关系。
        assert!(u64::from(TIMER_INTERVAL_AUTO_UPDATE) <= AUTO_CHECK_COOLDOWN_SECS * 1000 / 10);
    }

    #[test]
    fn test_timer_plan_suspended_has_no_timers() {
        assert_eq!(
            timer_plan(true, false, false),
            TimerPlan {
                fullscreen: false,
                network_interval: None,
                cpu_mem: false,
                auto_update: false,
            }
        );
    }

    #[test]
    fn test_timer_plan_fullscreen_only_keeps_detection_timer() {
        let plan = timer_plan(false, true, false);
        assert!(plan.fullscreen);
        assert_eq!(plan.network_interval, None);
        assert!(!plan.cpu_mem);
        assert!(!plan.auto_update);
    }

    #[test]
    fn test_timer_plan_normal_backoff_uses_slow_network_interval() {
        let plan = timer_plan(false, false, true);
        assert!(plan.fullscreen);
        assert_eq!(plan.network_interval, Some(TIMER_INTERVAL_NETWORK_BACKOFF));
        assert!(plan.cpu_mem);
        assert!(plan.auto_update);
    }

    #[test]
    fn test_timer_plan_normal_online_uses_regular_network_interval() {
        let plan = timer_plan(false, false, false);
        assert_eq!(plan.network_interval, Some(TIMER_INTERVAL_NETWORK));
        assert!(plan.cpu_mem);
        assert!(plan.auto_update);
    }

    #[test]
    fn test_should_resume_for_reason_matches_evidence_strength() {
        // SYSTEM：在 tick 上下文中恒为真——挂起的机器不会跑定时器，故位必为陈旧。
        assert!(should_resume_for_reason(
            SUSPEND_REASON_SYSTEM,
            ReasonProbe::RunningTick
        ));

        // SESSION：只有探针明确报「已解锁」才清位。
        assert!(should_resume_for_reason(
            SUSPEND_REASON_SESSION,
            ReasonProbe::Session { unlocked: true }
        ));
        assert!(!should_resume_for_reason(
            SUSPEND_REASON_SESSION,
            ReasonProbe::Session { unlocked: false }
        ));

        // MONITOR：TTL 是唯一判据，未到 TTL 必须保持位不变。
        assert!(!should_resume_for_reason(
            SUSPEND_REASON_MONITOR,
            ReasonProbe::Monitor {
                elapsed_secs: SUSPEND_MONITOR_TTL_SECS - 1
            }
        ));
        assert!(should_resume_for_reason(
            SUSPEND_REASON_MONITOR,
            ReasonProbe::Monitor {
                elapsed_secs: SUSPEND_MONITOR_TTL_SECS
            }
        ));

        // 原因与探针不匹配时一律判否：不得凭「别的探针说可以」清掉无关位。
        assert!(!should_resume_for_reason(
            SUSPEND_REASON_SESSION,
            ReasonProbe::RunningTick
        ));
        assert!(!should_resume_for_reason(
            SUSPEND_REASON_MONITOR,
            ReasonProbe::Session { unlocked: true }
        ));
    }

    #[test]
    fn test_missing_timer_plan_only_covers_wanted_and_missing() {
        // 空缺失集合 ⇒ 空清单，补建路径因此不会调用任何 set_coalescable_timer。
        assert!(
            missing_timer_plan(&timer_plan(false, false, false), MissingTimers::EMPTY).is_empty()
        );

        let all_missing = MissingTimers::EMPTY
            .with(TIMER_ID_FULLSCREEN)
            .with(TIMER_ID_NETWORK)
            .with(TIMER_ID_CPU_MEM);

        // 计划不要求的缺失项视为已解决：挂起态定时器集合全空，绝不允许补回来。
        assert!(missing_timer_plan(&timer_plan(true, false, false), all_missing).is_empty());

        // 正常态只补缺失项，网络间隔随退避状态取值。
        assert_eq!(
            missing_timer_plan(
                &timer_plan(false, false, true),
                MissingTimers::EMPTY.with(TIMER_ID_NETWORK)
            ),
            vec![(TIMER_ID_NETWORK, TIMER_INTERVAL_NETWORK_BACKOFF)]
        );

        // 全屏态只有全屏检测定时器要求存在，其余缺失项被计划过滤掉。
        assert_eq!(
            missing_timer_plan(&timer_plan(false, true, false), all_missing),
            vec![(TIMER_ID_FULLSCREEN, TIMER_INTERVAL_FULLSCREEN)]
        );
    }
}
