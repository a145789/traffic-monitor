//! 设备过热风险推断引擎。
//!
//! 目标不是测量真实 CPU 温度，而是预测"用户何时应采取物理降温措施"。
//!
//! 双策略热功率估计：
//! - 拔电：电池放电功率 = 系统总发热（能量守恒直测，精度高）
//! - 插电：CPU% + MEM% + 内核/用户比 多信号推断（有 GPU/NPU 盲区）
//!
//! 双节点热容（die 快 / skin 慢）：功率先加热 die，再耦合到机身节点，
//! 对齐"持续中载会越来越烫、停载后掌托仍烫一会儿"的体感。
//! 降频地板：高占用 + 低频比时抬高风险下限，避免 throttle 后功耗下降造成假凉快。
//! 状态机带滞回 + 升级/降级不对称驻留。

use std::sync::atomic::{AtomicBool, AtomicI8, AtomicI32, AtomicU32, AtomicU64, Ordering};

use crate::config::{
    A_CPU_MW_PER_PCT, B_MEM_MW_PER_PCT, C_KERNEL_HEAVY_MW, FP_BREAKS_MW, FP_BREAKS_RISK,
    KERNEL_GATE_CPU_PCT, KU_HEAVY_THRESHOLD_Q8, P_IDLE_PLUG_MW, ST_COOL_TO_WARM, ST_CRIT_TO_HOT,
    ST_DWELL_DOWN_SECS, ST_DWELL_SECS, ST_HOT_TO_CRIT, ST_HOT_TO_WARM, ST_WARM_TO_COOL,
    ST_WARM_TO_HOT, TAU_DIE_ALPHA_Q8, TAU_SKIN_ALPHA_Q8, THROTTLE_CPU_PCT, THROTTLE_FREQ_Q8,
    THROTTLE_RISK_FLOOR, TREND_BONUS_DN, TREND_BONUS_UP, TREND_FALL_MW, TREND_RISE_MW,
};
use crate::state::{MEM_USAGE, THERMAL_RISK, THERMAL_STATE};

// 独立的 GetSystemTimes 前值，与 collector 解耦，避免 1s/5s 采样周期混叠。
static PREV_IDLE: AtomicU64 = AtomicU64::new(0);
static PREV_KERNEL: AtomicU64 = AtomicU64::new(0);
static PREV_USER: AtomicU64 = AtomicU64::new(0);
static TIMES_INITIALIZED: AtomicBool = AtomicBool::new(false);

// 双节点热当量（mW 单位：稳态时节点值趋近输入功率）
static T_DIE: AtomicI32 = AtomicI32::new(0);
static T_SKIN: AtomicI32 = AtomicI32::new(0);
static THERMAL_INITIALIZED: AtomicBool = AtomicBool::new(false);
// 初始化：前 3 个样本取均值，避免启动尖峰把慢节点拉高导致数分钟误报。
static INIT_COUNT: AtomicU32 = AtomicU32::new(0);
static INIT_SUM: AtomicI32 = AtomicI32::new(0);

// 状态机：连续同向意图的驻留秒数，以及上一拍意图 (+1 升级 / -1 降级 / 0 无)
static DWELL: AtomicU32 = AtomicU32::new(0);
static DWELL_INTENT: AtomicI8 = AtomicI8::new(0);

use windows::Win32::System::Power::{
    CallNtPowerInformation, ProcessorInformation, SystemBatteryState,
};
use windows::Win32::System::Threading::{ALL_PROCESSOR_GROUPS, GetActiveProcessorCount};

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

/// Windows PROCESSOR_INFORMATION 结构体的 Rust 镜像。
///
/// `CallNtPowerInformation(ProcessorInformation, level=11)` 返回此结构的数组，
/// 每个元素对应一个逻辑处理器，包含其当前频率（CurrentMhz）和最大频率（MaxMhz）。
///
/// 字段按 Windows SDK 布局（6 × ULONG = 24 bytes），4 字节自然对齐。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ProcessorInformation {
    number: u32,
    max_mhz: u32,
    current_mhz: u32,
    mhz_limit: u32,
    max_idle_state: u32,
    current_idle_state: u32,
}

const _: () = assert!(std::mem::size_of::<ProcessorInformation>() == 24);

/// 读取电池状态。返回 (AC在线, 正在放电, 放电功率_mW)。
///
/// 传感器不可用或插电时返回 (true, false, 0)，调用方走插电推断路径。
fn read_battery() -> (bool, bool, i32) {
    let mut s = SystemBatteryState::default();
    let size = std::mem::size_of::<SystemBatteryState>() as u32;

    // SAFETY:
    // 1. &mut s 是栈上合法的 SystemBatteryState 结构体，size 与其大小一致。
    // 2. InformationLevel=SystemBatteryState 只读不写输入，传入 None 安全。
    // 3. CallNtPowerInformation 成功时填充 OutputBuffer，失败时不修改缓冲区（保持 Default）。
    // 4. 结构体为 repr(C)，与 Windows SYSTEM_BATTERY_STATE 逐字段对齐（32 bytes）。
    //    编译期断言 const _ below 保证布局变更时立即报错。
    let status = unsafe {
        CallNtPowerInformation(
            SystemBatteryState,
            None,
            0,
            Some(&mut s as *mut SystemBatteryState as *mut ::core::ffi::c_void),
            size,
        )
    };

    if status.0 != 0 || s.battery_present == 0 {
        return (true, false, 0);
    }

    let ac = s.ac_on_line != 0;
    let discharging = s.discharging != 0 && s.rate < 0;
    let mw = if discharging { -s.rate } else { 0 };
    (ac, discharging, mw)
}

/// 读取所有逻辑处理器的当前频率与最大频率，返回 Q8 定点频率比。
///
/// `ratio_q8 = ΣCurrentMhz / ΣMaxMhz × 256`，范围 \[1, 256\]。
///
/// 用于：
/// - 动态缩放插电路径的 `A_CPU_MW_PER_PCT`
/// - 降频地板（高占用 + 低频比 → 抬风险下限）
///
/// 传感器不可用或 sum_max 为 0 时返回 256（即 ratio=1.0，退化为原始静态系数）。
fn read_cpu_freq_ratio_q8() -> u32 {
    // SAFETY: ALL_PROCESSOR_GROUPS 是 Win32 约定的查询全部处理器组常量，调用无指针参数。
    let num_cpus = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) } as usize;
    if num_cpus == 0 {
        return 256;
    }

    let Some(buf_size) = num_cpus
        .checked_mul(std::mem::size_of::<ProcessorInformation>())
        .and_then(|size| u32::try_from(size).ok())
    else {
        return 256;
    };
    let mut processors = vec![ProcessorInformation::default(); num_cpus];

    // SAFETY:
    // 1. InformationLevel=ProcessorInformation 只读不写输入，传入 None 安全。
    // 2. processors 是按 ProcessorInformation 对齐的连续可写数组，缓冲区字节数
    //    由其元素数量和结构体大小经 checked_mul 计算，且已验证可表示为 u32。
    // 3. ProcessorInformation 为 repr(C)，与 Windows SDK 布局一致（24 bytes）。
    let status = unsafe {
        CallNtPowerInformation(
            ProcessorInformation,
            None,
            0,
            Some(processors.as_mut_ptr().cast()),
            buf_size,
        )
    };

    if status.0 != 0 {
        return 256;
    }

    let mut sum_current: u64 = 0;
    let mut sum_max: u64 = 0;
    for p in processors {
        // 跳过未填充的槽位（number 为 0 且频率为 0 表示此槽位无效）。
        if p.number == 0 && p.current_mhz == 0 && p.max_mhz == 0 {
            continue;
        }
        sum_current += p.current_mhz as u64;
        sum_max += p.max_mhz as u64;
    }

    if sum_max == 0 {
        return 256;
    }

    ((sum_current << 8) / sum_max).clamp(1, 256) as u32
}

/// 功率→风险分段线性映射 f(P)。
///
/// 沿 `FP_BREAKS_MW` / `FP_BREAKS_RISK` 断点插值，纯整数运算无浮点。
/// 输入负值或超出上限时分别夹紧到 0 / 100。
/// 热节点值与功率同量纲（mW 当量），稳态时节点≈输入功率，故复用此映射。
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

/// 双节点热容一步：die 跟踪功率，skin 跟踪 die（机身滞后）。
///
/// `T' = α·input + (1−α)·T`，α 为 Q8（/256）。单位均为 mW 当量。
fn step_thermal_nodes(p_mw: i32, t_die: i32, t_skin: i32) -> (i32, i32) {
    let ad = TAU_DIE_ALPHA_Q8 as i32;
    let as_ = TAU_SKIN_ALPHA_Q8 as i32;
    let new_die = (ad * p_mw + (256 - ad) * t_die) / 256;
    let new_skin = (as_ * new_die + (256 - as_) * t_skin) / 256;
    (new_die, new_skin)
}

/// 由 die/skin 节点合成风险指数（未施加降频地板）。
///
/// 以 skin（体感）为主、die 为辅；die−skin 温差提供上升/回落趋势修正。
fn risk_from_nodes(t_die: i32, t_skin: i32) -> i32 {
    let r_skin = f_p_to_risk(t_skin);
    let r_die = f_p_to_risk(t_die);
    let trend = t_die - t_skin;
    let bonus = if trend > TREND_RISE_MW {
        TREND_BONUS_UP
    } else if trend < -TREND_FALL_MW {
        TREND_BONUS_DN
    } else {
        0
    };
    (7 * r_skin + 3 * r_die) / 10 + bonus
}

/// 高占用 + 低频比 → 判定为 thermal throttle（或强限频），抬风险下限。
///
/// 返回应施加的 floor；不满足条件时返回 0（调用方 `r.max(floor)` 无影响）。
fn throttle_risk_floor(cpu_pct: i32, freq_ratio_q8: u32) -> u32 {
    if cpu_pct >= THROTTLE_CPU_PCT && freq_ratio_q8 <= THROTTLE_FREQ_Q8 {
        THROTTLE_RISK_FLOOR
    } else {
        0
    }
}

/// 状态转移意图：+1 升级，−1 降级，0 滞回带内不动。
fn transition_intent(cur: u8, r: u32) -> i8 {
    match cur {
        0 if r >= ST_COOL_TO_WARM => 1,
        1 if r >= ST_WARM_TO_HOT => 1,
        1 if r <= ST_WARM_TO_COOL => -1,
        2 if r >= ST_HOT_TO_CRIT => 1,
        2 if r <= ST_HOT_TO_WARM => -1,
        3 if r <= ST_CRIT_TO_HOT => -1,
        _ => 0,
    }
}

/// 滞回状态机一步：升级/降级均需连续同向意图达到各自 dwell 门槛。
///
/// 返回 `(next_state, new_dwell, new_intent)`。
fn step_state_machine(cur: u8, r: u32, dwell: u32, prev_intent: i8) -> (u8, u32, i8) {
    let intent = transition_intent(cur, r);
    if intent == 0 {
        return (cur, 0, 0);
    }

    let new_dwell = if intent == prev_intent {
        dwell.saturating_add(1)
    } else {
        1
    };

    let need = if intent > 0 {
        ST_DWELL_SECS
    } else {
        ST_DWELL_DOWN_SECS
    };

    if new_dwell >= need {
        let next = match (cur, intent) {
            (0, 1) => 1,
            (1, 1) => 2,
            (2, 1) => 3,
            (1, -1) => 0,
            (2, -1) => 1,
            (3, -1) => 2,
            _ => cur,
        };
        (next, 0, 0)
    } else {
        (cur, new_dwell, intent)
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
/// 开销：最多 2 次 `CallNtPowerInformation` + 1 次 `GetSystemTimes` + 纯整数运算。
/// 总开销仍远低于 1Hz 可感知阈值。
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

    // 4. 频率比：插电功耗缩放 + 全路径降频地板
    let freq_ratio_q8 = read_cpu_freq_ratio_q8();

    // 5. 估计热功率 (mW)
    let p_mw = if !ac && discharging && batt_mw > 0 {
        // 拔电放电：放电功率即系统总发热（能量守恒直测）
        batt_mw
    } else {
        // 插电或传感器失败：多信号推断
        let a_dynamic = A_CPU_MW_PER_PCT * freq_ratio_q8 as i32 / 256;
        let ku_heavy = cpu > KERNEL_GATE_CPU_PCT && ku_q8 > KU_HEAVY_THRESHOLD_Q8;
        let k = if ku_heavy { C_KERNEL_HEAVY_MW } else { 0 };
        P_IDLE_PLUG_MW + cpu * a_dynamic + mem * B_MEM_MW_PER_PCT + k
    };

    // 6. 双节点热容
    // 前 3 个样本取均值再初始化，避免启动尖峰把 skin（τ≈90s）拉高。
    if !THERMAL_INITIALIZED.load(Ordering::Relaxed) {
        let count = INIT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        INIT_SUM.fetch_add(p_mw, Ordering::Relaxed);
        if count < 3 {
            return;
        }
        let init_val = INIT_SUM.load(Ordering::Relaxed) / count as i32;
        T_DIE.store(init_val, Ordering::Relaxed);
        T_SKIN.store(init_val, Ordering::Relaxed);
        THERMAL_INITIALIZED.store(true, Ordering::Release);
        let r = risk_from_nodes(init_val, init_val)
            .clamp(0, 100)
            .max(throttle_risk_floor(cpu, freq_ratio_q8) as i32) as u32;
        THERMAL_RISK.store(r.min(100), Ordering::Release);
        return;
    }

    let prev_die = T_DIE.load(Ordering::Relaxed);
    let prev_skin = T_SKIN.load(Ordering::Relaxed);
    let (new_die, new_skin) = step_thermal_nodes(p_mw, prev_die, prev_skin);
    T_DIE.store(new_die, Ordering::Relaxed);
    T_SKIN.store(new_skin, Ordering::Relaxed);

    // 7. 风险指数 R(t) ∈ [0, 100] + 降频地板
    let r = risk_from_nodes(new_die, new_skin).clamp(0, 100) as u32;
    let r = r.max(throttle_risk_floor(cpu, freq_ratio_q8)).min(100);
    THERMAL_RISK.store(r, Ordering::Release);

    // 8. 状态机（滞回 + 升级/降级不对称驻留）
    let dwell = DWELL.load(Ordering::Relaxed);
    let prev_intent = DWELL_INTENT.load(Ordering::Relaxed);
    let cur = THERMAL_STATE.load(Ordering::Relaxed);
    let (next, new_dwell, new_intent) = step_state_machine(cur, r, dwell, prev_intent);
    THERMAL_STATE.store(next, Ordering::Release);
    DWELL.store(new_dwell, Ordering::Relaxed);
    DWELL_INTENT.store(new_intent, Ordering::Relaxed);
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
    fn test_nodes_converge_to_constant_power() {
        // 恒定功率下 die/skin 均应趋近 P。
        let p = 30000i32;
        let mut die = 0i32;
        let mut skin = 0i32;
        for _ in 0..1000 {
            let step = step_thermal_nodes(p, die, skin);
            die = step.0;
            skin = step.1;
        }
        assert!((die - p).abs() < 50, "die not converged: {die}");
        assert!((skin - p).abs() < 100, "skin not converged: {skin}");
    }

    #[test]
    fn test_skin_lags_die_on_step() {
        // 阶跃功率后 die 应领先 skin（机身蓄热滞后）。
        let p = 40000i32;
        let mut die = 0i32;
        let mut skin = 0i32;
        for _ in 0..30 {
            let step = step_thermal_nodes(p, die, skin);
            die = step.0;
            skin = step.1;
        }
        assert!(
            die > skin,
            "die should lead skin after step: die={die} skin={skin}"
        );
    }

    #[test]
    fn test_skin_holds_heat_after_power_drop() {
        // 先蓄热再掉功率：skin 回落应慢于 die（停载后仍烫一会儿）。
        let mut die = 0i32;
        let mut skin = 0i32;
        for _ in 0..200 {
            let step = step_thermal_nodes(40000, die, skin);
            die = step.0;
            skin = step.1;
        }
        for _ in 0..20 {
            let step = step_thermal_nodes(5000, die, skin);
            die = step.0;
            skin = step.1;
        }
        assert!(
            skin > die,
            "skin should retain heat after power drop: die={die} skin={skin}"
        );
    }

    #[test]
    fn test_throttle_floor_applies() {
        assert_eq!(
            throttle_risk_floor(THROTTLE_CPU_PCT, THROTTLE_FREQ_Q8),
            THROTTLE_RISK_FLOOR
        );
        assert_eq!(
            throttle_risk_floor(100, THROTTLE_FREQ_Q8),
            THROTTLE_RISK_FLOOR
        );
    }

    #[test]
    fn test_throttle_floor_not_when_idle_or_full_freq() {
        // 空闲即使低频也不抬（电源方案限频但无负载）
        assert_eq!(throttle_risk_floor(10, 80), 0);
        // 满频高占用不抬（正常性能，非 throttle 特征）
        assert_eq!(throttle_risk_floor(90, 256), 0);
        // 刚好未达门槛
        assert_eq!(
            throttle_risk_floor(THROTTLE_CPU_PCT - 1, THROTTLE_FREQ_Q8),
            0
        );
        assert_eq!(
            throttle_risk_floor(THROTTLE_CPU_PCT, THROTTLE_FREQ_Q8 + 1),
            0
        );
    }

    #[test]
    fn test_state_machine_upgrade_with_dwell() {
        // 首拍：意图建立，dwell=1，未升级
        let (s, d, i) = step_state_machine(0, 30, 0, 0);
        assert_eq!((s, d, i), (0, 1, 1));
        // 连续意图但未满 ST_DWELL_SECS
        let (s, d, i) = step_state_machine(0, 30, ST_DWELL_SECS - 2, 1);
        assert_eq!(s, 0);
        assert_eq!(d, ST_DWELL_SECS - 1);
        assert_eq!(i, 1);
        // 再一拍达标 → 升级并清零 dwell
        let (s, d, i) = step_state_machine(0, 25, ST_DWELL_SECS - 1, 1);
        assert_eq!((s, d, i), (1, 0, 0));
        let (s, _, _) = step_state_machine(1, 55, ST_DWELL_SECS - 1, 1);
        assert_eq!(s, 2);
        let (s, _, _) = step_state_machine(2, 85, ST_DWELL_SECS - 1, 1);
        assert_eq!(s, 3);
    }

    #[test]
    fn test_state_machine_downgrade_needs_dwell() {
        // 降级需要 ST_DWELL_DOWN_SECS，不再立即跳变
        let (s, d, i) = step_state_machine(1, 10, 0, 0);
        assert_eq!(s, 1);
        assert_eq!(d, 1);
        assert_eq!(i, -1);

        let (s, d, i) = step_state_machine(1, 10, ST_DWELL_DOWN_SECS - 1, -1);
        assert_eq!(s, 0);
        assert_eq!(d, 0);
        assert_eq!(i, 0);

        let (s, _, _) = step_state_machine(2, 45, ST_DWELL_DOWN_SECS - 1, -1);
        assert_eq!(s, 1);
        let (s, _, _) = step_state_machine(3, 75, ST_DWELL_DOWN_SECS - 1, -1);
        assert_eq!(s, 2);
    }

    #[test]
    fn test_state_machine_intent_reset_on_direction_change() {
        // 升级意图中途跌回滞回带再降：驻留重新计数
        let (s, d, i) = step_state_machine(1, 60, 3, 1);
        assert_eq!(s, 1);
        assert_eq!(d, 4);
        assert_eq!(i, 1);
        // 意图翻转为降级，dwell 从 1 起算
        let (s, d, i) = step_state_machine(1, 10, 4, 1);
        assert_eq!(s, 1);
        assert_eq!(d, 1);
        assert_eq!(i, -1);
    }

    #[test]
    fn test_state_machine_hysteresis() {
        // 滞回带内不抖动
        assert_eq!(transition_intent(1, 20), 0);
        assert_eq!(transition_intent(2, 50), 0);
        assert_eq!(transition_intent(3, 80), 0);
        let (s, d, i) = step_state_machine(1, 20, 100, 1);
        assert_eq!((s, d, i), (1, 0, 0));
    }

    #[test]
    fn test_state_machine_stays_in_cool() {
        assert_eq!(transition_intent(0, 10), 0);
        assert_eq!(transition_intent(0, 24), 0);
        let (s, _, _) = step_state_machine(0, 10, 100, 0);
        assert_eq!(s, 0);
    }

    #[test]
    fn test_risk_weights_skin_over_die() {
        // skin 主导：skin 高 die 低时风险应明显高于反过来的情况
        let high_skin = risk_from_nodes(10000, 40000);
        let high_die = risk_from_nodes(40000, 10000);
        assert!(
            high_skin > high_die,
            "skin should dominate risk: high_skin={high_skin} high_die={high_die}"
        );
    }

    #[test]
    fn test_real_battery_read() {
        let (ac, discharging, mw) = read_battery();
        println!("Real Battery Status:");
        println!("  AC Online  : {ac}");
        println!("  Discharging: {discharging}");
        println!("  Power (mW) : {mw}");
    }
}
