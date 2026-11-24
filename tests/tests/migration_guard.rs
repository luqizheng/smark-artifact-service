//! Migration 不变量守卫：
//!   * 已发布迁移的内容与 migrations.sha256 清单完全一致（禁止修改历史 migration）；
//!   * 每个迁移文件都必须在清单里（新增迁移在同一个 PR 里加一行）；
//!   * Mermaid seed 删除幂等，SeeCode 保留且 unavailable；
//!   * 0002 / 0003 的结构与约束存在；
//!   * 0003 能在 0002 已造成损坏数据的库上升级并自动修复。

mod common;

use artifact_application::WorkerCoordinationPort;
use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::postgres::PgPool;
use uuid::Uuid;

/// CI 用 `sha256sum -c` 校验同一份清单；这里用 Rust 再校验一次。
const MANIFEST: &str = include_str!("../../migrations.sha256");

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 清单里的路径相对仓库根目录（与 `sha256sum -c` 的解析方式一致）。
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[test]
fn released_migrations_are_unchanged() {
    let root = repo_root();
    let mut listed = std::collections::BTreeSet::new();
    for line in MANIFEST.lines().filter(|line| !line.trim().is_empty()) {
        let (expected, path) = line
            .split_once(char::is_whitespace)
            .unwrap_or_else(|| panic!("malformed manifest line: {line:?}"));
        let path = path.trim().trim_start_matches('*');
        listed.insert(path.to_string());
        let actual = sha256_hex(&std::fs::read(root.join(path)).expect("listed migration exists"));
        assert_eq!(
            actual, expected,
            "{path} must not be modified; released migrations are immutable"
        );
    }
    assert!(
        listed.len() >= 3,
        "expected the released migrations 0001..0003 to be pinned"
    );

    // 迁移目录里不允许存在未列入清单的迁移。
    for entry in std::fs::read_dir(root.join("migrations")).expect("migrations directory") {
        let entry = entry.expect("directory entry");
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".sql") {
            let path = format!("migrations/{name}");
            assert!(
                listed.contains(&path),
                "{path} is not pinned in migrations.sha256; add its hash in the same change"
            );
        }
    }
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
async fn job_coordination_structure_exists(pool: PgPool) {
    let cols: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_schema='artifact' AND table_name='generation_job' \
           AND column_name IN ('input_payload','options_payload','input_schema_version', \
                               'input_sha256','input_expires_at','cancel_requested_at', \
                               'next_attempt_at','expires_at','max_execution_seconds')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cols, 9);

    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT indexname FROM pg_indexes \
         WHERE schemaname='artifact' \
           AND indexname IN ('uq_generation_job_active_attempt', 'uq_generation_job_idempotency_key', \
                             'idx_generation_job_claimable', 'idx_generation_job_queue_expiry', \
                             'idx_generation_job_cancel_requested') \
         ORDER BY indexname",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        indexes,
        vec![
            "idx_generation_job_claimable",
            "idx_generation_job_queue_expiry",
            "uq_generation_job_active_attempt",
            "uq_generation_job_idempotency_key",
        ],
        "0003 must add the claim / idempotency indexes and drop the unused cancel index"
    );

    let constraints: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint \
         WHERE connamespace='artifact'::regnamespace AND conname IN \
           ('generation_job_attempt_active_lease_check', 'generation_job_error_code_check', \
            'generation_job_attempt_error_code_check', 'generation_job_attempt_deadline_check', \
            'generation_job_max_execution_check')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(constraints, 5);

    // claim 索引必须与领取查询的过滤与排序一致，否则规划器不会用它。
    let index: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes WHERE schemaname='artifact' AND indexname='idx_generation_job_claimable'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        index.contains("(generator_id, adapter_version, created_at, id)"),
        "{index}"
    );
    assert!(
        index.contains("status = ANY"),
        "the partial index must only cover queued / retry_wait jobs: {index}"
    );

    // PUBLIC 不应拥有任何函数的 EXECUTE 权限（包括 0003 新增的内部辅助函数）。
    let public_exec: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = 'artifact' AND has_function_privilege('public', p.oid, 'EXECUTE')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        public_exec, 0,
        "PUBLIC must not EXECUTE any artifact function"
    );
}

/// 0002 的 `lease_seconds = NULL` 会留下永久 running、无法恢复也无法取消的 Job，
/// 且没有幂等约束。0003 必须能在这样的库上升级成功并把数据修复。
#[sqlx::test]
async fn migration_0003_repairs_damage_left_by_0002(pool: PgPool) {
    sqlx::raw_sql(include_str!("../../migrations/0001_init.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../migrations/0002_job_coordination.sql"))
        .execute(&pool)
        .await
        .unwrap();

    let artifact =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "legacy").await;
    sqlx::query(
        "UPDATE artifact.generator_registry SET status='available' WHERE generator_id='seecode'",
    )
    .execute(&pool)
    .await
    .unwrap();

    // 0002 的创建函数签名：input_sha256 与 request_hash 都由调用方自报。
    async fn create_with_0002(pool: &PgPool, artifact: Uuid, key: Option<&str>) -> Uuid {
        let requested = Uuid::new_v4();
        let created: Uuid = sqlx::query_scalar(
            "SELECT artifact.create_generation_job_with_input \
             ($1, $2, $3, $4, 'seecode', '{}'::jsonb, '{}'::jsonb, '1', \
             $5, $5, $6, 3)",
        )
        .bind(requested)
        .bind(common::TENANT)
        .bind(common::PROJECT_1)
        .bind(artifact)
        .bind("a".repeat(64))
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(created, requested);
        created
    }

    // A / B / C：被 NULL 租约困住的 running Job（0002 的 claim 接受 NULL 租约秒数）。
    let mut trapped = Vec::new();
    for _ in 0..3 {
        trapped.push(create_with_0002(&pool, artifact, None).await);
        sqlx::query("SELECT attempt_id FROM artifact.claim_generation_job('buggy', NULL)")
            .execute(&pool)
            .await
            .unwrap();
    }
    // D：健康 attempt 被 heartbeat(NULL) 清空租约。
    let wiped = create_with_0002(&pool, artifact, None).await;
    let (attempt_id, token): (Uuid, String) = sqlx::query_as(
        "SELECT attempt_id, lease_token FROM artifact.claim_generation_job('w', 30)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query_scalar::<_, bool>("SELECT artifact.heartbeat_generation_job($1, $2, $3, NULL)")
        .bind(wiped)
        .bind(attempt_id)
        .bind(&token)
        .fetch_one(&pool)
        .await
        .unwrap();
    // E：取消后留下小写错误码。
    let cancelled = create_with_0002(&pool, artifact, None).await;
    let (attempt_id, token): (Uuid, String) = sqlx::query_as(
        "SELECT attempt_id, lease_token FROM artifact.claim_generation_job('w', 30)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query_scalar::<_, String>("SELECT artifact.request_generation_job_cancel($1)")
        .bind(cancelled)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query_scalar::<_, bool>("SELECT artifact.confirm_generation_job_cancelled($1, $2, $3)")
        .bind(cancelled)
        .bind(attempt_id)
        .bind(&token)
        .fetch_one(&pool)
        .await
        .unwrap();
    // E / F：同一个 Idempotency-Key 被用了两次（0002 没有唯一约束）。
    create_with_0002(&pool, artifact, Some("dup-key")).await;
    create_with_0002(&pool, artifact, Some("dup-key")).await;

    // A / B / C 被 claim(NULL) 直接困住，D 的租约被 heartbeat(NULL) 清空：共四个无租约的活跃 attempt。
    let stuck: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.generation_job_attempt \
         WHERE status IN ('claimed','running') AND lease_expires_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        stuck, 4,
        "fixture: 0002 must leave four attempts without a lease"
    );

    // 升级：重复的幂等键与卡死的 Job 都不能让迁移失败。
    sqlx::raw_sql(include_str!(
        "../../migrations/0003_job_coordination_fixes.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let stuck: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.generation_job_attempt \
         WHERE status IN ('claimed','running') AND lease_expires_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stuck, 0);

    let rows = sqlx::query(
        "SELECT status, error_code, next_attempt_at IS NOT NULL AS scheduled \
         FROM artifact.generation_job ORDER BY created_at",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let rendered: Vec<String> = rows
        .iter()
        .map(|row| {
            format!(
                "{}|{}|{}",
                row.get::<String, _>("status"),
                row.get::<Option<String>, _>("error_code")
                    .unwrap_or_default(),
                row.get::<bool, _>("scheduled")
            )
        })
        .collect();
    assert_eq!(
        rendered,
        vec![
            // 被困住的四个 Job（A / B / C / D）：重新排队并进入退避。
            "retry_wait||true".to_string(),
            "retry_wait||true".to_string(),
            "retry_wait||true".to_string(),
            "retry_wait||true".to_string(),
            // 小写错误码规范化。
            "cancelled|CANCELLED|false".to_string(),
            // 重复的 Idempotency-Key：只保留一个 Job 上的键。
            "queued||false".to_string(),
            "queued||false".to_string(),
        ]
    );

    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.generation_job WHERE idempotency_key = 'dup-key'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, 1);

    // 修复后的 Job 可以被新函数正常推进。
    let coordination =
        artifact_infrastructure::postgres::PgWorkerCoordination { pool: pool.clone() };
    // 旧 Job 没有固定的 adapter 版本（注册表当时也没有），所以领不到；但它们拿到了排队时限，
    // 会由恢复扫描收尾，而不是无限期挂着。
    assert!(
        coordination
            .claim("w", 30, &common::seecode_capabilities("adapter-x"))
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query(
        "UPDATE artifact.generation_job SET expires_at = now() - interval '1 second' \
         WHERE status IN ('queued','retry_wait')",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        coordination.recover_expired().await.unwrap(),
        6,
        "the four re-queued and the two queued legacy jobs expire"
    );
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.generation_job WHERE status IN ('queued','retry_wait','running')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0);
}
