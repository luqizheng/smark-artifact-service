//! GenerationJob keyed-create 的 PostgreSQL 实现（ART-04G v3 §5.3 / §9 / §10.1）。
//!
//! 全部创建/解析语义（caller identity 解析、key holder 硬门禁、generator 检查、
//! INSERT、并发仲裁、异常块恢复）都在受控函数
//! `artifact.create_generation_job_with_input`（SECURITY DEFINER）的**单个事务**
//! 内完成；本实现只负责调用它、按稳定消息 token 对错误做 token-first 分类
//! （ART-04G v3 §9.5：先匹配 token，再 SQLSTATE），并对未命中 token 的
//! `23505` / `40001` / `40P01` 做有界重试（≤3 次 + 退避）。
//!
//! 重要分类边界：
//!   * 业务冲突 token（IDEMPOTENCY_KEY_REUSED / CALLER_REQUEST_ID_REUSED / ...）
//!     命中即为 Terminal，绝不重试；
//!   * 完整性 token（IDEMPOTENCY_KEY_CROSS_IDENTITY /
//!     CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH）为 Terminal，失败关闭；
//!   * 未命中 token 的 23505（如主键冲突等未预期唯一索引）归为基础设施冲突，
//!     有界重试后 503（AS2-19）；
//!   * GENERATOR_UNAVAILABLE 是 Terminal 503，不落入 IntegrityFailure（AS2-51）。

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use artifact_application::port::keyed_job_create::{
    KeyedGenerationJobCreatePort, KeyedJobCreateError, KeyedJobCreateOutcome,
    ValidatedCreateGenerationJobRequest,
};
use artifact_application::service::artifact::to_job_dto;
use artifact_application::service::generation_fingerprint::CURRENT_JOB_FINGERPRINT_VERSION;
use artifact_contract::GenerationJobResponse;
use artifact_domain::GenerationJob;

use super::{JOB_COLUMNS, JobRow};

const MAX_ATTEMPTS: usize = 3;

/// 与 `JobOptions::default()` 一致的服务端执行策略（不进入指纹）。
const JOB_MAX_ATTEMPTS: i32 = 3;
const JOB_MAX_EXECUTION_SECONDS: i32 = 300;
const JOB_QUEUE_TTL_SECONDS: i32 = 86_400;

/// 单次尝试的错误：Terminal（不重试）或 Retry（基础设施冲突）。
enum AttemptError {
    Terminal(KeyedJobCreateError),
    Retry(String),
}

impl From<AttemptError> for KeyedJobCreateError {
    fn from(err: AttemptError) -> Self {
        match err {
            AttemptError::Terminal(e) => e,
            AttemptError::Retry(reason) => KeyedJobCreateError::RetryablePersistenceFailure(reason),
        }
    }
}

pub struct PgKeyedGenerationJobCreator {
    pub pool: PgPool,
}

/// ART-04G v3 §9.5 的 token-first 分类：稳定消息 token 必须是 RAISE 的
/// 固定字面量（无格式拼接），且先于 SQLSTATE 分支匹配。
fn classify_db_error(err: &sqlx::Error) -> AttemptError {
    let sqlx::Error::Database(db) = err else {
        // 非 DB 错误（连接断开、TLS 等）：暂时性基础设施故障。
        return AttemptError::Retry(format!("non-database persistence error: {err}"));
    };
    let message = db.message();

    // ① 业务/完整性 token：命中即 Terminal。
    if message.contains("CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH") {
        return AttemptError::Terminal(KeyedJobCreateError::FingerprintVersionMismatch(
            message.to_string(),
        ));
    }
    if message.contains("IDEMPOTENCY_KEY_CROSS_IDENTITY") {
        return AttemptError::Terminal(KeyedJobCreateError::IdempotencyKeyCrossIdentity(
            message.to_string(),
        ));
    }
    if message.contains("CALLER_REQUEST_ID_REUSED") {
        return AttemptError::Terminal(KeyedJobCreateError::CallerRequestIdReused);
    }
    if message.contains("IDEMPOTENCY_KEY_REUSED") {
        return AttemptError::Terminal(KeyedJobCreateError::IdempotencyConflict);
    }
    if message.contains("GENERATOR_UNAVAILABLE") {
        return AttemptError::Terminal(KeyedJobCreateError::GeneratorUnavailable);
    }
    if message.contains("UNKNOWN_GENERATOR") {
        return AttemptError::Terminal(KeyedJobCreateError::UnknownGenerator);
    }
    if message.contains("INVALID_IDEMPOTENCY_KEY") {
        return AttemptError::Terminal(KeyedJobCreateError::InvalidIdempotencyKey(
            message.to_string(),
        ));
    }
    // ② 其余校验类 token：确定性 400。
    const VALIDATION_TOKENS: [&str; 9] = [
        "INVALID_JOB_REQUEST",
        "INVALID_INPUT_PAYLOAD",
        "INVALID_INPUT_SCHEMA_VERSION",
        "INVALID_REQUEST_HASH",
        "INVALID_MAX_ATTEMPTS",
        "INVALID_MAX_EXECUTION_SECONDS",
        "INVALID_QUEUE_TTL",
        "INPUT_TOO_LARGE",
        "INVALID_GENERATOR",
    ];
    if VALIDATION_TOKENS.iter().any(|t| message.contains(t)) {
        return AttemptError::Terminal(KeyedJobCreateError::InvalidRequest(message.to_string()));
    }

    // ③ token 未命中的 SQLSTATE 分类（AS2-19：未知 23505 不归为业务冲突）。
    let code = db.code().map(|c| c.to_string()).unwrap_or_default();
    match code.as_str() {
        "23505" | "40001" | "40P01" => {
            AttemptError::Retry(format!("transient persistence conflict ({code})"))
        }
        _ => AttemptError::Terminal(KeyedJobCreateError::IntegrityFailure(format!(
            "unexpected database error [{code}]: {message}"
        ))),
    }
}

impl PgKeyedGenerationJobCreator {
    /// 一次完整尝试：调用受控函数 → 加载 Job → 按 resolution 构造结果。
    async fn try_once(
        &self,
        request: &ValidatedCreateGenerationJobRequest,
        request_hash: &str,
        idempotency_key: Option<&str>,
    ) -> Result<KeyedJobCreateOutcome, AttemptError> {
        let job_id = Uuid::new_v4();
        // 指纹由 Rust 侧对验证后的 typed DTO 计算（§9.4.1）；版本固定为当前值。
        let fingerprint = request
            .fingerprint(CURRENT_JOB_FINGERPRINT_VERSION)
            .map_err(|e| {
                AttemptError::Terminal(KeyedJobCreateError::IntegrityFailure(e.to_string()))
            })?;
        let (caller_request_id, request_fingerprint, fingerprint_version) =
            match request.caller_request_id {
                Some(caller) => (
                    Some(caller),
                    fingerprint.clone(),
                    Some(CURRENT_JOB_FINGERPRINT_VERSION),
                ),
                None => (None, None, None),
            };

        let (created_job_id, resolution): (Uuid, String) = sqlx::query_as(
            "SELECT * FROM artifact.create_generation_job_with_input(\
             $1, $2, $3, $4, $5, $6::jsonb, $7::jsonb, $8, $9, $10, \
             $11, $12, $13, $14, $15, $16)",
        )
        .bind(job_id)
        .bind(request.tenant_id)
        .bind(request.project_id)
        .bind(request.artifact_id)
        .bind(&request.generator_id)
        .bind(&request.input)
        .bind(&request.options)
        .bind(&request.input_schema_version)
        .bind(request_hash)
        .bind(idempotency_key)
        .bind(caller_request_id)
        .bind(&request_fingerprint)
        .bind(fingerprint_version)
        .bind(JOB_MAX_ATTEMPTS)
        .bind(JOB_MAX_EXECUTION_SECONDS)
        .bind(JOB_QUEUE_TTL_SECONDS)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| classify_db_error(&e))?;

        let job = self
            .load_job(request.tenant_id, request.project_id, created_job_id)
            .await?;
        Ok(match resolution.as_str() {
            "created" => KeyedJobCreateOutcome::Created(job),
            "caller_request_replay" => KeyedJobCreateOutcome::ResolvedByCallerRequest(job),
            "idempotency_replay" => KeyedJobCreateOutcome::ResolvedByIdempotency(job),
            other => {
                return Err(AttemptError::Terminal(
                    KeyedJobCreateError::IntegrityFailure(format!(
                        "unknown resolution returned by create function: {other}"
                    )),
                ));
            }
        })
    }

    async fn load_job(
        &self,
        tenant_id: Uuid,
        project_id: Uuid,
        job_id: Uuid,
    ) -> Result<GenerationJob, AttemptError> {
        let sql = format!(
            "SELECT {JOB_COLUMNS} FROM artifact.generation_job \
             WHERE id = $1 AND tenant_id = $2 AND project_id = $3"
        );
        let row: JobRow = sqlx::query_as(&sql)
            .bind(job_id)
            .bind(tenant_id)
            .bind(project_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| {
                AttemptError::Terminal(KeyedJobCreateError::IntegrityFailure(format!(
                    "created/replayed job is not readable: {e}"
                )))
            })?;
        row.try_into().map_err(|e: artifact_domain::DomainError| {
            AttemptError::Terminal(KeyedJobCreateError::IntegrityFailure(e.to_string()))
        })
    }
}

#[async_trait]
impl KeyedGenerationJobCreatePort for PgKeyedGenerationJobCreator {
    async fn create_or_resolve_keyed_job(
        &self,
        request: ValidatedCreateGenerationJobRequest,
        request_hash: String,
        idempotency_key: Option<String>,
    ) -> Result<KeyedJobCreateOutcome, KeyedJobCreateError> {
        let mut last_retry: Option<String> = None;
        for attempt in 0..MAX_ATTEMPTS {
            match self
                .try_once(&request, &request_hash, idempotency_key.as_deref())
                .await
            {
                Ok(outcome) => return Ok(outcome),
                Err(AttemptError::Terminal(err)) => return Err(err),
                Err(AttemptError::Retry(reason)) => {
                    last_retry = Some(reason);
                    // 退避后以全新事务重新解析；不在失败事务上继续。
                    if attempt + 1 < MAX_ATTEMPTS {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            5_u64 * 2_u64.pow(attempt as u32),
                        ))
                        .await;
                    }
                }
            }
        }
        tracing::warn!(
            attempts = MAX_ATTEMPTS,
            reason = ?last_retry,
            "generation job keyed create exhausted bounded retries"
        );
        Err(KeyedJobCreateError::RetryablePersistenceFailure(
            last_retry.unwrap_or_else(|| "bounded retries exhausted".into()),
        ))
    }
}

/// 供未来 AS-4 HTTP 接线与集成测试使用：Outcome → DTO + resolution 头值。
pub fn outcome_parts(outcome: &KeyedJobCreateOutcome) -> (&GenerationJob, &'static str) {
    match outcome {
        KeyedJobCreateOutcome::Created(job) => (job, "created"),
        KeyedJobCreateOutcome::ResolvedByCallerRequest(job) => (job, "caller_request_replay"),
        KeyedJobCreateOutcome::ResolvedByIdempotency(job) => (job, "idempotency_replay"),
    }
}

/// 便捷函数：Outcome → DTO（供集成测试断言）。
pub fn outcome_job_dto(
    outcome: &KeyedJobCreateOutcome,
) -> (&GenerationJob, &'static str, GenerationJobResponse) {
    let (job, resolution) = outcome_parts(outcome);
    (job, resolution, to_job_dto(job))
}
