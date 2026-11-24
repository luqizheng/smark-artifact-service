//! ArtifactVersion：不可变产物快照（ART-01 §2.3）。

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionState {
    Pending,
    Ready,
    Failed,
    Quarantined,
}

impl VersionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            VersionState::Pending => "pending",
            VersionState::Ready => "ready",
            VersionState::Failed => "failed",
            VersionState::Quarantined => "quarantined",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "ready" => Self::Ready,
            "failed" => Self::Failed,
            "quarantined" => Self::Quarantined,
            _ => return None,
        })
    }

    /// 允许的状态转移（ART-01 §4.4：commit 通过 → READY；校验失败 → FAILED/QUARANTINED）。
    /// READY 是终态：内容与状态均不可再变更（数据库触发器强制）。
    pub fn can_transition_to(&self, next: VersionState) -> bool {
        matches!(
            (self, next),
            (VersionState::Pending, VersionState::Ready)
                | (VersionState::Pending, VersionState::Failed)
                | (VersionState::Pending, VersionState::Quarantined)
        )
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactVersion {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub project_id: Uuid,
    pub artifact_id: Uuid,
    /// Artifact 内单调递增（数据库触发器基于父行锁分配，禁止先读 max 再无锁插入）。
    pub version_number: i64,
    pub state: VersionState,
    pub source_format: Option<String>,
    /// file-service 文件标识（非对象存储 Key 全文）。
    pub source_object_id: Option<String>,
    pub source_sha256: Option<String>,
    pub generator_id: Option<String>,
    pub generator_version: Option<String>,
    pub adapter_version: Option<String>,
    /// 输出清单与生成元信息；READY 后不可变。
    pub manifest: Value,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub ready_at: Option<DateTime<Utc>>,
    pub failure_code: Option<String>,
}

impl ArtifactVersion {
    /// failed / quarantined 版本不得成为 current_version_id（ART-01 §2.3）。
    pub fn can_be_current(&self) -> bool {
        self.state == VersionState::Ready
    }
}
