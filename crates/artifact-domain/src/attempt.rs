//! GenerationJobAttempt / Lease：Worker 执行事实（ART-03A A3A-03/A3A-04）。
//!
//! Attempt 是执行事实，Job 是用户可见的聚合状态。一次实际执行对应一个不可复用
//! 的 Attempt；租约 token 明文只在 claim 响应中出现一次，本层不持有明文。

use chrono::{DateTime, Utc};
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptState {
    Claimed,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    LeaseExpired,
}

impl AttemptState {
    pub fn as_str(&self) -> &'static str {
        match self {
            AttemptState::Claimed => "claimed",
            AttemptState::Running => "running",
            AttemptState::Succeeded => "succeeded",
            AttemptState::Failed => "failed",
            AttemptState::Cancelled => "cancelled",
            AttemptState::TimedOut => "timed_out",
            AttemptState::LeaseExpired => "lease_expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "claimed" => AttemptState::Claimed,
            "running" => AttemptState::Running,
            "succeeded" => AttemptState::Succeeded,
            "failed" => AttemptState::Failed,
            "cancelled" => AttemptState::Cancelled,
            "timed_out" => AttemptState::TimedOut,
            "lease_expired" => AttemptState::LeaseExpired,
            _ => return None,
        })
    }

    pub fn is_active(&self) -> bool {
        matches!(self, AttemptState::Claimed | AttemptState::Running)
    }

    pub fn is_terminal(&self) -> bool {
        !self.is_active()
    }
}

/// 执行事实（对应 artifact.generation_job_attempt 行；不含 lease token 明文）。
#[derive(Debug, Clone)]
pub struct GenerationJobAttempt {
    pub id: Uuid,
    pub job_id: Uuid,
    pub attempt_number: i32,
    pub worker_id: String,
    pub state: AttemptState,
    pub lease_token_hash: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error_code: Option<String>,
    pub diagnostic_summary: Option<String>,
    pub output_sha256: Option<String>,
}

/// Worker claim 响应：包含一次性的 lease token 明文与只读输入快照。
/// 明文不得写入日志/trace/错误；调用方用完后即丢弃。
#[derive(Clone)]
pub struct ClaimedJob {
    pub job_id: Uuid,
    pub attempt_id: Uuid,
    pub attempt_number: i32,
    pub lease_token: String,
    pub lease_expires_at: DateTime<Utc>,
    /// 本 Attempt 的最大执行时限；heartbeat 不会把租约续到它之后。
    pub deadline_at: DateTime<Utc>,
    pub input_payload: serde_json::Value,
    pub options_payload: Option<serde_json::Value>,
    pub input_schema_version: Option<String>,
    pub input_sha256: Option<String>,
    pub generator_id: String,
    pub generator_version: Option<String>,
    pub adapter_version: Option<String>,
    pub max_attempts: i32,
}

/// 租约 token 与输入正文属于敏感内容：手写 Debug 把它们脱敏，
/// 这样任何带问号占位的日志都不会泄露。
impl fmt::Debug for ClaimedJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaimedJob")
            .field("job_id", &self.job_id)
            .field("attempt_id", &self.attempt_id)
            .field("attempt_number", &self.attempt_number)
            .field("lease_expires_at", &self.lease_expires_at)
            .field("deadline_at", &self.deadline_at)
            .field("lease_token", &"[redacted]")
            .field("input_payload", &"[redacted]")
            .field("options_payload", &"[redacted]")
            .field("input_schema_version", &self.input_schema_version)
            .field("input_sha256", &self.input_sha256)
            .field("generator_id", &self.generator_id)
            .field("generator_version", &self.generator_version)
            .field("adapter_version", &self.adapter_version)
            .field("max_attempts", &self.max_attempts)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{AttemptState, ClaimedJob};
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn active_vs_terminal() {
        assert!(AttemptState::Claimed.is_active());
        assert!(AttemptState::Running.is_active());
        assert!(AttemptState::LeaseExpired.is_terminal());
        assert!(AttemptState::Succeeded.is_terminal());
        assert_eq!(
            AttemptState::parse("lease_expired"),
            Some(AttemptState::LeaseExpired)
        );
        assert_eq!(AttemptState::parse("bogus"), None);
    }

    /// Debug 输出不得包含租约明文或输入正文。
    #[test]
    fn claimed_job_debug_redacts_secrets() {
        let now = Utc::now();
        let claimed = ClaimedJob {
            job_id: Uuid::new_v4(),
            attempt_id: Uuid::new_v4(),
            attempt_number: 1,
            lease_token: "lease-plaintext-must-not-leak".into(),
            lease_expires_at: now,
            deadline_at: now,
            input_payload: json!({"secret": "business-text-must-not-leak"}),
            options_payload: Some(json!({"theme": "option-text-must-not-leak"})),
            input_schema_version: Some("1".into()),
            input_sha256: Some("a".repeat(64)),
            generator_id: "seecode".into(),
            generator_version: Some("0.6.3".into()),
            adapter_version: Some("adapter-1".into()),
            max_attempts: 3,
        };
        let rendered = format!("{claimed:?}");
        assert!(rendered.contains("lease_token: \"[redacted]\""));
        assert!(rendered.contains("input_payload: \"[redacted]\""));
        assert!(rendered.contains("options_payload: \"[redacted]\""));
        for secret in [
            "lease-plaintext-must-not-leak",
            "business-text-must-not-leak",
            "option-text-must-not-leak",
        ] {
            assert!(!rendered.contains(secret), "Debug leaked {secret}");
        }
    }
}
