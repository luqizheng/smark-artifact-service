//! Worker 协调端口（ART-03A A3A-04/A3A-05）。
//!
//! 只暴露受控数据库函数语义；实现方必须以 Worker 专用 DB 身份调用，
//! 不依赖 API 路由、不读取用户 delegation、不持有对象存储凭据。
//!
//! 约定：属于正常控制流的结果一律用返回值表达（stale / cancel_requested）；
//! 只有参数错误或数据库故障才是 `Err`。

use artifact_domain::{ClaimedJob, DomainError};
use async_trait::async_trait;
use std::collections::BTreeMap;
use uuid::Uuid;

/// Worker 申报的能力：generator_id → 该 Worker 实现的 adapter_version。
///
/// Job 在创建时固定了 adapter 版本；claim 只会把 Job 交给申报了同一版本的 Worker，
/// 空集合表示本 Worker 不能处理任何 Job。
pub type AdapterCapabilities = BTreeMap<String, String>;

/// 失败上报的结果（与数据库受控函数返回一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    RetryWait,
    Failed,
    TimedOut,
    StaleAttempt,
    NotFound,
    /// Job 已登记取消请求：调用方应停止执行并调用 [`WorkerCoordinationPort::confirm_cancelled`]。
    CancelRequested,
}

impl FailOutcome {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "retry_wait" => FailOutcome::RetryWait,
            "failed" => FailOutcome::Failed,
            "timed_out" => FailOutcome::TimedOut,
            "stale_attempt" => FailOutcome::StaleAttempt,
            "not_found" => FailOutcome::NotFound,
            "cancel_requested" => FailOutcome::CancelRequested,
            _ => return None,
        })
    }
}

/// 续租的结果（与数据库受控函数返回一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatOutcome {
    Renewed,
    /// 已续租，但调用方必须停止执行并确认取消。
    CancelRequested,
    /// 已超过本 Attempt 的最大执行时长：没有续租，Attempt 已按超时收尾。
    DeadlineExceeded,
    /// 租约已不属于调用方（过期、旧 Attempt、错误 token、Job 不在 running）。
    Stale,
}

impl HeartbeatOutcome {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "renewed" => HeartbeatOutcome::Renewed,
            "cancel_requested" => HeartbeatOutcome::CancelRequested,
            "deadline_exceeded" => HeartbeatOutcome::DeadlineExceeded,
            "stale" => HeartbeatOutcome::Stale,
            _ => return None,
        })
    }
}

/// 完成上报的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteOutcome {
    Succeeded,
    CancelRequested,
    Stale,
}

impl CompleteOutcome {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "succeeded" => CompleteOutcome::Succeeded,
            "cancel_requested" => CompleteOutcome::CancelRequested,
            "stale" => CompleteOutcome::Stale,
            _ => return None,
        })
    }
}

/// 确认取消的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmCancelOutcome {
    Cancelled,
    /// 没有登记中的取消请求：调用方不应调用本函数。
    NotRequested,
    Stale,
}

impl ConfirmCancelOutcome {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "cancelled" => ConfirmCancelOutcome::Cancelled,
            "not_requested" => ConfirmCancelOutcome::NotRequested,
            "stale" => ConfirmCancelOutcome::Stale,
            _ => return None,
        })
    }
}

/// Worker 上传产物后的对象面事实（WP-AS-06）。`sha256`/`size_bytes`/`mime`
/// 来自 [`crate::StoragePort::inspect_object`]（file-service 无 hash
/// attestation —— AS 下载重算，即"上传+校验"的校验半程）；`manifest` 由
/// Worker 构建（含 sanitizer 策略版本等执行事实）。
#[derive(Debug, Clone)]
pub struct GenerationOutputFacts {
    pub object_id: String,
    pub sha256: String,
    pub size_bytes: i64,
    pub mime_type: String,
    pub format: String,
    pub manifest: serde_json::Value,
}

/// 输出提交的结果（WP-AS-06 §8 单一受控路径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutputOutcome {
    /// 已提交；携带目标 Version id（稳定精确结果引用，ART-04 §211）。
    Committed(Uuid),
    /// 租约失效 / Job 非 running：提交放弃（不重试，按收尾语义处理）。
    Stale,
    /// 取消请求已登记：调用方应停止执行并 [`WorkerCoordinationPort::confirm_cancelled`]。
    CancelRequested,
}

#[async_trait]
pub trait WorkerCoordinationPort: Send + Sync {
    /// 原子领取一个 Job；返回一次性 lease token 与只读输入快照。
    /// 只会领到与 `capabilities` 中 adapter 版本一致的 Job；没有可领取的 Job 时返回 None。
    async fn claim(
        &self,
        worker_id: &str,
        lease_seconds: i32,
        capabilities: &AdapterCapabilities,
    ) -> Result<Option<ClaimedJob>, DomainError>;

    /// 三要素（job_id + attempt_id + lease token）全部匹配才续租。
    /// 续租不会超过 Attempt 的 deadline；取消请求已登记时返回 [`HeartbeatOutcome::CancelRequested`]。
    async fn heartbeat(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
        lease_seconds: i32,
    ) -> Result<HeartbeatOutcome, DomainError>;

    /// 完成 Attempt（0002 协调语义：仅标记 Job/Attempt succeeded，**不写
    /// Output/Version**）。生产执行循环的完成路径是 [`Self::commit_output`]
    /// （ART-03A §8 单一受控提交）；本方法保留给协调测试与旧流程。
    async fn complete(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
        output_sha256: &str,
    ) -> Result<CompleteOutcome, DomainError>;

    /// 输出提交（WP-AS-06：ART-03A §8 单一受控提交路径，0006
    /// `commit_generation_output`）。**生产执行循环的完成路径必须是本方法**
    /// （[`Self::complete`] 仅是协调层的成功标记，用于协调测试/旧流程，
    /// 不写 Output/Version）。单事务完成：租约校验 → 目标 Version 物化或
    /// 校验（pending）→ Output 插入（primary，safety passed）→
    /// Version pending→ready → Job/Attempt succeeded → audit。
    async fn commit_output(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
        output: &GenerationOutputFacts,
    ) -> Result<CommitOutputOutcome, DomainError>;

    /// 失败上报。`error_code` 必须是稳定机器码（见 [`crate::is_valid_error_code`]）；
    /// `retryable` 只有落在数据库的可重试白名单内才生效。
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
    ) -> Result<ConfirmCancelOutcome, DomainError>;

    /// 扫描排队超时与租约失效的 Attempt，标记并按上限恢复 Job。
    /// 数据库侧同一时刻只允许一个扫描器推进；其余调用返回 0。
    async fn recover_expired(&self) -> Result<i64, DomainError>;

    /// 清理终态 Job 的过期输入正文，保留 hash/版本元数据。
    async fn purge_expired_input(&self) -> Result<i64, DomainError>;
}
