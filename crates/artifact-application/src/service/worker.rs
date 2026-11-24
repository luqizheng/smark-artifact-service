//! Worker 侧的纯规则与维护步骤（ART-03A A3A-04）。
//!
//! 这些函数不依赖具体数据库实现；数据库受控函数是同一规则的最终权威，
//! 这里的校验只是避免把明显不合法的上报发给数据库。

use crate::port::worker::WorkerCoordinationPort;
use artifact_domain::DomainError;

/// 数据库对 diagnostic_summary 的长度上限（字符数）。
pub const MAX_DIAGNOSTIC_CHARS: usize = 2048;

/// 稳定机器错误码的形式：大写字母开头，其后为大写字母、数字或下划线，最长 64 个字符。
/// 与数据库的 CHECK 约束一致。
pub fn is_valid_error_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    match bytes.split_first() {
        Some((first, rest)) => {
            bytes.len() <= 64
                && first.is_ascii_uppercase()
                && rest
                    .iter()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
        }
        None => false,
    }
}

/// 把 Worker 的诊断摘要整理成可以安全入库的形态：去掉控制字符、截断到上限。
/// 摘要是诊断信息，调用方不得放入输入正文。
pub fn sanitize_diagnostic(summary: Option<&str>) -> Option<String> {
    let cleaned: String = summary?
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
        .take(MAX_DIAGNOSTIC_CHARS)
        .collect();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

/// 协调维护：排队超时与租约失效的恢复，以及终态 Job 的输入清理。
/// 幂等；多个 Worker 同时调用时数据库侧只允许一个扫描器推进。
pub async fn run_coordination_maintenance(
    coordination: &dyn WorkerCoordinationPort,
) -> Result<(i64, i64), DomainError> {
    let recovered = coordination.recover_expired().await?;
    let purged = coordination.purge_expired_input().await?;
    Ok((recovered, purged))
}

#[cfg(test)]
mod tests {
    use super::{MAX_DIAGNOSTIC_CHARS, is_valid_error_code, sanitize_diagnostic};

    #[test]
    fn error_codes_are_stable_uppercase_identifiers() {
        for ok in [
            "X",
            "GENERATION_TIMEOUT",
            "STORAGE_UNAVAILABLE",
            "MAX_ATTEMPTS_EXCEEDED",
        ] {
            assert!(is_valid_error_code(ok), "{ok}");
        }
        for bad in [
            "",
            "cancelled_by_request",
            "lower",
            "1ABC",
            "_ABC",
            "HAS SPACE",
            "HAS-DASH",
            &"A".repeat(65),
        ] {
            assert!(!is_valid_error_code(bad), "{bad:?}");
        }
        assert!(is_valid_error_code(&"A".repeat(64)));
    }

    #[test]
    fn diagnostic_is_cleaned_truncated_and_never_empty() {
        assert_eq!(sanitize_diagnostic(None), None);
        assert_eq!(sanitize_diagnostic(Some("  \n")), None);
        assert_eq!(
            sanitize_diagnostic(Some("exit code 1\u{0000}\u{0007}")),
            Some("exit code 1".into())
        );
        assert_eq!(
            sanitize_diagnostic(Some("line one\n\tline two")),
            Some("line one\n\tline two".into())
        );
        let long = "x".repeat(MAX_DIAGNOSTIC_CHARS + 500);
        let cleaned = sanitize_diagnostic(Some(&long)).unwrap();
        assert_eq!(cleaned.chars().count(), MAX_DIAGNOSTIC_CHARS);
    }
}
