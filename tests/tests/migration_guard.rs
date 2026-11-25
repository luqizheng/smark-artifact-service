//! Migration 不变量守卫：
//!   * 已发布迁移的内容与 migrations.sha256 清单完全一致（禁止修改历史 migration）；
//!   * 每个迁移文件都必须在清单里（新增迁移在同一个 PR 里加一行）；
//!   * Mermaid seed 删除幂等，SeeCode 保留且 unavailable；
//!   * 0002 / 0003 的结构与约束存在；
//!   * 0003 能在 0002 已造成损坏数据的库上升级并自动修复；
//!   * 0002 → 0003 的 deadline 回填：历史活跃 Attempt 获得 deadline、租约被夹紧，
//!     已过期的 deadline 由 recovery 立即按 GENERATION_TIMEOUT 收尾（场景 A/B/C）。

mod common;

use artifact_application::{HeartbeatOutcome, WorkerCoordinationPort};
use artifact_domain::AttemptState;
use artifact_infrastructure::postgres::PgWorkerCoordination;
use chrono::{DateTime, Utc};
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

// ── 0002 → 0003 的 deadline 回填回归（场景 A/B/C） ──────────────────────
//
// 三个场景都真实经过迁移路径：先只应用 0001+0002 并用 0002 的受控函数构造数据，
// 再应用 0003 后断言——而不是在最终形态的库上手工设置 deadline。

/// 布景：只应用 0001+0002（不含 0003），建 artifact 并把 seecode 临时置为 available
/// （0002 时代的注册表没有 adapter_version）。返回 artifact id。
async fn apply_0001_and_0002(pool: &PgPool) -> Uuid {
    sqlx::raw_sql(include_str!("../../migrations/0001_init.sql"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../migrations/0002_job_coordination.sql"))
        .execute(pool)
        .await
        .unwrap();
    let artifact =
        common::sql_insert_artifact(pool, common::TENANT, common::PROJECT_1, "deadline").await;
    sqlx::query(
        "UPDATE artifact.generator_registry SET status = 'available' \
         WHERE generator_id = 'seecode'",
    )
    .execute(pool)
    .await
    .unwrap();
    artifact
}

/// 按 0002 的受控函数签名创建 queued Job（input_sha256 由调用方自报）。
async fn create_job_under_0002(pool: &PgPool, artifact: Uuid, max_attempts: i32) -> Uuid {
    let requested = Uuid::new_v4();
    let created: Uuid = sqlx::query_scalar(
        "SELECT artifact.create_generation_job_with_input \
         ($1, $2, $3, $4, 'seecode', '{}'::jsonb, '{}'::jsonb, '1', \
         $5, $5, NULL, $6)",
    )
    .bind(requested)
    .bind(common::TENANT)
    .bind(common::PROJECT_1)
    .bind(artifact)
    .bind("a".repeat(64))
    .bind(max_attempts)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(created, requested);
    created
}

/// 0002 的 claim：返回 (job_id, attempt_id, lease_token 明文)。
async fn claim_under_0002(pool: &PgPool, worker: &str, lease_seconds: i32) -> (Uuid, Uuid, String) {
    sqlx::query_as::<_, (Uuid, Uuid, String)>(
        "SELECT job_id, attempt_id, lease_token FROM artifact.claim_generation_job($1, $2)",
    )
    .bind(worker)
    .bind(lease_seconds)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn apply_0003(pool: &PgPool) {
    sqlx::raw_sql(include_str!(
        "../../migrations/0003_job_coordination_fixes.sql"
    ))
    .execute(pool)
    .await
    .unwrap();
}

fn coordination(pool: &PgPool) -> PgWorkerCoordination {
    PgWorkerCoordination { pool: pool.clone() }
}

/// 场景 A：迁移时仍 running 的历史 Attempt 获得 deadline，租约被限制在 deadline 以内。
#[sqlx::test]
async fn migration_0003_backfills_deadline_for_active_attempts(pool: PgPool) {
    let artifact = apply_0001_and_0002(&pool).await;

    // 长租约（0002 允许 1..=3600 秒）：远超 deadline，迁移后必须被夹紧。
    let long_job = create_job_under_0002(&pool, artifact, 3).await;
    let (claimed, long_attempt, _) = claim_under_0002(&pool, "long-lease", 3600).await;
    assert_eq!(claimed, long_job);
    // 短租约：本来就在 deadline 之内，必须保持原值。
    let short_job = create_job_under_0002(&pool, artifact, 3).await;
    let (_, short_attempt, _) = claim_under_0002(&pool, "short-lease", 30).await;
    let _ = short_job;

    let short_lease_before: DateTime<Utc> = sqlx::query_scalar(
        "SELECT lease_expires_at FROM artifact.generation_job_attempt WHERE id = $1",
    )
    .bind(short_attempt)
    .fetch_one(&pool)
    .await
    .unwrap();

    // 迁移前没有 deadline 列（0002 无该列），活跃 Attempt 自然也没有 deadline。
    let deadline_columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_schema = 'artifact' \
           AND table_name = 'generation_job_attempt' AND column_name = 'deadline_at'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(deadline_columns, 0, "fixture: 0002 has no deadline column");

    apply_0003(&pool).await;

    for attempt_id in [long_attempt, short_attempt] {
        let (deadline, lease, started): (Option<DateTime<Utc>>, DateTime<Utc>, DateTime<Utc>) =
            sqlx::query_as(
                "SELECT deadline_at, lease_expires_at, started_at \
                 FROM artifact.generation_job_attempt WHERE id = $1",
            )
            .bind(attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let deadline = deadline.expect("deadline must be backfilled for an active attempt");
        // deadline = started_at + max_execution_seconds（0002 时代的 Job 迁移后默认 300 秒）。
        assert_eq!(deadline - started, chrono::Duration::seconds(300));
        assert!(lease <= deadline, "the lease must not exceed the deadline");
        let status: String =
            sqlx::query_scalar("SELECT status FROM artifact.generation_job_attempt WHERE id = $1")
                .bind(attempt_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "running", "a valid attempt survives the migration");
        let job_status: String = sqlx::query_scalar(
            "SELECT j.status FROM artifact.generation_job j \
             JOIN artifact.generation_job_attempt a ON a.job_id = j.id WHERE a.id = $1",
        )
        .bind(attempt_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(job_status, "running");
    }

    // 长租约被夹到 deadline（LEAST 生效）；短租约保持不变。
    let (long_lease, long_deadline): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT lease_expires_at, deadline_at FROM artifact.generation_job_attempt WHERE id = $1",
    )
    .bind(long_attempt)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        long_lease, long_deadline,
        "the 3600s lease must be clamped to the deadline"
    );
    let short_lease_after: DateTime<Utc> = sqlx::query_scalar(
        "SELECT lease_expires_at FROM artifact.generation_job_attempt WHERE id = $1",
    )
    .bind(short_attempt)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        short_lease_after, short_lease_before,
        "a lease already within the deadline must be untouched"
    );
}

/// 场景 B：迁移前已超过最大执行时长的活跃 Attempt（0002 语义下 Worker 一直 heartbeat，
/// 租约仍有效），迁移后由 recovery 立即按 GENERATION_TIMEOUT 收尾。
#[sqlx::test]
async fn migration_0003_expired_deadline_attempts_time_out_via_recovery(pool: PgPool) {
    let artifact = apply_0001_and_0002(&pool).await;
    let job = create_job_under_0002(&pool, artifact, 1).await;
    let (_, attempt_id, _) = claim_under_0002(&pool, "worker-a", 30).await;

    // started_at 在 10 分钟前、租约仍有效：deadline（默认 300 秒）已在过去。
    sqlx::query(
        "UPDATE artifact.generation_job_attempt \
         SET started_at = now() - interval '10 minutes', \
             lease_expires_at = now() + interval '30 minutes' \
         WHERE id = $1",
    )
    .bind(attempt_id)
    .execute(&pool)
    .await
    .unwrap();

    apply_0003(&pool).await;

    // 迁移把租约夹到已过期的 deadline：recovery 必须能立即处理。
    let (deadline, lease): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT deadline_at, lease_expires_at FROM artifact.generation_job_attempt WHERE id = $1",
    )
    .bind(attempt_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        deadline < Utc::now(),
        "fixture: the deadline must already be past"
    );
    assert_eq!(
        lease, deadline,
        "the lease must be clamped to the past deadline"
    );

    assert_eq!(coordination(&pool).recover_expired().await.unwrap(), 1);

    let (status, code): (String, Option<String>) =
        sqlx::query_as("SELECT status, error_code FROM artifact.generation_job WHERE id = $1")
            .bind(job)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "timed_out");
    assert_eq!(code.as_deref(), Some("GENERATION_TIMEOUT"));
    let attempt_status: String =
        sqlx::query_scalar("SELECT status FROM artifact.generation_job_attempt WHERE id = $1")
            .bind(attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let state = AttemptState::parse(&attempt_status).expect("valid attempt status");
    assert_eq!(state, AttemptState::TimedOut);
    assert!(!state.is_active(), "the attempt must no longer be active");
}

/// 场景 C：deadline 未到的历史 Attempt 可以正常续租但不越过 deadline；
/// deadline 过后不能再续租（heartbeat 返回 deadline_exceeded 并原地收尾为超时）。
#[sqlx::test]
async fn migration_0003_heartbeat_respects_backfilled_deadline(pool: PgPool) {
    let artifact = apply_0001_and_0002(&pool).await;
    let job = create_job_under_0002(&pool, artifact, 3).await;
    let (_, attempt_id, token) = claim_under_0002(&pool, "worker-a", 30).await;

    // 租约与 deadline 都在未来。
    sqlx::query(
        "UPDATE artifact.generation_job_attempt \
         SET started_at = now(), lease_expires_at = now() + interval '10 seconds' \
         WHERE id = $1",
    )
    .bind(attempt_id)
    .execute(&pool)
    .await
    .unwrap();

    apply_0003(&pool).await;
    let c = coordination(&pool);

    // deadline 之内正常续租。
    assert_eq!(
        c.heartbeat(job, attempt_id, &token, 30).await.unwrap(),
        HeartbeatOutcome::Renewed
    );
    let (lease, deadline): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT lease_expires_at, deadline_at FROM artifact.generation_job_attempt WHERE id = $1",
    )
    .bind(attempt_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(lease <= deadline);

    // 申请远超 deadline 的续租：被夹到 deadline，而不是无限延长。
    assert_eq!(
        c.heartbeat(job, attempt_id, &token, 3600).await.unwrap(),
        HeartbeatOutcome::Renewed
    );
    let (lease, deadline): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT lease_expires_at, deadline_at FROM artifact.generation_job_attempt WHERE id = $1",
    )
    .bind(attempt_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(lease, deadline, "renewal must be capped at the deadline");

    // deadline 过后不能再续租：heartbeat 不续租并原地收尾为超时。
    sqlx::query(
        "UPDATE artifact.generation_job_attempt \
         SET started_at = now() - interval '6 minutes', \
             deadline_at = now() - interval '1 second', \
             lease_expires_at = now() - interval '1 second' \
         WHERE id = $1",
    )
    .bind(attempt_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        c.heartbeat(job, attempt_id, &token, 30).await.unwrap(),
        HeartbeatOutcome::DeadlineExceeded
    );
    let attempt_status: String =
        sqlx::query_scalar("SELECT status FROM artifact.generation_job_attempt WHERE id = $1")
            .bind(attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempt_status, "timed_out");
    // Job 进入退避等待重试（max_attempts = 3）。
    let (job_status, code): (String, Option<String>) =
        sqlx::query_as("SELECT status, error_code FROM artifact.generation_job WHERE id = $1")
            .bind(job)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(job_status, "retry_wait");
    assert_eq!(code.as_deref(), Some("GENERATION_TIMEOUT"));
}

// ── AS2-44 / AS2-29：0005 的结构不变量，且不触碰 0003 语义 ─────────────────

/// 0005 只做加法：三列、配对/格式 CHECK、部分唯一索引、身份不可变触发器，
/// 并把创建函数升级为 16 参 + (job_id, resolution) 返回；0003 的
/// Idempotency-Key 唯一索引与清洗语义原样保留。
#[sqlx::test(migrations = "../migrations")]
async fn as2_44_migration_0005_structural_invariants(pool: PgPool) {
    // 1. 三列存在且类型正确。
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, data_type FROM information_schema.columns \
         WHERE table_schema='artifact' AND table_name='generation_job' \
           AND column_name IN ('caller_request_id','request_fingerprint','fingerprint_version') \
         ORDER BY column_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            ("caller_request_id".to_string(), "uuid".to_string()),
            ("fingerprint_version".to_string(), "smallint".to_string()),
            ("request_fingerprint".to_string(), "character".to_string()),
        ]
    );

    // 2. 配对/格式 CHECK 存在并覆盖 version>0 与 64 位小写十六进制。
    let check_def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conrelid='artifact.generation_job'::regclass \
           AND conname='generation_job_caller_identity_pairing_check'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(check_def.contains("fingerprint_version > 0"), "{check_def}");
    assert!(check_def.contains("[0-9a-f]{64}"), "{check_def}");

    // 3. 部分唯一索引：唯一 + 谓词仅限非 NULL。
    let (indexdef, is_unique): (String, bool) = sqlx::query_as(
        "SELECT indexdef, indisunique FROM pg_indexes i \
         JOIN pg_class c ON c.relname = i.indexname \
         JOIN pg_index x ON x.indexrelid = c.oid \
         WHERE i.schemaname='artifact' AND i.tablename='generation_job' \
           AND i.indexname='uq_generation_job_caller_request'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(is_unique);
    assert!(indexdef.contains("UNIQUE"), "{indexdef}");
    assert!(
        indexdef.contains("caller_request_id IS NOT NULL"),
        "{indexdef}"
    );

    // 4. 身份不可变由既有触发器函数承担（函数体包含三列不可变判定）。
    let guard_src: String = sqlx::query_scalar(
        "SELECT prosrc FROM pg_proc WHERE oid = 'artifact.guard_job_transition()'::regprocedure",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    for column in [
        "caller_request_id",
        "request_fingerprint",
        "fingerprint_version",
    ] {
        assert!(
            guard_src.contains(column),
            "guard_job_transition must enforce immutability of {column}"
        );
    }
    let trigger_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_trigger WHERE tgrelid='artifact.generation_job'::regclass \
           AND NOT tgisinternal",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        trigger_count >= 1,
        "the transition guard trigger must exist"
    );

    // 5. 创建函数 v2：16 参、(job_id uuid, resolution text)。
    let signature: String = sqlx::query_scalar(
        "SELECT pg_get_function_identity_arguments(oid) \
         FROM pg_proc WHERE oid = 'artifact.create_generation_job_with_input(\
         uuid,uuid,uuid,uuid,text,jsonb,jsonb,text,text,text,uuid,text,smallint,integer,integer,integer)'::regprocedure",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(signature.split(',').count(), 16, "{signature}");

    // 6. 旧 13 参签名必须不存在（避免两条并存路径）。
    let old_signature_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace \
         WHERE n.nspname='artifact' AND p.proname='create_generation_job_with_input' \
           AND pg_get_function_identity_arguments(p.oid) = \
               'uuid, uuid, uuid, uuid, text, jsonb, jsonb, text, text, text, integer, integer, integer')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        !old_signature_exists,
        "the 0003-era 13-arg function must be dropped"
    );

    // 7. AS2-29：0003 的 Idempotency-Key 唯一索引原样保留（0005 未触碰）。
    let key_index: Option<String> = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes WHERE schemaname='artifact' \
           AND tablename='generation_job' AND indexname='uq_generation_job_idempotency_key'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    let key_index = key_index.expect("0003 idempotency-key unique index must survive 0005");
    assert!(
        key_index.contains("idempotency_key IS NOT NULL"),
        "{key_index}"
    );

    // 8. 历史 Job 不回填：迁移前后 legacy 身份保持 NULL（新库无行，同样成立）。
    let backfilled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.generation_job WHERE caller_request_id IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(backfilled, 0);
}
