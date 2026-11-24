//! ArtifactOutput：Version 的可查询派生输出（ART-01 §2.4）。

use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputRole {
    Primary,
    Preview,
    Attachment,
    SourceMap,
}

impl OutputRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            OutputRole::Primary => "primary",
            OutputRole::Preview => "preview",
            OutputRole::Attachment => "attachment",
            OutputRole::SourceMap => "source-map",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactOutput {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub project_id: Uuid,
    pub artifact_id: Uuid,
    pub version_id: Uuid,
    pub output_role: OutputRole,
    pub format: String,
    pub mime_type: String,
    pub size_bytes: i64,
    /// 小写十六进制 SHA-256。
    pub sha256: String,
    /// file-service 文件标识。
    pub file_object_id: String,
    pub safety_status: SafetyStatus,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyStatus {
    Pending,
    Passed,
    Rejected,
}

impl SafetyStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SafetyStatus::Pending => "pending",
            SafetyStatus::Passed => "passed",
            SafetyStatus::Rejected => "rejected",
        }
    }
}
