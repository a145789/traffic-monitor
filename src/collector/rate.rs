//! 速率归一化与"最大流量单网卡"选择：纯函数，无 I/O，全部可单测。
//!
//! TODO(no-instant-rewind): 本模块是仓库里唯一成片做 `Instant` 差分的地方，因此把这条
//! 约束留在这里——**禁止用 `Instant - Duration` 表达「过去的时刻」**。Windows 上 `Instant`
//! 以 QPC 为原点（自系统启动计数），开机初期回推会 panic，而 release 是 `panic = "abort"`，
//! 整个常驻进程会静默消失。需要「更早的时刻」时改用 deadline 语义（存「下次该做什么的时刻」
//! 而不是「上次做过什么」），或 `checked_sub` 后显式处理 `None`。活例见 `update::next_check_deadline`。

use std::collections::HashMap;
use std::time::Instant;

/// 上次采样的 (入站字节, 出站字节, 采样时刻)。
/// 时刻用于在采样间隔波动时（例如断网退避从 1s 切到 15s）把累计差值
/// 归一化为"每秒字节"，避免恢复瞬间显示偏大 N 倍的虚假峰值。
pub(super) type Sample = (u64, u64, Instant);

/// 选择总流量（上行+下行）最大的单一网卡，更新历史并清理已离线 LUID。
/// 每 tick 独立重新择大，无跨周期赢家粘滞、无多卡累加（见 AGENTS.md 第 3 条）。
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

    for (luid, (in_octets, out_octets)) in current_data {
        history.insert(*luid, (*in_octets, *out_octets, now));
    }
    history.retain(|luid, _| current_data.contains_key(luid));

    (best_speed_down, best_speed_up)
}

/// 将"累计字节差值"按实际经过的毫秒数归一化为"每秒字节"。
///
///
/// 计算全程在 `u128` 下进行：`delta_bytes` 以完整 u64 参与乘除，仅在最终
/// 落盘 u32 时才截断，避免「先截后除」在大流量 + 长间隔组合下低估真实速率。
/// `u64 * 1000` 上限约 1.8e22，远小于 u128::MAX，无溢出风险。
///
/// - `delta_bytes`：本周期累计字节增量（已 saturating_sub 过初值）。
/// - `elapsed_ms`：距上次采样的毫秒数；`max(1)` 是纯零除兜底（正常采样间隔恒 > 0）。
fn normalize_bytes_per_sec(delta_bytes: u64, elapsed_ms: u64) -> u32 {
    let ms = elapsed_ms.max(1) as u128;
    let scaled = delta_bytes as u128 * 1000 / ms;
    scaled.min(u32::MAX as u128) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_one_second_interval_matches_raw_delta() {
        assert_eq!(normalize_bytes_per_sec(0, 1000), 0);
        assert_eq!(normalize_bytes_per_sec(1_500_000, 1000), 1_500_000);
    }

    #[test]
    fn test_normalize_backoff_interval_no_inflation() {
        let fifteen_mb = 15 * 1024 * 1024;
        let per_sec = normalize_bytes_per_sec(fifteen_mb, 15_000);
        assert_eq!(per_sec, 1024 * 1024);

        assert_eq!(normalize_bytes_per_sec(1024 * 1024, 1000), per_sec);
    }

    #[test]
    fn test_normalize_fractional_interval() {
        assert_eq!(normalize_bytes_per_sec(3000, 1500), 2000);
    }

    #[test]
    fn test_normalize_zero_elapsed_does_not_panic() {
        // 这是 max(1) 零除兜底的唯一覆盖；时间逆转只会被 saturating_duration_since
        // 饱和为 0 后落到同一分支，无法直接构造 now < prev 的 Instant 单独验证。
        assert_eq!(normalize_bytes_per_sec(5000, 0), 5_000_000);
    }

    #[test]
    fn test_normalize_saturates_at_u32_max() {
        assert_eq!(normalize_bytes_per_sec(u64::MAX, 1000), u32::MAX);
        assert_eq!(normalize_bytes_per_sec(u32::MAX as u64, 1000), u32::MAX);
    }

    #[test]
    fn test_normalize_large_traffic_long_interval_not_truncated_early() {
        // 回归测试：15 秒累计量约 18.7 GiB，超过 u32::MAX；归一化后的速率约 1.34 GB/s，
        // 仍低于 u32::MAX。u128 中转可避免「先截后除」把结果压到约 286 MB/s。
        let eighteen_gb: u64 = 18 * 1024 * 1024 * 1024 + (750 * 1024 * 1024);
        let per_sec = normalize_bytes_per_sec(eighteen_gb, 15_000);
        assert_eq!(per_sec, (eighteen_gb * 1000 / 15_000) as u32);
        assert!(
            per_sec > 1_000_000_000,
            "expected >1GB/s, got {per_sec} (early truncation regression)"
        );
    }

    #[test]
    fn test_select_winner_interface_multiple_active() {
        let mut history = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + std::time::Duration::from_secs(1);

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
        let mut history = HashMap::new();
        let t0 = Instant::now();

        let mut current = HashMap::new();
        current.insert(100, (5000, 2000));

        let (down, up) = select_winner_interface(&current, &mut history, t0);
        assert_eq!(down, 0);
        assert_eq!(up, 0);

        assert!(history.contains_key(&100));
        assert_eq!(history.get(&100).unwrap().0, 5000);

        // 空输入恒返回零值：network.rs 的断网判定只查 is_empty，蕴含依赖于此。
        assert_eq!(
            select_winner_interface(&HashMap::new(), &mut HashMap::new(), t0),
            (0, 0)
        );
    }

    #[test]
    fn test_select_winner_interface_counter_rollback() {
        let mut history = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + std::time::Duration::from_secs(1);

        history.insert(100, (10000, 5000, t0));

        let mut current = HashMap::new();
        current.insert(100, (8000, 4000));

        let (down, up) = select_winner_interface(&current, &mut history, t1);
        assert_eq!(down, 0);
        assert_eq!(up, 0);
    }

    #[test]
    fn test_select_winner_interface_offline_removed() {
        let mut history = HashMap::new();
        let t0 = Instant::now();

        history.insert(100, (10000, 5000, t0));
        history.insert(200, (20000, 10000, t0));

        let mut current = HashMap::new();
        current.insert(200, (21000, 10500));

        let _ = select_winner_interface(&current, &mut history, t0);
        assert!(!history.contains_key(&100), "已下线的网卡历史记录应被清除");
        assert!(history.contains_key(&200), "在线的网卡历史记录应保留");
    }

    #[test]
    fn test_select_winner_interface_backoff_scale() {
        let mut history = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + std::time::Duration::from_secs(15);

        history.insert(100, (10000, 5000, t0));

        let mut current = HashMap::new();
        current.insert(100, (160000, 35000));

        let (down, up) = select_winner_interface(&current, &mut history, t1);
        assert_eq!(down, 10000);
        assert_eq!(up, 2000);
    }
}
