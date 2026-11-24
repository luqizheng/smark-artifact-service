//! ArtifactReference：与外部业务对象的通用关联（ART-01 §2.5）。
//!
//! 不建立跨服务外键；owner_type/owner_id 的业务语义由 AI-PM 校验，
//! Artifact Service 不解析其含义、不凭其访问业务对象。

use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerType {
    Requirement,
    Design,
    Adr,
    Task,
    Knowledge,
    Project,
}

impl OwnerType {
    pub fn as_str(&self) -> &'static str {
        match self {
            OwnerType::Requirement => "requirement",
            OwnerType::Design => "design",
            OwnerType::Adr => "adr",
            OwnerType::Task => "task",
            OwnerType::Knowledge => "knowledge",
            OwnerType::Project => "project",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "requirement" => Self::Requirement,
            "design" => Self::Design,
            "adr" => Self::Adr,
            "task" => Self::Task,
            "knowledge" => Self::Knowledge,
            "project" => Self::Project,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationType {
    GeneratedFrom,
    Illustrates,
    Supports,
    References,
    EvidenceFor,
    DeliveredBy,
}

impl RelationType {
    pub fn as_str(&self) -> &'static str {
        match self {
            RelationType::GeneratedFrom => "generated_from",
            RelationType::Illustrates => "illustrates",
            RelationType::Supports => "supports",
            RelationType::References => "references",
            RelationType::EvidenceFor => "evidence_for",
            RelationType::DeliveredBy => "delivered_by",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "generated_from" => Self::GeneratedFrom,
            "illustrates" => Self::Illustrates,
            "supports" => Self::Supports,
            "references" => Self::References,
            "evidence_for" => Self::EvidenceFor,
            "delivered_by" => Self::DeliveredBy,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactReference {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub project_id: Uuid,
    pub artifact_id: Uuid,
    pub owner_type: OwnerType,
    /// 外部稳定 ID；禁止仅凭该字段访问 AI-PM 对象。
    pub owner_id: String,
    pub relation_type: RelationType,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    /// 软删除标记；唯一约束仅作用于未解除的引用。
    pub removed_at: Option<DateTime<Utc>>,
}
