//! BCrypt SHA-256 哈希计算与 RAII 句柄守卫。
//!
//! 与 HTTP/安装逻辑解耦：仅依赖 BCrypt API，输入字节切片或文件路径，输出大写十六进制哈希。

use windows::Win32::Security::Cryptography::*;

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
        Err(format!("{fn_name} 调用失败: 0x{status:08X}"))
    }
}

pub(super) fn compute_sha256_hex(data: &[u8]) -> Result<String, String> {
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

pub(super) fn compute_sha256_hex_file(path: &std::path::Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("打开待哈希文件失败: {e}"))?;

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
            .map_err(|e| format!("读取待哈希文件失败: {e}"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

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

    // ===== compute_sha256_hex known-answer =====

    #[test]
    fn test_sha256_known_answer() {
        // "hello world" 的 SHA-256，由 shasum -a 256 确认。
        let expected = "B94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9";
        let hash = compute_sha256_hex(b"hello world").unwrap();
        assert_eq!(hash, expected);
    }
}
