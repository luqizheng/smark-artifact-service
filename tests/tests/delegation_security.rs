//! WP-LMMS-DELEGATION-01 安全测试矩阵（契约 v0.2 §3；SECURITY-REVIEW §12）。
//!
//! 覆盖任务书 18 场景（以负面为主）+ 绑定存储并发/持久化 + DB 层不变量：
//!   1  正常 Token（E2E 创建/读取）
//!   2  错误签名                10 跨租户访问
//!   3  错误 issuer             11 跨项目访问（claim vs 头不一致）
//!   4  错误 audience           12 Action 越权
//!   5  未知 kid（含限频刷新）  13 同一 jti 不同请求
//!   6  JWKS 不可用 → 503       14 同一请求并发重放
//!   7  Token 过期              15 同一请求合法重试
//!   8  未来 iat                16 请求指纹篡改
//!   9  超长 TTL                17 撤销语义（TTL 残余窗口）
//!                              18 缺失 X-Project-Id
//!
//! 测试密钥在运行期用 `rsa` 生成（无任何真实凭据落盘/入 git）；JWKS 服务器
//! 为进程内 axum 回环服务。生产配置仍要求 https JWKS（config 单测覆盖）。

mod common;

use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

use artifact_application::{
    DelegationBindingOutcome, DelegationBindingStore, DelegationTokenUse, DelegationVerifier,
    assert_delegation_matches_request, canonical_request_fingerprint,
};
use artifact_infrastructure::delegation::jwks::{JwksCache, JwksLookup};
use artifact_infrastructure::delegation::{AiPmDelegationVerifier, TrustedIssuer};
use artifact_infrastructure::postgres::PgDelegationBindingStore;
use artifact_infrastructure::test_support::TestDelegationVerifier;
use axum::routing::get;
use base64::Engine as _;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use rand::rngs::OsRng;
use rsa::RsaPrivateKey;
use rsa::pkcs1::{EncodeRsaPrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

// ── 测试密钥与 token 铸造 ───────────────────────────────────────────────

/// 运行期生成、全测试二进制共享的 RSA 私钥。仅存在于测试进程内存。
static KEY_PRIMARY: LazyLock<RsaPrivateKey> = LazyLock::new(gen_key);
static KEY_OTHER: LazyLock<RsaPrivateKey> = LazyLock::new(gen_key);
pub const KID_PRIMARY: &str = "test-key-1";

fn gen_key() -> RsaPrivateKey {
    RsaPrivateKey::new(&mut OsRng, 2048).expect("test rsa keygen")
}

fn encoding_key(key: &RsaPrivateKey) -> EncodingKey {
    let pem = key.to_pkcs1_pem(LineEnding::LF).expect("pem encode");
    EncodingKey::from_rsa_pem(pem.as_bytes()).expect("encoding key")
}

/// JWKS 条目（RSA 公钥部分）。
fn jwk_entry(key: &RsaPrivateKey, kid: &str) -> Value {
    let n = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.n().to_bytes_be());
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.e().to_bytes_be());
    json!({"kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid, "n": n, "e": e})
}

struct JwksServer {
    base_url: String,
    keys: Arc<RwLock<Vec<Value>>>,
    fetches: Arc<AtomicUsize>,
}

/// 进程内 JWKS 回环服务器（仅测试）。
async fn spawn_jwks(initial: Vec<Value>) -> JwksServer {
    let keys = Arc::new(RwLock::new(initial));
    let fetches = Arc::new(AtomicUsize::new(0));
    let keys_state = Arc::clone(&keys);
    let fetch_state = Arc::clone(&fetches);
    let app = axum::Router::new().route(
        "/jwks.json",
        get(move || {
            let keys = Arc::clone(&keys_state);
            let fetches = Arc::clone(&fetch_state);
            async move {
                fetches.fetch_add(1, Ordering::SeqCst);
                let snapshot = keys.read().expect("jwks lock").clone();
                axum::Json(json!({ "keys": snapshot }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("jwks server");
    });
    JwksServer {
        base_url: format!("http://{addr}"),
        keys,
        fetches,
    }
}

impl JwksServer {
    fn verifier(&self, cache_ttl: Duration) -> Arc<AiPmDelegationVerifier> {
        let http = reqwest::Client::builder().build().unwrap();
        // cache_ttl 同时充当软刷新间隔与最大陈旧时间（测试用）。
        let cache = JwksCache::with_policy(http, cache_ttl, cache_ttl);
        Arc::new(
            AiPmDelegationVerifier::with_jwks_cache(
                vec![TrustedIssuer::new(
                    "ai-pm",
                    format!("{}/jwks.json", self.base_url),
                )],
                cache,
            )
            .expect("verifier assembly"),
        )
    }
}

/// 铸造一枚 token：`base_claims()` + 顶层覆盖。
fn mint(key: &RsaPrivateKey, kid: &str, claims: Value) -> String {
    let header = Header {
        alg: Algorithm::RS256,
        kid: Some(kid.to_string()),
        ..Header::default()
    };
    encode(&header, &claims, &encoding_key(key)).expect("token mint")
}

/// 契约 v0.2 §3.1 的最小合法 claims。
fn base_claims() -> Value {
    let now = chrono::Utc::now().timestamp();
    json!({
        "iss": "ai-pm",
        "sub": "user-1",
        "aud": "artifact-service",
        "purpose": "delegation",
        "lmms_tid": "42",
        "tenant_id": common::TENANT,
        "project_id": common::PROJECT_1,
        "actions": ["artifact:read", "artifact:create"],
        "iat": now,
        "exp": now + 120,
        "jti": Uuid::new_v4().to_string(),
        "delegation_id": Uuid::new_v4().to_string(),
    })
}

fn claims_with(overrides: Value) -> Value {
    let mut base = base_claims();
    let overrides = overrides.as_object().expect("overrides object").clone();
    for (k, v) in overrides {
        base[k] = v;
    }
    base
}

fn valid_token() -> String {
    mint(&KEY_PRIMARY, KID_PRIMARY, base_claims())
}

// ── 场景 1：正常 Token（E2E：签发方 → AS 验证 → 业务放行） ──────────────

#[sqlx::test(migrations = "../migrations")]
async fn s01_valid_token_roundtrip_creates_and_reads(pool: sqlx::PgPool) {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let base = common::spawn_app(pool, jwks.verifier(Duration::from_secs(600))).await;
    let http = reqwest::Client::new();
    let token = valid_token();

    let created = http
        .post(format!("{base}/v1/artifacts"))
        .header("Authorization", format!("Bearer {token}"))
        .header("x-project-id", common::PROJECT_1.to_string())
        .json(&json!({
            "project_id": common::PROJECT_1,
            "name": "Delegation Fixture",
            "artifact_type": "visualization"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        created.status(),
        201,
        "valid delegation must create: {}",
        created.text().await.unwrap_or_default()
    );
    let artifact: Value = created.json().await.unwrap();

    let fetched = http
        .get(format!(
            "{base}/v1/artifacts/{}",
            artifact["id"].as_str().unwrap()
        ))
        .header("Authorization", format!("Bearer {token}"))
        .header("x-project-id", common::PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(fetched.status(), 200);
}

// ── 场景 2–9、16：token 级拒绝矩阵（直接驱动验证器，无 HTTP 层） ─────────

async fn rejects_with(server: &JwksServer, token: &str, needle: &str) {
    let verifier = server.verifier(Duration::from_secs(600));
    let err = verifier
        .verify(token)
        .await
        .expect_err("token must be rejected");
    let artifact_application::VerificationError::Unauthenticated(message) = err else {
        panic!("expected 401-class rejection, got: {err}");
    };
    let hit = message.to_lowercase().contains(&needle.to_lowercase());
    assert!(hit, "rejection '{message}' must mention '{needle}'");
    assert!(
        !message.contains("ey"),
        "error message must not echo token body"
    );
}

#[tokio::test]
async fn s02_wrong_signature_rejected() {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    // kid 指向受信 k1，但签名来自 k2 → 验签失败。
    let token = mint(&KEY_OTHER, KID_PRIMARY, base_claims());
    rejects_with(&jwks, &token, "signature").await;
}

#[tokio::test]
async fn s03_wrong_issuer_rejected() {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let token = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"iss": "other-service"})),
    );
    rejects_with(&jwks, &token, "issuer").await;
}

#[tokio::test]
async fn s04_wrong_audience_rejected() {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let token = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"aud": "file-service"})),
    );
    rejects_with(&jwks, &token, "audience").await;
}

#[tokio::test]
async fn s05_unknown_kid_rejected_and_refresh_rate_limited() {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    // 单一验证器实例（共享缓存），才能对出站次数计数。
    let verifier = jwks.verifier(Duration::from_secs(600));
    // 首个未知 kid：缓存为空 → 1 次拉取；仍未知 → 拒绝。
    let t1 = mint(&KEY_OTHER, "ghost-kid-1", base_claims());
    let err1 = verifier
        .verify(&t1)
        .await
        .expect_err("unknown kid must be rejected");
    assert!(
        matches!(err1, artifact_application::VerificationError::Unauthenticated(m) if m.contains("kid"))
    );
    assert_eq!(jwks.fetches.load(Ordering::SeqCst), 1, "initial fetch only");

    // 第二个未知 kid：缓存有效且刷新限频（30s 窗口）→ 不再出站。
    let t2 = mint(&KEY_OTHER, "ghost-kid-2", base_claims());
    let err2 = verifier
        .verify(&t2)
        .await
        .expect_err("unknown kid must be rejected");
    assert!(
        matches!(err2, artifact_application::VerificationError::Unauthenticated(m) if m.contains("kid"))
    );
    assert_eq!(
        jwks.fetches.load(Ordering::SeqCst),
        1,
        "unknown-kid refresh must be rate limited (no outbound burst)"
    );
}

#[tokio::test]
async fn s05b_published_key_rotation_is_picked_up() {
    // 轮换双活：JWKS 先发布 k1，再追加 k3；小 TTL 缓存到期后重新拉取 → k3 可用。
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let verifier = jwks.verifier(Duration::from_millis(120));
    let ok1 = mint(&KEY_PRIMARY, KID_PRIMARY, base_claims());
    assert!(
        verifier.verify(&ok1).await.is_ok(),
        "primary kid must verify"
    );

    jwks.keys
        .write()
        .expect("jwks lock")
        .push(jwk_entry(&KEY_OTHER, "rotated-key"));
    let ok3 = mint(&KEY_OTHER, "rotated-key", base_claims());
    // 等缓存过期（120ms TTL；余量充分避免 CI 抖动）。
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        verifier.verify(&ok3).await.is_ok(),
        "key published in JWKS must verify after cache refresh"
    );
}

#[tokio::test]
async fn s07_expired_token_rejected() {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let now = chrono::Utc::now().timestamp();
    // exp 落后 now 120s（> 60s 时钟偏差容忍）→ ExpiredSignature；TTL 仍为 300s。
    let token = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"iat": now - 420, "exp": now - 120})),
    );
    rejects_with(&jwks, &token, "exp").await;
}

#[tokio::test]
async fn s08_future_iat_beyond_skew_rejected() {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let now = chrono::Utc::now().timestamp();
    // iat 在未来 3600s（> 60s 偏差）。
    let token = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"iat": now + 3600, "exp": now + 3900})),
    );
    rejects_with(&jwks, &token, "iat").await;
}

#[tokio::test]
async fn s09_overlong_ttl_rejected() {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let now = chrono::Utc::now().timestamp();
    let token = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"iat": now, "exp": now + 3600})),
    );
    rejects_with(&jwks, &token, "ttl").await;
}

#[tokio::test]
async fn s16_fingerprint_tampering_rejected_at_claim_level() {
    // 声明层：claim 指纹 ≠ AS 重算指纹 → 拒绝（AS-4 接线在写路径调用同一函数）。
    let caller = Uuid::new_v4();
    let body =
        json!({"project_id": common::PROJECT_1, "name": "X", "artifact_type": "visualization"});
    let real = canonical_request_fingerprint("post", "/v1/artifacts", Some(caller), &body);
    let forged = canonical_request_fingerprint(
        "post",
        "/v1/artifacts",
        Some(caller),
        &json!({"project_id": common::PROJECT_1, "name": "Y", "artifact_type": "visualization"}),
    );
    assert_ne!(real, forged);

    let delegation =
        common::delegation_for(common::TENANT, common::PROJECT_1, Some(caller), Some(&real));
    // 同一请求 → 通过。
    assert!(assert_delegation_matches_request(&delegation, caller, &real).is_ok());
    // 篡改后的指纹 → 拒绝。
    assert!(assert_delegation_matches_request(&delegation, caller, &forged).is_err());
}

// ── 场景 6：JWKS 断供 → 503 DELEGATION_VERIFIER_UNAVAILABLE（HTTP 层） ──

#[sqlx::test(migrations = "../migrations")]
async fn s06_jwks_outage_fails_closed_with_503(pool: sqlx::PgPool) {
    // 无 JWKS 服务器：指向必然拒绝连接的回环端口（loopback http 为配置层允许
    // 的唯一明文豁免；生产 https 校验由 config 单测锁定）。
    let issuers = vec![TrustedIssuer::new("ai-pm", "http://127.0.0.1:9/jwks.json")];
    let verifier = Arc::new(AiPmDelegationVerifier::new(issuers).expect("verifier"));
    let base = common::spawn_app(pool, verifier).await;
    let http = reqwest::Client::new();

    let res = http
        .post(format!("{base}/v1/artifacts"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .header("x-project-id", common::PROJECT_1.to_string())
        .json(&json!({
            "project_id": common::PROJECT_1,
            "name": "Denied",
            "artifact_type": "visualization"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 503, "JWKS outage must fail closed with 503");
    let envelope: Value = res.json().await.unwrap();
    assert_eq!(envelope["error"]["code"], "DELEGATION_VERIFIER_UNAVAILABLE");
    assert_eq!(envelope["error"]["retryable"], true);
}

// ── 场景 10/11/12/18：HTTP 层 scope 与动作边界 ──────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn s10_cross_tenant_access_is_isolated(pool: sqlx::PgPool) {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let base = common::spawn_app(pool, jwks.verifier(Duration::from_secs(600))).await;
    let http = reqwest::Client::new();

    // 在 (TENANT, PROJECT_1) 建资源。
    let created: Value = http
        .post(format!("{base}/v1/artifacts"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .header("x-project-id", common::PROJECT_1.to_string())
        .json(&json!({"project_id": common::PROJECT_1, "name": "T1", "artifact_type": "visualization"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // 换成"另一租户"的 token（tenant_id/project_id 均不同），试图用其 scope 读取
    // T1 资源：租户恒取自凭证 → (T2,P2) 下无此资源 → 404（不泄露存在性）。
    let other = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({
            "tenant_id": common::TENANT_OTHER,
            "project_id": common::PROJECT_OTHER
        })),
    );
    let res = http
        .get(format!(
            "{base}/v1/artifacts/{}",
            created["id"].as_str().unwrap()
        ))
        .header("Authorization", format!("Bearer {other}"))
        .header("x-project-id", common::PROJECT_OTHER.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        404,
        "cross-tenant lookup must not leak existence"
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn s11_claim_project_header_mismatch_is_403(pool: sqlx::PgPool) {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let base = common::spawn_app(pool, jwks.verifier(Duration::from_secs(600))).await;
    let http = reqwest::Client::new();

    // token 绑定 PROJECT_1，请求却携带 PROJECT_2 的 X-Project-Id → require 403。
    let res = http
        .post(format!("{base}/v1/artifacts"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .header("x-project-id", common::PROJECT_2.to_string())
        .json(&json!({"project_id": common::PROJECT_2, "name": "Cross", "artifact_type": "visualization"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    let envelope: Value = res.json().await.unwrap();
    assert_eq!(envelope["error"]["code"], "SCOPE_DENIED");
}

#[sqlx::test(migrations = "../migrations")]
async fn s12_action_outside_token_scope_is_403(pool: sqlx::PgPool) {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let base = common::spawn_app(pool, jwks.verifier(Duration::from_secs(600))).await;
    let http = reqwest::Client::new();

    // 只读 token 尝试创建（artifact:create 不在 actions）。
    let read_only = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"actions": ["artifact:read"]})),
    );
    let res = http
        .post(format!("{base}/v1/artifacts"))
        .header("Authorization", format!("Bearer {read_only}"))
        .header("x-project-id", common::PROJECT_1.to_string())
        .json(&json!({"project_id": common::PROJECT_1, "name": "Escalate", "artifact_type": "visualization"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    let envelope: Value = res.json().await.unwrap();
    assert_eq!(envelope["error"]["code"], "SCOPE_DENIED");
}

#[sqlx::test(migrations = "../migrations")]
async fn s18_missing_project_header_is_400(pool: sqlx::PgPool) {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let base = common::spawn_app(pool, jwks.verifier(Duration::from_secs(600))).await;
    let http = reqwest::Client::new();
    let token = valid_token();

    // 先合法建一个资源（create 的 project 取自请求体，ART-01 语义）。
    let created: Value = http
        .post(format!("{base}/v1/artifacts"))
        .header("Authorization", format!("Bearer {token}"))
        .header("x-project-id", common::PROJECT_1.to_string())
        .json(&json!({"project_id": common::PROJECT_1, "name": "NoHeader", "artifact_type": "visualization"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // 读取路径（Scoped 提取器）缺 X-Project-Id → 400 INVALID_REQUEST。
    let res = http
        .get(format!(
            "{base}/v1/artifacts/{}",
            created["id"].as_str().unwrap()
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let envelope: Value = res.json().await.unwrap();
    assert_eq!(envelope["error"]["code"], "INVALID_REQUEST");
}

// ── 场景 13/14/15/17 + 19：jti 绑定存储（数据库原子性） ─────────────────

fn binding_use(jti: &str, caller: Option<Uuid>, hash: &str) -> DelegationTokenUse {
    DelegationTokenUse {
        issuer: "ai-pm".into(),
        jti: jti.to_string(),
        tenant_id: common::TENANT,
        project_id: common::PROJECT_1,
        caller_request_id: caller,
        request_hash: Some(hash.to_string()),
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn s13_same_jti_different_request_is_rejected(pool: sqlx::PgPool) {
    let store = PgDelegationBindingStore::new(pool.clone());
    let jti = Uuid::new_v4().to_string();
    let caller = Uuid::new_v4();
    let hash_a = common::sha256_hex("request-a");
    let hash_b = common::sha256_hex("request-b");

    assert_eq!(
        store
            .consume(binding_use(&jti, Some(caller), &hash_a))
            .await
            .unwrap(),
        DelegationBindingOutcome::Fresh
    );
    // 同 jti、不同指纹 → 409 族硬拒绝。
    let err = store
        .consume(binding_use(&jti, Some(caller), &hash_b))
        .await
        .expect_err("different request with same jti must be rejected");
    assert_eq!(err.code(), "DELEGATION_TOKEN_REUSED");
    // 同 jti、不同 caller_request_id → 拒绝。
    let err = store
        .consume(binding_use(&jti, Some(Uuid::new_v4()), &hash_a))
        .await
        .expect_err("different caller_request_id with same jti must be rejected");
    assert_eq!(err.code(), "DELEGATION_TOKEN_REUSED");
    // 同 jti、不同 scope（跨项目）→ 拒绝。
    let mut cross = binding_use(&jti, Some(caller), &hash_a);
    cross.project_id = common::PROJECT_2;
    let err = store
        .consume(cross)
        .await
        .expect_err("scope change with same jti must be rejected");
    assert_eq!(err.code(), "DELEGATION_TOKEN_REUSED");
}

#[sqlx::test(migrations = "../migrations")]
async fn s14_concurrent_replay_yields_single_fresh_binding(pool: sqlx::PgPool) {
    let store = Arc::new(PgDelegationBindingStore::new(pool.clone()));
    let jti = Uuid::new_v4().to_string();
    let caller = Uuid::new_v4();
    let hash = common::sha256_hex("same-request");
    let use_a = binding_use(&jti, Some(caller), &hash);
    let use_b = binding_use(&jti, Some(caller), &hash);

    // 与 as2_17 相同的模式：不假设 tokio::join! 的 poll 顺序。
    let (a, b) = tokio::join!(store.consume(use_a), store.consume(use_b));
    let outcomes = [a.unwrap(), b.unwrap()];
    let fresh = outcomes
        .iter()
        .filter(|o| **o == DelegationBindingOutcome::Fresh)
        .count();
    let retry = outcomes
        .iter()
        .filter(|o| **o == DelegationBindingOutcome::RetryAllowed)
        .count();
    assert_eq!(
        fresh, 1,
        "exactly one concurrent request wins the jti binding"
    );
    assert_eq!(
        retry, 1,
        "the loser must be classified as an identical retry, not an error"
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn s15_and_s19_retry_is_legal_and_binding_survives_restart(pool: sqlx::PgPool) {
    let jti = Uuid::new_v4().to_string();
    let caller = Uuid::new_v4();
    let hash = common::sha256_hex("retry-me");
    let use_of = binding_use(&jti, Some(caller), &hash);

    let first = PgDelegationBindingStore::new(pool.clone());
    assert_eq!(
        first.consume(use_of.clone()).await.unwrap(),
        DelegationBindingOutcome::Fresh
    );

    // "进程重启"：新的 store 实例（新连接），绑定仍可判定 → 合法重试。
    let second = PgDelegationBindingStore::new(pool.clone());
    assert_eq!(
        second.consume(use_of.clone()).await.unwrap(),
        DelegationBindingOutcome::RetryAllowed
    );
    let third = PgDelegationBindingStore::new(pool.clone());
    assert_eq!(
        third.consume(use_of).await.unwrap(),
        DelegationBindingOutcome::RetryAllowed
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn s17_revocation_semantics_ttl_window_and_binding_guard(pool: sqlx::PgPool) {
    // 契约 §5 撤销模型（Q1 默认裁决）：
    // (a) 用户被移出项目后，AI-PM 停止签发新 token —— AS 侧无法也无需感知；
    // (b) 已签发 token 在 TTL ≤ 300s 残余窗口内仍可用于已授权动作（接受的风险，
    //     审计可按 jti 追溯到签发事件）；这里验证该窗口确实存在且受控；
    // (c) 残余窗口内同一 jti 仍不能改绑其他请求（绑定守卫不随撤销失效）。
    let store = PgDelegationBindingStore::new(pool.clone());
    let jti = Uuid::new_v4().to_string();
    let caller = Uuid::new_v4();
    let hash = common::sha256_hex("pre-revocation-request");
    assert_eq!(
        store
            .consume(binding_use(&jti, Some(caller), &hash))
            .await
            .unwrap(),
        DelegationBindingOutcome::Fresh,
        "token minted before revocation stays usable within its TTL"
    );
    let mut redirected = binding_use(&jti, Some(caller), &hash);
    redirected.project_id = common::PROJECT_2;
    let err = store
        .consume(redirected)
        .await
        .expect_err("post-revocation redirect of the same jti must stay rejected");
    assert_eq!(err.code(), "DELEGATION_TOKEN_REUSED");
}

#[sqlx::test(migrations = "../migrations")]
async fn s20_db_level_binding_invariants(pool: sqlx::PgPool) {
    // DB 层兜底（不依赖 Rust）：唯一 jti + 配对 CHECK。
    let jti = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO artifact.delegation_token_use \
         (issuer, jti, tenant_id, project_id, caller_request_id, request_hash) \
         VALUES ('ai-pm', $1, $2, $3, $4, $5)",
    )
    .bind(&jti)
    .bind(common::TENANT)
    .bind(common::PROJECT_1)
    .bind(Uuid::new_v4())
    .bind(common::sha256_hex("x"))
    .execute(&pool)
    .await
    .unwrap();

    // 重复 jti → 23505。
    let dup = sqlx::query(
        "INSERT INTO artifact.delegation_token_use \
         (issuer, jti, tenant_id, project_id, caller_request_id, request_hash) \
         VALUES ('ai-pm', $1, $2, $3, $4, $5)",
    )
    .bind(&jti)
    .bind(common::TENANT)
    .bind(common::PROJECT_1)
    .bind(Uuid::new_v4())
    .bind(common::sha256_hex("x"))
    .execute(&pool)
    .await;
    assert_eq!(
        common::pg_error_code(&dup.unwrap_err()).as_deref(),
        Some("23505")
    );

    // 半配对（caller 非空、hash 为空）→ 23514。
    let half = sqlx::query(
        "INSERT INTO artifact.delegation_token_use \
         (issuer, jti, tenant_id, project_id, caller_request_id, request_hash) \
         VALUES ('ai-pm', $1, $2, $3, $4, NULL)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(common::TENANT)
    .bind(common::PROJECT_1)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await;
    assert_eq!(
        common::pg_error_code(&half.unwrap_err()).as_deref(),
        Some("23514")
    );

    // 大写十六进制 hash → 23514。
    let upper = sqlx::query(
        "INSERT INTO artifact.delegation_token_use \
         (issuer, jti, tenant_id, project_id, caller_request_id, request_hash) \
         VALUES ('ai-pm', $1, $2, $3, $4, $5)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(common::TENANT)
    .bind(common::PROJECT_1)
    .bind(Uuid::new_v4())
    .bind("A".repeat(64))
    .execute(&pool)
    .await;
    assert_eq!(
        common::pg_error_code(&upper.unwrap_err()).as_deref(),
        Some("23514")
    );

    // 清理函数窗口护栏：拒绝清理过近的行。
    let guard = sqlx::query("SELECT artifact.purge_delegation_token_use(now())")
        .execute(&pool)
        .await;
    assert!(
        common::pg_error_message(&guard.unwrap_err()).contains("PURGE_WINDOW_TOO_RECENT"),
        "purge guard must reject too-recent windows"
    );
}

// ── 配置缺省：无受信 issuer = 维持全拒（fail-closed 生产姿态） ───────────

#[sqlx::test(migrations = "../migrations")]
async fn s21_empty_issuer_config_keeps_default_deny(pool: sqlx::PgPool) {
    let base = common::spawn_app_closed(pool).await;
    let http = reqwest::Client::new();
    // /v1/generators 是公开注册表端点；用受保护的业务端点验证全拒。
    let res = http
        .post(format!("{base}/v1/artifacts"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .header("x-project-id", common::PROJECT_1.to_string())
        .json(&json!({"project_id": common::PROJECT_1, "name": "Denied", "artifact_type": "visualization"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401, "closed verifier must reject every token");
}

// ── JWKS 缓存不可用单元测试补充（Unavailable 判定与限频不变量） ─────────

#[tokio::test]
async fn s06b_jwks_cache_unavailable_lookup_fails_closed() {
    let http = reqwest::Client::builder().build().unwrap();
    let cache = JwksCache::with_policy(http, Duration::from_secs(300), Duration::from_secs(900));
    let lookup = cache
        .lookup("http://127.0.0.1:9/jwks.json", KID_PRIMARY)
        .await;
    assert!(matches!(lookup, JwksLookup::Unavailable(_)));
}

// ── 测试替身回归：新增 Delegation 字段不破坏既有装配 ────────────────────

#[tokio::test]
async fn s22_test_double_verifier_still_fresh_after_field_extension() {
    let mut verifier = TestDelegationVerifier::new();
    verifier.allow(
        "tok",
        common::TENANT,
        common::PROJECT_1,
        "user-1",
        artifact_domain::ActorType::User,
        common::read_actions(),
    );
    let delegation = verifier.verify("tok").await.unwrap();
    assert_eq!(delegation.lmms_tid.as_deref(), None);
    assert!(delegation.artifact_id.is_none());
    assert!(delegation.operation_id.is_none());
    assert!(delegation.caller_request_id.is_none());
    assert!(delegation.request_hash.is_none());
}

// ── issers 辅助已内联；测试替身回归断言见 s22 ──────────────────────────
