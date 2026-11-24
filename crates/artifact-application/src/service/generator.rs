//! Generator 注册查询服务（ART-01 §7）。
//!
//! ART-02 阶段 Mermaid/SeeCode Adapter 均未实现，注册表如实上报
//! `unavailable`；本服务不伪造任何"可用"能力。

use crate::port::repo::GeneratorRepository;
use artifact_contract::GeneratorResponse;
use artifact_domain::DomainError;
use std::sync::Arc;

#[derive(Clone)]
pub struct GeneratorService {
    pub repo: Arc<dyn GeneratorRepository>,
}

impl GeneratorService {
    pub async fn list(&self) -> Result<Vec<GeneratorResponse>, DomainError> {
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
