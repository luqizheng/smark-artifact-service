//! Artifact 逻辑产物容器（ART-01 §2.2）。

use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactType {
    Document,
    Report,
    Image,
    Diagram,
    Visualization,
    Presentation,
    Dataset,
    Media,
    File,
}

impl ArtifactType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactType::Document => "document",
            ArtifactType::Report => "report",
            ArtifactType::Image => "image",
            ArtifactType::Diagram => "diagram",
            ArtifactType::Visualization => "visualization",
            ArtifactType::Presentation => "presentation",
            ArtifactType::Dataset => "dataset",
            ArtifactType::Media => "media",
            ArtifactType::File => "file",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "document" => Self::Document,
            "report" => Self::Report,
            "image" => Self::Image,
            "diagram" => Self::Diagram,
            "visualization" => Self::Visualization,
            "presentation" => Self::Presentation,
            "dataset" => Self::Dataset,
            "media" => Self::Media,
            "file" => Self::File,
            _ => return None,
        })
    }
}

/// 生命周期：active → archived → deleted（逻辑删除，终态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    Active,
    Archived,
    Deleted,
}

impl LifecycleState {
    pub fn as_str(&self) -> &'static str {
        match self {
            LifecycleState::Active => "active",
            LifecycleState::Archived => "archived",
            LifecycleState::Deleted => "deleted",
        }
    }

    /// 状态机（ADR-ART-001 §10）：仅允许 active→archived→deleted，
    /// deleted 为终态，不允许任何回退。
    pub fn can_transition_to(&self, next: LifecycleState) -> bool {
        matches!(
            (self, next),
            (LifecycleState::Active, LifecycleState::Archived)
                | (LifecycleState::Archived, LifecycleState::Deleted)
        )
    }
}

/// 逻辑产物容器。内容永远通过不可变 [`crate::version::ArtifactVersion`] 表达。
#[derive(Debug, Clone)]
pub struct Artifact {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub project_id: Uuid,
    /// 项目内可选业务稳定键；唯一约束 tenant+project+key（非空时）。
    pub artifact_key: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub artifact_type: ArtifactType,
    pub category: Option<String>,
    pub lifecycle_state: LifecycleState,
    /// 可变指针，只能指向同 Artifact 的 READY Version（数据库触发器强制）。
    pub current_version_id: Option<Uuid>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    /// 乐观并发锚（ETag 来源）。
    pub row_revision: i64,
}

impl Artifact {
    pub fn accepts_writes(&self) -> bool {
        self.lifecycle_state != LifecycleState::Deleted
    }
}
