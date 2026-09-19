//! WinHTTP 抓取与友好的中文错误映射。
//!
//! 元数据仍走 `fetch_url`（整包内存，上限仅 4KiB）；安装包走 `fetch_to_file`
//!（固定缓冲复用，边读边哈希边写，上限 `INSTALLER_MAX_BYTES`）。

use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows::Win32::Networking::WinHttp::*;
use windows::core::{PCWSTR, w};

use super::crypto::Sha256;
use crate::config::{APP_TITLE, HTTP_READ_CHUNK_BYTES, HTTP_TIMEOUT_MS};
use crate::util::to_wide;

const HTTP_OK: u32 = 200;

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

/// 已验证 200 的 GET 响应：建连到验状态码全序列唯一实现于 `open`，
/// 上限约束与分块读取唯一实现于 `for_each_chunk`，两抓取函数只提供消费闭包。
/// `for_each_chunk` 按值收 `self`：响应流是单向抽干的一次性消费，编译期禁止二次读取。
struct HttpGet {
    handles: WinHttpHandles,
}

impl HttpGet {
    fn open(host: &str, path: &str) -> Result<Self, FetchFileError> {
        let agent = to_wide(APP_TITLE);
        let host_wide = to_wide(host);
        let path_wide = to_wide(path);

        // RAII 守卫：Drop 按 request → connect → session 顺序关闭非空句柄。
        let mut handles = WinHttpHandles {
            h_request: std::ptr::null_mut(),
            h_connect: std::ptr::null_mut(),
            h_session: std::ptr::null_mut(),
        };

        // SAFETY: agent 为 NUL 终止宽字符串；失败返回 null。
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
            return Err(FetchFileError::Download(friendly_error(
                "初始化网络库",
                windows::core::Error::from_thread(),
            )));
        }

        // SAFETY: h_session 有效；超时值均为正 i32 毫秒数。
        // 超时设置失败直接早退：全仓唯一一次设置，静默继续会退回 WinHTTP 默认超时。
        unsafe {
            WinHttpSetTimeouts(
                handles.h_session,
                HTTP_TIMEOUT_MS,
                HTTP_TIMEOUT_MS,
                HTTP_TIMEOUT_MS,
                HTTP_TIMEOUT_MS,
            )
            .map_err(|e| FetchFileError::Download(friendly_error("设置网络超时", e)))?;
        }

        let port = INTERNET_DEFAULT_HTTPS_PORT;

        // SAFETY: h_session 有效；host_wide 为 NUL 终止宽字符串；失败返回 null。
        handles.h_connect =
            unsafe { WinHttpConnect(handles.h_session, PCWSTR(host_wide.as_ptr()), port, 0) };
        if handles.h_connect.is_null() {
            return Err(FetchFileError::Download(friendly_error(
                "建立网络连接",
                windows::core::Error::from_thread(),
            )));
        }

        // SAFETY: h_connect 有效；path_wide 为 NUL 终止宽字符串；其余取安全默认值。
        handles.h_request = unsafe {
            WinHttpOpenRequest(
                handles.h_connect,
                w!("GET"),
                PCWSTR(path_wide.as_ptr()),
                None,
                None,
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            )
        };
        if handles.h_request.is_null() {
            return Err(FetchFileError::Download(friendly_error(
                "创建网络请求",
                windows::core::Error::from_thread(),
            )));
        }

        // SAFETY: h_request 有效；GET 无附加缓冲区；响应缓冲由 API 内部分配。
        unsafe {
            WinHttpSendRequest(handles.h_request, None, Some(std::ptr::null()), 0, 0, 0)
                .map_err(|e| FetchFileError::Download(friendly_error("发送网络请求", e)))?;
        }
        unsafe {
            WinHttpReceiveResponse(handles.h_request, std::ptr::null_mut())
                .map_err(|e| FetchFileError::Download(friendly_error("接收网络响应", e)))?;
        }

        let mut status_code: u32 = 0;
        let mut status_code_size = std::mem::size_of::<u32>() as u32;

        // SAFETY: h_request 有效；&mut status_code 提供有效的 u32 缓冲区。
        unsafe {
            WinHttpQueryHeaders(
                handles.h_request,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                None,
                Some(&mut status_code as *mut u32 as *mut _),
                &mut status_code_size,
                std::ptr::null_mut(),
            )
            .map_err(|e| FetchFileError::Download(friendly_error("获取响应状态码", e)))?;
        }

        if status_code != HTTP_OK {
            return Err(FetchFileError::Download(format!(
                "HTTP 状态码错误: {status_code}"
            )));
        }

        Ok(Self { handles })
    }

    fn for_each_chunk(
        self,
        max_response_bytes: usize,
        mut on_chunk: impl FnMut(&[u8]) -> Result<(), FetchFileError>,
    ) -> Result<(), FetchFileError> {
        // 复用固定缓冲：每轮只取用前 chunk_len 字节，避免随包大小线性分配。
        let mut buf = vec![0u8; HTTP_READ_CHUNK_BYTES];
        let mut total: usize = 0;
        loop {
            let mut available: u32 = 0;

            // SAFETY: h_request 有效；&mut available 是有效的 u32 输出参数。
            unsafe {
                WinHttpQueryDataAvailable(self.handles.h_request, &mut available)
                    .map_err(|e| FetchFileError::Download(friendly_error("查询响应数据大小", e)))?;
            }

            if available == 0 {
                break;
            }

            let remaining = max_response_bytes.saturating_sub(total);
            if remaining == 0 {
                return Err(FetchFileError::Download(format!(
                    "响应数据超过大小上限 ({max_response_bytes} 字节)"
                )));
            }
            let chunk_len = (available as usize)
                .min(HTTP_READ_CHUNK_BYTES)
                .min(remaining);
            let mut read: u32 = 0;

            // SAFETY: h_request 有效；buf 前 chunk_len 字节可写（chunk_len <= buf.len()）。
            unsafe {
                WinHttpReadData(
                    self.handles.h_request,
                    buf.as_mut_ptr() as *mut _,
                    chunk_len as u32,
                    &mut read,
                )
                .map_err(|e| FetchFileError::Download(friendly_error("读取响应数据", e)))?;
            }

            let read = read as usize;
            if read == 0 {
                break;
            }
            if read > chunk_len {
                return Err(FetchFileError::Download(
                    "WinHTTP 返回了超过目标缓冲区的读取长度".to_string(),
                ));
            }
            on_chunk(&buf[..read])?;
            total += read;
        }

        Ok(())
    }
}

pub(super) fn fetch_url(
    host: &str,
    path: &str,
    max_response_bytes: usize,
) -> Result<Vec<u8>, String> {
    let map_err = |e: FetchFileError| match e {
        // 本路径无本地故障来源（收集闭包只做 Vec 写入），此臂仅为穷尽匹配；
        // 若将来在此引入 Local，必须同步改 fetch_url 的返回类型或调用方分支。
        FetchFileError::Download(msg) | FetchFileError::Local(msg) => msg,
    };
    let conn = HttpGet::open(host, path).map_err(map_err)?;
    let mut response = Vec::new();
    {
        let collect = |data: &[u8]| -> Result<(), FetchFileError> {
            response.extend_from_slice(data);
            Ok(())
        };
        conn.for_each_chunk(max_response_bytes, collect)
            .map_err(map_err)?;
    }

    Ok(response)
}

/// 流式下载安装包：边 `WinHttpReadData` 边 `Sha256::update` 边写文件。
///
/// 固定复用 `HTTP_READ_CHUNK_BYTES` 缓冲，全程累计字节数上限为 `max_response_bytes`
///（不只信 `Content-Length`，以实际读到为准）；调用方传入的应是已用
/// `create_new(true)` + `FILE_SHARE_READ` 建好的写锁文件。
/// 成功返回已下载内容的十六进制哈希（流式哈希）；失败返回结构化错误：
/// 抓取段（建连/发送/接收/状态码/查询/读取/超限）为 `Download`（可回落代理），
/// 哈希段与写入段为 `Local`（磁盘/加密本地故障，不回落，避免空耗整包流量）。
/// 文案均带中文 `op`，调用方按变体映射到回落决策，不要匹配文案猜来源。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FetchFileError {
    Download(String),
    Local(String),
}

pub(super) fn fetch_to_file(
    host: &str,
    path: &str,
    max_response_bytes: usize,
    file: &mut std::fs::File,
) -> Result<String, FetchFileError> {
    use std::io::Write;

    let conn = HttpGet::open(host, path)?;
    let hash =
        Sha256::new().map_err(|e| FetchFileError::Local(format!("计算安装包哈希失败: {e}")))?;
    {
        let consume = |data: &[u8]| -> Result<(), FetchFileError> {
            hash.update(data)
                .map_err(|e| FetchFileError::Local(format!("计算安装包哈希失败: {e}")))?;
            file.write_all(data)
                .map_err(|e| FetchFileError::Local(format!("写入安装包文件失败: {e}")))?;
            Ok(())
        };
        conn.for_each_chunk(max_response_bytes, consume)?;
    }

    file.flush()
        .map_err(|e| FetchFileError::Local(format!("写入安装包文件失败: {e}")))?;
    hash.finish()
        .map_err(|e| FetchFileError::Local(format!("计算安装包哈希失败: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
