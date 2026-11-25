//! 领域层稳定错误。HTTP 映射在 artifact-contract / artifact-api 完成。

#[derive(Debug, Clone, thiserror::Error)]
pub enum DomainError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("scope denied: {0}")]
    ScopeDenied(String),
    #[error("resource not found")]
    NotFound,
    #[error("version conflict: {0}")]
    VersionConflict(String),
    #[error("idempotency key reused: {0}")]
    IdempotencyKeyReused(String),
    /// caller_request_id 已绑定到指纹不同的操作（ART-04G v3 §9.1）。
    /// 409，确定性业务冲突，不重试。
    #[error("caller request id reused: {0}")]
    CallerRequestIdReused(String),
    /// 存储的 fingerprint_version 与传入版本不一致（ART-04G v3 §9.2.2）。
    /// 5xx 完整性错误，失败关闭，绝不自动重试或重放。
    #[error("caller request id fingerprint version mismatch: {0}")]
    FingerprintVersionMismatch(String),
    /// Idempotency-Key 与 caller 身份交叉冲突（ART-04G v3 §9.1 防御分支）。
    /// 5xx 完整性错误，失败关闭；独立语义，不并入通用冲突。
    #[error("idempotency key cross identity conflict: {0}")]
    IdempotencyKeyCrossIdentity(String),
    /// Idempotency-Key 规范化后非法（空白语义或超长，ART-04G v3 §10.1）。
    /// 400，确定性校验失败。
    #[error("invalid idempotency key: {0}")]
    InvalidIdempotencyKey(String),
    #[error("artifact key conflict: {0}")]
    ArtifactKeyConflict(String),
    #[error("artifact key cannot be verified: {0}")]
    ArtifactKeyLegacyUnverifiable(String),
    #[error("unsupported media type: {0}")]
    UnsupportedMediaType(String),
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),
    #[error("storage unavailable: {0}")]
    StorageUnavailable(String),
    #[error("generator unavailable: {0}")]
    GeneratorUnavailable(String),
    #[error("generator validation failed: {0}")]
    GeneratorValidationFailed(String),
    #[error("persistence unavailable: {0}")]
    PersistenceUnavailable(String),
    #[error("internal: {0}")]
    Internal(String),
}

/// ART-01 §8.1 稳定错误码。
impl DomainError {
    pub fn code(&self) -> &'static str {
        match self {
            DomainError::InvalidRequest(_) => "INVALID_REQUEST",
            DomainError::ScopeDenied(_) => "SCOPE_DENIED",
            DomainError::NotFound => "RESOURCE_NOT_FOUND",
            DomainError::VersionConflict(_) => "VERSION_CONFLICT",
            DomainError::IdempotencyKeyReused(_) => "IDEMPOTENCY_KEY_REUSED",
            DomainError::CallerRequestIdReused(_) => "CALLER_REQUEST_ID_REUSED",
            DomainError::FingerprintVersionMismatch(_) => {
                "CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH"
            }
            DomainError::IdempotencyKeyCrossIdentity(_) => "IDEMPOTENCY_KEY_CROSS_IDENTITY",
            DomainError::InvalidIdempotencyKey(_) => "INVALID_IDEMPOTENCY_KEY",
            DomainError::ArtifactKeyConflict(_) => "ARTIFACT_KEY_CONFLICT",
            DomainError::ArtifactKeyLegacyUnverifiable(_) => "ARTIFACT_KEY_LEGACY_UNVERIFIABLE",
            DomainError::UnsupportedMediaType(_) => "UNSUPPORTED_MEDIA_TYPE",
            DomainError::PayloadTooLarge(_) => "PAYLOAD_TOO_LARGE",
            DomainError::StorageUnavailable(_) => "STORAGE_UNAVAILABLE",
            DomainError::GeneratorUnavailable(_) => "GENERATOR_UNAVAILABLE",
            DomainError::GeneratorValidationFailed(_) => "GENERATOR_VALIDATION_FAILED",
            DomainError::PersistenceUnavailable(_) => "PERSISTENCE_UNAVAILABLE",
            DomainError::Internal(_) => "INTERNAL_ERROR",
        }
    }

    /// 客户端是否应当重试（ART-01 §8.2：只对明确 retryable 的错误重试）。
    /// 完整性错误（FingerprintVersionMismatch / IdempotencyKeyCrossIdentity /
    /// CallerRequestIdReused）必须失败关闭，绝不进入可重试集合（ART-04G v3 §9.2.2）。
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            DomainError::StorageUnavailable(_)
                | DomainError::GeneratorUnavailable(_)
                | DomainError::PersistenceUnavailable(_)
                | DomainError::Internal(_)
        )
    }
}
