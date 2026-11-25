//! Worker DB role 最小权限验证（ART-03A A3A-05 / 任务 §5）。
//! 使用真实 PG 角色与连接，证明 Worker 无法绕过受控函数直接修改业务表，
//! 且只能执行授权给它的那几个函数。

mod common;

use artifact_application::WorkerCoordinationPort;
use common::pg_error_code;
use sqlx::Row;
use sqlx::postgres::PgPool;
use uuid::Uuid;

const PASSWORD: &str = "test-only-password";

/// 建三个带随机名的角色，执行仓库真实角色脚本（占位符替换），
/// 再以 Worker 与 APP 角色各自建立连接。
async fn provision(pool: &PgPool) -> (String, String, String, PgPool, PgPool) {
    let suffix = &Uuid::new_v4().simple().to_string()[..8];
    let migrator = format!("wmig_{suffix}");
    let app = format!("wapp_{suffix}");
    let worker = format!("wkr_{suffix}");
    let maintenance = format!("wmtn_{suffix}");
    for role in [&migrator, &app, &worker, &maintenance] {
        sqlx::query(&format!(
            "CREATE ROLE \"{role}\" LOGIN PASSWORD '{PASSWORD}'"
        ))
        .execute(pool)
        .await
        .unwrap();
    }

    let script = include_str!("../../deploy/db-roles.sql")
        .replace("__MIGRATOR_ROLE__", &migrator)
        .replace("__APP_ROLE__", &app)
        .replace("__WORKER_ROLE__", &worker)
        .replace("__MAINTENANCE_ROLE__", &maintenance);
    sqlx::raw_sql(&script).execute(pool).await.unwrap();
    // 脚本必须可重复执行：每次迁移之后都要重新跑一遍。
    sqlx::raw_sql(&script).execute(pool).await.unwrap();

    // 本测试不校验清理主体，但角色是集群级对象：就地回收，避免污染集群。
    common::drop_test_roles(pool, &[&maintenance]).await;

    let worker_pool = connect_as(pool, &worker).await;
    let app_pool = connect_as(pool, &app).await;
    (migrator, app, worker, worker_pool, app_pool)
}

/// 连接到 sqlx::test 为本测试创建的数据库（而不是 DATABASE_URL 里的库）。
async fn connect_as(pool: &PgPool, role: &str) -> PgPool {
    let admin_url = std::env::var("DATABASE_URL").expect("DATABASE_URL for sqlx::test");
    let test_db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pool)
        .await
        .unwrap();
    let (creds_end, _) = admin_url.split_once('@').expect("DATABASE_URL credentials");
    let host_and_port = admin_url[creds_end.len() + 1..].split('/').next().unwrap();
    let url = format!("postgres://{role}:{PASSWORD}@{host_and_port}/{test_db}");
    PgPool::connect(&url).await.unwrap()
}

async fn executable_functions(pool: &PgPool, role: &str) -> Vec<(String, bool)> {
    // 触发器函数由表上的触发器调用，不经过调用方的 EXECUTE 权限，这里不参与比较。
    sqlx::query_as::<_, (String, bool)>(
        "SELECT p.proname, has_function_privilege($1, p.oid, 'EXECUTE') \
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = 'artifact' AND p.prorettype <> 'trigger'::regtype \
         ORDER BY p.proname",
    )
    .bind(role)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "../migrations")]
async fn worker_role_cannot_touch_business_tables(pool: PgPool) {
    let (migrator, app, worker, wp, ap) = provision(&pool).await;

    // 1. Worker 可以执行受控函数（claim 无 Job 时返回空，不报权限错误）。
    let claimed: Option<Uuid> =
        sqlx::query_scalar("SELECT job_id FROM artifact.claim_generation_job('role-worker', 30)")
            .fetch_optional(&wp)
            .await
            .unwrap();
    assert!(claimed.is_none());

    // 2. 动态遍历 artifact schema 的每一张业务表，对每张表逐一验证
    //    SELECT / INSERT / UPDATE / DELETE / TRUNCATE 全部被拒绝（42501）。
    //    将来新增业务表时，只要未显式授权 Worker，本测试自动覆盖。
    #[derive(sqlx::FromRow)]
    struct TableCol {
        table_name: String,
        first_col: String,
    }
    let tables: Vec<TableCol> = sqlx::query_as(
        "SELECT t.table_name, \
                (SELECT c.column_name FROM information_schema.columns c \
                  WHERE c.table_schema=t.table_schema AND c.table_name=t.table_name \
                  ORDER BY c.ordinal_position LIMIT 1) AS first_col \
         FROM information_schema.tables t \
         WHERE t.table_schema='artifact' AND t.table_type='BASE TABLE' \
         ORDER BY t.table_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        tables.len() >= 9,
        "expected all artifact business tables to be covered, got {}",
        tables.len()
    );
    for t in &tables {
        let statements = [
            format!("SELECT 1 FROM artifact.\"{}\" LIMIT 1", t.table_name),
            format!("INSERT INTO artifact.\"{}\" DEFAULT VALUES", t.table_name),
            format!(
                "UPDATE artifact.\"{}\" SET \"{}\"=\"{}\"",
                t.table_name, t.first_col, t.first_col
            ),
            format!("DELETE FROM artifact.\"{}\"", t.table_name),
            format!("TRUNCATE artifact.\"{}\"", t.table_name),
        ];
        for statement in &statements {
            let e = sqlx::query(statement).execute(&wp).await.unwrap_err();
            assert_eq!(
                pg_error_code(&e).as_deref(),
                Some("42501"),
                "Worker must not be allowed: {statement}"
            );
        }
    }

    // 3. Worker 不能 DDL，也不能创建函数。
    for ddl in [
        "CREATE TABLE artifact.worker_should_not_exist (id uuid primary key)",
        "CREATE FUNCTION artifact.worker_fn() RETURNS integer LANGUAGE sql AS 'SELECT 1'",
    ] {
        let e = sqlx::query(ddl).execute(&wp).await.unwrap_err();
        assert_eq!(pg_error_code(&e).as_deref(), Some("42501"), "{ddl}");
    }

    // 4. 函数权限矩阵是精确的：Worker 只能执行授权给它的 7 个协调函数。
    let worker_granted: Vec<String> = executable_functions(&pool, &worker)
        .await
        .into_iter()
        .filter(|(_, granted)| *granted)
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        worker_granted,
        vec![
            "claim_generation_job",
            "complete_generation_job_attempt",
            "confirm_generation_job_cancelled",
            "fail_generation_job_attempt",
            "heartbeat_generation_job",
            "purge_expired_generation_job_input",
            "recover_expired_generation_jobs",
        ]
    );

    // 5. API 角色只能执行创建与取消两个受控函数，拿不到任何 Worker 专用函数
    //    或内部辅助函数。
    let app_granted: Vec<String> = executable_functions(&pool, &app)
        .await
        .into_iter()
        .filter(|(_, granted)| *granted)
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        app_granted,
        vec![
            "create_generation_job_with_input",
            "request_generation_job_cancel"
        ]
    );

    // 6. 权限矩阵之外，再用真实连接确认两边各自的越权调用都被拒绝。
    let e = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT * FROM artifact.create_generation_job_with_input($1,$2,$3,$4,'seecode', \
         '{}'::jsonb,'{}'::jsonb,'1',$5,NULL,NULL::uuid,NULL::text,NULL::smallint,3,300,86400)",
    )
    .bind(Uuid::new_v4())
    .bind(common::TENANT)
    .bind(common::PROJECT_1)
    .bind(Uuid::new_v4())
    .bind("a".repeat(64))
    .fetch_one(&wp)
    .await
    .unwrap_err();
    assert_eq!(pg_error_code(&e).as_deref(), Some("42501"));

    let e =
        sqlx::query_scalar::<_, String>("SELECT artifact.request_generation_job_cancel($1,$2,$3)")
            .bind(common::TENANT)
            .bind(common::PROJECT_1)
            .bind(Uuid::new_v4())
            .fetch_one(&wp)
            .await
            .unwrap_err();
    assert_eq!(pg_error_code(&e).as_deref(), Some("42501"));

    let e = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT job_id FROM artifact.claim_generation_job('api', 30)",
    )
    .fetch_one(&ap)
    .await
    .unwrap_err();
    assert_eq!(pg_error_code(&e).as_deref(), Some("42501"));

    // 7. Worker 不是 API 角色的成员（不继承其权限）。
    let inherited: bool = sqlx::query_scalar("SELECT pg_has_role(current_user, $1, 'MEMBER')")
        .bind(&app)
        .fetch_one(&wp)
        .await
        .unwrap();
    assert!(!inherited, "worker must not inherit the API role");

    wp.close().await;
    ap.close().await;
    common::drop_test_roles(&pool, &[&migrator, &app, &worker]).await;
    pool.close().await;
}

#[sqlx::test(migrations = "../migrations")]
async fn worker_role_can_claim_and_heartbeat_via_controlled_function(pool: PgPool) {
    let (migrator, app, worker, wp, ap) = provision(&pool).await;

    // 布景（admin）：可领取的 Job（生成器可用且带 adapter 版本）。
    let artifact =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "wrole").await;
    common::sql_enable_seecode(&pool, "adapter-role").await;
    let job_id = common::sql_create_job(
        &pool,
        common::TENANT,
        common::PROJECT_1,
        artifact,
        &common::JobOptions::default(),
    )
    .await;

    // Worker 通过受控函数领取（SECURITY DEFINER 代为读写业务表）。
    let row = sqlx::query(
        "SELECT job_id, attempt_id, lease_token FROM \
         artifact.claim_generation_job('w', 30, '{\"seecode\":\"adapter-role\"}'::jsonb)",
    )
    .fetch_one(&wp)
    .await
    .unwrap();
    let claimed_job: Uuid = row.get("job_id");
    let attempt_id: Uuid = row.get("attempt_id");
    let token: String = row.get("lease_token");
    assert_eq!(claimed_job, job_id);

    // Worker 可通过受控函数 heartbeat / fail。
    let renewed: String =
        sqlx::query_scalar("SELECT artifact.heartbeat_generation_job($1,$2,$3,30)")
            .bind(job_id)
            .bind(attempt_id)
            .bind(&token)
            .fetch_one(&wp)
            .await
            .unwrap();
    assert_eq!(renewed, "renewed");

    let outcome: String = sqlx::query_scalar(
        "SELECT artifact.fail_generation_job_attempt($1,$2,$3,'WORKER_TEST',NULL,false)",
    )
    .bind(job_id)
    .bind(attempt_id)
    .bind(&token)
    .fetch_one(&wp)
    .await
    .unwrap();
    assert_eq!(outcome, "failed");

    // 同一套调用走 Rust 端口（以 Worker 角色连接）同样成立：
    // 权限边界只由数据库角色决定，与调用方式无关。
    let coordination = artifact_infrastructure::postgres::PgWorkerCoordination { pool: wp.clone() };
    let none = coordination
        .claim("w", 30, &common::seecode_capabilities("adapter-role"))
        .await
        .unwrap();
    assert!(none.is_none(), "the terminal job is not claimed again");

    wp.close().await;
    ap.close().await;
    common::drop_test_roles(&pool, &[&migrator, &app, &worker]).await;
    pool.close().await;
}
