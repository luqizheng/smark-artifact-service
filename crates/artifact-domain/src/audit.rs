//! AuditEvent：追加式审计事件（ART-01 §8.3）。
//!
//! 数据库层通过触发器强制 append-only；运行角色不带 UPDATE/DELETE 权限。

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    Allowed,
    Denied,
    Error,
}

impl AuditOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuditOutcome::Allowed => "allowed",
            AuditOutcome::Denied => "denied",
            AuditOutcome::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub actor_subject: Option<String>,
    pub actor_type: Option<String>,
    /// 稳定动作名，例如 `artifact.create`、`auth.denied`、`version.commit`。
    pub action: String,
    pub object_type: String,
    pub object_id: Option<Uuid>,
    pub outcome: AuditOutcome,
    pub error_code: Option<String>,
    pub detail: Value,
    pub request_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl AuditEvent {
    /// 记录授权拒绝（ART-01 §5.4：拒绝必须记录稳定错误码和审计事件；
    /// 日志不得记录 bearer token 原文——本结构不含任何凭证字段）。
    pub fn denial(
        action: &str,
        error_code: &str,
        request_id: Option<&str>,
        tenant_id: Option<Uuid>,
        project_id: Option<Uuid>,
        detail: Value,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id,
            project_id,
            actor_subject: None,
            actor_type: None,
            action: action.to_string(),
            object_type: "auth".to_string(),
            object_id: None,
            outcome: AuditOutcome::Denied,
            error_code: Some(error_code.to_string()),
            detail,
            request_id: request_id.map(str::to_string),
            created_at: Utc::now(),
        }
    }
}
