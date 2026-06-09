use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;
use windows::Win32::Foundation::{ERROR_CANCELLED, GetLastError, HWND, LPARAM, WPARAM};
use windows::Win32::Networking::WinHttp::*;
use windows::Win32::Security::Cryptography::*;
use windows::Win32::UI::Shell::{SHELLEXECUTEINFOW, ShellExecuteExW, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_YESNO, MessageBoxW, PostMessageW, PostQuitMessage,
    SW_SHOWNORMAL, WM_USER,
};
use windows::core::{PCWSTR, w};

use crate::config::{ENABLE_AUTO_UPDATE, UPDATE_IN_PROGRESS};
use crate::tray::remove_tray_icon;

pub const WM_USER_UPDATE_READY: u32 = WM_USER + 5;

pub const UPDATE_STATUS_NO_UPDATE: usize = 0;
pub const UPDATE_STATUS_PORTABLE_FOUND: usize = 1;
pub const UPDATE_STATUS_INSTALLED_READY: usize = 2;
pub const UPDATE_STATUS_ERROR: usize = 3;

const VERSION_HOST: &str = "raw.githubusercontent.com";
const VERSION_PATH: &str = "/a145789/traffic-monitor/main/assets/version.txt";
const DOWNLOAD_HOST: &str = "github.com";
const RELEASE_PAGE_URL: &str = "https://github.com/a145789/traffic-monitor/releases";
const TEMP_FILE_NAME: &str = "traffic-monitor-setup-temp.exe";
const HTTP_OK: u32 = 200;

const AUTO_CHECK_COOLDOWN_SECS: u64 = 3600;

static LAST_CHECK_TIME: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));
static LATEST_VERSION: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));
static TEMP_FILE_PATH: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));

struct WinHttpHandles {
    h_request: *mut std::ffi::c_void,
    h_connect: *mut std::ffi::c_void,
    h_session: *mut std::ffi::c_void,
}

impl Drop for WinHttpHandles {
    fn drop(&mut self) {
        // SAFETY: handles are valid pointers obtained from successful WinHTTP API calls.
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
        // SAFETY: handles are valid from successful BCrypt API calls.
        unsafe {
            let _ = BCryptDestroyHash(self.h_hash);
            let _ = BCryptCloseAlgorithmProvider(self.h_alg, 0);
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

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn fetch_url(host: &str, path: &str, secure: bool) -> Result<Vec<u8>, String> {
    let agent = to_wide("Traffic Monitor");
    let host_wide = to_wide(host);
    let path_wide = to_wide(path);

    // SAFETY: All pointers are valid for the duration of the calls.
    unsafe {
        let h_session = WinHttpOpen(
            Some(&PCWSTR(agent.as_ptr())),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            None,
            None,
            0,
        );
        if h_session.is_null() {
            return Err("WinHttpOpen returned null".to_string());
        }

        // Set 10-second timeouts to prevent thread hang.
        let _ = WinHttpSetTimeouts(h_session, 10000, 10000, 10000, 10000);

        let port = if secure {
            INTERNET_DEFAULT_HTTPS_PORT
        } else {
            INTERNET_DEFAULT_HTTP_PORT
        };

        let h_connect = WinHttpConnect(h_session, PCWSTR(host_wide.as_ptr()), port, 0);
        if h_connect.is_null() {
            let _ = WinHttpCloseHandle(h_session);
            return Err("WinHttpConnect returned null".to_string());
        }

        let h_request = WinHttpOpenRequest(
            h_connect,
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
        );
        if h_request.is_null() {
            let _ = WinHttpCloseHandle(h_connect);
            let _ = WinHttpCloseHandle(h_session);
            return Err("WinHttpOpenRequest returned null".to_string());
        }

        let _guard = WinHttpHandles {
            h_request,
            h_connect,
            h_session,
        };

        WinHttpSendRequest(h_request, None, Some(std::ptr::null()), 0, 0, 0)
            .map_err(|e| format!("WinHttpSendRequest failed: {e:?}"))?;

        WinHttpReceiveResponse(h_request, std::ptr::null_mut())
            .map_err(|e| format!("WinHttpReceiveResponse failed: {e:?}"))?;

        // Check status code
        let mut status_code: u32 = 0;
        let mut status_code_size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            h_request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            None,
            Some(&mut status_code as *mut u32 as *mut _),
            &mut status_code_size,
            std::ptr::null_mut(),
        )
        .map_err(|e| format!("WinHttpQueryHeaders failed: {e:?}"))?;

        if status_code != HTTP_OK {
            return Err(format!("HTTP status: {status_code}"));
        }

        let mut response = Vec::new();
        loop {
            let mut available: u32 = 0;
            WinHttpQueryDataAvailable(h_request, &mut available)
                .map_err(|e| format!("WinHttpQueryDataAvailable failed: {e:?}"))?;
            if available == 0 {
                break;
            }
            let mut buf = vec![0u8; available as usize];
            let mut read: u32 = 0;
            WinHttpReadData(h_request, buf.as_mut_ptr() as *mut _, available, &mut read)
                .map_err(|e| format!("WinHttpReadData failed: {e:?}"))?;
            if read == 0 {
                break;
            }
            response.extend_from_slice(&buf[..read as usize]);
        }

        Ok(response)
    }
}

fn compute_sha256_hex(data: &[u8]) -> Result<String, String> {
    // SAFETY: CNG API calls with properly typed handles.
    unsafe {
        let mut h_alg = BCRYPT_ALG_HANDLE::default();
        let status = BCryptOpenAlgorithmProvider(
            &mut h_alg,
            BCRYPT_SHA256_ALGORITHM,
            None,
            Default::default(),
        );
        check_status(status.0, "BCryptOpenAlgorithmProvider")?;

        let mut h_hash = BCRYPT_HASH_HANDLE::default();
        let status = BCryptCreateHash(h_alg, &mut h_hash, None, None, 0);
        check_status(status.0, "BCryptCreateHash")?;

        let _guard = BcryptHandles { h_hash, h_alg };

        let status = BCryptHashData(h_hash, data, 0);
        check_status(status.0, "BCryptHashData")?;

        let mut hash_bytes = [0u8; 32];
        let status = BCryptFinishHash(h_hash, &mut hash_bytes, 0);
        check_status(status.0, "BCryptFinishHash")?;

        Ok(format_hex(&hash_bytes))
    }
}

fn format_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join("")
}

fn compute_sha256_hex_file(path: &std::path::Path) -> Result<String, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("Failed to open file for hashing: {e}"))?;
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut data)
        .map_err(|e| format!("Failed to read file for hashing: {e}"))?;
    compute_sha256_hex(&data)
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
        Ok(exe) => {
            let has_unins = match exe.parent() {
                Some(dir) => dir.join("unins000.exe").exists(),
                None => false,
            };
            let in_program_files = exe
                .to_string_lossy()
                .to_lowercase()
                .contains("program files");
            has_unins && in_program_files
        }
        Err(_) => false,
    }
}

pub fn load_auto_update_enabled() -> bool {
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, RegOpenKeyExW, RegQueryValueExW,
    };

    let key_path: Vec<u16> = "Software\\Traffic Monitor\0".encode_utf16().collect();
    let value_name: Vec<u16> = "EnableAutoUpdate\0".encode_utf16().collect();
    let mut hkey = Default::default();

    let open_ok = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )
        .is_ok()
    };

    if open_ok {
        let _key_guard = crate::ffi_guard::RegKey::new(hkey);
        let mut dword: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let result = unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR(value_name.as_ptr()),
                None,
                None,
                Some(&mut dword as *mut u32 as *mut u8),
                Some(&mut size),
            )
        };
        if result.is_ok() {
            return dword != 0;
        }
    }

    true
}

pub fn save_auto_update_enabled(enabled: bool) {
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_WRITE, REG_CREATE_KEY_DISPOSITION, REG_DWORD, RegCreateKeyExW,
        RegSetValueExW,
    };

    let key_path: Vec<u16> = "Software\\Traffic Monitor\0".encode_utf16().collect();
    let value_name: Vec<u16> = "EnableAutoUpdate\0".encode_utf16().collect();
    let mut hkey = Default::default();
    let mut disposition = REG_CREATE_KEY_DISPOSITION(0);

    let open_ok = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            None,
            None,
            Default::default(),
            KEY_WRITE,
            None,
            &mut hkey,
            Some(&mut disposition),
        )
        .is_ok()
    };

    if open_ok {
        let _key_guard = crate::ffi_guard::RegKey::new(hkey);
        let dword: u32 = if enabled { 1 } else { 0 };
        unsafe {
            let _ = RegSetValueExW(
                hkey,
                PCWSTR(value_name.as_ptr()),
                Some(0),
                REG_DWORD,
                Some(std::slice::from_raw_parts(
                    &dword as *const u32 as *const u8,
                    std::mem::size_of::<u32>(),
                )),
            );
        }
    }
}

fn get_temp_installer_path() -> std::path::PathBuf {
    std::env::temp_dir().join(TEMP_FILE_NAME)
}

pub fn start_auto_check(hwnd: HWND) {
    if !ENABLE_AUTO_UPDATE.load(Ordering::Acquire) {
        return;
    }

    {
        let last = LAST_CHECK_TIME.lock().unwrap();
        if let Some(t) = *last {
            if t.elapsed().as_secs() < AUTO_CHECK_COOLDOWN_SECS {
                return;
            }
        }
    }

    if UPDATE_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return;
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
    let result = do_update_check(is_manual);
    let is_error = matches!(result, CheckResult::Error);

    let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);

    match result {
        CheckResult::NoUpdate => {
            if is_manual {
                post_update_status(hwnd, UPDATE_STATUS_NO_UPDATE);
            }
        }
        CheckResult::PortableFound(version) => {
            *LATEST_VERSION.lock().unwrap() = version;
            post_update_status(hwnd, UPDATE_STATUS_PORTABLE_FOUND);
        }
        CheckResult::InstalledReady(version, temp_path) => {
            *LATEST_VERSION.lock().unwrap() = version;
            *TEMP_FILE_PATH.lock().unwrap() = temp_path;
            post_update_status(hwnd, UPDATE_STATUS_INSTALLED_READY);
        }
        CheckResult::Error => {
            if is_manual {
                post_update_status(hwnd, UPDATE_STATUS_ERROR);
            }
        }
    }

    if !is_manual && !is_error {
        let mut last = LAST_CHECK_TIME.lock().unwrap();
        *last = Some(Instant::now());
    }

    UPDATE_IN_PROGRESS.store(false, Ordering::Release);
}

enum CheckResult {
    NoUpdate,
    PortableFound(String),
    InstalledReady(String, String),
    Error,
}

fn do_update_check(_is_manual: bool) -> CheckResult {
    let response = match fetch_url(VERSION_HOST, VERSION_PATH, true) {
        Ok(data) => data,
        Err(_) => return CheckResult::Error,
    };

    let text = match String::from_utf8(response) {
        Ok(t) => t,
        Err(_) => return CheckResult::Error,
    };

    let lines: Vec<&str> = text.lines().map(|l| l.trim()).collect();
    if lines.len() < 2 {
        return CheckResult::Error;
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

    let installer_data = match fetch_url(DOWNLOAD_HOST, &download_path, true) {
        Ok(data) => data,
        Err(_) => {
            return CheckResult::Error;
        }
    };

    let actual_hash_hex = match compute_sha256_hex(&installer_data) {
        Ok(h) => h,
        Err(_) => {
            return CheckResult::Error;
        }
    };

    if actual_hash_hex.to_uppercase() != expected_hash_hex {
        return CheckResult::Error;
    }

    if std::fs::write(&temp_path, &installer_data).is_err() {
        return CheckResult::Error;
    }

    CheckResult::InstalledReady(latest_version, temp_path_str)
}

fn post_update_status(hwnd: HWND, status: usize) {
    // SAFETY: hwnd is the valid main window handle, PostMessageW is thread-safe.
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
            show_error("检查更新失败，请检查网络连接。");
        }
        _ => {}
    }
}

fn show_info(msg: &str) {
    let title: Vec<u16> = "Traffic Monitor\0".encode_utf16().collect();
    let msg_wide: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: title and msg_wide are valid null-terminated wide strings.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(msg_wide.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

fn show_error(msg: &str) {
    let title: Vec<u16> = "Traffic Monitor\0".encode_utf16().collect();
    let msg_wide: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: title and msg_wide are valid null-terminated wide strings.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(msg_wide.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn show_yes_no(msg: &str) -> bool {
    let title: Vec<u16> = "Traffic Monitor\0".encode_utf16().collect();
    let msg_wide: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: title and msg_wide are valid null-terminated wide strings.
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
    // SAFETY: url_wide is a valid null-terminated wide string.
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

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        lpVerb: PCWSTR(verb_wide.as_ptr()),
        lpFile: PCWSTR(path_wide.as_ptr()),
        nShow: SW_SHOWNORMAL.0 as i32,
        ..Default::default()
    };

    // SAFETY: sei is properly initialized, path_wide and verb_wide are valid.
    let ok = unsafe { ShellExecuteExW(&mut sei) };

    if ok.is_ok() && sei.hInstApp.0 as usize > 32 {
        remove_tray_icon();
        // SAFETY: PostQuitMessage is safe from the main thread.
        unsafe {
            PostQuitMessage(0);
        }
    } else {
        // SAFETY: GetLastError is valid immediately after ShellExecuteExW failure.
        let err = unsafe { GetLastError() };
        if err == ERROR_CANCELLED {
            // User denied UAC, keep running
        } else {
            show_error(&format!("启动安装程序失败 (错误码: {err:?})"));
        }
    }
}

pub fn init_cleanup_temp() {
    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(3));
            let path = get_temp_installer_path();
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
        })
        .ok();
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
}
