//! 数据库角色分离（任务 §4.3）：迁移角色可 DDL，运行角色仅 DML
//! （无 CREATE、无 DELETE、审计表不可 UPDATE）。

mod common;

use common::pg_error_code;
use uuid::Uuid;

#[sqlx::test(migrations = "../migrations")]
async fn runtime_role_is_dml_only(pool: sqlx::PgPool) {
    // 测试库的连接用户即管理员（CI=postgres / 本地容器=postgres），可建角色。
    let suffix = Uuid::new_v4().simple().to_string()[..8].to_string();
    let migrator = format!("art_migr_{suffix}");
    let app = format!("art_app_{suffix}");
    let worker = format!("art_wrkr_{suffix}");
    let maintenance = format!("art_mntn_{suffix}");
    let password = "test-only-password";

    // 预建带口令的角色（脚本对已存在角色跳过创建）。
    for role in [&migrator, &app, &worker, &maintenance] {
        sqlx::query(&format!(
            "CREATE ROLE \"{role}\" LOGIN PASSWORD '{password}'"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }

    // 执行仓库真实角色脚本（占位符替换）。
    let script = include_str!("../../deploy/db-roles.sql")
        .replace("__MIGRATOR_ROLE__", &migrator)
        .replace("__APP_ROLE__", &app)
        .replace("__WORKER_ROLE__", &worker)
        .replace("__MAINTENANCE_ROLE__", &maintenance);
    sqlx::raw_sql(&script).execute(&pool).await.unwrap();

    // 以运行角色建立连接（连接到 sqlx::test 的当前测试库，而非 DATABASE_URL 里的库）。
    let admin_url = std::env::var("DATABASE_URL").expect("DATABASE_URL for sqlx::test");
    let test_db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (creds_end, _) = admin_url
        .split_once('@')
        .expect("DATABASE_URL with credentials");
    let after_creds = &admin_url[creds_end.len() + 1..];
    let host_and_port = after_creds
        .split('/')
        .next()
        .expect("host:port in DATABASE_URL");
    let app_url = format!("postgres://{app}:{password}@{host_and_port}/{test_db}");
    let app_pool = sqlx::PgPool::connect(&app_url).await.unwrap();

    // DML 允许：INSERT artifact。
    sqlx::query(
        "INSERT INTO artifact.artifact (id, tenant_id, project_id, name, artifact_type, created_by) \
         VALUES ($1, $2, $3, 'role-test', 'file', 'role-test')",
    )
    .bind(Uuid::new_v4())
    .bind(common::TENANT)
    .bind(common::PROJECT_1)
    .execute(&app_pool)
    .await
    .unwrap();

    // DML 允许：UPDATE artifact（软生命周期维护路径）。
    let id: Uuid =
        sqlx::query_scalar("SELECT id FROM artifact.artifact WHERE created_by='role-test'")
            .fetch_one(&app_pool)
            .await
            .unwrap();
    sqlx::query("UPDATE artifact.artifact SET description='patched' WHERE id=$1")
        .bind(id)
        .execute(&app_pool)
        .await
        .unwrap();

    // DDL 拒绝：CREATE TABLE。
    let ddl = sqlx::query("CREATE TABLE artifact.should_not_exist (id uuid primary key)")
        .execute(&app_pool)
        .await
        .unwrap_err();
    assert_eq!(pg_error_code(&ddl).as_deref(), Some("42501"));

    // DELETE 拒绝。
    let del = sqlx::query("DELETE FROM artifact.artifact WHERE id=$1")
        .bind(id)
        .execute(&app_pool)
        .await
        .unwrap_err();
    assert_eq!(pg_error_code(&del).as_deref(), Some("42501"));

    // 审计表 UPDATE 拒绝（append-only 双保险）。
    sqlx::query(
        "INSERT INTO artifact.audit_event (id, action, object_type, outcome) \
         VALUES ($1, 'role.test', 'artifact', 'allowed')",
    )
    .bind(Uuid::new_v4())
    .execute(&app_pool)
    .await
    .unwrap();
    let audit_update = sqlx::query("UPDATE artifact.audit_event SET action='tamper'")
        .execute(&app_pool)
        .await
        .unwrap_err();
    assert_eq!(pg_error_code(&audit_update).as_deref(), Some("42501"));

    // 默认权限必须写明 FOR ROLE 迁移角色。迁移由管理员应用、脚本也由管理员执行，
    // 所以"迁移角色之后新建的表"能否拿到授权，只取决于 ALTER DEFAULT PRIVILEGES 的 FOR ROLE。
    // SET ROLE 只对当前连接生效：必须与建表在同一事务、同一条连接里以迁移角色执行。
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE \"{migrator}\""))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE artifact.future_migration_table (id integer)")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let owner: String =
        sqlx::query_scalar("SELECT tableowner FROM pg_tables WHERE schemaname='artifact' AND tablename='future_migration_table'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        owner, migrator,
        "fixture: the table must be owned by the migrator role"
    );

    async fn privilege(pool: &sqlx::PgPool, role: &str, privilege: &str) -> bool {
        sqlx::query_scalar("SELECT has_table_privilege($1, 'artifact.future_migration_table', $2)")
            .bind(role)
            .bind(privilege)
            .fetch_one(pool)
            .await
            .unwrap()
    }
    assert!(
        privilege(&pool, &app, "SELECT").await,
        "tables created by the migrator after the grant script ran must be readable by the app role"
    );
    assert!(privilege(&pool, &app, "INSERT").await);
    assert!(
        !privilege(&pool, &app, "DELETE").await,
        "the app role never gets DELETE by default"
    );
    assert!(
        !privilege(&pool, &worker, "SELECT").await,
        "the worker role never gets table access by default"
    );

    // ── AS-1：持久化创建身份与清理函数的最小权限 ───────────────────────
    async fn table_privilege(
        pool: &sqlx::PgPool,
        role: &str,
        table: &str,
        privilege: &str,
    ) -> bool {
        sqlx::query_scalar("SELECT has_table_privilege($1, $2, $3)")
            .bind(role)
            .bind(table)
            .bind(privilege)
            .fetch_one(pool)
            .await
            .unwrap()
    }
    async fn function_executable(pool: &sqlx::PgPool, role: &str, signature: &str) -> bool {
        sqlx::query_scalar("SELECT has_function_privilege($1, $2, 'EXECUTE')")
            .bind(role)
            .bind(signature)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    assert!(
        table_privilege(&pool, &app, "artifact.artifact_creation_identity", "SELECT").await,
        "the app role must be able to read durable identities"
    );
    assert!(
        table_privilege(&pool, &app, "artifact.artifact_creation_identity", "INSERT").await,
        "the app role must be able to append durable identities"
    );
    assert!(
        !table_privilege(&pool, &app, "artifact.artifact_creation_identity", "UPDATE").await,
        "durable identities are append-only: the app role must not UPDATE them"
    );
    assert!(
        !table_privilege(&pool, &app, "artifact.artifact_creation_identity", "DELETE").await,
        "durable identities are append-only: the app role must not DELETE them"
    );
    assert!(
        !function_executable(
            &pool,
            &app,
            "artifact.purge_expired_idempotency_records(integer)"
        )
        .await,
        "the app role must not be able to run the maintenance purge"
    );

    assert!(
        function_executable(
            &pool,
            &maintenance,
            "artifact.purge_expired_idempotency_records(integer)"
        )
        .await,
        "the maintenance principal gets exactly the purge function"
    );
    for table in [
        "artifact.artifact",
        "artifact.idempotency_record",
        "artifact.artifact_creation_identity",
    ] {
        assert!(
            !table_privilege(&pool, &maintenance, table, "DELETE").await,
            "the maintenance principal must never hold table-level DELETE ({table})"
        );
        assert!(
            !table_privilege(&pool, &maintenance, table, "SELECT").await,
            "the maintenance principal must never read business tables directly ({table})"
        );
    }

    // 清理函数自身的硬化属性：SECURITY DEFINER + 固定 search_path + 无 PUBLIC EXECUTE。
    let (secdef, proconfig): (bool, Option<Vec<String>>) = sqlx::query_as(
        "SELECT prosecdef, proconfig FROM pg_proc \
         WHERE oid = 'artifact.purge_expired_idempotency_records(integer)'::regprocedure",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(secdef, "the purge function must be SECURITY DEFINER");
    let proconfig = proconfig.unwrap_or_default();
    assert!(
        proconfig
            .iter()
            .any(|c| c == "search_path=pg_catalog, pg_temp"),
        "the purge function must pin search_path, got {proconfig:?}"
    );
    let public_can_execute: bool = sqlx::query_scalar(
        "SELECT EXISTS ( \
           SELECT 1 FROM pg_proc p, aclexplode(COALESCE(p.proacl, acldefault('f', p.proowner))) a \
           WHERE p.oid = 'artifact.purge_expired_idempotency_records(integer)'::regprocedure \
             AND a.grantee = 0)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        !public_can_execute,
        "PUBLIC must not be able to execute the purge function"
    );
    // 权限不是「声称」而是实际拒绝：APP role 真正尝试改写身份行必须被 PostgreSQL 拒绝。
    let keyed_id = Uuid::new_v4();
    let mut tx = app_pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO artifact.artifact \
         (id, tenant_id, project_id, artifact_key, name, artifact_type, created_by) \
         VALUES ($1, $2, $3, 'role.identity', 'role-test', 'file', 'role-test')",
    )
    .bind(keyed_id)
    .bind(common::TENANT)
    .bind(common::PROJECT_1)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO artifact.artifact_creation_identity \
         (tenant_id, project_id, artifact_key, artifact_id, fingerprint_version, request_fingerprint, created_by) \
         VALUES ($1, $2, 'role.identity', $3, 1, repeat('0', 64), 'role-test')",
    )
    .bind(common::TENANT)
    .bind(common::PROJECT_1)
    .bind(keyed_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let update = sqlx::query(
        "UPDATE artifact.artifact_creation_identity SET created_by='tamper' \
         WHERE tenant_id=$1 AND project_id=$2 AND artifact_key='role.identity'",
    )
    .bind(common::TENANT)
    .bind(common::PROJECT_1)
    .execute(&app_pool)
    .await
    .unwrap_err();
    assert_eq!(pg_error_code(&update).as_deref(), Some("42501"));

    let delete = sqlx::query(
        "DELETE FROM artifact.artifact_creation_identity \
         WHERE tenant_id=$1 AND project_id=$2 AND artifact_key='role.identity'",
    )
    .bind(common::TENANT)
    .bind(common::PROJECT_1)
    .execute(&app_pool)
    .await
    .unwrap_err();
    assert_eq!(pg_error_code(&delete).as_deref(), Some("42501"));

    let rename =
        sqlx::query("UPDATE artifact.artifact SET artifact_key='role.renamed' WHERE id=$1")
            .bind(keyed_id)
            .execute(&app_pool)
            .await
            .unwrap_err();
    assert_eq!(
        pg_error_code(&rename).as_deref(),
        Some("P0001"),
        "the artifact_key immutability trigger must block direct SQL"
    );

    // 端到端：maintenance 真正执行清理函数（SECURITY DEFINER 运行时路径），
    // 且只删除「已完成 + 过期」行；未完成行与直接 DELETE 都被拒绝。
    let completed_id = Uuid::new_v4();
    let incomplete_id = Uuid::new_v4();
    for (id, completed) in [(completed_id, true), (incomplete_id, false)] {
        sqlx::query(
            "INSERT INTO artifact.idempotency_record \
             (id, tenant_id, project_id, endpoint, idempotency_key, request_hash, \
              response_snapshot, resource_id, expires_at) \
             VALUES ($1, $2, $3, 'role-test', $4, repeat('a', 64), \
                     CASE WHEN $5 THEN '{\"ok\":true}'::jsonb ELSE NULL END, \
                     NULL, now() - interval '1 day')",
        )
        .bind(id)
        .bind(common::TENANT)
        .bind(common::PROJECT_1)
        .bind(format!("role-purge-{}", id.simple()))
        .bind(completed)
        .execute(&pool)
        .await
        .unwrap();
    }
    let maintenance_url = format!("postgres://{maintenance}:{password}@{host_and_port}/{test_db}");
    let maintenance_pool = sqlx::PgPool::connect(&maintenance_url).await.unwrap();
    let purged: i32 = sqlx::query_scalar("SELECT artifact.purge_expired_idempotency_records(100)")
        .fetch_one(&maintenance_pool)
        .await
        .unwrap();
    assert!(purged >= 1, "the completed expired row must be purged");
    let completed_left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM artifact.idempotency_record WHERE id=$1")
            .bind(completed_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let incomplete_left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM artifact.idempotency_record WHERE id=$1")
            .bind(incomplete_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(completed_left, 0, "completed expired row is removable");
    assert_eq!(incomplete_left, 1, "incomplete rows are never TTL-purged");
    let direct_delete = sqlx::query("DELETE FROM artifact.idempotency_record WHERE id=$1")
        .bind(incomplete_id)
        .execute(&maintenance_pool)
        .await
        .unwrap_err();
    assert_eq!(pg_error_code(&direct_delete).as_deref(), Some("42501"));
    maintenance_pool.close().await;

    common::drop_test_roles(&pool, &[&migrator, &app, &worker, &maintenance]).await;
    app_pool.close().await;
    pool.close().await;
}
