//! PostgreSQL 委托 Token 单次绑定存储（WP 契约 v0.2 §3.4）。
//!
//! 原子性来自 `artifact.delegation_token_use.jti` 上的唯一约束：
//! `INSERT ... ON CONFLICT (jti) DO NOTHING RETURNING id` 决定首用/重放；
//! 冲突时在同一事务内 `SELECT ... FOR UPDATE` 重读胜者并比对
//! (tenant_id, project_id, caller_request_id, request_hash)——一致 = 合法重试，
//! 不一致 = 跨请求复用 jti，失败关闭（`DELEGATION_TOKEN_REUSED`，409）。
//! 数据库故障一律折算为 `PERSISTENCE_UNAVAILABLE`（可重试、绝不放行）。

use crate::postgres::persistence_unavailable;
use artifact_application::{DelegationBindingOutcome, DelegationBindingStore, DelegationTokenUse};
use artifact_domain::DomainError;
use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Clone)]
pub struct PgDelegationBindingStore {
    pub pool: PgPool,
}

impl PgDelegationBindingStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(sqlx::FromRow)]
struct BindingRow {
    tenant_id: Uuid,
    project_id: Uuid,
    caller_request_id: Option<Uuid>,
    request_hash: Option<String>,
}

#[async_trait]
impl DelegationBindingStore for PgDelegationBindingStore {
    async fn consume(
        &self,
        use_of_token: DelegationTokenUse,
    ) -> Result<DelegationBindingOutcome, DomainError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| persistence_unavailable("delegation binding begin", e))?;

        let inserted: Option<(Uuid,)> = sqlx::query_as(
            "INSERT INTO artifact.delegation_token_use \
             (issuer, jti, tenant_id, project_id, caller_request_id, request_hash) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (jti) DO NOTHING \
             RETURNING id",
        )
        .bind(&use_of_token.issuer)
        .bind(&use_of_token.jti)
        .bind(use_of_token.tenant_id)
        .bind(use_of_token.project_id)
        .bind(use_of_token.caller_request_id)
        .bind(use_of_token.request_hash.as_deref())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| persistence_unavailable("delegation binding insert", e))?;

        if inserted.is_some() {
            tx.commit()
                .await
                .map_err(|e| persistence_unavailable("delegation binding commit", e))?;
            return Ok(DelegationBindingOutcome::Fresh);
        }

        // jti 已存在：重读胜者（FOR UPDATE 防与清理并发竞争），逐项比对。
        let existing: Option<BindingRow> = sqlx::query_as(
            "SELECT tenant_id, project_id, caller_request_id, request_hash \
             FROM artifact.delegation_token_use WHERE jti = $1 FOR UPDATE",
        )
        .bind(&use_of_token.jti)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| persistence_unavailable("delegation binding reread", e))?;

        let Some(winner) = existing else {
            // 绑定行在本事务读取前被清理：放弃本次判定，调用方整体重试
            // （fail-closed：不猜测该 jti 是否曾被使用）。
            tx.rollback().await.ok();
            return Err(DomainError::PersistenceUnavailable(
                "delegation binding row vanished during consume; retry the request".into(),
            ));
        };

        let same = winner.tenant_id == use_of_token.tenant_id
            && winner.project_id == use_of_token.project_id
            && winner.caller_request_id == use_of_token.caller_request_id
            && winner.request_hash == use_of_token.request_hash;
        if same {
            tx.commit()
                .await
                .map_err(|e| persistence_unavailable("delegation binding commit", e))?;
            return Ok(DelegationBindingOutcome::RetryAllowed);
        }

        tx.rollback().await.ok();
        Err(DomainError::DelegationTokenReused(format!(
            "jti is already bound to a different operation (expected fingerprint {}, caller {:?} under scope ({}, {}))",
            use_of_token.request_hash.as_deref().unwrap_or("<none>"),
            use_of_token.caller_request_id,
            use_of_token.tenant_id,
            use_of_token.project_id,
        )))
    }
}
