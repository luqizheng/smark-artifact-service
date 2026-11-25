//! AS-2 GenerationJob Durable Caller Identity —— 真实 PostgreSQL 16 集成测试。
//!
//! 覆盖 ART-04G v3 §10.6 验收矩阵中所有必须在真实数据库上执行的项目：
//! 创建/重放、caller 与 Idempotency-Key 交叉冲突、并发仲裁、unique_violation
//! 异常块恢复、故障注入、迁移与权限不变量。并发与故障注入不使用 sleep 作为
//! 同步机制：屏障由显式事务的开启/提交顺序与 `pg_blocking_pids()` 可观测锁等待构成。

mod common;

use std::time::{Duration, Instant};

use artifact_application::port::keyed_job_create::{
    KeyedGenerationJobCreatePort, KeyedJobCreateError, KeyedJobCreateOutcome,
    ValidatedCreateGenerationJobRequest,
};
use artifact_domain::TenantScope;
use artifact_infrastructure::postgres::PgKeyedGenerationJobCreator;
use sqlx::postgres::PgPool;
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x0001);
const PROJECT: Uuid = Uuid::from_u128(0x0101);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x0002);
const OTHER_PROJECT: Uuid = Uuid::from_u128(0x0102);

const CALLER_A: Uuid = Uuid::from_u128(0x0a01);
const CALLER_B: Uuid = Uuid::from_u128(0x0b01);
const CALLER_C: Uuid = Uuid::from_u128(0x0c01);

fn fp_a() -> String {
    "a".repeat(64)
}
fn fp_b() -> String {
    "b".repeat(64)
}
fn fp_c() -> String {
    "c".repeat(64)
}

fn hash_x() -> String {
    common::sha256_hex("body-x")
}
fn hash_y() -> String {
    common::sha256_hex("body-y")
}

/// 一次创建调用的完整输入。`caller` 为 None 即 legacy 未键控语义。
#[derive(Clone)]
struct Spec<'a> {
    tenant: Uuid,
    project: Uuid,
    artifact: Uuid,
    caller: Option<Uuid>,
    fingerprint: Option<String>,
    version: Option<i16>,
    key: Option<&'a str>,
    request_hash: String,
}

impl<'a> Spec<'a> {
    fn caller(artifact: Uuid, caller: Uuid, fingerprint: &str) -> Self {
        Self {
            tenant: TENANT,
            project: PROJECT,
            artifact,
            caller: Some(caller),
            fingerprint: Some(fingerprint.to_string()),
            version: Some(1),
            key: None,
            request_hash: hash_x(),
        }
    }

    fn with_key(mut self, key: &'a str) -> Self {
        self.key = Some(key);
        self
    }

    fn with_hash(mut self, hash: String) -> Self {
        self.request_hash = hash;
        self
    }

    fn with_version(mut self, version: i16) -> Self {
        self.version = Some(version);
        self
    }

    fn unkeyed(artifact: Uuid) -> Self {
        Self {
            tenant: TENANT,
            project: PROJECT,
            artifact,
            caller: None,
            fingerprint: None,
            version: None,
            key: None,
            request_hash: hash_x(),
        }
    }
}

/// 直接调用受控创建函数 v2（16 参），返回 (job_id, resolution)。
async fn call_create<'e, E>(
    exec: E,
    job_id: Uuid,
    spec: &Spec<'_>,
) -> Result<(Uuid, String), sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as(
        "SELECT * FROM artifact.create_generation_job_with_input(\
         $1, $2, $3, $4, 'seecode', $5, '{}'::jsonb, '1', $6, $7, \
         $8, $9, $10, $11, $12, $13)",
    )
    .bind(job_id)
    .bind(spec.tenant)
    .bind(spec.project)
    .bind(spec.artifact)
    .bind(common::minimal_input())
    .bind(&spec.request_hash)
    .bind(spec.key)
    .bind(spec.caller)
    .bind(spec.fingerprint.clone())
    .bind(spec.version)
    .bind(3)
    .bind(300)
    .bind(86_400)
    .fetch_one(exec)
    .await
}

fn expect_code(err: &sqlx::Error, code: &str) {
    assert_eq!(
        common::pg_error_code(err).as_deref(),
        Some(code),
        "unexpected error: {err}"
    );
}

fn expect_token(err: &sqlx::Error, token: &str) {
    assert!(
        common::pg_error_message(err).contains(token),
        "expected token {token}, got {err}"
    );
}

async fn setup(pool: &PgPool) -> Uuid {
    common::sql_enable_seecode(pool, "seecode-v1").await;
    common::sql_insert_artifact(pool, TENANT, PROJECT, "as2").await
}

async fn job_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM artifact.generation_job")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn caller_count(pool: &PgPool, caller: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM artifact.generation_job \
         WHERE tenant_id = $1 AND project_id = $2 AND caller_request_id = $3",
    )
    .bind(TENANT)
    .bind(PROJECT)
    .bind(caller)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ── AS2-08：插入前异常回滚 ────────────────────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_08_validation_failure_leaves_no_residue(pool: PgPool) {
    let artifact = setup(&pool).await;
    let job_id = Uuid::new_v4();
    // input 为空 → 22023；不得留下任何 Job。
    let err = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT * FROM artifact.create_generation_job_with_input(\
         $1, $2, $3, $4, 'seecode', NULL, NULL, NULL, $5, NULL, \
         NULL::uuid, NULL::text, NULL::smallint, 3, 300, 86400)",
    )
    .bind(job_id)
    .bind(TENANT)
    .bind(PROJECT)
    .bind(artifact)
    .bind(hash_x())
    .fetch_one(&pool)
    .await
    .unwrap_err();
    expect_code(&err, "22023");
    assert_eq!(job_count(&pool).await, 0);

    // 同一个 job_id 重试合法请求：恰好创建一个 Job。
    let spec = Spec::caller(artifact, CALLER_A, &fp_a());
    let (created, resolution) = call_create(&pool, job_id, &spec).await.unwrap();
    assert_eq!((created, resolution.as_str()), (job_id, "created"));
    assert_eq!(job_count(&pool).await, 1);
}

// ── AS2-09：提交后响应丢失 → 同 ID 同指纹重放 ────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_09_replay_after_commit_returns_same_job(pool: PgPool) {
    let artifact = setup(&pool).await;
    let spec = Spec::caller(artifact, CALLER_A, &fp_a()).with_key("K1");
    let (first, resolution) = call_create(&pool, Uuid::new_v4(), &spec).await.unwrap();
    assert_eq!(resolution, "created");

    let (replayed, resolution) = call_create(&pool, Uuid::new_v4(), &spec).await.unwrap();
    assert_eq!(replayed, first, "replay must not create a new job id");
    assert_eq!(resolution, "caller_request_replay");
    assert_eq!(job_count(&pool).await, 1);
}

// ── AS2-10：创建事务中途 kill —— 三阶段故障注入（分别实现、分别断言） ─────
//
// 三个阶段独立测试（独立审查修复版）：
//   A. INSERT 前终止：以未提交的 SHARE 表锁作为可控注入点（函数此前的全部
//      SELECT 只需 ACCESS SHARE，不与 SHARE 冲突；INSERT 所需 ROW EXCLUSIVE
//      与 SHARE 冲突），目标 backend 阻塞在行写入开始之前，用
//      pg_terminate_backend 终止并等待 backend 真正退出。
//   B. INSERT 已执行、COMMIT 前终止：显式 BEGIN + 函数返回 'created' 证明
//      INSERT 已执行，不提交，由独立管理连接终止 backend。
//   C. COMMIT 成功后终止（响应丢失）：已提交数据必须完整持久化且可重放。
//
// 共同纪律：pg_terminate_backend() 返回 true 不等于事务已经结束——必须轮询
// pg_stat_activity 确认目标 backend 进程真正退出后，才以独立连接检查数据库
// 持久化状态；不凭客户端连接断开推断回滚。全部注入手段只存在于测试数据库，
// 不进入生产业务路径。

// ── 阶段 A：INSERT 前终止 ────────────────────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_10a_termination_before_insert_leaves_nothing(pool: PgPool) {
    let artifact = setup(&pool).await;
    let spec = Spec::caller(artifact, CALLER_A, &fp_a()).with_key("K10");
    let job_id = Uuid::new_v4();

    // 注入点：gate 连接持有 generation_job 的 SHARE 表锁（未提交）。
    let mut gate = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *gate).await.unwrap();
    sqlx::query("LOCK TABLE artifact.generation_job IN SHARE MODE")
        .execute(&mut *gate)
        .await
        .unwrap();
    let gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await
        .unwrap();

    // 目标 backend：调用创建函数，阻塞在 INSERT 获取 ROW EXCLUSIVE 表锁处。
    let mut victim = pool.acquire().await.unwrap();
    let (victim_pid, victim_started) = backend_identity(&mut victim).await;
    let spec_for_task = spec.clone();
    let handle =
        tokio::spawn(async move { call_create(&mut *victim, job_id, &spec_for_task).await });
    let evidence = wait_for_index_contention(&pool, victim_pid, gate_pid).await;
    assert!(
        evidence.blockers == vec![gate_pid],
        "the victim must be blocked solely by the SHARE lock holder, got {:?}",
        evidence.blockers
    );
    assert_eq!(
        evidence.wait_event_type.as_deref(),
        Some("Lock"),
        "the victim must be waiting on the table lock, got {evidence:?}"
    );
    assert_eq!(
        evidence.wait_event.as_deref(),
        Some("relation"),
        "a blocked table-lock acquisition must report the relation wait event, got {evidence:?}"
    );
    assert!(
        evidence.query.contains("create_generation_job_with_input"),
        "the victim must be blocked inside the real create statement, got {evidence:?}"
    );

    // 在 INSERT 前终止 backend；等待其真正退出后释放注入点。
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(victim_pid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(terminated, "the victim backend must be terminated");
    let victim_result = handle.await.unwrap();
    assert!(
        victim_result.is_err(),
        "the terminated session must fail its call: {victim_result:?}"
    );
    wait_backend_gone(&pool, victim_pid, victim_started).await;
    sqlx::query("ROLLBACK").execute(&mut *gate).await.unwrap();
    drop(gate);

    // 独立连接断言：Job、caller 身份、key 绑定都不存在。
    assert_eq!(
        job_count(&pool).await,
        0,
        "no job may survive a pre-insert kill"
    );
    assert_eq!(caller_count(&pool, CALLER_A).await, 0);
    let keyed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.generation_job WHERE idempotency_key = 'K10'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        keyed, 0,
        "no idempotency binding may survive a pre-insert kill"
    );
}

// ── 阶段 B：INSERT 已执行、COMMIT 前终止 ────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_10b_termination_before_commit_rolls_back(pool: PgPool) {
    let artifact = setup(&pool).await;
    let spec = Spec::caller(artifact, CALLER_A, &fp_a()).with_key("K11");
    let job_id = Uuid::new_v4();

    // 显式 BEGIN；函数返回 'created' 即证明 INSERT 已在该未提交事务内执行。
    let mut victim = pool.acquire().await.unwrap();
    let (victim_pid, victim_started) = backend_identity(&mut victim).await;
    sqlx::query("BEGIN").execute(&mut *victim).await.unwrap();
    let inserted = call_create(&mut *victim, job_id, &spec).await;
    let (inserted_id, resolution) = inserted.expect("the row must be inserted before the kill");
    assert_eq!((inserted_id, resolution.as_str()), (job_id, "created"));

    // 由独立管理连接终止 backend；等待 PostgreSQL 确认 backend 退出。
    // pg_terminate_backend 返回 true 不等于事务已结束，也不能凭客户端断开
    // 判断回滚——判断依据是 pg_stat_activity 中该 backend 消失。
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(victim_pid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(terminated, "the victim backend must be terminated");
    wait_backend_gone(&pool, victim_pid, victim_started).await;
    drop(victim);

    // 以数据库持久化状态证明回滚：Job、caller 身份、key 绑定均不存在。
    assert_eq!(job_count(&pool).await, 0, "no half-written job may survive");
    assert_eq!(caller_count(&pool, CALLER_A).await, 0);
    let keyed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.generation_job WHERE idempotency_key = 'K11'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(keyed, 0, "no idempotency binding may survive the rollback");

    // 重试收敛为恰好一个 Job。
    let (retried, resolution) = call_create(&pool, job_id, &spec).await.unwrap();
    assert_eq!((retried, resolution.as_str()), (job_id, "created"));
    assert_eq!(job_count(&pool).await, 1);
}

// ── 阶段 C：COMMIT 后终止（响应丢失） ───────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_10c_termination_after_commit_is_durable_and_replayable(pool: PgPool) {
    let artifact = setup(&pool).await;
    let spec = Spec::caller(artifact, CALLER_A, &fp_a()).with_key("K12");
    let job_id = Uuid::new_v4();

    // 创建 Job 并成功 COMMIT；随后终止客户端连接（模拟响应丢失）。
    let mut victim = pool.acquire().await.unwrap();
    let (victim_pid, victim_started) = backend_identity(&mut victim).await;
    sqlx::query("BEGIN").execute(&mut *victim).await.unwrap();
    let (created, resolution) = call_create(&mut *victim, job_id, &spec).await.unwrap();
    assert_eq!((created, resolution.as_str()), (job_id, "created"));
    sqlx::query("COMMIT").execute(&mut *victim).await.unwrap();
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(victim_pid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        terminated,
        "the committed session's backend must be terminated"
    );
    wait_backend_gone(&pool, victim_pid, victim_started).await;
    drop(victim);

    // 独立连接：已提交的 Job、caller 身份与 key 绑定完整持久化。
    let persisted: (Option<Uuid>, Option<String>) = sqlx::query_as(
        "SELECT caller_request_id, idempotency_key FROM artifact.generation_job WHERE id = $1",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        persisted,
        (Some(CALLER_A), Some("K12".to_string())),
        "a committed job must survive connection loss with its identity and key"
    );

    // 同 caller_request_id 与指纹重试：返回同一个 Job，不创建重复。
    let (replayed, resolution) = call_create(&pool, Uuid::new_v4(), &spec).await.unwrap();
    assert_eq!(
        (replayed, resolution.as_str()),
        (job_id, "caller_request_replay")
    );
    assert_eq!(
        job_count(&pool).await,
        1,
        "the retry must not duplicate the job"
    );
}

// ── AS2-11：Idempotency-Key 已在其他 Job 上使用（不同 hash） ─────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_11_key_bound_to_other_job_rejects_without_identity(pool: PgPool) {
    let artifact = setup(&pool).await;
    // Job B：无 caller，绑定 K2 + hash_y。
    let b = Spec::unkeyed(artifact).with_key("K2").with_hash(hash_y());
    call_create(&pool, Uuid::new_v4(), &b).await.unwrap();

    // caller A + 相同 K2 + 不同 raw hash → 409，且 caller 身份不被创建。
    let a = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K2")
        .with_hash(hash_x());
    let err = call_create(&pool, Uuid::new_v4(), &a).await.unwrap_err();
    expect_code(&err, "23505");
    expect_token(&err, "IDEMPOTENCY_KEY_REUSED");
    assert_eq!(caller_count(&pool, CALLER_A).await, 0);
    assert_eq!(job_count(&pool).await, 1, "no partial commit");
}

// ── AS2-12：generator unavailable 时创建 → 无残留；恢复后成功 ────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_12_generator_unavailable_leaves_no_residue(pool: PgPool) {
    let artifact = setup(&pool).await;
    sqlx::query(
        "UPDATE artifact.generator_registry SET status='disabled' WHERE generator_id='seecode'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let spec = Spec::caller(artifact, CALLER_A, &fp_a());
    let job_id = Uuid::new_v4();
    let err = call_create(&pool, job_id, &spec).await.unwrap_err();
    expect_code(&err, "P0001");
    expect_token(&err, "GENERATOR_UNAVAILABLE");
    assert_eq!(job_count(&pool).await, 0);
    assert_eq!(caller_count(&pool, CALLER_A).await, 0);

    common::sql_enable_seecode(&pool, "seecode-v1").await;
    let (created, resolution) = call_create(&pool, job_id, &spec).await.unwrap();
    assert_eq!((created, resolution.as_str()), (job_id, "created"));
}

// ── AS2-13：generator 被禁用后，重放路径仍返回 caller_request_replay ─────

#[sqlx::test(migrations = "../migrations")]
async fn as2_13_replay_survives_generator_disabled(pool: PgPool) {
    let artifact = setup(&pool).await;
    let spec = Spec::caller(artifact, CALLER_A, &fp_a());
    let (first, _) = call_create(&pool, Uuid::new_v4(), &spec).await.unwrap();
    sqlx::query(
        "UPDATE artifact.generator_registry SET status='disabled' WHERE generator_id='seecode'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (replayed, resolution) = call_create(&pool, Uuid::new_v4(), &spec).await.unwrap();
    assert_eq!(replayed, first);
    assert_eq!(
        resolution, "caller_request_replay",
        "replay gates precede generator availability (frozen order)"
    );
}

// ── AS2-14：N 并发同 caller 同指纹 → 恰好 1 行 ───────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_14_concurrent_same_caller_yields_single_job(pool: PgPool) {
    let artifact = setup(&pool).await;
    let mut handles = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        let spec = Spec::caller(artifact, CALLER_A, &fp_a());
        handles.push(tokio::spawn(async move {
            call_create(&pool, Uuid::new_v4(), &spec).await
        }));
    }
    let mut ids = Vec::new();
    let mut resolutions = Vec::new();
    for handle in handles {
        let (id, resolution) = handle.await.unwrap().expect("no error under same identity");
        assert!(
            resolution == "created" || resolution == "caller_request_replay",
            "unexpected resolution {resolution}"
        );
        ids.push(id);
        resolutions.push(resolution);
    }
    ids.dedup();
    assert_eq!(ids.len(), 1, "all successful parties must share one job id");
    assert_eq!(resolutions.iter().filter(|r| *r == "created").count(), 1);
    assert_eq!(job_count(&pool).await, 1);
}

// ── AS2-15：并发同 caller 不同指纹 → 1 胜者 + 其余 409 ───────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_15_concurrent_same_caller_different_fingerprint(pool: PgPool) {
    let artifact = setup(&pool).await;
    let spec_a = Spec::caller(artifact, CALLER_A, &fp_a());
    let spec_b = Spec::caller(artifact, CALLER_A, &fp_b());
    let (r1, r2) = tokio::join!(
        call_create(&pool, Uuid::new_v4(), &spec_a),
        call_create(&pool, Uuid::new_v4(), &spec_b),
    );
    let successes = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
    assert_eq!(successes, 1, "exactly one fingerprint may win");
    let loser = if r1.is_err() { r1 } else { r2 };
    let err = loser.unwrap_err();
    expect_code(&err, "23505");
    expect_token(&err, "CALLER_REQUEST_ID_REUSED");
    assert_eq!(job_count(&pool).await, 1);
}

// ── AS2-16：并发同 key 不同 caller → 1 胜者 + 409 ────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_16_concurrent_same_key_different_caller(pool: PgPool) {
    let artifact = setup(&pool).await;
    let a = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K-race")
        .with_hash(hash_x());
    let b = Spec::caller(artifact, CALLER_B, &fp_b())
        .with_key("K-race")
        .with_hash(hash_y());
    let (r1, r2) = tokio::join!(
        call_create(&pool, Uuid::new_v4(), &a),
        call_create(&pool, Uuid::new_v4(), &b),
    );
    assert_eq!(
        [&r1, &r2].iter().filter(|r| r.is_ok()).count(),
        1,
        "an Idempotency-Key binds exactly one job"
    );
    let err = if r1.is_err() { r1 } else { r2 }.unwrap_err();
    expect_code(&err, "23505");
    expect_token(&err, "IDEMPOTENCY_KEY_REUSED");
    assert_eq!(job_count(&pool).await, 1);
}

// ── AS2-17/18：双索引竞争与异常块不重用失败子事务 ────────────────────────
// 两个请求同 caller 同指纹同 key 并发：caller 索引与 key 索引同时竞争。
// 异常块必须先按 caller 解析；任一失败方必须在新语句快照上重读，不得 500 或重复。

#[sqlx::test(migrations = "../migrations")]
async fn as2_17_dual_index_race_resolves_without_500(pool: PgPool) {
    let artifact = setup(&pool).await;
    let spec = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K-dual")
        .with_hash(hash_x());
    let (r1, r2) = tokio::join!(
        call_create(&pool, Uuid::new_v4(), &spec),
        call_create(&pool, Uuid::new_v4(), &spec),
    );
    let (id1, res1) = r1.expect("no 500-class error under dual index race");
    let (id2, res2) = r2.expect("no 500-class error under dual index race");
    assert_eq!(id1, id2, "both must resolve to the same job");
    assert_eq!(res1, "created");
    assert_eq!(res2, "caller_request_replay");
    assert_eq!(job_count(&pool).await, 1);
}

// ── AS2-19：40001/40P01 注入 → 端口有界重试后 503，不误报冲突 ────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_19_transient_failure_retried_then_503(pool: PgPool) {
    let artifact = setup(&pool).await;
    // 故障注入：Job 插入前累计调用次数并抛出 serialization_failure（40001）。
    // 计数用 sequence（非事务性），否则 40001 回滚会把计数一起抹掉。
    sqlx::query("CREATE SEQUENCE artifact.as2_fault_seq")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE FUNCTION artifact.as2_inject_40001() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
           PERFORM nextval('artifact.as2_fault_seq'); \
           RAISE EXCEPTION 'injected serialization failure' USING ERRCODE = '40001'; \
         END $$",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER as2_inject BEFORE INSERT ON artifact.generation_job \
         FOR EACH ROW EXECUTE FUNCTION artifact.as2_inject_40001()",
    )
    .execute(&pool)
    .await
    .unwrap();

    let creator = PgKeyedGenerationJobCreator { pool: pool.clone() };
    let request = validated(&artifact, Some(CALLER_A));
    let err = creator
        .create_or_resolve_keyed_job(request, hash_x(), None)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        KeyedJobCreateError::RetryablePersistenceFailure(
            "transient persistence conflict (40001)".into()
        )
    );
    let attempts: i64 = sqlx::query_scalar("SELECT last_value FROM artifact.as2_fault_seq")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 3, "the port must retry a bounded number of times");

    // 移除故障后同一端口调用成功，证明未误报为业务冲突。
    sqlx::query("DROP TRIGGER as2_inject ON artifact.generation_job")
        .execute(&pool)
        .await
        .unwrap();
    let outcome = creator
        .create_or_resolve_keyed_job(validated(&artifact, Some(CALLER_A)), hash_x(), None)
        .await
        .unwrap();
    assert!(matches!(outcome, KeyedJobCreateOutcome::Created(_)));
}

// ── AS2-20：同 caller_request_id 不同 tenant/project → 各自独立 ──────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_20_same_caller_isolated_across_scopes(pool: PgPool) {
    let artifact_a = setup(&pool).await;
    let artifact_b = common::sql_insert_artifact(&pool, OTHER_TENANT, OTHER_PROJECT, "as2-b").await;

    let mut spec_b = Spec::caller(artifact_b, CALLER_A, &fp_a());
    spec_b.tenant = OTHER_TENANT;
    spec_b.project = OTHER_PROJECT;

    let (id_a, _) = call_create(
        &pool,
        Uuid::new_v4(),
        &Spec::caller(artifact_a, CALLER_A, &fp_a()),
    )
    .await
    .unwrap();
    let (id_b, _) = call_create(&pool, Uuid::new_v4(), &spec_b).await.unwrap();
    assert_ne!(id_a, id_b, "scopes must resolve to independent jobs");

    // 仓储查询同样按 scope 隔离。
    use artifact_application::port::repo::JobRepository;
    let repo = artifact_infrastructure::postgres::PgJobRepository { pool: pool.clone() };
    let found_b = repo
        .get_by_caller_request((OTHER_TENANT, OTHER_PROJECT), CALLER_A)
        .await
        .unwrap()
        .expect("job must exist in its own scope");
    assert_eq!(found_b.id, id_b);
}

// ── AS2-25：unkeyed 语义与 0003 逐项一致 ────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_25_unkeyed_semantics_preserved(pool: PgPool) {
    let artifact = setup(&pool).await;
    // 无 key 无 caller → 正常创建。
    let (first, res) = call_create(&pool, Uuid::new_v4(), &Spec::unkeyed(artifact))
        .await
        .unwrap();
    assert_eq!(res, "created");
    // 再次无 key → 新 Job。
    let (second, res) = call_create(&pool, Uuid::new_v4(), &Spec::unkeyed(artifact))
        .await
        .unwrap();
    assert_eq!(res, "created");
    assert_ne!(first, second);

    // 同 key 同 hash → idempotency_replay。
    let keyed = Spec::unkeyed(artifact).with_key("K9");
    let (k1, res) = call_create(&pool, Uuid::new_v4(), &keyed).await.unwrap();
    assert_eq!(res, "created");
    let (k2, res) = call_create(&pool, Uuid::new_v4(), &keyed).await.unwrap();
    assert_eq!((k2, res.as_str()), (k1, "idempotency_replay"));

    // 同 key 不同 hash → 409。
    let err = call_create(&pool, Uuid::new_v4(), &keyed.clone().with_hash(hash_y()))
        .await
        .unwrap_err();
    expect_code(&err, "23505");
    expect_token(&err, "IDEMPOTENCY_KEY_REUSED");
    // 身份三列保持 NULL。
    let identity: (Option<Uuid>, Option<String>, Option<i16>) = sqlx::query_as(
        "SELECT caller_request_id, request_fingerprint, fingerprint_version \
         FROM artifact.generation_job WHERE id = $1",
    )
    .bind(k1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(identity, (None, None, None));
}

// ── AS2-26：历史 Job（NULL 身份）经 lookup → 404（None） ─────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_26_legacy_job_lookup_returns_none(pool: PgPool) {
    let artifact = setup(&pool).await;
    let (legacy, _) = call_create(&pool, Uuid::new_v4(), &Spec::unkeyed(artifact))
        .await
        .unwrap();
    use artifact_application::port::repo::JobRepository;
    let repo = artifact_infrastructure::postgres::PgJobRepository { pool: pool.clone() };
    assert!(
        repo.get_by_caller_request((TENANT, PROJECT), CALLER_A)
            .await
            .unwrap()
            .is_none()
    );
    // 按 job id 仍可读，身份为 null。
    let job = repo.get((TENANT, PROJECT), legacy).await.unwrap();
    assert!(job.caller_request_id.is_none());
}

// ── AS2-27：重放携带新 key → 原 Job 的 key/hash 不被改写 ────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_27_replay_with_new_key_does_not_rewrite(pool: PgPool) {
    let artifact = setup(&pool).await;
    let spec = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K1")
        .with_hash(hash_x());
    let (first, _) = call_create(&pool, Uuid::new_v4(), &spec).await.unwrap();

    let replay = spec.clone().with_key("K2").with_hash(hash_y());
    let (again, resolution) = call_create(&pool, Uuid::new_v4(), &replay).await.unwrap();
    assert_eq!(again, first);
    assert_eq!(resolution, "caller_request_replay");

    let (key, hash): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT idempotency_key, request_hash FROM artifact.generation_job WHERE id = $1",
    )
    .bind(first)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(key.as_deref(), Some("K1"));
    assert_eq!(hash.as_deref(), Some(hash_x().as_str()));
}

// ── AS2-28a：caller 未命中 + 同 key 不同 hash → 409 ──────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_28a_unkeyed_key_reuse_with_different_hash(pool: PgPool) {
    let artifact = setup(&pool).await;
    let b = Spec::unkeyed(artifact).with_key("K2").with_hash(hash_y());
    call_create(&pool, Uuid::new_v4(), &b).await.unwrap();
    let a = Spec::unkeyed(artifact).with_key("K2").with_hash(hash_x());
    let err = call_create(&pool, Uuid::new_v4(), &a).await.unwrap_err();
    expect_code(&err, "23505");
    expect_token(&err, "IDEMPOTENCY_KEY_REUSED");
}

// ── AS2-28b：caller 命中 + 指纹一致 + key 绑定他 Job + hash 不同 → 409 ──
// 这是本轮挑战评审的核心风险：caller 命中不得绕过 Idempotency-Key 冲突检查。

#[sqlx::test(migrations = "../migrations")]
async fn as2_28b_caller_hit_is_not_allowed_to_bypass_key_conflict(pool: PgPool) {
    let artifact = setup(&pool).await;
    // Job A：caller A + K1。
    let a = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K1")
        .with_hash(hash_x());
    let (job_a, _) = call_create(&pool, Uuid::new_v4(), &a).await.unwrap();
    // Job B：caller B + K2。
    let b = Spec::caller(artifact, CALLER_B, &fp_b())
        .with_key("K2")
        .with_hash(hash_y());
    let (job_b, _) = call_create(&pool, Uuid::new_v4(), &b).await.unwrap();

    // 请求：caller A + 相同指纹 + 携带 K2（绑定 B）+ raw hash 与 B 不同。
    let cross = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K2")
        .with_hash(hash_x());
    let err = call_create(&pool, Uuid::new_v4(), &cross)
        .await
        .unwrap_err();
    expect_code(&err, "23505");
    expect_token(&err, "IDEMPOTENCY_KEY_REUSED");
    assert_eq!(job_count(&pool).await, 2, "no new job may be created");

    // A / B 的 key、hash、指纹全部不变。
    let a_row: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT idempotency_key, request_hash, request_fingerprint::text \
         FROM artifact.generation_job WHERE id = $1",
    )
    .bind(job_a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(a_row.0.as_deref(), Some("K1"));
    assert_eq!(a_row.1.as_deref(), Some(hash_x().as_str()));
    assert_eq!(a_row.2.as_deref(), Some(fp_a().as_str()));
    let b_row: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT idempotency_key, request_hash FROM artifact.generation_job WHERE id = $1",
    )
    .bind(job_b)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(b_row.0.as_deref(), Some("K2"));
    assert_eq!(b_row.1.as_deref(), Some(hash_y().as_str()));
}

// ── AS2-30：终态输入正文清理后重放（指纹来自存储列，不依赖正文） ────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_30_replay_after_input_purge(pool: PgPool) {
    let artifact = setup(&pool).await;
    let spec = Spec::caller(artifact, CALLER_A, &fp_a());
    let (first, _) = call_create(&pool, Uuid::new_v4(), &spec).await.unwrap();
    sqlx::query(
        "UPDATE artifact.generation_job \
         SET status='cancelled', completed_at=clock_timestamp(), \
             input_expires_at=clock_timestamp() - interval '1 day' \
         WHERE id=$1",
    )
    .bind(first)
    .execute(&pool)
    .await
    .unwrap();
    let _: i32 = sqlx::query_scalar("SELECT artifact.purge_expired_generation_job_input()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let payload_is_null: bool =
        sqlx::query_scalar("SELECT input_payload IS NULL FROM artifact.generation_job WHERE id=$1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(payload_is_null, "terminal job input must be purged");

    let (replayed, resolution) = call_create(&pool, Uuid::new_v4(), &spec).await.unwrap();
    assert_eq!(
        (replayed, resolution.as_str()),
        (first, "caller_request_replay")
    );
}

// ── AS2-32：终态 Job 身份保留且仍可解析 ─────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_32_terminal_job_identity_preserved(pool: PgPool) {
    let artifact = setup(&pool).await;
    let spec = Spec::caller(artifact, CALLER_A, &fp_a());
    let (first, _) = call_create(&pool, Uuid::new_v4(), &spec).await.unwrap();
    sqlx::query(
        "UPDATE artifact.generation_job \
         SET status='cancelled', completed_at=clock_timestamp() WHERE id=$1",
    )
    .bind(first)
    .execute(&pool)
    .await
    .unwrap();
    let (again, resolution) = call_create(&pool, Uuid::new_v4(), &spec).await.unwrap();
    assert_eq!(again, first);
    assert_eq!(resolution, "caller_request_replay");
    let identity: Option<Uuid> =
        sqlx::query_scalar("SELECT caller_request_id FROM artifact.generation_job WHERE id=$1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(identity, Some(CALLER_A));
}

// ── AS2-33：直接 UPDATE 身份列 → 触发器拒绝 ─────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_33_identity_columns_are_immutable(pool: PgPool) {
    let artifact = setup(&pool).await;
    let (job, _) = call_create(
        &pool,
        Uuid::new_v4(),
        &Spec::caller(artifact, CALLER_A, &fp_a()),
    )
    .await
    .unwrap();
    for statement in [
        "UPDATE artifact.generation_job SET request_fingerprint = repeat('9',64) WHERE id=$1",
        "UPDATE artifact.generation_job SET fingerprint_version = 2 WHERE id=$1",
        "UPDATE artifact.generation_job SET caller_request_id = gen_random_uuid() WHERE id=$1",
    ] {
        let err = sqlx::query(statement)
            .bind(job)
            .execute(&pool)
            .await
            .unwrap_err();
        expect_code(&err, "P0001");
        expect_token(&err, "immutable");
    }
}

// ── AS2-34：NULL → 非 NULL 身份补录 → 触发器拒绝 ────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_34_null_to_non_null_identity_is_rejected(pool: PgPool) {
    let artifact = setup(&pool).await;
    let (legacy, _) = call_create(&pool, Uuid::new_v4(), &Spec::unkeyed(artifact))
        .await
        .unwrap();
    let err = sqlx::query(
        "UPDATE artifact.generation_job \
         SET caller_request_id=$2, request_fingerprint=repeat('a',64), fingerprint_version=1 \
         WHERE id=$1",
    )
    .bind(legacy)
    .bind(CALLER_A)
    .execute(&pool)
    .await
    .unwrap_err();
    expect_code(&err, "P0001");
    expect_token(&err, "immutable");
    // 三列仍为 NULL。
    let identity: (Option<Uuid>, Option<String>, Option<i16>) = sqlx::query_as(
        "SELECT caller_request_id, request_fingerprint, fingerprint_version \
         FROM artifact.generation_job WHERE id=$1",
    )
    .bind(legacy)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(identity, (None, None, None));
}

// ── AS2-35：部分唯一索引在数据库层阻止身份复用 ─────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_35_partial_unique_index_blocks_duplicate_identity(pool: PgPool) {
    let artifact = setup(&pool).await;
    let (job, _) = call_create(
        &pool,
        Uuid::new_v4(),
        &Spec::caller(artifact, CALLER_A, &fp_a()),
    )
    .await
    .unwrap();
    let err = sqlx::query(
        "INSERT INTO artifact.generation_job \
         (id, tenant_id, project_id, artifact_id, generator_id, generator_version, adapter_version, \
          input_payload, input_schema_version, input_sha256, request_hash, status, \
          caller_request_id, request_fingerprint, fingerprint_version) \
         SELECT gen_random_uuid(), tenant_id, project_id, artifact_id, generator_id, \
                generator_version, adapter_version, input_payload, input_schema_version, \
                input_sha256, request_hash, status, caller_request_id, \
                request_fingerprint, fingerprint_version \
         FROM artifact.generation_job WHERE id=$1",
    )
    .bind(job)
    .execute(&pool)
    .await
    .unwrap_err();
    expect_code(&err, "23505");
    let index = common::pg_error_message(&err);
    assert!(
        index.contains("uq_generation_job_caller_request") || index.contains("generation_job_pkey"),
        "{index}"
    );
}

// ── AS2-45：Job 创建事务不写 idempotency_record ────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_45_job_creation_never_writes_idempotency_record(pool: PgPool) {
    let artifact = setup(&pool).await;
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.idempotency_record")
        .fetch_one(&pool)
        .await
        .unwrap();
    call_create(
        &pool,
        Uuid::new_v4(),
        &Spec::caller(artifact, CALLER_A, &fp_a()).with_key("K-write"),
    )
    .await
    .unwrap();
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.idempotency_record")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "GenerationJob creation must never write idempotency_record rows"
    );
}

// ── AS2-47 / AS2-48：交叉 key 请求（顺序） → 409，双方不变 ──────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_47_48_cross_key_requests_are_rejected(pool: PgPool) {
    let artifact = setup(&pool).await;
    let a = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K1")
        .with_hash(hash_x());
    let b = Spec::caller(artifact, CALLER_B, &fp_b())
        .with_key("K2")
        .with_hash(hash_y());
    let (job_a, _) = call_create(&pool, Uuid::new_v4(), &a).await.unwrap();
    let (job_b, _) = call_create(&pool, Uuid::new_v4(), &b).await.unwrap();
    type SnapshotRow = (Uuid, Option<String>, Option<String>, Option<String>);
    let before: Vec<SnapshotRow> = sqlx::query_as(
        "SELECT id, idempotency_key, request_hash, request_fingerprint::text \
         FROM artifact.generation_job ORDER BY created_at, id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();

    // AS2-47：caller A + F_A + K2 → 409。
    let cross_a = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K2")
        .with_hash(hash_x());
    let err = call_create(&pool, Uuid::new_v4(), &cross_a)
        .await
        .unwrap_err();
    expect_code(&err, "23505");
    expect_token(&err, "IDEMPOTENCY_KEY_REUSED");

    // AS2-48：caller B + F_B + K1 → 409。
    let cross_b = Spec::caller(artifact, CALLER_B, &fp_b())
        .with_key("K1")
        .with_hash(hash_y());
    let err = call_create(&pool, Uuid::new_v4(), &cross_b)
        .await
        .unwrap_err();
    expect_code(&err, "23505");
    expect_token(&err, "IDEMPOTENCY_KEY_REUSED");

    assert_eq!(
        snapshot_after(&pool).await,
        before,
        "neither job may be mutated"
    );
    assert_eq!(job_count(&pool).await, 2);
    assert!(job_a != job_b);
}

/// 读取全部 Job 的身份/key/hash 快照（用于断言「不被改写」）。
async fn snapshot_after(
    pool: &PgPool,
) -> Vec<(Uuid, Option<String>, Option<String>, Option<String>)> {
    sqlx::query_as(
        "SELECT id, idempotency_key, request_hash, request_fingerprint::text \
         FROM artifact.generation_job ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

// ── AS2-49：预置绑定全部提交后并发交叉请求 → 两请求均 409 ───────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_49_precommitted_cross_requests_both_conflict(pool: PgPool) {
    let artifact = setup(&pool).await;
    call_create(
        &pool,
        Uuid::new_v4(),
        &Spec::caller(artifact, CALLER_A, &fp_a())
            .with_key("K1")
            .with_hash(hash_x()),
    )
    .await
    .unwrap();
    call_create(
        &pool,
        Uuid::new_v4(),
        &Spec::caller(artifact, CALLER_B, &fp_b())
            .with_key("K2")
            .with_hash(hash_y()),
    )
    .await
    .unwrap();

    let r1 = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K2")
        .with_hash(hash_x());
    let r2 = Spec::caller(artifact, CALLER_B, &fp_b())
        .with_key("K1")
        .with_hash(hash_y());
    let (res1, res2) = tokio::join!(
        call_create(&pool, Uuid::new_v4(), &r1),
        call_create(&pool, Uuid::new_v4(), &r2),
    );
    for result in [res1, res2] {
        let err = result.expect_err("a committed foreign key holder must block replay");
        expect_code(&err, "23505");
        expect_token(&err, "IDEMPOTENCY_KEY_REUSED");
    }
    assert_eq!(job_count(&pool).await, 2);
}

// ── AS2-50：unique_violation 异常块必须执行同一 key 门禁（真实唯一索引竞争） ─
//
// （独立审查修复版）不再使用测试辅助表的行锁制造阻塞：T1 真实阻塞在
// uq_generation_job_caller_request 唯一索引上，等待 T3 的未提交索引条目。
//
// 时序（满足 ART-04G v3 §10.3 的确定性前提：T2 在 T1 异常块 gate 之前提交）：
//   1. T3 开启事务，创建 caller A + F_A + K3，保持未提交 —— caller A 的唯一
//      索引条目已写入但对其他事务不可见。
//   2. T1 开启事务，请求 caller A + F_A + K50：caller 读前查未命中（T3 未提交），
//      到达 INSERT 后真实阻塞在 caller 唯一索引上，等待 T3 的事务。
//      阻塞位置以三项可观测证据证明：pg_blocking_pids(T1) 恰为 [T3]（T3 在
//      该时点的唯一数据库足迹就是 caller A 的索引条目，K3 与 caller B 均与
//      T1 无交集）、T1 的 wait_event_type='Lock'、T1 的活跃语句就是本创建调用。
//   3. T2 创建 caller B + F_B + K50（不同 raw hash）并成功提交 —— 竞争 key
//      holder 在 T1 的异常块之前落库。
//   4. T3 提交 —— T1 的索引等待解除并确认 unique_violation，进入异常块。
//   5. 异常块以新语句快照重读 caller A（命中 T3 已提交行、指纹一致），执行与
//      读前查完全相同的 key 门禁：K50 已绑定 T2 且 raw hash 不同 → 409
//      IDEMPOTENCY_KEY_REUSED，不得 replay。
//
// 索引处理顺序说明（真实执行路径，非伪造）：PostgreSQL 按索引 OID 升序执行
// 插入期的唯一性检查；迁移序中 uq_generation_job_idempotency_key（0003）的 OID
// 小于 uq_generation_job_caller_request（0005），T1 会先把 K50 写入 key 索引再
// 阻塞，其 speculative 条目会反过来阻塞 T2 对 K50 的插入，破坏 §10.3 的提交
// 顺序前提。测试以完全相同的定义重建 key 索引（仅 OID 排序变化，唯一性语义
// 与部分索引谓词逐字不变），使 caller 索引先于 key 索引被检查，T1 在触及
// key 索引之前即阻塞在目标索引上。本测试不创建任何测试辅助表或触发器。

#[sqlx::test(migrations = "../migrations")]
async fn as2_50_exception_block_reapplies_key_gate(pool: PgPool) {
    let artifact = setup(&pool).await;

    // 0. 重建 key 索引使其 OID 晚于 caller 索引：同名、同列、同谓词，
    //    唯一约束语义不变；仅让 T1 的阻塞点落在 caller 唯一索引上。
    sqlx::query("DROP INDEX artifact.uq_generation_job_idempotency_key")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE UNIQUE INDEX uq_generation_job_idempotency_key \
         ON artifact.generation_job (tenant_id, project_id, idempotency_key) \
         WHERE idempotency_key IS NOT NULL",
    )
    .execute(&pool)
    .await
    .unwrap();

    // 1. T3：caller A + K3 的事务保持未提交 —— caller 唯一索引条目的未提交持有者。
    let mut t3 = pool.acquire().await.unwrap();
    let t3_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *t3)
        .await
        .unwrap();
    sqlx::query("BEGIN").execute(&mut *t3).await.unwrap();
    let t3_spec = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K3")
        .with_hash(hash_x());
    let (t3_job, t3_res) = call_create(&mut *t3, Uuid::new_v4(), &t3_spec)
        .await
        .unwrap();
    assert_eq!(
        t3_res, "created",
        "T3 must insert caller A inside its open transaction"
    );

    // 2. T1：caller A + F_A + K50 —— 目标竞争请求，必须真实阻塞在 caller 唯一索引。
    let mut t1 = pool.acquire().await.unwrap();
    let t1_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *t1)
        .await
        .unwrap();
    let t1_job_id = Uuid::new_v4();
    let t1_spec = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K50")
        .with_hash(hash_x());
    let t1_handle = tokio::spawn(async move { call_create(&mut *t1, t1_job_id, &t1_spec).await });

    // 3. 可观测证据：T1 的唯一阻塞者是 T3，等待事件是锁，活跃语句是真实创建调用。
    //    （不依赖推断：阻塞位置由数据库自身报告，本测试没有辅助表可阻塞。）
    let evidence = wait_for_index_contention(&pool, t1_pid, t3_pid).await;
    assert!(
        evidence.blockers == vec![t3_pid],
        "T1 must be blocked solely by T3's uncommitted caller index entry, got {:?}",
        evidence.blockers
    );
    assert_eq!(
        evidence.wait_event_type.as_deref(),
        Some("Lock"),
        "T1 must be waiting on a lock (transactionid held by T3), got {evidence:?}"
    );
    assert_eq!(
        evidence.wait_event.as_deref(),
        Some("transactionid"),
        "a unique-index insert conflict waits on the conflicting transaction itself, got {evidence:?}"
    );
    assert!(
        evidence.query.contains("create_generation_job_with_input"),
        "T1 must be blocked inside the real create statement, got {evidence:?}"
    );

    // 4. T2：caller B + F_B + K50（不同 raw hash）——竞争 key holder，先于 T1 的
    //    异常块提交（§10.3）。T1 阻塞在 caller 索引、尚未触及 key 索引，因此 T2
    //    对 K50 的插入不会与 T1 的 speculative 条目耦合，可确定性地立即提交。
    let mut t2 = pool.acquire().await.unwrap();
    let t2_spec = Spec::caller(artifact, CALLER_B, &fp_b())
        .with_key("K50")
        .with_hash(hash_y());
    let (t2_job, t2_res) = call_create(&mut *t2, Uuid::new_v4(), &t2_spec)
        .await
        .unwrap();
    assert_eq!(
        t2_res, "created",
        "T2 must commit while T1 is still blocked"
    );

    // 5. T3 提交：caller A 的索引条目可见，T1 以 unique_violation 进入异常块；
    //    异常块 gate 必须发现 K50 已归属 T2 并返回 409 语义的稳定 token。
    sqlx::query("COMMIT").execute(&mut *t3).await.unwrap();
    let err = t1_handle
        .await
        .unwrap()
        .expect_err("the exception block must enforce the key gate");
    expect_code(&err, "23505");
    expect_token(&err, "IDEMPOTENCY_KEY_REUSED");

    // 6. 数据库终态：T1 无残留；T2/T3 的事务结果完整保留且互不侵占。
    assert_eq!(job_count(&pool).await, 2, "no third job may be created");
    let caller_a_rows: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM artifact.generation_job \
         WHERE tenant_id = $1 AND project_id = $2 AND caller_request_id = $3",
    )
    .bind(TENANT)
    .bind(PROJECT)
    .bind(CALLER_A)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        caller_a_rows,
        vec![t3_job],
        "caller A must resolve to T3's job only"
    );
    let caller_b_rows: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM artifact.generation_job \
         WHERE tenant_id = $1 AND project_id = $2 AND caller_request_id = $3",
    )
    .bind(TENANT)
    .bind(PROJECT)
    .bind(CALLER_B)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        caller_b_rows,
        vec![t2_job],
        "caller B must resolve to T2's job only"
    );
    let k50_holders: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM artifact.generation_job WHERE idempotency_key = 'K50'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        k50_holders,
        vec![t2_job],
        "K50 must remain bound to T2's job only"
    );
    let t1_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM artifact.generation_job WHERE id = $1")
            .bind(t1_job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(t1_rows, 0, "T1 must not leave its own job behind");
    let survivors: Vec<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT idempotency_key, request_hash, request_fingerprint::text \
         FROM artifact.generation_job ORDER BY created_at, id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let expected: Vec<(Option<String>, Option<String>, Option<String>)> = vec![
        (Some("K3".to_string()), Some(hash_x()), Some(fp_a())),
        (Some("K50".to_string()), Some(hash_y()), Some(fp_b())),
    ];
    assert_eq!(
        survivors, expected,
        "T3/T2 rows must keep their key, hash and fingerprint untouched"
    );
}

/// 阻塞证据：阻塞者 PID 列表 + 被阻塞会话的等待事件与活跃语句。
/// 有界轮询 `pg_blocking_pids()` 与 `pg_stat_activity`：每轮都是一次真实的
/// 数据库状态观测（不使用 sleep），靠 `yield_now` 让步 + 时间上限兜底。
#[derive(Debug)]
struct ContentionEvidence {
    blockers: Vec<i32>,
    wait_event_type: Option<String>,
    wait_event: Option<String>,
    query: String,
}

async fn wait_for_index_contention(
    pool: &PgPool,
    blocked: i32,
    expected_blocker: i32,
) -> ContentionEvidence {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let blockers: Vec<i32> = sqlx::query_scalar("SELECT pg_blocking_pids($1)")
            .bind(blocked)
            .fetch_one(pool)
            .await
            .unwrap();
        let (wait_event_type, wait_event, query): (Option<String>, Option<String>, String) =
            sqlx::query_as(
                "SELECT wait_event_type, wait_event, left(query, 160) \
                 FROM pg_stat_activity WHERE pid = $1",
            )
            .bind(blocked)
            .fetch_one(pool)
            .await
            .unwrap();
        if blockers.contains(&expected_blocker) {
            return ContentionEvidence {
                blockers,
                wait_event_type,
                wait_event,
                query,
            };
        }
        if Instant::now() >= deadline {
            panic!(
                "backend {blocked} never contended on {expected_blocker} within the deadline; \
                 last blockers {blockers:?}, activity ({wait_event_type:?}, {wait_event:?}, {query:?})"
            );
        }
        tokio::task::yield_now().await;
    }
}

/// 有界轮询 `pg_stat_activity`：确认目标 backend 已真正退出（进程消失），
/// 而不是仅凭 `pg_terminate_backend` 返回 true 或客户端断开推断事务已结束。
/// 以 (pid, backend_start) 双条件匹配，避免 PID 复用造成的误判。
async fn wait_backend_gone(pool: &PgPool, pid: i32, backend_start_epoch: f64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let alive: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE pid = $1 AND extract(epoch from backend_start)::float8 = $2",
        )
        .bind(pid)
        .bind(backend_start_epoch)
        .fetch_one(pool)
        .await
        .unwrap();
        if alive == 0 {
            return;
        }
        if Instant::now() >= deadline {
            panic!("backend {pid} did not exit within the deadline");
        }
        tokio::task::yield_now().await;
    }
}

/// 捕获目标连接的 backend 身份：pid + backend_start（epoch 秒）。
async fn backend_identity(conn: &mut sqlx::PgConnection) -> (i32, f64) {
    sqlx::query_as(
        "SELECT pg_backend_pid(), extract(epoch from backend_start)::float8 \
                    FROM pg_stat_activity WHERE pid = pg_backend_pid()",
    )
    .fetch_one(conn)
    .await
    .unwrap()
}

// ── AS2-51：GENERATOR_UNAVAILABLE → 503 业务语义，不落入 IntegrityFailure ─

#[sqlx::test(migrations = "../migrations")]
async fn as2_51_generator_unavailable_is_a_business_error(pool: PgPool) {
    let artifact = setup(&pool).await;
    sqlx::query(
        "UPDATE artifact.generator_registry SET status='disabled' WHERE generator_id='seecode'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let creator = PgKeyedGenerationJobCreator { pool: pool.clone() };
    let err = creator
        .create_or_resolve_keyed_job(validated(&artifact, Some(CALLER_A)), hash_x(), None)
        .await
        .unwrap_err();
    assert_eq!(err, KeyedJobCreateError::GeneratorUnavailable);
    assert!(matches!(
        err.to_domain_error(),
        artifact_domain::DomainError::GeneratorUnavailable(_)
    ));
    assert!(!matches!(err, KeyedJobCreateError::IntegrityFailure(_)));
}

// ── 指纹版本不匹配：失败关闭，不重放（ART-04G v3 §9.2.2） ────────────────

#[sqlx::test(migrations = "../migrations")]
async fn caller_fingerprint_version_mismatch_fails_closed(pool: PgPool) {
    let artifact = setup(&pool).await;
    let original = Spec::caller(artifact, CALLER_C, &fp_c());
    let (job, _) = call_create(&pool, Uuid::new_v4(), &original).await.unwrap();

    let mismatched = original.clone().with_version(2);
    let err = call_create(&pool, Uuid::new_v4(), &mismatched)
        .await
        .unwrap_err();
    expect_code(&err, "P0001");
    expect_token(&err, "CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH");
    assert_eq!(job_count(&pool).await, 1);
    // 存储版本仍为 1，未被改写。
    let version: i16 =
        sqlx::query_scalar("SELECT fingerprint_version FROM artifact.generation_job WHERE id=$1")
            .bind(job)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(version, 1);
}

// ── AS2-52 / 52a / 52b：C-6 key 规范化回归 ──────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_52_padded_key_hits_normalized_binding(pool: PgPool) {
    let artifact = setup(&pool).await;
    let a = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K1")
        .with_hash(hash_x());
    call_create(&pool, Uuid::new_v4(), &a).await.unwrap();
    let b = Spec::caller(artifact, CALLER_B, &fp_b())
        .with_key("K2")
        .with_hash(hash_y());
    call_create(&pool, Uuid::new_v4(), &b).await.unwrap();

    // caller A 命中，但携带 ' K2 '：门禁必须按规范化 v_key 命中 Job B 并 409。
    let cross = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key(" K2 ")
        .with_hash(hash_x());
    let err = call_create(&pool, Uuid::new_v4(), &cross)
        .await
        .unwrap_err();
    expect_code(&err, "23505");
    expect_token(&err, "IDEMPOTENCY_KEY_REUSED");
    assert_eq!(job_count(&pool).await, 2);
}

#[sqlx::test(migrations = "../migrations")]
async fn as2_52a_blank_keys_equal_none_and_htab_is_kept(pool: PgPool) {
    let artifact = setup(&pool).await;
    // 空字符串 / 仅 ASCII 空格 ≡ 无 key：两次都创建新 Job，不产生 key 冲突。
    let (j1, r1) = call_create(&pool, Uuid::new_v4(), &Spec::unkeyed(artifact).with_key(""))
        .await
        .unwrap();
    assert_eq!(r1, "created");
    let (j2, r2) = call_create(
        &pool,
        Uuid::new_v4(),
        &Spec::unkeyed(artifact).with_key("   "),
    )
    .await
    .unwrap();
    assert_eq!(r2, "created");
    assert_ne!(j1, j2);
    let stored: Option<String> =
        sqlx::query_scalar("SELECT idempotency_key FROM artifact.generation_job WHERE id=$1")
            .bind(j1)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, None, "a blank key must be stored as NULL");

    // HTAB 不是空白：不得被 btrim 折叠。'\tK3\t' 与 'K3' 是两个不同的 key。
    let htab = "\tK3\t";
    let (j3, r3) = call_create(
        &pool,
        Uuid::new_v4(),
        &Spec::unkeyed(artifact).with_key(htab).with_hash(hash_x()),
    )
    .await
    .unwrap();
    assert_eq!(r3, "created");
    let (j4, r4) = call_create(
        &pool,
        Uuid::new_v4(),
        &Spec::unkeyed(artifact).with_key("K3").with_hash(hash_x()),
    )
    .await
    .unwrap();
    assert_eq!(
        r4, "created",
        "HTAB-wrapped key must not collide with the bare key"
    );
    assert_ne!(j3, j4);
    let stored_htab: String =
        sqlx::query_scalar("SELECT idempotency_key FROM artifact.generation_job WHERE id=$1")
            .bind(j3)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored_htab, htab, "the key must be stored verbatim");
}

#[sqlx::test(migrations = "../migrations")]
async fn as2_52b_overlong_key_rejected_on_both_paths(pool: PgPool) {
    let artifact = setup(&pool).await;
    let too_long = "k".repeat(256);
    // create 路径（无 caller）。
    let err = call_create(
        &pool,
        Uuid::new_v4(),
        &Spec::unkeyed(artifact).with_key(&too_long),
    )
    .await
    .unwrap_err();
    expect_code(&err, "22023");
    expect_token(&err, "INVALID_IDEMPOTENCY_KEY");
    assert_eq!(job_count(&pool).await, 0);

    // replay 路径（caller 已存在）。
    let spec = Spec::caller(artifact, CALLER_A, &fp_a());
    call_create(&pool, Uuid::new_v4(), &spec).await.unwrap();
    let err = call_create(&pool, Uuid::new_v4(), &spec.clone().with_key(&too_long))
        .await
        .unwrap_err();
    expect_code(&err, "22023");
    expect_token(&err, "INVALID_IDEMPOTENCY_KEY");
    assert_eq!(job_count(&pool).await, 1);
}

// ── AS2-53：benign interleaving（R1 gate 先于 R2 commit） ──────────────

#[sqlx::test(migrations = "../migrations")]
async fn as2_53_benign_interleaving_is_allowed(pool: PgPool) {
    let artifact = setup(&pool).await;
    // 预置 Job A：caller A + K1。
    let a = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K1")
        .with_hash(hash_x());
    let (job_a, _) = call_create(&pool, Uuid::new_v4(), &a).await.unwrap();

    // R2：caller B + K2 的创建事务开启并 INSERT，暂不提交（屏障前置）。
    let mut r2 = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *r2).await.unwrap();
    let b = Spec::caller(artifact, CALLER_B, &fp_b())
        .with_key("K2")
        .with_hash(hash_y());
    let (job_b, res_b) = call_create(&mut *r2, Uuid::new_v4(), &b).await.unwrap();
    assert_eq!(res_b, "created");

    // R1：caller A + F_A + K2 重放。R2 未提交 → gate 看不到 K2，返回 replay A。
    let r1 = Spec::caller(artifact, CALLER_A, &fp_a())
        .with_key("K2")
        .with_hash(hash_x());
    let (replayed, resolution) = call_create(&pool, Uuid::new_v4(), &r1).await.unwrap();
    assert_eq!(
        (replayed, resolution.as_str()),
        (job_a, "caller_request_replay"),
        "a gate that precedes the competing commit legitimately replays A"
    );

    // R2 提交：K2 最终只绑定 B。
    sqlx::query("COMMIT").execute(&mut *r2).await.unwrap();
    let holders: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM artifact.generation_job WHERE idempotency_key = 'K2'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(holders, vec![job_b], "K2 must bind to B only");
    // A 的身份/key/hash 未被改写。
    let a_row: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT idempotency_key, request_hash, request_fingerprint::text \
         FROM artifact.generation_job WHERE id=$1",
    )
    .bind(job_a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(a_row.0.as_deref(), Some("K1"));
    assert_eq!(a_row.1.as_deref(), Some(hash_x().as_str()));
    assert_eq!(a_row.2.as_deref(), Some(fp_a().as_str()));
}

// ── 共享：构造 validated 请求（供端口级测试） ────────────────────────────

fn validated(artifact: &Uuid, caller: Option<Uuid>) -> ValidatedCreateGenerationJobRequest {
    ValidatedCreateGenerationJobRequest::try_new(
        &TenantScope::new(TENANT, PROJECT),
        *artifact,
        caller,
        "seecode".into(),
        common::minimal_input(),
        None,
        "1".into(),
        4096,
    )
    .expect("fixture request is valid")
}
