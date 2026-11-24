//! Worker DB role 最小权限验证（ART-03A A3A-05 / 任务 §5 / 测试项 11）。
//! 使用真实 PG 角色与连接，证明 Worker 无法绕过受控函数直接修改业务表。

mod common;

use common::pg_error_code;
use sqlx::Row;
use uuid::Uuid;

#[sqlx::test(migrations = "../migrations")]
async fn worker_role_cannot_touch_business_tables(pool: sqlx::PgPool) {
    let suffix = &Uuid::new_v4().simple().to_string()[..8];
    let migrator = format!("wmig_{suffix}");
    let app = format!("wapp_{suffix}");
    let worker = format!("wkr_{suffix}");
    let password = "test-only-password";

    for role in [&migrator, &app, &worker] {
        sqlx::query(&format!(
            "CREATE ROLE \"{role}\" LOGIN PASSWORD '{password}'"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }

    let script = include_str!("../../deploy/db-roles.sql")
        .replace("__MIGRATOR_ROLE__", &migrator)
        .replace("__APP_ROLE__", &app)
        .replace("__WORKER_ROLE__", &worker);
    sqlx::raw_sql(&script).execute(&pool).await.unwrap();

    let admin_url = std::env::var("DATABASE_URL").expect("DATABASE_URL for sqlx::test");
    let test_db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (creds_end, _) = admin_url.split_once('@').expect("DATABASE_URL credentials");
    let host_and_port = admin_url[creds_end.len() + 1..].split('/').next().unwrap();
    let worker_url = format!("postgres://{worker}:{password}@{host_and_port}/{test_db}");
    let wp = sqlx::PgPool::connect(&worker_url).await.unwrap();

    // 1. Worker 可以执行受控函数（claim 无 Job 时返回空，不报权限错误）。
    let claimed: Option<Uuid> =
        sqlx::query_scalar("SELECT job_id FROM artifact.claim_generation_job('role-worker', 30)")
            .fetch_optional(&wp)
            .await
            .unwrap();
    assert!(claimed.is_none());

    // 2. 动态遍历 artifact schema 的每一张业务表（含 0002 新增的 attempt 表），
    //    对每张表逐一验证 SELECT / INSERT / UPDATE / DELETE 全部被拒绝（42501）。
    //    这样即使将来新增业务表，只要未显式授权 Worker，本测试自动覆盖。
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
    let mut checked = 0usize;
    for t in &tables {
        let q = format!("SELECT 1 FROM artifact.\"{}\" LIMIT 1", t.table_name);
        let e = sqlx::query(&q).execute(&wp).await.unwrap_err();
        assert_eq!(
            pg_error_code(&e).as_deref(),
            Some("42501"),
            "SELECT {}",
            t.table_name
        );

        let q = format!("INSERT INTO artifact.\"{}\" DEFAULT VALUES", t.table_name);
        let e = sqlx::query(&q).execute(&wp).await.unwrap_err();
        assert_eq!(
            pg_error_code(&e).as_deref(),
            Some("42501"),
            "INSERT {}",
            t.table_name
        );

        let q = format!(
            "UPDATE artifact.\"{}\" SET \"{}\"=\"{}\"",
            t.table_name, t.first_col, t.first_col
        );
        let e = sqlx::query(&q).execute(&wp).await.unwrap_err();
        assert_eq!(
            pg_error_code(&e).as_deref(),
            Some("42501"),
            "UPDATE {}",
            t.table_name
        );

        let q = format!("DELETE FROM artifact.\"{}\"", t.table_name);
        let e = sqlx::query(&q).execute(&wp).await.unwrap_err();
        assert_eq!(
            pg_error_code(&e).as_deref(),
            Some("42501"),
            "DELETE {}",
            t.table_name
        );

        checked += 1;
    }
    assert!(
        checked >= 9,
        "worker permission matrix must cover every business table"
    );

    // 6. Worker 不能 DDL。
    let e = sqlx::query("CREATE TABLE artifact.worker_should_not_exist (id uuid primary key)")
        .execute(&wp)
        .await
        .unwrap_err();
    assert_eq!(pg_error_code(&e).as_deref(), Some("42501"));

    // 7. Worker 不能执行未授权函数（create_generation_job_with_input 仅 APP role）。
    let e = sqlx::query_scalar::<_, Uuid>(
        "SELECT artifact.create_generation_job_with_input($1,$2,$3,$4,'seecode', \
         '{}'::jsonb,'{}'::jsonb,'1',$5,$6,NULL,3)",
    )
    .bind(Uuid::new_v4())
    .bind(common::TENANT)
    .bind(common::PROJECT_1)
    .bind(Uuid::new_v4())
    .bind("a".repeat(64))
    .bind("b".repeat(64))
    .fetch_one(&wp)
    .await
    .unwrap_err();
    assert_eq!(pg_error_code(&e).as_deref(), Some("42501"));

    // 8. Worker 不是 API role 的成员（不继承其权限）。
    let inherited: bool = sqlx::query_scalar("SELECT pg_has_role(current_user, $1, 'MEMBER')")
        .bind(&app)
        .fetch_one(&wp)
        .await
        .unwrap();
    assert!(!inherited, "worker must not inherit the API role");

    wp.close().await;
    pool.close().await;
}

#[sqlx::test(migrations = "../migrations")]
async fn worker_role_can_claim_and_heartbeat_via_controlled_function(pool: sqlx::PgPool) {
    let suffix = &Uuid::new_v4().simple().to_string()[..8];
    let worker = format!("wc_{suffix}");
    let password = "test-only-password";
    sqlx::query(&format!(
        "CREATE ROLE \"{worker}\" LOGIN NOINHERIT PASSWORD '{password}'"
    ))
    .execute(&pool)
    .await
    .unwrap();

    // 只执行 Worker 相关授权（从真实脚本中提取WORKER段，避免依赖 migrator/app 角色）。
    let script = include_str!("../../deploy/db-roles.sql")
        .replace("__MIGRATOR_ROLE__", &format!("wm_{suffix}"))
        .replace("__APP_ROLE__", &format!("wa_{suffix}"))
        .replace("__WORKER_ROLE__", &worker);
    // 先建 migrator/app 角色使脚本可完整执行。
    for role in [&format!("wm_{suffix}"), &format!("wa_{suffix}")] {
        sqlx::query(&format!(
            "CREATE ROLE \"{role}\" LOGIN PASSWORD '{password}'"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::raw_sql(&script).execute(&pool).await.unwrap();

    let admin_url = std::env::var("DATABASE_URL").unwrap();
    let test_db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (creds_end, _) = admin_url.split_once('@').unwrap();
    let host_and_port = admin_url[creds_end.len() + 1..].split('/').next().unwrap();
    let wp = sqlx::PgPool::connect(&format!(
        "postgres://{worker}:{password}@{host_and_port}/{test_db}"
    ))
    .await
    .unwrap();

    // 布景（admin）：可领取 Job + available generator。
    let artifact =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "wrole").await;
    sqlx::query(
        "UPDATE artifact.generator_registry SET status='available' WHERE generator_id='seecode'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let job_id = Uuid::new_v4();
    let hash = "a".repeat(64);
    let req_hash = "b".repeat(64);
    sqlx::query_scalar::<_, Uuid>(
        "SELECT artifact.create_generation_job_with_input($1,$2,$3,$4,'seecode', \
         '{}'::jsonb,'{}'::jsonb,'1',$5,$6,NULL,3)",
    )
    .bind(job_id)
    .bind(common::TENANT)
    .bind(common::PROJECT_1)
    .bind(artifact)
    .bind(hash)
    .bind(req_hash)
    .fetch_one(&pool)
    .await
    .unwrap();

    // Worker 通过受控函数领取（SECURITY DEFINER 代为读写业务表）。
    let row = sqlx::query(
        "SELECT job_id, attempt_id, lease_token FROM artifact.claim_generation_job('w',30)",
    )
    .fetch_one(&wp)
    .await
    .unwrap();
    let claimed_job: Uuid = row.get("job_id");
    let attempt_id: Uuid = row.get("attempt_id");
    let token: String = row.get("lease_token");
    assert_eq!(claimed_job, job_id);

    // Worker 可通过受控函数 heartbeat / fail。
    let ok: bool = sqlx::query_scalar("SELECT artifact.heartbeat_generation_job($1,$2,$3,30)")
        .bind(job_id)
        .bind(attempt_id)
        .bind(&token)
        .fetch_one(&wp)
        .await
        .unwrap();
    assert!(ok);

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

    wp.close().await;
    pool.close().await;
}
