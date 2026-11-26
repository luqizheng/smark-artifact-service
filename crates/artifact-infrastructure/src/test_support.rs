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
use std::sync::{Arc, Mutex};

/// 测试委托的可选操作绑定字段（WP-04 V10 接线测试用）。
#[derive(Debug, Clone, Default)]
pub struct TestTokenBinding {
    pub artifact_id: Option<uuid::Uuid>,
    pub operation_id: Option<String>,
    pub caller_request_id: Option<uuid::Uuid>,
    pub request_hash: Option<String>,
}

/// 内部互斥：允许测试在 `spawn_app` 之后按具体请求动态登记绑定 token
/// （V10 的指纹绑定要求 token 铸造时已知 method/path/body）。
#[derive(Default, Clone)]
pub struct TestDelegationVerifier {
    delegations: Arc<Mutex<HashMap<String, Delegation>>>,
}

impl TestDelegationVerifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一条测试委托：`token` 即 bearer 凭证原文。
    #[allow(clippy::too_many_arguments)]
    pub fn allow(
        &self,
        token: impl Into<String>,
        tenant_id: uuid::Uuid,
        project_id: uuid::Uuid,
        subject: impl Into<String>,
        actor_type: ActorType,
        actions: &[Action],
    ) -> &Self {
        self.allow_with_binding(
            token,
            tenant_id,
            project_id,
            subject,
            actor_type,
            actions,
            TestTokenBinding::default(),
        )
    }

    /// 登记一条携带操作绑定（V10）的测试委托。
    #[allow(clippy::too_many_arguments)]
    pub fn allow_with_binding(
        &self,
        token: impl Into<String>,
        tenant_id: uuid::Uuid,
        project_id: uuid::Uuid,
        subject: impl Into<String>,
        actor_type: ActorType,
        actions: &[Action],
        binding: TestTokenBinding,
    ) -> &Self {
        let token = token.into();
        self.delegations.lock().expect("test verifier lock").insert(
            token.clone(),
            Delegation {
                delegation_id: format!("test-delegation-{token}"),
                jti: format!("test-jti-{token}"),
                scope: TenantScope::new(tenant_id, project_id),
                actor: Actor::new(subject, actor_type),
                actions: actions.iter().copied().collect(),
                expires_at: Utc::now() + Duration::minutes(5),
                lmms_tid: None,
                artifact_id: binding.artifact_id,
                operation_id: binding.operation_id,
                caller_request_id: binding.caller_request_id,
                request_hash: binding.request_hash,
            },
        );
        self
    }
}

#[async_trait]
impl DelegationVerifier for TestDelegationVerifier {
    async fn verify(&self, bearer_token: &str) -> Result<Delegation, VerificationError> {
        let guard = self.delegations.lock().expect("test verifier lock");
        match guard.get(bearer_token) {
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
