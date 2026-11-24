//! Migration 不变量守卫（测试项 12/13）：
//!   * 0001_init.sql 内容与发布基线哈希完全一致（禁止修改历史 migration）；
//!   * Mermaid seed 删除幂等，SeeCode 保留且 unavailable。

mod common;

use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::postgres::PgPool;

const BASELINE_0001_SHA256: &str =
    "ea8acd4490516a31700005838c18a41cb7ff41e81b62f549cb407f874343a1c8";

#[test]
fn migration_0001_is_unchanged() {
    let content = include_str!("../../migrations/0001_init.sql");
    let digest = Sha256::digest(content.as_bytes());
    let actual: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        actual, BASELINE_0001_SHA256,
        "0001_init.sql must not be modified (ART-03B-01)"
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn mermaid_seed_removed_idempotently_and_seecode_kept(pool: PgPool) {
    // 0002 已删除 mermaid；重复执行同样 DELETE 不报错（幂等）。
    for _ in 0..2 {
        sqlx::query("DELETE FROM artifact.generator_registry WHERE generator_id='mermaid'")
            .execute(&pool)
            .await
            .unwrap();
    }
    let mermaid: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.generator_registry WHERE generator_id='mermaid'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(mermaid, 0);

    let row = sqlx::query(
        "SELECT status, status_reason FROM artifact.generator_registry WHERE generator_id='seecode'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let status: String = row.get("status");
    let reason: Option<String> = row.get("status_reason");
    assert_eq!(status, "unavailable");
    assert!(reason.is_some());
}

#[sqlx::test(migrations = "../migrations")]
async fn input_snapshot_columns_and_attempt_constraints_exist(pool: PgPool) {
    // 结构断言：Job 输入字段、Attempt 表与部分唯一索引存在。
    let cols: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_schema='artifact' AND table_name='generation_job' \
           AND column_name IN ('input_payload','options_payload','input_schema_version', \
                               'input_sha256','input_expires_at','cancel_requested_at')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cols, 6);

    let idx: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_indexes \
         WHERE schemaname='artifact' AND tablename='generation_job_attempt' \
           AND indexname='uq_generation_job_active_attempt'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(idx, 1);

    // PUBLIC 不应拥有受控函数的 EXECUTE 权限。
    let public_exec: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.role_routine_grants \
         WHERE routine_schema='artifact' AND grantee='PUBLIC' \
           AND privilege_type='EXECUTE' AND routine_name IN \
             ('claim_generation_job','complete_generation_job_attempt','heartbeat_generation_job')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(public_exec, 0, "PUBLIC must not EXECUTE worker functions");
}
