//! BCrypt SHA-256 哈希计算与 RAII 句柄守卫。
//!
//! 与 HTTP/安装逻辑解耦：仅依赖 BCrypt API，输入可读流或已锁定句柄，输出大写十六进制哈希。

use windows::Win32::Security::Cryptography::*;

use crate::config::HASH_READ_BUF_BYTES;

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

/// 增量式 SHA-256 计算。句柄由 RAII 守卫托管，`finish` 后自动销毁。
/// 唯一生产消费者是锁定句柄重验（`compute_sha256_hex_locked`）：
/// 边读边喂，避免整包常驻内存。
pub(super) struct Sha256 {
    handles: BcryptHandles,
}

impl Sha256 {
    pub(super) fn new() -> Result<Self, String> {
        let mut h_alg = BCRYPT_ALG_HANDLE::default();
        // SAFETY: BCRYPT_SHA256_ALGORITHM 是有效算法标识符；&mut h_alg 是输出参数。
        let status = unsafe {
            BCryptOpenAlgorithmProvider(
                &mut h_alg,
                BCRYPT_SHA256_ALGORITHM,
                None,
                Default::default(),
            )
        };
        check_status(status.0, "BCryptOpenAlgorithmProvider")?;

        let mut handles = BcryptHandles {
            h_hash: BCRYPT_HASH_HANDLE::default(),
            h_alg,
        };

        let mut h_hash = BCRYPT_HASH_HANDLE::default();
        // SAFETY: handles.h_alg 有效；&mut h_hash 是输出参数；SHA-256 无需密钥或 IV。
        let status = unsafe { BCryptCreateHash(handles.h_alg, &mut h_hash, None, None, 0) };
        check_status(status.0, "BCryptCreateHash")?;
        handles.h_hash = h_hash;

        Ok(Self { handles })
    }

    pub(super) fn update(&self, data: &[u8]) -> Result<(), String> {
        // SAFETY: h_hash 有效；data 是 Rust 切片保证的有效缓冲区。
        let status = unsafe { BCryptHashData(self.handles.h_hash, data, 0) };
        check_status(status.0, "BCryptHashData")
    }

    pub(super) fn finish(self) -> Result<String, String> {
        let mut hash_bytes = [0u8; 32];
        // SAFETY: h_hash 有效；hash_bytes 是 32 字节缓冲区，匹配 SHA-256 输出大小。
        let status = unsafe { BCryptFinishHash(self.handles.h_hash, &mut hash_bytes, 0) };
        check_status(status.0, "BCryptFinishHash")?;
        Ok(format_hex(&hash_bytes))
    }
}

/// 整包一次性哈希，仅供已知答案与流式一致性单测保留。
/// 生产路径禁止使用：安装包必须走流式增量，避免随包大小线性常驻内存。
#[cfg(test)]
pub(super) fn compute_sha256_hex(data: &[u8]) -> Result<String, String> {
    let hash = Sha256::new()?;
    hash.update(data)?;
    hash.finish()
}

/// 对任意可读流做增量哈希。当前生产路径在下载完成后对锁定句柄做增量重验；
/// 下载阶段只负责流式写盘。调用方负责累计字节上限；本函数只管喂数，不截断。
pub(super) fn compute_sha256_hex_reader(reader: &mut impl std::io::Read) -> Result<String, String> {
    let hash = Sha256::new()?;
    let mut buf = [0u8; HASH_READ_BUF_BYTES];
    loop {
        let n = std::io::Read::read(reader, &mut buf)
            .map_err(|e| format!("读取待哈希文件失败: {e}"))?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n])?;
    }
    hash.finish()
}

/// 对已加只读共享锁的安装器句柄重算哈希。
/// 不变量：调用方必须已持有 `FILE_SHARE_READ` 锁；本函数只在该句柄上读，
/// 不按路径另开文件，避免“验后另开锁”的 TOCTOU 窗口。
/// 成功返回大写十六进制哈希；失败返回中文错误。
pub(super) fn compute_sha256_hex_locked(file: &mut std::fs::File) -> Result<String, String> {
    use std::io::Seek;
    file.rewind()
        .map_err(|e| format!("读取待哈希文件失败: {e}"))?;
    compute_sha256_hex_reader(file)
}

fn check_status(status: i32, fn_name: &str) -> Result<(), String> {
    if status >= 0 {
        Ok(())
    } else {
        Err(format!("{fn_name} 调用失败: 0x{status:08X}"))
    }
}

fn format_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02X}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::cache::open_locked_installer;

    #[test]
    fn test_format_hex() {
        assert_eq!(format_hex(&[0xAB, 0xCD]), "ABCD");
        assert_eq!(format_hex(&[0x00, 0xFF]), "00FF");
        assert_eq!(format_hex(&[0x12, 0x34, 0x56]), "123456");
    }

    // ===== compute_sha256_hex known-answer =====

    #[test]
    fn test_sha256_known_answer() {
        // "hello world" 的 SHA-256，由 shasum -a 256 确认。
        let expected = "B94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9";
        let hash = compute_sha256_hex(b"hello world").unwrap();
        assert_eq!(hash, expected);
    }

    #[test]
    fn test_sha256_incremental_matches_one_shot() {
        // 分块 update 应与一次性计算结果一致（Sha256 复用正确性）。
        let hash = Sha256::new().unwrap();
        hash.update(b"hello ").unwrap();
        hash.update(b"world").unwrap();
        assert_eq!(
            hash.finish().unwrap(),
            compute_sha256_hex(b"hello world").unwrap()
        );
    }

    #[test]
    fn test_streaming_reader_matches_one_shot_large_input() {
        // 本地大输入驱动流式（reader 分块）与整包两种哈希入口结果一致：
        // 200KiB 确定性序列，多次 update/分块读取的切分方式不得影响结果。
        let mut data = Vec::with_capacity(200 * 1024);
        for i in 0..(200 * 1024) {
            data.push((i % 251) as u8);
        }
        let expected = compute_sha256_hex(&data).unwrap();
        let mut cursor = std::io::Cursor::new(&data);
        assert_eq!(compute_sha256_hex_reader(&mut cursor).unwrap(), expected);
    }

    #[test]
    fn test_locked_handle_hash_matches_reader() {
        // 锁定句柄入口与流式 reader 入口一致：同一内容经文件句柄重读应得同一哈希。
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "traffic-monitor-hash-test-{}-{}.tmp",
            std::process::id(),
            "locked"
        ));
        let data = b"locked-handle-hash-check-payload";
        std::fs::write(&path, data).unwrap();
        let mut file = open_locked_installer(&path).unwrap();
        let via_locked = compute_sha256_hex_locked(&mut file).unwrap();
        let mut cursor = std::io::Cursor::new(data.as_slice());
        assert_eq!(via_locked, compute_sha256_hex_reader(&mut cursor).unwrap());
        drop(file);
        let _ = std::fs::remove_file(&path);
    }
}
