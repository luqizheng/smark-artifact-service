//! 认证与作用域提取器。
//!
//! `Authed`：验证委托凭证（无有效凭证 → 401，缺凭证 → 401）。
//! `Scoped`：在 `Authed` 基础上解析 `X-Project-Id` 头并组装租户/项目作用域
//! （缺头/坏 UUID → 400 INVALID_REQUEST）。
//!
//! 作用域的 tenant 恒取自委托凭证（ART-01 §5.5：不信任请求体 tenant_id）。

use artifact_domain::{Delegation, DomainError, TenantScope};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use uuid::Uuid;

use crate::error::{ApiError, RequestId, api_error};
use crate::state::AppState;

pub const PROJECT_HEADER: &str = "x-project-id";

pub struct Authed {
    pub delegation: Delegation,
    pub request_id: Option<RequestId>,
}

impl FromRequestParts<AppState> for Authed {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let request_id = parts.extensions.get::<RequestId>().copied();
        let token = bearer_token(parts).ok_or_else(|| {
            api_error(
                request_id,
                &ApiError::Unauthenticated(
                    "missing Authorization: Bearer <delegation token>".into(),
                ),
            )
        })?;
        let delegation = state
            .verifier
            .verify(token)
            .await
            .map_err(|e| api_error(request_id, &ApiError::Unauthenticated(e.to_string())))?;
        Ok(Self {
            delegation,
            request_id,
        })
    }
}

pub struct Scoped {
    pub delegation: Delegation,
    pub scope: TenantScope,
    pub request_id: Option<RequestId>,
}

impl FromRequestParts<AppState> for Scoped {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let authed = Authed::from_request_parts(parts, state).await?;
        let project_id = parts
            .headers
            .get(PROJECT_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| Uuid::parse_str(v).ok())
            .ok_or_else(|| {
                api_error(
                    authed.request_id,
                    &ApiError::Domain(DomainError::InvalidRequest(format!(
                        "missing or invalid {PROJECT_HEADER} header (project scope is required)"
                    ))),
                )
            })?;
        let tenant_id = authed.delegation.scope.tenant_id;
        Ok(Scoped {
            delegation: authed.delegation,
            scope: TenantScope::new(tenant_id, project_id),
            request_id: authed.request_id,
        })
    }
}

fn bearer_token(parts: &Parts) -> Option<&str> {
    let value = parts.headers.get(axum::http::header::AUTHORIZATION)?;
    let value = value.to_str().ok()?;
    value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
}
