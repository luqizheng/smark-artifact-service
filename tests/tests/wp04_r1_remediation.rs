//! WP-LMMS-DELEGATION-04-R1 复评修复的对抗性集成测试。
//!
//! * F 组（P1）：v1 写端点拒绝 query——契约 §3.1.3 冻结规则（v1 指纹成分
//!   只有 METHOD/PATH/CALLER/BODY，且 v1 全部写端点不得携带 query）。合法
//!   token（指纹按无 query 路径铸造）+ 追加 query 的请求必须在 HTTP adapter
//!   统一入口（`write_binding`，全部 9 个写 handler 的唯一入口）被 400
//!   拒绝——先于任何数据库写入 / storage / generator / 队列副作用。
//!   变异敏感性：移除该检查后，正确绑定的 query 请求将绕过 query 门直接
//!   成功，本组测试随即失败。
//! * G 组（P2）：W8 目标绑定——W8（`/v1/artifacts/{artifact_id}/generation-jobs`）
//!   是目标寻址路由，其 artifact_id claim 与路径 artifact ID 精确比对。
//!   测试明确区分 403（绑定拒绝）与 503（generator 功能门）——503 不是
//!   授权失败；拒绝时零 DB 写入、generator 门未触达。

mod common;

use artifact_domain::Action;
use artifact_infrastructure::test_support::{TestDelegationVerifier, TestTokenBinding};
use reqwest::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x0451);
const PROJECT_1: Uuid = Uuid::from_u128(0x0452);

fn write_actions() -> &'static [Action] {
    &[
        Action::ArtifactCreate,
        Action::ArtifactUpdateMetadata,
        Action::VersionCreate,
        Action::VersionCommit,
        Action::VersionSetCurrent,
        Action::ReferenceWrite,
        Action::ReferenceRemove,
        Action::GenerationRequest,
        Action::GenerationCancel,
        Action::ArtifactRead,
    ]
}

async fn spawn(pool: &PgPool) -> (String, Arc<TestDelegationVerifier>) {
    let verifier = common::verifier_with(&[]);
    let base = common::spawn_app(pool.clone(), verifier.clone()).await;
    (base, verifier)
}

#[allow(clippy::too_many_arguments)]
fn mint(
    verifier: &Arc<TestDelegationVerifier>,
    tenant: Uuid,
    project: Uuid,
    actions: &[Action],
    method: &str,
    path: &str,
    body: &Value,
    caller: Option<Uuid>,
    artifact_id: Option<Uuid>,
) -> String {
    let caller = caller.unwrap_or_else(Uuid::new_v4);
    let hash = common::v1_fingerprint(method, path, caller, body);
    let token = format!("r1-{}", Uuid::new_v4());
    common::mint_bound_token(
        verifier,
        &token,
        tenant,
        project,
        actions,
        TestTokenBinding {
            artifact_id,
            operation_id: None,
            caller_request_id: Some(caller),
            request_hash: Some(hash),
        },
    );
    token
}

fn req_for(
    method: &str,
    url: String,
    token: &str,
    project: Option<Uuid>,
) -> reqwest::RequestBuilder {
    let client = reqwest::Client::new();
    let req = match method {
        "POST" => client.post(url),
        "PATCH" => client.patch(url),
        "DELETE" => client.delete(url),
        _ => panic!("unsupported test method {method}"),
    };
    let req = req.header("authorization", format!("Bearer {token}"));
    match project {
        Some(p) => req.header("x-project-id", p.to_string()),
        None => req,
    }
}

async fn send(req: reqwest::RequestBuilder, body: Option<&Value>) -> (StatusCode, Value) {
    let req = match body {
        Some(b) => req.json(b),
        None => req,
    };
    let res = req.send().await.expect("request sent");
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    (status, value)
}

fn create_body(project: Uuid, name: &str, artifact_key: Option<&str>) -> Value {
    json!({
        "project_id": project,
        "name": name,
        "artifact_type": "visualization",
        "artifact_key": artifact_key,
        "description": Value::Null,
        "category": Value::Null,
    })
}

// ── F 组：v1 写端点拒绝 query（P1）────────────────────────────────────

/// 9 条写路由的 query 攻击规格：`path` 为无 query 的实例化形态。
/// `caller`：W8 的请求侧 caller = body.caller_request_id（claim 必须一致）。
struct QuerySpec {
    label: &'static str,
    method: &'static str,
    path: String,
    body: Option<Value>,
    project: Uuid,
    if_match: bool,
    caller: Option<Uuid>,
}

async fn seed_query_fixtures(pool: &PgPool) -> Vec<QuerySpec> {
    let artifact = common::sql_insert_artifact(pool, TENANT, PROJECT_1, "q-fix").await;
    let ready = common::sql_insert_version(pool, TENANT, PROJECT_1, artifact, "ready").await;
    let reference: Uuid = sqlx::query_scalar(
        "INSERT INTO artifact.artifact_reference \
         (id, tenant_id, project_id, artifact_id, owner_type, owner_id, relation_type, created_by) \
         VALUES ($1, $2, $3, $4, 'adr', 'q-ref', 'illustrates', 'fixture') RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(TENANT)
    .bind(PROJECT_1)
    .bind(artifact)
    .fetch_one(pool)
    .await
    .unwrap();
    common::sql_enable_seecode(pool, "q-adapter").await;
    let job = common::sql_create_job(
        pool,
        TENANT,
        PROJECT_1,
        artifact,
        &common::JobOptions::default(),
    )
    .await;
    let keyed_body = create_body(PROJECT_1, "q-w1", Some("q.w1.key"));
    let patch_body = json!({ "name": "q-patch" });
    let session_body = json!({ "source_format": "yaml" });
    let current_body = json!({ "version_id": ready, "expected_current_version_id": Value::Null });
    let ref_body =
        json!({ "owner_type": "adr", "owner_id": "q-new", "relation_type": "illustrates" });
    let job_caller = Uuid::new_v4();
    let job_body = json!({ "generator_id": "seecode", "caller_request_id": job_caller, "input": common::minimal_input() });
    let job_caller_opt = Some(job_caller);
    vec![
        QuerySpec {
            label: "W1",
            method: "POST",
            path: "/v1/artifacts".into(),
            body: Some(keyed_body),
            project: PROJECT_1,
            if_match: false,
            caller: None,
        },
        QuerySpec {
            label: "W2",
            method: "PATCH",
            path: format!("/v1/artifacts/{artifact}"),
            body: Some(patch_body),
            project: PROJECT_1,
            if_match: true,
            caller: None,
        },
        QuerySpec {
            label: "W3",
            method: "POST",
            path: format!("/v1/artifacts/{artifact}/versions"),
            body: Some(session_body),
            project: PROJECT_1,
            if_match: false,
            caller: None,
        },
        QuerySpec {
            label: "W4",
            method: "POST",
            path: format!("/v1/artifacts/{artifact}/versions/{ready}/commit"),
            body: None,
            project: PROJECT_1,
            if_match: false,
            caller: None,
        },
        QuerySpec {
            label: "W5",
            method: "POST",
            path: format!("/v1/artifacts/{artifact}/current-version"),
            body: Some(current_body),
            project: PROJECT_1,
            if_match: false,
            caller: None,
        },
        QuerySpec {
            label: "W6",
            method: "POST",
            path: format!("/v1/artifacts/{artifact}/references"),
            body: Some(ref_body),
            project: PROJECT_1,
            if_match: false,
            caller: None,
        },
        QuerySpec {
            label: "W7",
            method: "DELETE",
            path: format!("/v1/artifacts/{artifact}/references/{reference}"),
            body: None,
            project: PROJECT_1,
            if_match: false,
            caller: None,
        },
        QuerySpec {
            label: "W8",
            method: "POST",
            path: format!("/v1/artifacts/{artifact}/generation-jobs"),
            body: Some(job_body),
            project: PROJECT_1,
            if_match: false,
            caller: job_caller_opt,
        },
        QuerySpec {
            label: "W9",
            method: "POST",
            path: format!("/v1/generation-jobs/{job}/cancel"),
            body: None,
            project: PROJECT_1,
            if_match: false,
            caller: None,
        },
    ]
}

/// 发送一次 query 攻击：token 按（method, 无 query path, body）正确铸造，
/// URL 追加 `suffix`（`?x=1` 或裸 `?`）。
async fn send_with_query_suffix(
    base: &str,
    verifier: &Arc<TestDelegationVerifier>,
    spec: &QuerySpec,
    suffix: &str,
) -> (StatusCode, Value) {
    let token = mint(
        verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        spec.method,
        &spec.path,
        spec.body.as_ref().unwrap_or(&Value::Null),
        spec.caller,
        None,
    );
    let req = req_for(
        spec.method,
        format!("{base}{}{suffix}", spec.path),
        &token,
        Some(spec.project),
    );
    let req = if spec.if_match {
        req.header("if-match", "\"r1\"")
    } else {
        req
    };
    send(req, spec.body.as_ref()).await
}

/// 可变写表面的行数快照（artifacts, references, jobs, idempotency records）。
async fn write_surface_counts(pool: &PgPool) -> (i64, i64, i64, i64) {
    let artifacts: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.artifact")
        .fetch_one(pool)
        .await
        .unwrap();
    let references: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.artifact_reference")
        .fetch_one(pool)
        .await
        .unwrap();
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.generation_job")
        .fetch_one(pool)
        .await
        .unwrap();
    let idem: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.idempotency_record")
        .fetch_one(pool)
        .await
        .unwrap();
    (artifacts, references, jobs, idem)
}

#[sqlx::test(migrations = "../migrations")]
async fn f1_query_with_params_is_rejected_on_every_write_route(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let specs = seed_query_fixtures(&pool).await;
    let before = write_surface_counts(&pool).await;
    assert_eq!(
        before,
        (1, 1, 1, 0),
        "fixture surface: 1 artifact, 1 reference, 1 job"
    );

    for spec in &specs {
        let (status, value) = send_with_query_suffix(&base, &verifier, spec, "?x=1").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{} must reject ?x=1: {value}",
            spec.label
        );
        assert_eq!(value["error"]["code"], "INVALID_REQUEST", "{}", spec.label);
        // 副作用门未触达的可观察证明：不是 503（storage/generator 门）、
        // 不是 200/201/204（写入）、不是 409（幂等仲裁）。
        assert_ne!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{} gate must not be reached",
            spec.label
        );
        if matches!(spec.label, "W3" | "W8") {
            assert_eq!(
                value["error"]["code"], "INVALID_REQUEST",
                "{}: 400 (not the gate's 503) proves the storage/generator gate was never reached",
                spec.label
            );
        }
    }

    let after = write_surface_counts(&pool).await;
    assert_eq!(after, before, "query rejections must leave zero DB writes");
}

#[sqlx::test(migrations = "../migrations")]
async fn f2_bare_empty_query_is_rejected_on_every_write_route(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let specs = seed_query_fixtures(&pool).await;
    let before = write_surface_counts(&pool).await;

    for spec in &specs {
        // 裸 `?`：解析后的 query map 为空，但原始 URI 的 query 存在性必须
        // 保留并拒绝（Uri::query() == Some("")）——这正是不能用解析后
        // query map 判断的原因。
        let (status, value) = send_with_query_suffix(&base, &verifier, spec, "?").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{} must reject bare '?': {value}",
            spec.label
        );
        assert_eq!(value["error"]["code"], "INVALID_REQUEST", "{}", spec.label);
    }

    let after = write_surface_counts(&pool).await;
    assert_eq!(
        after, before,
        "bare-query rejections must leave zero DB writes"
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn f3_positive_control_without_query_still_passes_query_gate(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let specs = seed_query_fixtures(&pool).await;
    // 同 token 同 body、不带 query：通过 query 门，按各路由语义到达既定结果
    // （证明 F1/F2 的 400 确由 query 触发，而非绑定或路由问题；同时证明
    // 变异敏感性——若移除 query 检查，F1/F2 的请求会像这里一样成功）。
    for spec in &specs {
        let (status, value) = send_with_query_suffix(&base, &verifier, spec, "").await;
        match spec.label {
            "W1" | "W6" => assert_eq!(status, StatusCode::CREATED, "{}: {value}", spec.label),
            "W2" | "W5" | "W9" => assert_eq!(status, StatusCode::OK, "{}: {value}", spec.label),
            "W7" => assert_eq!(status, StatusCode::NO_CONTENT, "{}: {value}", spec.label),
            // 功能门类：503（query 门未拦截无 query 的合法请求）。
            "W3" | "W4" | "W8" => {
                assert_eq!(
                    status,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "{}: {value}",
                    spec.label
                );
            }
            other => panic!("unexpected spec {other}"),
        }
    }
}

// ── G 组：W8 目标绑定（artifact_id claim ↔ 路径 artifact ID）（P2）────

#[sqlx::test(migrations = "../migrations")]
async fn g1_w8_artifact_id_claim_matching_path_passes_target_binding(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "g1").await;
    let path = format!("/v1/artifacts/{artifact}/generation-jobs");
    let caller = Uuid::new_v4();
    let body = json!({ "generator_id": "seecode", "caller_request_id": caller, "input": common::minimal_input() });
    // artifact_id claim == 路径 artifact → 目标绑定通过 → 到达 generator 门。
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &path,
        &body,
        Some(caller),
        Some(artifact),
    );
    let (status, value) = send(
        req_for("POST", format!("{base}{path}"), &token, Some(PROJECT_1)),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{value}");
    assert_eq!(
        value["error"]["code"], "GENERATOR_UNAVAILABLE",
        "503 functional gate, not a 403 binding denial"
    );
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.generation_job")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        jobs, 0,
        "gate is functionally closed; no job may be created"
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn g2_w8_artifact_id_claim_mismatch_is_403_with_no_side_effects(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "g2").await;
    let other = Uuid::new_v4();
    let path = format!("/v1/artifacts/{artifact}/generation-jobs");
    let caller = Uuid::new_v4();
    let body = json!({ "generator_id": "seecode", "caller_request_id": caller, "input": common::minimal_input() });
    // artifact_id claim 绑定到别的 artifact → 403 绑定拒绝（≠ 503 功能门），
    // generator 门未触达，零 DB 写入。
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &path,
        &body,
        Some(caller),
        Some(other),
    );
    let (status, value) = send(
        req_for("POST", format!("{base}{path}"), &token, Some(PROJECT_1)),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
    assert_eq!(value["error"]["code"], "SCOPE_DENIED");
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.generation_job")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs, 0, "denied binding leaves no DB write");
}

#[sqlx::test(migrations = "../migrations")]
async fn g3_w8_artifact_id_claim_absent_follows_frozen_contract_shape(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "g3").await;
    let path = format!("/v1/artifacts/{artifact}/generation-jobs");
    let caller = Uuid::new_v4();
    let body = json!({ "generator_id": "seecode", "caller_request_id": caller, "input": common::minimal_input() });
    // artifact_id claim 缺省：契约 §3.1.2 #1 冻结规则是"存在时校验"，
    // §3.1.1 示例中 generation:request 携带 artifact_id:null 合法（决策项 D4：
    // 必填性属签发侧契约、未冻结——本测试如实记录现状，未新增必填拒绝）。
    // caller+hash 轴仍完整绑定该请求（路径 artifact ID 进指纹）。
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &path,
        &body,
        Some(caller),
        None,
    );
    let (status, value) = send(
        req_for("POST", format!("{base}{path}"), &token, Some(PROJECT_1)),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{value}");
    assert_eq!(
        value["error"]["code"], "GENERATOR_UNAVAILABLE",
        "functional gate, not a binding denial"
    );
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.generation_job")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs, 0);
}
