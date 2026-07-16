use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
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
use crate::config::{HTTP_READ_CHUNK_BYTES, INSTALLER_MAX_BYTES, VERSION_METADATA_MAX_BYTES};
use crate::state::{ENABLE_AUTO_UPDATE, UPDATE_IN_PROGRESS};
use crate::tray::remove_tray_icon;
use crate::util::{reg_read_dword, reg_write_dword, show_error, show_info, to_wide};

pub const WM_USER_UPDATE_ACTION: u32 = WM_USER + 5;

const UPDATE_ACTION_EXIT_MAIN: usize = 1;

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

fn fetch_url(
    host: &str,
    path: &str,
    secure: bool,
    max_response_bytes: usize,
) -> Result<Vec<u8>, String> {
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

        let remaining = max_response_bytes.saturating_sub(response.len());
        if remaining == 0 {
            return Err(format!("响应数据超过大小上限 ({max_response_bytes} bytes)"));
        }
        let chunk_len = (available as usize)
            .min(HTTP_READ_CHUNK_BYTES)
            .min(remaining);
        let mut buf = vec![0u8; chunk_len];
        let mut read: u32 = 0;

        // SAFETY:
        // handles.h_request 有效；buf 是 chunk_len 字节的连续可写缓冲区。
        // 请求长度由 buf.len() 转换且不超过 u32，read 是有效的输出参数。
        unsafe {
            WinHttpReadData(
                handles.h_request,
                buf.as_mut_ptr() as *mut _,
                chunk_len as u32,
                &mut read,
            )
            .map_err(|e| friendly_error("读取响应数据", e))?;
        }

        let read = read as usize;
        if read == 0 {
            break;
        }
        if read > buf.len() {
            return Err("WinHTTP 返回了超过目标缓冲区的读取长度".to_string());
        }
        response.extend_from_slice(&buf[..read]);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

fn compare_versions(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(current), Some(latest)) => latest > current,
        _ => false,
    }
}

fn parse_version(value: &str) -> Option<Version> {
    let (base, suffix) = match value.split_once('-') {
        Some((base, suffix)) => (base, Some(suffix)),
        None => (value, None),
    };
    if suffix.is_some_and(|suffix| {
        suffix.is_empty()
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return None;
    }

    let mut parts = base.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }

    Some(Version {
        major,
        minor,
        patch,
    })
}

fn is_valid_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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

fn open_locked_installer(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    const FILE_SHARE_READ_ONLY: u32 = 0x0000_0001;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ_ONLY)
        .open(path)
}

fn create_locked_installer(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    const FILE_SHARE_READ_ONLY: u32 = 0x0000_0001;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ_ONLY)
        .open(path)
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
    // 更新相关提示必须全部由短生命周期子进程显示，避免 MessageBox/IME DLL
    // 因重复点击进入常驻主进程；已有检查运行时直接忽略本次点击。
    if UPDATE_IN_PROGRESS.swap(true, Ordering::AcqRel) {
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
    }
}

fn update_check_worker(hwnd_raw: isize, is_manual: bool) {
    let outcome = run_check_subprocess(is_manual);

    if !is_manual {
        let mut last = LAST_CHECK_TIME.lock().unwrap();
        if outcome.is_error {
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

    if outcome.action == UpdateAction::ExitMain && !outcome.is_error {
        let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
        if post_update_action(hwnd, UPDATE_ACTION_EXIT_MAIN) {
            return;
        }
    }

    UPDATE_IN_PROGRESS.store(false, Ordering::Release);
    compact_and_trim();
}

#[derive(Debug)]
struct VerifiedInstaller {
    version: String,
    path: std::path::PathBuf,
    // 保持只读共享句柄直到 ShellExecuteExW 返回，阻止其他进程改写或替换已校验文件。
    _file_lock: std::fs::File,
}

#[derive(Debug)]
enum CheckResult {
    NoUpdate,
    PortableFound(String),
    InstalledReady(VerifiedInstaller),
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateAction {
    Done,
    ExitMain,
}

struct SubprocessOutcome {
    action: UpdateAction,
    is_error: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum InstallerLaunch {
    Started,
    Cancelled,
    Failed(u32),
}

fn do_update_check() -> CheckResult {
    let mut response = fetch_url(
        VERSION_HOST,
        VERSION_PATH,
        true,
        VERSION_METADATA_MAX_BYTES,
    );
    if response.is_err() {
        // 失败时增加 1 次重试，并等待 500ms 防止抖动
        std::thread::sleep(std::time::Duration::from_millis(500));
        response = fetch_url(
            VERSION_HOST,
            VERSION_PATH,
            true,
            VERSION_METADATA_MAX_BYTES,
        );
    }

    let response = match response {
        Ok(data) => data,
        Err(e) => return CheckResult::Error(format!("获取版本文件失败: {e}")),
    };

    let text = match String::from_utf8(response) {
        Ok(t) => t,
        Err(_) => return CheckResult::Error("版本文件编码不是 UTF-8".to_string()),
    };

    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    if lines.len() != 2 {
        return CheckResult::Error("版本文件必须恰好包含版本号和 SHA-256 两行".to_string());
    }

    let latest_version = lines[0];
    if parse_version(latest_version).is_none() {
        return CheckResult::Error("版本号格式不正确，必须为 major.minor.patch".to_string());
    }
    let expected_hash_hex = lines[1];
    if !is_valid_sha256_hex(expected_hash_hex) {
        return CheckResult::Error("SHA-256 必须是 64 位十六进制字符串".to_string());
    }
    let expected_hash_hex = expected_hash_hex.to_ascii_uppercase();

    let current_version = env!("CARGO_PKG_VERSION");
    if !compare_versions(current_version, latest_version) {
        return CheckResult::NoUpdate;
    }

    if !is_installed_version() {
        return CheckResult::PortableFound(latest_version.to_string());
    }

    let download_path = format!(
        "/a145789/traffic-monitor/releases/download/v{latest_version}/TrafficMonitor-Setup-{latest_version}.exe"
    );

    let temp_path = get_temp_installer_path();

    // 若临时文件已存在且哈希匹配，以只读共享锁打开后直接复用。
    if temp_path.exists() {
        if let Ok(existing_hash) = compute_sha256_hex_file(&temp_path) {
            if existing_hash.to_uppercase() == expected_hash_hex {
                match open_locked_installer(&temp_path) {
                    Ok(file_lock) => {
                        return CheckResult::InstalledReady(VerifiedInstaller {
                            version: latest_version.to_string(),
                            path: temp_path,
                            _file_lock: file_lock,
                        });
                    }
                    Err(_) => {
                        // 无法锁定已有文件，删除后重新下载。
                        let _ = std::fs::remove_file(&temp_path);
                    }
                }
            } else {
                let _ = std::fs::remove_file(&temp_path);
            }
        } else {
            let _ = std::fs::remove_file(&temp_path);
        }
    }

    let mut installer_data = fetch_url(DOWNLOAD_HOST, &download_path, true, INSTALLER_MAX_BYTES);
    let mut err_msg = None;

    if let Err(e) = installer_data {
        err_msg = Some(format!("从主源下载失败: {e}"));
        let proxy_path = format!(
            "/{GITHUB_BASE}/releases/download/v{latest_version}/TrafficMonitor-Setup-{latest_version}.exe"
        );
        match fetch_url(PROXY_HOST, &proxy_path, true, INSTALLER_MAX_BYTES) {
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

    // 确保父目录存在后，以 create_new(true) + FILE_SHARE_READ 创建独占写锁文件。
    if let Some(parent) = temp_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 先尝试移除可能残留的无效文件（上次哈希不匹配或被中断的下载）。
    let _ = std::fs::remove_file(&temp_path);
    let mut file_lock = match create_locked_installer(&temp_path) {
        Ok(f) => f,
        Err(_) => {
            return CheckResult::Error("创建安装包文件失败".to_string());
        }
    };
    if file_lock.write_all(&installer_data).is_err() {
        return CheckResult::Error("写入安装包文件失败".to_string());
    }

    CheckResult::InstalledReady(VerifiedInstaller {
        version: latest_version.to_string(),
        path: temp_path,
        _file_lock: file_lock,
    })
}

/// 子进程入口：完成更新检查、用户交互和外部动作，仅将最终动作回传主进程。
///
/// stdout 单行协议：
/// - `DONE`：子进程已处理完毕，主进程继续运行。
/// - `EXIT_MAIN`：安装器已成功启动，主进程应退出。
///
/// 退出码：0 = 检查流程成功完成，1 = 更新检查失败。手动检查失败时，错误提示
/// 已由子进程显示；退出码只供主进程决定自动检查的重试冷却时间。
pub fn subprocess_main(is_manual: bool) -> i32 {
    let result = do_update_check();
    let is_error = matches!(result, CheckResult::Error(_));
    let action = complete_update_interaction(result, is_manual);
    let line = match action {
        UpdateAction::Done => "DONE",
        UpdateAction::ExitMain => "EXIT_MAIN",
    };

    let _ = std::io::stdout().write_all(line.as_bytes());
    let _ = std::io::stdout().flush();
    i32::from(is_error)
}

fn complete_update_interaction(result: CheckResult, is_manual: bool) -> UpdateAction {
    match result {
        CheckResult::NoUpdate => {
            if is_manual {
                let version = env!("CARGO_PKG_VERSION");
                show_info(&format!("当前已是最新版本 (v{version})。"));
            }
            UpdateAction::Done
        }
        CheckResult::PortableFound(version) => {
            let msg = format!("发现新版本 v{version}。\n是否打开网页下载免安装版？");
            if show_yes_no(&msg) {
                open_url(RELEASE_PAGE_URL);
            }
            UpdateAction::Done
        }
        CheckResult::InstalledReady(verified) => {
            let msg = format!(
                "新版本 v{} 已准备就绪。\n是否立即关闭程序并安装？",
                verified.version
            );
            if !show_yes_no(&msg) {
                return UpdateAction::Done;
            }

            // 启动安装器，文件锁在 ShellExecuteExW 返回后才释放。
            match launch_installer(verified) {
                InstallerLaunch::Started => UpdateAction::ExitMain,
                InstallerLaunch::Cancelled => UpdateAction::Done,
                InstallerLaunch::Failed(code) => {
                    show_error(&format!("启动安装程序失败 (错误码: {code})"));
                    UpdateAction::Done
                }
            }
        }
        CheckResult::Error(message) => {
            if is_manual {
                show_error(&format!("检查更新失败: {message}"));
            }
            UpdateAction::Done
        }
    }
}

/// 主进程调用：re-exec 自身 `--check-update` 子进程，等待其完成全部更新交互。
///
/// winhttp/bcrypt、MessageBox/IME 和 ShellExecute 相关 DLL 只会进入子进程；
/// 子进程退出后由操作系统整体回收，主进程只解析 `DONE/EXIT_MAIN` 最终动作。
///
/// 此处使用 `spawn()` + 手动读取 stdout，而非 `output()`，避免后者为并发读取
/// stderr 创建一个使用默认 2MB 栈预留的隐藏线程。
fn run_check_subprocess(is_manual: bool) -> SubprocessOutcome {
    let failed = || SubprocessOutcome {
        action: UpdateAction::Done,
        is_error: true,
    };
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return failed(),
    };

    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let mut command = std::process::Command::new(exe);
    command.arg("--check-update");
    if is_manual {
        command.arg("--manual");
    }

    let mut child = match command
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return failed(),
    };

    let mut stdout_data = Vec::new();
    let read_failed = match child.stdout.take() {
        Some(mut stdout) => stdout.read_to_end(&mut stdout_data).is_err(),
        None => true,
    };

    let exit_status = match child.wait() {
        Ok(status) => status,
        Err(_) => return failed(),
    };

    let parsed = parse_update_action(&stdout_data);
    SubprocessOutcome {
        action: parsed.unwrap_or(UpdateAction::Done),
        is_error: read_failed || parsed.is_none() || !exit_status.success(),
    }
}

fn parse_update_action(stdout: &[u8]) -> Option<UpdateAction> {
    match std::str::from_utf8(stdout).ok()?.trim() {
        "DONE" => Some(UpdateAction::Done),
        "EXIT_MAIN" => Some(UpdateAction::ExitMain),
        _ => None,
    }
}

fn post_update_action(hwnd: HWND, action: usize) -> bool {
    // SAFETY:
    // hwnd 来自主线程创建的窗口句柄，并且仅在主进程仍持有该窗口期间由工作线程使用。
    // PostMessageW 只向目标线程队列复制整数消息参数，不会跨线程解引用 Rust 内存；
    // 若窗口已销毁，API 会返回错误，调用本身不会访问无效内存。
    unsafe { PostMessageW(Some(hwnd), WM_USER_UPDATE_ACTION, WPARAM(action), LPARAM(0)).is_ok() }
}

pub fn handle_update_action(action: usize) {
    UPDATE_IN_PROGRESS.store(false, Ordering::Release);
    if action != UPDATE_ACTION_EXIT_MAIN {
        return;
    }

    remove_tray_icon();
    // SAFETY:
    // 此函数仅由主窗口过程处理 WM_USER_UPDATE_ACTION 时调用，因此当前线程就是
    // UI 消息循环所属线程；PostQuitMessage 会向当前线程队列投递 WM_QUIT。
    unsafe {
        PostQuitMessage(0);
    }
}

fn show_yes_no(msg: &str) -> bool {
    let title = to_wide("Traffic Monitor");
    let msg_wide = to_wide(msg);
    // SAFETY:
    // title 和 msg_wide 均由 to_wide 创建，包含尾部 NUL，且缓冲区在同步调用
    // MessageBoxW 返回前始终存活；None 表示对话框不依附其他窗口。
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
    // SAFETY:
    // url_wide 是包含尾部 NUL 的 UTF-16 缓冲区，并在同步的 ShellExecuteW 调用期间
    // 保持存活；其余字符串参数为 windows crate 提供的静态 NUL 终止字符串或 None。
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

fn launch_installer(verified: VerifiedInstaller) -> InstallerLaunch {
    let path_str = verified.path.to_string_lossy();
    let path_wide = to_wide(&path_str);
    let verb_wide = to_wide("runas");
    let params_wide = to_wide("/VERYSILENT /SUPPRESSMSGBOXES /NORESTART");

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        lpVerb: PCWSTR(verb_wide.as_ptr()),
        lpFile: PCWSTR(path_wide.as_ptr()),
        lpParameters: PCWSTR(params_wide.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // SAFETY:
    // path_wide、verb_wide 和 params_wide 都是 NUL 终止的 UTF-16 缓冲区，并在
    // ShellExecuteExW 同步读取 SHELLEXECUTEINFOW 期间保持存活。cbSize 与结构体
    // 实际大小一致，未设置需要调用方提供额外指针或接管进程句柄的掩码。
    let launched = unsafe { ShellExecuteExW(&mut sei) };

    // 安装器启动后（或启动失败后）释放文件锁——让 verified 的 _file_lock 在此函数
    // 返回时 drop，确保 ShellExecuteExW 执行期间安装器文件不可被同权限进程替换。
    drop(verified);

    if launched.is_ok() {
        return InstallerLaunch::Started;
    }

    // SAFETY:
    // 紧接失败的 ShellExecuteExW 调用读取当前线程 last-error，中间未调用其他
    // 可能覆盖错误码的 Win32 API。
    let error = unsafe { GetLastError() };
    if error == ERROR_CANCELLED {
        InstallerLaunch::Cancelled
    } else {
        InstallerLaunch::Failed(error.0)
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
    fn test_parse_version_valid() {
        assert_eq!(
            parse_version("0.4.2"),
            Some(Version {
                major: 0,
                minor: 4,
                patch: 2
            })
        );
        assert_eq!(
            parse_version("1.0.0"),
            Some(Version {
                major: 1,
                minor: 0,
                patch: 0
            })
        );
        assert_eq!(
            parse_version("0.4.3-nightly"),
            Some(Version {
                major: 0,
                minor: 4,
                patch: 3
            })
        );
    }

    #[test]
    fn test_parse_version_rejects_invalid() {
        // 不足三段
        assert_eq!(parse_version("0.4"), None);
        // 超过三段
        assert_eq!(parse_version("1.2.3.4"), None);
        // 非数字
        assert_eq!(parse_version("invalid"), None);
        assert_eq!(parse_version("1.x.3"), None);
        // 空段
        assert_eq!(parse_version("1..3"), None);
        // 空后缀
        assert_eq!(parse_version("1.2.3-"), None);
    }

    // ===== parse_update_action =====

    #[test]
    fn test_parse_done_action() {
        assert_eq!(parse_update_action(b"DONE"), Some(UpdateAction::Done));
        assert_eq!(parse_update_action(b"  DONE\r\n"), Some(UpdateAction::Done));
    }

    #[test]
    fn test_parse_exit_main_action() {
        assert_eq!(
            parse_update_action(b"EXIT_MAIN"),
            Some(UpdateAction::ExitMain)
        );
    }

    #[test]
    fn test_reject_invalid_update_actions() {
        for input in [
            b"".as_slice(),
            b"NO_UPDATE".as_slice(),
            b"EXIT_MAIN|extra".as_slice(),
            b"some random garbage".as_slice(),
            &[0xFF, 0xFE],
        ] {
            assert_eq!(parse_update_action(input), None);
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

    // ===== compute_sha256_hex known-answer =====

    #[test]
    fn test_sha256_known_answer() {
        // "hello world" 的 SHA-256，由 shasum -a 256 确认。
        let expected = "B94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9";
        let hash = compute_sha256_hex(b"hello world").unwrap();
        assert_eq!(hash, expected);
    }
}
