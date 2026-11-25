//! Consumer Contract Tests（ART-01 §10.1/§10.3）：AI-PM 未来通过
//! `artifact-client` SDK 消费本服务；SDK 的调用方式与错误语义必须与
//! OpenAPI 规范一致。

mod common;

use artifact_client::{ArtifactClient, ArtifactClientConfig, CallContext};
use artifact_contract::{
    CreateArtifactRequest, CreateGenerationJobRequest, CreateReferenceRequest, ListArtifactsQuery,
    SetCurrentVersionRequest, UpdateArtifactRequest,
};

fn client(base: &str) -> ArtifactClient {
    ArtifactClient::new(ArtifactClientConfig::new(base)).unwrap()
}

#[sqlx::test(migrations = "../migrations")]
async fn sdk_full_flow_matches_contract(pool: sqlx::PgPool) {
    let verifier = common::verifier_with(&[(
        "sdk-tok",
        common::TENANT,
        common::PROJECT_1,
        &common::all_actions(),
    )]);
    let base = common::spawn_app(pool, verifier).await;
    let client = client(&base);
    let ctx = CallContext::new()
        .token("sdk-tok")
        .project(common::PROJECT_1);

    // 创建 → 读取 → 更新 → 列表。
    let created = client
        .create_artifact(
            CreateArtifactRequest {
                project_id: common::PROJECT_1,
                name: "SDK flow".into(),
                artifact_type: "diagram".into(),
                artifact_key: None,
                description: None,
                category: None,
            },
            &ctx,
        )
        .await
        .unwrap();
    let got = client.get_artifact(created.id, &ctx).await.unwrap();
    assert_eq!(got.id, created.id);
    let patched = client
        .patch_artifact(
            created.id,
            UpdateArtifactRequest {
                name: Some("SDK renamed".into()),
                description: None,
                category: None,
            },
            &ctx.clone().if_match(&created.etag),
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
    let reference = client
        .create_reference(
            created.id,
            CreateReferenceRequest {
                owner_type: "requirement".into(),
                owner_id: "req-1".into(),
                relation_type: "supports".into(),
            },
            &ctx,
        )
        .await
        .unwrap();
    client
        .remove_reference(created.id, reference.id, &ctx)
        .await
        .unwrap();

    // current-version：SDK 侧同样只允许 READY（无版本 → 404）。
    let err = client
        .set_current_version(
            created.id,
            SetCurrentVersionRequest {
                version_id: uuid::Uuid::new_v4(),
                expected_current_version_id: None,
            },
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "RESOURCE_NOT_FOUND");

    // 生成请求：输入信封合法，但 Adapter 未实现 → GENERATOR_UNAVAILABLE，且不可重试。
    let generation = client
        .request_generation(
            created.id,
            CreateGenerationJobRequest {
                generator_id: "seecode".into(),
                input: serde_json::json!({
                    "schema": "smark.seecode.input",
                    "schema_version": "1",
                    "diagram": {"type": "architecture", "title": "SDK", "nodes": [], "edges": []}
                }),
                options: None,
                expected_current_version_id: None,
            },
            &ctx,
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
    // 文档化的元端点：SDK 无凭证可用。
    client.health().await.unwrap();
    client.generators().await.unwrap();
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
