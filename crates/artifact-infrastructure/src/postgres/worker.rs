//! PostgreSQL 受控函数实现（ART-03A A3A-04/A3A-05）。
//!
//! 本实现只调用 0002 migration 创建的 SECURITY DEFINER 函数；连接必须使用
//! Worker 专用 DB role（无任何业务表 DML 权限）。不读取用户 delegation。

use artifact_application::{FailOutcome, WorkerCoordinationPort};
use artifact_domain::{ClaimedJob, DomainError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

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
    input_payload: serde_json::Value,
    options_payload: Option<serde_json::Value>,
    input_schema_version: Option<String>,
    input_sha256: Option<String>,
    generator_id: String,
    generator_version: Option<String>,
    adapter_version: Option<String>,
    max_attempts: i32,
}

impl TryFrom<ClaimRow> for ClaimedJob {
    type Error = DomainError;

    fn try_from(row: ClaimRow) -> Result<Self, Self::Error> {
        Ok(Self {
            job_id: row.job_id,
            attempt_id: row.attempt_id,
            attempt_number: row.attempt_number,
            lease_token: row.lease_token,
            lease_expires_at: row.lease_expires_at,
            input_payload: row.input_payload,
            options_payload: row.options_payload,
            input_schema_version: row.input_schema_version,
            input_sha256: row.input_sha256,
            generator_id: row.generator_id,
            generator_version: row.generator_version,
            adapter_version: row.adapter_version,
            max_attempts: row.max_attempts,
        })
    }
}

fn map_db(err: sqlx::Error, context: &str) -> DomainError {
    tracing::warn!(context, error = %err, "worker coordination database error");
    DomainError::Internal(format!("{context}: {err}"))
}

#[async_trait]
impl WorkerCoordinationPort for PgWorkerCoordination {
    async fn claim(
        &self,
        worker_id: &str,
        lease_seconds: i32,
    ) -> Result<Option<ClaimedJob>, DomainError> {
        let row: Option<ClaimRow> = sqlx::query_as(
            "SELECT job_id, attempt_id, attempt_number, lease_token, lease_expires_at, \
             input_payload, options_payload, input_schema_version, input_sha256, \
             generator_id, generator_version, adapter_version, max_attempts \
             FROM artifact.claim_generation_job($1, $2)",
        )
        .bind(worker_id)
        .bind(lease_seconds)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| map_db(e, "claim generation job"))?;
        row.map(ClaimedJob::try_from).transpose()
    }

    async fn heartbeat(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
        lease_seconds: i32,
    ) -> Result<bool, DomainError> {
        sqlx::query_scalar::<_, bool>("SELECT artifact.heartbeat_generation_job($1, $2, $3, $4)")
            .bind(job_id)
            .bind(attempt_id)
            .bind(lease_token)
            .bind(lease_seconds)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| map_db(e, "heartbeat generation job"))
    }

    async fn complete(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
        output_sha256: &str,
    ) -> Result<bool, DomainError> {
        sqlx::query_scalar::<_, bool>(
            "SELECT artifact.complete_generation_job_attempt($1, $2, $3, $4)",
        )
        .bind(job_id)
        .bind(attempt_id)
        .bind(lease_token)
        .bind(output_sha256)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| map_db(e, "complete generation job attempt"))
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
        let outcome: String = sqlx::query_scalar(
            "SELECT artifact.fail_generation_job_attempt($1, $2, $3, $4, $5, $6)",
        )
        .bind(job_id)
        .bind(attempt_id)
        .bind(lease_token)
        .bind(error_code)
        .bind(diagnostic_summary)
        .bind(retryable)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| map_db(e, "fail generation job attempt"))?;
        Ok(FailOutcome::parse(&outcome))
    }

    async fn confirm_cancelled(
        &self,
        job_id: Uuid,
        attempt_id: Uuid,
        lease_token: &str,
    ) -> Result<bool, DomainError> {
        sqlx::query_scalar::<_, bool>(
            "SELECT artifact.confirm_generation_job_cancelled($1, $2, $3)",
        )
        .bind(job_id)
        .bind(attempt_id)
        .bind(lease_token)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| map_db(e, "confirm generation job cancelled"))
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
