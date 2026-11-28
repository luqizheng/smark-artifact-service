//! 业务端点 handler：Artifact 元数据（已实现）、Version 会话（P0 阻断）、
//! Reference（已实现）、GenerationJob（Adapter 未实现 → 503）。
//!
//! 所有业务端点必须通过 `Scoped`/`Authed` 提取器：无有效委托 → 401，
//! 范围不符 → 403；没有未授权默认放行的路径。

use artifact_contract::{
    CreateArtifactRequest, CreateGenerationJobRequest, CreateReferenceRequest,
    CreateVersionSessionRequest, ErrorEnvelope, ListArtifactsQuery, SetCurrentVersionRequest,
    UpdateArtifactRequest,
};
use artifact_domain::DomainError;
use axum::Json;
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::header::ETAG;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use artifact_application::WriteOperationType;

use crate::auth::{Authed, Scoped};
use crate::error::{ApiError, RequestId, api_error, error_response};
use crate::state::AppState;

// ── 通用提取器 ─────────────────────────────────────────────────────────

/// JSON body + 原始 Value（V10 v1 指纹绑定用，未知字段参与指纹）+ 原文
/// sha256（幂等 request_hash 的基础）。解析失败返回 400 INVALID_REQUEST。
pub struct ValidJson<T> {
    pub value: T,
    pub raw: serde_json::Value,
    pub request_hash: String,
}

impl<S, T> FromRequest<S> for ValidJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::to_bytes(req.into_body(), 8 << 20)
            .await
            .map_err(|_| invalid_request("request body is too large or unreadable"))?;
        let request_hash = sha256_hex(&bytes);
        // 先按 Value 解析（保形），再收窄到 typed DTO：指纹与签发方对原始
        // 请求 JSON 的计算保持一致，未知字段不会被 DTO 静默丢弃。
        let raw: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| invalid_request(format!("invalid JSON body: {e}")))?;
        let value: T = serde_json::from_value(raw.clone())
            .map_err(|e| invalid_request(format!("invalid JSON body: {e}")))?;
        Ok(Self {
            value,
            raw,
            request_hash,
        })
    }
}

/// 生成请求专用提取器：body 上限 256 KiB（ART-03A A3A-02），超限返回 413。
/// 只保留原始字节数、原始 Value（V10 指纹绑定）与解析后的 JSON；
/// 输入正文不进入日志/trace。
pub struct ValidGenerationJson<T> {
    pub value: T,
    pub raw: serde_json::Value,
    pub envelope_bytes: usize,
    /// 原始请求体 sha256（keyed create 的 Idempotency-Key 冲突判定依据，
    /// ART-04G v3 §5.3：即使未携带 Idempotency-Key 也必须提供）。
    pub request_hash: String,
}

impl<S, T> FromRequest<S> for ValidGenerationJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let request_id = req.extensions().get::<RequestId>().copied();
        let bytes = axum::body::to_bytes(req.into_body(), artifact_application::MAX_ENVELOPE_BYTES)
            .await
            .map_err(|_| {
                api_error(
                    request_id,
                    &generation_input_rejected(
                        request_id,
                        "INPUT_TOO_LARGE",
                        "request body",
                        "generation input envelope exceeds 256 KiB",
                    ),
                )
            })?;
        let envelope_bytes = bytes.len();
        let request_hash = sha256_hex(&bytes);
        let raw: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| invalid_request(format!("invalid JSON body: {e}")))?;
        let value: T = serde_json::from_value(raw.clone())
            .map_err(|e| invalid_request(format!("invalid JSON body: {e}")))?;
        Ok(Self {
            value,
            raw,
            envelope_bytes,
            request_hash,
        })
    }
}

/// 生成请求被拒绝的错误：稳定原因码与结构位置放在 `details` 里，
/// 消息是固定文本，不回显任何输入内容。
fn generation_input_rejected(
    request_id: Option<RequestId>,
    code: &'static str,
    field: &str,
    message: &str,
) -> ApiError {
    let error_code = if code == "INPUT_TOO_LARGE" {
        "PAYLOAD_TOO_LARGE"
    } else {
        "INVALID_REQUEST"
    };
    ApiError::Envelope(ErrorEnvelope::new(
        error_code,
        message,
        request_id.map(|id| id.0.to_string()),
        false,
        serde_json::json!({ "reason": code, "field": field }),
    ))
}

/// 查询参数（解析失败 → 400 INVALID_REQUEST 信封）。
pub struct ValidQuery<T> {
    pub value: T,
}

impl<S, T> FromRequestParts<S> for ValidQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let query: Query<T> = Query::from_request_parts(parts, state)
            .await
            .map_err(|e| invalid_request(format!("invalid query parameters: {e}")))?;
        Ok(Self { value: query.0 })
    }
}

fn invalid_request(message: impl Into<String>) -> Response {
    error_response(
        None,
        ErrorEnvelope::new(
            "INVALID_REQUEST",
            message.into(),
            None,
            false,
            serde_json::Value::Null,
        ),
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn parse_path_uuid(raw: &str, field: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| {
        // 稳定原因码 + 400；错误消息不回显用户提交的原始路径参数。
        DomainError::InvalidRequest(format!(
            "invalid {field}: the path parameter must be a valid UUID"
        ))
        .into()
    })
}

fn parse_if_match_revision(headers: &HeaderMap) -> Result<i64, ApiError> {
    let value = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            DomainError::InvalidRequest("If-Match header is required for updates".to_string())
        })?;
    let raw = value.trim().trim_matches('"');
    raw.strip_prefix('r')
        .and_then(|digits| digits.parse::<i64>().ok())
        .ok_or_else(|| {
            DomainError::InvalidRequest(format!(
                "If-Match must be the artifact etag like \"r12\" (got {value:?})"
            ))
        })
        .map_err(Into::into)
}

fn json_response(status: StatusCode, value: impl serde::Serialize) -> Response {
    (status, Json(value)).into_response()
}

fn etag_response(status: StatusCode, dto: &artifact_contract::ArtifactResponse) -> Response {
    let mut res = json_response(status, dto);
    if let Ok(value) = axum::http::HeaderValue::from_str(&dto.etag) {
        res.headers_mut().insert(ETAG, value);
    }
    res
}

fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
}

fn request_id_str(id: Option<RequestId>) -> Option<String> {
    id.map(|r| r.0.to_string())
}

/// 构造 V10 操作绑定事实（WP-04）：method + 实例化 path（字节原样）+ 原始
/// body Value。判定在应用服务层完成；本函数只采集，不做任何策略判定——
/// 唯一例外是 v1 契约红线的 query 拒绝（WP-04-R1 P1，见下）。
///
/// # v1 写端点拒绝 query（契约 §3.1.3 冻结规则，全部写路由统一生效）
///
/// v1 request fingerprint 的成分只有 METHOD/PATH/CALLER/BODY，**不含
/// query**；契约明文规定 v1 全部写端点不得携带 query。因此 query 不是
/// 指纹的遗漏维度，而是 v1 写请求的非法形态——在绑定事实采集的统一入口
/// 拒绝（本函数为全部 9 个写 handler 的唯一入口），早于任何数据库写入、
/// storage、generator 或队列副作用，也早于服务层绑定判定。
///
/// 存在性判断使用 [`axum::http::Uri::query`]：它保留原始 query 的存在性
/// （裸 `?` 返回 `Some("")`，`?x=1` 返回 `Some("x=1")`，无 query 才是
/// `None`）——不能用解析后的 query map 判断，空 query 会被解析层吃掉。
fn write_binding(
    method: &axum::http::Method,
    uri: &axum::http::Uri,
    operation_type: WriteOperationType,
    target_artifact_id: Option<Uuid>,
    target_job_id: Option<Uuid>,
    raw_body: serde_json::Value,
    request_caller: Option<Uuid>,
) -> Result<artifact_application::WriteOperationBinding, ApiError> {
    if uri.query().is_some() {
        return Err(ApiError::Domain(DomainError::InvalidRequest(
            "write endpoints must not carry a query string (request-hash v1 contract)".into(),
        )));
    }
    Ok(artifact_application::WriteOperationBinding {
        method: method.as_str().to_string(),
        path: uri.path().to_string(),
        operation_type,
        target_artifact_id,
        target_job_id,
        body: raw_body,
        request_caller_request_id: request_caller,
    })
}

// ── Artifact 元数据（已实现） ──────────────────────────────────────────

pub async fn create_artifact(
    State(state): State<AppState>,
    req: Authed,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: ValidJson<CreateArtifactRequest>,
) -> Result<Response, ApiError> {
    let scope =
        artifact_domain::TenantScope::new(req.delegation.scope.tenant_id, body.value.project_id);
    let binding = write_binding(
        &method,
        &uri,
        WriteOperationType::ArtifactCreate,
        None,
        None,
        body.raw,
        None,
    )?;
    let outcome = state
        .artifacts
        .create_artifact(
            &req.delegation,
            &scope,
            request_id_str(req.request_id).as_deref(),
            body.value,
            idempotency_key(&headers),
            body.request_hash,
            &binding,
        )
        .await?;
    // 稳定解析语义（ART-04E §7）：created → 201；existing_key / idempotency_replay → 200。
    let status = match outcome.resolution {
        artifact_application::CreateResolution::Created => StatusCode::CREATED,
        artifact_application::CreateResolution::ExistingKey
        | artifact_application::CreateResolution::IdempotencyReplay => StatusCode::OK,
    };
    let mut res = etag_response(status, &outcome.resource);
    res.headers_mut().insert(
        "x-artifact-resolution",
        axum::http::HeaderValue::from_static(outcome.resolution.as_str()),
    );
    if outcome.replayed {
        res.headers_mut().insert(
            "x-idempotent-replay",
            axum::http::HeaderValue::from_static("true"),
        );
    }
    Ok(res)
}

pub async fn get_artifact(
    State(state): State<AppState>,
    req: Scoped,
    Path(artifact_id): Path<String>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let dto = state
        .artifacts
        .get_artifact(&req.delegation, &req.scope, artifact_id)
        .await?;
    Ok(etag_response(StatusCode::OK, &dto))
}

pub async fn list_artifacts(
    State(state): State<AppState>,
    req: Scoped,
    ValidQuery { value: query }: ValidQuery<ListArtifactsQuery>,
) -> Result<Response, ApiError> {
    let page = state
        .artifacts
        .list_artifacts(&req.delegation, &req.scope, query)
        .await?;
    Ok(json_response(StatusCode::OK, page))
}

pub async fn patch_artifact(
    State(state): State<AppState>,
    req: Scoped,
    method: axum::http::Method,
    uri: axum::http::Uri,
    Path(artifact_id): Path<String>,
    headers: HeaderMap,
    body: ValidJson<UpdateArtifactRequest>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let expected_revision = parse_if_match_revision(&headers)?;
    let binding = write_binding(
        &method,
        &uri,
        WriteOperationType::ArtifactPatchMetadata,
        Some(artifact_id),
        None,
        body.raw,
        None,
    )?;
    let dto = state
        .artifacts
        .patch_artifact(
            &req.delegation,
            &req.scope,
            artifact_id,
            expected_revision,
            body.value,
            request_id_str(req.request_id).as_deref(),
            &binding,
        )
        .await?;
    Ok(etag_response(StatusCode::OK, &dto))
}

pub async fn set_current_version(
    State(state): State<AppState>,
    req: Scoped,
    method: axum::http::Method,
    uri: axum::http::Uri,
    Path(artifact_id): Path<String>,
    body: ValidJson<SetCurrentVersionRequest>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let binding = write_binding(
        &method,
        &uri,
        WriteOperationType::CurrentVersionSet,
        Some(artifact_id),
        None,
        body.raw,
        None,
    )?;
    let dto = state
        .artifacts
        .set_current_version(
            &req.delegation,
            &req.scope,
            artifact_id,
            body.value,
            request_id_str(req.request_id).as_deref(),
            &binding,
        )
        .await?;
    Ok(etag_response(StatusCode::OK, &dto))
}

// ── Version 会话（P0 阻断 → 503 STORAGE_UNAVAILABLE） ─────────────────

pub async fn create_version_session(
    State(state): State<AppState>,
    req: Scoped,
    method: axum::http::Method,
    uri: axum::http::Uri,
    Path(artifact_id): Path<String>,
    body: ValidJson<CreateVersionSessionRequest>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let binding = write_binding(
        &method,
        &uri,
        WriteOperationType::VersionSessionCreate,
        Some(artifact_id),
        None,
        body.raw,
        None,
    )?;
    let dto = state
        .artifacts
        .create_version_session(
            &req.delegation,
            &req.scope,
            artifact_id,
            body.value,
            &binding,
            request_id_str(req.request_id).as_deref(),
        )
        .await?;
    // WP-AS-05：201 Created，Location 指向版本资源（提交协议见 commit）。
    let location = format!("/v1/artifacts/{artifact_id}/versions/{}", dto.id);
    Ok((
        StatusCode::CREATED,
        [("Location", location)],
        axum::Json(dto),
    )
        .into_response())
}

pub async fn commit_version(
    State(state): State<AppState>,
    req: Scoped,
    method: axum::http::Method,
    uri: axum::http::Uri,
    Path((artifact_id, version_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let version_id = parse_path_uuid(&version_id, "version_id")?;
    // 无 body 端点：v1 指纹 body 段 = 字面量 null（契约 §3.1.3）。
    let binding = write_binding(
        &method,
        &uri,
        WriteOperationType::VersionCommit,
        Some(artifact_id),
        None,
        serde_json::Value::Null,
        None,
    )?;
    match state
        .artifacts
        .commit_version_session(
            &req.delegation,
            &req.scope,
            artifact_id,
            version_id,
            &binding,
            request_id_str(req.request_id).as_deref(),
        )
        .await
    {
        Err(e) => Err(e.into()),
        // WP-AS-05：Ok 分支由原"拒绝伪成功"守卫改为真实提交协议
        // （inspect + sha256 比对 + 单事务 output/ready），此处保持直返。
        Ok(dto) => Ok(json_response(StatusCode::OK, dto)),
    }
}

pub async fn get_version(
    State(state): State<AppState>,
    req: Scoped,
    Path((artifact_id, version_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let version_id = parse_path_uuid(&version_id, "version_id")?;
    // 授权（version:read）在服务层执行且前置于任何数据读取（WP-03 R2-F-03）。
    let dto = state
        .artifacts
        .get_version(&req.delegation, &req.scope, artifact_id, version_id)
        .await?;
    Ok(json_response(StatusCode::OK, dto))
}

pub async fn list_versions(
    State(state): State<AppState>,
    req: Scoped,
    Path(artifact_id): Path<String>,
    ValidQuery { value: query }: ValidQuery<std::collections::HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let limit: i64 = query
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    // 授权（version:read）在服务层执行且前置于任何数据读取（WP-03 R2-F-03）。
    let items = state
        .artifacts
        .list_versions(&req.delegation, &req.scope, artifact_id, limit)
        .await?;
    Ok(json_response(
        StatusCode::OK,
        serde_json::json!({ "items": items, "next_cursor": serde_json::Value::Null }),
    ))
}

// ── References（已实现） ───────────────────────────────────────────────

pub async fn create_reference(
    State(state): State<AppState>,
    req: Scoped,
    method: axum::http::Method,
    uri: axum::http::Uri,
    Path(artifact_id): Path<String>,
    body: ValidJson<CreateReferenceRequest>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let binding = write_binding(
        &method,
        &uri,
        WriteOperationType::ReferenceCreate,
        Some(artifact_id),
        None,
        body.raw,
        None,
    )?;
    let outcome = state
        .artifacts
        .create_reference(
            &req.delegation,
            &req.scope,
            artifact_id,
            body.value,
            request_id_str(req.request_id).as_deref(),
            &binding,
        )
        .await?;
    Ok(json_response(StatusCode::CREATED, outcome.resource))
}

pub async fn list_references(
    State(state): State<AppState>,
    req: Scoped,
    Path(artifact_id): Path<String>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    // 授权（artifact:read）在服务层执行且前置于任何数据读取（WP-03 R2-F-03）。
    let items = state
        .artifacts
        .list_references(&req.delegation, &req.scope, artifact_id)
        .await?;
    Ok(json_response(
        StatusCode::OK,
        serde_json::json!({ "items": items, "next_cursor": serde_json::Value::Null }),
    ))
}

pub async fn remove_reference(
    State(state): State<AppState>,
    req: Scoped,
    method: axum::http::Method,
    uri: axum::http::Uri,
    Path((artifact_id, reference_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let reference_id = parse_path_uuid(&reference_id, "reference_id")?;
    // 无 body 端点：v1 指纹 body 段 = 字面量 null（契约 §3.1.3）。
    let binding = write_binding(
        &method,
        &uri,
        WriteOperationType::ReferenceRemove,
        Some(artifact_id),
        None,
        serde_json::Value::Null,
        None,
    )?;
    state
        .artifacts
        .remove_reference(
            &req.delegation,
            &req.scope,
            artifact_id,
            reference_id,
            request_id_str(req.request_id).as_deref(),
            &binding,
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── GenerationJobs（Adapter 未实现 → 503 GENERATOR_UNAVAILABLE） ──────

pub async fn request_generation(
    State(state): State<AppState>,
    req: Scoped,
    method: axum::http::Method,
    uri: axum::http::Uri,
    Path(artifact_id): Path<String>,
    headers: HeaderMap,
    body: ValidGenerationJson<CreateGenerationJobRequest>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;

    // 服务端输入校验（A3A-02）：大小、结构上限、信封 allowlist、危险字段与 URL 形态的值。
    // 拒绝只返回稳定原因码与结构位置，绝不回显输入正文。
    if let Err(e) = artifact_application::validate_job_input_envelope(
        body.envelope_bytes,
        &body.value.input,
        body.value.options.as_ref(),
    ) {
        return Err(generation_input_rejected(
            req.request_id,
            e.code,
            &e.field,
            "generation input envelope was rejected",
        ));
    }

    // V10：请求侧 caller = body.caller_request_id（与 token claim 交叉比对）。
    let binding = write_binding(
        &method,
        &uri,
        WriteOperationType::GenerationRequest,
        Some(artifact_id),
        None,
        body.raw,
        Some(body.value.caller_request_id),
    )?;
    let outcome = state
        .artifacts
        .request_generation(
            &req.delegation,
            &req.scope,
            artifact_id,
            body.value,
            body.request_hash,
            idempotency_key(&headers),
            body.envelope_bytes,
            request_id_str(req.request_id).as_deref(),
            &binding,
        )
        .await?;
    // WP-AS-06：202 新建 / 200 重放（OpenAPI 冻结契约；ART-04 §74），
    // X-Generation-Job-Resolution 区分三态。
    let status = match outcome.resolution {
        artifact_application::JobResolution::Created => StatusCode::ACCEPTED,
        _ => StatusCode::OK,
    };
    Ok((
        status,
        [("X-Generation-Job-Resolution", outcome.resolution.as_str())],
        axum::Json(outcome.job),
    )
        .into_response())
}

pub async fn get_job(
    State(state): State<AppState>,
    req: Scoped,
    Path(job_id): Path<String>,
) -> Result<Response, ApiError> {
    let job_id = parse_path_uuid(&job_id, "job_id")?;
    let dto = state
        .artifacts
        .get_job(&req.delegation, &req.scope, job_id)
        .await?;
    Ok(json_response(StatusCode::OK, dto))
}

/// AS-2 Caller Request Lookup（ART-04G v3 §5.4）：与 get_job 相同链路的只读查询。
/// 非法 UUID → 400 稳定原因码；跨 scope 与不存在统一 404（不泄露存在性）。
pub async fn get_job_by_caller_request(
    State(state): State<AppState>,
    req: Scoped,
    Path(caller_request_id): Path<String>,
) -> Result<Response, ApiError> {
    let caller_request_id = parse_path_uuid(&caller_request_id, "caller_request_id")?;
    let dto = state
        .artifacts
        .get_job_by_caller_request(&req.delegation, &req.scope, caller_request_id)
        .await?;
    Ok(json_response(StatusCode::OK, dto))
}

pub async fn cancel_job(
    State(state): State<AppState>,
    req: Scoped,
    method: axum::http::Method,
    uri: axum::http::Uri,
    Path(job_id): Path<String>,
) -> Result<Response, ApiError> {
    let job_id = parse_path_uuid(&job_id, "job_id")?;
    // 无 body 端点：v1 指纹 body 段 = 字面量 null（契约 §3.1.3）。
    let binding = write_binding(
        &method,
        &uri,
        WriteOperationType::GenerationCancel,
        None,
        Some(job_id),
        serde_json::Value::Null,
        None,
    )?;
    let dto = state
        .artifacts
        .cancel_job(
            &req.delegation,
            &req.scope,
            job_id,
            request_id_str(req.request_id).as_deref(),
            &binding,
        )
        .await?;
    Ok(json_response(StatusCode::OK, dto))
}
