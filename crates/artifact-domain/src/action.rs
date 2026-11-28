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

    /// 是否写动作（契约 §5.3 Q1 撤销检查清单的 11 个动作；WP-03 PC-O5-1
    /// 以同一集合强制"写路径 delegation 必带 caller_request_id"红线）。
    pub fn is_write(self) -> bool {
        matches!(
            self,
            Action::ArtifactCreate
                | Action::ArtifactUpdateMetadata
                | Action::ArtifactArchive
                | Action::ArtifactDelete
                | Action::VersionCreate
                | Action::VersionCommit
                | Action::VersionSetCurrent
                | Action::ReferenceWrite
                | Action::ReferenceRemove
                | Action::GenerationRequest
                | Action::GenerationCancel
        )
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 委托凭证（验证通过后的领域表示）。
///
/// claims 结构对齐 ART-01 §5.2 与 WP-LMMS-DELEGATION-01 契约 v0.2 §3.1.1：
/// aud 固定为 Artifact Service，tenant/project/action 全部来自凭证而非请求体。
/// `scope.tenant_id` 是 AI-PM 内部租户 UUID（授权作用域唯一来源，P0-01）；
/// `lmms_tid` 仅作来源追溯审计镜像，**不参与任何授权判定**。
/// 操作绑定字段（§3.1.2/V10）：`artifact_id`/`operation_id`/`caller_request_id`
/// 存在时，AS-4 接线的服务层必须校验请求目标一致性；`request_hash` 为本实现
/// 提供的请求指纹扩展（WP-03 修订后并入契约 §3.1.1，算法 v1，见
/// `artifact_application::canonical_request_fingerprint`）。
/// 签名/JWKS 验证由 `DelegationVerifier`（application ports）实现方负责。
#[derive(Debug, Clone)]
pub struct Delegation {
    pub delegation_id: String,
    pub jti: String,
    pub scope: crate::scope::TenantScope,
    pub actor: crate::scope::Actor,
    pub actions: std::collections::HashSet<Action>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// 来源追溯：LMMS 数值租户标识的字符串形式（仅审计，不参与授权）。
    pub lmms_tid: Option<String>,
    /// 操作绑定（V10）：限定单 Artifact 的窄化委托。
    pub artifact_id: Option<uuid::Uuid>,
    /// 操作绑定（V10）：稳定操作标识（如 `generation_job:{uuid}`）。
    pub operation_id: Option<String>,
    /// 操作绑定：AS-2 调用方请求幂等键（Job 创建路径必填，§3.1.2）。
    pub caller_request_id: Option<uuid::Uuid>,
    /// 扩展（O-5）：签发方对规范化请求的 SHA-256 指纹（64 位小写十六进制）。
    pub request_hash: Option<String>,
}

impl Delegation {
    pub fn has_action(&self, action: Action) -> bool {
        self.actions.contains(&action)
    }

    /// 对象面出站路由用的 LMMS 数字 tid（WP-AS-05）：取自 `lmms_tid` 追溯
    /// claim，**仅用于出站服务令牌 mint 与 file-service 自身的租户 scope**，
    /// 绝不参与 AS 授权判定或 scope 装配（本结构体注释的红线）。缺失、
    /// 非法或非正数 → `None`（调用方 fail-closed，绝不回落默认租户）。
    pub fn lmms_tid_i64(&self) -> Option<i64> {
        self.lmms_tid
            .as_deref()
            .map(str::trim)
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|t| *t > 0)
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
