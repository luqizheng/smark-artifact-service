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

async fn spawn(pool: &PgPool) -> String {
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
    common::spawn_app(pool.clone(), verifier).await
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
    let base = spawn(&pool).await;

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
    let base = spawn(&pool).await;
    let url = format!("{base}/v1/generation-jobs/{job_id}/cancel");

    let res = auth(reqwest::Client::new().post(&url), PROJECT_1)
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
    let res = auth(reqwest::Client::new().post(&url), PROJECT_1)
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
    let base = spawn(&pool).await;
    let url = format!("{base}/v1/generation-jobs/{job_id}/cancel");

    let running = get_job(&base, job_id).await;
    assert_eq!(running["status"], "running");
    assert_eq!(running["cancel_requested"], false);
    assert_eq!(running["attempt_count"], 1);

    let res = auth(reqwest::Client::new().post(&url), PROJECT_1)
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
    let base = spawn(&pool).await;
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
    let res = client
        .post(format!("{base}/v1/generation-jobs/{job_id}/cancel"))
        .header("x-project-id", PROJECT_1.to_string())
        .header("authorization", format!("Bearer {OTHER_TENANT_TOKEN}"))
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
    let base = spawn(&pool).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/v1/artifacts/{artifact}/generation-jobs");
    let jobs_before: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.generation_job")
        .fetch_one(&pool)
        .await
        .unwrap();

    // 合法信封：生成器没有 Worker，仍然 503，且不会产生 Job。
    let res = auth(
        client
            .post(&url)
            .json(&json!({"generator_id": "seecode", "input": common::minimal_input()})),
        PROJECT_1,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 503);
    assert_eq!(
        res.json::<Value>().await.unwrap()["error"]["code"],
        "GENERATOR_UNAVAILABLE"
    );

    // 超过 256 KiB：413，带 request_id。
    let oversized = json!({
        "generator_id": "seecode",
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
            .json(&json!({"generator_id": "seecode", "input": unknown})),
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
            .json(&json!({"generator_id": "seecode", "input": url_shaped})),
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
            .json(&json!({"generator_id": "seecode", "input": wrong_version})),
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
    assert_eq!(
        jobs_after, jobs_before,
        "no request in this test may create a job"
    );
}
