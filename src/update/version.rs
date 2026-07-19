//! 版本号解析与远端 metadata 严格解析。
//!
//! 与 HTTP/加密/安装逻辑解耦：纯字符串/字节处理，便于单测。

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

pub(super) fn compare_versions(current: &str, latest: &str) -> bool {
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

/// 严格解析版本元数据文件：必须恰好两行（版本号 + SHA-256），版本号符合
/// `major.minor.patch`（可选已知后缀），哈希为恰好 64 位 ASCII 十六进制且不含内部 NUL。
/// 解析出的哈希统一转为大写，供后续校验比对。
pub(super) struct ParsedMetadata {
    pub(super) version: String,
    pub(super) hash_hex: String,
}

pub(super) fn parse_update_metadata(text: &str) -> Result<ParsedMetadata, String> {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    if lines.len() != 2 {
        return Err("版本文件必须恰好包含版本号和 SHA-256 两行".to_string());
    }

    let version_line = lines[0];
    if parse_version(version_line).is_none() {
        return Err("版本号格式不正确，必须为 major.minor.patch".to_string());
    }

    let hash_line = lines[1];
    if !is_valid_sha256_hex(hash_line) {
        return Err("SHA-256 必须是 64 位十六进制字符串".to_string());
    }

    Ok(ParsedMetadata {
        version: version_line.to_string(),
        hash_hex: hash_line.to_ascii_uppercase(),
    })
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

    #[test]
    fn test_parse_update_metadata_valid() {
        let text = "1.2.3\nB94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9\n";
        let m = parse_update_metadata(text).unwrap();
        assert_eq!(m.version, "1.2.3");
        assert_eq!(
            m.hash_hex,
            "B94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9"
        );
    }

    #[test]
    fn test_parse_update_metadata_lowercases_hash_to_upper() {
        let lower = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        let m = parse_update_metadata(&format!("0.1.0\n{lower}")).unwrap();
        assert_eq!(m.hash_hex, lower.to_ascii_uppercase());
    }

    #[test]
    fn test_parse_update_metadata_rejects_wrong_line_count() {
        // 0 行
        assert!(parse_update_metadata("").is_err());
        // 1 行
        assert!(parse_update_metadata("1.2.3").is_err());
        // 3 行
        assert!(
            parse_update_metadata(
                "1.2.3\nB94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9\nextra\n"
            )
            .is_err()
        );
        // 末尾多余空行（trim 后仍被记为一行）
        assert!(parse_update_metadata("1.2.3\nhash\n\n").is_err());
    }

    #[test]
    fn test_parse_update_metadata_rejects_bad_version_format() {
        let good = "B94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9";
        // 不足三段
        assert!(parse_update_metadata(&format!("1.2\n{good}")).is_err());
        // 超过三段
        assert!(parse_update_metadata(&format!("1.2.3.4\n{good}")).is_err());
        // 非数字段
        assert!(parse_update_metadata(&format!("1.x.3\n{good}")).is_err());
        // 空段
        assert!(parse_update_metadata(&format!("1..3\n{good}")).is_err());
        // 非法后缀
        assert!(parse_update_metadata(&format!("1.2.3-\n{good}")).is_err());
        // 仅前缀版本
        assert!(parse_update_metadata(&format!("invalid\n{good}")).is_err());
    }

    #[test]
    fn test_parse_update_metadata_rejects_bad_hash() {
        // 非 64 位
        assert!(parse_update_metadata("1.2.3\nABCD").is_err());
        assert!(
            parse_update_metadata(
                "1.2.3\nB94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE"
            )
            .is_err()
        );
        assert!(
            parse_update_metadata(
                "1.2.3\nB94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9AA"
            )
            .is_err()
        );
        // 非十六进制字符
        assert!(
            parse_update_metadata(
                "1.2.3\nZ94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9"
            )
            .is_err()
        );
        // 包含空格
        assert!(
            parse_update_metadata(
                "1.2.3\nB94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE908 F7ACE2EFCDE9"
            )
            .is_err()
        );
    }

    #[test]
    fn test_parse_update_metadata_rejects_internal_nul() {
        // 版本行包含内部 NUL：parse_version 走 split('.') 后 "3\0..." 段不可解析为 u32。
        assert!(
            parse_update_metadata(
                "1.2.3\0\0B94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9"
            )
            .is_err()
        );
        // 哈希行包含内部 NUL：NUL 非 ascii_hexdigit，长度也会超过 64。
        assert!(
            parse_update_metadata(
                "1.2.3\nB94D27B9934D3E08A52E52D7DA7DABFAC484EF\0E37A5380EE9088F7ACE2EFCDE9"
            )
            .is_err()
        );
    }

    #[test]
    fn test_parse_update_metadata_rejects_oversize_blob() {
        // 模拟远端返回超大的合法风格 blob：大量额外行使其超过两行限制即被拒。
        let huge = "0.0.1\n".repeat(10_000);
        assert!(parse_update_metadata(&huge).is_err());
    }

    #[test]
    fn test_parse_update_metadata_trims_whitespace() {
        // 合法但带前后空白与多余空白行（首尾被 trim 后正确解析）。
        let text =
            "  1.2.3  \n  B94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9  ";
        let m = parse_update_metadata(text).unwrap();
        assert_eq!(m.version, "1.2.3");
    }
}
