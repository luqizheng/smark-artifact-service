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
