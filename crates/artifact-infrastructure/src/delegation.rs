//! 生产委托验证器：**fail-closed**。
//!
//! ART-02 结论（`ART-02-artifact-service-pre-implementation-verification-v1.0.md`）：
//! lmms v1.8.1 token-service 的 resource token endpoint 未挂载认证 middleware，
//! 且 claims 缺少 `aud` / `tid`。在两个 P0 关闭之前，不存在可安全验证的
//! 委托凭证签发方，因此生产实现拒绝一切业务请求，而不是降级为
//! "只验签名不验范围"（ART-01 §5.5 明确禁止）。

use artifact_application::{DelegationVerifier, VerificationError};
use artifact_domain::Delegation;
use async_trait::async_trait;

/// 默认验证器：无条件拒绝。生产配置中不存在任何绕过开关。
#[derive(Debug, Clone, Copy, Default)]
pub struct ClosedDelegationVerifier;

impl ClosedDelegationVerifier {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl DelegationVerifier for ClosedDelegationVerifier {
    async fn verify(&self, _bearer_token: &str) -> Result<Delegation, VerificationError> {
        Err(VerificationError::Unauthenticated(
            "delegation verification is not configured: the trusted delegation issuer \
             is blocked by ART-02 P0 findings (unauthenticated resource-token endpoint, \
             missing aud/tid claims); all business APIs are deny-by-default"
                .to_string(),
        ))
    }
}
