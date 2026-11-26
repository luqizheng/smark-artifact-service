//! 无 artifact_key 的 Artifact 创建端口（WP-04 G2-1/G2-2 修复）。
//!
//! 与 keyed 路径（[`super::keyed_create`]）同理：幂等预留、Artifact 插入与
//! 幂等完成必须在**单个事务**内原子提交——修复旧实现 reserve→insert→complete
//! 跨三个事务的崩溃窗口（已提交 Artifact + 永久 pending 记录）与错误快照
//! 重放 500 两个缺口。复用既有 `idempotency_record` 表（无新表、无迁移），
//! `endpoint`/`request_hash` 列语义与 AS-1 保持一致（原始 body sha256）。

use artifact_domain::{Artifact, DomainError};
use async_trait::async_trait;
use uuid::Uuid;

use super::keyed_create::IdempotencyInput;

/// 无键创建的已验证输入（键为空由服务层判定，不进入本结构）。
#[derive(Debug, Clone)]
pub struct UnkeyedArtifactCreate {
    pub tenant_id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub artifact_type: String,
    pub category: Option<String>,
    pub created_by: String,
}

/// 无键创建结果。
#[derive(Debug)]
pub enum UnkeyedArtifactCreateOutcome {
    /// 首次创建（幂等记录同事务写入并完成）。
    Created(Artifact),
    /// 同幂等键同指纹的重放：零写入返回原 Artifact。
    Replayed(Artifact),
}

#[async_trait]
pub trait UnkeyedArtifactCreationPort: Send + Sync {
    /// 单事务创建/重放解析。幂等输入必填（无键无头的请求不走本端口）。
    async fn create_or_replay_unkeyed(
        &self,
        request: UnkeyedArtifactCreate,
        idempotency: IdempotencyInput,
    ) -> Result<UnkeyedArtifactCreateOutcome, DomainError>;
}
