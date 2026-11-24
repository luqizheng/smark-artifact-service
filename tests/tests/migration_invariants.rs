//! 数据库级不变量测试（任务 §六：READY/current pointer 不变量、幂等唯一、
//! 版本号并发分配、audit append-only、Job 状态机、孤儿约束）。
//! 全部通过 SQL 直连，验证 migration 0001 的约束与触发器。

mod common;

use common::pg_error_code;
use sqlx::Row;
use uuid::Uuid;

async fn expect_p0001<T: std::fmt::Debug>(
    future: impl std::future::Future<Output = Result<T, sqlx::Error>>,
) {
    let err = future
        .await
        .expect_err("database must reject this operation");
    assert_eq!(
        pg_error_code(&err).as_deref(),
        Some("P0001"),
        "expected invariant trigger rejection, got {err}"
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn version_numbers_are_serialized_and_dense(pool: sqlx::PgPool) {
    let artifact =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "concurrent").await;
    // 6 个并发插入：父行锁保证串行分配 1..6，无重复无跳号。
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            sqlx::query(
                "INSERT INTO artifact.artifact_version \
                 (id, tenant_id, project_id, artifact_id, state, manifest, created_by) \
                 VALUES ($1, $2, $3, $4, 'pending', '{}', 'fixture')",
            )
            .bind(Uuid::new_v4())
            .bind(common::TENANT)
            .bind(common::PROJECT_1)
            .bind(artifact)
            .execute(&pool)
            .await
            .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let rows = sqlx::query(
        "SELECT version_number FROM artifact.artifact_version WHERE artifact_id = $1 ORDER BY version_number",
    )
    .bind(artifact)
    .fetch_all(&pool)
    .await
    .unwrap();
    let numbers: Vec<i64> = rows.iter().map(|row| row.get::<i64, _>(0)).collect();
    assert_eq!(numbers, vec![1, 2, 3, 4, 5, 6]);
}

#[sqlx::test(migrations = "../migrations")]
async fn current_pointer_requires_ready_version_same_artifact(pool: sqlx::PgPool) {
    let artifact =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "ptr").await;
    let other_artifact =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "other").await;
    let pending = common::sql_insert_version(
        &pool,
        common::TENANT,
        common::PROJECT_1,
        artifact,
        "pending",
    )
    .await;
    let ready =
        common::sql_insert_version(&pool, common::TENANT, common::PROJECT_1, artifact, "ready")
            .await;
    let foreign_ready = common::sql_insert_version(
        &pool,
        common::TENANT,
        common::PROJECT_1,
        other_artifact,
        "ready",
    )
    .await;

    // pending 不可为 current。
    expect_p0001(
        sqlx::query("UPDATE artifact.artifact SET current_version_id=$2 WHERE id=$1")
            .bind(artifact)
            .bind(pending)
            .execute(&pool),
    )
    .await;
    // 其他 artifact 的 ready 版本不可为 current（复合 scope 约束）。
    expect_p0001(
        sqlx::query("UPDATE artifact.artifact SET current_version_id=$2 WHERE id=$1")
            .bind(artifact)
            .bind(foreign_ready)
            .execute(&pool),
    )
    .await;
    // 同 artifact 的 ready 版本可以。
    sqlx::query("UPDATE artifact.artifact SET current_version_id=$2 WHERE id=$1")
        .bind(artifact)
        .bind(ready)
        .execute(&pool)
        .await
        .unwrap();
    let current: Uuid =
        sqlx::query_scalar("SELECT current_version_id FROM artifact.artifact WHERE id=$1")
            .bind(artifact)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(current, ready);
}

#[sqlx::test(migrations = "../migrations")]
async fn ready_version_is_immutable_and_terminal(pool: sqlx::PgPool) {
    let artifact =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "immutable").await;
    let version =
        common::sql_insert_version(&pool, common::TENANT, common::PROJECT_1, artifact, "ready")
            .await;

    // 内容不可改。
    expect_p0001(
        sqlx::query("UPDATE artifact.artifact_version SET manifest='{\"x\":1}' WHERE id=$1")
            .bind(version)
            .execute(&pool),
    )
    .await;
    expect_p0001(
        sqlx::query("UPDATE artifact.artifact_version SET source_object_id='obj-1' WHERE id=$1")
            .bind(version)
            .execute(&pool),
    )
    .await;
    // 状态不可离开 ready。
    expect_p0001(
        sqlx::query("UPDATE artifact.artifact_version SET state='failed' WHERE id=$1")
            .bind(version)
            .execute(&pool),
    )
    .await;
    // pending → failed 合法。
    let pending = common::sql_insert_version(
        &pool,
        common::TENANT,
        common::PROJECT_1,
        artifact,
        "pending",
    )
    .await;
    sqlx::query("UPDATE artifact.artifact_version SET state='failed', failure_code='RENDER_FAILED' WHERE id=$1")
        .bind(pending)
        .execute(&pool)
        .await
        .unwrap();
    // failed → ready 非法（状态机白名单）。
    expect_p0001(
        sqlx::query("UPDATE artifact.artifact_version SET state='ready' WHERE id=$1")
            .bind(pending)
            .execute(&pool),
    )
    .await;
}

#[sqlx::test(migrations = "../migrations")]
async fn deleted_artifact_rejects_children(pool: sqlx::PgPool) {
    let artifact =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "doomed").await;
    sqlx::query(
        "UPDATE artifact.artifact SET lifecycle_state='deleted', deleted_at=now() WHERE id=$1",
    )
    .bind(artifact)
    .execute(&pool)
    .await
    .unwrap();
    expect_p0001(sqlx::query(
        "INSERT INTO artifact.artifact_version (id, tenant_id, project_id, artifact_id, state, manifest) \
         VALUES ($1, $2, $3, $4, 'pending', '{}')",
    )
    .bind(Uuid::new_v4()).bind(common::TENANT).bind(common::PROJECT_1).bind(artifact)
    .execute(&pool)).await;
    expect_p0001(
        sqlx::query(
            "INSERT INTO artifact.artifact_reference \
         (id, tenant_id, project_id, artifact_id, owner_type, owner_id, relation_type, created_by) \
         VALUES ($1, $2, $3, $4, 'task', 't-1', 'illustrates', 'fixture')",
        )
        .bind(Uuid::new_v4())
        .bind(common::TENANT)
        .bind(common::PROJECT_1)
        .bind(artifact)
        .execute(&pool),
    )
    .await;
    expect_p0001(
        sqlx::query(
            "INSERT INTO artifact.generation_job \
         (id, tenant_id, project_id, artifact_id, generator_id) \
         VALUES ($1, $2, $3, $4, 'seecode')",
        )
        .bind(Uuid::new_v4())
        .bind(common::TENANT)
        .bind(common::PROJECT_1)
        .bind(artifact)
        .execute(&pool),
    )
    .await;
}

#[sqlx::test(migrations = "../migrations")]
async fn active_reference_unique_and_soft_remove(pool: sqlx::PgPool) {
    let artifact =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "refs").await;
    async fn insert(
        pool: &sqlx::PgPool,
        artifact: Uuid,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        sqlx::query(
            "INSERT INTO artifact.artifact_reference \
             (id, tenant_id, project_id, artifact_id, owner_type, owner_id, relation_type, created_by) \
             VALUES ($1, $2, $3, $4, 'task', 'task-42', 'illustrates', 'fixture')",
        )
        .bind(Uuid::new_v4()).bind(common::TENANT).bind(common::PROJECT_1).bind(artifact)
        .execute(pool)
        .await
    }
    insert(&pool, artifact).await.unwrap();
    // 同 (scope, artifact, owner, relation) 活跃引用唯一。
    let dup = insert(&pool, artifact).await.unwrap_err();
    assert_eq!(pg_error_code(&dup).as_deref(), Some("23505"));
    // 软删除后可重建。
    sqlx::query("UPDATE artifact.artifact_reference SET removed_at=now() WHERE owner_id='task-42'")
        .execute(&pool)
        .await
        .unwrap();
    insert(&pool, artifact).await.unwrap();
}

#[sqlx::test(migrations = "../migrations")]
async fn artifact_key_unique_within_project_only(pool: sqlx::PgPool) {
    async fn insert(
        pool: &sqlx::PgPool,
        project: Uuid,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        sqlx::query(
            "INSERT INTO artifact.artifact (id, tenant_id, project_id, artifact_key, name, artifact_type, created_by) \
             VALUES ($1, $2, $3, 'stable-key', 'keyed', 'file', 'fixture')",
        )
        .bind(Uuid::new_v4()).bind(common::TENANT).bind(project)
        .execute(pool)
        .await
    }
    insert(&pool, common::PROJECT_1).await.unwrap();
    let dup = insert(&pool, common::PROJECT_1).await.unwrap_err();
    assert_eq!(pg_error_code(&dup).as_deref(), Some("23505"));
    // 其他项目同名 key 合法。
    insert(&pool, common::PROJECT_2).await.unwrap();
    // NULL key 不受唯一约束。
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO artifact.artifact (id, tenant_id, project_id, name, artifact_type, created_by) \
             VALUES ($1, $2, $3, 'nokey', 'file', 'fixture')",
        )
        .bind(Uuid::new_v4()).bind(common::TENANT).bind(common::PROJECT_1)
        .execute(&pool)
        .await
        .unwrap();
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn audit_is_append_only(pool: sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO artifact.audit_event (id, action, object_type, outcome) \
         VALUES ($1, 'test.event', 'artifact', 'allowed')",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM artifact.audit_event LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    expect_p0001(
        sqlx::query("UPDATE artifact.audit_event SET action='tampered' WHERE id=$1")
            .bind(id)
            .execute(&pool),
    )
    .await;
    expect_p0001(
        sqlx::query("DELETE FROM artifact.audit_event WHERE id=$1")
            .bind(id)
            .execute(&pool),
    )
    .await;
}

#[sqlx::test(migrations = "../migrations")]
async fn generation_job_state_machine_enforced(pool: sqlx::PgPool) {
    let artifact =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "jobs").await;
    let job = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO artifact.generation_job (id, tenant_id, project_id, artifact_id, generator_id) \
         VALUES ($1, $2, $3, $4, 'seecode')",
    )
    .bind(job).bind(common::TENANT).bind(common::PROJECT_1).bind(artifact)
    .execute(&pool)
    .await
    .unwrap();
    // queued → running 合法。
    sqlx::query(
        "UPDATE artifact.generation_job SET status='running', started_at=now() WHERE id=$1",
    )
    .bind(job)
    .execute(&pool)
    .await
    .unwrap();
    // running → queued 非法（状态机白名单：running 只能进终态/retry_wait）。
    expect_p0001(
        sqlx::query("UPDATE artifact.generation_job SET status='queued' WHERE id=$1")
            .bind(job)
            .execute(&pool),
    )
    .await;
    // running → failed 合法。
    sqlx::query("UPDATE artifact.generation_job SET status='failed', completed_at=now(), error_code='X' WHERE id=$1")
        .bind(job)
        .execute(&pool)
        .await
        .unwrap();
    // 终态 failed → running 非法。
    expect_p0001(
        sqlx::query("UPDATE artifact.generation_job SET status='running' WHERE id=$1")
            .bind(job)
            .execute(&pool),
    )
    .await;
}

#[sqlx::test(migrations = "../migrations")]
async fn idempotency_record_unique_per_scope(pool: sqlx::PgPool) {
    async fn insert(
        pool: &sqlx::PgPool,
        project: Uuid,
        endpoint: &str,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        sqlx::query(
            "INSERT INTO artifact.idempotency_record \
             (id, tenant_id, project_id, endpoint, idempotency_key, request_hash) \
             VALUES ($1, $2, $3, $4, 'key-1', $5)",
        )
        .bind(Uuid::new_v4())
        .bind(common::TENANT)
        .bind(project)
        .bind(endpoint)
        .bind("a".repeat(64))
        .execute(pool)
        .await
    }
    insert(&pool, common::PROJECT_1, "POST /v1/artifacts")
        .await
        .unwrap();
    let dup = insert(&pool, common::PROJECT_1, "POST /v1/artifacts")
        .await
        .unwrap_err();
    assert_eq!(pg_error_code(&dup).as_deref(), Some("23505"));
    // 不同 endpoint / 不同 project 不冲突。
    insert(&pool, common::PROJECT_1, "POST /v1/other")
        .await
        .unwrap();
    insert(&pool, common::PROJECT_2, "POST /v1/artifacts")
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../migrations")]
async fn output_unique_per_version_role_format(pool: sqlx::PgPool) {
    let artifact =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "outputs").await;
    let version =
        common::sql_insert_version(&pool, common::TENANT, common::PROJECT_1, artifact, "ready")
            .await;
    async fn insert(
        pool: &sqlx::PgPool,
        artifact: Uuid,
        version: Uuid,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        sqlx::query(
            "INSERT INTO artifact.artifact_output \
             (id, tenant_id, project_id, artifact_id, version_id, output_role, format, mime_type, \
              size_bytes, sha256, file_object_id, safety_status) \
             VALUES ($1, $2, $3, $4, $5, 'primary', 'svg', 'image/svg+xml', 10, $6, 'obj-1', 'passed')",
        )
        .bind(Uuid::new_v4()).bind(common::TENANT).bind(common::PROJECT_1)
        .bind(artifact).bind(version)
        .bind("b".repeat(64))
        .execute(pool)
        .await
    }
    insert(&pool, artifact, version).await.unwrap();
    let dup = insert(&pool, artifact, version).await.unwrap_err();
    assert_eq!(pg_error_code(&dup).as_deref(), Some("23505"));
}
