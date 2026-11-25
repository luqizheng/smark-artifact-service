//! 委托凭证验证端口（ART-01 §5）。
//!
//! 生产实现必须验证：受信签发方签名、aud（固定为 Artifact Service）、
//! 有效期、tenant/project/actions 范围（§5.2）。当前 lmms v1.8.1 token-service
//! 存在两个 P0（resource token endpoint 无鉴权挂载、claims 缺 aud/tid），
//! 因此生产实现暂为 fail-closed，真实实现待 P0 关闭（ART-02 §安全状态）。

use artifact_domain::{Delegation, DomainError};
use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum VerificationError {
    /// 缺少或无效凭证（HTTP 401 UNAUTHENTICATED）。
    /// 消息只含判定原因（签名/kid/iss/aud/时间/claims），绝不包含 token 本体。
    #[error("unauthenticated: {0}")]
    Unauthenticated(String),
    /// 验证器自身依赖不可用（JWKS 拉取失败且无有效缓存）。
    /// HTTP 503 DELEGATION_VERIFIER_UNAVAILABLE，可重试；这是失败关闭，不是降级放行。
    #[error("delegation verifier unavailable: {0}")]
    VerifierUnavailable(String),
}

/// 验证并解析短时能力委托凭证。实现方不得信任任何请求体字段。
#[async_trait]
pub trait DelegationVerifier: Send + Sync {
    async fn verify(&self, bearer_token: &str) -> Result<Delegation, VerificationError>;
}

/// 将验证错误折算为领域错误（用于审计记录的稳定错误码）。
impl From<VerificationError> for DomainError {
    fn from(err: VerificationError) -> Self {
        match err {
            VerificationError::Unauthenticated(msg) => DomainError::Internal(msg),
            VerificationError::VerifierUnavailable(msg) => DomainError::PersistenceUnavailable(msg),
        }
    }
}
