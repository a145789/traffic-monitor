//! WinHTTP 抓取与友好的中文错误映射。
//!
//! 仅依赖 WinHTTP API。`fetch_url` 返回完整响应字节，调用方决定如何消费。

use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows::Win32::Networking::WinHttp::*;
use windows::core::{PCWSTR, w};

use crate::config::HTTP_READ_CHUNK_BYTES;
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

pub(super) fn fetch_url(
    host: &str,
    path: &str,
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

    let port = INTERNET_DEFAULT_HTTPS_PORT;

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
            WINHTTP_FLAG_SECURE,
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
            return Err(format!("响应数据超过大小上限 ({max_response_bytes} 字节)"));
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
