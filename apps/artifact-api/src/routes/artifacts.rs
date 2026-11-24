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

use crate::auth::{Authed, Scoped};
use crate::error::{ApiError, RequestId, error_response};
use crate::state::AppState;

// ── 通用提取器 ─────────────────────────────────────────────────────────

/// JSON body + 原文 sha256（幂等 request_hash 的基础）。
/// 解析失败返回 400 INVALID_REQUEST（机器可读信封）。
pub struct ValidJson<T> {
    pub value: T,
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
        let value: T = serde_json::from_slice(&bytes)
            .map_err(|e| invalid_request(format!("invalid JSON body: {e}")))?;
        Ok(Self {
            value,
            request_hash,
        })
    }
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
    Uuid::parse_str(raw)
        .map_err(|_| DomainError::InvalidRequest(format!("invalid {field} '{raw}'")).into())
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

// ── Artifact 元数据（已实现） ──────────────────────────────────────────

pub async fn create_artifact(
    State(state): State<AppState>,
    req: Authed,
    headers: HeaderMap,
    body: ValidJson<CreateArtifactRequest>,
) -> Result<Response, ApiError> {
    let scope =
        artifact_domain::TenantScope::new(req.delegation.scope.tenant_id, body.value.project_id);
    let outcome = state
        .artifacts
        .create_artifact(
            &req.delegation,
            &scope,
            request_id_str(req.request_id).as_deref(),
            body.value,
            idempotency_key(&headers),
            body.request_hash,
        )
        .await?;
    let status = if outcome.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let mut res = etag_response(status, &outcome.resource);
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
    Path(artifact_id): Path<String>,
    headers: HeaderMap,
    body: ValidJson<UpdateArtifactRequest>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let expected_revision = parse_if_match_revision(&headers)?;
    let dto = state
        .artifacts
        .patch_artifact(
            &req.delegation,
            &req.scope,
            artifact_id,
            expected_revision,
            body.value,
            request_id_str(req.request_id).as_deref(),
        )
        .await?;
    Ok(etag_response(StatusCode::OK, &dto))
}

pub async fn set_current_version(
    State(state): State<AppState>,
    req: Scoped,
    Path(artifact_id): Path<String>,
    body: ValidJson<SetCurrentVersionRequest>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let dto = state
        .artifacts
        .set_current_version(
            &req.delegation,
            &req.scope,
            artifact_id,
            body.value,
            request_id_str(req.request_id).as_deref(),
        )
        .await?;
    Ok(etag_response(StatusCode::OK, &dto))
}

// ── Version 会话（P0 阻断 → 503 STORAGE_UNAVAILABLE） ─────────────────

pub async fn create_version_session(
    State(state): State<AppState>,
    req: Scoped,
    Path(artifact_id): Path<String>,
    body: ValidJson<CreateVersionSessionRequest>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    match state
        .artifacts
        .create_version_session(&req.delegation, &req.scope, artifact_id, body.value)
        .await
    {
        Err(e) => Err(e.into()),
        // 不可达：StoragePort 生产实现为 ProtocolBlocked；若未来此处返回 Ok，
        // 必须先落地完整版本提交协议（ART-02 存储 P0）。
        Ok(_) => Err(DomainError::Internal(
            "version session succeeded while storage protocol is blocked; refusing to fabricate success"
                .into(),
        )
        .into()),
    }
}

pub async fn commit_version(
    State(state): State<AppState>,
    req: Scoped,
    Path((artifact_id, version_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let version_id = parse_path_uuid(&version_id, "version_id")?;
    match state
        .artifacts
        .commit_version_session(&req.delegation, &req.scope, artifact_id, version_id)
        .await
    {
        Err(e) => Err(e.into()),
        Ok(_) => Err(DomainError::Internal(
            "version commit succeeded while storage protocol is blocked; refusing to fabricate success"
                .into(),
        )
        .into()),
    }
}

pub async fn get_version(
    State(state): State<AppState>,
    req: Scoped,
    Path((artifact_id, version_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let version_id = parse_path_uuid(&version_id, "version_id")?;
    let version = state
        .artifacts
        .versions
        .get(
            (req.scope.tenant_id, req.scope.project_id),
            artifact_id,
            version_id,
        )
        .await?;
    Ok(json_response(
        StatusCode::OK,
        artifact_application::to_version_dto(&version),
    ))
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
    let versions = state
        .artifacts
        .versions
        .list(
            (req.scope.tenant_id, req.scope.project_id),
            artifact_id,
            limit,
        )
        .await?;
    let items: Vec<_> = versions
        .iter()
        .map(artifact_application::to_version_dto)
        .collect();
    Ok(json_response(
        StatusCode::OK,
        serde_json::json!({ "items": items, "next_cursor": serde_json::Value::Null }),
    ))
}

// ── References（已实现） ───────────────────────────────────────────────

pub async fn create_reference(
    State(state): State<AppState>,
    req: Scoped,
    Path(artifact_id): Path<String>,
    body: ValidJson<CreateReferenceRequest>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let outcome = state
        .artifacts
        .create_reference(
            &req.delegation,
            &req.scope,
            artifact_id,
            body.value,
            request_id_str(req.request_id).as_deref(),
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
    let references = state
        .artifacts
        .references
        .list(
            (req.scope.tenant_id, req.scope.project_id),
            artifact_id,
            100,
        )
        .await?;
    let items: Vec<_> = references
        .iter()
        .map(artifact_application::to_reference_dto)
        .collect();
    Ok(json_response(
        StatusCode::OK,
        serde_json::json!({ "items": items, "next_cursor": serde_json::Value::Null }),
    ))
}

pub async fn remove_reference(
    State(state): State<AppState>,
    req: Scoped,
    Path((artifact_id, reference_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    let reference_id = parse_path_uuid(&reference_id, "reference_id")?;
    state
        .artifacts
        .remove_reference(
            &req.delegation,
            &req.scope,
            artifact_id,
            reference_id,
            request_id_str(req.request_id).as_deref(),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── GenerationJobs（Adapter 未实现 → 503 GENERATOR_UNAVAILABLE） ──────

pub async fn request_generation(
    State(state): State<AppState>,
    req: Scoped,
    Path(artifact_id): Path<String>,
    _headers: HeaderMap,
    body: ValidJson<CreateGenerationJobRequest>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_path_uuid(&artifact_id, "artifact_id")?;
    match state
        .artifacts
        .request_generation(
            &req.delegation,
            &req.scope,
            artifact_id,
            body.value,
            request_id_str(req.request_id).as_deref(),
        )
        .await
    {
        Err(e) => Err(e.into()),
        // 不可达：注册表中无 available 生成器；新增可用生成器必须伴随
        // GenerationJob 编排与 Worker 契约实现（ART-03+）。
        Ok(_) => Err(DomainError::Internal(
            "generation request succeeded while generator adapters are not implemented".into(),
        )
        .into()),
    }
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

pub async fn cancel_job(
    State(state): State<AppState>,
    req: Scoped,
    Path(job_id): Path<String>,
) -> Result<Response, ApiError> {
    let job_id = parse_path_uuid(&job_id, "job_id")?;
    let dto = state
        .artifacts
        .cancel_job(
            &req.delegation,
            &req.scope,
            job_id,
            request_id_str(req.request_id).as_deref(),
        )
        .await?;
    Ok(json_response(StatusCode::OK, dto))
}
