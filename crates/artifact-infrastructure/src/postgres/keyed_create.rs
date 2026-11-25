//! PostgreSQL 单事务 keyed create/resolve（ART-04E §5.1–§5.5）。
//!
//! 关键性质：
//!   * 一个请求 = 一个 PostgreSQL 事务。Idempotency-Key 预留、Artifact 插入、
//!     持久化创建身份、幂等完成快照与一次 COMMIT 全部原子完成。
//!   * 未提交的幂等预留行对其他请求不可见，因此提交后的「未完成行」只可能来自
//!     旧写入者（ART-04E §6.1/§6.2 的人工对账前提），新代码不会制造该状态。
//!   * 唯一约束竞争失败时先回滚整个事务，再以新事务重新读取竞争胜者；
//!     绝不在已失败的 PostgreSQL 事务上继续执行语句。
//!   * 事务仅在本模块内使用；端口 Trait 不暴露 `sqlx::Transaction`。

use crate::postgres::{ARTIFACT_COLUMNS, ArtifactRow};
use artifact_application::{
    CURRENT_FINGERPRINT_VERSION, IdempotencyInput, KeyedArtifactCreateError,
    KeyedArtifactCreateOutcome, KeyedArtifactCreationPort, ValidatedCreateArtifactRequest,
    canonical_fingerprint,
};
use artifact_domain::Artifact;
use async_trait::async_trait;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

const MAX_ATTEMPTS: usize = 3;

#[derive(Clone)]
pub struct PgKeyedArtifactCreator {
    pub pool: PgPool,
}

// ── 内部尝试结果 ────────────────────────────────────────────────────────

enum AttemptError {
    /// 并发竞争导致的确定性可重试冲突；事务已回滚。
    Retry(String),
    /// 终止性结果；事务已回滚。
    Terminal(KeyedArtifactCreateError),
}

impl From<sqlx::Error> for AttemptError {
    fn from(err: sqlx::Error) -> Self {
        classify_sqlx_error(err)
    }
}

fn classify_sqlx_error(err: sqlx::Error) -> AttemptError {
    if let sqlx::Error::Database(db) = &err {
        let code = db.code().map(|c| c.to_string()).unwrap_or_default();
        return match code.as_str() {
            // 唯一约束 / 序列化失败 / 死锁：回滚后重试。
            "23505" | "40001" | "40P01" => AttemptError::Retry(format!("{code} {}", db.message())),
            // 不变量触发器 / CHECK / FK：失败关闭，绝不重试。
            "P0001" | "23514" | "23503" | "23502" => AttemptError::Terminal(
                KeyedArtifactCreateError::IntegrityFailure(format!("{code} {}", db.message())),
            ),
            _ => AttemptError::Terminal(KeyedArtifactCreateError::IntegrityFailure(format!(
                "database error {code}: {}",
                db.message()
            ))),
        };
    }
    // 连接池 / IO / 协议层：可重试的持久化不可用。
    AttemptError::Terminal(KeyedArtifactCreateError::RetryablePersistenceFailure(
        err.to_string(),
    ))
}

#[derive(sqlx::FromRow)]
struct IdentityRow {
    artifact_id: Uuid,
    fingerprint_version: i16,
    request_fingerprint: String,
}

type IdempotencyRow = (Uuid, String, Option<serde_json::Value>, Option<Uuid>);

#[async_trait]
impl KeyedArtifactCreationPort for PgKeyedArtifactCreator {
    async fn create_or_resolve_keyed(
        &self,
        request: ValidatedCreateArtifactRequest,
        idempotency: Option<IdempotencyInput>,
    ) -> Result<KeyedArtifactCreateOutcome, KeyedArtifactCreateError> {
        let mut last_retry: Option<String> = None;
        for attempt in 0..MAX_ATTEMPTS {
            match self.try_once(&request, idempotency.as_ref()).await {
                Ok(outcome) => return Ok(outcome),
                Err(AttemptError::Terminal(err)) => return Err(err),
                Err(AttemptError::Retry(reason)) => {
                    last_retry = Some(reason);
                    // 退避后以全新事务重新解析；不在失败事务上继续。
                    if attempt + 1 < MAX_ATTEMPTS {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            5 * (attempt as u64 + 1),
                        ))
                        .await;
                    }
                }
            }
        }
        tracing::warn!(
            attempts = MAX_ATTEMPTS,
            reason = last_retry.as_deref().unwrap_or("unknown"),
            "keyed artifact create exhausted bounded retries"
        );
        Err(KeyedArtifactCreateError::RetryablePersistenceFailure(
            "concurrent create conflict persisted after bounded retries".into(),
        ))
    }
}

impl PgKeyedArtifactCreator {
    async fn try_once(
        &self,
        request: &ValidatedCreateArtifactRequest,
        idempotency: Option<&IdempotencyInput>,
    ) -> Result<KeyedArtifactCreateOutcome, AttemptError> {
        let mut tx = self.pool.begin().await?;

        // ① Idempotency-Key：预留或读取（锁定行，防止清理并发获胜）。
        let mut record_id: Option<Uuid> = None;
        if let Some(input) = idempotency {
            let inserted: Option<(Uuid,)> = sqlx::query_as(
                "INSERT INTO artifact.idempotency_record \
                 (id, tenant_id, project_id, endpoint, idempotency_key, request_hash) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (tenant_id, project_id, endpoint, idempotency_key) DO NOTHING \
                 RETURNING id",
            )
            .bind(Uuid::new_v4())
            .bind(request.tenant_id)
            .bind(request.project_id)
            .bind(&input.endpoint)
            .bind(&input.key)
            .bind(&input.request_hash)
            .fetch_optional(&mut *tx)
            .await?;

            if let Some((id,)) = inserted {
                record_id = Some(id);
            } else {
                let existing: Option<IdempotencyRow> = sqlx::query_as(
                    "SELECT id, request_hash, response_snapshot, resource_id \
                     FROM artifact.idempotency_record \
                     WHERE tenant_id = $1 AND project_id = $2 AND endpoint = $3 \
                       AND idempotency_key = $4 \
                     FOR UPDATE",
                )
                .bind(request.tenant_id)
                .bind(request.project_id)
                .bind(&input.endpoint)
                .bind(&input.key)
                .fetch_optional(&mut *tx)
                .await?;

                let Some((id, hash, snapshot, resource_id)) = existing else {
                    // 与清理竞争：行已在本事务读取前被删除，重试即可。
                    return Err(AttemptError::Retry("idempotency row vanished".into()));
                };
                if hash != input.request_hash {
                    tx.rollback().await.ok();
                    return Err(AttemptError::Terminal(
                        KeyedArtifactCreateError::IdempotencyConflict,
                    ));
                }
                record_id = Some(id);
                if snapshot.is_some() {
                    // 完成记录重放：必须同时验证 Artifact 与持久化身份。
                    let artifact = verify_replay(&mut tx, request, resource_id).await?;
                    tx.commit().await?;
                    return Ok(KeyedArtifactCreateOutcome::ReplayedByIdempotency(artifact));
                }
                // 未完成行：只可能是旧写入者遗留（新代码同事务提交）。
                // 继续走「解析或接管」路径，绝不复用其响应。
            }
        }

        // ② 持久化身份解析。
        let identity: Option<IdentityRow> = sqlx::query_as(
            "SELECT artifact_id, fingerprint_version, request_fingerprint \
             FROM artifact.artifact_creation_identity \
             WHERE tenant_id = $1 AND project_id = $2 AND artifact_key = $3",
        )
        .bind(request.tenant_id)
        .bind(request.project_id)
        .bind(&request.artifact_key)
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(identity) = identity {
            let stored =
                canonical_fingerprint(identity.fingerprint_version, request).map_err(|e| {
                    AttemptError::Terminal(KeyedArtifactCreateError::IntegrityFailure(
                        e.to_string(),
                    ))
                })?;
            if stored != identity.request_fingerprint {
                tx.rollback().await.ok();
                return Err(AttemptError::Terminal(
                    KeyedArtifactCreateError::ArtifactKeyFingerprintConflict,
                ));
            }

            let artifact = load_artifact(&mut tx, request, identity.artifact_id)
                .await?
                .ok_or_else(|| {
                    AttemptError::Terminal(KeyedArtifactCreateError::IntegrityFailure(
                        "creation identity references a missing artifact".into(),
                    ))
                })?;

            if let Some(record_id) = record_id {
                complete_idempotency(&mut tx, record_id, &artifact).await?;
            }
            tx.commit().await?;
            return Ok(KeyedArtifactCreateOutcome::ResolvedExisting(artifact));
        }

        // ③ 同 key 的旧 Artifact（无身份行）不可自动收养，失败关闭。
        //    必须与身份存在性在同一条语句内判定：READ COMMITTED 下两条独立语句
        //    取到不同快照时，并发赢家可能刚提交 Artifact+身份，导致把同一个
        //    指纹的请求误判为 legacy。NOT EXISTS 保证 Artifact 与身份来自同一快照。
        let legacy: Option<(Uuid,)> = sqlx::query_as(
            "SELECT a.id FROM artifact.artifact a \
             WHERE a.tenant_id = $1 AND a.project_id = $2 AND a.artifact_key = $3 \
               AND NOT EXISTS ( \
                 SELECT 1 FROM artifact.artifact_creation_identity i \
                 WHERE i.tenant_id = a.tenant_id AND i.project_id = a.project_id \
                   AND i.artifact_id = a.id) \
             LIMIT 1",
        )
        .bind(request.tenant_id)
        .bind(request.project_id)
        .bind(&request.artifact_key)
        .fetch_optional(&mut *tx)
        .await?;
        if legacy.is_some() {
            tx.rollback().await.ok();
            return Err(AttemptError::Terminal(
                KeyedArtifactCreateError::LegacyUnverifiable,
            ));
        }

        // ④ 插入 Artifact + 身份行（同事务；唯一约束竞争由外层重试）。
        let artifact_id = Uuid::new_v4();
        let insert_sql = format!(
            "INSERT INTO artifact.artifact \
             (id, tenant_id, project_id, artifact_key, name, description, artifact_type, category, created_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING {ARTIFACT_COLUMNS}"
        );
        let row: ArtifactRow = sqlx::query_as(&insert_sql)
            .bind(artifact_id)
            .bind(request.tenant_id)
            .bind(request.project_id)
            .bind(&request.artifact_key)
            .bind(&request.name)
            .bind(&request.description)
            .bind(&request.artifact_type)
            .bind(&request.category)
            .bind(&request.created_by)
            .fetch_one(&mut *tx)
            .await?;
        let artifact: Artifact = row.try_into().map_err(|e: artifact_domain::DomainError| {
            AttemptError::Terminal(KeyedArtifactCreateError::IntegrityFailure(e.to_string()))
        })?;

        sqlx::query(
            "INSERT INTO artifact.artifact_creation_identity \
             (tenant_id, project_id, artifact_key, artifact_id, fingerprint_version, \
              request_fingerprint, created_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(request.tenant_id)
        .bind(request.project_id)
        .bind(&request.artifact_key)
        .bind(artifact.id)
        .bind(CURRENT_FINGERPRINT_VERSION)
        .bind(artifact_fingerprint(request))
        .bind(&request.created_by)
        .execute(&mut *tx)
        .await?;

        if let Some(record_id) = record_id {
            complete_idempotency(&mut tx, record_id, &artifact).await?;
        }

        tx.commit().await?;
        Ok(KeyedArtifactCreateOutcome::Created(artifact))
    }
}

fn artifact_fingerprint(request: &ValidatedCreateArtifactRequest) -> String {
    canonical_fingerprint(CURRENT_FINGERPRINT_VERSION, request)
        .expect("current fingerprint version is always supported")
}

async fn load_artifact(
    tx: &mut Transaction<'_, Postgres>,
    request: &ValidatedCreateArtifactRequest,
    artifact_id: Uuid,
) -> Result<Option<Artifact>, AttemptError> {
    let sql = format!(
        "SELECT {ARTIFACT_COLUMNS} FROM artifact.artifact \
         WHERE id = $1 AND tenant_id = $2 AND project_id = $3"
    );
    let row: Option<ArtifactRow> = sqlx::query_as(&sql)
        .bind(artifact_id)
        .bind(request.tenant_id)
        .bind(request.project_id)
        .fetch_optional(&mut **tx)
        .await?;
    match row {
        None => Ok(None),
        Some(row) => row
            .try_into()
            .map(Some)
            .map_err(|e: artifact_domain::DomainError| {
                AttemptError::Terminal(KeyedArtifactCreateError::IntegrityFailure(e.to_string()))
            }),
    }
}

/// 完成记录重放的失败关闭校验（ART-04E §5.2 / §9.5 #46）：
/// resource_id → Artifact → 持久化身份，三者必须一致。
async fn verify_replay(
    tx: &mut Transaction<'_, Postgres>,
    request: &ValidatedCreateArtifactRequest,
    resource_id: Option<Uuid>,
) -> Result<Artifact, AttemptError> {
    let Some(artifact_id) = resource_id else {
        // 旧写入者以错误快照收尾（resource_id 为空）：无法验证，人工对账。
        return Err(AttemptError::Terminal(
            KeyedArtifactCreateError::LegacyUnverifiable,
        ));
    };
    let artifact = load_artifact(tx, request, artifact_id)
        .await?
        .ok_or_else(|| {
            AttemptError::Terminal(KeyedArtifactCreateError::IntegrityFailure(
                "idempotency record references a missing artifact".into(),
            ))
        })?;
    if artifact.artifact_key.as_deref() != Some(request.artifact_key.as_str()) {
        return Err(AttemptError::Terminal(
            KeyedArtifactCreateError::IntegrityFailure(
                "idempotency record resolves to an artifact with a different artifact_key".into(),
            ),
        ));
    }

    let identity: Option<(String, i16, String)> = sqlx::query_as(
        "SELECT artifact_key, fingerprint_version, request_fingerprint \
         FROM artifact.artifact_creation_identity \
         WHERE tenant_id = $1 AND project_id = $2 AND artifact_id = $3",
    )
    .bind(request.tenant_id)
    .bind(request.project_id)
    .bind(artifact_id)
    .fetch_optional(&mut **tx)
    .await?;

    let Some((identity_key, version, stored)) = identity else {
        // 迁移前创建、且幂等记录已完成的旧数据：无法验证身份。
        return Err(AttemptError::Terminal(
            KeyedArtifactCreateError::LegacyUnverifiable,
        ));
    };
    if identity_key != request.artifact_key {
        return Err(AttemptError::Terminal(
            KeyedArtifactCreateError::IntegrityFailure(
                "durable identity key does not match the replayed artifact".into(),
            ),
        ));
    }
    let replayed = canonical_fingerprint(version, request).map_err(|e| {
        AttemptError::Terminal(KeyedArtifactCreateError::IntegrityFailure(e.to_string()))
    })?;
    if replayed != stored {
        return Err(AttemptError::Terminal(
            KeyedArtifactCreateError::IntegrityFailure(
                "completed idempotency record does not match the durable creation identity".into(),
            ),
        ));
    }
    Ok(artifact)
}

async fn complete_idempotency(
    tx: &mut Transaction<'_, Postgres>,
    record_id: Uuid,
    artifact: &Artifact,
) -> Result<(), AttemptError> {
    let snapshot = serde_json::to_value(artifact_application::to_artifact_dto(artifact))
        .expect("artifact dto is serializable");
    let result = sqlx::query(
        "UPDATE artifact.idempotency_record \
         SET response_snapshot = $2, resource_id = $3 \
         WHERE id = $1 AND response_snapshot IS NULL",
    )
    .bind(record_id)
    .bind(snapshot)
    .bind(artifact.id)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() != 1 {
        return Err(AttemptError::Terminal(
            KeyedArtifactCreateError::IntegrityFailure(
                "idempotency record was completed concurrently within the same key".into(),
            ),
        ));
    }
    Ok(())
}
