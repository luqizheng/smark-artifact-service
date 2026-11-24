//! 动作权限集合（ART-01 §5.3）：细粒度动作，不设单一 admin。

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    ArtifactRead,
    ArtifactCreate,
    ArtifactUpdateMetadata,
    ArtifactArchive,
    ArtifactDelete,
    VersionCreate,
    VersionRead,
    VersionCommit,
    VersionSetCurrent,
    ReferenceWrite,
    ReferenceRemove,
    GenerationRequest,
    GenerationRead,
    GenerationCancel,
    OutputDownload,
}

impl Action {
    /// 委托凭证 `actions` claim 中的稳定字符串形式。
    pub fn as_str(&self) -> &'static str {
        match self {
            Action::ArtifactRead => "artifact:read",
            Action::ArtifactCreate => "artifact:create",
            Action::ArtifactUpdateMetadata => "artifact:update_metadata",
            Action::ArtifactArchive => "artifact:archive",
            Action::ArtifactDelete => "artifact:delete",
            Action::VersionCreate => "version:create",
            Action::VersionRead => "version:read",
            Action::VersionCommit => "version:commit",
            Action::VersionSetCurrent => "version:set_current",
            Action::ReferenceWrite => "reference:write",
            Action::ReferenceRemove => "reference:remove",
            Action::GenerationRequest => "generation:request",
            Action::GenerationRead => "generation:read",
            Action::GenerationCancel => "generation:cancel",
            Action::OutputDownload => "output:download",
        }
    }

    pub fn parse(s: &str) -> Option<Action> {
        Some(match s {
            "artifact:read" => Action::ArtifactRead,
            "artifact:create" => Action::ArtifactCreate,
            "artifact:update_metadata" => Action::ArtifactUpdateMetadata,
            "artifact:archive" => Action::ArtifactArchive,
            "artifact:delete" => Action::ArtifactDelete,
            "version:create" => Action::VersionCreate,
            "version:read" => Action::VersionRead,
            "version:commit" => Action::VersionCommit,
            "version:set_current" => Action::VersionSetCurrent,
            "reference:write" => Action::ReferenceWrite,
            "reference:remove" => Action::ReferenceRemove,
            "generation:request" => Action::GenerationRequest,
            "generation:read" => Action::GenerationRead,
            "generation:cancel" => Action::GenerationCancel,
            "output:download" => Action::OutputDownload,
            _ => return None,
        })
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 委托凭证（验证通过后的领域表示）。
///
/// claims 结构对齐 ART-01 §5.2：aud 固定为 Artifact Service，tenant/project/action
/// 全部来自凭证而非请求体。签名/JWKS 验证由 [`crate`] 之外的
/// `DelegationVerifier`（application ports）实现方负责。
#[derive(Debug, Clone)]
pub struct Delegation {
    pub delegation_id: String,
    pub jti: String,
    pub scope: crate::scope::TenantScope,
    pub actor: crate::scope::Actor,
    pub actions: std::collections::HashSet<Action>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

impl Delegation {
    pub fn has_action(&self, action: Action) -> bool {
        self.actions.contains(&action)
    }

    /// 校验动作与作用域；失败时返回稳定错误码文本（SCOPE_DENIED）。
    pub fn require(
        &self,
        action: Action,
        scope: &crate::scope::TenantScope,
    ) -> Result<(), crate::error::DomainError> {
        if !self.scope.matches(scope.tenant_id, scope.project_id) {
            return Err(crate::error::DomainError::ScopeDenied(format!(
                "delegation scope does not cover project {}",
                scope.project_id
            )));
        }
        if !self.has_action(action) {
            return Err(crate::error::DomainError::ScopeDenied(format!(
                "delegation is missing action {}",
                action
            )));
        }
        Ok(())
    }
}
