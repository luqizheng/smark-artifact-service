//! 专用 keyed-create 端口（ART-04E §5.1 / §5.2）。
//!
//! 这个端口的实现必须拥有**完整**的数据库事务：作用域校验、Idempotency-Key
//! 解析、Artifact 插入、持久化创建身份、幂等完成快照与一次提交都在同一个
//! PostgreSQL 事务里完成。Application Service 不得在端口外再包一层事务，
//! 端口外的 Repository/IdempotencyStore 各自使用连接池提交。
//!
//! 端口契约只暴露 `sqlx::Transaction` 之外的类型：不允许把 SQLx 事务或
//! 行类型泄露到无关的 Repository trait。

use artifact_domain::{Artifact, ArtifactType, DomainError, TenantScope};
use async_trait::async_trait;
use uuid::Uuid;

/// 已通过校验并完成 Unicode NFC 规范化的创建请求。
///
/// 指纹必须基于本类型的字段计算（即「验证和规范化后的 DTO」），
/// 而不是原始 HTTP 请求体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCreateArtifactRequest {
    pub tenant_id: Uuid,
    pub project_id: Uuid,
    pub artifact_key: String,
    pub name: String,
    pub description: Option<String>,
    pub artifact_type: String,
    pub category: Option<String>,
    pub created_by: String,
}

impl ValidatedCreateArtifactRequest {
    /// 校验并规范化。任何失败都不会进入持久化路径。
    pub fn try_new(
        scope: &TenantScope,
        artifact_key: String,
        name: String,
        description: Option<String>,
        artifact_type: String,
        category: Option<String>,
        created_by: String,
    ) -> Result<Self, DomainError> {
        use crate::service::creation_fingerprint::{normalize_optional_text, normalize_text};

        let artifact_key = normalize_text(&artifact_key);
        let name = normalize_text(&name);
        let description = normalize_optional_text(description.as_deref());
        let artifact_type = normalize_text(&artifact_type);
        let category = normalize_optional_text(category.as_deref());

        validate_artifact_key(&artifact_key)?;
        if name.trim().is_empty() {
            return Err(DomainError::InvalidRequest("name must not be blank".into()));
        }
        ArtifactType::parse(&artifact_type).ok_or_else(|| {
            DomainError::InvalidRequest(format!("unknown artifact_type '{artifact_type}'"))
        })?;
        if created_by.trim().is_empty() {
            return Err(DomainError::Internal(
                "create actor subject must not be blank".into(),
            ));
        }

        Ok(Self {
            tenant_id: scope.tenant_id,
            project_id: scope.project_id,
            artifact_key,
            name,
            description,
            artifact_type,
            category,
            created_by,
        })
    }

    pub fn scope(&self) -> TenantScope {
        TenantScope::new(self.tenant_id, self.project_id)
    }
}

/// `artifact_key` 的表层规则（ART-04E §4.2）。长度以字符计，与数据库
/// `length()` 语义一致；不允许静默 trim。
fn validate_artifact_key(key: &str) -> Result<(), DomainError> {
    if key.is_empty() {
        return Err(DomainError::InvalidRequest(
            "artifact_key must not be empty".into(),
        ));
    }
    if key.chars().count() > 200 {
        return Err(DomainError::InvalidRequest(
            "artifact_key must be at most 200 characters".into(),
        ));
    }
    if key.trim() != key {
        return Err(DomainError::InvalidRequest(
            "artifact_key must not have leading or trailing whitespace".into(),
        ));
    }
    if key.chars().any(char::is_control) {
        return Err(DomainError::InvalidRequest(
            "artifact_key must not contain control characters".into(),
        ));
    }
    Ok(())
}

/// 传输层幂等输入。`request_hash` 是原始 HTTP Body Hash，**不是**
/// Artifact 语义创建指纹。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyInput {
    pub endpoint: String,
    pub key: String,
    pub request_hash: String,
}

/// 端口的成功结果（ART-04E §5.3）。
#[derive(Debug, Clone)]
pub enum KeyedArtifactCreateOutcome {
    /// 本次事务创建了新的 Artifact 与创建身份。
    Created(Artifact),
    /// artifact_key 已存在且指纹一致，解析到原 Artifact。
    ResolvedExisting(Artifact),
    /// 完成的 Idempotency-Key 重放；Artifact 与身份均已验证。
    ReplayedByIdempotency(Artifact),
}

/// 端口的失败结果。只有 `RetryablePersistenceFailure` 建议客户端重试；
/// 其余都是确定性业务结果。
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum KeyedArtifactCreateError {
    #[error("idempotency key was reused with a different request body")]
    IdempotencyConflict,
    #[error("artifact_key already exists with a different creation fingerprint")]
    ArtifactKeyFingerprintConflict,
    #[error("artifact_key exists without a verifiable creation identity")]
    LegacyUnverifiable,
    #[error("retryable persistence failure: {0}")]
    RetryablePersistenceFailure(String),
    /// 完成记录与 Artifact/身份状态不一致，或身份行损坏。
    /// 必须失败关闭并上报完整性信号，绝不盲目重放。
    #[error("persistence integrity failure: {0}")]
    IntegrityFailure(String),
}

impl KeyedArtifactCreateError {
    pub fn to_domain_error(&self) -> DomainError {
        match self {
            Self::IdempotencyConflict => DomainError::IdempotencyKeyReused(
                "this Idempotency-Key was already used with a different request body".into(),
            ),
            Self::ArtifactKeyFingerprintConflict => DomainError::ArtifactKeyConflict(
                "artifact_key already exists with a different creation payload".into(),
            ),
            Self::LegacyUnverifiable => DomainError::ArtifactKeyLegacyUnverifiable(
                "artifact_key exists but cannot be verified; manual reconciliation required".into(),
            ),
            Self::RetryablePersistenceFailure(message) => {
                DomainError::PersistenceUnavailable(message.clone())
            }
            Self::IntegrityFailure(message) => DomainError::Internal(message.clone()),
        }
    }
}

/// keyed create/resolve 的唯一入口。
#[async_trait]
pub trait KeyedArtifactCreationPort: Send + Sync {
    /// 在**一个** PostgreSQL 事务内完成：idempotency 冲突解析 → 插入或解析
    /// Artifact → 插入持久化创建身份 → 写入幂等完成快照 → 提交。
    ///
    /// 并发唯一约束冲突时必须回滚失败事务，再以新事务重新解析竞争胜者。
    async fn create_or_resolve_keyed(
        &self,
        request: ValidatedCreateArtifactRequest,
        idempotency: Option<IdempotencyInput>,
    ) -> Result<KeyedArtifactCreateOutcome, KeyedArtifactCreateError>;
}
