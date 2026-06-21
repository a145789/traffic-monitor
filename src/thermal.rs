//! 设备过热风险推断引擎。
//!
//! 目标不是测量真实 CPU 温度，而是预测"用户何时应采取物理降温措施"。
//!
//! 双策略热功率估计：
//! - 拔电：电池放电功率 = 系统总发热（能量守恒直测，精度高）
//! - 插电：CPU% + MEM% + 内核/用户比 多信号推断（有 GPU/NPU 盲区）
//!
//! 双 EMA（τ=10s 快 / τ=90s 慢）模拟皮肤热容滞后，对齐用户体感。
//! 状态机带滞回 + 最小驻留，把"何时该降温"变成明确的 HOT/CRITICAL 事件。

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

use crate::config::{
    A_CPU_MW_PER_PCT, B_MEM_MW_PER_PCT, C_KERNEL_HEAVY_MW, EMA_ALPHA_FAST_Q8, EMA_ALPHA_SLOW_Q8,
    FP_BREAKS_MW, FP_BREAKS_RISK, KERNEL_GATE_CPU_PCT, KU_HEAVY_THRESHOLD_Q8, MEM_USAGE,
    P_IDLE_PLUG_MW, ST_COOL_TO_WARM, ST_CRIT_TO_HOT, ST_DWELL_SECS, ST_HOT_TO_CRIT, ST_HOT_TO_WARM,
    ST_WARM_TO_COOL, ST_WARM_TO_HOT, THERMAL_RISK, THERMAL_STATE, TREND_BONUS_DN, TREND_BONUS_UP,
    TREND_FALL_MW, TREND_RISE_MW,
};

// 独立的 GetSystemTimes 前值，与 collector 解耦，避免 1s/5s 采样周期混叠。
static PREV_IDLE: AtomicU64 = AtomicU64::new(0);
static PREV_KERNEL: AtomicU64 = AtomicU64::new(0);
static PREV_USER: AtomicU64 = AtomicU64::new(0);
static TIMES_INITIALIZED: AtomicBool = AtomicBool::new(false);

// 双 EMA 状态（mW，定点整数）
static P_FAST: AtomicI32 = AtomicI32::new(0);
static P_SLOW: AtomicI32 = AtomicI32::new(0);
static EMA_INITIALIZED: AtomicBool = AtomicBool::new(false);

// 状态机驻留计数
static DWELL: AtomicU32 = AtomicU32::new(0);

const SYS_BATT_STATE_LEVEL: u32 = 5;

// 使用 powrprof.dll 的 CallNtPowerInformation 而非 kernel32 的 GetSystemPowerStatus，
// 因为后者不提供放电功率（mW），只有剩余电量百分比。
#[link(name = "powrprof")]
unsafe extern "system" {
    fn CallNtPowerInformation(
        level: u32,
        in_buf: *const u8,
        in_len: u32,
        out_buf: *mut u8,
        out_len: u32,
    ) -> i32;
}

/// Windows SYSTEM_BATTERY_STATE 结构体的 Rust 镜像。
///
/// 与 Windows SDK 逐字段对齐（已通过 CallNtPowerInformation 原始字节转储验证）：
///   offset 0:  AcOnLine        BOOLEAN (u8)
///   offset 1:  BatteryPresent  BOOLEAN (u8)
///   offset 2:  Charging        BOOLEAN (u8)
///   offset 3:  Discharging     BOOLEAN (u8)
///   offset 4:  Spare1[3]       3 bytes
///   offset 7:  Tag             BYTE (u8)  ← 注意是 1 字节，不是 ULONG
///   offset 8:  MaxCapacity     ULONG
///   offset 12: RemainingCapacity ULONG
///   offset 16: Rate            LONG (正=充电, 负=放电 mW)
///   offset 20: EstimatedTime   ULONG
///   offset 24: DefaultAlert1   ULONG
///   offset 28: DefaultAlert2   ULONG
/// 总大小 32 bytes。不需要 `packed`：4+3+1=8 bytes 后 ULONG 自然 4 字节对齐。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SystemBatteryState {
    ac_on_line: u8,
    battery_present: u8,
    charging: u8,
    discharging: u8,
    spare1: [u8; 3],
    tag: u8,
    max_capacity: u32,
    remaining_capacity: u32,
    rate: i32, // offset 16, 正=充电(mW) 负=放电(mW)
    estimated_time: u32,
    default_alert1: u32,
    default_alert2: u32,
}

// 编译期布局断言：防止未来字段调整意外破坏与 Windows SDK 的对齐。
const _: () = assert!(std::mem::size_of::<SystemBatteryState>() == 32);

/// 读取电池状态。返回 (AC在线, 正在放电, 放电功率_mW)。
///
/// 传感器不可用或插电时返回 (true, false, 0)，调用方走插电推断路径。
fn read_battery() -> (bool, bool, i32) {
    let mut s = SystemBatteryState::default();
    let size = std::mem::size_of::<SystemBatteryState>() as u32;

    // SAFETY:
    // 1. &mut s 是栈上合法的 SystemBatteryState 结构体，size 与其大小一致。
    // 2. InformationLevel=5 (SystemBatteryState) 只读不写输入，传入 null/0 安全。
    // 3. CallNtPowerInformation 成功时填充 OutputBuffer，失败时不修改缓冲区（保持 Default）。
    // 4. 结构体为 repr(C)，与 Windows SYSTEM_BATTERY_STATE 逐字段对齐（32 bytes）。
    //    编译期断言 const _ below 保证布局变更时立即报错。
    let status = unsafe {
        CallNtPowerInformation(
            SYS_BATT_STATE_LEVEL,
            std::ptr::null(),
            0,
            &mut s as *mut _ as *mut u8,
            size,
        )
    };

    if status != 0 || s.battery_present == 0 {
        return (true, false, 0);
    }

    let ac = s.ac_on_line != 0;
    let discharging = s.discharging != 0 && s.rate < 0;
    let mw = if discharging { -s.rate } else { 0 };
    (ac, discharging, mw)
}

/// 功率→风险分段线性映射 f(P)。
///
/// 沿 `FP_BREAKS_MW` / `FP_BREAKS_RISK` 断点插值，纯整数运算无浮点。
/// 输入负值或超出上限时分别夹紧到 0 / 100。
fn f_p_to_risk(p_mw: i32) -> i32 {
    if p_mw <= FP_BREAKS_MW[0] {
        return 0;
    }
    if p_mw >= FP_BREAKS_MW[4] {
        return 100;
    }
    for i in 0..4 {
        if p_mw < FP_BREAKS_MW[i + 1] {
            let span_mw = FP_BREAKS_MW[i + 1] - FP_BREAKS_MW[i];
            let span_risk = FP_BREAKS_RISK[i + 1] - FP_BREAKS_RISK[i];
            return FP_BREAKS_RISK[i] + (p_mw - FP_BREAKS_MW[i]) * span_risk / span_mw;
        }
    }
    100
}

/// 状态机转移函数。升级需满足 dwell 门槛（防抖），降级立即（快速恢复）。
fn next_state(cur: u8, r: u32, dwell: u32) -> u8 {
    match (cur, r) {
        (0, r) if r >= ST_COOL_TO_WARM && dwell >= ST_DWELL_SECS => 1,
        (1, r) if r >= ST_WARM_TO_HOT && dwell >= ST_DWELL_SECS => 2,
        (2, r) if r >= ST_HOT_TO_CRIT && dwell >= ST_DWELL_SECS => 3,
        (1, r) if r <= ST_WARM_TO_COOL => 0,
        (2, r) if r <= ST_HOT_TO_WARM => 1,
        (3, r) if r <= ST_CRIT_TO_HOT => 2,
        _ => cur,
    }
}

/// 独立采样 GetSystemTimes，计算 CPU 利用率与内核/用户比。
///
/// 返回 (cpu_pct, ku_q8)。ku_q8 = (kernel_diff << 8) / user_diff，定点 Q8。
/// 与 `collector::collect_cpu` 完全解耦，避免 1s/5s 采样周期混叠。
fn sample_cpu_times() -> Option<(i32, u32)> {
    let mut idle = 0u64;
    let mut kernel = 0u64;
    let mut user = 0u64;

    // SAFETY: 传入的指针均指向当前栈帧分配的有效且可变的 u64 变量。
    // Windows API 仅在此调用期间写入数据，符合内存安全和对齐要求。
    let ok = unsafe {
        windows::Win32::System::Threading::GetSystemTimes(
            Some(&mut idle as *mut u64 as *mut _),
            Some(&mut kernel as *mut u64 as *mut _),
            Some(&mut user as *mut u64 as *mut _),
        )
        .is_ok()
    };

    if !ok {
        return None;
    }

    if !TIMES_INITIALIZED.load(Ordering::Acquire) {
        PREV_IDLE.store(idle, Ordering::Release);
        PREV_KERNEL.store(kernel, Ordering::Release);
        PREV_USER.store(user, Ordering::Release);
        TIMES_INITIALIZED.store(true, Ordering::Release);
        return None;
    }

    let di = idle.saturating_sub(PREV_IDLE.load(Ordering::Acquire));
    let dk = kernel.saturating_sub(PREV_KERNEL.load(Ordering::Acquire));
    let du = user.saturating_sub(PREV_USER.load(Ordering::Acquire));

    PREV_IDLE.store(idle, Ordering::Release);
    PREV_KERNEL.store(kernel, Ordering::Release);
    PREV_USER.store(user, Ordering::Release);

    let total = dk + du;
    if total == 0 {
        return None;
    }

    let cpu_pct = (((total - di) * 100 / total).min(100)) as i32;
    // Windows GetSystemTimes 的 kernel 时间包含 idle，需扣除得到纯内核时间。
    // 否则 idle 时 ku_q8 被空闲时间严重高估（idle 时 dk≈di+du，比纯内核大 10 倍+）。
    let dk_actual = dk.saturating_sub(di);
    let ku_q8 = if du > 0 {
        ((dk_actual << 8) / du) as u32
    } else {
        255
    };
    Some((cpu_pct, ku_q8))
}

/// 热风险采集入口。每秒由 `TIMER_ID_THERMAL` 调用。
///
/// 开销：1 次 `CallNtPowerInformation` + 1 次 `GetSystemTimes` + 纯整数运算 + 6 次原子 store。
/// 总开销 < 20μs，1Hz 下 CPU 占用可忽略。
pub fn collect_thermal() {
    // 1. 采集电池状态
    let (ac, discharging, batt_mw) = read_battery();

    // 2. 独立采样 CPU 时间
    let (cpu, ku_q8) = match sample_cpu_times() {
        Some(v) => v,
        None => return,
    };

    // 3. 读取内存负载（由 collector 每 5s 更新，热模型容忍 5s 滞后）
    let mem = MEM_USAGE.load(Ordering::Relaxed) as i32;

    // 4. 估计热功率 (mW)
    let p_mw = if !ac && discharging && batt_mw > 0 {
        // 拔电放电：放电功率即系统总发热（能量守恒直测）
        batt_mw
    } else {
        // 插电或传感器失败：多信号推断
        let ku_heavy = cpu > KERNEL_GATE_CPU_PCT && ku_q8 > KU_HEAVY_THRESHOLD_Q8;
        let k = if ku_heavy { C_KERNEL_HEAVY_MW } else { 0 };
        P_IDLE_PLUG_MW + cpu * A_CPU_MW_PER_PCT + mem * B_MEM_MW_PER_PCT + k
    };

    // 5. 双 EMA（定点 Q8: alpha = N/256）
    // 首次采样直接用 p_mw 初始化，避免从 0 爬升数分钟导致风险严重低估。
    if !EMA_INITIALIZED.swap(true, Ordering::AcqRel) {
        P_FAST.store(p_mw, Ordering::Relaxed);
        P_SLOW.store(p_mw, Ordering::Relaxed);
        THERMAL_RISK.store(f_p_to_risk(p_mw).clamp(0, 100) as u32, Ordering::Release);
        return;
    }
    let prev_fast = P_FAST.load(Ordering::Relaxed);
    let prev_slow = P_SLOW.load(Ordering::Relaxed);
    let af = EMA_ALPHA_FAST_Q8 as i32;
    let as_ = EMA_ALPHA_SLOW_Q8 as i32;
    let new_fast = (af * p_mw + (256 - af) * prev_fast) / 256;
    let new_slow = (as_ * p_mw + (256 - as_) * prev_slow) / 256;
    P_FAST.store(new_fast, Ordering::Relaxed);
    P_SLOW.store(new_slow, Ordering::Relaxed);

    // 6. 风险指数 R(t) ∈ [0, 100]
    let r_slow = f_p_to_risk(new_slow);
    let r_fast = f_p_to_risk(new_fast);
    let trend = new_fast - new_slow;
    let bonus = if trend > TREND_RISE_MW {
        TREND_BONUS_UP
    } else if trend < -TREND_FALL_MW {
        TREND_BONUS_DN
    } else {
        0
    };
    let r = (7 * r_slow + 3 * r_fast) / 10 + bonus;
    let r = r.clamp(0, 100) as u32;
    THERMAL_RISK.store(r, Ordering::Release);

    // 7. 状态机（滞回 + 最小驻留）
    // saturating_add 防止长期滞留同一状态时 u32 溢出回绕到 0 触发意外跳变。
    let mut dwell = DWELL.load(Ordering::Relaxed).saturating_add(1);
    let cur = THERMAL_STATE.load(Ordering::Relaxed);
    let next = next_state(cur, r, dwell);
    if next != cur {
        dwell = 0;
    }
    THERMAL_STATE.store(next, Ordering::Release);
    DWELL.store(dwell, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_f_p_to_risk_breakpoints() {
        assert_eq!(f_p_to_risk(0), 0);
        assert_eq!(f_p_to_risk(12000), 20);
        assert_eq!(f_p_to_risk(22000), 50);
        assert_eq!(f_p_to_risk(35000), 80);
        assert_eq!(f_p_to_risk(50000), 100);
    }

    #[test]
    fn test_f_p_to_risk_midpoints() {
        assert_eq!(f_p_to_risk(6000), 10);
        assert_eq!(f_p_to_risk(17000), 35);
        assert_eq!(f_p_to_risk(28500), 65);
        assert_eq!(f_p_to_risk(42500), 90);
    }

    #[test]
    fn test_f_p_to_risk_clamping() {
        assert_eq!(f_p_to_risk(-1000), 0);
        assert_eq!(f_p_to_risk(-1), 0);
        assert_eq!(f_p_to_risk(50001), 100);
        assert_eq!(f_p_to_risk(100000), 100);
    }

    #[test]
    fn test_ema_converges_to_constant_input() {
        // 连续输入相同 P，EMA 应逐步趋近 P。
        // 用纯函数模拟，不触碰全局原子。
        let p = 30000i32;
        let af = EMA_ALPHA_FAST_Q8 as i32;
        let as_ = EMA_ALPHA_SLOW_Q8 as i32;
        let mut fast = 0i32;
        let mut slow = 0i32;
        for _ in 0..1000 {
            fast = (af * p + (256 - af) * fast) / 256;
            slow = (as_ * p + (256 - as_) * slow) / 256;
        }
        // 定点截断误差允许 ±100mW（slow EMA 的 α=3/256 极小，截断累积）。
        assert!((fast - p).abs() < 50, "fast EMA not converged: {fast}");
        assert!((slow - p).abs() < 100, "slow EMA not converged: {slow}");
    }

    #[test]
    fn test_ema_fast_responsiveness() {
        // 快 EMA 应比慢 EMA 更快响应阶跃输入。
        let p = 40000i32;
        let af = EMA_ALPHA_FAST_Q8 as i32;
        let as_ = EMA_ALPHA_SLOW_Q8 as i32;
        let mut fast = 0i32;
        let mut slow = 0i32;
        for _ in 0..30 {
            fast = (af * p + (256 - af) * fast) / 256;
            slow = (as_ * p + (256 - as_) * slow) / 256;
        }
        assert!(
            fast > slow,
            "fast should lead slow after step: fast={fast} slow={slow}"
        );
    }

    #[test]
    fn test_state_machine_upgrade_with_dwell() {
        // dwell 不够时不升级
        assert_eq!(next_state(0, 30, 5), 0);
        assert_eq!(next_state(0, 30, 9), 0);
        // dwell 达标后升级
        assert_eq!(next_state(0, 25, 10), 1);
        assert_eq!(next_state(0, 99, 10), 1);
        assert_eq!(next_state(1, 55, 10), 2);
        assert_eq!(next_state(2, 85, 10), 3);
    }

    #[test]
    fn test_state_machine_downgrade_immediate() {
        // 降级不需要 dwell
        assert_eq!(next_state(1, 15, 0), 0);
        assert_eq!(next_state(1, 10, 0), 0);
        assert_eq!(next_state(2, 45, 0), 1);
        assert_eq!(next_state(3, 75, 0), 2);
    }

    #[test]
    fn test_state_machine_hysteresis() {
        // 滞回带内不抖动：WARM 态 R=20（在 15-25 之间）保持不动
        assert_eq!(next_state(1, 20, 100), 1);
        // HOT 态 R=50（在 45-55 之间）保持不动
        assert_eq!(next_state(2, 50, 100), 2);
        // CRITICAL 态 R=80（在 75-85 之间）保持不动
        assert_eq!(next_state(3, 80, 100), 3);
    }

    #[test]
    fn test_state_machine_stays_in_cool() {
        // COOL 态低风险保持不动
        assert_eq!(next_state(0, 10, 100), 0);
        assert_eq!(next_state(0, 24, 100), 0);
    }
}
