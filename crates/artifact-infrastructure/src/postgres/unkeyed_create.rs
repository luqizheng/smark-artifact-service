//! 无键 Artifact 创建的 PostgreSQL 单事务实现（WP-04 G2-1/G2-2 修复）。
//!
//! 与 [`super::keyed_create`] 同构（ART-04E §5 单事务语义在 unkeyed+header
//! 路径的对等落地）：
//!   * 幂等预留（INSERT ON CONFLICT）→ Artifact 插入 → 幂等完成 → 一次 COMMIT；
//!   * 业务失败 = 整体回滚，**不留下任何幂等痕迹**（修复旧实现"错误快照
//!     重放 → 500"缺口；重试即全新尝试）；
//!   * 唯一约束/序列化/死锁冲突回滚后有界重试（≤3 次 + 退避）；
//!   * 完成记录重放前必须验证 resource_id → Artifact 存在且 scope 一致
//!     （失败关闭；旧写入者遗留的半成品行不复用其响应）。
//!
//! 无新表、无迁移：复用 `artifact.idempotency_record`（0001），`endpoint`
//! 列区分路由族，`request_hash` 维持 AS-1 语义（原始 body sha256）。

use crate::postgres::{ARTIFACT_COLUMNS, ArtifactRow};
use artifact_application::{
    IdempotencyInput, UnkeyedArtifactCreate, UnkeyedArtifactCreateOutcome,
    UnkeyedArtifactCreationPort,
};
use artifact_domain::{Artifact, DomainError};
use async_trait::async_trait;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

const MAX_ATTEMPTS: usize = 3;

/// 单次尝试的错误：Terminal（不重试）或 Retry（基础设施冲突；事务已回滚）。
enum AttemptError {
    Retry(String),
    Terminal(DomainError),
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
            // 触发器拒绝 / CHECK / FK / NOT NULL：不变量拒绝，失败关闭。
            "P0001" | "23514" | "23503" | "23502" => {
                AttemptError::Terminal(DomainError::InvalidRequest(db.message().to_string()))
            }
            _ => AttemptError::Terminal(DomainError::Internal(format!(
                "unkeyed artifact create database error {code}: {}",
                db.message()
            ))),
        };
    }
    // 连接池 / IO / 协议层：可重试的持久化不可用。
    AttemptError::Terminal(DomainError::PersistenceUnavailable(err.to_string()))
}

#[derive(Clone)]
pub struct PgUnkeyedArtifactCreator {
    pub pool: PgPool,
}

#[async_trait]
impl UnkeyedArtifactCreationPort for PgUnkeyedArtifactCreator {
    async fn create_or_replay_unkeyed(
        &self,
        request: UnkeyedArtifactCreate,
        idempotency: IdempotencyInput,
    ) -> Result<UnkeyedArtifactCreateOutcome, DomainError> {
        let mut last_retry: Option<String> = None;
        for attempt in 0..MAX_ATTEMPTS {
            match self.try_once(&request, &idempotency).await {
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
            "unkeyed artifact create exhausted bounded retries"
        );
        Err(DomainError::PersistenceUnavailable(
            "concurrent unkeyed create conflict persisted after bounded retries".into(),
        ))
    }
}

impl PgUnkeyedArtifactCreator {
    async fn try_once(
        &self,
        request: &UnkeyedArtifactCreate,
        idempotency: &IdempotencyInput,
    ) -> Result<UnkeyedArtifactCreateOutcome, AttemptError> {
        let mut tx = self.pool.begin().await?;

        // ① 幂等预留：INSERT ON CONFLICT DO NOTHING。
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
        .bind(&idempotency.endpoint)
        .bind(&idempotency.key)
        .bind(&idempotency.request_hash)
        .fetch_optional(&mut *tx)
        .await?;

        let record_id = match inserted {
            Some((id,)) => id,
            None => {
                // ② 已有记录：行锁读取（防清理竞争），先比指纹再判完成态。
                let existing: Option<(Uuid, String, Option<serde_json::Value>, Option<Uuid>)> =
                    sqlx::query_as(
                        "SELECT id, request_hash, response_snapshot, resource_id \
                         FROM artifact.idempotency_record \
                         WHERE tenant_id = $1 AND project_id = $2 AND endpoint = $3 \
                           AND idempotency_key = $4 \
                         FOR UPDATE",
                    )
                    .bind(request.tenant_id)
                    .bind(request.project_id)
                    .bind(&idempotency.endpoint)
                    .bind(&idempotency.key)
                    .fetch_optional(&mut *tx)
                    .await?;

                let Some((_id, hash, snapshot, resource_id)) = existing else {
                    // 与清理竞争：行已在本事务读取前被删除，回滚重试。
                    return Err(AttemptError::Retry("idempotency row vanished".into()));
                };
                if hash != idempotency.request_hash {
                    tx.rollback().await.ok();
                    return Err(AttemptError::Terminal(DomainError::IdempotencyKeyReused(
                        "this Idempotency-Key was already used with a different request body"
                            .into(),
                    )));
                }
                if snapshot.is_some() {
                    // 完成记录重放：resource_id → Artifact 必须存在且 scope 一致。
                    let Some(artifact_id) = resource_id else {
                        // 旧写入者错误快照收尾（resource_id 为空）：不可验证，
                        // 确定性拒绝（修复旧实现的 500 重放缺口）。
                        tx.rollback().await.ok();
                        return Err(AttemptError::Terminal(DomainError::IdempotencyKeyReused(
                            "a prior attempt with this Idempotency-Key did not complete \
                             successfully; use a new key"
                                .into(),
                        )));
                    };
                    let artifact = load_artifact(&mut tx, request, artifact_id)
                        .await?
                        .ok_or_else(|| {
                            AttemptError::Terminal(DomainError::Internal(
                                "idempotency record references a missing artifact".into(),
                            ))
                        })?;
                    tx.commit().await?;
                    return Ok(UnkeyedArtifactCreateOutcome::Replayed(artifact));
                }
                // 未完成行：新代码同事务提交，不会产生该状态；只可能来自
                // 旧写入者遗留（ART-04E §6 人工对账前提）。绝不复用其响应。
                tx.rollback().await.ok();
                return Err(AttemptError::Terminal(DomainError::IdempotencyKeyReused(
                    "an identical request with this key is still in progress; retry later".into(),
                )));
            }
        };

        // ③ 首次创建：Artifact 插入（业务失败 = 整体回滚，幂等记录一并消失）。
        let insert_sql = format!(
            "INSERT INTO artifact.artifact \
             (id, tenant_id, project_id, artifact_key, name, description, artifact_type, category, created_by) \
             VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8) RETURNING {ARTIFACT_COLUMNS}"
        );
        let row: ArtifactRow = sqlx::query_as(&insert_sql)
            .bind(Uuid::new_v4())
            .bind(request.tenant_id)
            .bind(request.project_id)
            .bind(&request.name)
            .bind(&request.description)
            .bind(&request.artifact_type)
            .bind(&request.category)
            .bind(&request.created_by)
            .fetch_one(&mut *tx)
            .await?;
        let artifact: Artifact = row.try_into().map_err(|_: artifact_domain::DomainError| {
            AttemptError::Terminal(DomainError::Internal(
                "unkeyed artifact create produced an invalid row".into(),
            ))
        })?;

        // ④ 幂等完成（快照 + resource_id），与插入同事务原子提交。
        let snapshot = serde_json::to_value(artifact_application::to_artifact_dto(&artifact))
            .map_err(|e| {
                AttemptError::Terminal(DomainError::Internal(format!("artifact dto snapshot: {e}")))
            })?;
        let completed = sqlx::query(
            "UPDATE artifact.idempotency_record \
             SET response_snapshot = $2, resource_id = $3 \
             WHERE id = $1 AND response_snapshot IS NULL",
        )
        .bind(record_id)
        .bind(snapshot)
        .bind(artifact.id)
        .execute(&mut *tx)
        .await?;
        if completed.rows_affected() != 1 {
            return Err(AttemptError::Terminal(DomainError::Internal(
                "idempotency record was completed concurrently within the same key".into(),
            )));
        }

        tx.commit().await?;
        Ok(UnkeyedArtifactCreateOutcome::Created(artifact))
    }
}

async fn load_artifact(
    tx: &mut Transaction<'_, Postgres>,
    request: &UnkeyedArtifactCreate,
    artifact_id: Uuid,
) -> Result<Option<Artifact>, sqlx::Error> {
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
    Ok(row.and_then(|row| row.try_into().ok()))
}
