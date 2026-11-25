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
            "/v1/generation-jobs/by-caller-request/{caller_request_id}".into(),
            vec!["get".into()],
        ),
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

/// GenerationJob 的响应必须让调用方拿到结果（原因码、重试进度、取消状态），
/// 且永远不暴露输入正文或租约。
#[test]
fn generation_job_documents_outcome_fields() {
    let spec = load_spec();
    let job = &spec["components"]["schemas"]["GenerationJob"];
    let required: Vec<String> = job["required"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    for field in [
        "id",
        "artifact_id",
        "generator_id",
        "status",
        "error_code",
        "attempt_count",
        "max_attempts",
        "cancel_requested",
        "created_at",
        "poll_url",
    ] {
        assert!(
            required.contains(&field.to_string()),
            "GenerationJob.{field} must be documented as required"
        );
    }
    let properties = job["properties"].as_mapping().unwrap();
    for forbidden in [
        "input",
        "input_payload",
        "options",
        "options_payload",
        "lease_token",
    ] {
        assert!(
            !properties.contains_key(forbidden),
            "GenerationJob must never expose {forbidden}"
        );
    }

    let responses =
        &spec["paths"]["/v1/artifacts/{artifact_id}/generation-jobs"]["post"]["responses"];
    for status in ["200", "202", "400", "409", "413", "500", "503"] {
        assert!(
            responses.get(status).is_some(),
            "requestGeneration must document {status}"
        );
    }
}

/// AS2-37：AS-2 的 OpenAPI parity —— DTO ↔ schema 双向、必填 caller_request_id、
/// 可空响应字段、resolution 头、新路由与 409/500 错误码。
#[test]
fn as2_37_openapi_mirrors_caller_identity_contract() {
    let spec = load_spec();

    // 请求：caller_request_id 必填且为 uuid。
    let create = &spec["components"]["schemas"]["CreateGenerationJobRequest"];
    let required: Vec<String> = create["required"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        required.contains(&"caller_request_id".to_string()),
        "caller_request_id must be required in the create request"
    );
    let caller = &create["properties"]["caller_request_id"];
    assert_eq!(caller["format"].as_str(), Some("uuid"));

    // 响应：caller_request_id 可空。
    let job_caller =
        &spec["components"]["schemas"]["GenerationJob"]["properties"]["caller_request_id"];
    let types: Vec<String> = job_caller["type"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(types.contains(&"string".to_string()) && types.contains(&"null".to_string()));

    // resolution 头三值。
    let enum_values: Vec<String> =
        spec["components"]["headers"]["GenerationJobResolution"]["schema"]["enum"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
    assert_eq!(
        enum_values,
        vec!["created", "caller_request_replay", "idempotency_replay"]
    );

    // POST 200/202 都必须声明 resolution 头。
    let responses =
        &spec["paths"]["/v1/artifacts/{artifact_id}/generation-jobs"]["post"]["responses"];
    for status in ["200", "202"] {
        assert!(
            responses[status]["headers"]
                .get("X-Generation-Job-Resolution")
                .is_some(),
            "the {status} response must document the resolution header"
        );
    }
    // 409 说明并入 CALLER_REQUEST_ID_REUSED；500 说明两个完整性错误码。
    let conflict = responses["409"]["description"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(conflict.contains("CALLER_REQUEST_ID_REUSED"), "{conflict}");
    let integrity = responses["500"]["description"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        integrity.contains("CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH")
            && integrity.contains("IDEMPOTENCY_KEY_CROSS_IDENTITY"),
        "{integrity}"
    );

    // lookup 路由：路径参数 + 404 语义。
    let lookup = &spec["paths"]["/v1/generation-jobs/by-caller-request/{caller_request_id}"]["get"];
    let params = lookup["parameters"].as_sequence().unwrap();
    let has_path_param = params.iter().any(|p| {
        p["name"].as_str() == Some("caller_request_id") && p["in"].as_str() == Some("path")
    });
    assert!(
        has_path_param,
        "caller_request_id path parameter must be documented"
    );
    assert!(lookup["responses"].get("404").is_some());
}

/// AS2-38：新增稳定错误码进入 contract 映射（HTTP status + 不可重试）。
#[test]
fn as2_38_new_error_codes_are_mapped() {
    use artifact_contract::ErrorEnvelope;
    use artifact_domain::DomainError;

    let cases = [
        (
            DomainError::CallerRequestIdReused("x".into()),
            "CALLER_REQUEST_ID_REUSED",
            409,
        ),
        (
            DomainError::InvalidIdempotencyKey("x".into()),
            "INVALID_IDEMPOTENCY_KEY",
            400,
        ),
        (
            DomainError::FingerprintVersionMismatch("x".into()),
            "CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH",
            500,
        ),
        (
            DomainError::IdempotencyKeyCrossIdentity("x".into()),
            "IDEMPOTENCY_KEY_CROSS_IDENTITY",
            500,
        ),
    ];
    for (err, code, status) in cases {
        let envelope = ErrorEnvelope::from_domain(&err, None);
        assert_eq!(envelope.error.code, code);
        assert_eq!(envelope.http_status(), status, "{code} status");
        assert!(!envelope.error.retryable, "{code} must be non-retryable");
    }
}

/// P1-03（独立审查修复）：Error.code 枚举必须覆盖服务端实际会产生的全部
/// 稳定错误码。每个 DomainError::code() 都必须出现在 OpenAPI 枚举中，
/// 防止新增错误变体时契约静默漂移；HTTP status 语义由 as2_38 与
/// ErrorEnvelope::http_status 映射负责，数据库内部 token 不直接越过该映射。
/// 注意：新增 DomainError 变体时必须同步扩展下方的显式清单。
#[test]
fn error_code_enum_covers_every_domain_error() {
    use artifact_domain::DomainError;

    let spec = load_spec();
    let enum_values: Vec<String> =
        spec["components"]["schemas"]["Error"]["properties"]["error"]["properties"]["code"]["enum"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();

    let produced = [
        DomainError::InvalidRequest(String::new()),
        DomainError::ScopeDenied(String::new()),
        DomainError::NotFound,
        DomainError::VersionConflict(String::new()),
        DomainError::IdempotencyKeyReused(String::new()),
        DomainError::CallerRequestIdReused(String::new()),
        DomainError::FingerprintVersionMismatch(String::new()),
        DomainError::IdempotencyKeyCrossIdentity(String::new()),
        DomainError::InvalidIdempotencyKey(String::new()),
        DomainError::ArtifactKeyConflict(String::new()),
        DomainError::ArtifactKeyLegacyUnverifiable(String::new()),
        DomainError::UnsupportedMediaType(String::new()),
        DomainError::PayloadTooLarge(String::new()),
        DomainError::StorageUnavailable(String::new()),
        DomainError::GeneratorUnavailable(String::new()),
        DomainError::GeneratorValidationFailed(String::new()),
        DomainError::PersistenceUnavailable(String::new()),
        DomainError::Internal(String::new()),
    ];
    for code in produced.iter().map(|err| err.code()) {
        assert!(
            enum_values.iter().any(|v| v == code),
            "the OpenAPI error code enum must document {code}"
        );
    }
    // 认证失败信封由 ApiError::Unauthenticated 产生，同样必须入库。
    assert!(
        enum_values.iter().any(|v| v == "UNAUTHENTICATED"),
        "UNAUTHENTICATED must stay documented"
    );
    // 独立审查 P1-03 点名的四个 AS-2 新增码，显式固化。
    for code in [
        "CALLER_REQUEST_ID_REUSED",
        "CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH",
        "IDEMPOTENCY_KEY_CROSS_IDENTITY",
        "INVALID_IDEMPOTENCY_KEY",
    ] {
        assert!(
            enum_values.iter().any(|v| v == code),
            "the AS-2 error code {code} must stay in the enum"
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
