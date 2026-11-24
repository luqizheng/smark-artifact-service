//! 未授权请求默认拒绝（任务 §4.5 / §六）：生产装配（ClosedDelegationVerifier）
//! 下所有业务端点一律 401 UNAUTHENTICATED，且拒绝被写入审计。

mod common;

use artifact_domain::AuditOutcome;
use serde_json::{Value, json};

#[sqlx::test(migrations = "../migrations")]
async fn all_business_endpoints_denied_without_credentials(pool: sqlx::PgPool) {
    let base = common::spawn_app_closed(pool.clone()).await;
    let client = reqwest::Client::new();
    let project = common::PROJECT_1;

    let cases: Vec<(reqwest::Method, String, Option<Value>)> = vec![
        (
            reqwest::Method::POST,
            "/v1/artifacts".into(),
            Some(json!({"project_id": project, "name": "x", "artifact_type": "file"})),
        ),
        (
            reqwest::Method::GET,
            format!("/v1/artifacts?project_hint={project}"),
            None,
        ),
        (
            reqwest::Method::GET,
            format!("/v1/artifacts/{}", uuid::Uuid::new_v4()),
            None,
        ),
        (
            reqwest::Method::PATCH,
            format!("/v1/artifacts/{}", uuid::Uuid::new_v4()),
            Some(json!({"name": "y"})),
        ),
        (
            reqwest::Method::POST,
            format!("/v1/artifacts/{}/versions", uuid::Uuid::new_v4()),
            Some(json!({})),
        ),
        (
            reqwest::Method::POST,
            format!("/v1/artifacts/{}/current-version", uuid::Uuid::new_v4()),
            Some(json!({"version_id": uuid::Uuid::new_v4()})),
        ),
        (
            reqwest::Method::POST,
            format!("/v1/artifacts/{}/references", uuid::Uuid::new_v4()),
            Some(json!({"owner_type": "task", "owner_id": "t-1", "relation_type": "illustrates"})),
        ),
        (
            reqwest::Method::POST,
            format!("/v1/artifacts/{}/generation-jobs", uuid::Uuid::new_v4()),
            Some(json!({"generator_id": "seecode", "input": {}})),
        ),
        (
            reqwest::Method::GET,
            format!("/v1/generation-jobs/{}", uuid::Uuid::new_v4()),
            None,
        ),
        (
            reqwest::Method::POST,
            format!("/v1/generation-jobs/{}/cancel", uuid::Uuid::new_v4()),
            None,
        ),
    ];

    for (method, path, body) in &cases {
        let project_header: &str = &project.to_string();
        let mut req = client
            .request(method.clone(), format!("{base}{path}"))
            .header("x-project-id", project_header);
        if let Some(body) = body {
            req = req.json(body);
        }
        let res = req.send().await.unwrap();
        let status = res.status();
        assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED, "{method} {path}");
        let envelope: Value = res.json().await.unwrap();
        assert_eq!(
            envelope["error"]["code"], "UNAUTHENTICATED",
            "{method} {path}"
        );
        assert_eq!(envelope["error"]["retryable"], false);
        assert!(
            envelope["error"]["request_id"].is_string(),
            "request_id must be present"
        );
    }

    // 拒绝必须记录审计（ART-01 §5.4）。
    let audits = sqlx::query(
        "SELECT outcome, error_code FROM artifact.audit_event WHERE action = 'auth.denied'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(!audits.is_empty(), "denials must be audited");
    for row in audits {
        let outcome: String = row.get(0);
        assert_eq!(outcome, AuditOutcome::Denied.as_str());
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn garbage_bearer_is_unauthenticated(pool: sqlx::PgPool) {
    let base = common::spawn_app_closed(pool).await;
    let client = reqwest::Client::new();
    let res = client
        .get(format!("{}/v1/artifacts", base))
        .header("x-project-id", common::PROJECT_1.to_string())
        .header("authorization", "Bearer not-a-real-delegation")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);
    let envelope: Value = res.json().await.unwrap();
    assert_eq!(envelope["error"]["code"], "UNAUTHENTICATED");
}

use sqlx::Row;
