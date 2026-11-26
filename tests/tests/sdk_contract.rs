//! Consumer Contract Tests（ART-01 §10.1/§10.3）：AI-PM 未来通过
//! `artifact-client` SDK 消费本服务；SDK 的调用方式与错误语义必须与
//! OpenAPI 规范一致。

mod common;

use artifact_client::{ArtifactClient, ArtifactClientConfig, CallContext};
use artifact_contract::{
    CreateArtifactRequest, CreateGenerationJobRequest, CreateReferenceRequest, ListArtifactsQuery,
    SetCurrentVersionRequest, UpdateArtifactRequest,
};
use uuid::Uuid;

fn client(base: &str) -> ArtifactClient {
    ArtifactClient::new(ArtifactClientConfig::new(base)).unwrap()
}

/// 为一次 SDK 写调用铸造 V10 绑定 token（WP-04）：method/path/body 指纹绑定。
fn write_ctx(
    verifier: &std::sync::Arc<artifact_infrastructure::test_support::TestDelegationVerifier>,
    project: Uuid,
    method: &str,
    path: String,
    body: &serde_json::Value,
    caller: Option<Uuid>,
) -> CallContext {
    use artifact_infrastructure::test_support::TestTokenBinding;
    let token = format!("sdk-wt-{}", Uuid::new_v4());
    let caller = caller.unwrap_or_else(Uuid::new_v4);
    let hash = common::v1_fingerprint(method, &path, caller, body);
    common::mint_bound_token(
        verifier,
        &token,
        common::TENANT,
        project,
        &common::all_actions(),
        TestTokenBinding {
            artifact_id: None,
            operation_id: None,
            caller_request_id: Some(caller),
            request_hash: Some(hash),
        },
    );
    CallContext::new().token(token).project(project)
}

#[sqlx::test(migrations = "../migrations")]
async fn sdk_full_flow_matches_contract(pool: sqlx::PgPool) {
    let verifier = common::verifier_with(&[(
        "sdk-tok",
        common::TENANT,
        common::PROJECT_1,
        &common::all_actions(),
    )]);
    let base = common::spawn_app(pool, verifier.clone()).await;
    let client = client(&base);
    let ctx = CallContext::new()
        .token("sdk-tok")
        .project(common::PROJECT_1);

    // 创建 → 读取 → 更新 → 列表。（WP-04：每次写调用一枚绑定 token。）
    let create_req = CreateArtifactRequest {
        project_id: common::PROJECT_1,
        name: "SDK flow".into(),
        artifact_type: "diagram".into(),
        artifact_key: None,
        description: None,
        category: None,
    };
    let created = client
        .create_artifact(
            create_req.clone(),
            &write_ctx(
                &verifier,
                common::PROJECT_1,
                "POST",
                "/v1/artifacts".into(),
                &serde_json::to_value(&create_req).unwrap(),
                None,
            ),
        )
        .await
        .unwrap();
    let got = client.get_artifact(created.id, &ctx).await.unwrap();
    assert_eq!(got.id, created.id);
    let patch_req = UpdateArtifactRequest {
        name: Some("SDK renamed".into()),
        description: None,
        category: None,
    };
    let patched = client
        .patch_artifact(
            created.id,
            patch_req.clone(),
            &write_ctx(
                &verifier,
                common::PROJECT_1,
                "PATCH",
                format!("/v1/artifacts/{created_id}", created_id = created.id),
                &serde_json::to_value(&patch_req).unwrap(),
                None,
            )
            .if_match(&created.etag),
        )
        .await
        .unwrap();
    assert_eq!(patched.name, "SDK renamed");
    let page = client
        .list_artifacts(
            ListArtifactsQuery {
                limit: Some(10),
                cursor: None,
                artifact_type: None,
                lifecycle_state: None,
            },
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);

    // 引用。
    let reference_req = CreateReferenceRequest {
        owner_type: "requirement".into(),
        owner_id: "req-1".into(),
        relation_type: "supports".into(),
    };
    let reference = client
        .create_reference(
            created.id,
            reference_req.clone(),
            &write_ctx(
                &verifier,
                common::PROJECT_1,
                "POST",
                format!("/v1/artifacts/{}/references", created.id),
                &serde_json::to_value(&reference_req).unwrap(),
                None,
            ),
        )
        .await
        .unwrap();
    client
        .remove_reference(
            created.id,
            reference.id,
            &write_ctx(
                &verifier,
                common::PROJECT_1,
                "DELETE",
                format!("/v1/artifacts/{}/references/{}", created.id, reference.id),
                &serde_json::Value::Null,
                None,
            ),
        )
        .await
        .unwrap();

    // current-version：SDK 侧同样只允许 READY（无版本 → 404）。
    let set_current_req = SetCurrentVersionRequest {
        version_id: uuid::Uuid::new_v4(),
        expected_current_version_id: None,
    };
    let err = client
        .set_current_version(
            created.id,
            set_current_req.clone(),
            &write_ctx(
                &verifier,
                common::PROJECT_1,
                "POST",
                format!("/v1/artifacts/{}/current-version", created.id),
                &serde_json::to_value(&set_current_req).unwrap(),
                None,
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "RESOURCE_NOT_FOUND");

    // 生成请求：输入信封合法，但 Adapter 未实现 → GENERATOR_UNAVAILABLE，且不可重试。
    // （WP-04：请求侧 caller = body.caller_request_id，与 claim 交叉比对。）
    let job_caller = uuid::Uuid::new_v4();
    let job_req = CreateGenerationJobRequest {
        generator_id: "seecode".into(),
        input: serde_json::json!({
            "schema": "smark.seecode.input",
            "schema_version": "1",
            "diagram": {"type": "architecture", "title": "SDK", "nodes": [], "edges": []}
        }),
        options: None,
        expected_current_version_id: None,
        caller_request_id: job_caller,
    };
    let generation = client
        .request_generation(
            created.id,
            job_req.clone(),
            &write_ctx(
                &verifier,
                common::PROJECT_1,
                "POST",
                format!("/v1/artifacts/{}/generation-jobs", created.id),
                &serde_json::to_value(&job_req).unwrap(),
                Some(job_caller),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(generation.code(), "GENERATOR_UNAVAILABLE");
    // 503 类错误按契约 retryable=true（Worker/Generator 未来部署后可用）。
    assert!(generation.retryable());

    // 不符合输入信封的请求在校验阶段被拒绝：稳定错误码 + 结构化的原因与位置。
    let rejected = client
        .request_generation(
            created.id,
            CreateGenerationJobRequest {
                generator_id: "seecode".into(),
                input: serde_json::json!({"schema": "aipm.vizspec"}),
                options: None,
                expected_current_version_id: None,
                caller_request_id: uuid::Uuid::new_v4(),
            },
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(rejected.code(), "INVALID_REQUEST");
    assert!(!rejected.retryable());
}

#[sqlx::test(migrations = "../migrations")]
async fn sdk_surfaces_error_envelope_fields(pool: sqlx::PgPool) {
    // 无凭证：401 + request_id 透出。
    let base = common::spawn_app_closed(pool).await;
    let client = client(&base);
    let ctx = CallContext::new().project(common::PROJECT_1);
    let err = client
        .list_artifacts(
            ListArtifactsQuery {
                limit: None,
                cursor: None,
                artifact_type: None,
                lifecycle_state: None,
            },
            &ctx,
        )
        .await
        .unwrap_err();
    match &err {
        artifact_client::SdkError::Api {
            status,
            code,
            request_id,
            retryable,
            ..
        } => {
            assert_eq!(*status, 401);
            assert_eq!(code, "UNAUTHENTICATED");
            assert!(request_id.is_some());
            assert!(!retryable);
        }
        other => panic!("expected Api error, got {other:?}"),
    }
    // 文档化的元端点：健康/就绪探针无凭证可用（WP-03 起 generators 需要
    // 委托凭证 + generation:read，不再属于无凭证面）。
    client.health().await.unwrap();
    client.ready().await.unwrap();
    // generators 无凭证同样得到 401 信封（Q3 全量 delegation）。
    let err = client.generators(&CallContext::new()).await.unwrap_err();
    match &err {
        artifact_client::SdkError::Api { status, code, .. } => {
            assert_eq!(*status, 401);
            assert_eq!(code, "UNAUTHENTICATED");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[test]
fn sdk_has_no_internal_aipm_dependencies() {
    // 静态守卫：SDK 依赖面只允许本仓库 contract 与公共栈。
    // （真实保证由 Cargo 依赖图给出：artifact-client 的 Cargo.toml 仅依赖
    //   artifact-contract / reqwest / serde / uuid / thiserror。）
    let manifest = include_str!("../../crates/artifact-client/Cargo.toml");
    assert!(!manifest.contains("pm-domain"));
    assert!(!manifest.contains("pm-application"));
    assert!(!manifest.contains("pm-infrastructure"));
    assert!(!manifest.contains("artifact-infrastructure"));
    assert!(!manifest.contains("artifact-api"));
}

// ── AS-2 SDK：AS2-39（lookup 方法）与 AS2-40（请求/响应类型） ─────────────

const SDK_CALLER: Uuid = Uuid::from_u128(0x0e01);
const SDK_FP: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

fn sdk_ctx() -> CallContext {
    CallContext::new()
        .token("sdk-tok")
        .project(common::PROJECT_1)
}

/// AS2-39：`get_generation_job_by_caller_request` 的命中 / 404 / 403 / 传输错误分类。
#[sqlx::test(migrations = "../migrations")]
async fn as2_39_sdk_caller_request_lookup(pool: sqlx::PgPool) {
    // 布景：一个带 caller identity 的 Job。
    let artifact =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "sdk").await;
    common::sql_enable_seecode(&pool, "sdk-adapter").await;
    let (job, _) = common::try_sql_create_job_with_identity(
        &pool,
        Uuid::new_v4(),
        common::TENANT,
        common::PROJECT_1,
        artifact,
        &common::JobOptions::default(),
        None,
        &common::sha256_hex("sdk-lookup"),
        Some((SDK_CALLER, SDK_FP, 1)),
    )
    .await
    .unwrap();

    let verifier = common::verifier_with(&[(
        "sdk-tok",
        common::TENANT,
        common::PROJECT_1,
        &common::all_actions(),
    )]);
    let base = common::spawn_app(pool, verifier).await;
    let sdk = client(&base);
    let ctx = sdk_ctx();

    // 命中：200 + caller_request_id。
    let found = sdk
        .get_generation_job_by_caller_request(SDK_CALLER, &ctx)
        .await
        .unwrap();
    assert_eq!(found.id, job);
    assert_eq!(found.caller_request_id, Some(SDK_CALLER));

    // 不存在：404 RESOURCE_NOT_FOUND。
    let err = sdk
        .get_generation_job_by_caller_request(Uuid::new_v4(), &ctx)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "RESOURCE_NOT_FOUND");

    // 项目不匹配：403 SCOPE_DENIED。
    let err = sdk
        .get_generation_job_by_caller_request(SDK_CALLER, &ctx.clone().project(common::PROJECT_2))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "SCOPE_DENIED");

    // 无凭证：401。
    let err = sdk
        .get_generation_job_by_caller_request(
            SDK_CALLER,
            &CallContext::new().project(common::PROJECT_1),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "UNAUTHENTICATED");

    // 传输错误（不可达地址）必须与 HTTP 错误区分。
    let dead = client("http://127.0.0.1:1");
    let err = dead
        .get_generation_job_by_caller_request(SDK_CALLER, &ctx)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "TRANSPORT_ERROR");
}

/// AS2-40：请求体必含 caller_request_id；响应可反序列化可空 caller_request_id。
#[sqlx::test(migrations = "../migrations")]
async fn as2_40_sdk_types_require_caller_identity(pool: sqlx::PgPool) {
    // 序列化：caller_request_id 必然出现在 JSON 中（类型层必填）。
    let request = CreateGenerationJobRequest {
        generator_id: "seecode".into(),
        input: common::minimal_input(),
        options: None,
        expected_current_version_id: None,
        caller_request_id: SDK_CALLER,
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["caller_request_id"], SDK_CALLER.to_string());

    // 反序列化：legacy Job 的 caller_request_id 为 null 时仍可解析。
    let legacy = serde_json::json!({
        "id": Uuid::new_v4().to_string(),
        "artifact_id": Uuid::new_v4().to_string(),
        "generator_id": "seecode",
        "status": "queued",
        "error_code": null,
        "attempt_count": 0,
        "max_attempts": 3,
        "caller_request_id": null,
        "cancel_requested": false,
        "created_at": "2026-01-01T00:00:00Z",
        "poll_url": "/v1/generation-jobs/x"
    });
    let parsed: artifact_contract::GenerationJobResponse = serde_json::from_value(legacy).unwrap();
    assert_eq!(parsed.caller_request_id, None);

    // 布景无关：确认 SDK 在缺少 caller_request_id 的请求上行为一致（400）。
    let verifier = common::verifier_with(&[(
        "sdk-tok",
        common::TENANT,
        common::PROJECT_1,
        &common::all_actions(),
    )]);
    let base = common::spawn_app(pool, verifier).await;
    let res = reqwest::Client::new()
        .post(format!(
            "{base}/v1/artifacts/{}/generation-jobs",
            Uuid::new_v4()
        ))
        .header("x-project-id", common::PROJECT_1.to_string())
        .header("authorization", "Bearer sdk-tok")
        .json(&serde_json::json!({
            "generator_id": "seecode",
            "input": common::minimal_input()
        }))
        .send()
        .await
        .unwrap();
    assert!(
        res.status() == 400 || res.status() == 422,
        "a request without caller_request_id must be rejected, got {}",
        res.status()
    );
}
