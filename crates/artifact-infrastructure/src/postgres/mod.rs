//! PostgreSQL 仓储实现（schema `artifact`）。
//!
//! 不变量由 migration 0001/0002 的约束与触发器强制；本层负责范围限定查询、
//! 乐观并发（row_revision / ETag）与错误码映射。所有查询显式携带
//! tenant_id/project_id，杜绝跨租户读取。

use artifact_application::{
    ArtifactListFilter, ArtifactPatch, ArtifactRepository, AuditSink, GeneratorRepository,
    IdempotencyOutcome, IdempotencyStore, JobCancelOutcome, JobRepository, NewArtifact,
    ReferenceRepository, VersionRepository,
};
use artifact_domain::{
    Artifact, ArtifactReference, ArtifactType, ArtifactVersion, DomainError, GenerationJob,
    GeneratorDescriptor, GeneratorStatus, JobState, LifecycleState, OwnerType, RelationType,
    VersionState,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

pub mod keyed_create;
pub mod worker;

pub use keyed_create::PgKeyedArtifactCreator;
pub use worker::PgWorkerCoordination;

// ── 行类型 ──────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct ArtifactRow {
    id: Uuid,
    tenant_id: Uuid,
    project_id: Uuid,
    artifact_key: Option<String>,
    name: String,
    description: Option<String>,
    artifact_type: String,
    category: Option<String>,
    lifecycle_state: String,
    current_version_id: Option<Uuid>,
    created_by: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    deleted_at: Option<DateTime<Utc>>,
    row_revision: i64,
}

impl TryFrom<ArtifactRow> for Artifact {
    type Error = DomainError;

    fn try_from(row: ArtifactRow) -> Result<Self, DomainError> {
        Ok(Self {
            id: row.id,
            tenant_id: row.tenant_id,
            project_id: row.project_id,
            artifact_key: row.artifact_key,
            name: row.name,
            description: row.description,
            artifact_type: ArtifactType::parse(&row.artifact_type).ok_or_else(|| {
                DomainError::Internal(format!("unknown artifact_type {}", row.artifact_type))
            })?,
            category: row.category,
            lifecycle_state: parse_lifecycle(&row.lifecycle_state)?,
            current_version_id: row.current_version_id,
            created_by: row.created_by,
            created_at: row.created_at,
            updated_at: row.updated_at,
            deleted_at: row.deleted_at,
            row_revision: row.row_revision,
        })
    }
}

fn parse_lifecycle(s: &str) -> Result<LifecycleState, DomainError> {
    Ok(match s {
        "active" => LifecycleState::Active,
        "archived" => LifecycleState::Archived,
        "deleted" => LifecycleState::Deleted,
        other => {
            return Err(DomainError::Internal(format!(
                "unknown lifecycle_state {other}"
            )));
        }
    })
}

#[derive(sqlx::FromRow)]
struct VersionRow {
    id: Uuid,
    tenant_id: Uuid,
    project_id: Uuid,
    artifact_id: Uuid,
    version_number: i64,
    state: String,
    source_format: Option<String>,
    source_object_id: Option<String>,
    source_sha256: Option<String>,
    generator_id: Option<String>,
    generator_version: Option<String>,
    adapter_version: Option<String>,
    manifest: serde_json::Value,
    created_by: String,
    created_at: DateTime<Utc>,
    ready_at: Option<DateTime<Utc>>,
    failure_code: Option<String>,
}

impl TryFrom<VersionRow> for ArtifactVersion {
    type Error = DomainError;

    fn try_from(row: VersionRow) -> Result<Self, DomainError> {
        Ok(Self {
            id: row.id,
            tenant_id: row.tenant_id,
            project_id: row.project_id,
            artifact_id: row.artifact_id,
            version_number: row.version_number,
            state: VersionState::parse(&row.state).ok_or_else(|| {
                DomainError::Internal(format!("unknown version state {}", row.state))
            })?,
            source_format: row.source_format,
            source_object_id: row.source_object_id,
            source_sha256: row.source_sha256.map(|s| s.trim().to_string()),
            generator_id: row.generator_id,
            generator_version: row.generator_version,
            adapter_version: row.adapter_version,
            manifest: row.manifest,
            created_by: row.created_by,
            created_at: row.created_at,
            ready_at: row.ready_at,
            failure_code: row.failure_code,
        })
    }
}

#[derive(sqlx::FromRow)]
struct ReferenceRow {
    id: Uuid,
    tenant_id: Uuid,
    project_id: Uuid,
    artifact_id: Uuid,
    owner_type: String,
    owner_id: String,
    relation_type: String,
    created_by: String,
    created_at: DateTime<Utc>,
}

impl TryFrom<ReferenceRow> for ArtifactReference {
    type Error = DomainError;

    fn try_from(row: ReferenceRow) -> Result<Self, DomainError> {
        Ok(Self {
            id: row.id,
            tenant_id: row.tenant_id,
            project_id: row.project_id,
            artifact_id: row.artifact_id,
            owner_type: OwnerType::parse(&row.owner_type).ok_or_else(|| {
                DomainError::Internal(format!("unknown owner_type {}", row.owner_type))
            })?,
            owner_id: row.owner_id,
            relation_type: RelationType::parse(&row.relation_type).ok_or_else(|| {
                DomainError::Internal(format!("unknown relation_type {}", row.relation_type))
            })?,
            created_by: row.created_by,
            created_at: row.created_at,
            removed_at: None,
        })
    }
}

/// Job 查询列。不含输入正文：输入永不出现在普通 Job 查询里。
const JOB_COLUMNS: &str = "id, tenant_id, project_id, artifact_id, target_version_id, \
     generator_id, generator_version, adapter_version, status, attempt_count, max_attempts, \
     created_at, started_at, heartbeat_at, completed_at, error_code, error_message, trace_id, \
     cancel_requested_at";

#[derive(sqlx::FromRow)]
struct JobRow {
    id: Uuid,
    tenant_id: Uuid,
    project_id: Uuid,
    artifact_id: Uuid,
    target_version_id: Option<Uuid>,
    generator_id: String,
    generator_version: Option<String>,
    adapter_version: Option<String>,
    status: String,
    attempt_count: i32,
    max_attempts: i32,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    heartbeat_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    error_code: Option<String>,
    error_message: Option<String>,
    trace_id: Option<String>,
    cancel_requested_at: Option<DateTime<Utc>>,
}

impl TryFrom<JobRow> for GenerationJob {
    type Error = DomainError;

    fn try_from(row: JobRow) -> Result<Self, DomainError> {
        Ok(Self {
            id: row.id,
            tenant_id: row.tenant_id,
            project_id: row.project_id,
            artifact_id: row.artifact_id,
            target_version_id: row.target_version_id,
            generator_id: row.generator_id,
            generator_version: row.generator_version,
            adapter_version: row.adapter_version,
            status: parse_job_state(&row.status)?,
            idempotency_key: None,
            request_hash: None,
            attempt_count: row.attempt_count,
            max_attempts: row.max_attempts,
            created_at: row.created_at,
            started_at: row.started_at,
            heartbeat_at: row.heartbeat_at,
            completed_at: row.completed_at,
            error_code: row.error_code,
            error_message: row.error_message,
            trace_id: row.trace_id,
            cancel_requested_at: row.cancel_requested_at,
        })
    }
}

fn parse_job_state(s: &str) -> Result<JobState, DomainError> {
    Ok(match s {
        "queued" => JobState::Queued,
        "running" => JobState::Running,
        "retry_wait" => JobState::RetryWait,
        "succeeded" => JobState::Succeeded,
        "failed" => JobState::Failed,
        "cancelled" => JobState::Cancelled,
        "timed_out" => JobState::TimedOut,
        other => return Err(DomainError::Internal(format!("unknown job status {other}"))),
    })
}

#[derive(sqlx::FromRow)]
struct GeneratorRow {
    generator_id: String,
    display_name: String,
    version: String,
    adapter_version: Option<String>,
    supported_input_formats: Vec<String>,
    supported_output_formats: Vec<String>,
    capabilities: serde_json::Value,
    status: String,
    status_reason: Option<String>,
}

impl TryFrom<GeneratorRow> for GeneratorDescriptor {
    type Error = DomainError;

    fn try_from(row: GeneratorRow) -> Result<Self, DomainError> {
        Ok(Self {
            generator_id: row.generator_id,
            display_name: row.display_name,
            version: row.version,
            adapter_version: row.adapter_version,
            supported_input_formats: row.supported_input_formats,
            supported_output_formats: row.supported_output_formats,
            capabilities: row.capabilities,
            status: match row.status.as_str() {
                "available" => GeneratorStatus::Available,
                "unavailable" => GeneratorStatus::Unavailable,
                "disabled" => GeneratorStatus::Disabled,
                other => {
                    return Err(DomainError::Internal(format!(
                        "unknown generator status {other}"
                    )));
                }
            },
            status_reason: row.status_reason,
            registered_at: None,
        })
    }
}

// ── 错误映射 ────────────────────────────────────────────────────────────

fn map_db_error(err: sqlx::Error, context: &str) -> DomainError {
    if let sqlx::Error::Database(db) = &err {
        if db.is_unique_violation() {
            return DomainError::InvalidRequest(format!("{context}: duplicate unique value"));
        }
        // 触发器 RAISE EXCEPTION（P0001）/ 违反约束（23514）→ 不变量拒绝。
        let code: String = db.code().map(|c| c.to_string()).unwrap_or_default();
        if code == "P0001" || code == "23514" {
            return DomainError::VersionConflict(db.message().to_string());
        }
        tracing::warn!(context, pg_code = %code, message = %db.message(), "database error");
        return DomainError::Internal(format!("{context}: {code} {}", db.message()));
    }
    tracing::warn!(context, error = %err, "database error");
    DomainError::Internal(format!("{context}: {err}"))
}

fn is_unique_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db) if db.is_unique_violation())
}

/// 触发器 RAISE EXCEPTION 的消息提取（不变量守卫拒绝）。
fn trigger_message(err: &sqlx::Error) -> Option<String> {
    if let sqlx::Error::Database(db) = err {
        // 23514 = check_violation；自定义 RAISE 通常映射为 P0001/23514/23000。
        return Some(format!(
            "{} {}",
            db.code().as_deref().unwrap_or("db"),
            db.message()
        ));
    }
    None
}

// ── ArtifactRepository ─────────────────────────────────────────────────

const ARTIFACT_COLUMNS: &str = "id, tenant_id, project_id, artifact_key, name, description, \
     artifact_type, category, lifecycle_state, current_version_id, created_by, \
     created_at, updated_at, deleted_at, row_revision";

#[derive(Clone)]
pub struct PgArtifactRepository {
    pub pool: PgPool,
}

#[async_trait]
impl ArtifactRepository for PgArtifactRepository {
    async fn insert(&self, new: NewArtifact) -> Result<Artifact, DomainError> {
        let sql = format!(
            "INSERT INTO artifact.artifact \
             (id, tenant_id, project_id, artifact_key, name, description, artifact_type, category, created_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING {ARTIFACT_COLUMNS}"
        );
        let row: ArtifactRow = sqlx::query_as(&sql)
            .bind(Uuid::new_v4())
            .bind(new.tenant_id)
            .bind(new.project_id)
            .bind(new.artifact_key)
            .bind(new.name)
            .bind(new.description)
            .bind(new.artifact_type)
            .bind(new.category)
            .bind(new.created_by)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| {
                if is_unique_violation(&e) {
                    DomainError::InvalidRequest(
                        "artifact_key already exists in this project".to_string(),
                    )
                } else {
                    map_db_error(e, "insert artifact")
                }
            })?;
        row.try_into()
    }

    async fn get(&self, scope: (Uuid, Uuid), artifact_id: Uuid) -> Result<Artifact, DomainError> {
        let sql = format!(
            "SELECT {ARTIFACT_COLUMNS} FROM artifact.artifact \
             WHERE id = $1 AND tenant_id = $2 AND project_id = $3"
        );
        let row: ArtifactRow = sqlx::query_as(&sql)
            .bind(artifact_id)
            .bind(scope.0)
            .bind(scope.1)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| map_db_error(e, "get artifact"))?
            .ok_or(DomainError::NotFound)?;
        row.try_into()
    }

    async fn list(
        &self,
        scope: (Uuid, Uuid),
        filter: &ArtifactListFilter,
    ) -> Result<Vec<Artifact>, DomainError> {
        let sql = format!(
            "SELECT {ARTIFACT_COLUMNS} FROM artifact.artifact \
             WHERE tenant_id = $1 AND project_id = $2 \
               AND ($4::text IS NULL OR artifact_type = $4) \
               AND ($5::text IS NULL OR lifecycle_state = $5) \
               AND ($6::timestamptz IS NULL OR (created_at, id) < ($6, $7)) \
             ORDER BY created_at DESC, id DESC \
             LIMIT $3"
        );
        let rows: Vec<ArtifactRow> = sqlx::query_as(&sql)
            .bind(scope.0)
            .bind(scope.1)
            .bind(filter.limit)
            .bind(filter.artifact_type.as_deref())
            .bind(filter.lifecycle_state.as_deref())
            .bind(filter.before_created_at)
            .bind(filter.before_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| map_db_error(e, "list artifacts"))?;
        rows.into_iter()
            .map(Artifact::try_from)
            .collect::<Result<Vec<_>, _>>()
    }

    async fn patch(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        expected_revision: i64,
        patch: ArtifactPatch,
    ) -> Result<Artifact, DomainError> {
        let sql = format!(
            "UPDATE artifact.artifact \
             SET name = COALESCE($4, name), \
                 description = COALESCE($5, description), \
                 category = COALESCE($6, category) \
             WHERE id = $1 AND tenant_id = $2 AND project_id = $3 \
               AND row_revision = $7 AND lifecycle_state <> 'deleted' \
             RETURNING {ARTIFACT_COLUMNS}"
        );
        let updated: Option<ArtifactRow> = sqlx::query_as(&sql)
            .bind(artifact_id)
            .bind(scope.0)
            .bind(scope.1)
            .bind(patch.name)
            .bind(patch.description)
            .bind(patch.category)
            .bind(expected_revision)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| map_db_error(e, "patch artifact"))?;
        match updated {
            Some(row) => row.try_into(),
            None => {
                // 区分：不存在 / 已删除 / ETag 不匹配。
                let current = self.get(scope, artifact_id).await?;
                if current.lifecycle_state == LifecycleState::Deleted {
                    Err(DomainError::VersionConflict("artifact is deleted".into()))
                } else {
                    Err(DomainError::VersionConflict(format!(
                        "If-Match revision mismatch (expected r{expected_revision}, current r{})",
                        current.row_revision
                    )))
                }
            }
        }
    }

    async fn set_current_version(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        version_id: Uuid,
    ) -> Result<Artifact, DomainError> {
        let sql = format!(
            "UPDATE artifact.artifact SET current_version_id = $4 \
             WHERE id = $1 AND tenant_id = $2 AND project_id = $3 AND lifecycle_state <> 'deleted' \
             RETURNING {ARTIFACT_COLUMNS}"
        );
        let updated: Option<ArtifactRow> = sqlx::query_as(&sql)
            .bind(artifact_id)
            .bind(scope.0)
            .bind(scope.1)
            .bind(version_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| match trigger_message(&e) {
                Some(msg) => DomainError::VersionConflict(msg),
                None => map_db_error(e, "set current version"),
            })?;
        match updated {
            Some(row) => row.try_into(),
            None => {
                let artifact = self.get(scope, artifact_id).await?;
                if artifact.lifecycle_state == LifecycleState::Deleted {
                    Err(DomainError::VersionConflict("artifact is deleted".into()))
                } else {
                    Err(DomainError::NotFound)
                }
            }
        }
    }
}

// ── VersionRepository ──────────────────────────────────────────────────

const VERSION_COLUMNS: &str = "id, tenant_id, project_id, artifact_id, version_number, state, \
     source_format, source_object_id, source_sha256, generator_id, generator_version, \
     adapter_version, manifest, created_by, created_at, ready_at, failure_code";

#[derive(Clone)]
pub struct PgVersionRepository {
    pub pool: PgPool,
}

#[async_trait]
impl VersionRepository for PgVersionRepository {
    async fn get(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        version_id: Uuid,
    ) -> Result<ArtifactVersion, DomainError> {
        let sql = format!(
            "SELECT {VERSION_COLUMNS} FROM artifact.artifact_version \
             WHERE id = $1 AND artifact_id = $2 AND tenant_id = $3 AND project_id = $4"
        );
        let row: VersionRow = sqlx::query_as(&sql)
            .bind(version_id)
            .bind(artifact_id)
            .bind(scope.0)
            .bind(scope.1)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| map_db_error(e, "get version"))?
            .ok_or(DomainError::NotFound)?;
        row.try_into()
    }

    async fn list(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        limit: i64,
    ) -> Result<Vec<ArtifactVersion>, DomainError> {
        let sql = format!(
            "SELECT {VERSION_COLUMNS} FROM artifact.artifact_version \
             WHERE artifact_id = $1 AND tenant_id = $2 AND project_id = $3 \
             ORDER BY version_number DESC LIMIT $4"
        );
        let rows: Vec<VersionRow> = sqlx::query_as(&sql)
            .bind(artifact_id)
            .bind(scope.0)
            .bind(scope.1)
            .bind(limit.clamp(1, 200))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| map_db_error(e, "list versions"))?;
        rows.into_iter()
            .map(ArtifactVersion::try_from)
            .collect::<Result<Vec<_>, _>>()
    }
}

// ── ReferenceRepository ────────────────────────────────────────────────

#[derive(Clone)]
pub struct PgReferenceRepository {
    pub pool: PgPool,
}

#[async_trait]
impl ReferenceRepository for PgReferenceRepository {
    async fn insert(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        owner_type: OwnerType,
        owner_id: &str,
        relation_type: RelationType,
        created_by: &str,
    ) -> Result<ArtifactReference, DomainError> {
        let sql = "INSERT INTO artifact.artifact_reference \
             (id, tenant_id, project_id, artifact_id, owner_type, owner_id, relation_type, created_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             RETURNING id, tenant_id, project_id, artifact_id, owner_type, owner_id, relation_type, created_by, created_at";
        let row: ReferenceRow = sqlx::query_as(sql)
            .bind(Uuid::new_v4())
            .bind(scope.0)
            .bind(scope.1)
            .bind(artifact_id)
            .bind(owner_type.as_str())
            .bind(owner_id)
            .bind(relation_type.as_str())
            .bind(created_by)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| map_db_error(e, "insert reference"))?;
        row.try_into()
    }

    async fn soft_delete(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        reference_id: Uuid,
    ) -> Result<(), DomainError> {
        let sql = "UPDATE artifact.artifact_reference SET removed_at = now() \
             WHERE id = $1 AND artifact_id = $2 AND tenant_id = $3 AND project_id = $4 \
               AND removed_at IS NULL";
        let result = sqlx::query(sql)
            .bind(reference_id)
            .bind(artifact_id)
            .bind(scope.0)
            .bind(scope.1)
            .execute(&self.pool)
            .await
            .map_err(|e| map_db_error(e, "remove reference"))?;
        if result.rows_affected() == 0 {
            return Err(DomainError::NotFound);
        }
        Ok(())
    }

    async fn list(
        &self,
        scope: (Uuid, Uuid),
        artifact_id: Uuid,
        limit: i64,
    ) -> Result<Vec<ArtifactReference>, DomainError> {
        let sql = "SELECT id, tenant_id, project_id, artifact_id, owner_type, owner_id, \
               relation_type, created_by, created_at \
             FROM artifact.artifact_reference \
             WHERE artifact_id = $1 AND tenant_id = $2 AND project_id = $3 AND removed_at IS NULL \
             ORDER BY created_at DESC, id DESC LIMIT $4";
        let rows: Vec<ReferenceRow> = sqlx::query_as(sql)
            .bind(artifact_id)
            .bind(scope.0)
            .bind(scope.1)
            .bind(limit.clamp(1, 200))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| map_db_error(e, "list references"))?;
        rows.into_iter()
            .map(ArtifactReference::try_from)
            .collect::<Result<Vec<_>, _>>()
    }
}

// ── JobRepository ──────────────────────────────────────────────────────

#[derive(Clone)]
pub struct PgJobRepository {
    pub pool: PgPool,
}

#[async_trait]
impl JobRepository for PgJobRepository {
    async fn get(&self, scope: (Uuid, Uuid), job_id: Uuid) -> Result<GenerationJob, DomainError> {
        let sql = format!(
            "SELECT {JOB_COLUMNS} FROM artifact.generation_job WHERE id = $1 AND tenant_id = $2 AND project_id = $3"
        );
        let row: JobRow = sqlx::query_as(&sql)
            .bind(job_id)
            .bind(scope.0)
            .bind(scope.1)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| map_db_error(e, "get job"))?
            .ok_or(DomainError::NotFound)?;
        row.try_into()
    }

    async fn cancel(
        &self,
        scope: (Uuid, Uuid),
        job_id: Uuid,
    ) -> Result<(GenerationJob, JobCancelOutcome), DomainError> {
        // 取消只走受控函数，避免"直接 UPDATE"与函数两个权威来源。
        // 租户/项目在函数内校验：scope 不匹配返回 not_found，与 Job 不存在无法区分。
        let outcome = sqlx::query_scalar::<_, String>(
            "SELECT artifact.request_generation_job_cancel($1, $2, $3)",
        )
        .bind(scope.0)
        .bind(scope.1)
        .bind(job_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| map_db_error(e, "request generation job cancel"))?;
        let outcome = match outcome.as_str() {
            "cancelled" => JobCancelOutcome::Cancelled,
            "cancel_requested" => JobCancelOutcome::CancelRequested,
            "terminal" => JobCancelOutcome::Terminal,
            "not_found" => return Err(DomainError::NotFound),
            other => {
                tracing::error!(outcome = other, "unexpected job cancel outcome");
                return Err(DomainError::Internal(format!(
                    "request generation job cancel: unexpected outcome '{other}'"
                )));
            }
        };
        let job = self.get(scope, job_id).await?;
        Ok((job, outcome))
    }
}

// ── GeneratorRepository ────────────────────────────────────────────────

#[derive(Clone)]
pub struct PgGeneratorRepository {
    pub pool: PgPool,
}

#[async_trait]
impl GeneratorRepository for PgGeneratorRepository {
    async fn list_all(&self) -> Result<Vec<GeneratorDescriptor>, DomainError> {
        let sql = "SELECT generator_id, display_name, version, adapter_version, \
               supported_input_formats, supported_output_formats, capabilities, status, status_reason \
             FROM artifact.generator_registry ORDER BY generator_id";
        let rows: Vec<GeneratorRow> = sqlx::query_as(sql)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| map_db_error(e, "list generators"))?;
        rows.into_iter()
            .map(GeneratorDescriptor::try_from)
            .collect::<Result<Vec<_>, _>>()
    }

    async fn get(&self, generator_id: &str) -> Result<GeneratorDescriptor, DomainError> {
        let sql = "SELECT generator_id, display_name, version, adapter_version, \
               supported_input_formats, supported_output_formats, capabilities, status, status_reason \
             FROM artifact.generator_registry WHERE generator_id = $1";
        let row: GeneratorRow = sqlx::query_as(sql)
            .bind(generator_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| map_db_error(e, "get generator"))?
            .ok_or(DomainError::NotFound)?;
        row.try_into()
    }
}

// ── IdempotencyStore ───────────────────────────────────────────────────

#[derive(Clone)]
pub struct PgIdempotencyStore {
    pub pool: PgPool,
}

#[async_trait]
impl IdempotencyStore for PgIdempotencyStore {
    async fn reserve(
        &self,
        tenant_id: Uuid,
        project_id: Uuid,
        endpoint: &str,
        idempotency_key: &str,
        request_hash: &str,
    ) -> Result<IdempotencyOutcome, DomainError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| map_db_error(e, "idempotency begin"))?;
        let inserted: Option<(Uuid,)> = sqlx::query_as(
            "INSERT INTO artifact.idempotency_record \
             (id, tenant_id, project_id, endpoint, idempotency_key, request_hash) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (tenant_id, project_id, endpoint, idempotency_key) DO NOTHING \
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(tenant_id)
        .bind(project_id)
        .bind(endpoint)
        .bind(idempotency_key)
        .bind(request_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| map_db_error(e, "idempotency reserve insert"))?;

        let outcome = match inserted {
            Some((record_id,)) => IdempotencyOutcome {
                fresh: true,
                replay_response: None,
                conflicting_hash: false,
                record_id,
            },
            None => {
                let existing: Option<(String, Option<serde_json::Value>, Uuid)> = sqlx::query_as(
                    "SELECT request_hash, response_snapshot, id \
                     FROM artifact.idempotency_record \
                     WHERE tenant_id = $1 AND project_id = $2 AND endpoint = $3 AND idempotency_key = $4",
                )
                .bind(tenant_id)
                .bind(project_id)
                .bind(endpoint)
                .bind(idempotency_key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| map_db_error(e, "idempotency reserve select"))?;
                match existing {
                    Some((hash, snapshot, record_id)) => IdempotencyOutcome {
                        fresh: false,
                        replay_response: snapshot,
                        conflicting_hash: hash != request_hash,
                        record_id,
                    },
                    None => {
                        // 理论不可达：INSERT 冲突但 SELECT 未见（并发已完成清理）。
                        return Err(DomainError::IdempotencyKeyReused(
                            "idempotency record vanished; retry".into(),
                        ));
                    }
                }
            }
        };
        tx.commit()
            .await
            .map_err(|e| map_db_error(e, "idempotency commit"))?;
        Ok(outcome)
    }

    async fn complete(
        &self,
        record_id: Uuid,
        response_snapshot: serde_json::Value,
        resource_id: Option<Uuid>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE artifact.idempotency_record \
             SET response_snapshot = $2, resource_id = $3 \
             WHERE id = $1 AND response_snapshot IS NULL",
        )
        .bind(record_id)
        .bind(response_snapshot)
        .bind(resource_id)
        .execute(&self.pool)
        .await
        .map_err(|e| map_db_error(e, "idempotency complete"))?;
        Ok(())
    }
}

// ── AuditSink ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct PgAuditSink {
    pub pool: PgPool,
}

#[async_trait]
impl AuditSink for PgAuditSink {
    async fn record(&self, event: artifact_domain::AuditEvent) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO artifact.audit_event \
             (id, tenant_id, project_id, actor_subject, actor_type, action, object_type, \
              object_id, outcome, error_code, detail, request_id, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(event.id)
        .bind(event.tenant_id)
        .bind(event.project_id)
        .bind(event.actor_subject)
        .bind(event.actor_type)
        .bind(event.action)
        .bind(event.object_type)
        .bind(event.object_id)
        .bind(event.outcome.as_str())
        .bind(event.error_code)
        .bind(event.detail)
        .bind(event.request_id)
        .bind(event.created_at)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
    }
}

/// 仅测试辅助：取单值。
pub async fn ping(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1").execute(pool).await.map(|_| ())
}
