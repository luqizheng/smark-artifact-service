//! Worker 协调端口（ART-03A A3A-04/A3A-05）。
//!
//! 只暴露受控数据库函数语义；实现方必须以 Worker 专用 DB 身份调用，
//! 不依赖 API 路由、不读取用户 delegation、不持有对象存储凭据。

use artifact_domain::{ClaimedJob, DomainError};
use async_trait::async_trait;
use uuid::Uuid;

/// 失败上报的结果（与数据库受控函数返回一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    RetryWait,
    Failed,
    StaleAttempt,
    NotFound,
}

impl FailOutcome {
    pub fn parse(s: &str) -> Self {
        match s {
            "retry_wait" => FailOutcome::RetryWait,
            "failed" => FailOutcome::Failed,
            "stale_attempt" => FailOutcome::StaleAttempt,
            _ => FailOutcome::NotFound,
        }
    }
}

#[async_trait]
pub trait WorkerCoordinationPort: Send + Sync {
    /// 原子领取一个 Job；返回一次性 lease token 与只读输入快照。无可用 Job 时返回 None。
    async fn claim(
        &self,
        worker_id: &str,
        lease_seconds: i32,
    ) -> Result<Option<ClaimedJob>, DomainError>;

    /// 三要素（job_id + attempt_id + lease token）全部匹配才续租。
    async fn heartbeat(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
        lease_seconds: i32,
    ) -> Result<bool, DomainError>;

    /// 完成 Attempt；lease 失效或已登记取消时返回 false/错误。
    async fn complete(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
        output_sha256: &str,
    ) -> Result<bool, DomainError>;

    async fn fail(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
        error_code: &str,
        diagnostic_summary: Option<&str>,
        retryable: bool,
    ) -> Result<FailOutcome, DomainError>;

    /// Worker 确认已停止后才把 Job/Attempt 置为 cancelled。
    async fn confirm_cancelled(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
    ) -> Result<bool, DomainError>;

    /// 扫描租约过期 Attempt，标记 lease_expired 并按上限恢复 Job。
    async fn recover_expired(&self) -> Result<i64, DomainError>;

    /// 清理终态 Job 的过期输入正文，保留 hash/版本元数据。
    async fn purge_expired_input(&self) -> Result<i64, DomainError>;
}
