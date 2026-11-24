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
    #[error("unauthenticated: {0}")]
    Unauthenticated(String),
}

/// 验证并解析短时能力委托凭证。实现方不得信任任何请求体字段。
#[async_trait]
pub trait DelegationVerifier: Send + Sync {
    async fn verify(&self, bearer_token: &str) -> Result<Delegation, VerificationError>;
}

/// 将验证错误折算为领域错误（用于审计记录的稳定错误码）。
impl From<VerificationError> for DomainError {
    fn from(err: VerificationError) -> Self {
        DomainError::Internal(err.to_string())
    }
}
