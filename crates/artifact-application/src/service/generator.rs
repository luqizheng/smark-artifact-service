//! Generator 注册查询服务（ART-01 §7）。
//!
//! ART-02 阶段 Mermaid/SeeCode Adapter 均未实现，注册表如实上报
//! `unavailable`；本服务不伪造任何"可用"能力。
//!
//! 授权（WP-03 R2-F-03 修复，Q3"所有受保护 API 均要求 delegation"）：
//! 注册表查询要求 `generation:read` 动作。注册表本身不按 project 分片
//! （全局能力注册），故 scope 校验对委托自身的 (tenant, project) 执行；
//! 无有效委托（401）或动作不符（403）一律在读库前拒绝。

use crate::port::repo::GeneratorRepository;
use artifact_contract::GeneratorResponse;
use artifact_domain::{Action, Delegation, DomainError};
use std::sync::Arc;

#[derive(Clone)]
pub struct GeneratorService {
    pub repo: Arc<dyn GeneratorRepository>,
}

impl GeneratorService {
    /// 注册表实况（授权：`generation:read`，读库前强制）。
    pub async fn list_authorized(
        &self,
        delegation: &Delegation,
    ) -> Result<Vec<GeneratorResponse>, DomainError> {
        // 注册表全局共享：以委托自身 scope 判定动作资格（不涉及资源定位）。
        let own_scope = delegation.scope.clone();
        delegation.require(Action::GenerationRead, &own_scope)?;
        let generators = self.repo.list_all().await?;
        Ok(generators
            .iter()
            .map(crate::service::artifact::to_generator_dto)
            .collect())
    }

    pub async fn get(&self, generator_id: &str) -> Result<GeneratorResponse, DomainError> {
        let generator = self.repo.get(generator_id).await?;
        Ok(crate::service::artifact::to_generator_dto(&generator))
    }
}
