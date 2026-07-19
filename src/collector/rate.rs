//! 速率归一化与"最大流量单网卡"选择：纯函数，无 I/O，全部可单测。

use std::collections::HashMap;
use std::time::Instant;

/// 上次采样的 (入站字节, 出站字节, 采样时刻)。
/// 时刻用于在采样间隔波动时（例如断网退避从 1s 切到 15s）把累计差值
/// 归一化为"每秒字节"，避免恢复瞬间显示偏大 N 倍的虚假峰值。
pub(super) type Sample = (u64, u64, Instant);

/// 选择总流量（上行+下行）最大的单一网卡，更新历史并清理已离线 LUID。
pub(super) fn select_winner_interface(
    current_data: &HashMap<u64, (u64, u64)>,
    history: &mut HashMap<u64, Sample>,
    now: Instant,
) -> (u32, u32) {
    let mut max_total: u64 = 0;
    let mut best_speed_down: u32 = 0;
    let mut best_speed_up: u32 = 0;

    for (luid, (in_octets, out_octets)) in current_data {
        if let Some(&(prev_in, prev_out, prev_time)) = history.get(luid) {
            // 用 saturating_duration_since 而非 duration_since 以防止时间回退导致崩溃。
            let elapsed_ms = now.saturating_duration_since(prev_time).as_millis() as u64;
            let speed_down = normalize_bytes_per_sec(in_octets.saturating_sub(prev_in), elapsed_ms);
            let speed_up = normalize_bytes_per_sec(out_octets.saturating_sub(prev_out), elapsed_ms);
            let total = speed_down as u64 + speed_up as u64;

            if total > max_total {
                max_total = total;
                best_speed_down = speed_down;
                best_speed_up = speed_up;
            }
        }
    }

    // 用本次采样数据覆盖历史，附带采样时刻用于下次归一化。
    for (luid, (in_octets, out_octets)) in current_data {
        history.insert(*luid, (*in_octets, *out_octets, now));
    }
    // 清除已离线网卡的历史，防止陈旧 LUID 残留。
    history.retain(|luid, _| current_data.contains_key(luid));

    (best_speed_down, best_speed_up)
}

/// 将"累计字节差值"按实际经过的毫秒数归一化为"每秒字节"。
///
/// 正常采样间隔恒为 1 秒时，结果与直接相减一致；但当断网退避把 timer
/// 间隔从 1s 切到 15s 后，下一次采样的差值实际是 15 秒累计量，若不归一化
/// 会导致显示偏大约 15 倍的虚假峰值。
///
/// 计算全程在 `u128` 下进行：`delta_bytes` 以完整 u64 参与乘除，仅在最终
/// 落盘 u32 时才截断，避免「先截后除」在大流量 + 长间隔组合下低估真实速率。
/// `u64 * 1000` 上限约 1.8e22，远小于 u128::MAX，无溢出风险。
///
/// - `delta_bytes`：本周期累计字节增量（已 saturating_sub 过初值）。
/// - `elapsed_ms`：距上次采样的毫秒数；`max(1)` 规避零除（防御性，正常 > 0；
///   时间逆转经 `saturating_duration_since` 饱和为 0 时亦走此兜底）。
fn normalize_bytes_per_sec(delta_bytes: u64, elapsed_ms: u64) -> u32 {
    let ms = elapsed_ms.max(1) as u128;
    let scaled = delta_bytes as u128 * 1000 / ms;
    scaled.min(u32::MAX as u128) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== normalize_bytes_per_sec =====

    #[test]
    fn test_normalize_one_second_interval_matches_raw_delta() {
        // 正常 1s 间隔：归一化结果应与原始字节差值相等。
        assert_eq!(normalize_bytes_per_sec(0, 1000), 0);
        assert_eq!(normalize_bytes_per_sec(1_500_000, 1000), 1_500_000);
    }

    #[test]
    fn test_normalize_backoff_interval_no_inflation() {
        // 断网退避后 15s 间隔：15s 内累计 15MB，应为 ~1MB/s，而非 15MB/s。
        let fifteen_mb = 15 * 1024 * 1024;
        let per_sec = normalize_bytes_per_sec(fifteen_mb, 15_000);
        assert_eq!(per_sec, 1024 * 1024);

        // 同样速率在 1s 间隔下应得相同结果——归一化后与采样周期无关。
        assert_eq!(normalize_bytes_per_sec(1024 * 1024, 1000), per_sec);
    }

    #[test]
    fn test_normalize_fractional_interval() {
        // 非整秒间隔（如 1500ms）应正确按比例换算。
        // 1500ms 传 3000 字节 => 2000 B/s。
        assert_eq!(normalize_bytes_per_sec(3000, 1500), 2000);
    }

    #[test]
    fn test_normalize_zero_elapsed_does_not_panic() {
        // 防御性：elapsed_ms 为 0 时不应零除 panic，按 1ms 处理。
        // 此分支亦覆盖时间逆转经 saturating_duration_since 饱和为 0 的场景。
        assert_eq!(normalize_bytes_per_sec(5000, 0), 5_000_000);
    }

    #[test]
    fn test_normalize_saturates_at_u32_max() {
        // 巨大 delta 应仅在最终落盘 u32 时截断，而非溢出回绕。
        assert_eq!(normalize_bytes_per_sec(u64::MAX, 1000), u32::MAX);
        assert_eq!(normalize_bytes_per_sec(u32::MAX as u64, 1000), u32::MAX);
    }

    #[test]
    fn test_normalize_large_traffic_long_interval_not_truncated_early() {
        // 回归测试：万兆网 × 15s 退避 = 累计 ~18.75GB（超出 u32::MAX ≈ 4.29GB）。
        // u128 中转后应正确反映每秒速率 ~1.25GB/s，而非被「先截后除」压到 ~286MB/s。
        // 注意：1.25GB/s 已超 u32::MAX（~4.29GB/s 的 B/s 表达 = 4_294_967_295 B/s），
        // 实际 18.75GB/15s = 1_342_177_280 B/s < u32::MAX，应精确命中。
        let eighteen_gb: u64 = 18 * 1024 * 1024 * 1024 + (750 * 1024 * 1024);
        let per_sec = normalize_bytes_per_sec(eighteen_gb, 15_000);
        assert_eq!(per_sec, (eighteen_gb * 1000 / 15_000) as u32);
        // 关键断言：绝不能是旧「先截后除」的 ~286MB/s。
        assert!(
            per_sec > 1_000_000_000,
            "expected >1GB/s, got {per_sec} (early truncation regression)"
        );
    }

    #[test]
    fn test_instant_saturating_duration_since_does_not_panic_on_time_regression() {
        // 回归守护：确认标准库在时间逆转时走 saturating 路径而非 panic。
        // 无法直接构造 now < prev 的 Instant，但可断言同瞬时下返回 0 Duration，
        // 证明我们用的是不会 panic 的 saturating 变体（duration_since 同参也返回 0，
        // 真正差异在逆转行为，此处至少锁定 API 选择不被误改回 duration_since）。
        let t = Instant::now();
        assert_eq!(t.saturating_duration_since(t).as_millis(), 0);
    }

    // ===== select_winner_interface =====

    #[test]
    fn test_select_winner_interface_multiple_active() {
        // 两张网卡同时有流量，应选出总流量最大的那一张。
        let mut history = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + std::time::Duration::from_secs(1);

        // 网卡 1 (LUID = 100): 1s 内增量 1000 字节，下行 800，上行 200 => 1000 B/s
        // 网卡 2 (LUID = 200): 1s 内增量 2000 字节，下行 1500，上行 500 => 2000 B/s
        history.insert(100, (10000, 5000, t0));
        history.insert(200, (20000, 10000, t0));

        let mut current = HashMap::new();
        current.insert(100, (10800, 5200));
        current.insert(200, (21500, 10500));

        let (down, up) = select_winner_interface(&current, &mut history, t1);
        assert_eq!(down, 1500);
        assert_eq!(up, 500);
    }

    #[test]
    fn test_select_winner_interface_first_appearance() {
        // 某张网卡首次出现，没有历史，不应该计算出任何速度（返回 0）。
        let mut history = HashMap::new();
        let t0 = Instant::now();

        // 网卡 1 首次出现
        let mut current = HashMap::new();
        current.insert(100, (5000, 2000));

        let (down, up) = select_winner_interface(&current, &mut history, t0);
        assert_eq!(down, 0);
        assert_eq!(up, 0);

        // 但此时它的数据应该已经被正确记入历史中
        assert!(history.contains_key(&100));
        assert_eq!(history.get(&100).unwrap().0, 5000);
    }

    #[test]
    fn test_select_winner_interface_counter_rollback() {
        // 网卡计数器回退（例如网卡重置或溢出），不应溢出崩溃，通过 saturating_sub 速度返回 0
        let mut history = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + std::time::Duration::from_secs(1);

        // 网卡 1 (LUID = 100): 历史值为 10000/5000
        history.insert(100, (10000, 5000, t0));

        // 当前值回退到 8000/4000
        let mut current = HashMap::new();
        current.insert(100, (8000, 4000));

        let (down, up) = select_winner_interface(&current, &mut history, t1);
        assert_eq!(down, 0);
        assert_eq!(up, 0);
    }

    #[test]
    fn test_select_winner_interface_offline_removed() {
        // 网卡下线/消失后，历史记录中应该不再保留其 LUID
        let mut history = HashMap::new();
        let t0 = Instant::now();

        history.insert(100, (10000, 5000, t0));
        history.insert(200, (20000, 10000, t0));

        // 当前只剩 200，100 已下线
        let mut current = HashMap::new();
        current.insert(200, (21000, 10500));

        let _ = select_winner_interface(&current, &mut history, t0);
        assert!(!history.contains_key(&100), "已下线的网卡历史记录应被清除");
        assert!(history.contains_key(&200), "在线的网卡历史记录应保留");
    }

    #[test]
    fn test_select_winner_interface_backoff_scale() {
        // 15 秒退避恢复：经过 15s 后，即使累计流量很大，也应正确进行时间除法，求得每秒平均速度。
        let mut history = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + std::time::Duration::from_secs(15);

        // 网卡 1 (LUID = 100): 15秒内下行增量 150,000 字节，上行 30,000 字节 => 平均每秒 10,000/2,000
        history.insert(100, (10000, 5000, t0));

        let mut current = HashMap::new();
        current.insert(100, (160000, 35000));

        let (down, up) = select_winner_interface(&current, &mut history, t1);
        assert_eq!(down, 10000);
        assert_eq!(up, 2000);
    }
}
