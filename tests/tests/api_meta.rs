//! 健康检查 / 就绪检查 / Generator 注册表（无凭证只读面）。

mod common;

use artifact_client::{ArtifactClient, ArtifactClientConfig};

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
    let base = common::spawn_app_closed(pool).await;
    let client = ArtifactClient::new(ArtifactClientConfig::new(&base)).unwrap();
    let generators = client.generators().await.unwrap();
    let ids: Vec<_> = generators.iter().map(|g| g.generator_id.as_str()).collect();
    assert!(ids.contains(&"mermaid") && ids.contains(&"seecode"));
    for generator in &generators {
        // ART-02 事实：两个 Adapter 均未实现，禁止上报 available。
        assert_eq!(
            generator.status, "unavailable",
            "{}",
            generator.generator_id
        );
        assert!(generator.status_reason.is_some());
    }
}
