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

thread_local! {
    static CURRENT_DATA: RefCell<HashMap<u64, (u64, u64)>> = RefCell::new(HashMap::with_capacity(16));
    static INTERFACE_HISTORY: RefCell<HashMap<u64, (u64, u64)>> = RefCell::new(HashMap::with_capacity(16));
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
            INTERFACE_HISTORY.with(|cell| {
                let mut history = cell.borrow_mut();
                std::mem::swap(&mut *history, &mut current_data);
            });
            NET_INITIALIZED.store(true, Ordering::Release);
            CURRENT_DATA.with(|cell| *cell.borrow_mut() = current_data);
            return;
        }

        let mut max_total: u64 = 0;
        let mut best_speed_down: u32 = 0;
        let mut best_speed_up: u32 = 0;

        INTERFACE_HISTORY.with(|cell| {
            let mut history = cell.borrow_mut();
            for (luid, (in_octets, out_octets)) in &current_data {
                if let Some(&(prev_in, prev_out)) = history.get(luid) {
                    let speed_down = in_octets.saturating_sub(prev_in).min(u32::MAX as u64) as u32;
                    let speed_up = out_octets.saturating_sub(prev_out).min(u32::MAX as u64) as u32;
                    let total = speed_down as u64 + speed_up as u64;

                    if total > max_total {
                        max_total = total;
                        best_speed_down = speed_down;
                        best_speed_up = speed_up;
                    }
                }
            }

            std::mem::swap(&mut *history, &mut current_data);
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
