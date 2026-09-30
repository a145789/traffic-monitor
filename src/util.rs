use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Memory::{GetProcessHeaps, HEAP_FLAGS, HeapCompact};
use windows::Win32::System::Power::HPOWERNOTIFY;
use windows::Win32::System::Threading::{
    GetCurrentProcess, MEMORY_PRIORITY_INFORMATION, MEMORY_PRIORITY_LOW, OpenMutexW, OpenProcess,
    OpenProcessToken, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    PROCESS_QUERY_LIMITED_INFORMATION, ProcessMemoryPriority, ProcessPowerThrottling,
    SYNCHRONIZATION_SYNCHRONIZE, SetProcessInformation, SetProcessWorkingSetSize,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowThreadProcessId, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MESSAGEBOX_RESULT,
    MESSAGEBOX_STYLE, MessageBoxW,
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

/// 把 `windows::core::Error` 携带的 `HRESULT` 折回裸 Win32 错误码。
///
/// `HRESULT_FROM_WIN32(code)` 的形状是 `0x8007_0000 | code`，故只有高位是
/// `FACILITY_WIN32` 前缀时才有裸码可取；其余（如 `E_HANDLE`、`E_INVALIDARG`）返回
/// `None`。必须经它比较，不能直接拿 `HRESULT` 去比 `ERROR_*` 常量——两者不是同一个
/// 数值空间。
///
/// 唯一用途：让 `Err` 自带的错误码成为唯一来源，调用点不再裸读 `GetLastError()`。
/// 裸读会把「crate 内部不会插入其它 Win32 调用」这个没有任何人承诺过的前提变成
/// 正确性依赖；`Err` 已经带了码，再读一次是同一事实的重复表示。
pub fn win32_code_from_hresult(code: u32) -> Option<u32> {
    const FACILITY_WIN32_HRESULT_PREFIX: u32 = 0x8007_0000;
    (code & 0xFFFF_0000 == FACILITY_WIN32_HRESULT_PREFIX).then_some(code & 0xFFFF)
}

/// 取错误码用于日志：能折回裸 Win32 码就用裸码，否则回落到 `HRESULT` 原文——
/// 取不到码时也不能让日志空掉（`win32_code_from_hresult` 为 `None` 的那一类
/// `Err` 正是最需要原样留痕的）。
pub fn win32_error_code(err: &windows::core::Error) -> u32 {
    let hresult = err.code().0 as u32;
    win32_code_from_hresult(hresult).unwrap_or(hresult)
}

/// 读当前进程令牌的提权标志（`TOKEN_ELEVATION`）：`Some(true)` 表示已提权（high IL）。
///
/// 取不到时返回 `None`，与「确定未提权」严格分开：调用方（`main::de_elevate_self`）
/// 对「未知」必须走保守侧（维持现状），不能当成「未提权」而跳过自检。
pub fn current_process_is_elevated() -> Option<bool> {
    // SAFETY: GetCurrentProcess 返回当前进程伪句柄，不需关闭；token 句柄成功取得后
    // 在本函数内关闭一次。
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return None;
        }
        let elevated = token_is_elevated(token);
        let _ = CloseHandle(token);
        elevated
    }
}

/// 读任意窗口所属进程的提权标志；窗口无效、进程已退出或无权打开时返回 `None`。
///
/// 只请求 `PROCESS_QUERY_LIMITED_INFORMATION` + `TOKEN_QUERY`：提权进程查询低完整性
/// 等级进程（典型为 medium IL 的 explorer）只需这两项权限，不依赖调试特权。
pub fn window_process_is_elevated(hwnd: HWND) -> Option<bool> {
    let mut pid = 0u32;
    // SAFETY: 只写本地 u32；hwnd 为调用方持有的窗口句柄，本 API 仅做查询，
    // 对任意句柄值安全返回（取不到所属进程时 pid 保持 0）。
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    if pid == 0 {
        return None;
    }
    // SAFETY: OpenProcess 只请求查询权限、不继承句柄；进程句柄与令牌句柄各关闭一次，
    // 且都只在本函数内使用。
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut token = HANDLE::default();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token).is_ok();
        let elevated = if opened {
            token_is_elevated(token)
        } else {
            None
        };
        if opened {
            let _ = CloseHandle(token);
        }
        let _ = CloseHandle(process);
        elevated
    }
}

/// `TOKEN_ELEVATION` 的读取与判定：上面两个入口的唯一实现。
fn token_is_elevated(token: HANDLE) -> Option<bool> {
    let mut info = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: info 为本地 TOKEN_ELEVATION，长度按 `size_of` 原样给出，GetTokenInformation
    // 同步写入且不超过该长度；returned 为本地 u32。token 的有效性由调用方保证
    // （它来自成功的 OpenProcessToken，且在本函数返回前不被关闭）。
    unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(std::ptr::from_mut(&mut info).cast::<core::ffi::c_void>()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .ok()?;
    }
    Some(info.TokenIsElevated != 0)
}

/// 单例互斥量当前是否存在：组件实例是否在跑的唯一探针。
///
/// 只报「打开成功」与「打开失败」两种结果，任何错误（含 ACCESS_DENIED）都归「不存在」。
/// 两处调用方都只需要这个方向的近似——「替代实例是否已接管」与「是否重复启动」——判错
/// 的最坏后果都只是多一次注定按重复实例静默退出的进程创建；反过来把错误当成「存在」，
/// 会让该补拉起的不补（组件真消失）、该让位的不让位（两个实例并存）。
pub fn main_instance_exists() -> bool {
    // MUTEX_NAME 常量已含尾 NUL。
    let name: Vec<u16> = crate::config::MUTEX_NAME.encode_utf16().collect();
    // SAFETY: name 以 NUL 结尾；句柄仅用于存在性探测，成功取得时立即关闭。
    // 最小权限：SYNCHRONIZE 只够打开既有互斥量做存在性探测。
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) } {
        Ok(handle) => {
            // SAFETY: handle 由紧邻的 OpenMutexW 成功返回，仅关闭一次。
            unsafe {
                let _ = CloseHandle(handle);
            }
            true
        }
        Err(_) => false,
    }
}

/// 轮询等待组件实例接管单例（同名单例互斥量出现），超时返回 false。
///
/// 两处共用，超时参数由调用方给——两处判错的代价不对称，故意不统一：
/// - 组件启动期的自去提权（`main::de_elevate_self`）：等的是 explorer 中转的替代实例。
///   `ShellExecuteW` 只说明 shell 接受了请求，不代表目标起来了；等不到就继续以提权身份
///   运行（不影响用户），所以可以等满 5 秒。
/// - 更新子进程在安装器收场后（`update::complete_update_interaction`）：判据是
///   「组件是不是已经在跑」，而不是安装器的退出码。等不到的代价只是多拉起一个注定
///   按重复实例静默退出的进程，因此这里故意取更短的上限，不让用户干等。
pub fn wait_main_instance_appear(timeout_ms: u64, poll_interval_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if main_instance_exists() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(poll_interval_ms));
    }
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
/// 内值为 `isize`，无需指针转换。内存序契约与 [`AtomicHwnd`] 一致
/// （`store`/`clear` 用 Release，`load` 用 Acquire，`take` 用 AcqRel）。
pub struct AtomicPowerNotify(std::sync::atomic::AtomicIsize);

impl AtomicPowerNotify {
    pub const fn new() -> Self {
        Self(std::sync::atomic::AtomicIsize::new(0))
    }

    pub fn store(&self, handle: HPOWERNOTIFY) {
        self.0.store(handle.0, std::sync::atomic::Ordering::Release);
    }

    /// 只读查询「该项订阅是否仍然在册」。唯一消费者是恢复调度器的补注册路径：
    /// 句柄为空 ⇔ 那条订阅不在，而订阅不在就再也收不到对应通知，必须在周期 tick 上
    /// 补回来，不能等下一次同源事件（它永远不会来）。
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

pub fn module_instance() -> Result<windows::Win32::Foundation::HINSTANCE, String> {
    // SAFETY: GetModuleHandleW(None) 查询当前进程模块，无指针参数。
    unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
        .map(Into::into)
        .map_err(|e| format!("获取模块句柄失败: {e:?}"))
}

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

pub fn debug_log_enabled() -> bool {
    DEBUG_LOG_ENABLED.load(Ordering::Relaxed)
}

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

fn debug_log_path_for_base(base: &std::path::Path) -> std::path::PathBuf {
    base.join(DEBUG_LOG_DIR_NAME).join(DEBUG_LOG_FILE_NAME)
}

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
    // 先把整行格式化进 String，再单次 write_all 落盘：`writeln!` 会按格式串片段
    // 拆成多次 WriteFile，与其它写者（UI 线程 / 更新工作线程 / 更新子进程，
    // 同写 %LOCALAPPDATA% 下同一文件）并发时可能交错出半行。
    // 单次写在 append 语义下不会被另一方的单次写切碎（跨进程仍可能整行插队）。
    let record = format!("[{now}] {line}\n");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(record.as_bytes())?;
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
        assert_eq!(buf, vec![b'A' as u16, 0, b'B' as u16, 0]);
    }

    #[test]
    fn test_copy_wide_truncated_fits_and_truncates() {
        let mut dst = [0xFFFFu16; 4];
        copy_wide_truncated(&mut dst, &to_wide("ab"));
        assert_eq!(dst, [b'a' as u16, b'b' as u16, 0, 0]);

        let mut dst = [0xFFFFu16; 3];
        copy_wide_truncated(&mut dst, &to_wide("abcd"));
        assert_eq!(dst, [b'a' as u16, b'b' as u16, 0]);

        let mut dst: [u16; 0] = [];
        copy_wide_truncated(&mut dst, &to_wide("ab"));
    }

    #[test]
    fn test_os_to_wide_matches_to_wide_on_ascii() {
        let w = os_to_wide(std::ffi::OsStr::new("C:\\Temp\\a.exe"));
        assert_eq!(w, to_wide("C:\\Temp\\a.exe"));
    }

    #[test]
    fn test_os_to_wide_preserves_lone_surrogate() {
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
        let p = debug_log_path_for_base(std::path::Path::new("C:\\Base"));
        assert_eq!(
            p,
            std::path::Path::new("C:\\Base\\Traffic Monitor\\debug.log")
        );
    }

    #[test]
    fn test_debug_log_disable_threshold() {
        use crate::config::DEBUG_LOG_DISABLE_AFTER_FAILURES;
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

    #[test]
    fn test_win32_code_from_hresult_folds_only_facility_win32() {
        // ERROR_CANCELLED(1223) / ERROR_FILE_NOT_FOUND(2) 的 HRESULT 必须折回裸码：
        // 三个调用点靠裸码比较分支，折不出来就会落进保守分支。
        assert_eq!(win32_code_from_hresult(0x8007_04C7), Some(1223));
        assert_eq!(win32_code_from_hresult(0x8007_0002), Some(2));
        // 非 FACILITY_WIN32 的 HRESULT 无裸码可取，必须返回 None 而不是 0
        // （当作 0 会被误判成某个具体分支）。
        assert_eq!(win32_code_from_hresult(0x8000_4005), None); // E_FAIL
        assert_eq!(win32_code_from_hresult(0x8007_0000), Some(0));
    }

    #[test]
    fn test_current_process_elevation_is_queryable() {
        // 只钉「本进程的令牌可被查询」这一事实，不钉具体取值：用例既可能在普通用户
        // 也可能在提权 CI/终端里跑，把 Some(false) 写死会变成环境依赖的假红。
        // 但必须是 Some：None 意味着 OpenProcessToken/GetTokenInformation 失败，
        // 而 main::de_elevate_self 对 None 走保守分支，等于自去提权整条路径失效。
        assert!(
            current_process_is_elevated().is_some(),
            "必须能读到本进程令牌的提权状态"
        );
    }

    #[test]
    fn test_invalid_window_has_no_process_elevation() {
        // 取不到所属进程（空句柄）必须返回 None 而不是 Some(false)：
        // Some(false) 会让 main::de_elevate_self 把「查不到」当成「shell 未提权」，
        // 从而把组件托付给一个根本不该被信任的中转对象。
        assert_eq!(window_process_is_elevated(HWND(std::ptr::null_mut())), None);
    }
}
