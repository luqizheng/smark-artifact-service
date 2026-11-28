//! GenerationJob 的 HTTP 契约：查询与取消的成功路径、响应字段、scope 校验，
//! 以及生成请求对输入信封的拒绝形态。
//!
//! Job 由测试直接通过受控函数创建（生产 API 的创建路径仍然 503，未开放）。

mod common;

use artifact_application::WorkerCoordinationPort;
use artifact_domain::Action;
use artifact_infrastructure::postgres::PgWorkerCoordination;
use common::{PROJECT_1, PROJECT_2, TENANT};
use serde_json::{Value, json};
use sqlx::postgres::PgPool;
use uuid::Uuid;

const ADAPTER: &str = "adapter-api-test";
const TOKEN: &str = "gen-api-token";
const OTHER_TENANT: Uuid = Uuid::from_u128(0x0002);
const OTHER_TENANT_TOKEN: &str = "gen-api-other-tenant";
const READ_ONLY_TOKEN: &str = "gen-api-read-only";

async fn spawn(
    pool: &PgPool,
) -> (
    String,
    std::sync::Arc<artifact_infrastructure::test_support::TestDelegationVerifier>,
) {
    let verifier = common::verifier_with(&[
        (TOKEN, TENANT, PROJECT_1, &common::all_actions()),
        (
            OTHER_TENANT_TOKEN,
            OTHER_TENANT,
            PROJECT_1,
            &common::all_actions(),
        ),
        (
            READ_ONLY_TOKEN,
            TENANT,
            PROJECT_1,
            &[Action::GenerationRequest, Action::GenerationRead],
        ),
    ]);
    let base = common::spawn_app(pool.clone(), verifier.clone()).await;
    (base, verifier)
}

/// 写请求：铸造 V10 绑定 token 并附加鉴权头（caller 随机或指定，
/// v1 指纹按 method/path/body 计算；WP-04）。
#[allow(clippy::too_many_arguments)]
fn bound_auth(
    verifier: &std::sync::Arc<artifact_infrastructure::test_support::TestDelegationVerifier>,
    req: reqwest::RequestBuilder,
    tenant: Uuid,
    project: Uuid,
    method: &str,
    path: &str,
    body: &Value,
    caller: Option<Uuid>,
) -> reqwest::RequestBuilder {
    use artifact_infrastructure::test_support::TestTokenBinding;
    let token = format!("gen-bound-{}", Uuid::new_v4());
    let caller = caller.unwrap_or_else(Uuid::new_v4);
    let hash = common::v1_fingerprint(method, path, caller, body);
    common::mint_bound_token(
        verifier,
        &token,
        tenant,
        project,
        &common::all_actions(),
        TestTokenBinding {
            artifact_id: None,
            operation_id: None,
            caller_request_id: Some(caller),
            request_hash: Some(hash),
        },
    );
    req.header("x-project-id", project.to_string())
        .header("authorization", format!("Bearer {token}"))
}

/// 取消请求绑定（无 body：v1 指纹 body 段 = null）。
fn bound_cancel(
    verifier: &std::sync::Arc<artifact_infrastructure::test_support::TestDelegationVerifier>,
    base: &str,
    tenant: Uuid,
    project: Uuid,
    job_id: Uuid,
) -> reqwest::RequestBuilder {
    let path = format!("/v1/generation-jobs/{job_id}/cancel");
    bound_auth(
        verifier,
        reqwest::Client::new().post(format!("{base}{path}")),
        tenant,
        project,
        "POST",
        &path,
        &Value::Null,
        None,
    )
}

/// 建一个 queued Job，返回 (job_id, artifact_id)。
async fn seed(pool: &PgPool) -> (Uuid, Uuid) {
    let artifact = common::sql_insert_artifact(pool, TENANT, PROJECT_1, "api").await;
    common::sql_enable_seecode(pool, ADAPTER).await;
    let job_id = common::sql_create_job(
        pool,
        TENANT,
        PROJECT_1,
        artifact,
        &common::JobOptions::default(),
    )
    .await;
    (job_id, artifact)
}

fn auth(req: reqwest::RequestBuilder, project: Uuid) -> reqwest::RequestBuilder {
    req.header("x-project-id", project.to_string())
        .header("authorization", format!("Bearer {TOKEN}"))
}

async fn get_job(base: &str, job_id: Uuid) -> Value {
    let res = auth(
        reqwest::Client::new().get(format!("{base}/v1/generation-jobs/{job_id}")),
        PROJECT_1,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 200);
    res.json().await.unwrap()
}

async fn cancel_count(pool: &PgPool, job_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM artifact.audit_event \
         WHERE action = 'generation.cancel' AND object_id = $1",
    )
    .bind(job_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// 响应字段与 OpenAPI 完全一致：调用方拿得到结果，也拿不到输入正文或租约。
#[sqlx::test(migrations = "../migrations")]
async fn get_job_serves_the_documented_shape(pool: PgPool) {
    let (job_id, _) = seed(&pool).await;
    let (base, _verifier) = spawn(&pool).await;

    let body = get_job(&base, job_id).await;
    let spec: serde_yaml::Value =
        serde_yaml::from_str(include_str!("../../openapi/openapi.yaml")).unwrap();
    let mut documented: Vec<String> = spec["components"]["schemas"]["GenerationJob"]["properties"]
        .as_mapping()
        .unwrap()
        .keys()
        .map(|key| key.as_str().unwrap().to_string())
        .collect();
    documented.sort();
    let mut served: Vec<String> = body.as_object().unwrap().keys().map(Clone::clone).collect();
    served.sort();
    assert_eq!(
        served, documented,
        "served fields must match the documented schema"
    );

    assert_eq!(body["id"], job_id.to_string());
    assert_eq!(body["status"], "queued");
    assert_eq!(body["error_code"], Value::Null);
    assert_eq!(body["attempt_count"], 0);
    assert_eq!(body["max_attempts"], 3);
    assert_eq!(body["cancel_requested"], false);
    assert_eq!(body["poll_url"], format!("/v1/generation-jobs/{job_id}"));

    // 响应能反序列化为 SDK 的契约类型。
    let typed: artifact_contract::GenerationJobResponse = serde_json::from_value(body).unwrap();
    assert_eq!(typed.id, job_id);
}

/// queued 的 Job 直接取消；终态 Job 上的取消是空操作，不重复记审计。
#[sqlx::test(migrations = "../migrations")]
async fn cancel_queued_job_and_terminal_noop(pool: PgPool) {
    let (job_id, _) = seed(&pool).await;
    let (base, verifier) = spawn(&pool).await;
    let _url = format!("{base}/v1/generation-jobs/{job_id}/cancel");

    let res = bound_cancel(&verifier, &base, TENANT, PROJECT_1, job_id)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["status"], "cancelled");
    assert_eq!(body["error_code"], "CANCELLED");
    assert_eq!(body["cancel_requested"], false);
    assert_eq!(cancel_count(&pool, job_id).await, 1);

    // 再次取消：终态原样返回。
    let res = bound_cancel(&verifier, &base, TENANT, PROJECT_1, job_id)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["status"], "cancelled");
    assert_eq!(
        cancel_count(&pool, job_id).await,
        1,
        "a no-op cancel is not audited again"
    );
    assert_eq!(get_job(&base, job_id).await["status"], "cancelled");
}

/// running 的 Job 只登记取消请求：响应明确告诉调用方请求已被接受，
/// 但状态仍是 running；Worker 确认之后才变为 cancelled。
#[sqlx::test(migrations = "../migrations")]
async fn cancel_running_job_reports_cancel_requested(pool: PgPool) {
    let (job_id, _) = seed(&pool).await;
    let coordination = PgWorkerCoordination { pool: pool.clone() };
    let claimed = coordination
        .claim("worker-a", 30, &common::seecode_capabilities(ADAPTER))
        .await
        .unwrap()
        .unwrap();
    let (base, verifier) = spawn(&pool).await;
    let _url = format!("{base}/v1/generation-jobs/{job_id}/cancel");

    let running = get_job(&base, job_id).await;
    assert_eq!(running["status"], "running");
    assert_eq!(running["cancel_requested"], false);
    assert_eq!(running["attempt_count"], 1);

    let res = bound_cancel(&verifier, &base, TENANT, PROJECT_1, job_id)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body["status"], "running",
        "cancel must not fabricate that the worker stopped"
    );
    assert_eq!(body["cancel_requested"], true);
    assert_eq!(body["error_code"], Value::Null);
    assert_eq!(get_job(&base, job_id).await["cancel_requested"], true);
    assert_eq!(cancel_count(&pool, job_id).await, 1);

    // 取消请求被 Worker 看见，确认后 Job 进入终态。
    assert_eq!(
        coordination
            .heartbeat(job_id, claimed.attempt_id, &claimed.lease_token, 30)
            .await
            .unwrap(),
        artifact_application::HeartbeatOutcome::CancelRequested
    );
    assert_eq!(
        coordination
            .confirm_cancelled(job_id, claimed.attempt_id, &claimed.lease_token)
            .await
            .unwrap(),
        artifact_application::ConfirmCancelOutcome::Cancelled
    );
    let cancelled = get_job(&base, job_id).await;
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(
        cancelled["cancel_requested"], true,
        "the request itself stays visible"
    );
    assert_eq!(cancelled["error_code"], "CANCELLED");
}

/// scope 与动作校验：项目不匹配 403，缺动作 403，跨租户一律 404（不泄露 Job 是否存在）。
#[sqlx::test(migrations = "../migrations")]
async fn job_endpoints_enforce_scope_and_action(pool: PgPool) {
    let (job_id, _) = seed(&pool).await;
    let (base, verifier) = spawn(&pool).await;
    let client = reqwest::Client::new();

    // 委托不覆盖请求的项目。
    let res = auth(
        client.get(format!("{base}/v1/generation-jobs/{job_id}")),
        PROJECT_2,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 403);
    assert_eq!(
        res.json::<Value>().await.unwrap()["error"]["code"],
        "SCOPE_DENIED"
    );

    // 不存在的 Job。
    let res = auth(
        client.get(format!("{base}/v1/generation-jobs/{}", Uuid::new_v4())),
        PROJECT_1,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 404);

    // 委托缺少 generation:cancel 动作。
    let res = client
        .post(format!("{base}/v1/generation-jobs/{job_id}/cancel"))
        .header("x-project-id", PROJECT_1.to_string())
        .header("authorization", format!("Bearer {READ_ONLY_TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);

    // 另一个租户对同一个 project_id：取消在数据库函数内按 scope 校验，一律 not found。
    // （WP-04：使用一枚绑定完整的合法 token——绑定缺失的拒绝属于另一条防线，
    // 与跨租户不可区分性语义无关。）
    let res = bound_cancel(&verifier, &base, OTHER_TENANT, PROJECT_1, job_id)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    let res = client
        .get(format!("{base}/v1/generation-jobs/{job_id}"))
        .header("x-project-id", PROJECT_1.to_string())
        .header("authorization", format!("Bearer {OTHER_TENANT_TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);

    // Job 没有被动过，也没有留下取消审计。
    let status: String =
        sqlx::query_scalar("SELECT status FROM artifact.generation_job WHERE id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "queued");
    assert_eq!(cancel_count(&pool, job_id).await, 0);
}

/// 生成请求对输入信封的拒绝形态：稳定错误码 + 结构化原因与位置，且不回显输入内容。
#[sqlx::test(migrations = "../migrations")]
async fn generation_request_enforces_the_input_envelope(pool: PgPool) {
    let (_, artifact) = seed(&pool).await;
    let (base, verifier) = spawn(&pool).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/v1/artifacts/{artifact}/generation-jobs");
    let jobs_before: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.generation_job")
        .fetch_one(&pool)
        .await
        .unwrap();

    // 合法信封：WP-AS-06 接线 keyed create → 202 新建（0006 翻转 seecode 为
    // available）。请求侧 caller = body.caller_request_id，与 token claim
    // 交叉比对（WP-04）。
    let valid_caller = Uuid::new_v4();
    let valid_body = json!({
        "generator_id": "seecode",
        "caller_request_id": valid_caller,
        "input": common::minimal_input()
    });
    let res = bound_auth(
        &verifier,
        client.post(&url).json(&valid_body),
        TENANT,
        PROJECT_1,
        "POST",
        &format!("/v1/artifacts/{artifact}/generation-jobs"),
        &valid_body,
        Some(valid_caller),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 202);
    assert_eq!(res.headers()["x-generation-job-resolution"], "created");
    let created: Value = res.json().await.unwrap();
    assert_eq!(created["status"], "queued");
    assert_eq!(created["caller_request_id"], valid_caller.to_string());
    assert_eq!(created["target_version_id"], Value::Null);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM artifact.generation_job")
            .fetch_one(&pool)
            .await
            .unwrap(),
        jobs_before + 1,
        "exactly one job must be created"
    );

    // 超过 256 KiB：413，带 request_id。
    let oversized = json!({
        "generator_id": "seecode",
        "caller_request_id": Uuid::new_v4(),
        "input": {"schema": "smark.seecode.input", "padding": "x".repeat(300_000)}
    });
    let res = auth(client.post(&url).json(&oversized), PROJECT_1)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 413);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "PAYLOAD_TOO_LARGE");
    assert_eq!(body["error"]["retryable"], false);
    assert_eq!(body["error"]["details"]["reason"], "INPUT_TOO_LARGE");
    assert!(body["error"]["request_id"].is_string());

    // 未知顶层字段：400，原因与位置在 details 里；字段名本身不回显。
    let mut unknown = common::minimal_input();
    unknown["internal-notes-SECRET-VALUE"] = json!("SECRET-CONTENT-VALUE");
    let res = auth(
        client
            .post(&url)
            .json(&json!({"generator_id": "seecode", "caller_request_id": Uuid::new_v4(), "input": unknown})),
        PROJECT_1,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 400);
    let text = res.text().await.unwrap();
    assert!(
        !text.contains("SECRET-VALUE"),
        "error must not echo the field name: {text}"
    );
    assert!(
        !text.contains("SECRET-CONTENT-VALUE"),
        "error must not echo the value: {text}"
    );
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["error"]["code"], "INVALID_REQUEST");
    assert_eq!(body["error"]["details"]["reason"], "UNKNOWN_FIELD");
    assert_eq!(body["error"]["details"]["field"], "input");

    // URL 形态的值：400，位置精确到字段。
    let url_shaped = json!({
        "schema": "smark.seecode.input",
        "schema_version": "1",
        "diagram": {"type": "architecture", "nodes": [{"id": "a", "icon": "http://evil.example/x.png"}]}
    });
    let res = auth(
        client
            .post(&url)
            .json(&json!({"generator_id": "seecode", "caller_request_id": Uuid::new_v4(), "input": url_shaped})),
        PROJECT_1,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "INVALID_REQUEST");
    assert_eq!(body["error"]["details"]["reason"], "URL_NOT_ALLOWED");
    assert_eq!(
        body["error"]["details"]["field"],
        "input.diagram.nodes[0].icon"
    );

    // 不支持的 schema 版本。
    let mut wrong_version = common::minimal_input();
    wrong_version["schema_version"] = json!("2");
    let res = auth(
        client
            .post(&url)
            .json(&json!({"generator_id": "seecode", "caller_request_id": Uuid::new_v4(), "input": wrong_version})),
        PROJECT_1,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["details"]["reason"], "UNSUPPORTED_SCHEMA");
    assert_eq!(body["error"]["details"]["field"], "input.schema_version");

    let jobs_after: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.generation_job")
        .fetch_one(&pool)
        .await
        .unwrap();
    // WP-AS-06：本测试仅首个合法信封请求创建一个 Job（202）；其余
    // （超限/非法信封）一律不产生 Job。
    assert_eq!(
        jobs_after,
        jobs_before + 1,
        "exactly the valid-envelope request may create a job"
    );
}

// ── AS-2 Caller Request Lookup（HTTP 层）：AS2-21/22/23/24/36/41 ────────────

const LOOKUP_CALLER: Uuid = Uuid::from_u128(0x0d01);
const LOOKUP_FP: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

/// 经受控函数建一个带 caller identity 的 Job。
async fn seed_identified(pool: &PgPool, tenant: Uuid, project: Uuid, caller: Uuid) -> (Uuid, Uuid) {
    let artifact = common::sql_insert_artifact(pool, tenant, project, "api-identified").await;
    common::sql_enable_seecode(pool, ADAPTER).await;
    let (job, replayed) = common::try_sql_create_job_with_identity(
        pool,
        Uuid::new_v4(),
        tenant,
        project,
        artifact,
        &common::JobOptions::default(),
        None,
        &common::sha256_hex("lookup-request"),
        Some((caller, LOOKUP_FP, 1)),
    )
    .await
    .unwrap();
    assert!(!replayed);
    (job, artifact)
}

async fn lookup(base: &str, caller: Uuid, project: Uuid) -> reqwest::Response {
    auth(
        reqwest::Client::new().get(format!(
            "{base}/v1/generation-jobs/by-caller-request/{caller}"
        )),
        project,
    )
    .send()
    .await
    .unwrap()
}

/// AS2-36：命中返回 200 + 标准 GenerationJob（含 caller_request_id）+ 可用 poll_url。
#[sqlx::test(migrations = "../migrations")]
async fn as2_36_lookup_serves_the_job_with_caller_identity(pool: PgPool) {
    let (job, _) = seed_identified(&pool, TENANT, PROJECT_1, LOOKUP_CALLER).await;
    let (base, _verifier) = spawn(&pool).await;

    let res = lookup(&base, LOOKUP_CALLER, PROJECT_1).await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["id"], job.to_string());
    assert_eq!(body["caller_request_id"], LOOKUP_CALLER.to_string());
    assert_eq!(body["poll_url"], format!("/v1/generation-jobs/{job}"));
    // 与按 job_id 查询完全一致（同 DTO）。
    let by_id = get_job(&base, job).await;
    assert_eq!(body, by_id);
}

/// AS2-22：跨 scope 与不存在统一 404，不泄露存在性。
#[sqlx::test(migrations = "../migrations")]
async fn as2_22_lookup_does_not_leak_across_scopes(pool: PgPool) {
    let (job, _) = seed_identified(&pool, TENANT, PROJECT_1, LOOKUP_CALLER).await;
    let (base, _verifier) = spawn(&pool).await;

    // 另一个租户查询同一个 caller_request_id：404（存在但不可见）。
    let res = reqwest::Client::new()
        .get(format!(
            "{base}/v1/generation-jobs/by-caller-request/{LOOKUP_CALLER}"
        ))
        .header("x-project-id", PROJECT_1.to_string())
        .header("authorization", format!("Bearer {OTHER_TENANT_TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "RESOURCE_NOT_FOUND");

    // 完全不存在的 caller 在同一 scope：同样 404（响应形态一致）。
    let res = lookup(&base, Uuid::new_v4(), PROJECT_1).await;
    assert_eq!(res.status(), 404);
    let missing: Value = res.json().await.unwrap();
    assert_eq!(missing["error"]["code"], "RESOURCE_NOT_FOUND");

    // 只读：命中后 Job 状态未改变。
    let status: String =
        sqlx::query_scalar("SELECT status FROM artifact.generation_job WHERE id = $1")
            .bind(job)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "queued");
}

/// AS2-21：无项目授权 → 403（授权在查询之前，且不泄露存在性）。
#[sqlx::test(migrations = "../migrations")]
async fn as2_21_lookup_requires_project_authorization(pool: PgPool) {
    let _ = seed_identified(&pool, TENANT, PROJECT_1, LOOKUP_CALLER).await;
    let (base, _verifier) = spawn(&pool).await;
    let res = lookup(&base, LOOKUP_CALLER, PROJECT_2).await;
    assert_eq!(res.status(), 403);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "SCOPE_DENIED");
}

/// AS2-23：非法路径参数 → 400 稳定原因码。
/// P2-01（独立审查修复）：错误响应不得回显用户提交的原始路径参数。
#[sqlx::test(migrations = "../migrations")]
async fn as2_23_lookup_rejects_invalid_uuid(pool: PgPool) {
    let _ = seed_identified(&pool, TENANT, PROJECT_1, LOOKUP_CALLER).await;
    let (base, _verifier) = spawn(&pool).await;
    // 哨兵值带有唯一标记，确保任何回显都能被检出。
    let malicious = "not-a-uuid-MARKER-9f3a";
    let res = auth(
        reqwest::Client::new().get(format!(
            "{base}/v1/generation-jobs/by-caller-request/{malicious}"
        )),
        PROJECT_1,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "INVALID_REQUEST");
    let reason = body["error"]["details"]["reason"]
        .as_str()
        .unwrap_or_default();
    assert!(
        reason.contains("invalid caller_request_id"),
        "stable reason for a malformed path uuid, got {reason}"
    );
    let raw = serde_json::to_string(&body).unwrap_or_default();
    assert!(
        !raw.contains(malicious),
        "the error response must not echo the raw path parameter, got {raw}"
    );
    assert!(
        !body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains(malicious),
        "the error message must not echo the raw path parameter"
    );
}

/// AS2-24：lookup 与创建互斥于提交时点——提交前 404，提交后必达（N-6）。
#[sqlx::test(migrations = "../migrations")]
async fn as2_24_lookup_sees_only_committed_jobs(pool: PgPool) {
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "api-commit").await;
    common::sql_enable_seecode(&pool, ADAPTER).await;
    let (base, _verifier) = spawn(&pool).await;
    let caller = Uuid::from_u128(0x0d02);

    // 未提交的创建事务。
    let mut tx = pool.begin().await.unwrap();
    let (job, resolution): (Uuid, String) = sqlx::query_as(
        "SELECT * FROM artifact.create_generation_job_with_input(\
         $1, $2, $3, $4, 'seecode', $5, '{}'::jsonb, '1', $6, NULL, \
         $7, $8, 1::smallint, 3, 300, 86400)",
    )
    .bind(Uuid::new_v4())
    .bind(TENANT)
    .bind(PROJECT_1)
    .bind(artifact)
    .bind(common::minimal_input())
    .bind(common::sha256_hex("commit-visibility"))
    .bind(caller)
    .bind(LOOKUP_FP)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(resolution, "created");

    // 提交前：lookup 必须 404（READ COMMITTED 语句快照语义，不泄露未提交数据）。
    let res = lookup(&base, caller, PROJECT_1).await;
    assert_eq!(res.status(), 404);

    tx.commit().await.unwrap();

    // 提交后：lookup 必达同一 Job。
    let res = lookup(&base, caller, PROJECT_1).await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["id"], job.to_string());
}

/// AS2-41：HTTP create 继续 fail-closed 503，不因 AS-2 放开生成入口。
#[sqlx::test(migrations = "../migrations")]
async fn as2_41_http_create_stays_fail_closed(pool: PgPool) {
    let (_, artifact) = seed(&pool).await;
    let (base, verifier) = spawn(&pool).await;
    let jobs_before: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.generation_job")
        .fetch_one(&pool)
        .await
        .unwrap();
    // WP-AS-06 后 seecode seed 为 available；fail-closed 语义由
    // disabled 状态表达（HTTP 层仍 503 GENERATOR_UNAVAILABLE）。
    sqlx::query(
        "UPDATE artifact.generator_registry SET status='disabled' WHERE generator_id='seecode'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let job_caller = Uuid::new_v4();
    let job_body = json!({
        "generator_id": "seecode",
        "caller_request_id": job_caller,
        "input": common::minimal_input()
    });
    let res = bound_auth(
        &verifier,
        reqwest::Client::new()
            .post(format!("{base}/v1/artifacts/{artifact}/generation-jobs"))
            .json(&job_body),
        TENANT,
        PROJECT_1,
        "POST",
        &format!("/v1/artifacts/{artifact}/generation-jobs"),
        &job_body,
        Some(job_caller),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 503);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "GENERATOR_UNAVAILABLE");
    let jobs_after: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.generation_job")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs_before, jobs_after, "no job may be created over HTTP");
}
