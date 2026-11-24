//! ART-03B-01 Worker 协调基础：真实 PostgreSQL 集成测试。
//!
//! 覆盖：并发 claim、有效 attempt 唯一、错误 token、迟到 complete、旧 attempt
//! 隔离、崩溃恢复、最大 attempt、cancel/complete 竞争、running 取消语义、
//! 7 天输入清理。全部通过 sqlx::test 在真实 PG 上执行，不用 mock 替代并发测试。

mod common;

use artifact_application::{FailOutcome, WorkerCoordinationPort};
use artifact_domain::{AttemptState, JobState};
use artifact_infrastructure::postgres::PgWorkerCoordination;
use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::postgres::PgPool;
use uuid::Uuid;

const TENANT: Uuid = common::TENANT;
const PROJECT: Uuid = common::PROJECT_1;

fn sha256_hex(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// 测试布景：建 artifact、把 seecode 临时设为 available（仅测试库）、建带输入的 queued Job。
async fn setup_job(pool: &PgPool, max_attempts: i32) -> Uuid {
    let artifact = common::sql_insert_artifact(pool, TENANT, PROJECT, "coord").await;
    sqlx::query(
        "UPDATE artifact.generator_registry SET status='available' WHERE generator_id='seecode'",
    )
    .execute(pool)
    .await
    .unwrap();
    let job_id = Uuid::new_v4();
    let _: Uuid = sqlx::query_scalar(
        "SELECT artifact.create_generation_job_with_input($1,$2,$3,$4,'seecode', \
         '{\"schema\":\"smark.seecode.input\",\"schema_version\":\"1\",\"diagram\":{\"nodes\":[],\"edges\":[]}}'::jsonb, \
         '{}'::jsonb, '1', $5, $6, NULL, $7)",
    )
    .bind(job_id)
    .bind(TENANT)
    .bind(PROJECT)
    .bind(artifact)
    .bind(sha256_hex("input"))
    .bind(sha256_hex("request"))
    .bind(max_attempts)
    .fetch_one(pool)
    .await
    .unwrap();
    job_id
}

fn coord(pool: PgPool) -> PgWorkerCoordination {
    PgWorkerCoordination { pool }
}

async fn job_status(pool: &PgPool, job_id: Uuid) -> JobState {
    let s: String = sqlx::query_scalar("SELECT status FROM artifact.generation_job WHERE id=$1")
        .bind(job_id)
        .fetch_one(pool)
        .await
        .unwrap();
    match s.as_str() {
        "queued" => JobState::Queued,
        "running" => JobState::Running,
        "retry_wait" => JobState::RetryWait,
        "succeeded" => JobState::Succeeded,
        "failed" => JobState::Failed,
        "cancelled" => JobState::Cancelled,
        "timed_out" => JobState::TimedOut,
        _ => panic!("unknown status {s}"),
    }
}

async fn attempt_state(pool: &PgPool, attempt_id: Uuid) -> AttemptState {
    let s: String =
        sqlx::query_scalar("SELECT status FROM artifact.generation_job_attempt WHERE id=$1")
            .bind(attempt_id)
            .fetch_one(pool)
            .await
            .unwrap();
    AttemptState::parse(&s).expect("valid attempt state")
}

// 1 & 2：两个 Worker 并发 claim，不会领取同一有效 Attempt；有效 Attempt 唯一。
#[sqlx::test(migrations = "../migrations")]
async fn concurrent_claim_yields_one_active_attempt(pool: PgPool) {
    let job_id = setup_job(&pool, 3).await;
    let c1 = coord(pool.clone());
    let c2 = coord(pool.clone());

    let (r1, r2) = tokio::join!(c1.claim("worker-a", 30), c2.claim("worker-b", 30));
    let c1 = r1.unwrap();
    let c2 = r2.unwrap();
    assert_eq!(
        c1.is_some() as u8 + c2.is_some() as u8,
        1,
        "exactly one worker must claim the job"
    );
    let claimed = c1.or(c2).unwrap();
    assert_eq!(claimed.job_id, job_id);
    assert_eq!(claimed.attempt_number, 1);
    assert_eq!(claimed.lease_token.len(), 72);

    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.generation_job_attempt \
         WHERE job_id=$1 AND status IN ('claimed','running')",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, 1);
}

// 3：Lease token 错误时拒绝 Heartbeat。
#[sqlx::test(migrations = "../migrations")]
async fn wrong_lease_token_rejects_heartbeat(pool: PgPool) {
    let job_id = setup_job(&pool, 3).await;
    let c = coord(pool.clone());
    let claimed = c.claim("worker-a", 30).await.unwrap().unwrap();

    assert!(
        !c.heartbeat(job_id, claimed.attempt_id, "wrong-token", 30)
            .await
            .unwrap()
    );
    assert!(
        c.heartbeat(job_id, claimed.attempt_id, &claimed.lease_token, 30)
            .await
            .unwrap()
    );
}

// 4：Lease 过期后拒绝迟到 Complete。
#[sqlx::test(migrations = "../migrations")]
async fn expired_lease_rejects_late_complete(pool: PgPool) {
    let job_id = setup_job(&pool, 3).await;
    let c = coord(pool.clone());
    let claimed = c.claim("worker-a", 30).await.unwrap().unwrap();

    sqlx::query(
        "UPDATE artifact.generation_job_attempt \
         SET lease_expires_at = now() - interval '1 minute' WHERE id=$1",
    )
    .bind(claimed.attempt_id)
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        !c.complete(
            job_id,
            claimed.attempt_id,
            &claimed.lease_token,
            &sha256_hex("svg")
        )
        .await
        .unwrap()
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::Running);
}

// 5：旧 Attempt 无法覆盖新 Attempt；每次重新领取创建新 Attempt。
#[sqlx::test(migrations = "../migrations")]
async fn stale_attempt_cannot_override_new_attempt(pool: PgPool) {
    let job_id = setup_job(&pool, 3).await;
    let c = coord(pool.clone());
    let first = c.claim("worker-a", 30).await.unwrap().unwrap();

    sqlx::query(
        "UPDATE artifact.generation_job_attempt \
         SET lease_expires_at = now() - interval '1 minute' WHERE id=$1",
    )
    .bind(first.attempt_id)
    .execute(&pool)
    .await
    .unwrap();
    c.recover_expired().await.unwrap();
    assert_eq!(job_status(&pool, job_id).await, JobState::RetryWait);

    let second = c.claim("worker-b", 30).await.unwrap().unwrap();
    assert_ne!(second.attempt_id, first.attempt_id);
    assert_eq!(second.attempt_number, 2);
    assert_eq!(
        attempt_state(&pool, first.attempt_id).await,
        AttemptState::LeaseExpired
    );

    // 旧 attempt + 旧 token 完成 → false；新 attempt 成功完成。
    assert!(
        !c.complete(
            job_id,
            first.attempt_id,
            &first.lease_token,
            &sha256_hex("old")
        )
        .await
        .unwrap()
    );
    assert!(
        c.complete(
            job_id,
            second.attempt_id,
            &second.lease_token,
            &sha256_hex("new")
        )
        .await
        .unwrap()
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::Succeeded);
    assert_eq!(
        attempt_state(&pool, second.attempt_id).await,
        AttemptState::Succeeded
    );
}

// 6：Worker 崩溃（租约过期）后 Job 可恢复。
#[sqlx::test(migrations = "../migrations")]
async fn crashed_worker_job_is_recoverable(pool: PgPool) {
    let job_id = setup_job(&pool, 3).await;
    let c = coord(pool.clone());
    let claimed = c.claim("crashed-worker", 30).await.unwrap().unwrap();

    sqlx::query(
        "UPDATE artifact.generation_job_attempt \
         SET lease_expires_at = now() - interval '1 minute' WHERE id=$1",
    )
    .bind(claimed.attempt_id)
    .execute(&pool)
    .await
    .unwrap();

    let recovered = c.recover_expired().await.unwrap();
    assert_eq!(recovered, 1);
    assert_eq!(job_status(&pool, job_id).await, JobState::RetryWait);
    // 恢复是幂等的：第二次扫描不再处理同一 attempt。
    assert_eq!(c.recover_expired().await.unwrap(), 0);
    // 恢复后可以被再次领取（新 Attempt）。
    let again = c.claim("new-worker", 30).await.unwrap().unwrap();
    assert_eq!(again.attempt_number, 2);
}

// 7：超过最大 Attempt 次数后不会无限重试（终态 Job 不再 claim）。
#[sqlx::test(migrations = "../migrations")]
async fn max_attempts_stops_retry(pool: PgPool) {
    let job_id = setup_job(&pool, 1).await;
    let c = coord(pool.clone());
    let first = c.claim("worker-a", 30).await.unwrap().unwrap();

    sqlx::query(
        "UPDATE artifact.generation_job_attempt \
         SET lease_expires_at = now() - interval '1 minute' WHERE id=$1",
    )
    .bind(first.attempt_id)
    .execute(&pool)
    .await
    .unwrap();
    c.recover_expired().await.unwrap();

    assert_eq!(job_status(&pool, job_id).await, JobState::TimedOut);
    assert!(c.claim("worker-b", 30).await.unwrap().is_none());
    assert_eq!(c.recover_expired().await.unwrap(), 0);
}

// 8：Cancel 与 Complete 竞争时只有一个终态获胜（事务裁决）。
//    两种顺序分别验证：cancel 先登记则 complete 被拒；complete 先成功则 cancel 为 terminal no-op。
#[sqlx::test(migrations = "../migrations")]
async fn cancel_complete_race_has_single_winner(pool: PgPool) {
    // 顺序 A：complete 先成功 → cancel 不覆盖 succeeded。
    let job_a = setup_job(&pool, 3).await;
    let c = coord(pool.clone());
    let a = c.claim("worker-a", 30).await.unwrap().unwrap();
    assert!(
        c.complete(job_a, a.attempt_id, &a.lease_token, &sha256_hex("svg-a"))
            .await
            .unwrap()
    );
    let r: String = sqlx::query_scalar("SELECT artifact.request_generation_job_cancel($1)")
        .bind(job_a)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(r, "terminal");
    assert_eq!(job_status(&pool, job_a).await, JobState::Succeeded);

    // 顺序 B：cancel 先登记 → complete 抛 CANCEL_REQUESTED（由 DB 事务裁决）。
    let job_b = setup_job(&pool, 3).await;
    let b = c.claim("worker-b", 30).await.unwrap().unwrap();
    let r: String = sqlx::query_scalar("SELECT artifact.request_generation_job_cancel($1)")
        .bind(job_b)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(r, "cancel_requested");
    let err = c
        .complete(job_b, b.attempt_id, &b.lease_token, &sha256_hex("svg-b"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("CANCEL_REQUESTED"));
    assert_eq!(job_status(&pool, job_b).await, JobState::Running);
    // Worker 确认停止后才置 cancelled。
    assert!(
        c.confirm_cancelled(job_b, b.attempt_id, &b.lease_token)
            .await
            .unwrap()
    );
    assert_eq!(job_status(&pool, job_b).await, JobState::Cancelled);
}

// 8b：真正并发的 cancel 与 complete（tokio::join!）由数据库事务裁决唯一结果。
//     不允许出现「Job succeeded 同时 Attempt cancelled」或「两个终态」的撕裂状态。
#[sqlx::test(migrations = "../migrations")]
async fn concurrent_cancel_and_complete_yield_consistent_terminal(pool: PgPool) {
    let job_id = setup_job(&pool, 3).await;
    let c = coord(pool.clone());
    let claimed = c.claim("worker-a", 30).await.unwrap().unwrap();

    let cancel_pool = pool.clone();
    let cancel_job = job_id;
    let cancel_fut = async move {
        sqlx::query_scalar::<_, String>("SELECT artifact.request_generation_job_cancel($1)")
            .bind(cancel_job)
            .fetch_one(&cancel_pool)
            .await
            .unwrap()
    };
    let complete_job = job_id;
    let complete_fut = async move {
        c.complete(
            complete_job,
            claimed.attempt_id,
            &claimed.lease_token,
            &sha256_hex("svg"),
        )
        .await
    };

    let (cancel_result, complete_result) = tokio::join!(cancel_fut, complete_fut);
    // 两种合法结果：complete 获胜（succeeded，cancel=no-op）；cancel 先登记（complete 被拒）。
    let status = job_status(&pool, job_id).await;
    let attempt = attempt_state(&pool, claimed.attempt_id).await;
    match status {
        JobState::Succeeded => {
            assert_eq!(cancel_result, "terminal");
            assert!(complete_result.unwrap());
            assert_eq!(attempt, AttemptState::Succeeded);
        }
        JobState::Cancelled => {
            assert_eq!(cancel_result, "cancelled");
            assert!(complete_result.is_err());
            assert_eq!(attempt, AttemptState::Cancelled);
        }
        JobState::Running => {
            // cancel 先登记请求但 Worker 尚未确认：Job 仍 running，complete 被拒。
            assert_eq!(cancel_result, "cancel_requested");
            assert!(complete_result.is_err());
            assert_eq!(attempt, AttemptState::Running);
        }
        other => panic!("unexpected terminal race result: {other:?}"),
    }
}

// 9：running Job 的取消请求不会提前伪造 Worker 已停止。
#[sqlx::test(migrations = "../migrations")]
async fn running_cancel_does_not_fabricate_stop(pool: PgPool) {
    let job_id = setup_job(&pool, 3).await;
    let c = coord(pool.clone());
    let claimed = c.claim("worker-a", 30).await.unwrap().unwrap();

    let r: String = sqlx::query_scalar("SELECT artifact.request_generation_job_cancel($1)")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(r, "cancel_requested");

    let (status, cancel_set): (String, bool) = sqlx::query_as(
        "SELECT status, cancel_requested_at IS NOT NULL \
         FROM artifact.generation_job WHERE id=$1",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "running");
    assert!(cancel_set);
    assert_eq!(
        attempt_state(&pool, claimed.attempt_id).await,
        AttemptState::Running
    );
}

// 10：终态 7 天后清理输入正文，保留 hash/schema version 与执行元数据。
#[sqlx::test(migrations = "../migrations")]
async fn terminal_input_purged_after_retention(pool: PgPool) {
    let job_id = setup_job(&pool, 3).await;
    let c = coord(pool.clone());
    let claimed = c.claim("worker-a", 30).await.unwrap().unwrap();
    c.complete(
        job_id,
        claimed.attempt_id,
        &claimed.lease_token,
        &sha256_hex("svg"),
    )
    .await
    .unwrap();

    let before: (bool, bool) = sqlx::query_as(
        "SELECT input_payload IS NOT NULL, input_expires_at IS NOT NULL \
         FROM artifact.generation_job WHERE id=$1",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before, (true, true));

    sqlx::query(
        "UPDATE artifact.generation_job \
         SET input_expires_at = now() - interval '1 second' WHERE id=$1",
    )
    .bind(job_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(c.purge_expired_input().await.unwrap(), 1);

    let row = sqlx::query(
        "SELECT input_payload, options_payload, input_sha256, input_schema_version, status \
         FROM artifact.generation_job WHERE id=$1",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let input_null: Option<serde_json::Value> = row.get("input_payload");
    let options_null: Option<serde_json::Value> = row.get("options_payload");
    let hash: Option<String> = row.get("input_sha256");
    let version: Option<String> = row.get("input_schema_version");
    let status: String = row.get("status");
    assert!(input_null.is_none() && options_null.is_none());
    assert_eq!(
        hash.as_deref().map(str::trim),
        Some(sha256_hex("input").as_str())
    );
    assert_eq!(version.as_deref(), Some("1"));
    assert_eq!(status, "succeeded");
}

// Fail 语义：可重试 → retry_wait；不可重试 → failed（附带 attempt 上限验证）。
#[sqlx::test(migrations = "../migrations")]
async fn fail_retryable_vs_terminal(pool: PgPool) {
    let job_id = setup_job(&pool, 3).await;
    let c = coord(pool.clone());
    let claimed = c.claim("worker-a", 30).await.unwrap().unwrap();
    assert_eq!(
        c.fail(
            job_id,
            claimed.attempt_id,
            &claimed.lease_token,
            "FILE_SERVICE_TEMP",
            None,
            true,
        )
        .await
        .unwrap(),
        FailOutcome::RetryWait
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::RetryWait);

    let second = c.claim("worker-b", 30).await.unwrap().unwrap();
    assert_eq!(
        c.fail(
            job_id,
            second.attempt_id,
            &second.lease_token,
            "OUTPUT_REJECTED",
            None,
            false,
        )
        .await
        .unwrap(),
        FailOutcome::Failed
    );
    assert_eq!(job_status(&pool, job_id).await, JobState::Failed);
    assert!(c.claim("worker-c", 30).await.unwrap().is_none());
}
