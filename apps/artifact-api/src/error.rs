//! HTTP 错误映射与请求 ID 中间件。

use artifact_contract::ErrorEnvelope;
use artifact_domain::DomainError;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

/// API 层错误类型：认证失败（无有效委托）、领域错误，或已组装好的错误信封。
#[derive(Debug, Clone)]
pub enum ApiError {
    Unauthenticated(String),
    Domain(DomainError),
    /// 需要携带结构化 `details` 的拒绝（例如输入校验失败的原因码与位置）。
    Envelope(ErrorEnvelope),
}

impl ApiError {
    pub fn envelope(&self, request_id: Option<String>) -> ErrorEnvelope {
        match self {
            ApiError::Unauthenticated(msg) => ErrorEnvelope::unauthenticated(msg, request_id),
            ApiError::Domain(err) => ErrorEnvelope::from_domain(err, request_id),
            ApiError::Envelope(envelope) => {
                let mut envelope = envelope.clone();
                if envelope.error.request_id.is_none() {
                    envelope.error.request_id = request_id;
                }
                envelope
            }
        }
    }
}

impl From<DomainError> for ApiError {
    fn from(err: DomainError) -> Self {
        ApiError::Domain(err)
    }
}

/// 每请求标识（透传 `X-Request-Id` 或生成）。
#[derive(Debug, Clone, Copy)]
pub struct RequestId(pub Uuid);

pub async fn request_id_layer(mut req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| Uuid::parse_str(v).ok())
        .map(RequestId)
        .unwrap_or_else(|| RequestId(Uuid::new_v4()));
    req.extensions_mut().insert(request_id);
    let mut res = next.run(req).await;
    if let Ok(value) = HeaderValue::from_str(&request_id.0.to_string()) {
        res.headers_mut().insert("x-request-id", value);
    }
    res
}

/// 错误信封注入 request_id；401/403 记录审计拒绝事件（ART-01 §5.4）。
/// 经 `from_fn_with_state` 挂载。
pub async fn audit_and_enrich_layer(
    State(state): State<crate::state::AppState>,
    req: Request,
    next: Next,
) -> Response {
    let request_id = req.extensions().get::<RequestId>().copied();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let mut res = next.run(req).await;

    let status = res.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        let code = if status == StatusCode::UNAUTHORIZED {
            "UNAUTHENTICATED"
        } else {
            "SCOPE_DENIED"
        };
        let event = artifact_domain::AuditEvent::denial(
            "auth.denied",
            code,
            request_id.map(|r| r.0.to_string()).as_deref(),
            None,
            None,
            serde_json::json!({ "method": method.as_str(), "path": path }),
        );
        if let Err(e) = state.artifacts.audit.record(event).await {
            tracing::warn!(error = %e, "audit denial record failed");
        }
    }
    if status.is_client_error() || status.is_server_error() {
        let (parts, body) = res.into_parts();
        let bytes = axum::body::to_bytes(body, 1 << 20)
            .await
            .unwrap_or_default();
        if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(err) = value.get_mut("error").and_then(|e| e.as_object_mut()) {
                err.insert(
                    "request_id".to_string(),
                    serde_json::json!(request_id.map(|r| r.0.to_string())),
                );
            }
            res = Response::from_parts(parts, Body::from(value.to_string()));
        } else {
            res = Response::from_parts(parts, Body::from(bytes));
        }
    }
    res
}

pub fn error_response(request_id: Option<RequestId>, envelope: ErrorEnvelope) -> Response {
    let status =
        StatusCode::from_u16(envelope.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut res = (status, envelope.to_json()).into_response();
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Some(rid) = request_id
        && let Ok(value) = HeaderValue::from_str(&rid.0.to_string())
    {
        res.headers_mut().insert("x-request-id", value);
    }
    res
}

pub fn api_error(request_id: Option<RequestId>, err: &ApiError) -> Response {
    error_response(
        request_id,
        err.envelope(request_id.map(|r| r.0.to_string())),
    )
}

impl IntoResponse for ApiError {
    /// 无请求上下文的兜底映射（request_id 为空）。
    fn into_response(self) -> Response {
        error_response(None, self.envelope(None))
    }
}
