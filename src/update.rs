use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_CANCELLED, GetLastError, HWND, LPARAM, WPARAM,
};
use windows::Win32::Networking::WinHttp::*;
use windows::Win32::Security::Cryptography::*;
use windows::Win32::UI::Shell::{SHELLEXECUTEINFOW, ShellExecuteExW, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONINFORMATION, MB_YESNO, MessageBoxW, PostMessageW, PostQuitMessage, SW_SHOWNORMAL,
    WM_USER,
};
use windows::core::{PCWSTR, w};

use crate::collector::compact_and_trim;
use crate::state::{ENABLE_AUTO_UPDATE, UPDATE_IN_PROGRESS};
use crate::tray::remove_tray_icon;
use crate::util::{reg_read_dword, reg_write_dword, show_error, show_info, to_wide};

pub const WM_USER_UPDATE_READY: u32 = WM_USER + 5;

pub const UPDATE_STATUS_NO_UPDATE: usize = 0;
pub const UPDATE_STATUS_PORTABLE_FOUND: usize = 1;
pub const UPDATE_STATUS_INSTALLED_READY: usize = 2;
pub const UPDATE_STATUS_ERROR: usize = 3;

const VERSION_HOST: &str = "github.com";
const VERSION_PATH: &str = "/a145789/traffic-monitor/releases/latest/download/version.txt";
const DOWNLOAD_HOST: &str = "github.com";
const PROXY_HOST: &str = "ghproxy.cn";
const GITHUB_BASE: &str = "https://github.com/a145789/traffic-monitor";
const RELEASE_PAGE_URL: &str = "https://github.com/a145789/traffic-monitor/releases";
const TEMP_FILE_NAME: &str = "traffic-monitor-setup-temp.exe";
const HTTP_OK: u32 = 200;

const AUTO_CHECK_COOLDOWN_SECS: u64 = 3600;
const AUTO_CHECK_ERROR_COOLDOWN_SECS: u64 = 300;

static LAST_CHECK_TIME: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));
static LATEST_VERSION: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));
static TEMP_FILE_PATH: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));
static UPDATE_ERROR_MESSAGE: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));

struct WinHttpHandles {
    h_request: *mut std::ffi::c_void,
    h_connect: *mut std::ffi::c_void,
    h_session: *mut std::ffi::c_void,
}

impl Drop for WinHttpHandles {
    fn drop(&mut self) {
        // SAFETY: 句柄来自成功的 WinHTTP API 调用，均为有效指针。
        unsafe {
            if !self.h_request.is_null() {
                let _ = WinHttpCloseHandle(self.h_request);
            }
            if !self.h_connect.is_null() {
                let _ = WinHttpCloseHandle(self.h_connect);
            }
            if !self.h_session.is_null() {
                let _ = WinHttpCloseHandle(self.h_session);
            }
        }
    }
}

struct BcryptHandles {
    h_hash: BCRYPT_HASH_HANDLE,
    h_alg: BCRYPT_ALG_HANDLE,
}

impl Drop for BcryptHandles {
    fn drop(&mut self) {
        // SAFETY: 句柄来自成功的 BCrypt API 调用，均有效。
        unsafe {
            if self.h_hash != BCRYPT_HASH_HANDLE::default() {
                let _ = BCryptDestroyHash(self.h_hash);
            }
            if self.h_alg != BCRYPT_ALG_HANDLE::default() {
                let _ = BCryptCloseAlgorithmProvider(self.h_alg, 0);
            }
        }
    }
}

fn check_status(status: i32, fn_name: &str) -> Result<(), String> {
    if status >= 0 {
        Ok(())
    } else {
        Err(format!("{fn_name} failed: 0x{status:08X}"))
    }
}

fn win32_code_from_hresult(code: u32) -> Option<u32> {
    const FACILITY_WIN32_HRESULT_PREFIX: u32 = 0x8007_0000;
    (code & 0xFFFF_0000 == FACILITY_WIN32_HRESULT_PREFIX).then_some(code & 0xFFFF)
}

fn friendly_error(op: &str, err: windows::core::Error) -> String {
    let hresult = err.code().0 as u32;
    let detail = match win32_code_from_hresult(hresult) {
        Some(ERROR_WINHTTP_TIMEOUT) => "连接超时 (ERROR_WINHTTP_TIMEOUT)".to_string(),
        Some(ERROR_WINHTTP_NAME_NOT_RESOLVED) => {
            "域名解析失败 (ERROR_WINHTTP_NAME_NOT_RESOLVED)".to_string()
        }
        Some(ERROR_WINHTTP_CANNOT_CONNECT) => {
            "无法连接到服务器 (ERROR_WINHTTP_CANNOT_CONNECT)".to_string()
        }
        Some(ERROR_WINHTTP_CONNECTION_ERROR) => {
            "连接异常终止 (ERROR_WINHTTP_CONNECTION_ERROR)".to_string()
        }
        Some(ERROR_WINHTTP_SECURE_FAILURE) => "安全连接失败 (SSL/TLS 证书校验失败)".to_string(),
        Some(code) if code == ERROR_ACCESS_DENIED.0 => "拒绝访问 (ACCESS_DENIED)".to_string(),
        _ => format!("系统错误码: 0x{hresult:08X}"),
    };
    format!("{op}失败: {detail}")
}

fn fetch_url(host: &str, path: &str, secure: bool) -> Result<Vec<u8>, String> {
    let agent = to_wide("Traffic Monitor");
    let host_wide = to_wide(host);
    let path_wide = to_wide(path);

    // RAII 守卫：Drop 会关闭所有非空句柄。
    let mut handles = WinHttpHandles {
        h_request: std::ptr::null_mut(),
        h_connect: std::ptr::null_mut(),
        h_session: std::ptr::null_mut(),
    };

    // SAFETY:
    // agent 是有效的 NUL 终止宽字符串（来自 to_wide）。
    // 所有输出参数均在栈上分配且对齐正确。
    // WinHttpOpen 返回 HINTERNET 或失败时返回 null。
    handles.h_session = unsafe {
        WinHttpOpen(
            Some(&PCWSTR(agent.as_ptr())),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            None,
            None,
            0,
        )
    };
    if handles.h_session.is_null() {
        return Err(friendly_error(
            "初始化网络库",
            windows::core::Error::from_thread(),
        ));
    }

    // SAFETY:
    // handles.h_session 是 WinHttpOpen 返回的有效 HINTERNET。
    // 所有超时值均为正 i32 毫秒数。
    unsafe {
        let _ = WinHttpSetTimeouts(handles.h_session, 15000, 15000, 15000, 15000);
    }

    let port = if secure {
        INTERNET_DEFAULT_HTTPS_PORT
    } else {
        INTERNET_DEFAULT_HTTP_PORT
    };

    // SAFETY:
    // handles.h_session 有效；host_wide 是有效的 NUL 终止宽字符串。
    // WinHttpConnect 返回 HINTERNET 或失败时返回 null。
    handles.h_connect =
        unsafe { WinHttpConnect(handles.h_session, PCWSTR(host_wide.as_ptr()), port, 0) };
    if handles.h_connect.is_null() {
        return Err(friendly_error(
            "建立网络连接",
            windows::core::Error::from_thread(),
        ));
    }

    // SAFETY:
    // handles.h_connect 来自 WinHttpConnect，有效。
    // path_wide 是有效的 NUL 终止宽字符串。
    // 其余参数使用安全默认值（None/null）。
    // WinHttpOpenRequest 返回 HINTERNET 或失败时返回 null。
    handles.h_request = unsafe {
        WinHttpOpenRequest(
            handles.h_connect,
            w!("GET"),
            PCWSTR(path_wide.as_ptr()),
            None,
            None,
            std::ptr::null(),
            if secure {
                WINHTTP_FLAG_SECURE
            } else {
                Default::default()
            },
        )
    };
    if handles.h_request.is_null() {
        return Err(friendly_error(
            "创建网络请求",
            windows::core::Error::from_thread(),
        ));
    }

    // SAFETY:
    // handles.h_request 来自 WinHttpOpenRequest，有效。
    // GET 请求无附加缓冲区（lpOptional 为 null，dwOptionalLength 为 0）。
    unsafe {
        WinHttpSendRequest(handles.h_request, None, Some(std::ptr::null()), 0, 0, 0)
            .map_err(|e| friendly_error("发送网络请求", e))?;
    }

    // SAFETY:
    // handles.h_request 有效；lpBuffersReceived 为 null（由 API 内部分配）。
    unsafe {
        WinHttpReceiveResponse(handles.h_request, std::ptr::null_mut())
            .map_err(|e| friendly_error("接收网络响应", e))?;
    }

    let mut status_code: u32 = 0;
    let mut status_code_size = std::mem::size_of::<u32>() as u32;

    // SAFETY:
    // handles.h_request 有效。
    // &mut status_code 转换为 *mut _ 提供有效的 u32 缓冲区。
    // status_code_size 与缓冲区大小匹配。
    // lpwszName 为 null（查询主头部）。
    unsafe {
        WinHttpQueryHeaders(
            handles.h_request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            None,
            Some(&mut status_code as *mut u32 as *mut _),
            &mut status_code_size,
            std::ptr::null_mut(),
        )
        .map_err(|e| friendly_error("获取响应状态码", e))?;
    }

    if status_code != HTTP_OK {
        return Err(format!("HTTP 状态码错误: {status_code}"));
    }

    let mut response = Vec::new();
    loop {
        let mut available: u32 = 0;

        // SAFETY:
        // handles.h_request 有效。
        // &mut available 是有效的 u32 输出参数。
        unsafe {
            WinHttpQueryDataAvailable(handles.h_request, &mut available)
                .map_err(|e| friendly_error("查询响应数据大小", e))?;
        }

        if available == 0 {
            break;
        }

        let mut buf = vec![0u8; available as usize];
        let mut read: u32 = 0;

        // SAFETY:
        // handles.h_request 有效。
        // buf 已分配 `available` 字节；as_mut_ptr() 返回有效的可写指针。
        // lpdwNumberOfBytesRead 是有效的 u32 输出参数。
        unsafe {
            WinHttpReadData(
                handles.h_request,
                buf.as_mut_ptr() as *mut _,
                available,
                &mut read,
            )
            .map_err(|e| friendly_error("读取响应数据", e))?;
        }

        if read == 0 {
            break;
        }
        response.extend_from_slice(&buf[..read as usize]);
    }

    Ok(response)
}

fn compute_sha256_hex(data: &[u8]) -> Result<String, String> {
    let mut h_alg = BCRYPT_ALG_HANDLE::default();

    // SAFETY:
    // BCRYPT_SHA256_ALGORITHM 是有效的算法标识符。
    // &mut h_alg 是算法句柄的输出参数。
    let status = unsafe {
        BCryptOpenAlgorithmProvider(
            &mut h_alg,
            BCRYPT_SHA256_ALGORITHM,
            None,
            Default::default(),
        )
    };
    check_status(status.0, "BCryptOpenAlgorithmProvider")?;

    // RAII 守卫：Drop 依次关闭 h_hash（非默认值时）和 h_alg。
    let mut guard = BcryptHandles {
        h_hash: BCRYPT_HASH_HANDLE::default(),
        h_alg,
    };

    let mut h_hash = BCRYPT_HASH_HANDLE::default();

    // SAFETY:
    // guard.h_alg 来自 BCryptOpenAlgorithmProvider，有效。
    // &mut h_hash 是输出参数；SHA-256 无需密钥或 IV。
    let status = unsafe { BCryptCreateHash(guard.h_alg, &mut h_hash, None, None, 0) };
    check_status(status.0, "BCryptCreateHash")?;
    guard.h_hash = h_hash;

    // SAFETY:
    // h_hash 来自 BCryptCreateHash，有效。
    // data 是有效的字节切片（Rust 切片保证）。
    let status = unsafe { BCryptHashData(h_hash, data, 0) };
    check_status(status.0, "BCryptHashData")?;

    let mut hash_bytes = [0u8; 32];

    // SAFETY:
    // h_hash 有效；hash_bytes 是 32 字节缓冲区，匹配 SHA-256 输出大小。
    let status = unsafe { BCryptFinishHash(h_hash, &mut hash_bytes, 0) };
    check_status(status.0, "BCryptFinishHash")?;

    Ok(format_hex(&hash_bytes))
}

fn format_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02X}");
    }
    s
}

fn compute_sha256_hex_file(path: &std::path::Path) -> Result<String, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("Failed to open file for hashing: {e}"))?;

    let mut h_alg = BCRYPT_ALG_HANDLE::default();

    // SAFETY:
    // BCRYPT_SHA256_ALGORITHM 是有效的算法标识符。
    // &mut h_alg 是算法句柄的输出参数。
    let status = unsafe {
        BCryptOpenAlgorithmProvider(
            &mut h_alg,
            BCRYPT_SHA256_ALGORITHM,
            None,
            Default::default(),
        )
    };
    check_status(status.0, "BCryptOpenAlgorithmProvider")?;

    // RAII 守卫：Drop 依次关闭 h_hash（非默认值时）和 h_alg。
    let mut guard = BcryptHandles {
        h_hash: BCRYPT_HASH_HANDLE::default(),
        h_alg,
    };

    let mut h_hash = BCRYPT_HASH_HANDLE::default();

    // SAFETY:
    // guard.h_alg 来自 BCryptOpenAlgorithmProvider，有效。
    // &mut h_hash 是输出参数；SHA-256 无需密钥或 IV。
    let status = unsafe { BCryptCreateHash(guard.h_alg, &mut h_hash, None, None, 0) };
    check_status(status.0, "BCryptCreateHash")?;
    guard.h_hash = h_hash;

    let mut buf = [0u8; 8192];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)
            .map_err(|e| format!("Failed to read file for hashing: {e}"))?;
        if n == 0 {
            break;
        }

        // SAFETY:
        // h_hash 来自 BCryptCreateHash，有效。
        // buf[..n] 是从文件读取的 n 字节有效切片。
        let status = unsafe { BCryptHashData(h_hash, &buf[..n], 0) };
        check_status(status.0, "BCryptHashData")?;
    }

    let mut hash_bytes = [0u8; 32];

    // SAFETY:
    // h_hash 有效；hash_bytes 是 32 字节缓冲区，匹配 SHA-256 输出大小。
    let status = unsafe { BCryptFinishHash(h_hash, &mut hash_bytes, 0) };
    check_status(status.0, "BCryptFinishHash")?;

    Ok(format_hex(&hash_bytes))
}

fn compare_versions(current: &str, latest: &str) -> bool {
    parse_version(latest) > parse_version(current)
}

fn parse_version(v: &str) -> Vec<u32> {
    let base = v.split('-').next().unwrap_or(v);
    base.split('.')
        .filter_map(|s| s.parse::<u32>().ok())
        .collect()
}

pub fn is_installed_version() -> bool {
    match std::env::current_exe() {
        Ok(exe) => match exe.parent() {
            Some(dir) => dir.join("unins000.exe").exists(),
            None => false,
        },
        Err(_) => false,
    }
}

pub fn load_auto_update_enabled() -> bool {
    reg_read_dword("Software\\Traffic Monitor", "EnableAutoUpdate")
        .map(|v| v != 0)
        .unwrap_or(true)
}

pub fn save_auto_update_enabled(enabled: bool) {
    reg_write_dword(
        "Software\\Traffic Monitor",
        "EnableAutoUpdate",
        if enabled { 1 } else { 0 },
    );
}

fn get_temp_installer_path() -> std::path::PathBuf {
    let local_appdata = std::env::var("LOCALAPPDATA")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    std::path::PathBuf::from(local_appdata)
        .join("Traffic Monitor")
        .join(TEMP_FILE_NAME)
}

pub fn start_auto_check(hwnd: HWND) {
    if !ENABLE_AUTO_UPDATE.load(Ordering::Acquire) {
        return;
    }

    if UPDATE_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return;
    }

    {
        let last = LAST_CHECK_TIME.lock().unwrap();
        if let Some(t) = *last
            && t.elapsed().as_secs() < AUTO_CHECK_COOLDOWN_SECS
        {
            UPDATE_IN_PROGRESS.store(false, Ordering::Release);
            return;
        }
    }

    let hwnd_raw: isize = hwnd.0 as isize;

    if std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            update_check_worker(hwnd_raw, false);
        })
        .is_err()
    {
        UPDATE_IN_PROGRESS.store(false, Ordering::Release);
    }
}

pub fn start_manual_check(hwnd: HWND) {
    if UPDATE_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        show_info("检查更新正在进行中，请稍后再试。");
        return;
    }

    let hwnd_raw: isize = hwnd.0 as isize;

    if std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            update_check_worker(hwnd_raw, true);
        })
        .is_err()
    {
        UPDATE_IN_PROGRESS.store(false, Ordering::Release);
        show_error("启动更新检查失败。");
    }
}

fn update_check_worker(hwnd_raw: isize, is_manual: bool) {
    let result = run_check_subprocess();
    let is_error = matches!(result, CheckResult::Error(_));

    let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);

    let mut posted = false;
    match result {
        CheckResult::NoUpdate => {
            if is_manual {
                post_update_status(hwnd, UPDATE_STATUS_NO_UPDATE);
                posted = true;
            }
        }
        CheckResult::PortableFound(version) => {
            *LATEST_VERSION.lock().unwrap() = version;
            post_update_status(hwnd, UPDATE_STATUS_PORTABLE_FOUND);
            posted = true;
        }
        CheckResult::InstalledReady(version, temp_path) => {
            *LATEST_VERSION.lock().unwrap() = version;
            *TEMP_FILE_PATH.lock().unwrap() = temp_path;
            post_update_status(hwnd, UPDATE_STATUS_INSTALLED_READY);
            posted = true;
        }
        CheckResult::Error(msg) => {
            *UPDATE_ERROR_MESSAGE.lock().unwrap() = msg;
            if is_manual {
                post_update_status(hwnd, UPDATE_STATUS_ERROR);
                posted = true;
            }
        }
    }

    if !is_manual {
        let mut last = LAST_CHECK_TIME.lock().unwrap();
        if is_error {
            // Offset the timestamp so only a short cooldown remains.
            *last = Some(
                Instant::now()
                    - std::time::Duration::from_secs(
                        AUTO_CHECK_COOLDOWN_SECS - AUTO_CHECK_ERROR_COOLDOWN_SECS,
                    ),
            );
        } else {
            *last = Some(Instant::now());
        }
    }

    if !posted {
        UPDATE_IN_PROGRESS.store(false, Ordering::Release);
        compact_and_trim();
    }
}

#[derive(Debug)]
enum CheckResult {
    NoUpdate,
    PortableFound(String),
    InstalledReady(String, String),
    Error(String),
}

fn do_update_check() -> CheckResult {
    let mut response = fetch_url(VERSION_HOST, VERSION_PATH, true);
    if response.is_err() {
        // 失败时增加 1 次重试，并等待 500ms 防止抖动
        std::thread::sleep(std::time::Duration::from_millis(500));
        response = fetch_url(VERSION_HOST, VERSION_PATH, true);
    }

    let response = match response {
        Ok(data) => data,
        Err(e) => return CheckResult::Error(format!("获取版本文件失败: {e}")),
    };

    let text = match String::from_utf8(response) {
        Ok(t) => t,
        Err(_) => return CheckResult::Error("版本文件编码不是 UTF-8".to_string()),
    };

    let lines: Vec<&str> = text.lines().map(|l| l.trim()).collect();
    if lines.len() < 2 {
        return CheckResult::Error("版本文件格式不正确 (不足两行)".to_string());
    }

    let latest_version = lines[0].to_string();
    let expected_hash_hex = lines[1].to_uppercase();

    let current_version = env!("CARGO_PKG_VERSION");
    if !compare_versions(current_version, &latest_version) {
        return CheckResult::NoUpdate;
    }

    if !is_installed_version() {
        return CheckResult::PortableFound(latest_version);
    }

    let download_path = format!(
        "/a145789/traffic-monitor/releases/download/v{latest_version}/TrafficMonitor-Setup-{latest_version}.exe"
    );

    let temp_path = get_temp_installer_path();
    let temp_path_str = temp_path.to_string_lossy().to_string();

    // Skip download if temp file already exists with matching hash.
    if temp_path.exists() {
        if let Ok(existing_hash) = compute_sha256_hex_file(&temp_path) {
            if existing_hash.to_uppercase() == expected_hash_hex {
                return CheckResult::InstalledReady(latest_version, temp_path_str);
            }
            let _ = std::fs::remove_file(&temp_path);
        } else {
            let _ = std::fs::remove_file(&temp_path);
        }
    }

    let mut installer_data = fetch_url(DOWNLOAD_HOST, &download_path, true);
    let mut err_msg = None;

    if let Err(e) = installer_data {
        err_msg = Some(format!("从主源下载失败: {e}"));
        let proxy_path = format!(
            "/{GITHUB_BASE}/releases/download/v{latest_version}/TrafficMonitor-Setup-{latest_version}.exe"
        );
        match fetch_url(PROXY_HOST, &proxy_path, true) {
            Ok(data) => {
                installer_data = Ok(data);
            }
            Err(pe) => {
                err_msg = Some(format!("主源失败({e}), 代理源失败({pe})"));
                installer_data = Err(pe);
            }
        }
    }

    let installer_data = match installer_data {
        Ok(data) => data,
        Err(_) => {
            return CheckResult::Error(err_msg.unwrap_or_else(|| "下载安装包失败".to_string()));
        }
    };

    let actual_hash_hex = match compute_sha256_hex(&installer_data) {
        Ok(h) => h,
        Err(e) => {
            return CheckResult::Error(format!("计算安装包哈希失败: {e}"));
        }
    };

    if actual_hash_hex.to_uppercase() != expected_hash_hex {
        return CheckResult::Error(format!(
            "安装包校验失败 (预期: {}, 实际: {})",
            expected_hash_hex, actual_hash_hex
        ));
    }

    if std::fs::write(&temp_path, &installer_data).is_err() {
        // Try creating the parent directory and retry once.
        if let Some(parent) = temp_path.parent() {
            let _ = std::fs::create_dir_all(parent);
            if std::fs::write(&temp_path, &installer_data).is_err() {
                return CheckResult::Error("保存安装包文件失败".to_string());
            }
        } else {
            return CheckResult::Error("保存安装包文件失败 (无法获取父目录)".to_string());
        }
    }

    CheckResult::InstalledReady(latest_version, temp_path_str)
}

/// 子进程入口：执行更新检查并将结果序列化到 stdout，供主进程解析。
///
/// 协议（单行，`|` 分隔）：
/// - `NO_UPDATE`
/// - `PORTABLE|<version>`
/// - `INSTALLED|<version>|<path>`
/// - `ERROR|<reason>`
///
/// 退出码：0 = 成功（含 NoUpdate），1 = Error。
pub fn subprocess_main() -> i32 {
    let result = do_update_check();
    let line = match &result {
        CheckResult::NoUpdate => "NO_UPDATE".to_string(),
        CheckResult::PortableFound(v) => format!("PORTABLE|{v}"),
        CheckResult::InstalledReady(v, p) => format!("INSTALLED|{v}|{p}"),
        CheckResult::Error(msg) => {
            let single_line_msg = msg.replace(['\r', '\n'], " ");
            format!("ERROR|{single_line_msg}")
        }
    };
    let _ = std::io::stdout().write_all(line.as_bytes());
    let _ = std::io::stdout().flush();
    if matches!(result, CheckResult::Error(_)) {
        1
    } else {
        0
    }
}

/// 主进程调用：re-exec 自身 `--check-update` 子进程，读取 stdout 解析结果。
///
/// winhttp/bcrypt 系列通过 /DELAYLOAD 延迟加载，主进程代码路径不触碰它们，
/// 因此稳态下这些 DLL 永不进入主进程空间；子进程退出后它们随之释放。
fn run_check_subprocess() -> CheckResult {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return CheckResult::Error("无法获取当前程序路径".to_string()),
    };
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let output = match std::process::Command::new(exe)
        .arg("--check-update")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => o,
        Err(e) => return CheckResult::Error(format!("无法启动更新子进程: {e}")),
    };
    let parsed = parse_check_result(&output.stdout);
    match parsed {
        CheckResult::Error(msg) => {
            if msg == "未知错误" && !output.status.success() {
                CheckResult::Error("检查更新子进程异常退出".to_string())
            } else {
                CheckResult::Error(msg)
            }
        }
        other => other,
    }
}

fn parse_check_result(stdout: &[u8]) -> CheckResult {
    let text = match std::str::from_utf8(stdout) {
        Ok(s) => s.trim(),
        Err(_) => return CheckResult::Error("无法解析子进程输出编码".to_string()),
    };
    if text.is_empty() {
        return CheckResult::Error("子进程未返回任何数据".to_string());
    }
    if text == "NO_UPDATE" {
        return CheckResult::NoUpdate;
    }
    if let Some(version) = text.strip_prefix("PORTABLE|") {
        return if version.is_empty() {
            CheckResult::Error("更新结果解析错误 (PORTABLE 无版本号)".to_string())
        } else {
            CheckResult::PortableFound(version.to_string())
        };
    }
    if let Some(payload) = text.strip_prefix("INSTALLED|") {
        return match payload.split_once('|') {
            Some((version, path)) if !version.is_empty() && !path.is_empty() => {
                CheckResult::InstalledReady(version.to_string(), path.to_string())
            }
            _ => CheckResult::Error("更新结果解析错误 (INSTALLED 无版本或路径)".to_string()),
        };
    }
    if text == "ERROR" {
        return CheckResult::Error("未知网络或内部错误".to_string());
    }
    if let Some(message) = text.strip_prefix("ERROR|") {
        return if message.is_empty() {
            CheckResult::Error("未知网络或内部错误".to_string())
        } else {
            CheckResult::Error(message.to_string())
        };
    }
    CheckResult::Error("未知错误".to_string())
}

fn post_update_status(hwnd: HWND, status: usize) {
    // SAFETY: hwnd 是有效的主窗口句柄，PostMessageW 线程安全。
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_USER_UPDATE_READY, WPARAM(status), LPARAM(0));
    }
}

pub fn handle_update_ready(hwnd: HWND, status: usize) {
    match status {
        UPDATE_STATUS_NO_UPDATE => {
            let version = env!("CARGO_PKG_VERSION");
            show_info(&format!("当前已是最新版本 (v{version})。"));
        }
        UPDATE_STATUS_PORTABLE_FOUND => {
            let version = LATEST_VERSION.lock().unwrap().clone();
            let msg = format!("发现新版本 v{version}。\n是否打开网页下载免安装版？");
            if show_yes_no(&msg) {
                open_url(RELEASE_PAGE_URL);
            }
        }
        UPDATE_STATUS_INSTALLED_READY => {
            let version = LATEST_VERSION.lock().unwrap().clone();
            let temp_path = TEMP_FILE_PATH.lock().unwrap().clone();
            let msg = format!("新版本 v{version} 已准备就绪。\n是否立即关闭程序并安装？");
            if show_yes_no(&msg) {
                launch_installer_and_exit(hwnd, &temp_path);
            }
        }
        UPDATE_STATUS_ERROR => {
            let error_msg = UPDATE_ERROR_MESSAGE.lock().unwrap().clone();
            let show_msg = if error_msg.is_empty() {
                "检查更新失败，请检查网络连接。".to_string()
            } else {
                format!("检查更新失败: {error_msg}")
            };
            show_error(&show_msg);
        }
        _ => {}
    }
    UPDATE_IN_PROGRESS.store(false, Ordering::Release);
}

fn show_yes_no(msg: &str) -> bool {
    let title = to_wide("Traffic Monitor");
    let msg_wide = to_wide(msg);
    let result = unsafe {
        MessageBoxW(
            None,
            PCWSTR(msg_wide.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONINFORMATION,
        )
    };
    result == windows::Win32::UI::WindowsAndMessaging::IDYES
}

fn open_url(url: &str) {
    let url_wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: url_wide 是有效的 NUL 终止宽字符串。
    unsafe {
        let _ = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(url_wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

fn launch_installer_and_exit(_hwnd: HWND, installer_path: &str) {
    let path_wide: Vec<u16> = installer_path
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let verb_wide: Vec<u16> = "runas\0".encode_utf16().collect();
    let params_wide = to_wide("/VERYSILENT /SUPPRESSMSGBOXES /NORESTART");

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        lpVerb: PCWSTR(verb_wide.as_ptr()),
        lpFile: PCWSTR(path_wide.as_ptr()),
        lpParameters: PCWSTR(params_wide.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // SAFETY: sei 已正确初始化，path_wide 和 verb_wide 均有效。
    let ok = unsafe { ShellExecuteExW(&mut sei) };

    if ok.is_ok() && sei.hInstApp.0 as usize > 32 {
        remove_tray_icon();
        // SAFETY: PostQuitMessage 从主线程调用是安全的。
        unsafe {
            PostQuitMessage(0);
        }
    } else {
        // SAFETY: GetLastError 在 ShellExecuteExW 失败后立即调用，结果有效。
        let err = unsafe { GetLastError() };
        if err == ERROR_CANCELLED {
            // User denied UAC, keep running
        } else {
            show_error(&format!("启动安装程序失败 (错误码: {err:?})"));
        }
    }
}

pub fn init_cleanup_temp() {
    let path = get_temp_installer_path();
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compare_versions() {
        assert!(compare_versions("0.4.2", "0.4.3"));
        assert!(!compare_versions("0.4.3", "0.4.2"));
        assert!(!compare_versions("0.4.2", "0.4.2"));
        assert!(compare_versions("0.3.9", "0.4.0"));
        assert!(compare_versions("0.4.2", "1.0.0"));
        assert!(!compare_versions("1.0.0", "0.4.2"));
    }

    #[test]
    fn test_compare_versions_with_suffix() {
        assert!(compare_versions("0.4.2", "0.4.3-nightly"));
        assert!(!compare_versions("0.4.3-nightly", "0.4.2"));
        assert!(!compare_versions("0.4.2-nightly", "0.4.2-nightly"));
        assert!(compare_versions("0.4.2-nightly", "0.4.3"));
    }

    #[test]
    fn test_format_hex() {
        assert_eq!(format_hex(&[0xAB, 0xCD]), "ABCD");
        assert_eq!(format_hex(&[0x00, 0xFF]), "00FF");
        assert_eq!(format_hex(&[0x12, 0x34, 0x56]), "123456");
    }

    #[test]
    fn test_hash_hex_case_insensitive() {
        let data = b"hello world";
        let hash = compute_sha256_hex(data).unwrap();
        let upper = hash.to_uppercase();
        let lower = hash.to_lowercase();
        assert_eq!(upper, lower.to_uppercase());
    }

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("0.4.2"), vec![0, 4, 2]);
        assert_eq!(parse_version("1.0.0"), vec![1, 0, 0]);
        assert_eq!(parse_version("0.4.3-nightly"), vec![0, 4, 3]);
        assert_eq!(parse_version("0.4"), vec![0, 4]);
        assert_eq!(parse_version("invalid"), Vec::<u32>::new());
    }

    // ===== parse_check_result =====

    #[test]
    fn test_parse_no_update() {
        let result = parse_check_result(b"NO_UPDATE");
        assert!(matches!(result, CheckResult::NoUpdate));
    }

    #[test]
    fn test_parse_portable() {
        let result = parse_check_result(b"PORTABLE|0.9.0");
        match result {
            CheckResult::PortableFound(v) => assert_eq!(v, "0.9.0"),
            other => panic!("expected PortableFound, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_installed() {
        let result =
            parse_check_result(b"INSTALLED|1.0.0|C:\\Temp\\traffic-monitor-setup-temp.exe");
        match result {
            CheckResult::InstalledReady(v, p) => {
                assert_eq!(v, "1.0.0");
                assert_eq!(p, "C:\\Temp\\traffic-monitor-setup-temp.exe");
            }
            other => panic!("expected InstalledReady, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_error() {
        let result = parse_check_result(b"ERROR|network error|proxy error");
        match result {
            CheckResult::Error(msg) => assert_eq!(msg, "network error|proxy error"),
            other => panic!("expected Error, got {other:?}"),
        }

        for input in [b"ERROR".as_slice(), b"ERROR|".as_slice()] {
            match parse_check_result(input) {
                CheckResult::Error(msg) => assert_eq!(msg, "未知网络或内部错误"),
                other => panic!("expected Error, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_winhttp_error_code_mapping() {
        let cannot_connect = 0x8007_0000 | ERROR_WINHTTP_CANNOT_CONNECT;
        let connection_error = 0x8007_0000 | ERROR_WINHTTP_CONNECTION_ERROR;

        assert_eq!(
            win32_code_from_hresult(cannot_connect),
            Some(ERROR_WINHTTP_CANNOT_CONNECT)
        );
        assert_eq!(
            win32_code_from_hresult(connection_error),
            Some(ERROR_WINHTTP_CONNECTION_ERROR)
        );
        assert_eq!(win32_code_from_hresult(0x8000_4005), None);
    }

    #[test]
    fn test_parse_empty_input() {
        assert!(matches!(parse_check_result(b""), CheckResult::Error(_)));
    }

    #[test]
    fn test_parse_garbage() {
        assert!(matches!(
            parse_check_result(b"some random garbage"),
            CheckResult::Error(_)
        ));
    }

    #[test]
    fn test_parse_installed_missing_path() {
        // INSTALLED 必须有版本号和路径两个字段。
        assert!(matches!(
            parse_check_result(b"INSTALLED|1.0.0"),
            CheckResult::Error(_)
        ));
    }

    #[test]
    fn test_parse_extra_pipes_in_path() {
        // 路径中可能包含 `|`（虽然 Windows 路径不允许），splitn(3, '|') 不应拆分。
        let result = parse_check_result(b"INSTALLED|2.0.0|C:\\A|B\\setup.exe");
        match result {
            CheckResult::InstalledReady(v, p) => {
                assert_eq!(v, "2.0.0");
                assert_eq!(p, "C:\\A|B\\setup.exe");
            }
            other => panic!("expected InstalledReady, got {other:?}"),
        }
    }

    // ===== compute_sha256_hex known-answer =====

    #[test]
    fn test_sha256_known_answer() {
        // "hello world" 的 SHA-256，由 shasum -a 256 确认。
        let expected = "B94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9";
        let hash = compute_sha256_hex(b"hello world").unwrap();
        assert_eq!(hash, expected);
    }
}
