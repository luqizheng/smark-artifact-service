//! WP-LMMS-DELEGATION-04：操作绑定（V10）与逐操作幂等的对抗性集成测试。
//!
//! 覆盖矩阵（任务书 §7）：
//! * 9 条写路由的正确绑定正向 + method/path/目标/父 ID/body/租户/项目/操作
//!   类型逐轴变异拒绝；
//! * caller_request_id / request_hash 缺失与伪造拒绝（服务层防线，独立于
//!   验证器 401 红线——`TestDelegationVerifier` 不实现 PC-O5-1，因此本文件
//!   同时证明服务层即使面对"验证器放行"的未绑定 token 也失败关闭）；
//! * 同 token 同请求重放安全（幂等收敛）、同 caller 异指纹拒绝；
//! * 真实 PostgreSQL 并发重复恰一次、提交前失败零残留、响应丢失重试确定、
//!   legacy 错误快照确定性拒绝；
//! * 绑定拒绝先于任何副作用（storage/generator 门从未被触达、零 DB 痕迹）。
//!
//! 变异敏感性说明：D 组断言的 403 全部来自 `enforce_write_binding`——移除或
//! 绕过该调用，动作+作用域检查仍然放行，写入会成功，这些测试随即失败。

mod common;

use artifact_domain::Action;
use artifact_infrastructure::test_support::{TestDelegationVerifier, TestTokenBinding};
use reqwest::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x0401);
const PROJECT_1: Uuid = Uuid::from_u128(0x0411);
const PROJECT_2: Uuid = Uuid::from_u128(0x0412);
const TENANT_OTHER: Uuid = Uuid::from_u128(0x0402);
const ADAPTER: &str = "wp04-adapter";

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

/// 全轴受控铸造：caller 缺省随机、hash 缺省按 v1 对 (method,path,body) 计算。
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
    operation_id: Option<&str>,
    hash_override: Option<String>,
) -> String {
    let caller = caller.unwrap_or_else(Uuid::new_v4);
    let hash = hash_override.unwrap_or_else(|| common::v1_fingerprint(method, path, caller, body));
    let token = format!("wp04-{}", Uuid::new_v4());
    common::mint_bound_token(
        verifier,
        &token,
        tenant,
        project,
        actions,
        TestTokenBinding {
            artifact_id,
            operation_id: operation_id.map(str::to_string),
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
        "GET" => client.get(url),
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

async fn count_artifacts(pool: &PgPool, project: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM artifact.artifact WHERE tenant_id=$1 AND project_id=$2",
    )
    .bind(TENANT)
    .bind(project)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn idempotency_row_count(pool: &PgPool, key: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM artifact.idempotency_record WHERE idempotency_key = $1",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ── A 组：9 条写路由的正确绑定正向 ─────────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn a1_keyed_create_with_exact_binding_succeeds(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let body = create_body(PROJECT_1, "a1", Some("a1.key"));
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &body,
        None,
        None,
        None,
        None,
    );
    let (status, artifact) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts"),
            &token,
            Some(PROJECT_1),
        ),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{artifact}");
    assert_eq!(artifact["artifact_key"], "a1.key");
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn a2_unkeyed_create_with_exact_binding_succeeds(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let body = create_body(PROJECT_1, "a2", None);
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &body,
        None,
        None,
        None,
        None,
    );
    let (status, artifact) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts"),
            &token,
            Some(PROJECT_1),
        ),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{artifact}");
}

#[sqlx::test(migrations = "../migrations")]
async fn a3_patch_with_exact_binding_succeeds(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "a3").await;
    let path = format!("/v1/artifacts/{artifact}");
    let body = json!({"name": "renamed"});
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "PATCH",
        &path,
        &body,
        None,
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for("PATCH", format!("{base}{path}"), &token, Some(PROJECT_1))
            .header("if-match", "\"r1\""),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["name"], "renamed");
}

#[sqlx::test(migrations = "../migrations")]
async fn a4_set_current_with_exact_binding_succeeds(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "a4").await;
    let version = common::sql_insert_version(&pool, TENANT, PROJECT_1, artifact, "ready").await;
    let path = format!("/v1/artifacts/{artifact}/current-version");
    let body = json!({"version_id": version, "expected_current_version_id": Value::Null});
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &path,
        &body,
        None,
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for("POST", format!("{base}{path}"), &token, Some(PROJECT_1)),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["current_version_id"], version.to_string());
}

#[sqlx::test(migrations = "../migrations")]
async fn a5_create_reference_with_exact_binding_succeeds(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "a5").await;
    let path = format!("/v1/artifacts/{artifact}/references");
    let body = json!({"owner_type": "adr", "owner_id": "adr-1", "relation_type": "illustrates"});
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &path,
        &body,
        None,
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for("POST", format!("{base}{path}"), &token, Some(PROJECT_1)),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
}

#[sqlx::test(migrations = "../migrations")]
async fn a6_remove_reference_with_exact_binding_succeeds(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "a6").await;
    let reference: Uuid = sqlx::query_scalar(
        "INSERT INTO artifact.artifact_reference \
         (id, tenant_id, project_id, artifact_id, owner_type, owner_id, relation_type, created_by) \
         VALUES ($1, $2, $3, $4, 'adr', 'adr-2', 'illustrates', 'fixture') RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(TENANT)
    .bind(PROJECT_1)
    .bind(artifact)
    .fetch_one(&pool)
    .await
    .unwrap();
    let path = format!("/v1/artifacts/{artifact}/references/{reference}");
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "DELETE",
        &path,
        &Value::Null,
        None,
        None,
        None,
        None,
    );
    let (status, _) = send(
        req_for("DELETE", format!("{base}{path}"), &token, Some(PROJECT_1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrations = "../migrations")]
async fn a7_request_generation_with_exact_binding_reaches_generator_gate(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "a7").await;
    let path = format!("/v1/artifacts/{artifact}/generation-jobs");
    let caller = Uuid::new_v4();
    let body = json!({"generator_id": "unregistered-gen", "caller_request_id": caller, "input": common::minimal_input()});
    // 请求侧 caller = body.caller_request_id，与 claim 交叉比对。
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
        None,
        None,
    );
    let (status, value) = send(
        req_for("POST", format!("{base}{path}"), &token, Some(PROJECT_1)),
        Some(&body),
    )
    .await;
    // 绑定已通过（先于 generator 门）；未注册生成器 → 400 未知生成器。
    // 关键断言：不是 403——绑定不是本请求的失败原因。
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert_eq!(value["error"]["code"], "INVALID_REQUEST");
}

#[sqlx::test(migrations = "../migrations")]
async fn a8_cancel_with_exact_binding_succeeds(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "a8").await;
    common::sql_enable_seecode(&pool, ADAPTER).await;
    let job = common::sql_create_job(
        &pool,
        TENANT,
        PROJECT_1,
        artifact,
        &common::JobOptions::default(),
    )
    .await;
    let path = format!("/v1/generation-jobs/{job}/cancel");
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &path,
        &Value::Null,
        None,
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for("POST", format!("{base}{path}"), &token, Some(PROJECT_1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["status"], "cancelled");
}

#[sqlx::test(migrations = "../migrations")]
async fn a9_version_session_routes_binding_passes_then_storage_blocks(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "a9").await;

    // W3：正确绑定 → 通过服务绑定检查 → 存储门 503（WP-AS-05 后：绑定通过
    // → 输入校验需要 source_object_id/sha256（齐备）→ 默认装配仍是
    // ProtocolBlocked → 503 STORAGE_UNAVAILABLE，不是 403）。
    let path = format!("/v1/artifacts/{artifact}/versions");
    let body = json!({
        "source_format": "yaml",
        "source_object_id": uuid::Uuid::new_v4().to_string(),
        "source_sha256": common::sha256_hex("wp04-a9"),
    });
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &path,
        &body,
        None,
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for("POST", format!("{base}{path}"), &token, Some(PROJECT_1)),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{value}");
    assert_eq!(value["error"]["code"], "STORAGE_UNAVAILABLE");

    // W4：正确绑定 → 同样到达存储门（WP-AS-05：pending 版本需带声明对象；
    // 委托缺 lmms_tid → 对象面路由 fail-closed 503，语义不变）。
    let version = common::sql_insert_version(&pool, TENANT, PROJECT_1, artifact, "pending").await;
    sqlx::query(
        "UPDATE artifact.artifact_version \
         SET source_object_id = $2, source_sha256 = $3 WHERE id = $1",
    )
    .bind(version)
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(common::sha256_hex("wp04-a9-w4"))
    .execute(&pool)
    .await
    .unwrap();
    let commit_path = format!("/v1/artifacts/{artifact}/versions/{version}/commit");
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &commit_path,
        &Value::Null,
        None,
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for(
            "POST",
            format!("{base}{commit_path}"),
            &token,
            Some(PROJECT_1),
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{value}");
}

// ── B 组：绑定轴逐项变异拒绝 ───────────────────────────────────────────

async fn seed_two_artifacts(pool: &PgPool) -> (Uuid, Uuid) {
    let a = common::sql_insert_artifact(pool, TENANT, PROJECT_1, "mut-a").await;
    let b = common::sql_insert_artifact(pool, TENANT, PROJECT_1, "mut-b").await;
    (a, b)
}

#[sqlx::test(migrations = "../migrations")]
async fn b1_token_for_other_target_is_rejected(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let (a, b) = seed_two_artifacts(&pool).await;
    // token 为 artifact A 的 patch 铸造，却用于 patch artifact B（路径不同 → 指纹不同）。
    let path_a = format!("/v1/artifacts/{a}");
    let body = json!({"name": "x"});
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "PATCH",
        &path_a,
        &body,
        None,
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for(
            "PATCH",
            format!("{base}/v1/artifacts/{b}"),
            &token,
            Some(PROJECT_1),
        )
        .header("if-match", "\"r1\""),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
    assert_eq!(value["error"]["code"], "SCOPE_DENIED");
}

#[sqlx::test(migrations = "../migrations")]
async fn b2_method_mutation_is_rejected(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "b2").await;
    let path = format!("/v1/artifacts/{artifact}");
    let body = json!({"name": "x"});
    // 为 PATCH 铸造，按 PATCH 发送会成功；这里证明 POST 指纹不匹配。
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &path,
        &body,
        None,
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for("PATCH", format!("{base}{path}"), &token, Some(PROJECT_1))
            .header("if-match", "\"r1\""),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
}

#[sqlx::test(migrations = "../migrations")]
async fn b3_body_mutation_is_rejected(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "b3").await;
    let path = format!("/v1/artifacts/{artifact}");
    let minted = json!({"name": "minted"});
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "PATCH",
        &path,
        &minted,
        None,
        None,
        None,
        None,
    );
    // 同 token、同路径、不同 body。
    let (status, value) = send(
        req_for("PATCH", format!("{base}{path}"), &token, Some(PROJECT_1))
            .header("if-match", "\"r1\""),
        Some(&json!({"name": "tampered"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
}

#[sqlx::test(migrations = "../migrations")]
async fn b4_same_action_different_target_is_rejected(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let (a, b) = seed_two_artifacts(&pool).await;
    // 同为 reference:write，token 为 A 的引用铸造，用于 B。
    let ref_path_a = format!("/v1/artifacts/{a}/references");
    let body = json!({"owner_type": "adr", "owner_id": "o", "relation_type": "illustrates"});
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &ref_path_a,
        &body,
        None,
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts/{b}/references"),
            &token,
            Some(PROJECT_1),
        ),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
    // 零写入。
    let refs: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.artifact_reference")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(refs, 0, "denied binding must leave no side effect");
}

#[sqlx::test(migrations = "../migrations")]
async fn b5_artifact_id_claim_mismatch_and_creation_route_rejected(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let (a, b) = seed_two_artifacts(&pool).await;

    // patch B 用 artifact_id=A 绑定的 token → 403。
    let path_b = format!("/v1/artifacts/{b}");
    let body = json!({"name": "x"});
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "PATCH",
        &path_b,
        &body,
        None,
        Some(a),
        None,
        None,
    );
    let (status, value) = send(
        req_for("PATCH", format!("{base}{path_b}"), &token, Some(PROJECT_1))
            .header("if-match", "\"r1\""),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");

    // patch B 用 artifact_id=B 绑定的 token → 200（绑定轴真正生效的正面证明）。
    let token_ok = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "PATCH",
        &path_b,
        &body,
        None,
        Some(b),
        None,
        None,
    );
    let (status, value) = send(
        req_for(
            "PATCH",
            format!("{base}{path_b}"),
            &token_ok,
            Some(PROJECT_1),
        )
        .header("if-match", "\"r1\""),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");

    // 创建路由（无目标）携带 artifact_id → 字面 fail-closed（裁决 D2）。
    let create = create_body(PROJECT_1, "b5-new", None);
    let token_create = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &create,
        None,
        Some(a),
        None,
        None,
    );
    let (status, value) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts"),
            &token_create,
            Some(PROJECT_1),
        ),
        Some(&create),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
}

#[sqlx::test(migrations = "../migrations")]
async fn b6_cancel_operation_id_must_match_job(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "b6").await;
    common::sql_enable_seecode(&pool, ADAPTER).await;
    let job = common::sql_create_job(
        &pool,
        TENANT,
        PROJECT_1,
        artifact,
        &common::JobOptions::default(),
    )
    .await;
    let other_job = Uuid::new_v4();
    let path = format!("/v1/generation-jobs/{job}/cancel");

    // 绑定到别的 job → 403。
    let wrong = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &path,
        &Value::Null,
        None,
        None,
        Some(&format!("generation_job:{other_job}")),
        None,
    );
    let (status, value) = send(
        req_for("POST", format!("{base}{path}"), &wrong, Some(PROJECT_1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
    assert_eq!(value["error"]["code"], "SCOPE_DENIED");

    // 绑定正确（契约 §5.3 语法 generation_job:{job_id}）→ 200 cancelled。
    let right = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &path,
        &Value::Null,
        None,
        None,
        Some(&format!("generation_job:{job}")),
        None,
    );
    let (status, value) = send(
        req_for("POST", format!("{base}{path}"), &right, Some(PROJECT_1)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["status"], "cancelled");
}

#[sqlx::test(migrations = "../migrations")]
async fn b7_write_token_without_caller_or_hash_is_rejected_by_service(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let body = create_body(PROJECT_1, "b7", None);

    // ① 验证器红线被绕过（无 caller、无 hash 的 token 直达服务层）→ 服务层 403。
    let naked = format!("wp04-naked-{}", Uuid::new_v4());
    common::mint_bound_token(
        &verifier,
        &naked,
        TENANT,
        PROJECT_1,
        write_actions(),
        TestTokenBinding::default(),
    );
    let (status, value) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts"),
            &naked,
            Some(PROJECT_1),
        ),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");

    // ② caller 有、hash 缺（半配对）→ 403。
    let caller_only = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &body,
        None,
        None,
        None,
        Some(String::new()),
    );
    // 上面 hash_override 为空串：服务层比较必败（64hex 才可能匹配）。
    let (status, value) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts"),
            &caller_only,
            Some(PROJECT_1),
        ),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");

    // ③ hash 被伪造（合法 64hex 但不是本请求的指纹）→ 403。
    let forged = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &body,
        None,
        None,
        None,
        Some("f".repeat(64)),
    );
    let (status, value) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts"),
            &forged,
            Some(PROJECT_1),
        ),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn b8_cross_project_and_cross_tenant_writes_with_bound_tokens_are_denied(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    // 跨项目：token scope = PROJECT_1，请求体/头指向 PROJECT_2（绑定正确、scope 拒绝）。
    let body_p2 = create_body(PROJECT_2, "b8-p2", None);
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &body_p2,
        None,
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts"),
            &token,
            Some(PROJECT_2),
        ),
        Some(&body_p2),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
    assert_eq!(value["error"]["code"], "SCOPE_DENIED");

    // 跨租户：token scope = TENANT_OTHER，目标 PROJECT_1（另一租户的命名空间）
    // → require 拒绝；PROJECT_1 零写入，PROJECT_2 亦无资源（无跨租户泄漏）。
    let body_t1 = create_body(PROJECT_1, "b8-t1", None);
    let token = mint(
        &verifier,
        TENANT_OTHER,
        PROJECT_2,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &body_t1,
        None,
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts"),
            &token,
            Some(PROJECT_1),
        ),
        Some(&body_t1),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
    assert_eq!(value["error"]["code"], "SCOPE_DENIED");
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 0);
    assert_eq!(count_artifacts(&pool, PROJECT_2).await, 0);
}

// ── C 组：冻结向量跨层一致性 ───────────────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn c1_frozen_vector_passes_over_http_cross_layer(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    // WP-03 冻结向量 1（delegation_request.rs v1_frozen_vector_minimal_body）：
    // caller 0b2e5f1a-9c4d-4e8a-b7f6-2a1d3c5e7f90 + POST /v1/artifacts +
    // {"artifact_type":"visualization","name":"wp03-vector"} → 64cc551d…。
    let caller = Uuid::parse_str("0b2e5f1a-9c4d-4e8a-b7f6-2a1d3c5e7f90").unwrap();
    let vector_body = json!({"artifact_type": "visualization", "name": "wp03-vector"});
    // ① 冻结向量（claim 级）：测试侧实现与 WP-03 冻结常量逐字节一致。
    let frozen = common::v1_fingerprint("POST", "/v1/artifacts", caller, &vector_body);
    assert_eq!(
        frozen, "64cc551d71fd091999486c83377cc492b2e38aa5946d23c93d771f49de5f4d8f",
        "test-side computation must equal the frozen vector"
    );
    // ② 跨层证明（HTTP 级）：完整 body（含 project_id，DTO 必填）+ v1 指纹
    //    作为 claim → 真实服务端按同一规范重算比对，必须一致放行。
    let body = create_body(PROJECT_1, "wp03-vector-cross-layer", None);
    let expected = common::v1_fingerprint("POST", "/v1/artifacts", caller, &body);
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &body,
        Some(caller),
        None,
        None,
        Some(expected),
    );
    let (status, value) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts"),
            &token,
            Some(PROJECT_1),
        ),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
}

// ── D 组：重放 / 并发 / 失败恢复 ───────────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn d1_same_token_same_request_retries_safely_keyed(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let body = create_body(PROJECT_1, "d1", Some("d1.key"));
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &body,
        None,
        None,
        None,
        None,
    );
    let send_create = || {
        let base = base.clone();
        let token = token.clone();
        let body = body.clone();
        async move {
            send(
                req_for(
                    "POST",
                    format!("{base}/v1/artifacts"),
                    &token,
                    Some(PROJECT_1),
                )
                .header("idempotency-key", "d1-key"),
                Some(&body),
            )
            .await
        }
    };
    let (s1, v1) = send_create().await;
    assert_eq!(s1, StatusCode::CREATED, "{v1}");
    // 响应丢失后的合法重试：同 caller 同指纹 + 同幂等键 → 幂等收敛。
    let (s2, v2) = send_create().await;
    assert_eq!(s2, StatusCode::OK, "{v2}");
    assert_eq!(v2["id"], v1["id"], "retry must resolve the same artifact");
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn d2_same_caller_different_fingerprint_is_rejected(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "d2").await;
    let path = format!("/v1/artifacts/{artifact}");
    let caller = Uuid::new_v4();
    // token 铸造给 body {"name":"first"}。
    let first = json!({"name": "first"});
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "PATCH",
        &path,
        &first,
        Some(caller),
        None,
        None,
        None,
    );
    let (status, _) = send(
        req_for("PATCH", format!("{base}{path}"), &token, Some(PROJECT_1))
            .header("if-match", "\"r1\""),
        Some(&first),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // 同 caller 的 token 被拿去提交不同 body → 指纹不匹配 → 403。
    let second = json!({"name": "second"});
    let (status, value) = send(
        req_for("PATCH", format!("{base}{path}"), &token, Some(PROJECT_1))
            .header("if-match", "\"r2\""),
        Some(&second),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
    // 同 token 同 body 重放（新 If-Match）：CAS 收敛为 409，确定性且安全。
    let (status, value) = send(
        req_for("PATCH", format!("{base}{path}"), &token, Some(PROJECT_1))
            .header("if-match", "\"r1\""),
        Some(&first),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert_eq!(value["error"]["code"], "VERSION_CONFLICT");
}

#[sqlx::test(migrations = "../migrations")]
async fn d3_unkeyed_idempotency_replay_is_atomic_single_tx(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let body = create_body(PROJECT_1, "d3", None);
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &body,
        None,
        None,
        None,
        None,
    );
    let send_create = || {
        let base = base.clone();
        let token = token.clone();
        let body = body.clone();
        async move {
            let (status, value, replay, resolution) = {
                let res = req_for(
                    "POST",
                    format!("{base}/v1/artifacts"),
                    &token,
                    Some(PROJECT_1),
                )
                .header("idempotency-key", "d3-key")
                .json(&body)
                .send()
                .await
                .unwrap();
                let replay = res
                    .headers()
                    .get("x-idempotent-replay")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let resolution = res
                    .headers()
                    .get("x-artifact-resolution")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let status = res.status();
                let value: Value = res.json().await.unwrap_or(Value::Null);
                (status, value, replay, resolution)
            };
            (status, value, replay, resolution)
        }
    };
    let (s1, _, r1, res1) = send_create().await;
    assert_eq!(s1, StatusCode::CREATED);
    assert_eq!(res1.as_deref(), Some("created"));
    assert_eq!(r1, None);
    let (s2, _, r2, res2) = send_create().await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(res2.as_deref(), Some("idempotency_replay"));
    assert_eq!(r2.as_deref(), Some("true"));
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn d4_unkeyed_concurrent_duplicates_yield_exactly_one_artifact(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let body = create_body(PROJECT_1, "d4", None);
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &body,
        None,
        None,
        None,
        None,
    );
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let base = base.clone();
        let token = token.clone();
        let body = body.clone();
        tasks.push(tokio::spawn(async move {
            let res = reqwest::Client::new()
                .post(format!("{base}/v1/artifacts"))
                .header("authorization", format!("Bearer {token}"))
                .header("x-project-id", PROJECT_1.to_string())
                .header("idempotency-key", "d4-shared")
                .json(&body)
                .send()
                .await
                .unwrap();
            res.status()
        }));
    }
    let mut created = 0;
    let mut replayed = 0;
    let mut conflicts = 0;
    for task in tasks {
        match task.await.unwrap() {
            StatusCode::CREATED => created += 1,
            StatusCode::OK => replayed += 1,
            StatusCode::CONFLICT => conflicts += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(created, 1, "exactly one fresh creation");
    assert_eq!(
        conflicts, 0,
        "same fingerprint duplicates must not conflict"
    );
    assert_eq!(created + replayed, 8);
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn d5_keyed_concurrent_duplicates_one_logical_write(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let body = create_body(PROJECT_1, "d5", Some("d5.key"));
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &body,
        None,
        None,
        None,
        None,
    );
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let base = base.clone();
        let token = token.clone();
        let body = body.clone();
        tasks.push(tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/artifacts"))
                .header("authorization", format!("Bearer {token}"))
                .header("x-project-id", PROJECT_1.to_string())
                .json(&body)
                .send()
                .await
                .unwrap()
                .status()
        }));
    }
    let mut created = 0;
    let mut resolved = 0;
    for task in tasks {
        match task.await.unwrap() {
            StatusCode::CREATED => created += 1,
            StatusCode::OK => resolved += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(created, 1);
    assert_eq!(created + resolved, 8);
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    let identities: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.artifact_creation_identity WHERE tenant_id=$1 AND project_id=$2",
    )
    .bind(TENANT)
    .bind(PROJECT_1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(identities, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn d6_failure_before_commit_leaves_no_idempotency_trace(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    // 故障注入：名为 INJECT-FAIL 的 Artifact 插入时触发异常（单事务内失败）。
    sqlx::query(
        "CREATE FUNCTION artifact.wp04_inject_fail() RETURNS trigger LANGUAGE plpgsql \
         AS 'BEGIN IF NEW.name = ''INJECT-FAIL'' THEN RAISE EXCEPTION ''wp04 injected failure''; END IF; RETURN NEW; END;'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER wp04_inject BEFORE INSERT ON artifact.artifact \
         FOR EACH ROW EXECUTE FUNCTION artifact.wp04_inject_fail()",
    )
    .execute(&pool)
    .await
    .unwrap();

    let body = create_body(PROJECT_1, "INJECT-FAIL", None);
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &body,
        None,
        None,
        None,
        None,
    );
    let (status, _) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts"),
            &token,
            Some(PROJECT_1),
        )
        .header("idempotency-key", "d6-key"),
        Some(&body),
    )
    .await;
    assert!(
        status.is_server_error() || status == StatusCode::BAD_REQUEST,
        "injected failure must surface: {status}"
    );

    // 提交前失败：幂等记录与 Artifact 都不存在（G2-1 修复的可观察结果）。
    assert_eq!(idempotency_row_count(&pool, "d6-key").await, 0);
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 0);

    // 解除注入后重试：全新尝试成功，恰一次。
    sqlx::query("DROP TRIGGER wp04_inject ON artifact.artifact")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION artifact.wp04_inject_fail()")
        .execute(&pool)
        .await
        .unwrap();
    let (status, value) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts"),
            &token,
            Some(PROJECT_1),
        )
        .header("idempotency-key", "d6-key"),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn d7_legacy_error_snapshot_is_deterministically_rejected(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    // 旧写入者遗留：错误快照（{"error":...}）且 resource_id 为空。
    // 同 key 同 body 的重放必须确定性 409（修复旧实现的 500 缺口，G2-2）。
    let body = create_body(PROJECT_1, "d7", None);
    let raw_hash = {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(&body).unwrap();
        let digest = Sha256::digest(&bytes);
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    sqlx::query(
        "INSERT INTO artifact.idempotency_record \
         (id, tenant_id, project_id, endpoint, idempotency_key, request_hash, response_snapshot, resource_id) \
         VALUES ($1, $2, $3, 'POST /v1/artifacts', 'd7-legacy', $4, $5, NULL)",
    )
    .bind(Uuid::new_v4())
    .bind(TENANT)
    .bind(PROJECT_1)
    .bind(&raw_hash)
    .bind(json!({"error": "SOME_HISTORICAL_FAILURE"}))
    .execute(&pool)
    .await
    .unwrap();

    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts",
        &body,
        None,
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts"),
            &token,
            Some(PROJECT_1),
        )
        .header("idempotency-key", "d7-legacy"),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert_eq!(value["error"]["code"], "IDEMPOTENCY_KEY_REUSED");
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn d8_cancel_retry_after_success_is_terminal_noop(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "d8").await;
    common::sql_enable_seecode(&pool, ADAPTER).await;
    let job = common::sql_create_job(
        &pool,
        TENANT,
        PROJECT_1,
        artifact,
        &common::JobOptions::default(),
    )
    .await;
    let path = format!("/v1/generation-jobs/{job}/cancel");
    let send_cancel = |verifier: &Arc<TestDelegationVerifier>| {
        let base = base.clone();
        let path = path.clone();
        // 每次重试独立铸造（重试请求各自的绑定），状态机保证收敛。
        let token = mint(
            verifier,
            TENANT,
            PROJECT_1,
            write_actions(),
            "POST",
            &path,
            &Value::Null,
            None,
            None,
            None,
            None,
        );
        async move {
            send(
                req_for("POST", format!("{base}{path}"), &token, Some(PROJECT_1)),
                None,
            )
            .await
        }
    };
    let (s1, v1) = send_cancel(&verifier).await;
    assert_eq!(s1, StatusCode::OK, "{v1}");
    assert_eq!(v1["status"], "cancelled");
    let (s2, v2) = send_cancel(&verifier).await;
    assert_eq!(s2, StatusCode::OK, "{v2}");
    assert_eq!(v2["status"], "cancelled", "terminal no-op returns the job");
    // 只有一次取消审计（第二次是终态空操作）。
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.audit_event WHERE action='generation.cancel' AND object_id=$1",
    )
    .bind(job)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audits, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn d9_concurrent_cancels_serialize_through_state_machine(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "d9").await;
    common::sql_enable_seecode(&pool, ADAPTER).await;
    let job = common::sql_create_job(
        &pool,
        TENANT,
        PROJECT_1,
        artifact,
        &common::JobOptions::default(),
    )
    .await;
    let path = format!("/v1/generation-jobs/{job}/cancel");
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let base = base.clone();
        let path = path.clone();
        let verifier = verifier.clone();
        tasks.push(tokio::spawn(async move {
            let token = mint(
                &verifier,
                TENANT,
                PROJECT_1,
                write_actions(),
                "POST",
                &path,
                &Value::Null,
                None,
                None,
                None,
                None,
            );
            send(
                req_for("POST", format!("{base}{path}"), &token, Some(PROJECT_1)),
                None,
            )
            .await
            .0
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
    }
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.audit_event WHERE action='generation.cancel' AND object_id=$1",
    )
    .bind(job)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audits, 1, "only the first transition is audited");
}

// ── E 组：绑定拒绝先于副作用 ───────────────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn e1_binding_denial_leaves_no_db_trace_and_never_reaches_gates(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "e1").await;

    // W6 引用：body 变异的 token → 403，零引用行。
    let ref_body = json!({"owner_type": "adr", "owner_id": "e1", "relation_type": "illustrates"});
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts/other/references",
        &ref_body,
        None,
        None,
        None,
        None,
    );
    let (status, _) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts/{artifact}/references"),
            &token,
            Some(PROJECT_1),
        ),
        Some(&ref_body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let refs: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.artifact_reference")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(refs, 0);

    // W3 版本会话：绑定错误 → 403（先于存储门 503，证明存储端口未被触达）。
    let session_body = json!({"source_format": "yaml"});
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts/wrong/versions",
        &session_body,
        None,
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts/{artifact}/versions"),
            &token,
            Some(PROJECT_1),
        ),
        Some(&session_body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");

    // W8 生成请求：绑定错误 → 403（先于 generator 门），caller 交叉比对不一致。
    let caller = Uuid::new_v4();
    let job_body = json!({"generator_id": "seecode", "caller_request_id": caller, "input": common::minimal_input()});
    // claim caller 与 body caller 不同：铸造时用另一个 caller。
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        "/v1/artifacts/wrong/generation-jobs",
        &job_body,
        Some(Uuid::new_v4()),
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for(
            "POST",
            format!("{base}/v1/artifacts/{artifact}/generation-jobs"),
            &token,
            Some(PROJECT_1),
        ),
        Some(&job_body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.generation_job")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn e2_job_request_caller_must_equal_token_claim(pool: PgPool) {
    let (base, verifier) = spawn(&pool).await;
    let artifact = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "e2").await;
    let path = format!("/v1/artifacts/{artifact}/generation-jobs");
    let body_caller = Uuid::new_v4();
    let body = json!({"generator_id": "seecode", "caller_request_id": body_caller, "input": common::minimal_input()});
    // token claim caller ≠ body caller → 交叉比对拒绝。
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &path,
        &body,
        Some(Uuid::new_v4()),
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for("POST", format!("{base}{path}"), &token, Some(PROJECT_1)),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{value}");
    // claim == body caller → 通过绑定，到达 generator 门：seecode 在注册表中
    // 状态为 unavailable → 503 GENERATOR_UNAVAILABLE（证明绑定先于 generator 查询）。
    let token = mint(
        &verifier,
        TENANT,
        PROJECT_1,
        write_actions(),
        "POST",
        &path,
        &body,
        Some(body_caller),
        None,
        None,
        None,
    );
    let (status, value) = send(
        req_for("POST", format!("{base}{path}"), &token, Some(PROJECT_1)),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{value}");
    assert_eq!(value["error"]["code"], "GENERATOR_UNAVAILABLE");
}
