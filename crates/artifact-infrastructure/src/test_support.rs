//! 测试替身委托验证器（**仅**在 `test-doubles` feature 下编译）。
//!
//! 用途：`tests/` 集成测试需要一个可控的委托来源来驱动业务路径。
//! 这不是运行时开关：发布二进制（`cargo build --release`，无该 feature）
//! 不包含本模块，`main` 永远装配 [`crate::delegation::ClosedDelegationVerifier`]。

use artifact_application::{DelegationVerifier, VerificationError};
use artifact_domain::{Action, Actor, ActorType, Delegation, TenantScope};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use std::collections::HashMap;

#[derive(Default, Clone)]
pub struct TestDelegationVerifier {
    delegations: HashMap<String, Delegation>,
}

impl TestDelegationVerifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一条测试委托：`token` 即 bearer 凭证原文。
    pub fn allow(
        &mut self,
        token: impl Into<String>,
        tenant_id: uuid::Uuid,
        project_id: uuid::Uuid,
        subject: impl Into<String>,
        actor_type: ActorType,
        actions: &[Action],
    ) -> &mut Self {
        let token = token.into();
        self.delegations.insert(
            token.clone(),
            Delegation {
                delegation_id: format!("test-delegation-{token}"),
                jti: format!("test-jti-{token}"),
                scope: TenantScope::new(tenant_id, project_id),
                actor: Actor::new(subject, actor_type),
                actions: actions.iter().copied().collect(),
                expires_at: Utc::now() + Duration::minutes(5),
                lmms_tid: None,
                artifact_id: None,
                operation_id: None,
                caller_request_id: None,
                request_hash: None,
            },
        );
        self
    }
}

#[async_trait]
impl DelegationVerifier for TestDelegationVerifier {
    async fn verify(&self, bearer_token: &str) -> Result<Delegation, VerificationError> {
        match self.delegations.get(bearer_token) {
            Some(delegation) if delegation.expires_at > Utc::now() => Ok(delegation.clone()),
            Some(_) => Err(VerificationError::Unauthenticated(
                "test delegation token expired".into(),
            )),
            None => Err(VerificationError::Unauthenticated(
                "test delegation verifier: unknown token".into(),
            )),
        }
    }
}
