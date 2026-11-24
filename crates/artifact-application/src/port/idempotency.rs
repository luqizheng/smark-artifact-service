//! 幂等记录端口（ART-01 §4.1 / §3.1 idempotency_record）。

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

/// reserve 的返回：fresh=true 时调用方继续业务并在完成后调用
/// [`IdempotencyStore::complete`]；否则按重放（replay_response）、
/// 冲突（conflicting_hash → 409 IDEMPOTENCY_KEY_REUSED）或
/// 进行中（两者皆空）处理。
#[derive(Debug, Clone)]
pub struct IdempotencyOutcome {
    pub fresh: bool,
    pub replay_response: Option<Value>,
    pub conflicting_hash: bool,
    pub record_id: Uuid,
}

#[async_trait]
pub trait IdempotencyStore: Send + Sync {
    /// 原子预留：同 (tenant, project, endpoint, key) 唯一。
    async fn reserve(
        &self,
        tenant_id: Uuid,
        project_id: Uuid,
        endpoint: &str,
        idempotency_key: &str,
        request_hash: &str,
    ) -> Result<IdempotencyOutcome, artifact_domain::DomainError>;

    /// 业务事务成功后写入响应快照，供后续同 key 重放。
    async fn complete(
        &self,
        record_id: Uuid,
        response_snapshot: Value,
        resource_id: Option<Uuid>,
    ) -> Result<(), artifact_domain::DomainError>;
}
