//! 路由装配与健康/就绪/Generator 端点。

pub mod artifacts;

use artifact_contract::{HealthResponse, ReadyComponents, ReadyResponse};
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use std::time::Duration;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::error::{ApiError, audit_and_enrich_layer, request_id_layer};
use crate::state::AppState;

/// 完整路由（含中间件）。测试与 main 共用，保证生产/测试行为一致。
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/ready", get(ready))
        .route("/v1/generators", get(list_generators))
        .route(
            "/v1/artifacts",
            post(artifacts::create_artifact).get(artifacts::list_artifacts),
        )
        .route(
            "/v1/artifacts/{artifact_id}",
            get(artifacts::get_artifact).patch(artifacts::patch_artifact),
        )
        .route(
            "/v1/artifacts/{artifact_id}/versions",
            post(artifacts::create_version_session).get(artifacts::list_versions),
        )
        .route(
            "/v1/artifacts/{artifact_id}/versions/{version_id}",
            get(artifacts::get_version),
        )
        .route(
            "/v1/artifacts/{artifact_id}/versions/{version_id}/commit",
            post(artifacts::commit_version),
        )
        .route(
            "/v1/artifacts/{artifact_id}/current-version",
            post(artifacts::set_current_version),
        )
        .route(
            "/v1/artifacts/{artifact_id}/references",
            post(artifacts::create_reference).get(artifacts::list_references),
        )
        .route(
            "/v1/artifacts/{artifact_id}/references/{reference_id}",
            delete(artifacts::remove_reference),
        )
        .route(
            "/v1/artifacts/{artifact_id}/generation-jobs",
            post(artifacts::request_generation),
        )
        .route("/v1/generation-jobs/{job_id}", get(artifacts::get_job))
        .route(
            "/v1/generation-jobs/{job_id}/cancel",
            post(artifacts::cancel_job),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            audit_and_enrich_layer,
        ))
        .layer(middleware::from_fn(request_id_layer))
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(10),
        ))
        .with_state(state)
}

/// /v1/health：进程存活，不探测外部依赖（ART-01 §10.4）。
async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        service: "artifact-api".to_string(),
        version: state.build_info.version.to_string(),
        git_sha: state.build_info.git_sha.to_string(),
        built_at: state.build_info.built_at.to_string(),
    })
}

/// /v1/ready：数据库可达即 ready；file-storage 协议被 P0 阻断时如实上报。
async fn ready(State(state): State<AppState>) -> Response {
    let database_ok = sqlx::query("SELECT 1").fetch_one(&state.pool).await.is_ok();
    let body = ReadyResponse {
        status: if database_ok { "ready" } else { "not_ready" }.to_string(),
        components: ReadyComponents {
            database: if database_ok { "ok" } else { "unreachable" }.to_string(),
            // 诚实上报：file-service 上传/finalize 协议被 ART-02 存储 P0 阻断。
            file_storage: "protocol_blocked_p0".to_string(),
        },
        version: state.build_info.version.to_string(),
    };
    let status = if database_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

/// /v1/generators：注册表实况（未实现 Adapter 如实上报 unavailable）。
async fn list_generators(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let generators = state.generators.list().await.map_err(ApiError::Domain)?;
    Ok(Json(serde_json::json!({ "generators": generators })))
}
