//! 仓储端口：领域实体在 PostgreSQL 上的持久化接口。
//!
//! 实现方（artifact-infrastructure）必须保证：所有查询显式限定
//! tenant_id/project_id；写路径依赖数据库触发器维持不变量。

use artifact_domain::DomainError;
use artifact_domain::{
    Artifact, ArtifactReference, ArtifactVersion, GenerationJob, GeneratorDescriptor, OwnerType,
    RelationType,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct NewArtifact {
    pub tenant_id: Uuid,
    pub project_id: Uuid,
    pub artifact_key: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub artifact_type: String,
    pub category: Option<String>,
    pub created_by: String,
}

#[derive(Debug, Clone)]
pub struct ArtifactPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub category: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ArtifactListFilter {
    pub limit: i64,
    pub before_created_at: Option<DateTime<Utc>>,
    pub before_id: Option<Uuid>,
    pub artifact_type: Option<String>,
    pub lifecycle_state: Option<String>,
}

#[async_trait]
pub trait ArtifactRepository: Send + Sync {
    async fn insert(&self, new: NewArtifact) -> Result<Artifact, DomainError>;
    async fn get(&self, scope: (Uuid, Uuid), artifact_id: Uuid) -> Result<Artifact, DomainError>;
    async fn list(
        &self,
        scope: (Uuid, Uuid),
        filter: &ArtifactListFilter,
    ) -> Result<Vec<Artifact>, DomainError>;
    /// 乐观并发更新（row_revision 匹配）；0 行更新返回对应冲突/未找到。
    async fn patch(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        expected_revision: i64,
        patch: ArtifactPatch,
    ) -> Result<Artifact, DomainError>;
    /// 切换 current_version_id；数据库触发器保证只指向同 Artifact 的 READY Version。
    async fn set_current_version(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        version_id: Uuid,
    ) -> Result<Artifact, DomainError>;
}

#[async_trait]
pub trait VersionRepository: Send + Sync {
    async fn get(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        version_id: Uuid,
    ) -> Result<ArtifactVersion, DomainError>;
    async fn list(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        limit: i64,
    ) -> Result<Vec<ArtifactVersion>, DomainError>;
}

#[async_trait]
pub trait ReferenceRepository: Send + Sync {
    async fn insert(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        owner_type: OwnerType,
        owner_id: &str,
        relation_type: RelationType,
        created_by: &str,
    ) -> Result<ArtifactReference, DomainError>;
    async fn soft_delete(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        reference_id: Uuid,
    ) -> Result<(), DomainError>;
    async fn list(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        limit: i64,
    ) -> Result<Vec<ArtifactReference>, DomainError>;
}

/// 取消请求的结果（与数据库受控函数返回一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobCancelOutcome {
    /// queued / retry_wait 的 Job 已直接取消。
    Cancelled,
    /// running 的 Job 只登记了取消请求，等待 Worker 确认。
    CancelRequested,
    /// Job 已是终态，本次调用没有改变任何状态。
    Terminal,
}

#[async_trait]
pub trait JobRepository: Send + Sync {
    async fn get(&self, scope: (Uuid, Uuid), job_id: Uuid) -> Result<GenerationJob, DomainError>;
    /// Caller Request Lookup（ART-04G v3 §5.4）：按 (tenant, project, caller_request_id)
    /// 查询 Job。谓词走部分唯一索引 `uq_generation_job_caller_request`；
    /// 未命中与跨 scope 统一返回 `Ok(None)`（HTTP 层转 404，不泄露存在性）。
    async fn get_by_caller_request(
        &self,
        scope: (Uuid, Uuid),
        caller_request_id: Uuid,
    ) -> Result<Option<GenerationJob>, DomainError>;
    /// 取消必须经数据库受控函数完成；租户/项目校验在函数内进行，
    /// scope 不匹配一律按 NotFound 处理，不泄露 Job 是否存在。
    async fn cancel(
        &self,
        scope: (Uuid, Uuid),
        job_id: Uuid,
    ) -> Result<(GenerationJob, JobCancelOutcome), DomainError>;
}

#[async_trait]
pub trait GeneratorRepository: Send + Sync {
    async fn list_all(&self) -> Result<Vec<GeneratorDescriptor>, DomainError>;
    async fn get(&self, generator_id: &str) -> Result<GeneratorDescriptor, DomainError>;
}
