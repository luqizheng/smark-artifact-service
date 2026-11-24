//! ART-03B-01 Worker 协调（0002 + 0003）：真实 PostgreSQL 集成测试。
//!
//! 覆盖：并发 claim、能力匹配、NULL 租约防护、错误 token、迟到 complete、旧 attempt 隔离、
//! 崩溃恢复与退避、最大执行时长、cancel/complete 竞争、取消可见性、完成一致性、
//! 排队超时、审计与输入清理。全部通过 sqlx::test 在真实 PG 上执行，不用 mock 替代并发测试。

mod common;

use artifact_application::{
    AdapterCapabilities, CompleteOutcome, ConfirmCancelOutcome, FailOutcome, HeartbeatOutcome,
    MAX_DIAGNOSTIC_CHARS, WorkerCoordinationPort, run_coordination_maintenance,
};
use artifact_domain::{AttemptState, ClaimedJob, DomainError, JobState};
use artifact_infrastructure::postgres::PgWorkerCoordination;
use chrono::{DateTime, Utc};
use common::{PROJECT_1, TENANT};
use sqlx::Row;
use sqlx::postgres::PgPool;
use uuid::Uuid;

const ADAPTER: &str = "adapter-test-1";

fn caps() -> AdapterCapabilities {
    common::seecode_capabilities(ADAPTER)
}

fn coord(pool: &PgPool) -> PgWorkerCoordination {
    PgWorkerCoordination { pool: pool.clone() }
}

/// 测试布景：建 artifact、把 seecode 临时设为 available（仅测试库）、建带输入的 queued Job。
/// 返回 (job_id, artifact_id)。
async fn setup_job(pool: &PgPool, options: &common::JobOptions) -> (Uuid, Uuid) {
    let artifact = common::sql_insert_artifact(pool, TENANT, PROJECT_1, "coord").await;
    common::sql_enable_seecode(pool, ADAPTER).await;
    let job_id = common::sql_create_job(pool, TENANT, PROJECT_1, artifact, options).await;
    (job_id, artifact)
}

async fn claim(c: &PgWorkerCoordination, worker: &str) -> ClaimedJob {
    c.claim(worker, 30, &caps())
        .await
        .unwrap()
        .expect("a claimable job")
}

/// 模拟 Worker 在 deadline 之前失联：租约已过期，deadline 还在未来。
async fn lose_worker(pool: &PgPool, attempt_id: Uuid) {
    sqlx::query(
        "UPDATE artifact.generation_job_attempt \
         SET lease_expires_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(attempt_id)
    .execute(pool)
    .await
    .unwrap();
}

/// 退避结束。
async fn finish_backoff(pool: &PgPool, job_id: Uuid) {
    sqlx::query(
        "UPDATE artifact.generation_job \
         SET next_attempt_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(job_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn job_status(pool: &PgPool, job_id: Uuid) -> JobState {
    let status: String =
        sqlx::query_scalar("SELECT status FROM artifact.generation_job WHERE id = $1")
            .bind(job_id)
            .fetch_one(pool)
            .await
            .unwrap();
    match status.as_str() {
        "queued" => JobState::Queued,
        "running" => JobState::Running,
        "retry_wait" => JobState::RetryWait,
        "succeeded" => JobState::Succeeded,
        "failed" => JobState::Failed,
        "cancelled" => JobState::Cancelled,
        "timed_out" => JobState::TimedOut,
        other => panic!("unknown job status {other}"),
    }
}

async fn job_error_code(pool: &PgPool, job_id: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT error_code FROM artifact.generation_job WHERE id = $1")
        .bind(job_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn attempt_state(pool: &PgPool, attempt_id: Uuid) -> AttemptState {
    let status: String =
        sqlx::query_scalar("SELECT status FROM artifact.generation_job_attempt WHERE id = $1")
            .bind(attempt_id)
            .fetch_one(pool)
            .await
            .unwrap();
    AttemptState::parse(&status).expect("valid attempt status")
}

async fn request_cancel(pool: &PgPool, job_id: Uuid) -> String {
    sqlx::query_scalar("SELECT artifact.request_generation_job_cancel($1, $2, $3)")
        .bind(TENANT)
        .bind(PROJECT_1)
        .bind(job_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn audit_actions(pool: &PgPool, job_id: Uuid) -> Vec<String> {
    sqlx::query("SELECT action FROM artifact.audit_event WHERE object_id = $1 ORDER BY created_at")
        .bind(job_id)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get::<String, _>(0))
        .collect()
}

// 1：两个 Worker 并发 claim，不会领取同一有效 Attempt；有效 Attempt 唯一。
#[sqlx::test(migrations = "../migrations")]
async fn concurrent_claim_yields_one_active_attempt(pool: PgPool) {
    let (job_id, _) = setup_job(&pool, &common::JobOptions::default()).await;
    let (c1, c2) = (coord(&pool), coord(&pool));
    let (caps_a, caps_b) = (caps(), caps());
    let (r1, r2) = tokio::join!(
        c1.claim("worker-a", 30, &caps_a),
        c2.claim("worker-b", 30, &caps_b)
    );
    let (c1, c2) = (r1.unwrap(), r2.unwrap());
    assert_eq!(
        c1.is_some() as u8 + c2.is_some() as u8,
        1,
        "exactly one worker must claim the job"
    );
    let claimed = c1.or(c2).unwrap();
    assert_eq!(claimed.job_id, job_id);
    assert_eq!(claimed.attempt_number, 1);
    assert_eq!(claimed.lease_token.len(), 72);
    assert_eq!(claimed.adapter_version.as_deref(), Some(ADAPTER));
    assert_eq!(claimed.generator_version.as_deref(), Some("0.6.3-test"));
    assert_eq!(claimed.input_sha256.as_deref().map(str::len), Some(64));
    assert_eq!(claimed.input_payload, common::minimal_input());
    assert!(claimed.lease_expires_at <= claimed.deadline_at);

    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.generation_job_attempt \
         WHERE job_id = $1 AND status IN ('claimed', 'running')",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, 1);
}

// 2：claim 只把 Job 交给申报了相同 adapter 版本的 Worker。
#[sqlx::test(migrations = "../migrations")]
async fn claim_only_matches_advertised_adapter_version(pool: PgPool) {
    let (job_id, _) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);

    // 没有申报任何能力（例如没有 Adapter 的 Worker）：领不到 Job。
    let none = c
        .claim("no-adapter", 30, &AdapterCapabilities::new())
        .await
        .unwrap();
    assert!(none.is_none());
    // 申报了别的 adapter 版本：领不到 Job，重试中不会无声切换版本。
    let other = c
        .claim(
            "other-version",
            30,
            &common::seecode_capabilities("adapter-test-2"),
        )
        .await
        .unwrap();
    assert!(other.is_none());
    // 生成器被停用：即使版本匹配也领不到。
    sqlx::query(
        "UPDATE artifact.generator_registry SET status = 'disabled' WHERE generator_id = 'seecode'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let disabled = c.claim("w", 30, &caps()).await.unwrap();
    assert!(disabled.is_none());

    common::sql_enable_seecode(&pool, ADAPTER).await;
    let claimed = claim(&c, "matching-worker").await;
    assert_eq!(claimed.job_id, job_id);

    // 前面几次尝试没有留下任何 Attempt，也没有改变 Job。
    let attempts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.generation_job_attempt WHERE job_id = $1",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attempts, 1);
}

// 3：lease_seconds 非法（包括 NULL）不会产生永久 running 的 Job。
#[sqlx::test(migrations = "../migrations")]
async fn invalid_lease_seconds_never_leaves_a_stuck_job(pool: PgPool) {
    let (job_id, _) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);

    for bad in [0, 3601] {
        assert!(
            matches!(
                c.claim("w", bad, &caps()).await,
                Err(DomainError::InvalidRequest(_))
            ),
            "lease_seconds={bad}"
        );
    }
    assert!(matches!(
        c.claim("   ", 30, &caps()).await,
        Err(DomainError::InvalidRequest(_))
    ));

    // 绕过 Rust 直接调用数据库函数：NULL 同样被拒绝（0002 在这里会产生永久 running 的 Job）。
    let err = sqlx::query_scalar::<_, Uuid>(
        "SELECT job_id FROM artifact.claim_generation_job('w', NULL)",
    )
    .fetch_optional(&pool)
    .await
    .unwrap_err();
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("22023"));
    assert!(common::pg_error_message(&err).contains("INVALID_LEASE_SECONDS"));
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.generation_job_attempt")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        attempts, 0,
        "a rejected claim must not leave an attempt behind"
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::Queued);

    let claimed = claim(&c, "worker-a").await;
    let err = sqlx::query_scalar::<_, String>(
        "SELECT artifact.heartbeat_generation_job($1, $2, $3, NULL)",
    )
    .bind(job_id)
    .bind(claimed.attempt_id)
    .bind(&claimed.lease_token)
    .fetch_one(&pool)
    .await
    .unwrap_err();
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("22023"));
    // 租约完好：Worker 之后失联，Job 仍能被恢复扫描处理。
    let lease: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT lease_expires_at FROM artifact.generation_job_attempt WHERE id = $1",
    )
    .bind(claimed.attempt_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(lease.is_some());
    lose_worker(&pool, claimed.attempt_id).await;
    assert_eq!(c.recover_expired().await.unwrap(), 1);
    assert_eq!(job_status(&pool, job_id).await, JobState::RetryWait);

    // 数据库约束：活跃 Attempt 的租约不可为空，即使直接写表也不行。
    finish_backoff(&pool, job_id).await;
    let again = claim(&c, "worker-b").await;
    let err = sqlx::query(
        "UPDATE artifact.generation_job_attempt SET lease_expires_at = NULL WHERE id = $1",
    )
    .bind(again.attempt_id)
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("23514"));
}

// 4：Lease token 错误或为空时拒绝 Heartbeat。
#[sqlx::test(migrations = "../migrations")]
async fn wrong_lease_token_rejects_heartbeat(pool: PgPool) {
    let (job_id, _) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);
    let claimed = claim(&c, "worker-a").await;

    assert_eq!(
        c.heartbeat(job_id, claimed.attempt_id, "wrong-token", 30)
            .await
            .unwrap(),
        HeartbeatOutcome::Stale
    );
    assert_eq!(
        c.heartbeat(job_id, claimed.attempt_id, "", 30)
            .await
            .unwrap(),
        HeartbeatOutcome::Stale
    );
    assert_eq!(
        c.heartbeat(job_id, claimed.attempt_id, &claimed.lease_token, 30)
            .await
            .unwrap(),
        HeartbeatOutcome::Renewed
    );
    assert!(
        matches!(
            c.heartbeat(job_id, claimed.attempt_id, &claimed.lease_token, 0)
                .await,
            Err(DomainError::InvalidRequest(_))
        ),
        "lease_seconds is validated before reaching the database"
    );
}

// 5：Lease 过期后拒绝迟到 Complete，即使输出已经登记。
#[sqlx::test(migrations = "../migrations")]
async fn expired_lease_rejects_late_complete(pool: PgPool) {
    let (job_id, artifact) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);
    let claimed = claim(&c, "worker-a").await;

    let sha = common::sql_register_ready_output(&pool, TENANT, PROJECT_1, artifact, job_id).await;
    lose_worker(&pool, claimed.attempt_id).await;
    assert_eq!(
        c.complete(job_id, claimed.attempt_id, &claimed.lease_token, &sha)
            .await
            .unwrap(),
        CompleteOutcome::Stale
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::Running);
}

// 6：旧 Attempt 无法覆盖新 Attempt；每次重新领取创建新 Attempt。
#[sqlx::test(migrations = "../migrations")]
async fn stale_attempt_cannot_override_new_attempt(pool: PgPool) {
    let (job_id, artifact) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);
    let first = claim(&c, "worker-a").await;

    lose_worker(&pool, first.attempt_id).await;
    c.recover_expired().await.unwrap();
    assert_eq!(job_status(&pool, job_id).await, JobState::RetryWait);
    assert_eq!(
        attempt_state(&pool, first.attempt_id).await,
        AttemptState::LeaseExpired
    );

    // 旧 Worker 的迟到 heartbeat / fail / complete 一律无效。
    assert_eq!(
        c.heartbeat(job_id, first.attempt_id, &first.lease_token, 30)
            .await
            .unwrap(),
        HeartbeatOutcome::Stale
    );
    assert_eq!(
        c.fail(
            job_id,
            first.attempt_id,
            &first.lease_token,
            "GENERATION_TIMEOUT",
            None,
            true
        )
        .await
        .unwrap(),
        FailOutcome::StaleAttempt
    );
    assert_eq!(
        c.complete(
            job_id,
            first.attempt_id,
            &first.lease_token,
            &common::sha256_hex("stale")
        )
        .await
        .unwrap(),
        CompleteOutcome::Stale
    );

    finish_backoff(&pool, job_id).await;
    let second = claim(&c, "worker-b").await;
    assert_ne!(second.attempt_id, first.attempt_id);
    assert_eq!(second.attempt_number, 2);
    let sha = common::sql_register_ready_output(&pool, TENANT, PROJECT_1, artifact, job_id).await;
    assert_eq!(
        c.complete(job_id, second.attempt_id, &second.lease_token, &sha)
            .await
            .unwrap(),
        CompleteOutcome::Succeeded
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::Succeeded);
    assert_eq!(
        attempt_state(&pool, second.attempt_id).await,
        AttemptState::Succeeded
    );
}

// 7：Worker 崩溃（租约过期）后 Job 可恢复；恢复后进入退避，退避结束前不会被重新领取。
#[sqlx::test(migrations = "../migrations")]
async fn crashed_worker_waits_for_backoff_before_reclaim(pool: PgPool) {
    let (job_id, _) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);
    let first = claim(&c, "crashed-worker").await;

    lose_worker(&pool, first.attempt_id).await;
    assert_eq!(c.recover_expired().await.unwrap(), 1);
    assert_eq!(job_status(&pool, job_id).await, JobState::RetryWait);
    assert_eq!(
        attempt_state(&pool, first.attempt_id).await,
        AttemptState::LeaseExpired
    );
    assert_eq!(
        job_error_code(&pool, job_id).await.as_deref(),
        Some("LEASE_EXPIRED")
    );

    let next_attempt: DateTime<Utc> =
        sqlx::query_scalar("SELECT next_attempt_at FROM artifact.generation_job WHERE id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(next_attempt > Utc::now(), "retry must wait for backoff");
    assert!(
        c.claim("early-worker", 30, &caps())
            .await
            .unwrap()
            .is_none(),
        "a job in backoff must not be claimed"
    );

    // 恢复是幂等的：第二次扫描不再处理同一 attempt。
    assert_eq!(c.recover_expired().await.unwrap(), 0);

    // 退避结束后可以被再次领取（新 Attempt）。
    finish_backoff(&pool, job_id).await;
    let again = claim(&c, "new-worker").await;
    assert_eq!(again.attempt_number, 2);
    assert_ne!(again.attempt_id, first.attempt_id);
}

// 8：Worker 反复失联，attempt 达到上限后以 timed_out 收尾，不会无限重试。
#[sqlx::test(migrations = "../migrations")]
async fn repeated_crashes_stop_at_max_attempts(pool: PgPool) {
    let (job_id, _) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);

    for attempt_number in 1..=3 {
        if attempt_number > 1 {
            finish_backoff(&pool, job_id).await;
        }
        let claimed = claim(&c, "crashy").await;
        assert_eq!(claimed.attempt_number, attempt_number);
        lose_worker(&pool, claimed.attempt_id).await;
        assert_eq!(c.recover_expired().await.unwrap(), 1);
        assert_eq!(
            attempt_state(&pool, claimed.attempt_id).await,
            AttemptState::LeaseExpired
        );
    }

    assert_eq!(job_status(&pool, job_id).await, JobState::TimedOut);
    assert_eq!(
        job_error_code(&pool, job_id).await.as_deref(),
        Some("MAX_ATTEMPTS_EXCEEDED")
    );
    assert!(c.claim("worker-b", 30, &caps()).await.unwrap().is_none());
    assert_eq!(c.recover_expired().await.unwrap(), 0);
}

// 9：重试只针对白名单内的暂时性错误；Worker 自报 retryable 不能扩大范围。
#[sqlx::test(migrations = "../migrations")]
async fn retry_is_limited_to_allowlisted_error_codes(pool: PgPool) {
    let (job_id, _) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);
    let first = claim(&c, "worker-a").await;

    assert_eq!(
        c.fail(
            job_id,
            first.attempt_id,
            &first.lease_token,
            "STORAGE_UNAVAILABLE",
            None,
            true
        )
        .await
        .unwrap(),
        FailOutcome::RetryWait
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::RetryWait);

    finish_backoff(&pool, job_id).await;
    let second = claim(&c, "worker-b").await;
    // 非法错误码是参数错误，在到达数据库之前被拒绝。
    assert!(
        matches!(
            c.fail(
                job_id,
                second.attempt_id,
                &second.lease_token,
                "oops",
                None,
                false
            )
            .await,
            Err(DomainError::InvalidRequest(_))
        ),
        "error_code must be a stable machine code"
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::Running);

    assert_eq!(
        c.fail(
            job_id,
            second.attempt_id,
            &second.lease_token,
            "GENERATOR_EXECUTION_FAILED",
            None,
            true
        )
        .await
        .unwrap(),
        FailOutcome::Failed
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::Failed);
    assert_eq!(
        job_error_code(&pool, job_id).await.as_deref(),
        Some("GENERATOR_EXECUTION_FAILED")
    );
    assert!(
        c.claim("worker-c", 30, &caps()).await.unwrap().is_none(),
        "a terminal job is never claimed again"
    );
}

// 10：最大执行时长。租约被 deadline 截断；超过 deadline 后 heartbeat 不再续租，
//     Attempt 原地收尾为超时。
#[sqlx::test(migrations = "../migrations")]
async fn deadline_caps_the_lease_and_ends_the_attempt(pool: PgPool) {
    let (job_id, _) = setup_job(
        &pool,
        &common::JobOptions {
            max_execution_seconds: 60,
            ..common::JobOptions::default()
        },
    )
    .await;
    let c = coord(&pool);

    // 申请 3600 秒的租约，实际只拿到到 deadline 为止的 60 秒。
    let claimed = c.claim("worker-a", 3600, &caps()).await.unwrap().unwrap();
    assert!(
        (claimed.lease_expires_at - claimed.deadline_at)
            .num_milliseconds()
            .abs()
            < 2_000,
        "the lease must be capped by the attempt deadline"
    );
    // 时限之内续租不会越过 deadline。
    assert_eq!(
        c.heartbeat(job_id, claimed.attempt_id, &claimed.lease_token, 3600)
            .await
            .unwrap(),
        HeartbeatOutcome::Renewed
    );
    let (lease, deadline): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT lease_expires_at, deadline_at FROM artifact.generation_job_attempt WHERE id = $1",
    )
    .bind(claimed.attempt_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(lease <= deadline);

    // 时限已过：不再续租，Attempt 收尾为 timed_out，Job 进入退避。
    sqlx::query(
        "UPDATE artifact.generation_job_attempt \
         SET started_at = now() - interval '2 minutes', \
             deadline_at = now() - interval '1 minute', \
             lease_expires_at = now() - interval '1 minute' \
         WHERE id = $1",
    )
    .bind(claimed.attempt_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        c.heartbeat(job_id, claimed.attempt_id, &claimed.lease_token, 3600)
            .await
            .unwrap(),
        HeartbeatOutcome::DeadlineExceeded
    );
    assert_eq!(
        attempt_state(&pool, claimed.attempt_id).await,
        AttemptState::TimedOut
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::RetryWait);
    assert_eq!(
        job_error_code(&pool, job_id).await.as_deref(),
        Some("GENERATION_TIMEOUT")
    );

    // 超时之后的迟到 heartbeat 一律 stale。
    assert_eq!(
        c.heartbeat(job_id, claimed.attempt_id, &claimed.lease_token, 30)
            .await
            .unwrap(),
        HeartbeatOutcome::Stale
    );
}

// 11：Worker 自己上报的超时记为 timed_out，并在 attempt 耗尽后成为 Job 的终态。
#[sqlx::test(migrations = "../migrations")]
async fn worker_reported_timeout_is_recorded_as_timed_out(pool: PgPool) {
    let (job_id, _) = setup_job(
        &pool,
        &common::JobOptions {
            max_attempts: 2,
            ..common::JobOptions::default()
        },
    )
    .await;
    let c = coord(&pool);

    let first = claim(&c, "worker-a").await;
    assert_eq!(
        c.fail(
            job_id,
            first.attempt_id,
            &first.lease_token,
            "GENERATION_TIMEOUT",
            None,
            true
        )
        .await
        .unwrap(),
        FailOutcome::RetryWait
    );
    assert_eq!(
        attempt_state(&pool, first.attempt_id).await,
        AttemptState::TimedOut
    );

    finish_backoff(&pool, job_id).await;
    let second = claim(&c, "worker-b").await;
    assert_eq!(
        c.fail(
            job_id,
            second.attempt_id,
            &second.lease_token,
            "GENERATION_TIMEOUT",
            None,
            true
        )
        .await
        .unwrap(),
        FailOutcome::TimedOut
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::TimedOut);
    assert_eq!(
        job_error_code(&pool, job_id).await.as_deref(),
        Some("GENERATION_TIMEOUT")
    );
}

// 12：取消对运行中的 Worker 可见；结果一律是返回值而不是异常；
//     取消检查不先于租约校验（错误 token 得到 stale）。
#[sqlx::test(migrations = "../migrations")]
async fn cancel_is_visible_to_the_running_worker(pool: PgPool) {
    let (job_id, _) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);
    let claimed = claim(&c, "worker-a").await;

    assert_eq!(request_cancel(&pool, job_id).await, "cancel_requested");
    assert_eq!(
        c.heartbeat(job_id, claimed.attempt_id, &claimed.lease_token, 30)
            .await
            .unwrap(),
        HeartbeatOutcome::CancelRequested
    );

    // 租约已不属于调用方：即使取消待确认，也只会得到 stale。
    assert_eq!(
        c.fail(job_id, claimed.attempt_id, "wrong-token", "X", None, false)
            .await
            .unwrap(),
        FailOutcome::StaleAttempt
    );
    assert_eq!(
        c.confirm_cancelled(job_id, claimed.attempt_id, "wrong-token")
            .await
            .unwrap(),
        ConfirmCancelOutcome::Stale
    );

    // 持有有效租约的 Worker：三种上报都被告知取消，Job 仍是 running（不伪造已停止）。
    assert_eq!(
        c.fail(
            job_id,
            claimed.attempt_id,
            &claimed.lease_token,
            "GENERATOR_EXECUTION_FAILED",
            None,
            false
        )
        .await
        .unwrap(),
        FailOutcome::CancelRequested
    );
    assert_eq!(
        c.complete(
            job_id,
            claimed.attempt_id,
            &claimed.lease_token,
            &common::sha256_hex("svg")
        )
        .await
        .unwrap(),
        CompleteOutcome::CancelRequested
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::Running);

    // Worker 确认已停止后才进入终态。
    assert_eq!(
        c.confirm_cancelled(job_id, claimed.attempt_id, &claimed.lease_token)
            .await
            .unwrap(),
        ConfirmCancelOutcome::Cancelled
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::Cancelled);
    assert_eq!(
        job_error_code(&pool, job_id).await.as_deref(),
        Some("CANCELLED")
    );
    assert_eq!(
        attempt_state(&pool, claimed.attempt_id).await,
        AttemptState::Cancelled
    );
}

// 12b：没有取消请求时 confirm 不会把 Job 置为 cancelled。
#[sqlx::test(migrations = "../migrations")]
async fn confirm_cancel_without_a_request_is_refused(pool: PgPool) {
    let (job_id, _) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);
    let claimed = claim(&c, "worker-a").await;

    assert_eq!(
        c.confirm_cancelled(job_id, claimed.attempt_id, &claimed.lease_token)
            .await
            .unwrap(),
        ConfirmCancelOutcome::NotRequested
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::Running);
}

// 13：Cancel 与 Complete 竞争时只有一个终态获胜（事务裁决）。
#[sqlx::test(migrations = "../migrations")]
async fn cancel_complete_race_has_single_winner(pool: PgPool) {
    let c = coord(&pool);

    // 顺序 A：complete 先成功 → cancel 不覆盖 succeeded。
    let (job_a, artifact) = setup_job(&pool, &common::JobOptions::default()).await;
    let a = claim(&c, "worker-a").await;
    let sha = common::sql_register_ready_output(&pool, TENANT, PROJECT_1, artifact, job_a).await;
    assert_eq!(
        c.complete(job_a, a.attempt_id, &a.lease_token, &sha)
            .await
            .unwrap(),
        CompleteOutcome::Succeeded
    );
    assert_eq!(request_cancel(&pool, job_a).await, "terminal");
    assert_eq!(job_status(&pool, job_a).await, JobState::Succeeded);

    // 顺序 B：cancel 先登记 → complete 被告知取消，Worker 确认后置 cancelled。
    let (job_b, _) = setup_job(&pool, &common::JobOptions::default()).await;
    let b = claim(&c, "worker-b").await;
    assert_eq!(request_cancel(&pool, job_b).await, "cancel_requested");
    assert_eq!(
        c.complete(job_b, b.attempt_id, &b.lease_token, &sha)
            .await
            .unwrap(),
        CompleteOutcome::CancelRequested
    );
    assert_eq!(job_status(&pool, job_b).await, JobState::Running);
    assert_eq!(
        c.confirm_cancelled(job_b, b.attempt_id, &b.lease_token)
            .await
            .unwrap(),
        ConfirmCancelOutcome::Cancelled
    );
    assert_eq!(job_status(&pool, job_b).await, JobState::Cancelled);
}

// 14：真正并发的 cancel 与 complete（tokio::join!）由数据库事务裁决唯一结果，
//     不允许出现「Job succeeded 同时 Attempt 被取消」之类的撕裂状态。
#[sqlx::test(migrations = "../migrations")]
async fn concurrent_cancel_and_complete_yield_consistent_terminal(pool: PgPool) {
    let (job_id, artifact) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);
    let claimed = claim(&c, "worker-a").await;
    let sha = common::sql_register_ready_output(&pool, TENANT, PROJECT_1, artifact, job_id).await;

    let cancel_pool = pool.clone();
    let cancel_job = job_id;
    let cancel_fut = async move {
        sqlx::query_scalar::<_, String>("SELECT artifact.request_generation_job_cancel($1, $2, $3)")
            .bind(TENANT)
            .bind(PROJECT_1)
            .bind(cancel_job)
            .fetch_one(&cancel_pool)
            .await
            .unwrap()
    };
    let complete_job = job_id;
    let complete_fut = async move {
        c.complete(complete_job, claimed.attempt_id, &claimed.lease_token, &sha)
            .await
    };

    let (cancel_result, complete_result) = tokio::join!(cancel_fut, complete_fut);
    // 两种合法结果：complete 获胜（succeeded，cancel=no-op）；cancel 先登记（complete 被告知取消）。
    let status = job_status(&pool, job_id).await;
    let attempt = attempt_state(&pool, claimed.attempt_id).await;
    match status {
        JobState::Succeeded => {
            assert_eq!(cancel_result, "terminal");
            assert_eq!(complete_result.unwrap(), CompleteOutcome::Succeeded);
            assert_eq!(attempt, AttemptState::Succeeded);
        }
        JobState::Running => {
            assert_eq!(cancel_result, "cancel_requested");
            assert_eq!(complete_result.unwrap(), CompleteOutcome::CancelRequested);
            assert_eq!(attempt, AttemptState::Running);
        }
        other => panic!("unexpected race result: {other:?}"),
    }
}

// 15：取消请求登记后 Worker 失联：恢复扫描把 Job 收尾为 cancelled。
#[sqlx::test(migrations = "../migrations")]
async fn recovery_cancels_job_when_worker_dies_after_cancel_request(pool: PgPool) {
    let (job_id, _) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);
    let claimed = claim(&c, "worker-a").await;

    assert_eq!(request_cancel(&pool, job_id).await, "cancel_requested");
    lose_worker(&pool, claimed.attempt_id).await;
    assert_eq!(c.recover_expired().await.unwrap(), 1);

    assert_eq!(job_status(&pool, job_id).await, JobState::Cancelled);
    assert_eq!(
        job_error_code(&pool, job_id).await.as_deref(),
        Some("CANCELLED")
    );
    assert_eq!(
        attempt_state(&pool, claimed.attempt_id).await,
        AttemptState::LeaseExpired
    );
    // 取消不会重试。
    assert!(c.claim("worker-b", 30, &caps()).await.unwrap().is_none());
}

// 15b：Worker 一直存活到 deadline 后失联：恢复扫描按超时归类，而不是失联。
#[sqlx::test(migrations = "../migrations")]
async fn recovery_classifies_a_reached_deadline_as_timeout(pool: PgPool) {
    let (job_id, _) = setup_job(
        &pool,
        &common::JobOptions {
            max_attempts: 1,
            max_execution_seconds: 60,
            ..common::JobOptions::default()
        },
    )
    .await;
    let c = coord(&pool);
    let claimed = c.claim("worker-a", 3600, &caps()).await.unwrap().unwrap();

    sqlx::query(
        "UPDATE artifact.generation_job_attempt \
         SET started_at = now() - interval '2 minutes', \
             deadline_at = now() - interval '1 minute', \
             lease_expires_at = now() - interval '1 minute' \
         WHERE id = $1",
    )
    .bind(claimed.attempt_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(c.recover_expired().await.unwrap(), 1);

    assert_eq!(
        attempt_state(&pool, claimed.attempt_id).await,
        AttemptState::TimedOut
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::TimedOut);
    assert_eq!(
        job_error_code(&pool, job_id).await.as_deref(),
        Some("GENERATION_TIMEOUT")
    );
}

// 16：Job 只有在目标 Version 已 READY、且存在已通过安全检查的主 Output 时才能成功。
#[sqlx::test(migrations = "../migrations")]
async fn completion_requires_a_ready_version_with_passed_output(pool: PgPool) {
    let (job_id, artifact) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);
    let claimed = claim(&c, "worker-a").await;

    // 没有登记任何输出：拒绝，Job 保持 running。
    let err = c
        .complete(
            job_id,
            claimed.attempt_id,
            &claimed.lease_token,
            &common::sha256_hex("svg"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("OUTPUT_NOT_REGISTERED"), "{err}");
    assert_eq!(job_status(&pool, job_id).await, JobState::Running);
    assert_eq!(
        attempt_state(&pool, claimed.attempt_id).await,
        AttemptState::Running
    );

    let sha = common::sql_register_ready_output(&pool, TENANT, PROJECT_1, artifact, job_id).await;

    // 输出已登记，但上报的 sha256 与之不符：仍然拒绝。
    let err = c
        .complete(
            job_id,
            claimed.attempt_id,
            &claimed.lease_token,
            &common::sha256_hex("other"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("OUTPUT_NOT_REGISTERED"), "{err}");
    assert_eq!(job_status(&pool, job_id).await, JobState::Running);

    // 非法的 sha256 是参数错误。
    assert!(
        matches!(
            c.complete(
                job_id,
                claimed.attempt_id,
                &claimed.lease_token,
                "not-a-sha"
            )
            .await,
            Err(DomainError::InvalidRequest(_))
        ),
        "output_sha256 is validated before reaching the database"
    );

    assert_eq!(
        c.complete(job_id, claimed.attempt_id, &claimed.lease_token, &sha)
            .await
            .unwrap(),
        CompleteOutcome::Succeeded
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::Succeeded);
    assert_eq!(job_error_code(&pool, job_id).await, None);

    // 重复提交是空操作。
    assert_eq!(
        c.complete(job_id, claimed.attempt_id, &claimed.lease_token, &sha)
            .await
            .unwrap(),
        CompleteOutcome::Stale
    );
}

// 17：排队时限。超过 expires_at 仍未被领取的 Job 不会再被领取，并由恢复扫描收尾。
#[sqlx::test(migrations = "../migrations")]
async fn queue_ttl_cancels_unclaimed_job(pool: PgPool) {
    let (job_id, _) = setup_job(
        &pool,
        &common::JobOptions {
            queue_ttl_seconds: 1,
            ..common::JobOptions::default()
        },
    )
    .await;
    let c = coord(&pool);

    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    assert!(
        c.claim("worker-a", 30, &caps()).await.unwrap().is_none(),
        "an expired job must not be claimed"
    );
    assert_eq!(c.recover_expired().await.unwrap(), 1);
    assert_eq!(job_status(&pool, job_id).await, JobState::Cancelled);
    assert_eq!(
        job_error_code(&pool, job_id).await.as_deref(),
        Some("QUEUE_EXPIRED")
    );
    let input_expires: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT input_expires_at FROM artifact.generation_job WHERE id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        input_expires.is_some(),
        "a terminal job schedules its input purge"
    );
}

// 18：终态 7 天后清理输入正文，保留 hash / schema 版本与执行元数据；未终态的 Job 不受影响。
#[sqlx::test(migrations = "../migrations")]
async fn terminal_input_purged_after_retention(pool: PgPool) {
    let (job_id, artifact) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);
    let claimed = claim(&c, "worker-a").await;
    let sha = common::sql_register_ready_output(&pool, TENANT, PROJECT_1, artifact, job_id).await;
    c.complete(job_id, claimed.attempt_id, &claimed.lease_token, &sha)
        .await
        .unwrap();

    let before: (bool, bool) = sqlx::query_as(
        "SELECT input_payload IS NOT NULL, input_expires_at IS NOT NULL \
         FROM artifact.generation_job WHERE id = $1",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before, (true, true));

    // 另一个仍在排队的 Job：输入必须保留。
    let queued = common::sql_create_job(
        &pool,
        TENANT,
        PROJECT_1,
        artifact,
        &common::JobOptions::default(),
    )
    .await;

    sqlx::query(
        "UPDATE artifact.generation_job \
         SET input_expires_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(job_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(c.purge_expired_input().await.unwrap(), 1);

    let row = sqlx::query(
        "SELECT input_payload, options_payload, input_sha256, input_schema_version, status \
         FROM artifact.generation_job WHERE id = $1",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let input: Option<serde_json::Value> = row.get("input_payload");
    let options: Option<serde_json::Value> = row.get("options_payload");
    let hash: Option<String> = row.get("input_sha256");
    let schema_version: Option<String> = row.get("input_schema_version");
    assert!(input.is_none() && options.is_none());
    assert_eq!(hash.as_deref().map(str::len), Some(64));
    assert_eq!(schema_version.as_deref(), Some("1"));
    assert_eq!(row.get::<String, _>("status"), "succeeded");

    let queued_input: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT input_payload FROM artifact.generation_job WHERE id = $1")
            .bind(queued)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(queued_input, Some(common::minimal_input()));
}

// 19：过长的诊断摘要被截断，而不是让整次失败上报被拒绝。
#[sqlx::test(migrations = "../migrations")]
async fn long_diagnostic_is_truncated_not_rejected(pool: PgPool) {
    let (job_id, _) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);
    let claimed = claim(&c, "worker-a").await;

    let long = format!("{}\u{0007}", "x".repeat(MAX_DIAGNOSTIC_CHARS + 500));
    assert_eq!(
        c.fail(
            job_id,
            claimed.attempt_id,
            &claimed.lease_token,
            "STORAGE_UNAVAILABLE",
            Some(&long),
            true
        )
        .await
        .unwrap(),
        FailOutcome::RetryWait
    );
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT diagnostic_summary FROM artifact.generation_job_attempt WHERE id = $1",
    )
    .bind(claimed.attempt_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.unwrap().chars().count(), MAX_DIAGNOSTIC_CHARS);
    assert_eq!(
        attempt_state(&pool, claimed.attempt_id).await,
        AttemptState::Failed
    );
}

// 20：受控函数写审计；审计详情不含输入正文。
#[sqlx::test(migrations = "../migrations")]
async fn controlled_functions_write_audit_events(pool: PgPool) {
    let (job_id, artifact) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);

    let first = claim(&c, "worker-a").await;
    c.fail(
        job_id,
        first.attempt_id,
        &first.lease_token,
        "STORAGE_UNAVAILABLE",
        None,
        true,
    )
    .await
    .unwrap();
    finish_backoff(&pool, job_id).await;
    let second = claim(&c, "worker-b").await;
    let sha = common::sql_register_ready_output(&pool, TENANT, PROJECT_1, artifact, job_id).await;
    c.complete(job_id, second.attempt_id, &second.lease_token, &sha)
        .await
        .unwrap();

    assert_eq!(
        audit_actions(&pool, job_id).await,
        vec![
            "generation.retry".to_string(),
            "generation.complete".to_string()
        ]
    );
    let rows = sqlx::query(
        "SELECT outcome, error_code, actor_type, actor_subject, detail FROM artifact.audit_event \
         WHERE object_id = $1 ORDER BY created_at",
    )
    .bind(job_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows[0].get::<String, _>("outcome"), "error");
    assert_eq!(
        rows[0].get::<Option<String>, _>("error_code").as_deref(),
        Some("STORAGE_UNAVAILABLE")
    );
    assert_eq!(rows[0].get::<String, _>("actor_type"), "service");
    assert_eq!(
        rows[0].get::<Option<String>, _>("actor_subject").as_deref(),
        Some("worker-a")
    );
    assert_eq!(rows[1].get::<String, _>("outcome"), "allowed");
    assert_eq!(rows[1].get::<Option<String>, _>("error_code"), None);

    let leaks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.audit_event \
         WHERE detail::text LIKE '%smark.seecode.input%' \
            OR detail::text LIKE '%\"diagram\"%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(leaks, 0, "audit detail must not contain the input payload");
}

// 21：Job 创建固定 generator / adapter 版本、由数据库计算 input_sha256，并且是幂等的。
#[sqlx::test(migrations = "../migrations")]
async fn job_creation_pins_versions_and_is_idempotent(pool: PgPool) {
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "create").await;
    let options = common::JobOptions::default();

    // 生成器不可用（生产 seed）：不能入队。
    let err = common::try_sql_create_job_with(
        &pool,
        Uuid::new_v4(),
        TENANT,
        PROJECT_1,
        artifact,
        &options,
        None,
        &common::sha256_hex("r"),
    )
    .await
    .unwrap_err();
    assert!(
        common::pg_error_message(&err).contains("GENERATOR_UNAVAILABLE"),
        "{err}"
    );

    common::sql_enable_seecode(&pool, ADAPTER).await;
    let hash_a = common::sha256_hex("request-a");
    let (first, replayed) = common::sql_create_job_with(
        &pool,
        TENANT,
        PROJECT_1,
        artifact,
        &options,
        Some("key-1"),
        &hash_a,
    )
    .await;
    assert!(!replayed);

    let (generator_version, adapter_version, input_sha, max_execution): (
        Option<String>,
        Option<String>,
        Option<String>,
        i32,
    ) = sqlx::query_as(
        "SELECT generator_version, adapter_version, input_sha256, max_execution_seconds \
         FROM artifact.generation_job WHERE id = $1",
    )
    .bind(first)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(generator_version.as_deref(), Some("0.6.3-test"));
    assert_eq!(adapter_version.as_deref(), Some(ADAPTER));
    assert_eq!(input_sha.as_deref().map(str::len), Some(64));
    assert_ne!(
        input_sha.as_deref(),
        Some(hash_a.as_str()),
        "input_sha256 is computed by the database, not reported by the caller"
    );
    assert_eq!(max_execution, options.max_execution_seconds);

    // 同 key 同请求体：返回原 Job。
    let (again, replayed) = common::sql_create_job_with(
        &pool,
        TENANT,
        PROJECT_1,
        artifact,
        &options,
        Some("key-1"),
        &hash_a,
    )
    .await;
    assert!(replayed);
    assert_eq!(again, first);

    // 同 key 不同请求体：冲突。
    let err = common::try_sql_create_job_with(
        &pool,
        Uuid::new_v4(),
        TENANT,
        PROJECT_1,
        artifact,
        &options,
        Some("key-1"),
        &common::sha256_hex("request-b"),
    )
    .await
    .unwrap_err();
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("23505"));
    assert!(
        common::pg_error_message(&err).contains("IDEMPOTENCY_KEY_REUSED"),
        "{err}"
    );

    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.generation_job WHERE idempotency_key = 'key-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs, 1);

    // 空输入被拒绝，不会留下永远排不上队的 Job。
    let err = sqlx::query_as::<_, (Uuid, bool)>(
        "SELECT * FROM artifact.create_generation_job_with_input \
         ($1, $2, $3, $4, 'seecode', NULL, NULL, NULL, $5, NULL, 3, 300, 86400)",
    )
    .bind(Uuid::new_v4())
    .bind(TENANT)
    .bind(PROJECT_1)
    .bind(artifact)
    .bind(common::sha256_hex("r"))
    .fetch_one(&pool)
    .await
    .unwrap_err();
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("22023"));
}

// 22：协调维护步骤（Worker 每个维护周期执行一次）把恢复与清理串起来。
#[sqlx::test(migrations = "../migrations")]
async fn maintenance_step_runs_recovery_and_purge(pool: PgPool) {
    let (job_id, artifact) = setup_job(&pool, &common::JobOptions::default()).await;
    let c = coord(&pool);

    // 一个失联的 attempt。
    let claimed = claim(&c, "worker-a").await;
    lose_worker(&pool, claimed.attempt_id).await;
    // 一个过了保留期的终态 Job。
    let done = common::sql_create_job(
        &pool,
        TENANT,
        PROJECT_1,
        artifact,
        &common::JobOptions::default(),
    )
    .await;
    sqlx::query(
        "UPDATE artifact.generation_job \
         SET status = 'cancelled', completed_at = now() - interval '8 days', \
             input_expires_at = now() - interval '1 second', error_code = 'CANCELLED' \
         WHERE id = $1",
    )
    .bind(done)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(run_coordination_maintenance(&c).await.unwrap(), (1, 1));
    assert_eq!(job_status(&pool, job_id).await, JobState::RetryWait);
    let purged: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT input_payload FROM artifact.generation_job WHERE id = $1")
            .bind(done)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(purged.is_none());

    // 幂等：没有待处理项时返回 0。
    finish_backoff(&pool, job_id).await;
    let claimed_again = claim(&c, "worker-b").await;
    assert_eq!(run_coordination_maintenance(&c).await.unwrap(), (0, 0));
    assert_eq!(
        attempt_state(&pool, claimed_again.attempt_id).await,
        AttemptState::Running
    );
}
