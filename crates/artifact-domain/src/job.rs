//! GenerationJob：生成任务状态机（ART-01 §2.6）。
//!
//! Job 状态与 Artifact 生命周期分离；失败 Job 不影响既有可用版本。

use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    RetryWait,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

impl JobState {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::RetryWait => "retry_wait",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
            JobState::TimedOut => "timed_out",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobState::Succeeded | JobState::Failed | JobState::Cancelled | JobState::TimedOut
        )
    }

    /// 状态机（ART-01 §2.6）：
    /// queued→running/cancelled；running→succeeded/failed/cancelled/timed_out/retry_wait；retry_wait→queued。
    pub fn can_transition_to(&self, next: JobState) -> bool {
        matches!(
            (self, next),
            (JobState::Queued, JobState::Running)
                | (JobState::Queued, JobState::Cancelled)
                | (JobState::Running, JobState::Succeeded)
                | (JobState::Running, JobState::Failed)
                | (JobState::Running, JobState::Cancelled)
                | (JobState::Running, JobState::TimedOut)
                | (JobState::Running, JobState::RetryWait)
                | (JobState::RetryWait, JobState::Queued)
        )
    }
}

#[derive(Debug, Clone)]
pub struct GenerationJob {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub project_id: Uuid,
    pub artifact_id: Uuid,
    pub target_version_id: Option<Uuid>,
    pub generator_id: String,
    pub generator_version: Option<String>,
    pub adapter_version: Option<String>,
    pub status: JobState,
    pub idempotency_key: Option<String>,
    pub request_hash: Option<String>,
    /// Durable caller identity（ART-04G v3 §5.1）：三者同空（legacy Job）或同非空。
    /// 一旦写入永久不可变（数据库触发器保证）；`request_fingerprint` 为小写
    /// 64 位十六进制 SHA-256，`fingerprint_version` > 0。
    pub caller_request_id: Option<Uuid>,
    pub request_fingerprint: Option<String>,
    pub fingerprint_version: Option<i16>,
    pub attempt_count: i32,
    pub max_attempts: i32,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub trace_id: Option<String>,
    /// running 期间收到取消请求的时间；Job 此时仍是 running，等待 Worker 确认。
    pub cancel_requested_at: Option<DateTime<Utc>>,
}

impl GenerationJob {
    /// 调用方是否已经请求取消（尚未成为终态）。
    pub fn cancel_requested(&self) -> bool {
        self.cancel_requested_at.is_some()
    }
}
