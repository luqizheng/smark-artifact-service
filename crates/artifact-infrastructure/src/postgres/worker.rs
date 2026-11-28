//! PostgreSQL 受控函数实现（ART-03A A3A-04/A3A-05）。
//!
//! 本实现只调用 0002 / 0003 migration 创建的 SECURITY DEFINER 函数；连接必须使用
//! Worker 专用 DB role（无任何业务表 DML 权限）。不读取用户 delegation。
//!
//! 属于正常控制流的结果（stale / cancel_requested）由数据库以返回值表达；
//! 这里只把参数错误映射为 `InvalidRequest`，把真正的数据库故障映射为 `Internal`。

use artifact_application::{
    AdapterCapabilities, CommitOutputOutcome, CompleteOutcome, ConfirmCancelOutcome, FailOutcome,
    GenerationOutputFacts, HeartbeatOutcome, WorkerCoordinationPort, is_valid_error_code,
    sanitize_diagnostic,
};
use artifact_domain::{ClaimedJob, DomainError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// 数据库对租约秒数的允许范围。
const LEASE_SECONDS_RANGE: std::ops::RangeInclusive<i32> = 1..=3600;

#[derive(Clone)]
pub struct PgWorkerCoordination {
    pub pool: PgPool,
}

#[derive(sqlx::FromRow)]
struct ClaimRow {
    job_id: Uuid,
    attempt_id: Uuid,
    attempt_number: i32,
    lease_token: String,
    lease_expires_at: DateTime<Utc>,
    deadline_at: DateTime<Utc>,
    input_payload: serde_json::Value,
    options_payload: Option<serde_json::Value>,
    input_schema_version: Option<String>,
    input_sha256: Option<String>,
    generator_id: String,
    generator_version: Option<String>,
    adapter_version: Option<String>,
    max_attempts: i32,
}

impl From<ClaimRow> for ClaimedJob {
    fn from(row: ClaimRow) -> Self {
        Self {
            job_id: row.job_id,
            attempt_id: row.attempt_id,
            attempt_number: row.attempt_number,
            lease_token: row.lease_token,
            lease_expires_at: row.lease_expires_at,
            deadline_at: row.deadline_at,
            input_payload: row.input_payload,
            options_payload: row.options_payload,
            input_schema_version: row.input_schema_version,
            input_sha256: row.input_sha256,
            generator_id: row.generator_id,
            generator_version: row.generator_version,
            adapter_version: row.adapter_version,
            max_attempts: row.max_attempts,
        }
    }
}

fn map_db(err: sqlx::Error, context: &str) -> DomainError {
    tracing::warn!(context, error = %err, "worker coordination database error");
    DomainError::Internal(format!("{context}: {err}"))
}

/// commit_generation_output 的错误映射（WP-AS-06）：
/// * `CANCEL_REQUESTED`（P0001）→ [`DomainError::CancelRequested`] 语义由
///   调用方以 message token 识别并转 [`CommitOutputOutcome::CancelRequested`]；
/// * `INVALID_OUTPUT_*`（22023）→ 400 InvalidRequest（Worker 协议错误）；
/// * `TARGET_VERSION_NOT_PENDING`（P0001）→ 409 VersionConflict（DB 状态分叉，
///   fail-closed 不重试）；
/// * 其余 → Internal（数据库故障）。
fn map_commit_output_db(err: sqlx::Error, context: &str) -> DomainError {
    let message = match err.as_database_error() {
        Some(db) => db.message().to_string(),
        None => return map_db(err, context),
    };
    if message.contains("CANCEL_REQUESTED") {
        tracing::warn!(context, "generation output commit rejected: cancel requested");
        return DomainError::InvalidRequest("CANCEL_REQUESTED".into());
    }
    if message.contains("TARGET_VERSION_NOT_PENDING") {
        tracing::warn!(context, "generation output commit rejected: target version not pending");
        return DomainError::VersionConflict(
            "target version is not pending and cannot receive output".into(),
        );
    }
    if message.contains("INVALID_OUTPUT_") {
        tracing::warn!(context, "generation output commit rejected: invalid output facts");
        return DomainError::InvalidRequest(message);
    }
    map_db(err, context)
}

/// 数据库返回了契约之外的值：不是调用方的问题，按内部错误处理并保留原值以便排查。
fn unexpected(context: &str, outcome: String) -> DomainError {
    tracing::error!(context, outcome = %outcome, "unexpected coordination outcome");
    DomainError::Internal(format!("{context}: unexpected outcome '{outcome}'"))
}

fn check_worker_id(worker_id: &str) -> Result<(), DomainError> {
    if worker_id.trim().is_empty() || worker_id.len() > 128 {
        return Err(DomainError::InvalidRequest(
            "worker_id must be 1..=128 characters".into(),
        ));
    }
    Ok(())
}

fn check_lease_seconds(lease_seconds: i32) -> Result<(), DomainError> {
    if !LEASE_SECONDS_RANGE.contains(&lease_seconds) {
        return Err(DomainError::InvalidRequest(format!(
            "lease_seconds must be within {LEASE_SECONDS_RANGE:?}"
        )));
    }
    Ok(())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[async_trait]
impl WorkerCoordinationPort for PgWorkerCoordination {
    async fn claim(
        &self,
        worker_id: &str,
        lease_seconds: i32,
        capabilities: &AdapterCapabilities,
    ) -> Result<Option<ClaimedJob>, DomainError> {
        check_worker_id(worker_id)?;
        check_lease_seconds(lease_seconds)?;
        let capabilities_json = serde_json::Value::Object(
            capabilities
                .iter()
                .map(|(generator, version)| {
                    (
                        generator.clone(),
                        serde_json::Value::String(version.clone()),
                    )
                })
                .collect(),
        );
        let row: Option<ClaimRow> = sqlx::query_as(
            "SELECT job_id, attempt_id, attempt_number, lease_token, lease_expires_at, \
             deadline_at, input_payload, options_payload, input_schema_version, input_sha256, \
             generator_id, generator_version, adapter_version, max_attempts \
             FROM artifact.claim_generation_job($1, $2, $3)",
        )
        .bind(worker_id)
        .bind(lease_seconds)
        .bind(capabilities_json)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| map_db(e, "claim generation job"))?;
        Ok(row.map(ClaimedJob::from))
    }

    async fn heartbeat(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
        lease_seconds: i32,
    ) -> Result<HeartbeatOutcome, DomainError> {
        check_lease_seconds(lease_seconds)?;
        let outcome: String =
            sqlx::query_scalar("SELECT artifact.heartbeat_generation_job($1, $2, $3, $4)")
                .bind(job_id)
                .bind(attempt_id)
                .bind(lease_token)
                .bind(lease_seconds)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| map_db(e, "heartbeat generation job"))?;
        HeartbeatOutcome::parse(&outcome)
            .ok_or_else(|| unexpected("heartbeat generation job", outcome))
    }

    async fn complete(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
        output_sha256: &str,
    ) -> Result<CompleteOutcome, DomainError> {
        if !is_sha256_hex(output_sha256) {
            return Err(DomainError::InvalidRequest(
                "output_sha256 must be 64 lowercase hex characters".into(),
            ));
        }
        let outcome: String =
            sqlx::query_scalar("SELECT artifact.complete_generation_job_attempt($1, $2, $3, $4)")
                .bind(job_id)
                .bind(attempt_id)
                .bind(lease_token)
                .bind(output_sha256)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| map_db(e, "complete generation job attempt"))?;
        CompleteOutcome::parse(&outcome)
            .ok_or_else(|| unexpected("complete generation job attempt", outcome))
    }

    async fn commit_output(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
        output: &GenerationOutputFacts,
    ) -> Result<CommitOutputOutcome, DomainError> {
        if !is_sha256_hex(&output.sha256) {
            return Err(DomainError::InvalidRequest(
                "output sha256 must be 64 lowercase hex characters".into(),
            ));
        }
        if output.object_id.trim().is_empty() {
            return Err(DomainError::InvalidRequest(
                "output object_id must not be blank".into(),
            ));
        }
        if output.size_bytes < 0 {
            return Err(DomainError::InvalidRequest(
                "output size_bytes must be non-negative".into(),
            ));
        }
        let committed: Result<CommitOutputOutcome, DomainError> = sqlx::query_scalar(
            "SELECT artifact.commit_generation_output($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(job_id)
        .bind(attempt_id)
        .bind(lease_token)
        .bind(output.object_id.trim())
        .bind(&output.sha256)
        .bind(output.size_bytes)
        .bind(output.mime_type.trim())
        .bind(output.format.trim())
        .bind(&output.manifest)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| map_commit_output_db(e, "commit generation output"))
        .and_then(|version_id| {
            Ok(match version_id {
                Some(id) => CommitOutputOutcome::Committed(id),
                None => CommitOutputOutcome::Stale,
            })
        });
        // map_commit_output_db 以稳定 token 表达取消：转语义结果。
        match committed {
            Ok(outcome) => Ok(outcome),
            Err(DomainError::InvalidRequest(message)) if message == "CANCEL_REQUESTED" => {
                Ok(CommitOutputOutcome::CancelRequested)
            }
            Err(other) => Err(other),
        }
    }

    async fn fail(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
        error_code: &str,
        diagnostic_summary: Option<&str>,
        retryable: bool,
    ) -> Result<FailOutcome, DomainError> {
        if !is_valid_error_code(error_code) {
            return Err(DomainError::InvalidRequest(
                "error_code must match ^[A-Z][A-Z0-9_]{0,63}$".into(),
            ));
        }
        let diagnostic = sanitize_diagnostic(diagnostic_summary);
        let outcome: String = sqlx::query_scalar(
            "SELECT artifact.fail_generation_job_attempt($1, $2, $3, $4, $5, $6)",
        )
        .bind(job_id)
        .bind(attempt_id)
        .bind(lease_token)
        .bind(error_code)
        .bind(diagnostic.as_deref())
        .bind(retryable)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| map_db(e, "fail generation job attempt"))?;
        FailOutcome::parse(&outcome)
            .ok_or_else(|| unexpected("fail generation job attempt", outcome))
    }

    async fn confirm_cancelled(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
    ) -> Result<ConfirmCancelOutcome, DomainError> {
        let outcome: String =
            sqlx::query_scalar("SELECT artifact.confirm_generation_job_cancelled($1, $2, $3)")
                .bind(job_id)
                .bind(attempt_id)
                .bind(lease_token)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| map_db(e, "confirm generation job cancelled"))?;
        ConfirmCancelOutcome::parse(&outcome)
            .ok_or_else(|| unexpected("confirm generation job cancelled", outcome))
    }

    async fn recover_expired(&self) -> Result<i64, DomainError> {
        sqlx::query_scalar::<_, i32>("SELECT artifact.recover_expired_generation_jobs()")
            .fetch_one(&self.pool)
            .await
            .map(i64::from)
            .map_err(|e| map_db(e, "recover expired generation jobs"))
    }

    async fn purge_expired_input(&self) -> Result<i64, DomainError> {
        sqlx::query_scalar::<_, i32>("SELECT artifact.purge_expired_generation_job_input()")
            .fetch_one(&self.pool)
            .await
            .map(i64::from)
            .map_err(|e| map_db(e, "purge expired generation job input"))
    }
}
