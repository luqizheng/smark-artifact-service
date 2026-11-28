//! 健康检查 / 就绪检查（无凭证只读面）/ Generator 注册表（WP-03 起要求
//! 委托凭证 + `generation:read`，Q3"所有受保护 API 均要求 delegation"）。

mod common;

use artifact_client::{ArtifactClient, ArtifactClientConfig, CallContext};
use artifact_domain::Action;

#[sqlx::test(migrations = "../migrations")]
async fn health_reports_build_info(pool: sqlx::PgPool) {
    let base = common::spawn_app_closed(pool).await;
    let client = ArtifactClient::new(ArtifactClientConfig::new(&base)).unwrap();
    let health = client.health().await.unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.service, "artifact-api");
    assert!(!health.version.is_empty());
    assert!(!health.git_sha.is_empty());
}

#[sqlx::test(migrations = "../migrations")]
async fn ready_reports_database_and_blocked_storage(pool: sqlx::PgPool) {
    let base = common::spawn_app_closed(pool).await;
    let client = ArtifactClient::new(ArtifactClientConfig::new(&base)).unwrap();
    let ready = client.ready().await.unwrap();
    assert_eq!(ready.status, "ready");
    assert_eq!(ready.components.database, "ok");
    // 诚实上报：file-service 上传协议被 ART-02 存储 P0 阻断。
    assert_eq!(ready.components.file_storage, "protocol_blocked_p0");
}

#[sqlx::test(migrations = "../migrations")]
async fn generators_report_unavailable_adapters(pool: sqlx::PgPool) {
    let verifier = common::verifier_with(&[(
        common::GENERATORS_TOKEN,
        common::TENANT,
        common::PROJECT_1,
        &[Action::GenerationRead],
    )]);
    let base = common::spawn_app(pool, verifier).await;
    let client = ArtifactClient::new(ArtifactClientConfig::new(&base)).unwrap();
    let ctx = CallContext::new().token(common::GENERATORS_TOKEN);
    let generators = client.generators(&ctx).await.unwrap();
    let ids: Vec<_> = generators.iter().map(|g| g.generator_id.as_str()).collect();
    // ART-03A A3A-08：Mermaid seed 由 0002 后续 migration 删除，仅在 seecode 保留。
    assert_eq!(ids, vec!["seecode"]);
    for generator in &generators {
        // WP-AS-06：SeeCode Adapter + Worker 执行循环已落地（0006 翻转 seed），
        // 注册表如实上报 available；能力真实性由执行链路测试背书。
        assert_eq!(generator.status, "available", "{}", generator.generator_id);
        assert!(generator.status_reason.is_some());
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn generators_require_delegation_and_action(pool: sqlx::PgPool) {
    // WP-03（R2-F-03 / Q3）：无凭证 → 401；有凭证但缺 generation:read → 403。
    let read_only = common::verifier_with(&[(
        common::GENERATORS_TOKEN,
        common::TENANT,
        common::PROJECT_1,
        common::read_actions(),
    )]);
    let base = common::spawn_app(pool, read_only).await;
    let client = ArtifactClient::new(ArtifactClientConfig::new(&base)).unwrap();

    let err = client.generators(&CallContext::new()).await.unwrap_err();
    match &err {
        artifact_client::SdkError::Api { status, code, .. } => {
            assert_eq!(*status, 401, "no credentials must be 401");
            assert_eq!(code, "UNAUTHENTICATED");
        }
        other => panic!("expected Api error, got {other:?}"),
    }

    let ctx = CallContext::new().token(common::GENERATORS_TOKEN);
    let err = client.generators(&ctx).await.unwrap_err();
    match &err {
        artifact_client::SdkError::Api { status, code, .. } => {
            assert_eq!(*status, 403, "missing generation:read must be 403");
            assert_eq!(code, "SCOPE_DENIED");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}
