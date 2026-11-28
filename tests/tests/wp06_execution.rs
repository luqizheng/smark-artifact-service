//! WP-AS-06 GenerationJob 执行链路 E2E（ART-03A §8 单一受控提交路径）。
//!
//! 拓扑：Worker `Executor`（真实 `PgWorkerCoordination` + 真实
//! `FileServiceStorage` ↔ loopback fake LMMS token/file-service）+ 可编程
//! FakeRenderer（注入 [`GeneratorAdapter`]，覆盖四类渲染结果）。
//! 真实 vendored CLI 的本地冒烟由 `--features real-cli` 门控（CI 无 node）。
//!
//! 场景：
//! * e1 全链：claim → 渲染（合法 SVG）→ 上传 → inspect 重算 sha256 →
//!   单事务提交 → Job succeeded + 目标 Version READY（manifest 校验事实
//!   + sanitizer 策略版本）+ Output passed + audit `generation.complete`；
//! * e2 静态化违规产物 → Job failed `SVG_SAFETY_REJECTED`，零 Output/Version；
//! * e3 渲染器不可用 → `fail(retryable=true)` → Job `retry_wait`（attempt 1）；
//! * e4 渲染器拒绝输入 → `INPUT_REJECTED`（不重试）→ Job failed；
//! * e5 file-service 断供 → 上传失败 → `OUTPUT_UPLOAD_FAILED`（可重试语义）。

mod common;

use artifact_application::{StoragePort, WorkerCoordinationPort};
use artifact_infrastructure::file_service::service_jwt::ServiceJwtMinter;
use artifact_infrastructure::file_service::storage::{FileServiceConfig, FileServiceStorage};
use artifact_infrastructure::postgres::PgWorkerCoordination;
use artifact_worker::execute::Executor;
use artifact_worker::seecode_adapter::{GeneratorAdapter, SEECODE_ADAPTER_VERSION};
use async_trait::async_trait;
use axum::{
    Router,
    extract::{Multipart, Path as AxumPath, State as AxumState},
    http::StatusCode,
    routing::{delete, get, post},
};
use common::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

// ── FakeRenderer（可编程渲染结果） ─────────────────────────────────────

const SAFE_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
    <title>wp06 fixture</title>
    <style>@font-face{font-family:x;src:url(data:font/woff2;base64,AAAA)}</style>
    <g class="node"><text x="1" y="2">节点</text></g>
</svg>"#;

const UNSAFE_SVG: &str =
    r#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script><text>x</text></svg>"#;

#[derive(Debug, Clone, Copy)]
enum RenderMode {
    Safe,
    Unsafe,
    Unavailable,
    RejectInput,
}

#[derive(Clone)]
struct FakeRenderer(Arc<Mutex<RenderMode>>);

#[async_trait]
impl GeneratorAdapter for FakeRenderer {
    fn generator_id(&self) -> &'static str {
        "seecode"
    }

    fn adapter_version(&self) -> &'static str {
        SEECODE_ADAPTER_VERSION
    }

    async fn render(
        &self,
        _input: &serde_json::Value,
    ) -> Result<Vec<u8>, artifact_worker::seecode_adapter::AdapterError> {
        use artifact_worker::seecode_adapter::AdapterError;
        match *self.0.lock().unwrap() {
            RenderMode::Safe => Ok(SAFE_SVG.as_bytes().to_vec()),
            RenderMode::Unsafe => Ok(UNSAFE_SVG.as_bytes().to_vec()),
            RenderMode::Unavailable => {
                Err(AdapterError::Unavailable("fake renderer outage".into()))
            }
            RenderMode::RejectInput => Err(AdapterError::Rejected("bad diagram.type".into())),
        }
    }
}

// ── loopback fake LMMS ─────────────────────────────────────────────────

#[derive(Clone, Default)]
struct FileState {
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

async fn fake_upload(
    AxumState(state): AxumState<FileState>,
    mut multipart: Multipart,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, &'static str)> {
    let mut data: Option<Vec<u8>> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("file") {
            data = Some(field.bytes().await.unwrap().to_vec());
        }
    }
    let Some(bytes) = data else {
        return Err((StatusCode::BAD_REQUEST, "missing file field"));
    };
    let id = Uuid::new_v4().to_string();
    let size = bytes.len();
    state.objects.lock().unwrap().insert(id.clone(), bytes);
    Ok(axum::Json(serde_json::json!({
        "id": id,
        "size": size,
        "content_type": serde_json::Value::Null,
        "hash": serde_json::Value::Null,
    })))
}

async fn fake_info(
    AxumState(state): AxumState<FileState>,
    AxumPath(id): AxumPath<String>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    let objects = state.objects.lock().unwrap();
    let Some(bytes) = objects.get(&id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    Ok(axum::Json(serde_json::json!({
        "id": id,
        "size": bytes.len(),
        "content_type": serde_json::Value::Null,
        "hash": serde_json::Value::Null,
    })))
}

async fn fake_download(
    AxumState(state): AxumState<FileState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Vec<u8>, StatusCode> {
    let objects = state.objects.lock().unwrap();
    let Some(bytes) = objects.get(&id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    Ok(bytes.clone())
}

async fn fake_delete(
    AxumState(state): AxumState<FileState>,
    AxumPath(id): AxumPath<String>,
) -> StatusCode {
    match state.objects.lock().unwrap().remove(&id) {
        Some(_) => StatusCode::OK,
        None => StatusCode::NOT_FOUND,
    }
}

async fn spawn_fake_file_service() -> (String, FileState) {
    let state = FileState::default();
    let app = Router::new()
        .route("/file/upload", post(fake_upload))
        .route("/file/{id}/info", get(fake_info))
        .route("/file/{id}", get(fake_download).delete(delete(fake_delete)))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.expect("fake file server") });
    (format!("http://{addr}"), state)
}

async fn spawn_fake_token_service() -> String {
    let app = Router::new().route(
        "/tokens/service-jwt",
        post(|| async {
            (
                StatusCode::OK,
                axum::Json(
                    serde_json::json!({"access_token": "fake-svc-token", "expires_in": 3600}),
                ),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.expect("fake token server") });
    format!("http://{addr}")
}

fn real_storage(file_base: &str, token_base: &str) -> Arc<dyn StoragePort> {
    let tokens = ServiceJwtMinter::new(
        artifact_infrastructure::file_service::service_jwt::ServiceJwtConfig {
            token_service_url: token_base.to_string(),
            bootstrap_token: "fake-bootstrap".into(),
            request_timeout: Duration::from_secs(5),
            refresh_skew: Duration::from_secs(60),
        },
    )
    .expect("minter");
    Arc::new(
        FileServiceStorage::new(
            FileServiceConfig {
                file_service_url: file_base.to_string(),
                upload_max_bytes: 1_048_576,
                request_timeout: Duration::from_secs(5),
            },
            tokens,
        )
        .expect("storage"),
    )
}

// ── 测试执行器装配 ─────────────────────────────────────────────────────

fn executor_for(pool: &sqlx::PgPool, storage: Arc<dyn StoragePort>, mode: RenderMode) -> Executor {
    let coordination: Arc<dyn WorkerCoordinationPort> =
        Arc::new(PgWorkerCoordination { pool: pool.clone() });
    Executor::new(
        coordination,
        storage,
        Arc::new(FakeRenderer(Arc::new(Mutex::new(mode)))),
        "wp06-worker",
        30,
        LMMS_TID,
    )
}

async fn seed_job(pool: &sqlx::PgPool, artifact_id: Uuid) -> Uuid {
    sql_enable_seecode(pool, SEECODE_ADAPTER_VERSION).await;
    sql_create_job(pool, TENANT, PROJECT_1, artifact_id, &JobOptions::default()).await
}

async fn job_state(pool: &sqlx::PgPool, job_id: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, error_code FROM artifact.generation_job WHERE id = $1")
        .bind(job_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ── 场景 ───────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn e1_full_chain_commit(pool: sqlx::PgPool) {
    let artifact = sql_insert_artifact(&pool, TENANT, PROJECT_1, "wp06-e1").await;
    let job_id = seed_job(&pool, artifact).await;
    let (file_base, files) = spawn_fake_file_service().await;
    let token_base = spawn_fake_token_service().await;
    let executor = executor_for(
        &pool,
        real_storage(&file_base, &token_base),
        RenderMode::Safe,
    );

    executor.claim_and_execute().await;

    // Job 终态与结果引用。
    let (status, target): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT status, target_version_id FROM artifact.generation_job WHERE id = $1",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "succeeded", "job must succeed");
    let version_id = target.expect("target version must be materialized");

    // Version READY + manifest 校验事实。
    let (state, manifest): (String, serde_json::Value) =
        sqlx::query_as("SELECT state, manifest FROM artifact.artifact_version WHERE id = $1")
            .bind(version_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "ready");
    assert_eq!(manifest["sanitizer_policy_version"], "svg-allowlist-1");
    assert_eq!(manifest["sha256"].as_str().unwrap().len(), 64);
    assert!(manifest["size_bytes"].as_i64().unwrap() > 0);

    // Output（primary / passed / file_object_id 指向 fake LMMS 对象）。
    let (sha, file_object, safety): (String, String, String) = sqlx::query_as(
        "SELECT sha256, file_object_id, safety_status FROM artifact.artifact_output \
         WHERE version_id = $1 AND output_role = 'primary'",
    )
    .bind(version_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(safety, "passed");
    assert_eq!(sha, manifest["sha256"]);
    assert!(
        files.objects.lock().unwrap().contains_key(&file_object),
        "output object must exist in the (fake) object store"
    );

    // 审计事件（受控函数写入）。
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.audit_event \
         WHERE action = 'generation.complete' AND object_id = $1 AND outcome = 'allowed'",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audits, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn e2_unsafe_output_fails_without_residue(pool: sqlx::PgPool) {
    let artifact = sql_insert_artifact(&pool, TENANT, PROJECT_1, "wp06-e2").await;
    let job_id = seed_job(&pool, artifact).await;
    let (file_base, _files) = spawn_fake_file_service().await;
    let token_base = spawn_fake_token_service().await;
    let executor = executor_for(
        &pool,
        real_storage(&file_base, &token_base),
        RenderMode::Unsafe,
    );

    executor.claim_and_execute().await;

    let (status, error_code) = job_state(&pool, job_id).await;
    assert_eq!(status, "failed", "safety violation must fail the job");
    assert_eq!(error_code.as_deref(), Some("SVG_SAFETY_REJECTED"));
    let versions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM artifact.artifact_version WHERE artifact_id = $1")
            .bind(artifact)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(versions, 0, "no version may be created for unsafe output");
    let outputs: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact.artifact_output")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(outputs, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn e3_renderer_unavailable_is_retryable(pool: sqlx::PgPool) {
    let artifact = sql_insert_artifact(&pool, TENANT, PROJECT_1, "wp06-e3").await;
    let job_id = seed_job(&pool, artifact).await;
    let (file_base, _files) = spawn_fake_file_service().await;
    let token_base = spawn_fake_token_service().await;
    let executor = executor_for(
        &pool,
        real_storage(&file_base, &token_base),
        RenderMode::Unavailable,
    );

    executor.claim_and_execute().await;

    let (status, _error_code) = job_state(&pool, job_id).await;
    assert_eq!(status, "retry_wait", "transient failure must be requeued");
    let attempts: i32 =
        sqlx::query_scalar("SELECT attempt_count FROM artifact.generation_job WHERE id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn e4_rejected_input_fails_without_retry(pool: sqlx::PgPool) {
    let artifact = sql_insert_artifact(&pool, TENANT, PROJECT_1, "wp06-e4").await;
    let job_id = seed_job(&pool, artifact).await;
    let (file_base, _files) = spawn_fake_file_service().await;
    let token_base = spawn_fake_token_service().await;
    let executor = executor_for(
        &pool,
        real_storage(&file_base, &token_base),
        RenderMode::RejectInput,
    );

    executor.claim_and_execute().await;

    let (status, error_code) = job_state(&pool, job_id).await;
    assert_eq!(
        status, "failed",
        "deterministic input rejection must not retry"
    );
    assert_eq!(error_code.as_deref(), Some("INPUT_REJECTED"));
}

#[sqlx::test(migrations = "../migrations")]
async fn e5_file_service_outage_fails_upload_retryably(pool: sqlx::PgPool) {
    let artifact = sql_insert_artifact(&pool, TENANT, PROJECT_1, "wp06-e5").await;
    let job_id = seed_job(&pool, artifact).await;
    // 死端口 file-service（token 服务正常 → 失败来自上传路径）。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = listener.local_addr().unwrap();
    drop(listener);
    let token_base = spawn_fake_token_service().await;
    let executor = executor_for(
        &pool,
        real_storage(&format!("http://{dead}"), &token_base),
        RenderMode::Safe,
    );

    executor.claim_and_execute().await;

    let (status, error_code) = job_state(&pool, job_id).await;
    assert_eq!(
        error_code.as_deref(),
        Some("STORAGE_UNAVAILABLE"),
        "transient failure maps to the retry-whitelist code"
    );
    assert!(
        status == "retry_wait" || status == "failed",
        "upload failure must be reported as retryable (DB whitelist decides): {status}"
    );
}
