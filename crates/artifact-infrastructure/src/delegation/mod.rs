//! 委托验证器实现。
//!
//! * [`ClosedDelegationVerifier`]：默认装配，无条件拒绝（fail-closed）。
//!   受信 issuer 清单为空时生产姿态不变。
//! * [`ai_pm::AiPmDelegationVerifier`]：候选 B 真实验证器（WP 契约 v0.2 §3），
//!   仅在显式配置 `ARTIFACT_API_DELEGATION_ISSUERS` 后才会被装配。
//! * [`jwks`]：JWKS 信任根（固定 URL / kid-strict / 失败关闭）。
//!
//! ART-02 结论（`ART-02-artifact-service-pre-implementation-verification-v1.0.md`）：
//! lmms v1.8.1 token-service 的 resource token endpoint 未挂载认证 middleware，
//! 且 claims 缺少 `aud` / `tid`——候选 B 完全绕开该不可用端点（AI-PM 签发），
//! LMMS 侧两个 P0 仍是后续版本需求，不因本实现而消失。

pub mod ai_pm;
pub mod jwks;

pub use ai_pm::{AiPmDelegationVerifier, TrustedIssuer};

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
