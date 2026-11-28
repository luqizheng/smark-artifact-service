//! WP-AS-05 Version 生命周期集成测试。
//!
//! 拓扑：真实 `FileServiceStorage` 适配器 ↔ loopback fake LMMS
//! （token-service mint + file-service multipart/info/download/presign/delete），
//! 驱动真实 HTTP 提交协议：pending 会话 → sha256 重算校验 → 单事务
//! output/ready → set_current。
//!
//! fail-closed 矩阵：对象缺失（404）、字段缺失（400）、指纹不符（400）、
//! 重复 commit（409）、file-service 断供（503）、委托缺 lmms_tid（503）；
//! 以及 V10 操作绑定在版本写路径仍被强制（body 变异 → 403）。
//!
//! 注：Artifact 一律经 `sql_insert_artifact` 布景（API 创建路径的 V10 绑定
//! 语义由 wp04_operation_binding / as1 测试覆盖，本文件聚焦版本链路）。

mod common;

use artifact_application::StoragePort;
use artifact_infrastructure::file_service::service_jwt::ServiceJwtMinter;
use artifact_infrastructure::file_service::storage::{FileServiceConfig, FileServiceStorage};
use artifact_infrastructure::test_support::{TestDelegationVerifier, TestTokenBinding};
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

// ── loopback fake LMMS ─────────────────────────────────────────────────

#[derive(Clone, Default)]
struct FileState {
    objects: Arc<Mutex<HashMap<String, StoredObject>>>,
    deleted: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone, Default)]
struct StoredObject {
    bytes: Vec<u8>,
    #[allow(dead_code)] // 记录 filename 供未来断言扩展；当前仅 bytes 参与
    filename: String,
}

async fn fake_upload(
    AxumState(state): AxumState<FileState>,
    mut multipart: Multipart,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, &'static str)> {
    let mut data: Option<(Vec<u8>, String)> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("file") {
            let name = field.file_name().unwrap_or_default().to_string();
            data = Some((field.bytes().await.unwrap().to_vec(), name));
        }
    }
    let Some((bytes, filename)) = data else {
        return Err((StatusCode::BAD_REQUEST, "missing file field"));
    };
    let id = Uuid::new_v4().to_string();
    let size = bytes.len();
    state
        .objects
        .lock()
        .unwrap()
        .insert(id.clone(), StoredObject { bytes, filename });
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
    let Some(obj) = objects.get(&id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    Ok(axum::Json(serde_json::json!({
        "id": id,
        "size": obj.bytes.len(),
        "content_type": serde_json::Value::Null,
        "hash": serde_json::Value::Null,
    })))
}

async fn fake_download(
    AxumState(state): AxumState<FileState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Vec<u8>, StatusCode> {
    let objects = state.objects.lock().unwrap();
    let Some(obj) = objects.get(&id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    Ok(obj.bytes.clone())
}

async fn fake_delete(
    AxumState(state): AxumState<FileState>,
    AxumPath(id): AxumPath<String>,
) -> StatusCode {
    match state.objects.lock().unwrap().remove(&id) {
        Some(_) => {
            state.deleted.lock().unwrap().push(id);
            StatusCode::OK
        }
        None => StatusCode::NOT_FOUND,
    }
}

async fn fake_presign() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "url": "https://presigned.invalid/get" }))
}

/// 起 loopback server（task 常驻至进程结束），返回 base URL。
async fn spawn_axum(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("fake server");
    });
    format!("http://{addr}")
}

async fn fake_file_service() -> (String, FileState) {
    let state = FileState::default();
    let app = Router::new()
        .route("/file/upload", post(fake_upload))
        .route("/file/{id}/info", get(fake_info))
        .route("/file/{id}", get(fake_download).delete(delete(fake_delete)))
        .route("/file/presign", post(fake_presign))
        .with_state(state.clone());
    (spawn_axum(app).await, state)
}

#[derive(Clone, Default)]
struct TokenState {
    hits: Arc<Mutex<usize>>,
}

async fn fake_token_service() -> (String, TokenState) {
    let state = TokenState::default();
    let app = Router::new()
        .route(
            "/tokens/service-jwt",
            post(|AxumState(st): AxumState<TokenState>| async move {
                *st.hits.lock().unwrap() += 1;
                (
                    StatusCode::OK,
                    axum::Json(
                        serde_json::json!({"access_token": "fake-svc-token", "expires_in": 3600}),
                    ),
                )
            }),
        )
        .with_state(state.clone());
    (spawn_axum(app).await, state)
}

fn make_storage(file_base: &str, token_base: &str) -> Arc<dyn StoragePort> {
    let tokens = ServiceJwtMinter::new(
        artifact_infrastructure::file_service::service_jwt::ServiceJwtConfig {
            token_service_url: token_base.to_string(),
            bootstrap_token: "fake-bootstrap".into(),
            request_timeout: Duration::from_secs(5),
            refresh_skew: Duration::from_secs(60),
        },
    )
    .expect("minter config");
    let storage = FileServiceStorage::new(
        FileServiceConfig {
            file_service_url: file_base.to_string(),
            upload_max_bytes: 1_048_576,
            request_timeout: Duration::from_secs(5),
        },
        tokens,
    )
    .expect("storage config");
    Arc::new(storage)
}

/// 经适配器真实上传一个对象（全链测试的"调用方先上传"步骤）。
async fn upload_via_adapter(storage: &Arc<dyn StoragePort>, body: &[u8]) -> String {
    storage
        .upload_object(
            LMMS_TID,
            bytes::Bytes::copy_from_slice(body),
            "source.bin",
            Some("application/octet-stream"),
        )
        .await
        .expect("adapter upload")
}

// ── 测试上下文 ─────────────────────────────────────────────────────────

struct Ctx {
    base: String,
    verifier: Arc<TestDelegationVerifier>,
}

async fn setup(pool: &sqlx::PgPool) -> (Ctx, FileState, Arc<dyn StoragePort>) {
    let (file_base, file_state) = fake_file_service().await;
    let (token_base, _token_state) = fake_token_service().await;
    let storage = make_storage(&file_base, &token_base);
    let verifier = Arc::new(TestDelegationVerifier::new());
    let base = spawn_app_with_storage(pool.clone(), verifier.clone(), storage.clone()).await;
    (Ctx { base, verifier }, file_state, storage)
}

impl Ctx {
    /// 为具体写请求铸造 V10 绑定 + lmms_tid 的 token（WP-04 红线：写 token
    /// 必带 caller_request_id；指纹按 method+path+caller+body 计算）。
    #[allow(clippy::too_many_arguments)]
    fn mint_bound(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: &serde_json::Value,
        lmms_tid: Option<&str>,
    ) {
        let caller = Uuid::new_v4();
        let hash = v1_fingerprint(method, path, caller, body);
        self.verifier.allow_with_tid_binding(
            token,
            TENANT,
            PROJECT_1,
            "test-user",
            artifact_domain::ActorType::User,
            &all_actions(),
            TestTokenBinding {
                artifact_id: None,
                operation_id: None,
                caller_request_id: Some(caller),
                request_hash: Some(hash),
            },
            lmms_tid,
        );
    }
}

// ── 场景 ───────────────────────────────────────────────────────────────

/// s1 全链：上传 → 201 pending → commit 200 ready（manifest 校验事实）→
/// set_current → get_version。
#[sqlx::test(migrations = "../migrations")]
async fn s1_full_chain_commit_and_set_current(pool: sqlx::PgPool) {
    let (ctx, _files, storage) = setup(&pool).await;
    let http = reqwest::Client::new();

    let artifact_id = sql_insert_artifact(&pool, TENANT, PROJECT_1, "wp05-s1").await;
    let body = b"version source payload";
    let object_id = upload_via_adapter(&storage, body).await;
    let sha = sha256_hex(std::str::from_utf8(body).unwrap());

    let create_path = format!("/v1/artifacts/{artifact_id}/versions");
    let create_body = serde_json::json!({
        "source_format": "svg",
        "source_object_id": object_id,
        "source_sha256": sha,
    });
    ctx.mint_bound("tok-1", "POST", &create_path, &create_body, Some("42"));
    let resp = http
        .post(format!("{}{create_path}", ctx.base))
        .bearer_auth("tok-1")
        .header("X-Project-Id", PROJECT_1.to_string())
        .json(&create_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "create session must succeed");
    assert!(
        resp.headers()
            .get("location")
            .is_some_and(|v| v.to_str().unwrap().contains("/versions/")),
        "Location header must point at the version resource"
    );
    let created: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(created["state"], "pending");
    assert_eq!(created["source_sha256"].as_str().unwrap().len(), 64);
    let version_id = created["id"].as_str().unwrap().to_string();
    assert!(created["version_number"].as_i64().unwrap() >= 1);

    let commit_path = format!("/v1/artifacts/{artifact_id}/versions/{version_id}/commit");
    ctx.mint_bound(
        "tok-1b",
        "POST",
        &commit_path,
        &serde_json::Value::Null,
        Some("42"),
    );
    let resp = http
        .post(format!("{}{commit_path}", ctx.base))
        .bearer_auth("tok-1b")
        .header("X-Project-Id", PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "commit must succeed");
    let committed: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(committed["state"], "ready");
    let manifest = &committed["manifest"];
    assert_eq!(manifest["sha256"], sha.as_str());
    assert_eq!(manifest["object_id"], object_id.as_str());
    assert_eq!(manifest["size_bytes"].as_i64().unwrap(), body.len() as i64);

    // set_current 只接受 READY 版本（版本链路打通后 AS-1 现状路径可用）。
    let cur_path = format!("/v1/artifacts/{artifact_id}/current-version");
    let cur_body = serde_json::json!({ "version_id": version_id });
    ctx.mint_bound("tok-1c", "POST", &cur_path, &cur_body, Some("42"));
    let resp = http
        .post(format!("{}{cur_path}", ctx.base))
        .bearer_auth("tok-1c")
        .header("X-Project-Id", PROJECT_1.to_string())
        .json(&cur_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "set_current on READY must succeed");

    ctx.verifier.allow(
        "tok-1r",
        TENANT,
        PROJECT_1,
        "test-user",
        artifact_domain::ActorType::User,
        &[artifact_domain::Action::VersionRead],
    );
    let resp = http
        .get(format!(
            "{}/v1/artifacts/{artifact_id}/versions/{version_id}",
            ctx.base
        ))
        .bearer_auth("tok-1r")
        .header("X-Project-Id", PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["state"],
        "ready"
    );
}

/// s2 声明指纹与实际对象不符 → 400，且 Version 保持 pending。
#[sqlx::test(migrations = "../migrations")]
async fn s2_sha_mismatch_rejected_and_version_stays_pending(pool: sqlx::PgPool) {
    let (ctx, files, storage) = setup(&pool).await;
    let http = reqwest::Client::new();

    let artifact_id = sql_insert_artifact(&pool, TENANT, PROJECT_1, "wp05-s2").await;
    let object_id = upload_via_adapter(&storage, b"actual-bytes").await;

    let create_path = format!("/v1/artifacts/{artifact_id}/versions");
    let create_body = serde_json::json!({
        "source_object_id": object_id,
        "source_sha256": sha256_hex("declared-but-wrong"),
    });
    ctx.mint_bound("tok-2", "POST", &create_path, &create_body, Some("42"));
    let resp = http
        .post(format!("{}{create_path}", ctx.base))
        .bearer_auth("tok-2")
        .header("X-Project-Id", PROJECT_1.to_string())
        .json(&create_body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        201,
        "session creation only records the claim"
    );
    let version_id: String = resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .into();

    let commit_path = format!("/v1/artifacts/{artifact_id}/versions/{version_id}/commit");
    ctx.mint_bound(
        "tok-2b",
        "POST",
        &commit_path,
        &serde_json::Value::Null,
        Some("42"),
    );
    let resp = http
        .post(format!("{}{commit_path}", ctx.base))
        .bearer_auth("tok-2b")
        .header("X-Project-Id", PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "sha mismatch must be rejected");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "INVALID_REQUEST");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sha256 mismatch")
    );

    // Version 保持 pending（可修正后重试）；调用方对象不被删除（补偿删除
    // 仅用于 AS 自身上传的输出对象——WP-AS-06 Worker 路径）。
    ctx.verifier.allow(
        "tok-2r",
        TENANT,
        PROJECT_1,
        "test-user",
        artifact_domain::ActorType::User,
        &[artifact_domain::Action::VersionRead],
    );
    let resp = http
        .get(format!(
            "{}/v1/artifacts/{artifact_id}/versions/{version_id}",
            ctx.base
        ))
        .bearer_auth("tok-2r")
        .header("X-Project-Id", PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["state"],
        "pending"
    );
    assert!(
        !files.deleted.lock().unwrap().contains(&object_id),
        "caller-owned objects must survive a failed commit"
    );
}

/// s3 对象不存在 → 404（fail-early，不创建会话）。
#[sqlx::test(migrations = "../migrations")]
async fn s3_missing_object_is_404(pool: sqlx::PgPool) {
    let (ctx, _files, _storage) = setup(&pool).await;
    let http = reqwest::Client::new();
    let artifact_id = sql_insert_artifact(&pool, TENANT, PROJECT_1, "wp05-s3").await;
    let path = format!("/v1/artifacts/{artifact_id}/versions");
    let body = serde_json::json!({
        "source_object_id": Uuid::new_v4().to_string(),
        "source_sha256": sha256_hex("x"),
    });
    ctx.mint_bound("tok-3", "POST", &path, &body, Some("42"));
    let resp = http
        .post(format!("{}{path}", ctx.base))
        .bearer_auth("tok-3")
        .header("X-Project-Id", PROJECT_1.to_string())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// s4 缺 source_object_id / source_sha256 / 非法 sha → 400。
#[sqlx::test(migrations = "../migrations")]
async fn s4_missing_required_fields_are_400(pool: sqlx::PgPool) {
    let (ctx, _files, _storage) = setup(&pool).await;
    let http = reqwest::Client::new();
    let artifact_id = sql_insert_artifact(&pool, TENANT, PROJECT_1, "wp05-s4").await;
    let path = format!("/v1/artifacts/{artifact_id}/versions");
    let url = format!("{}{path}", ctx.base);

    for (i, body) in [
        serde_json::json!({ "source_sha256": sha256_hex("x") }),
        serde_json::json!({ "source_object_id": Uuid::new_v4().to_string() }),
        serde_json::json!({
            "source_object_id": Uuid::new_v4().to_string(),
            "source_sha256": "not-hex",
        }),
    ]
    .into_iter()
    .enumerate()
    {
        let token = format!("tok-4-{i}");
        ctx.mint_bound(&token, "POST", &path, &body, Some("42"));
        let resp = http
            .post(&url)
            .bearer_auth(&token)
            .header("X-Project-Id", PROJECT_1.to_string())
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "body={body}");
    }
}

/// s5 已 commit 的版本再 commit → 409（READY 终态）。
#[sqlx::test(migrations = "../migrations")]
async fn s5_double_commit_is_409(pool: sqlx::PgPool) {
    let (ctx, _files, storage) = setup(&pool).await;
    let http = reqwest::Client::new();
    let artifact_id = sql_insert_artifact(&pool, TENANT, PROJECT_1, "wp05-s5").await;
    let object_id = upload_via_adapter(&storage, b"once").await;

    let create_path = format!("/v1/artifacts/{artifact_id}/versions");
    let create_body = serde_json::json!({
        "source_object_id": object_id,
        "source_sha256": sha256_hex("once"),
    });
    ctx.mint_bound("tok-5", "POST", &create_path, &create_body, Some("42"));
    let resp = http
        .post(format!("{}{create_path}", ctx.base))
        .bearer_auth("tok-5")
        .header("X-Project-Id", PROJECT_1.to_string())
        .json(&create_body)
        .send()
        .await
        .unwrap();
    let version_id: String = resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .into();
    let commit_path = format!("/v1/artifacts/{artifact_id}/versions/{version_id}/commit");
    ctx.mint_bound(
        "tok-5b",
        "POST",
        &commit_path,
        &serde_json::Value::Null,
        Some("42"),
    );
    let url = format!("{}{commit_path}", ctx.base);
    let first = http
        .post(&url)
        .bearer_auth("tok-5b")
        .header("X-Project-Id", PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let second = http
        .post(&url)
        .bearer_auth("tok-5b")
        .header("X-Project-Id", PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 409);
    let body: serde_json::Value = second.json().await.unwrap();
    assert_eq!(body["error"]["code"], "VERSION_CONFLICT");
}

/// s6 委托缺 lmms_tid → 503（对象面路由 fail-closed，绝不回落租户）。
#[sqlx::test(migrations = "../migrations")]
async fn s6_missing_lmms_tid_is_fail_closed_503(pool: sqlx::PgPool) {
    let (ctx, _files, storage) = setup(&pool).await;
    let http = reqwest::Client::new();
    let artifact_id = sql_insert_artifact(&pool, TENANT, PROJECT_1, "wp05-s6").await;
    let object_id = upload_via_adapter(&storage, b"payload").await;

    // 绑定齐全但无 lmms_tid：授权/绑定通过，对象面路由 fail-closed。
    let path = format!("/v1/artifacts/{artifact_id}/versions");
    let body = serde_json::json!({
        "source_object_id": object_id,
        "source_sha256": sha256_hex("payload"),
    });
    ctx.mint_bound("tok-6", "POST", &path, &body, None);
    let resp = http
        .post(format!("{}{path}", ctx.base))
        .bearer_auth("tok-6")
        .header("X-Project-Id", PROJECT_1.to_string())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "STORAGE_UNAVAILABLE");
}

/// s7 file-service 断供 → 503（fail-closed，可重试，绝不放行）。
#[sqlx::test(migrations = "../migrations")]
async fn s7_file_service_outage_is_503(pool: sqlx::PgPool) {
    // 指向无服务的回环端口。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = listener.local_addr().unwrap();
    drop(listener);
    let (token_base, _ts) = fake_token_service().await;
    let storage = make_storage(&format!("http://{}", dead), &token_base);
    let verifier = Arc::new(TestDelegationVerifier::new());
    let base = spawn_app_with_storage(pool.clone(), verifier.clone(), storage.clone()).await;
    let http = reqwest::Client::new();
    let artifact_id = sql_insert_artifact(&pool, TENANT, PROJECT_1, "wp05-s7").await;
    let path = format!("/v1/artifacts/{artifact_id}/versions");
    let body = serde_json::json!({
        "source_object_id": Uuid::new_v4().to_string(),
        "source_sha256": sha256_hex("x"),
    });
    let caller = Uuid::new_v4();
    verifier.allow_with_tid_binding(
        "tok-7",
        TENANT,
        PROJECT_1,
        "test-user",
        artifact_domain::ActorType::User,
        &all_actions(),
        TestTokenBinding {
            caller_request_id: Some(caller),
            request_hash: Some(v1_fingerprint("POST", &path, caller, &body)),
            ..Default::default()
        },
        Some("42"),
    );
    let resp = http
        .post(format!("{}{path}", base))
        .bearer_auth("tok-7")
        .header("X-Project-Id", PROJECT_1.to_string())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "STORAGE_UNAVAILABLE");
    assert_eq!(body["error"]["retryable"], true);
}

/// s8 /v1/ready 如实上报装配模式（注入存储 → file_service_adapter）。
#[sqlx::test(migrations = "../migrations")]
async fn s8_ready_reports_storage_mode(pool: sqlx::PgPool) {
    let (file_base, _fs) = fake_file_service().await;
    let (token_base, _ts) = fake_token_service().await;
    let storage = make_storage(&file_base, &token_base);
    let base = spawn_app_with_storage(pool, artifact_api::default_verifier(), storage).await;
    let body: serde_json::Value = reqwest::get(format!("{base}/v1/ready"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["components"]["file_storage"], "file_service_adapter");
}

/// s9 V10 操作绑定在版本写路径仍强制：body 变异 → 403 SCOPE_DENIED。
#[sqlx::test(migrations = "../migrations")]
async fn s9_operation_binding_still_enforced(pool: sqlx::PgPool) {
    let (ctx, _files, storage) = setup(&pool).await;
    let artifact_id = sql_insert_artifact(&pool, TENANT, PROJECT_1, "wp05-s9").await;
    let object_id = upload_via_adapter(&storage, b"bound").await;
    let path = format!("/v1/artifacts/{artifact_id}/versions");
    let body = serde_json::json!({
        "source_object_id": object_id,
        "source_sha256": sha256_hex("bound"),
    });
    mint_write_token_with_tid(
        &ctx.verifier,
        "tok-9",
        TENANT,
        PROJECT_1,
        &all_actions(),
        "POST",
        &path,
        &body,
        "42",
    );
    // 变异 body（换 sha）→ 指纹不同 → 403。
    let mut tampered = body.clone();
    tampered["source_sha256"] = serde_json::Value::String(sha256_hex("tampered"));
    let http = reqwest::Client::new();
    let resp = http
        .post(format!("{}{path}", ctx.base))
        .bearer_auth("tok-9")
        .header("X-Project-Id", PROJECT_1.to_string())
        .json(&tampered)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "SCOPE_DENIED");
}

/// s10 补偿：删除对象幂等（404 视为已删除）。
#[sqlx::test(migrations = "../migrations")]
async fn s10_delete_object_is_idempotent(pool: sqlx::PgPool) {
    let (_ctx, files, storage) = setup(&pool).await;
    let object_id = upload_via_adapter(&storage, b"to-be-deleted").await;
    storage
        .delete_object(LMMS_TID, &object_id)
        .await
        .expect("first delete");
    // 第二次：404 → Ok（幂等补偿语义）。
    storage
        .delete_object(LMMS_TID, &object_id)
        .await
        .expect("second delete");
    assert!(files.deleted.lock().unwrap().contains(&object_id));
    let err = storage
        .head_object(LMMS_TID, &object_id)
        .await
        .expect_err("object must be gone");
    assert!(matches!(
        err,
        artifact_application::StorageError::ObjectNotFound
    ));
}
