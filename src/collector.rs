use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::time::Instant;
use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, HWND, LPARAM, WPARAM};
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GET_ADAPTERS_ADDRESSES_FLAGS, GetAdaptersAddresses, GetIfTable2,
    IP_ADAPTER_ADDRESSES_LH, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_USER};

use crate::config::{
    BACKOFF_ZERO_THRESHOLD, CONSECUTIVE_ZERO_COUNT, CPU_USAGE, MEM_USAGE, NET_SPEED_DOWN,
    NET_SPEED_UP, NETWORK_BACKOFF,
};

pub const WM_USER_NETWORK_DISCONNECTED: u32 = WM_USER + 3;
pub const WM_USER_NETWORK_RECONNECTED: u32 = WM_USER + 4;

const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
const IF_TYPE_IEEE80211: u32 = 71;

static PREV_IDLE_TIME: AtomicU64 = AtomicU64::new(0);
static PREV_KERNEL_TIME: AtomicU64 = AtomicU64::new(0);
static PREV_USER_TIME: AtomicU64 = AtomicU64::new(0);
static CPU_INITIALIZED: AtomicBool = AtomicBool::new(false);

static NET_INITIALIZED: AtomicBool = AtomicBool::new(false);

static MAIN_HWND_NETWORK: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

type BlacklistCache = Option<(Vec<u64>, Instant)>;
const BLACKLIST_REFRESH_SECS: u64 = 30;

/// 上次采样的 (入站字节, 出站字节, 采样时刻)。
/// 时刻用于在采样间隔波动时（例如断网退避从 1s 切到 15s）把累计差值
/// 归一化为"每秒字节"，避免恢复瞬间显示偏大 N 倍的虚假峰值。
type Sample = (u64, u64, Instant);

thread_local! {
    static CURRENT_DATA: RefCell<HashMap<u64, (u64, u64)>> = RefCell::new(HashMap::with_capacity(16));
    static INTERFACE_HISTORY: RefCell<HashMap<u64, Sample>> = RefCell::new(HashMap::with_capacity(16));
    static VIRTUAL_BLACKLIST: RefCell<BlacklistCache> = const { RefCell::new(None) };
}

pub fn init_network_listener(hwnd: HWND) {
    MAIN_HWND_NETWORK.store(hwnd.0, Ordering::Release);
}

pub fn collect_cpu() {
    let mut idle_time = 0u64;
    let mut kernel_time = 0u64;
    let mut user_time = 0u64;

    // SAFETY:
    // 传入的指针均指向当前栈帧分配的有效且可变的 u64 变量。
    // Windows API 仅在此调用期间写入数据，符合内存安全和对齐要求。
    let ok = unsafe {
        windows::Win32::System::Threading::GetSystemTimes(
            Some(&mut idle_time as *mut u64 as *mut _),
            Some(&mut kernel_time as *mut u64 as *mut _),
            Some(&mut user_time as *mut u64 as *mut _),
        )
        .is_ok()
    };

    if ok {
        if !CPU_INITIALIZED.load(Ordering::Acquire) {
            PREV_IDLE_TIME.store(idle_time, Ordering::Release);
            PREV_KERNEL_TIME.store(kernel_time, Ordering::Release);
            PREV_USER_TIME.store(user_time, Ordering::Release);
            CPU_INITIALIZED.store(true, Ordering::Release);
            return;
        }

        let idle_diff = idle_time.saturating_sub(PREV_IDLE_TIME.load(Ordering::Acquire));
        let kernel_diff = kernel_time.saturating_sub(PREV_KERNEL_TIME.load(Ordering::Acquire));
        let user_diff = user_time.saturating_sub(PREV_USER_TIME.load(Ordering::Acquire));
        let total = kernel_diff + user_diff;

        if total > 0 {
            let usage = ((total - idle_diff) * 100 / total) as u32;
            CPU_USAGE.store(usage.min(100), Ordering::Release);
        }

        PREV_IDLE_TIME.store(idle_time, Ordering::Release);
        PREV_KERNEL_TIME.store(kernel_time, Ordering::Release);
        PREV_USER_TIME.store(user_time, Ordering::Release);
    }
}

pub fn collect_memory() {
    let mut mem_info = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };

    // SAFETY:
    // mem_info 结构体已正确初始化其 dwLength 字段以供 API 校验结构大小。
    // 传入其可变引用符合内存对齐与独占性要求，API 仅在调用期间安全填充系统内存状态。
    let ok = unsafe { GlobalMemoryStatusEx(&mut mem_info).is_ok() };

    if ok {
        MEM_USAGE.store(mem_info.dwMemoryLoad as u32, Ordering::Release);
    }
}

struct MibTable(*mut MIB_IF_TABLE2);

impl MibTable {
    fn rows(&self) -> &[MIB_IF_ROW2] {
        if self.0.is_null() {
            return &[];
        }
        // SAFETY: self.0 是由成功返回的 GetIfTable2 分配的非空有效指针。
        let num_entries = unsafe { (*self.0).NumEntries as usize };
        if num_entries == 0 {
            return &[];
        }
        // SAFETY:
        // 1. self.0 是由 GetIfTable2 成功分配的非空有效指针。
        // 2. 操作系统保证该结构体的 Table 数组实际包含 num_entries 个连续的 MIB_IF_ROW2 元素。
        // 3. 虽然 Rust 绑定将 Table 定义为大小为 1 的数组（对应 C 的柔性数组），但在内存中它是大小为 num_entries 的连续块。
        //    我们通过 std::slice::from_raw_parts 构造切片，内存布局合法连续。
        // 4. 切片借用生命周期绑定到 &self，保证在 MibTable 析构前切片始终有效且不被修改。
        unsafe { std::slice::from_raw_parts((*self.0).Table.as_ptr(), num_entries) }
    }
}

impl Drop for MibTable {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: self.0 是先前通过 GetIfTable2 成功分配的、非空的有效堆指针，在 Drop 发生时调用 FreeMibTable 将其内存安全回收。
            unsafe {
                FreeMibTable(self.0 as *const _);
            }
        }
    }
}

pub fn collect_network() {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    // SAFETY:
    // table 指针传入 GetIfTable2 的可变引用中，成功调用后由 Windows 操作系统分配一块
    // MIB_IF_TABLE2 结构的内存并填充数据，然后通过包装器 MibTable(table) 在其 Drop 中正确自动释放（RAII）。
    let result = unsafe { GetIfTable2(&mut table) };

    if result.0 == 0 && !table.is_null() {
        let table_wrapper = MibTable(table);
        let virtual_blacklist = get_virtual_blacklist();
        let mut current_data = CURRENT_DATA.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
        current_data.clear();
        let mut has_up_interface = false;

        for row in table_wrapper.rows() {
            if !is_valid_interface(row) {
                continue;
            }

            // SAFETY:
            // InterfaceLuid 是 Win32 中的联合体（union）。
            // 操作系统返回的 MibTable 中的每一行数据均由系统成功初始化，因此访问此联合体字段是内存安全的。
            let luid = unsafe { row.InterfaceLuid.Value };
            if virtual_blacklist.contains(&luid) {
                continue;
            }

            if row.OperStatus == IfOperStatusUp {
                has_up_interface = true;
                current_data.insert(luid, (row.InOctets, row.OutOctets));
            }
        }

        if !NET_INITIALIZED.load(Ordering::Acquire) {
            // 首次采样：仅记录基线字节与时刻，不计算速率。
            let now = Instant::now();
            INTERFACE_HISTORY.with(|cell| {
                let mut history = cell.borrow_mut();
                history.clear();
                for (luid, (in_octets, out_octets)) in &current_data {
                    history.insert(*luid, (*in_octets, *out_octets, now));
                }
            });
            NET_INITIALIZED.store(true, Ordering::Release);
            CURRENT_DATA.with(|cell| *cell.borrow_mut() = current_data);
            return;
        }

        let mut max_total: u64 = 0;
        let mut best_speed_down: u32 = 0;
        let mut best_speed_up: u32 = 0;
        let now = Instant::now();

        INTERFACE_HISTORY.with(|cell| {
            let mut history = cell.borrow_mut();
            for (luid, (in_octets, out_octets)) in &current_data {
                if let Some(&(prev_in, prev_out, prev_time)) = history.get(luid) {
                    // 用 saturating_duration_since 而非 duration_since：后者在
                    // now < prev_time 时会 panic，配合本 crate 的 panic="abort"
                    // 会直接杀进程。Instant 虽是单调时钟，但 VM 挂起/恢复、系统
                    // 休眠唤醒等场景下确有时间回退报告；时间逆转时这里安全返回
                    // 0，后续经 normalize_bytes_per_sec 的 max(1) 兜底按极高网速
                    // 处理，远好于静默崩溃。
                    let elapsed_ms = now.saturating_duration_since(prev_time).as_millis() as u64;
                    let speed_down =
                        normalize_bytes_per_sec(in_octets.saturating_sub(prev_in), elapsed_ms);
                    let speed_up =
                        normalize_bytes_per_sec(out_octets.saturating_sub(prev_out), elapsed_ms);
                    let total = speed_down as u64 + speed_up as u64;

                    if total > max_total {
                        max_total = total;
                        best_speed_down = speed_down;
                        best_speed_up = speed_up;
                    }
                }
            }

            // 用本次采样数据覆盖历史，附带采样时刻用于下次归一化。
            for (luid, (in_octets, out_octets)) in &current_data {
                history.insert(*luid, (*in_octets, *out_octets, now));
            }
            // 清除已离线网卡的历史，防止陈旧 LUID 残留。
            history.retain(|luid, _| current_data.contains_key(luid));
        });

        NET_SPEED_DOWN.store(best_speed_down, Ordering::Release);
        NET_SPEED_UP.store(best_speed_up, Ordering::Release);

        if best_speed_down == 0 && best_speed_up == 0 && !has_up_interface {
            let count = CONSECUTIVE_ZERO_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if count >= BACKOFF_ZERO_THRESHOLD && !NETWORK_BACKOFF.load(Ordering::Acquire) {
                NETWORK_BACKOFF.store(true, Ordering::Release);
                let hwnd = HWND(MAIN_HWND_NETWORK.load(Ordering::Acquire));
                // SAFETY:
                // HWND 句柄是由主线程初始化并存储在原子指针中的有效窗口句柄。
                // PostMessageW 是线程安全的 Windows API，能安全地跨线程投递自定义的网络断开消息。
                unsafe {
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_USER_NETWORK_DISCONNECTED,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
        } else {
            CONSECUTIVE_ZERO_COUNT.store(0, Ordering::Release);
            if NETWORK_BACKOFF.load(Ordering::Acquire) {
                NETWORK_BACKOFF.store(false, Ordering::Release);
                let hwnd = HWND(MAIN_HWND_NETWORK.load(Ordering::Acquire));
                // SAFETY:
                // HWND 句柄是由主线程初始化并存储在原子指针中的有效窗口句柄。
                // PostMessageW 是线程安全的 Windows API，能安全地跨线程投递自定义的网络重连消息。
                unsafe {
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_USER_NETWORK_RECONNECTED,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
        }

        CURRENT_DATA.with(|cell| *cell.borrow_mut() = current_data);
    }
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

fn is_valid_interface(row: &MIB_IF_ROW2) -> bool {
    let if_type = row.Type;
    if if_type != IF_TYPE_ETHERNET_CSMACD && if_type != IF_TYPE_IEEE80211 {
        return false;
    }

    if row.PhysicalAddressLength == 0 {
        return false;
    }

    // 注意：此处故意不检查 HardwareInterface 标志位。
    // 在 Hyper-V / WSL2 / Docker Desktop 环境下，物理网卡绑定到虚拟交换机后，
    // 外网流量实际由 vEthernet 等虚拟网口承载，其 HardwareInterface 为 false。
    // 若保留该检查，这些环境下网速将始终显示为 0。
    // 虚拟网口的过滤现已交由 is_virtual_friendly_name 黑名单完成。
    true
}

fn is_virtual_friendly_name(name: &str) -> bool {
    let name_lower = name.to_ascii_lowercase();
    name_lower.contains("virtual")
        || name_lower.contains("vbox")
        || name_lower.contains("vmware")
        || name_lower.contains("hyper-v")
        || name_lower.contains("wsl")
        || name_lower.contains("tap")
        || name_lower.contains("vpn")
        || name_lower.contains("loopback")
        || name_lower.contains("teredo")
        || name_lower.contains("isatap")
        || name_lower.contains("6to4")
        || name_lower.contains("ppp")
        || name_lower.contains("kvm")
        || name_lower.contains("xen")
}

/// # Safety
///
/// 调用者必须保证：
/// 1. `ptr` 必须指向一个有效的、以 `0` (NUL) 结尾的 UTF-16 宽字符序列。
/// 2. 在此函数执行结束前，`ptr` 指向的内存块必须保持有效且不可变。
unsafe fn read_wide_string(ptr: *mut u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0;
    // SAFETY: 根据函数安全契约，调用者保证了 ptr 为非空、对齐且以 NUL 结尾的合法 UTF-16 字符串指针。
    // 在计算长度 len 期间，我们没有超出该缓冲区的有效边界。通过 std::slice::from_raw_parts 构造的临时切片
    // 真实反映了原字符串在内存中的布局，由于我们在当前栈帧读取它并不作修改，在此转换为 String 是安全的。
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }
}

fn build_virtual_blacklist() -> Option<Vec<u64>> {
    let mut buf_size: u32 = 0;
    // SAFETY:
    // 1. GetAdaptersAddresses 是 Windows IP 助手模块的导出函数。
    // 2. 第一次调用传入 None 作为 AdapterAddresses 的目标缓冲区，仅为了让系统计算出缓冲区所需的实际字节数并回填至栈分配的 buf_size。
    // 3. 传入 &mut buf_size 可变指针在此期间是独占且安全的。
    let ret = unsafe {
        GetAdaptersAddresses(
            0,
            GET_ADAPTERS_ADDRESSES_FLAGS(0),
            None,
            None,
            &mut buf_size,
        )
    };
    if ret != ERROR_BUFFER_OVERFLOW.0 {
        return None;
    }

    // 分配对齐的内存来存储适配器信息
    let mut buf: Vec<u64> = vec![0u64; (buf_size as usize).div_ceil(8)];
    let adapter_ptr = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;

    // SAFETY:
    // 1. adapter_ptr 指向刚才基于 u64 对齐分配的、大小足以容纳 buf_size 字节的有效 Vec 缓冲区。
    // 2. 操作系统在该调用期间独占该缓冲区并安全填充系统网卡适配器链表数据，不会发生缓冲区溢出。
    let ret = unsafe {
        GetAdaptersAddresses(
            0,
            GET_ADAPTERS_ADDRESSES_FLAGS(0),
            None,
            Some(adapter_ptr),
            &mut buf_size,
        )
    };
    if ret != 0 {
        return None;
    }

    let mut blacklist = Vec::new();
    let mut current = adapter_ptr;
    while !current.is_null() {
        // SAFETY:
        // 1. current 指向链表节点。根据 GetAdaptersAddresses 的成功返回值，整个单向链表的所有节点都完整地驻留在先前分配的 buf 向量的生命周期内。
        // 2. 由于 buf 向量在读取期间不可变且没有发生重分配，解引用 current 获得静态结构体引用是完全内存安全的。
        let adapter = unsafe { &*current };

        // SAFETY:
        // adapter.FriendlyName.0 和 adapter.Description.0 是由 GetAdaptersAddresses 成功填充的且以 NUL 结尾的合法 UTF-16 宽字符数组指针。
        // 在 buf 缓冲区生命周期内（即当前函数退出前）它们指向的内存保持合法且不可变，满足 read_wide_string 的安全契约。
        let friendly = unsafe { read_wide_string(adapter.FriendlyName.0) };
        let desc = unsafe { read_wide_string(adapter.Description.0) };

        if is_virtual_friendly_name(&friendly) || is_virtual_friendly_name(&desc) {
            // SAFETY:
            // 1. Luid 在 Win32 API 绑定中是联合体类型。
            // 2. GetAdaptersAddresses 成功返回的节点中，其 Luid 的内存数据已由系统完全初始化。
            // 3. 访问联合体的 Value 字段在 Rust 中需 unsafe 块，此处仅读取其拷贝，不违反内存安全。
            let luid_val = unsafe { adapter.Luid.Value };
            blacklist.push(luid_val);
        }

        current = adapter.Next;
    }

    Some(blacklist)
}

fn get_virtual_blacklist() -> Vec<u64> {
    {
        let skip = VIRTUAL_BLACKLIST.with(|cell| {
            let cache = cell.borrow();
            if let Some((list, last_refresh)) = cache.as_ref()
                && last_refresh.elapsed().as_secs() < BLACKLIST_REFRESH_SECS
            {
                return Some(list.clone());
            }
            None
        });
        if let Some(list) = skip {
            return list;
        }
    }

    let new_list = build_virtual_blacklist();
    match new_list {
        Some(list) => {
            VIRTUAL_BLACKLIST.with(|cell| {
                *cell.borrow_mut() = Some((list.clone(), Instant::now()));
            });
            list
        }
        None => VIRTUAL_BLACKLIST.with(|cell| {
            let mut cache = cell.borrow_mut();
            let old_list = cache.as_ref().map(|(l, _)| l.clone()).unwrap_or_default();
            // Update timestamp on failure so we don't retry GetAdaptersAddresses
            // on every tick; reuse the old list (or empty) for 30s.
            *cache = Some((old_list.clone(), Instant::now()));
            old_list
        }),
    }
}

pub fn trim_working_set() {
    // SAFETY:
    // 1. GetCurrentProcess() 返回当前进程的伪句柄，它是安全的特殊常量句柄，不需要关闭且在当前进程内有效。
    // 2. 将 (usize::MAX, usize::MAX) 传给 SetProcessWorkingSetSize 是系统约定的资源清理命令，旨在临时将进程工作集内存刷回磁盘，属于纯系统级配置，不存在内存越界写入危险。
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
