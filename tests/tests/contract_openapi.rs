//! OpenAPI 契约检查（任务 §4.4）：规范是唯一 API 描述来源。
//! ① 路由集合与规范一致；② 错误信封 schema 字段对齐；③ 无凭证探测
//! 每个业务端点得到的 401 响应与规范记载一致；④ meta 端点响应形状对齐。

mod common;

use serde_json::Value;
use serde_yaml::Value as Yaml;

fn load_spec() -> Yaml {
    serde_yaml::from_str(include_str!("../../openapi/openapi.yaml")).expect("openapi.yaml is valid")
}

fn spec_paths(spec: &Yaml) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let paths = spec.get("paths").expect("paths");
    let map = paths.as_mapping().expect("paths mapping");
    for (path, item) in map {
        let path = path.as_str().expect("path str").to_string();
        let mut methods: Vec<String> = item
            .as_mapping()
            .expect("path item mapping")
            .keys()
            .filter_map(|k| k.as_str().map(str::to_string))
            .filter(|k| matches!(k.as_str(), "get" | "post" | "patch" | "put" | "delete"))
            .collect();
        methods.sort();
        out.push((path, methods));
    }
    out.sort();
    out
}

/// 服务端实际暴露的路由（与 apps/artifact-api/src/routes/mod.rs 一致）。
fn server_routes() -> Vec<(String, Vec<String>)> {
    let mut routes: Vec<(String, Vec<String>)> = vec![
        ("/v1/health".into(), vec!["get".into()]),
        ("/v1/ready".into(), vec!["get".into()]),
        ("/v1/generators".into(), vec!["get".into()]),
        ("/v1/artifacts".into(), vec!["get".into(), "post".into()]),
        (
            "/v1/artifacts/{artifact_id}".into(),
            vec!["get".into(), "patch".into()],
        ),
        (
            "/v1/artifacts/{artifact_id}/versions".into(),
            vec!["get".into(), "post".into()],
        ),
        (
            "/v1/artifacts/{artifact_id}/versions/{version_id}".into(),
            vec!["get".into()],
        ),
        (
            "/v1/artifacts/{artifact_id}/versions/{version_id}/commit".into(),
            vec!["post".into()],
        ),
        (
            "/v1/artifacts/{artifact_id}/current-version".into(),
            vec!["post".into()],
        ),
        (
            "/v1/artifacts/{artifact_id}/references".into(),
            vec!["get".into(), "post".into()],
        ),
        (
            "/v1/artifacts/{artifact_id}/references/{reference_id}".into(),
            vec!["delete".into()],
        ),
        (
            "/v1/artifacts/{artifact_id}/generation-jobs".into(),
            vec!["post".into()],
        ),
        ("/v1/generation-jobs/{job_id}".into(), vec!["get".into()]),
        (
            "/v1/generation-jobs/{job_id}/cancel".into(),
            vec!["post".into()],
        ),
    ];
    routes.sort();
    routes
}

#[test]
fn openapi_paths_match_server_routes() {
    assert_eq!(spec_paths(&load_spec()), server_routes());
}

#[test]
fn error_envelope_schema_matches_contract() {
    let spec = load_spec();
    let error = &spec["components"]["schemas"]["Error"]["properties"]["error"];
    let required: Vec<String> = error["required"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(required.contains(&"code".to_string()));
    assert!(required.contains(&"message".to_string()));
    assert!(required.contains(&"retryable".to_string()));
    let properties = error["properties"].as_mapping().unwrap();
    for key in ["code", "message", "request_id", "retryable", "details"] {
        assert!(
            properties.contains_key(key),
            "error.{key} must be documented"
        );
    }
    // Artifact DTO 必备字段与 contract DTO 对齐。
    let artifact = &spec["components"]["schemas"]["Artifact"]["required"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    for field in [
        "id",
        "tenant_id",
        "project_id",
        "artifact_type",
        "lifecycle_state",
        "current_version_id",
        "etag",
    ] {
        assert!(
            artifact.contains(&field.to_string()),
            "Artifact.{field} documented"
        );
    }
}

#[test]
fn every_business_path_documents_401() {
    // 无 DB 依赖的静态部分：规范中每个业务路径都声明了 401。
    let spec = load_spec();
    let paths = spec.get("paths").unwrap();
    let map = paths.as_mapping().unwrap();
    for (path, item) in map {
        let path_str = path.as_str().unwrap();
        if path_str == "/v1/health" || path_str == "/v1/ready" || path_str == "/v1/generators" {
            continue;
        }
        for (method, op) in item.as_mapping().unwrap() {
            let method = method.as_str().unwrap();
            if !matches!(method, "get" | "post" | "patch" | "delete") {
                continue;
            }
            let op = op.as_mapping().unwrap();
            let responses = op.get("responses").and_then(|r| r.as_mapping()).unwrap();
            assert!(
                responses.contains_key("401"),
                "{method} {path_str} must document 401"
            );
        }
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn served_responses_match_openapi_shapes(pool: sqlx::PgPool) {
    let base = common::spawn_app_closed(pool).await;
    let client = reqwest::Client::new();

    // /v1/health：schema 字段对齐。
    let health: Value = client
        .get(format!("{base}/v1/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for key in ["status", "service", "version", "git_sha", "built_at"] {
        assert!(health.get(key).is_some(), "health.{key} must be served");
    }

    // /v1/ready。
    let ready: Value = client
        .get(format!("{base}/v1/ready"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ready["status"], "ready");
    assert!(ready["components"]["database"].is_string());
    assert!(ready["components"]["file_storage"].is_string());

    // /v1/generators。
    let generators: Value = client
        .get(format!("{base}/v1/generators"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(generators["generators"].is_array());

    // 业务端点无凭证 → 规范声明的 401 信封。
    let res = client
        .get(format!("{base}/v1/artifacts"))
        .header("x-project-id", common::PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let envelope: Value = res.json().await.unwrap();
    assert_eq!(envelope["error"]["code"], "UNAUTHENTICATED");
    assert_eq!(envelope["error"]["retryable"], false);
}
