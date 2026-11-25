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
    let password = "test-only-password";

    // 预建带口令的角色（脚本对已存在角色跳过创建）。
    for role in [&migrator, &app, &worker] {
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
        .replace("__WORKER_ROLE__", &worker);
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

    common::drop_test_roles(&pool, &[&migrator, &app, &worker]).await;
    app_pool.close().await;
    pool.close().await;
}
