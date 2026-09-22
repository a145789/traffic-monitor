use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Memory::{GetProcessHeaps, HEAP_FLAGS, HeapCompact};
use windows::Win32::System::Power::HPOWERNOTIFY;
use windows::Win32::System::Threading::{
    GetCurrentProcess, MEMORY_PRIORITY_INFORMATION, MEMORY_PRIORITY_LOW,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, ProcessMemoryPriority, ProcessPowerThrottling,
    SetProcessInformation, SetProcessWorkingSetSize,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, MessageBoxW,
};
use windows::core::PCWSTR;
use windows_registry::CURRENT_USER;

use crate::config::{
    APP_TITLE, DEBUG_LOG_DIR_NAME, DEBUG_LOG_DISABLE_AFTER_FAILURES, DEBUG_LOG_FILE_NAME,
    DEBUG_LOG_MAX_BYTES, REG_PATH_APP, REG_VALUE_DEBUG_LOG,
};

/// 业务字符串 → NUL 结尾 UTF-16。Win32 API 的标准入口。
///
/// 只用于进程内已是合法 `str` 的业务文案；路径与外部原始数据请用
/// [`os_to_wide`]（无损），勿经 `to_string_lossy()` 中转。
///
/// `config` 中已含尾 NUL 的常量请直接 `encode_utf16().collect()`，勿再套本函数
/// （会多一个多余的 NUL，虽通常无害但语义不清晰）。
pub fn to_wide(s: &str) -> Vec<u16> {
    let mut v = Vec::with_capacity(s.len() + 1);
    push_wide(&mut v, s);
    v
}

/// 向已有缓冲区追加 NUL 结尾 UTF-16，供渲染热路径复用缓冲、避免逐帧分配。
pub fn push_wide(buf: &mut Vec<u16>, s: &str) {
    buf.extend(s.encode_utf16());
    buf.push(0);
}

/// 定长宽字符缓冲截断拷贝：`src`（含尾 NUL 的 `to_wide` 产出）截断装入 `dst`
/// 并保证尾 NUL。托盘 `szTip` 与字体 `lfFaceName` 共用同一份实现。
///
/// 空 `dst` 直接返回；其余情况下必写 `dst[len-1] = 0`，调用方无需再补 NUL。
pub fn copy_wide_truncated(dst: &mut [u16], src: &[u16]) {
    if dst.is_empty() {
        return;
    }
    let copy_len = (src.len() + 1).min(dst.len()) - 1;
    dst[..copy_len].copy_from_slice(&src[..copy_len]);
    dst[copy_len] = 0;
}

/// `OsStr` → NUL 结尾 UTF-16。Windows 上 `OsStr` 可无损转宽字符，
/// 不经 `String` 中转：含非 Unicode 可解码字符的路径不再被替换成 U+FFFD。
/// 常规路径输出与 `to_wide(&s.to_string_lossy())` 逐字节一致。
pub fn os_to_wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut v: Vec<u16> = s.encode_wide().collect();
    v.push(0);
    v
}

/// DPI 缩放：全项目唯一的 `base * dpi / 96` 舍入实现。
///
/// 窗口矩形（`window::calc_widget_rect`）与位图/字体尺寸
/// （`renderer::Renderer::update_dpi`）必须共享同一舍入策略，否则任一处改动
/// 舍入即出现「窗口与位图差一像素」的错位。调用方直接传 `GetDpiForWindow`
/// 返回的 `u32`，无需自行计算 `scale`。
///
/// `Layout::new` 刻意不调用本函数：它从已舍入的实际宽度反推比例
/// （`width / DISPLAY_WIDTH`），若经整数 DPI 中转会因二次舍入在部分 DPI 下
/// 差一像素（96–384 范围实测 77 处），故保持宽度推导以逐像素不变。
pub fn dpi_scaled(base: i32, dpi: u32) -> i32 {
    ((base as f64) * (dpi as f64) / 96.0).round() as i32
}

/// `HWND` 的原子存储：「0 为空位」约定与内存序配对收口一处。
///
/// - `store`（Release）：发布新句柄。
/// - `load`（Acquire）：只读查询，0 映射为 `None`；不做 `IsWindow` 校验，
///   有效性由调用方按需查询。
/// - `take`（AcqRel `swap(0)`）：取走语义，重建/注销路径专用；
///   查询路径误用会清零丢句柄，类型层面与 `load` 区分。
/// - `clear`（Release）：无条件归零（缓存失效）。
pub struct AtomicHwnd(std::sync::atomic::AtomicIsize);

impl AtomicHwnd {
    pub const fn new() -> Self {
        Self(std::sync::atomic::AtomicIsize::new(0))
    }

    pub fn store(&self, hwnd: HWND) {
        self.0
            .store(hwnd.0 as isize, std::sync::atomic::Ordering::Release);
    }

    pub fn load(&self) -> Option<HWND> {
        let raw = self.0.load(std::sync::atomic::Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(HWND(raw as *mut std::ffi::c_void))
        }
    }

    pub fn take(&self) -> Option<HWND> {
        let raw = self.0.swap(0, std::sync::atomic::Ordering::AcqRel);
        if raw == 0 {
            None
        } else {
            Some(HWND(raw as *mut std::ffi::c_void))
        }
    }

    pub fn clear(&self) {
        self.0.store(0, std::sync::atomic::Ordering::Release);
    }

    #[cfg(test)]
    pub fn store_raw(&self, raw: isize) {
        self.0.store(raw, std::sync::atomic::Ordering::Release);
    }
}

/// `HPOWERNOTIFY` 的原子存储：与 [`AtomicHwnd`] 同构单列。
///
/// 内值为 `isize`，无需指针转换；内存序契约与 `AtomicHwnd` 一致
/// （`store`/`clear` 用 Release，`load` 用 Acquire，`take` 用 AcqRel）。
pub struct AtomicPowerNotify(std::sync::atomic::AtomicIsize);

impl AtomicPowerNotify {
    pub const fn new() -> Self {
        Self(std::sync::atomic::AtomicIsize::new(0))
    }

    pub fn store(&self, handle: HPOWERNOTIFY) {
        self.0.store(handle.0, std::sync::atomic::Ordering::Release);
    }

    pub fn load(&self) -> Option<HPOWERNOTIFY> {
        let raw = self.0.load(std::sync::atomic::Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(HPOWERNOTIFY(raw))
        }
    }

    pub fn take(&self) -> Option<HPOWERNOTIFY> {
        let raw = self.0.swap(0, std::sync::atomic::Ordering::AcqRel);
        if raw == 0 {
            None
        } else {
            Some(HPOWERNOTIFY(raw))
        }
    }
}

/// 当前进程模块句柄（HINSTANCE），用于注册窗口类、加载内置资源。
pub fn module_instance() -> Result<windows::Win32::Foundation::HINSTANCE, String> {
    // SAFETY: GetModuleHandleW(None) 查询当前进程模块，无指针参数。
    unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
        .map(Into::into)
        .map_err(|e| format!("获取模块句柄失败: {e:?}"))
}

/// 统一的 MessageBoxW 入口：所有弹窗都经由本函数创建，避免各处重复拼装
/// 标题/正文宽字符串。`style` 直接透传 Win32 组合标志，返回用户选择结果。
pub fn message_box(msg: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let title = to_wide(APP_TITLE);
    let msg_wide = to_wide(msg);
    // SAFETY: title/msg_wide 含尾 NUL，在 MessageBoxW 返回前存活。
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(msg_wide.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
        )
    }
}

pub fn show_error(msg: &str) {
    message_box(msg, MB_OK | MB_ICONERROR);
}

pub fn show_info(msg: &str) {
    message_box(msg, MB_OK | MB_ICONINFORMATION);
}

/// 仅把当前进程的内存优先级调低：系统内存紧张时，OS 会优先回收本进程的
/// 代码/堆/栈页（退回 Standby），而不是与其他进程争抢物理内存。
///
/// 与 EcoQoS（ProcessPowerThrottling）不同，此设置不影响 CPU 调度与核心
/// 选择，常驻主进程可安全使用；1s 采样定时器触发时的软缺页代价为微秒级。
/// 内存优先级会被本进程创建的子进程继承。
///
/// 这是最佳努力设置：旧系统或策略限制导致设置失败时不影响功能。
pub fn set_low_memory_priority() {
    // SAFETY: MEMORY_PRIORITY_INFORMATION 为 Win32 API 要求的固定布局，
    // 指针只在同步调用期间有效；当前进程伪句柄无需关闭。
    unsafe {
        let memory = MEMORY_PRIORITY_INFORMATION {
            MemoryPriority: MEMORY_PRIORITY_LOW,
        };
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessMemoryPriority,
            &memory as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        );
    }
}

/// 将进程标记为低优先级后台工作（显式 EcoQoS + 低内存优先级）。
///
/// 仅用于 `--check-update` 短生命周期子进程：主进程是任务栏常显窗口，
/// 显式 EcoQoS 会把它钉进效率核/低频调度类并拖慢 1s 采样与 GDI 绘制。
/// 子进程的内存优先级本就继承自父进程，EcoQoS 则必须显式设置。
///
/// 这是最佳努力设置：旧系统或策略限制导致设置失败时不影响功能。
pub fn configure_background_process() {
    // SAFETY: PROCESS_POWER_THROTTLING_STATE 为 Win32 API 要求的固定布局，
    // 指针只在同步调用期间有效；当前进程伪句柄无需关闭。
    unsafe {
        let power = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &power as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }

    set_low_memory_priority();
}

/// 静默失败点的诊断埋点。
///
/// 仅用于今天完全静默的 `let _ =` 失败路径（托盘、定时器、嵌入、投递）：
/// debug 构建经 `OutputDebugStringW` 输出（DebugView / Visual Studio 输出窗口
/// 可见），release 构建宏体展开为空、零成本。不替代错误处理，不引入
/// tracing/log 依赖（GUI 子系统进程无控制台，`eprintln!` 不可用）。
///
/// 埋点纪律：只加在「失败后无任何感知通道」的调用点；有弹框或返回值
/// 的路径不需要。
#[cfg(debug_assertions)]
macro_rules! diag {
    ($($arg:tt)*) => {{
        let msg = ::std::format!("traffic-monitor: {}", ::std::format_args!($($arg)*));
        let wide: Vec<u16> = msg.encode_utf16().chain(::std::iter::once(0)).collect();
        // SAFETY: wide 以 NUL 结尾，且仅在本次同步调用期间存活。
        // allow(unused_unsafe)：部分调用点本身位于 unsafe 块内，嵌套 unsafe 会触发告警。
        #[allow(unused_unsafe)]
        unsafe {
            ::windows::Win32::System::Diagnostics::Debug::OutputDebugStringW(
                ::windows::core::PCWSTR(wide.as_ptr()),
            );
        }
    }};
}

/// release 版本：整体空展开，但保留 `format_args!` 的形式校验（含参数是否
/// 在作用域内），避免 debug/release 之间的格式串漂移与 unused 变量告警。
#[cfg(not(debug_assertions))]
macro_rules! diag {
    ($($arg:tt)*) => {{ if false { let _ = ::std::format_args!($($arg)*); } }};
}

pub(crate) use diag;

/// release 现场诊断日志开关的进程内缓存。
///
/// 唯一真值源是注册表 `REG_PATH_APP\EnableDebugLog`（DWORD）；本原子量是其
/// 启动时快照：`refresh_debug_log_flag` 在主进程与更新子进程入口各加载一次。
/// 写失败达阈值时复位为 false，使后续调用在 `log_event!` 门即返回、不再产生
/// 系统调用；注册表值本身不动（瞬时故障不应抹掉用户设置），重启后按注册表重载。
/// 读/写均为 Relaxed：单进程内开关轮询，无跨线程握手语义。
static DEBUG_LOG_ENABLED: AtomicBool = AtomicBool::new(false);
/// 连续写失败计数；成功一次即清零。Relaxed（与开关同理）。
static DEBUG_LOG_CONSEC_FAILURES: AtomicU32 = AtomicU32::new(0);

/// `log_event!` 的唯一门：开关关闭时仅一次 Relaxed 原子读即返回。
pub fn debug_log_enabled() -> bool {
    DEBUG_LOG_ENABLED.load(Ordering::Relaxed)
}

/// 从注册表重载调试日志开关并清零失败计数。启动入口调用一次；
/// 运行中改注册表需重启生效（热路径不读注册表，开关关闭时零系统调用）。
pub fn refresh_debug_log_flag() {
    let on = reg_read_dword(REG_PATH_APP, REG_VALUE_DEBUG_LOG)
        .map(|v| v != 0)
        .unwrap_or(false);
    DEBUG_LOG_ENABLED.store(on, Ordering::Relaxed);
    DEBUG_LOG_CONSEC_FAILURES.store(0, Ordering::Relaxed);
}

/// 调试日志落盘（`%LOCALAPPDATA%\Traffic Monitor\debug.log`，环形截断）。
///
/// 调用前必须已由 `log_event!` 门控；本函数不再重复读开关（热路径只付一次
/// 原子读）。任何失败静默丢弃并计入连续失败，达阈值自动关开关：
/// 不 panic、不弹框、不阻塞 UI 线程（单次追加写 + 偶发截断读，均有界）。
pub fn write_debug_log(line: &str) {
    if append_debug_log(&debug_log_path(), line).is_ok() {
        DEBUG_LOG_CONSEC_FAILURES.store(0, Ordering::Relaxed);
    } else {
        let n = DEBUG_LOG_CONSEC_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
        if failures_should_disable(n) {
            DEBUG_LOG_ENABLED.store(false, Ordering::Relaxed);
        }
    }
}

/// 连续失败达阈值即停写（纯函数，阈值见 `DEBUG_LOG_DISABLE_AFTER_FAILURES`）。
fn failures_should_disable(consec_failures: u32) -> bool {
    consec_failures >= DEBUG_LOG_DISABLE_AFTER_FAILURES
}

/// 日志完整路径。`LOCALAPPDATA` 缺失时回退 temp（与安装包缓存同策略）；
/// 含非 Unicode 字符时 `var_os` 无损直转，不经 `String` 中转。
fn debug_log_path() -> std::path::PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    debug_log_path_for_base(&base)
}

/// 纯拼接：`base\Traffic Monitor\debug.log`（单测钉死目录文件名）。
fn debug_log_path_for_base(base: &std::path::Path) -> std::path::PathBuf {
    base.join(DEBUG_LOG_DIR_NAME).join(DEBUG_LOG_FILE_NAME)
}

/// 单次追加写；超限时先保留尾部一半（环形截断）。失败由调用方计数。
fn append_debug_log(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::metadata(path)
        .map(|m| m.len() > DEBUG_LOG_MAX_BYTES)
        .unwrap_or(false)
    {
        truncate_debug_log(path);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "[{now}] {line}")?;
    Ok(())
}

/// 环形截断：只保留尾部一半；截断本身失败由本次追加写一并承担
/// （追加大概率同样失败，调用方统一计数，不单独处理）。
fn truncate_debug_log(path: &std::path::Path) {
    let keep = (DEBUG_LOG_MAX_BYTES / 2) as usize;
    if let Ok(content) = std::fs::read(path) {
        let start = content.len().saturating_sub(keep);
        let _ = std::fs::write(path, &content[start..]);
    }
}

/// release 现场诊断埋点（写 `%LOCALAPPDATA%\Traffic Monitor\debug.log`）。
///
/// 与 `diag!` 分工：`diag!` 只服务开发期（release 空展开），本宏服务 release
/// 现场（嵌入失败、更新卡住等只能靠用户转述的问题）。开关关闭时仅一次
/// Relaxed 原子读即返回：`format!` 不求值，无系统调用、无分配。
/// 开关开启且写失败时静默丢弃并计数，达阈值自动关开关（见 `write_debug_log`）。
macro_rules! log_event {
    ($($arg:tt)*) => {{
        if $crate::util::debug_log_enabled() {
            $crate::util::write_debug_log(&::std::format!($($arg)*));
        }
    }};
}

pub(crate) use log_event;

pub fn reg_read_dword(subkey: &str, value_name: &str) -> Option<u32> {
    CURRENT_USER
        .open(subkey)
        .and_then(|key| key.get_u32(value_name))
        .ok()
}

pub fn reg_write_dword(subkey: &str, value_name: &str, value: u32) -> bool {
    CURRENT_USER
        .create(subkey)
        .and_then(|key| key.set_u32(value_name, value))
        .is_ok()
}

pub fn reg_read_string(subkey: &str, value_name: &str) -> Option<String> {
    CURRENT_USER
        .open(subkey)
        .and_then(|key| key.get_string(value_name))
        .ok()
}

pub fn reg_write_string(subkey: &str, value_name: &str, value: &str) -> bool {
    CURRENT_USER
        .create(subkey)
        .and_then(|key| key.set_string(value_name, value))
        .is_ok()
}

/// `OsStr` → REG_SZ 无损写入。自启项等路径值请走本函数：
/// `reg_write_string` 的 `&str` 接口会把非 Unicode 路径堵死在
/// `to_string_lossy()` 的替换字符上。
///
/// 与 `set_string` 同布局：`os_to_wide` 恒含尾 NUL，逐码元 reinterpret 为
/// LE 字节流后按 `Type::String` 原样写入，不经 `String` 中转。
pub fn reg_write_string_os(subkey: &str, value_name: &str, value: &std::ffi::OsStr) -> bool {
    let wide = os_to_wide(value);
    // SAFETY/内存布局：u16 LE 码元与 REG_SZ 字节流逐字节对应；`wide` 恒含尾
    // NUL 从而非空，切片与 `wide` 同生死、不逃逸本函数。
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    CURRENT_USER
        .create(subkey)
        .and_then(|key| key.set_bytes(value_name, windows_registry::Type::String, bytes))
        .is_ok()
}

pub fn reg_remove_value(subkey: &str, value_name: &str) -> bool {
    CURRENT_USER
        .open(subkey)
        .and_then(|key| key.remove_value(value_name))
        .is_ok()
}

/// 修剪当前进程工作集（物理页面退到 Standby List）。
///
/// 与 `compact_and_trim` 的区别：本函数**不**压缩堆，适合挂起、初始化后等
/// 一次性场景调用，不会引发工作集反弹。
pub fn trim_working_set() {
    // SAFETY: GetCurrentProcess() 返回当前进程伪句柄，不需关闭；
    // (usize::MAX, usize::MAX) 是系统约定的工作集修剪命令。
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

/// 压缩进程**所有堆**并修剪工作集。
///
/// 与 `trim_working_set` 的区别：先遍历进程内所有堆（含 UCRT malloc 堆）
/// 调用 `HeapCompact` 将空闲页 decommit 归还 OS，再修剪工作集物理页面。
///
/// Rust 默认分配器走 UCRT 的 `malloc` 堆，与 `GetProcessHeap()` 返回的默认
/// 进程堆是**不同的堆句柄**。若只压缩默认堆，`Command::output()` 等 Rust
/// 代码路径在 UCRT 堆上释放的内存不会被 decommit，造成内存水位居高不下。
///
/// 仅在更新检查等「大量临时堆分配已全部释放」的场景中调用；**不可**用于常规
/// 周期性 trim，否则会因过度 decommit 导致后续正常分配反复 recommit 页面，
/// 造成工作集反弹到更高水位。
pub fn compact_and_trim() {
    // SAFETY:
    // 1. GetProcessHeaps(None) 返回进程堆数量，无副作用。
    // 2. 第二次调用传入足够大的缓冲区，OS 填充所有堆句柄。
    // 3. HeapCompact(flags=0) 使用默认序列化，对多线程安全。
    //    它合并空闲块并将整页空闲内存 decommit 归还 OS。
    unsafe {
        let count = GetProcessHeaps(&mut []);
        if count > 0 {
            let mut heaps = vec![windows::Win32::Foundation::HANDLE::default(); count as usize];
            let actual = GetProcessHeaps(&mut heaps);
            for heap in heaps.iter().take(actual as usize) {
                let _ = HeapCompact(*heap, HEAP_FLAGS(0));
            }
        }
    }
    trim_working_set();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_wide_nul_terminated() {
        let w = to_wide("hello");
        assert_eq!(w.last(), Some(&0));
        let without_nul = &w[..w.len() - 1];
        let expected: Vec<u16> = "hello".encode_utf16().collect();
        assert_eq!(without_nul, expected);
    }

    #[test]
    fn test_to_wide_empty() {
        let w = to_wide("");
        assert_eq!(w, vec![0]);
    }

    #[test]
    fn test_to_wide_unicode() {
        let w = to_wide("\u{2191}\u{2193}");
        let without_nul = &w[..w.len() - 1];
        assert_eq!(without_nul, &[0x2191u16, 0x2193u16]);
    }

    #[test]
    fn test_to_wide_roundtrip() {
        let original = "Traffic Monitor 监控";
        let w = to_wide(original);
        let without_nul = &w[..w.len() - 1];
        let rt = String::from_utf16(without_nul).unwrap();
        assert_eq!(rt, original);
    }

    #[test]
    fn test_push_wide_appends() {
        let mut buf = to_wide("A");
        push_wide(&mut buf, "B");
        // "A\0" + "B\0"
        assert_eq!(buf, vec![b'A' as u16, 0, b'B' as u16, 0]);
    }

    #[test]
    fn test_copy_wide_truncated_fits_and_truncates() {
        // 恰好装下：整体搬入（含尾 NUL）并补一位 NUL。
        let mut dst = [0xFFFFu16; 4];
        copy_wide_truncated(&mut dst, &to_wide("ab"));
        assert_eq!(dst, [b'a' as u16, b'b' as u16, 0, 0]);

        // 装不下：截断并保证尾 NUL，调用方无需再补。
        let mut dst = [0xFFFFu16; 3];
        copy_wide_truncated(&mut dst, &to_wide("abcd"));
        assert_eq!(dst, [b'a' as u16, b'b' as u16, 0]);

        // 空目标：静默返回，不 panic。
        let mut dst: [u16; 0] = [];
        copy_wide_truncated(&mut dst, &to_wide("ab"));
    }

    #[test]
    fn test_os_to_wide_matches_to_wide_on_ascii() {
        // 不变量：常规路径输出逐字节不变。
        let w = os_to_wide(std::ffi::OsStr::new("C:\\Temp\\a.exe"));
        assert_eq!(w, to_wide("C:\\Temp\\a.exe"));
    }

    #[test]
    fn test_os_to_wide_preserves_lone_surrogate() {
        // 含非 Unicode 可解码字符（孤立代理项）：无损直转保留原码元，
        // 而 to_string_lossy 会替换成 U+FFFD。
        use std::os::windows::ffi::OsStringExt;
        let raw = std::ffi::OsString::from_wide(&[0x41u16, 0xD800u16, 0x42u16]);
        assert_eq!(
            os_to_wide(raw.as_os_str()),
            vec![0x41u16, 0xD800u16, 0x42u16, 0]
        );
        assert_ne!(to_wide(&raw.to_string_lossy()), os_to_wide(raw.as_os_str()));
    }

    #[test]
    fn test_dpi_scaled_matches_forward_formula() {
        // 96 DPI 下恒等；150%（144）与 200%（192）逐点对账收敛前的调用点写法
        //（base * (dpi/96)，与合并后的 (base*dpi)/96 乘除顺序不同）。
        for (base, dpi, expected) in [
            (170, 96, 170),
            (32, 96, 32),
            (-3, 96, -3),
            (13, 96, 13),
            (170, 144, 255),
            (32, 144, 48),
            (-3, 144, -5),
            (13, 144, 20),
            (170, 192, 340),
            (32, 192, 64),
            (76, 120, 95),
        ] {
            assert_eq!(dpi_scaled(base, dpi), expected, "base={base} dpi={dpi}");
            let legacy = (base as f64 * (dpi as f64 / 96.0)).round() as i32;
            assert_eq!(dpi_scaled(base, dpi), legacy, "base={base} dpi={dpi}");
        }
    }

    #[test]
    fn test_dpi_scaled_matches_legacy_across_dpi_range() {
        // 新旧公式只是浮点乘除顺序不同，等价是实测结论而非恒等式：
        // 四个实际调用点常量在 96–384 全范围逐点相等，改舍入即红。
        use crate::config::{DISPLAY_HEIGHT, DISPLAY_WIDTH, FONT_BASE_SIZE, GAP};
        for base in [DISPLAY_WIDTH, DISPLAY_HEIGHT, GAP, FONT_BASE_SIZE] {
            for dpi in 96..=384u32 {
                let legacy = (base as f64 * (dpi as f64 / 96.0)).round() as i32;
                assert_eq!(dpi_scaled(base, dpi), legacy, "base={base} dpi={dpi}");
            }
        }
    }

    #[test]
    fn test_debug_log_path_layout() {
        // 日志目录文件名钉死：改名即红，调用方不再各自拼接路径。
        let p = debug_log_path_for_base(std::path::Path::new("C:\\Base"));
        assert_eq!(
            p,
            std::path::Path::new("C:\\Base\\Traffic Monitor\\debug.log")
        );
    }

    #[test]
    fn test_debug_log_disable_threshold() {
        use crate::config::DEBUG_LOG_DISABLE_AFTER_FAILURES;
        // 未达阈值不断写（容忍瞬时故障），达阈值即停写、后续调用在宏门返回。
        assert!(!failures_should_disable(0));
        assert!(!failures_should_disable(
            DEBUG_LOG_DISABLE_AFTER_FAILURES - 1
        ));
        assert!(failures_should_disable(DEBUG_LOG_DISABLE_AFTER_FAILURES));
        assert!(failures_should_disable(
            DEBUG_LOG_DISABLE_AFTER_FAILURES + 1
        ));
    }

    #[test]
    fn test_log_event_disabled_writes_nothing() {
        // 开关默认关闭（本进程内无测试开启它）：log_event 只做一次原子读即返回，
        // 不得产生任何文件操作。本用例零副作用：只读不断言存在性，前后内容一致即过。
        assert!(!debug_log_enabled());
        let path = debug_log_path();
        let before = std::fs::read(&path).ok();
        log_event!("disabled-noop-marker");
        let after = std::fs::read(&path).ok();
        assert_eq!(before, after);
    }

    #[test]
    fn test_append_debug_log_ring_truncates() {
        use crate::config::DEBUG_LOG_MAX_BYTES;
        // 真实文件行为：小写追加留痕；超限后只保留尾部一半 + 本次行。
        let dir = std::env::temp_dir().join(format!(
            "traffic-monitor-debuglog-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("debug.log");

        append_debug_log(&path, "hello").unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.contains("hello"));

        let big = vec![b'x'; DEBUG_LOG_MAX_BYTES as usize + 100];
        std::fs::write(&path, &big).unwrap();
        append_debug_log(&path, "tail").unwrap();
        let kept = std::fs::read(&path).unwrap();
        assert!(kept.len() < big.len(), "超限文件必须被截断");
        assert!(kept.ends_with(b"tail\n"), "截断后本次行必须保留");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
