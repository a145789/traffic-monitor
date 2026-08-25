//! 网卡流量采样：接口过滤、虚拟网卡黑名单缓存、断网/恢复判定。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::Instant;
use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, HWND, LPARAM, WPARAM};
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GET_ADAPTERS_ADDRESSES_FLAGS, GetAdaptersAddresses, GetIfTable2,
    IP_ADAPTER_ADDRESSES_LH, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use super::rate::{Sample, select_winner_interface};
use crate::config::{
    BACKOFF_ZERO_THRESHOLD, BLACKLIST_REFRESH_SECS, WM_USER_NETWORK_DISCONNECTED,
    WM_USER_NETWORK_RECONNECTED,
};
use crate::state::{CONSECUTIVE_ZERO_COUNT, NET_SPEED_DOWN, NET_SPEED_UP, NETWORK_BACKOFF};

const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
const IF_TYPE_IEEE80211: u32 = 71;

static NET_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// 主窗口句柄（isize 存储），断网/恢复时向 UI 线程投递消息。
static MAIN_HWND_NETWORK: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    static CURRENT_DATA: RefCell<HashMap<u64, (u64, u64)>> = RefCell::new(HashMap::with_capacity(16));
    static INTERFACE_HISTORY: RefCell<HashMap<u64, Sample>> = RefCell::new(HashMap::with_capacity(16));
    static VIRTUAL_BLACKLIST: RefCell<Option<(HashSet<u64>, Instant)>> = const { RefCell::new(None) };
}

pub fn init_network_listener(hwnd: HWND) {
    MAIN_HWND_NETWORK.store(hwnd.0 as isize, Ordering::Release);
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
        // SAFETY: Table 是 C 柔性数组，实际为 num_entries 个连续 MIB_IF_ROW2；
        // 切片借用绑定 &self，MibTable 析构前始终有效。
        unsafe { std::slice::from_raw_parts((*self.0).Table.as_ptr(), num_entries) }
    }
}

impl Drop for MibTable {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: self.0 由 GetIfTable2 分配，FreeMibTable 配对释放。
            unsafe {
                FreeMibTable(self.0 as *const _);
            }
        }
    }
}

pub fn collect_network() {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    // SAFETY: 成功时 OS 分配表，由 MibTable Drop → FreeMibTable 释放。
    let result = unsafe { GetIfTable2(&mut table) };

    if result.0 != 0 || table.is_null() {
        return;
    }

    let table_wrapper = MibTable(table);

    with_virtual_blacklist(|virtual_blacklist| {
        let mut has_up_interface = false;

        CURRENT_DATA.with(|cell| {
            let mut current_data = cell.borrow_mut();
            current_data.clear();

            for row in table_wrapper.rows() {
                if !is_valid_interface(row) {
                    continue;
                }

                // SAFETY: 系统已初始化的 InterfaceLuid 联合体，只读 Value。
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
                // 首次采样：只记基线，不算速率。
                let now = Instant::now();
                INTERFACE_HISTORY.with(|hist| {
                    let mut history = hist.borrow_mut();
                    history.clear();
                    for (luid, (in_octets, out_octets)) in current_data.iter() {
                        history.insert(*luid, (*in_octets, *out_octets, now));
                    }
                });
                NET_INITIALIZED.store(true, Ordering::Release);
                return;
            }

            let now = Instant::now();
            let (best_speed_down, best_speed_up) = INTERFACE_HISTORY
                .with(|hist| select_winner_interface(&current_data, &mut hist.borrow_mut(), now));

            NET_SPEED_DOWN.store(best_speed_down, Ordering::Relaxed);
            NET_SPEED_UP.store(best_speed_up, Ordering::Relaxed);

            if best_speed_down == 0 && best_speed_up == 0 && !has_up_interface {
                let count = CONSECUTIVE_ZERO_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                if count >= BACKOFF_ZERO_THRESHOLD && !NETWORK_BACKOFF.load(Ordering::Acquire) {
                    NETWORK_BACKOFF.store(true, Ordering::Release);
                    post_to_main(WM_USER_NETWORK_DISCONNECTED);
                }
            } else {
                CONSECUTIVE_ZERO_COUNT.store(0, Ordering::Relaxed);
                if NETWORK_BACKOFF.load(Ordering::Acquire) {
                    NETWORK_BACKOFF.store(false, Ordering::Release);
                    post_to_main(WM_USER_NETWORK_RECONNECTED);
                }
            }
        });
    });
}

/// 向主窗口投递网络状态消息（断网退避/恢复）。
fn post_to_main(msg: u32) {
    let hwnd = HWND(MAIN_HWND_NETWORK.load(Ordering::Acquire) as *mut std::ffi::c_void);
    // SAFETY: PostMessageW 只投递消息，线程安全；窗口已销毁时返回错误。
    unsafe {
        let _ = PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0));
    }
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
/// 调用者必须保证 `ptr` 指向有效的、以 NUL 结尾的 UTF-16 宽字符序列，
/// 且在本函数返回前该内存保持有效且不可变。
unsafe fn read_wide_string(ptr: *mut u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0;
    // SAFETY: 按函数契约在 NUL 终止的缓冲区边界内扫描。
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }
}

fn build_virtual_blacklist() -> Option<HashSet<u64>> {
    let mut buf_size: u32 = 0;
    // SAFETY: 首次调用传 None 缓冲区，仅让系统回填所需字节数到 buf_size。
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

    // 以 u64 分配保证结构体对齐。
    let mut buf: Vec<u64> = vec![0u64; (buf_size as usize).div_ceil(8)];
    let adapter_ptr = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;

    // SAFETY: adapter_ptr 指向大小为 buf_size 字节的 u64 对齐缓冲区，调用期间独占。
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

    let mut blacklist = HashSet::new();
    let mut current = adapter_ptr;
    while !current.is_null() {
        // SAFETY: 链表节点完整驻留在 buf 生命周期内，期间 buf 不可变且无重分配。
        let adapter = unsafe { &*current };

        // SAFETY: FriendlyName/Description 由 API 成功填充，为 NUL 结尾宽字符串，
        // 在 buf 生命周期内有效，满足 read_wide_string 契约。
        let friendly = unsafe { read_wide_string(adapter.FriendlyName.0) };
        let desc = unsafe { read_wide_string(adapter.Description.0) };

        if is_virtual_friendly_name(&friendly) || is_virtual_friendly_name(&desc) {
            // SAFETY: 成功返回的节点中 Luid 联合体已完全初始化，此处只读其拷贝。
            let luid_val = unsafe { adapter.Luid.Value };
            blacklist.insert(luid_val);
        }

        current = adapter.Next;
    }

    Some(blacklist)
}

/// 在 UI 线程上以不可变借用读取黑名单；过期时先就地重建缓存。
///
/// 闭包执行期间持有 `VIRTUAL_BLACKLIST` 的不可变借用，闭包内禁止再次调用本函数。
fn with_virtual_blacklist<R>(f: impl FnOnce(&HashSet<u64>) -> R) -> R {
    VIRTUAL_BLACKLIST.with(|cell| {
        let now = Instant::now();
        if blacklist_needs_refresh(&cell.borrow(), now) {
            let mut cache = cell.borrow_mut();
            rebuild_virtual_blacklist(&mut cache, now, build_virtual_blacklist);
        }

        // 重建成功或失败回退后缓存必然为 Some；该分支只是类型层面的空名单兜底。
        let cache = cell.borrow();
        let Some((blacklist, _)) = cache.as_ref() else {
            return f(&HashSet::new());
        };
        f(blacklist)
    })
}

fn blacklist_needs_refresh(cache: &Option<(HashSet<u64>, Instant)>, now: Instant) -> bool {
    cache.as_ref().is_none_or(|(_, last_refresh)| {
        now.saturating_duration_since(*last_refresh).as_secs() >= BLACKLIST_REFRESH_SECS
    })
}

/// 重建黑名单缓存：成功覆盖；失败保留旧表并刷新时间戳，沿用旧表一个缓存周期。
fn rebuild_virtual_blacklist(
    cache: &mut Option<(HashSet<u64>, Instant)>,
    now: Instant,
    rebuild: impl FnOnce() -> Option<HashSet<u64>>,
) {
    match rebuild() {
        Some(set) => *cache = Some((set, now)),
        None => {
            // 失败也刷新时间戳，避免每 tick 重试 GetAdaptersAddresses。
            if let Some((_, last_refresh)) = cache.as_mut() {
                *last_refresh = now;
            } else {
                *cache = Some((HashSet::new(), now));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== is_valid_interface =====

    #[test]
    fn test_is_valid_interface_ethernet() {
        let row = MIB_IF_ROW2 {
            Type: IF_TYPE_ETHERNET_CSMACD,
            PhysicalAddressLength: 6,
            ..Default::default()
        };
        assert!(is_valid_interface(&row));
    }

    #[test]
    fn test_is_valid_interface_wifi() {
        let row = MIB_IF_ROW2 {
            Type: IF_TYPE_IEEE80211,
            PhysicalAddressLength: 6,
            ..Default::default()
        };
        assert!(is_valid_interface(&row));
    }

    #[test]
    fn test_is_valid_interface_unknown_type_rejected() {
        // 非以太网/非 WiFi 类型（如软件环回 IF_TYPE_SOFTWARE_LOOPBACK=24）应被过滤。
        let row = MIB_IF_ROW2 {
            Type: 24,
            PhysicalAddressLength: 6,
            ..Default::default()
        };
        assert!(!is_valid_interface(&row));
    }

    #[test]
    fn test_is_valid_interface_zero_mac_rejected() {
        // PhysicalAddressLength == 0 表示无 MAC 地址，不可用于流量统计。
        let row = MIB_IF_ROW2 {
            Type: IF_TYPE_ETHERNET_CSMACD,
            PhysicalAddressLength: 0,
            ..Default::default()
        };
        assert!(!is_valid_interface(&row));
    }

    // ===== is_virtual_friendly_name =====

    #[test]
    fn test_virtual_name_hyperv() {
        assert!(is_virtual_friendly_name("Hyper-V Virtual Ethernet Adapter"));
    }

    #[test]
    fn test_virtual_name_vmware() {
        assert!(is_virtual_friendly_name("VMware Virtual Ethernet Adapter"));
    }

    #[test]
    fn test_virtual_name_vbox() {
        assert!(is_virtual_friendly_name(
            "VirtualBox Host-Only Ethernet Adapter"
        ));
    }

    #[test]
    fn test_virtual_name_wsl() {
        assert!(is_virtual_friendly_name("vEthernet (WSL)"));
    }

    #[test]
    fn test_virtual_name_vpn() {
        assert!(is_virtual_friendly_name("VPN Client Adapter"));
    }

    #[test]
    fn test_virtual_name_loopback() {
        assert!(is_virtual_friendly_name("Microsoft Loopback Adapter"));
    }

    #[test]
    fn test_virtual_name_case_insensitive() {
        // 大小写不敏感匹配。
        assert!(is_virtual_friendly_name("VBOX Network Adapter"));
        assert!(is_virtual_friendly_name("Virtual Ethernet Device"));
    }

    #[test]
    fn test_virtual_name_physical_not_matched() {
        // 真实物理网卡的常见名称不应被误判为虚拟。
        assert!(!is_virtual_friendly_name(
            "Intel(R) Ethernet Connection I219-LM"
        ));
        assert!(!is_virtual_friendly_name(
            "Realtek PCIe GbE Family Controller"
        ));
        assert!(!is_virtual_friendly_name("Killer Wi-Fi 6 AX1650"));
    }

    // ===== 黑名单缓存刷新语义 =====

    #[test]
    fn test_blacklist_needs_refresh_when_empty_or_stale() {
        let now = Instant::now();

        assert!(blacklist_needs_refresh(&None, now));
        assert!(blacklist_needs_refresh(
            &Some((
                HashSet::new(),
                now - std::time::Duration::from_secs(BLACKLIST_REFRESH_SECS)
            )),
            now
        ));
        assert!(!blacklist_needs_refresh(&Some((HashSet::new(), now)), now));
    }

    #[test]
    fn test_blacklist_refresh_success_replaces_cache() {
        let old_time = Instant::now();
        let mut cache = Some((HashSet::from([1]), old_time));
        let now = old_time + std::time::Duration::from_secs(BLACKLIST_REFRESH_SECS + 1);

        rebuild_virtual_blacklist(&mut cache, now, || Some(HashSet::from([2])));

        let (list, refreshed_at) = cache.as_ref().unwrap();
        assert_eq!(list, &HashSet::from([2]));
        assert!(*refreshed_at >= old_time);
        assert!(!blacklist_needs_refresh(&cache, now));
    }

    #[test]
    fn test_blacklist_refresh_failure_keeps_old_list_and_resets_timer() {
        let old_time = Instant::now();
        let mut cache = Some((HashSet::from([7]), old_time));
        let now = old_time + std::time::Duration::from_secs(BLACKLIST_REFRESH_SECS + 1);

        let mut rebuild_calls = 0;
        rebuild_virtual_blacklist(&mut cache, now, || {
            rebuild_calls += 1;
            None
        });

        // 失败路径必须保留旧表且只探测一次；刷新后的时间戳保证 30 秒内不会重试。
        assert_eq!(rebuild_calls, 1);
        let (list, refreshed_at) = cache.as_ref().unwrap();
        assert_eq!(list, &HashSet::from([7]));
        assert_eq!(*refreshed_at, now);
        assert!(!blacklist_needs_refresh(&cache, now));
    }

    #[test]
    fn test_blacklist_refresh_failure_without_old_list_stores_empty_list() {
        let now = Instant::now();
        let mut cache = None;

        rebuild_virtual_blacklist(&mut cache, now, || None);

        let (list, refreshed_at) = cache.as_ref().unwrap();
        assert!(list.is_empty());
        assert_eq!(*refreshed_at, now);
        assert!(!blacklist_needs_refresh(&cache, now));
    }
}
