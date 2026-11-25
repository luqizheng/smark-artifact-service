//! 委托 Token 单次操作绑定端口（WP 契约 v0.2 §3.4）。
//!
//! 每个 delegation token（jti）至多绑定到一个规范化请求指纹；绑定通过数据库
//! 唯一约束原子完成，使并发重放、跨请求复用 jti 在服务端被确定性拒绝，而不
//! 依赖调用方自律。同指纹重试是合法的（交由业务幂等层收敛为单次效果）。
//!
//! 事务边界与故障恢复（契约 v0.2 §3.4.4）：
//! * `consume` 自身是一个独立事务：`INSERT ... ON CONFLICT (jti) DO NOTHING`
//!   预留，冲突时 `SELECT ... FOR UPDATE` 判定同指纹重试或跨请求拒绝。
//! * AS-4 接线后，业务写入方应在同一业务事务内先 consume 再写入（写操作）；
//!   本端口不暴露 `sqlx::Transaction`，由实现方（PostgreSQL）保证原子性。
//! * 进程在 consume 与业务写入之间崩溃：绑定行已存在且指纹相同，重试请求
//!   走 `RetryAllowed` 进入业务幂等路径（caller_request_id / Idempotency-Key），
//!   不会产生第二次业务效果。

use artifact_domain::DomainError;
use async_trait::async_trait;
use uuid::Uuid;

/// 一次委托消费的绑定输入（全部来自已验证的委托与规范化请求，非请求体裸字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationTokenUse {
    pub issuer: String,
    pub jti: String,
    pub tenant_id: Uuid,
    pub project_id: Uuid,
    /// 写操作委托必填（契约 v0.2 §3.1 biz 配对规则）；与 `request_hash` 同空同非空。
    pub caller_request_id: Option<Uuid>,
    /// 规范化请求指纹（64 位小写十六进制 SHA-256）。
    pub request_hash: Option<String>,
}

/// consume 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationBindingOutcome {
    /// 本请求首次绑定该 jti：继续执行业务。
    Fresh,
    /// 同 jti 同指纹的合法重试：不得产生新的业务效果，走幂等重放路径。
    RetryAllowed,
}

#[async_trait]
pub trait DelegationBindingStore: Send + Sync {
    /// 原子绑定（或校验既有绑定）。同一 jti 与不同 (scope, caller_request_id,
    /// request_hash) 的第二次消费返回 [`DomainError::DelegationTokenReused`]。
    async fn consume(
        &self,
        use_of_token: DelegationTokenUse,
    ) -> Result<DelegationBindingOutcome, DomainError>;
}
