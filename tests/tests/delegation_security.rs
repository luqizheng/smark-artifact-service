//! WP-LMMS-DELEGATION-01/03 安全测试矩阵（契约 v0.2 §3 + WP-03 修订）。
//!
//! 覆盖任务书 18 场景（以负面为主）+ WP-03 新增负面矩阵：
//!   1  正常 Token（E2E 创建/读取）
//!   2  错误签名                10 跨租户访问
//!   3  错误 issuer             11 跨项目访问（claim vs 头不一致）
//!   4  错误 audience           12 Action 越权
//!   5  未知 kid（含限频刷新）  16 请求指纹篡改
//!   6  JWKS 不可用 → 503       18 缺失 X-Project-Id
//!   7  Token 过期
//!   8  未来 iat                WP-03（R2 评审缺口修复）：
//!   9  超长 TTL                * V4 数组 aud / V6 jti 非 UUID / V7 purpose /
//!                                 V8 actions 形状（原 PoC P1–P7 移植，R2-F-12）
//!                              * PC-O5-1：写动作 token 缺 caller_request_id 拒绝
//!                              * 三个读 handler 动作授权（version:read /
//!                                artifact:read，R2-F-03）
//!                              * /v1/generators 认证（api_meta 套件）
//!
//! 注：O-5 绑定表已按 WP-03 裁决移除（迁移 0006 + PgDelegationBindingStore），
//! 原 s13/s14/s15/s17/s19/s20 绑定存储场景随之删除；同 jti 并发仲裁职责由
//! 既有业务幂等机制承接（AS-2 caller 唯一索引回归见 as2_caller_identity.rs）。
//!
//! 测试密钥在运行期用 `rsa` 生成（无任何真实凭据落盘/入 git）；JWKS 服务器
//! 为进程内 axum 回环服务。生产配置仍要求 https JWKS（config 单测覆盖）。

mod common;

use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

use artifact_application::{
    DelegationVerifier, assert_delegation_matches_request, canonical_request_fingerprint,
};
use artifact_infrastructure::delegation::jwks::{JwksCache, JwksLookup};
use artifact_infrastructure::delegation::{AiPmDelegationVerifier, TrustedIssuer};
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

/// 契约 v0.2 §3.1 的最小合法 claims（WP-03：含写动作 → 必须携带
/// caller_request_id，PC-O5-1 红线）。
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
        "caller_request_id": Uuid::new_v4().to_string(),
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

// ── WP-03（R2-F-12）：V4/V6/V7/V8 验证器级负面测试（原 R2 PoC P1–P7 移植） ──

#[tokio::test]
async fn v4_array_audience_even_containing_expected_is_rejected() {
    // PoC P7：aud 为数组（即使包含 artifact-service）→ 拒绝。claims 层
    // `aud: String` 反序列化即失败，故拒绝消息是解析类错误（"claims rejected"）；
    // 断言要点是"包含期望值的数组 aud 也不得通过"。
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let token = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        json!({
            "aud": ["artifact-service", "other"],
            "iss": "ai-pm",
            "sub": "user-1",
            "purpose": "delegation",
            "tenant_id": common::TENANT,
            "project_id": common::PROJECT_1,
            "actions": ["artifact:read"],
            "iat": chrono::Utc::now().timestamp(),
            "exp": chrono::Utc::now().timestamp() + 120,
            "jti": Uuid::new_v4().to_string(),
            "delegation_id": Uuid::new_v4().to_string(),
        }),
    );
    rejects_with(&jwks, &token, "claims rejected").await;
}

#[tokio::test]
async fn v6_jti_must_be_uuid() {
    // PoC P2：jti 非 UUID → 拒绝（撤销查询主键类型边界）。
    // 纯读动作，避免写绑定红线先于 V6 判定（保持单项失败语义）。
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let token = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"actions": ["artifact:read"], "jti": "not-a-uuid"})),
    );
    rejects_with(&jwks, &token, "jti").await;
}

#[tokio::test]
async fn v7_purpose_must_be_delegation() {
    // PoC P1 / P1b：LMMS 用户 Token 形态（purpose=user_login）与缺失 purpose
    // 一律拒绝（类型混淆双闸之二）。缺失 purpose 属必填字段反序列化失败，
    // 拒绝消息为解析类错误（deny-by-absence）。
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let user_login = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"purpose": "user_login"})),
    );
    rejects_with(&jwks, &user_login, "purpose").await;

    let mut missing = base_claims();
    missing
        .as_object_mut()
        .expect("claims object")
        .remove("purpose")
        .expect("purpose present in base claims");
    let missing = mint(&KEY_PRIMARY, KID_PRIMARY, missing);
    rejects_with(&jwks, &missing, "claims rejected").await;
}

#[tokio::test]
async fn v8_actions_shape_rejections() {
    // PoC P3–P6：通配、未知动作、重复条目、空数组 → 全部拒绝。
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;

    let wildcard = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"actions": ["*"]})),
    );
    rejects_with(&jwks, &wildcard, "unknown action").await;

    let unknown = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"actions": ["artifact:purge_all"]})),
    );
    rejects_with(&jwks, &unknown, "unknown action").await;

    let duplicate = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"actions": ["artifact:read", "artifact:read"]})),
    );
    rejects_with(&jwks, &duplicate, "duplicate action").await;

    let empty = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"actions": []})),
    );
    rejects_with(&jwks, &empty, "empty").await;
}

// ── WP-03（PC-O5-1 红线）：写动作 token 必须携带 caller_request_id ────────

#[tokio::test]
async fn write_action_token_without_caller_request_id_is_rejected() {
    // O-5 移除的强制前置条件：授予写动作的 delegation 缺顶层 caller_request_id
    // → 验证器拒绝（缺绑定即拒绝，不依赖签发方自律）。显式置 null 覆盖
    // base_claims 的合法 caller。
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let token = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"actions": ["artifact:create"], "caller_request_id": null})),
    );
    rejects_with(&jwks, &token, "caller_request_id").await;

    // 混合读+写动作同样受红线约束（写能力存在即要求绑定）。
    let mixed = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(
            json!({"actions": ["artifact:read", "artifact:create"], "caller_request_id": null}),
        ),
    );
    rejects_with(&jwks, &mixed, "caller_request_id").await;

    // 纯读动作 token 无 caller_request_id 仍然合法（读路径不要求绑定）。
    let read_only = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"actions": ["artifact:read", "version:read"]})),
    );
    let verifier = jwks.verifier(Duration::from_secs(600));
    assert!(
        verifier.verify(&read_only).await.is_ok(),
        "read-only token without operation binding must verify"
    );

    // 写动作 + caller_request_id → 合法。
    let bound_write = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({
            "actions": ["artifact:create"],
            "caller_request_id": Uuid::new_v4().to_string(),
        })),
    );
    assert!(
        verifier.verify(&bound_write).await.is_ok(),
        "write token carrying caller_request_id must verify"
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn write_without_binding_is_rejected_over_http(pool: sqlx::PgPool) {
    // HTTP 层证据：写动作但无 caller_request_id 的 token 走真实 verifier
    // （RS256 + 回环 JWKS）→ 401，任何写路径不可达。
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let base = common::spawn_app(pool, jwks.verifier(Duration::from_secs(600))).await;
    let http = reqwest::Client::new();
    let token = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"actions": ["artifact:create"], "caller_request_id": null})),
    );
    let res = http
        .post(format!("{base}/v1/artifacts"))
        .header("Authorization", format!("Bearer {token}"))
        .header("x-project-id", common::PROJECT_1.to_string())
        .json(&json!({
            "project_id": common::PROJECT_1,
            "name": "UnboundWrite",
            "artifact_type": "visualization"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401, "unbound write token must fail closed");
    let envelope: Value = res.json().await.unwrap();
    let message = envelope["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("caller_request_id"),
        "rejection must cite the missing operation binding: {message}"
    );
}

// ── WP-03（R2-F-03）：三个读 handler 的动作授权（Q3：GET 也要 delegation） ──

#[sqlx::test(migrations = "../migrations")]
async fn version_reads_require_version_read_action(pool: sqlx::PgPool) {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let base = common::spawn_app(pool.clone(), jwks.verifier(Duration::from_secs(600))).await;
    let http = reqwest::Client::new();

    // 布景：artifact + version 直接落库（独立测试库）。
    let artifact_id =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "R2F03").await;
    let version_id = common::sql_insert_version(
        &pool,
        common::TENANT,
        common::PROJECT_1,
        artifact_id,
        "pending",
    )
    .await;

    // ① 无 version:read 动作（artifact:read）→ 两个版本读端点 403，SCOPE_DENIED，
    //    拒绝发生在任何数据读取之前。
    let no_version = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"actions": ["artifact:read"]})),
    );
    for url in [
        format!("{base}/v1/artifacts/{artifact_id}/versions/{version_id}"),
        format!("{base}/v1/artifacts/{artifact_id}/versions"),
    ] {
        let res = http
            .get(&url)
            .header("Authorization", format!("Bearer {no_version}"))
            .header("x-project-id", common::PROJECT_1.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 403, "GET {url} must require version:read");
        let envelope: Value = res.json().await.unwrap();
        assert_eq!(envelope["error"]["code"], "SCOPE_DENIED");
    }

    // ② 合法 version:read → 200。
    let with_version = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({
            "actions": ["version:read"],
            "caller_request_id": Uuid::new_v4().to_string(),
        })),
    );
    let ok = http
        .get(format!(
            "{base}/v1/artifacts/{artifact_id}/versions/{version_id}"
        ))
        .header("Authorization", format!("Bearer {with_version}"))
        .header("x-project-id", common::PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200, "version:read token must read version");
    let ok_list = http
        .get(format!("{base}/v1/artifacts/{artifact_id}/versions"))
        .header("Authorization", format!("Bearer {with_version}"))
        .header("x-project-id", common::PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(
        ok_list.status(),
        200,
        "version:read token must list versions"
    );

    // ③ 无 delegation → 401（Q3：GET 不豁免）。
    let anon = http
        .get(format!("{base}/v1/artifacts/{artifact_id}/versions"))
        .header("x-project-id", common::PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);

    // ④ 跨项目（header PROJECT_2，token 绑定 PROJECT_1）→ 403，不泄露数据。
    let cross = http
        .get(format!("{base}/v1/artifacts/{artifact_id}/versions"))
        .header("Authorization", format!("Bearer {with_version}"))
        .header("x-project-id", common::PROJECT_2.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(cross.status(), 403, "cross-project read must be denied");
}

#[sqlx::test(migrations = "../migrations")]
async fn reference_list_requires_artifact_read_action(pool: sqlx::PgPool) {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY_PRIMARY, KID_PRIMARY)]).await;
    let base = common::spawn_app(pool.clone(), jwks.verifier(Duration::from_secs(600))).await;
    let http = reqwest::Client::new();

    let artifact_id =
        common::sql_insert_artifact(&pool, common::TENANT, common::PROJECT_1, "R2F03Ref").await;

    // 无 artifact:read → 403（references 无独立 read 动作，映射 artifact:read）。
    let no_read = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(
            json!({"actions": ["version:read"], "caller_request_id": Uuid::new_v4().to_string()}),
        ),
    );
    let res = http
        .get(format!("{base}/v1/artifacts/{artifact_id}/references"))
        .header("Authorization", format!("Bearer {no_read}"))
        .header("x-project-id", common::PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        403,
        "list_references must require artifact:read"
    );
    let envelope: Value = res.json().await.unwrap();
    assert_eq!(envelope["error"]["code"], "SCOPE_DENIED");

    // 合法 artifact:read → 200（空列表）。
    let with_read = mint(
        &KEY_PRIMARY,
        KID_PRIMARY,
        claims_with(json!({"actions": ["artifact:read"]})),
    );
    let ok = http
        .get(format!("{base}/v1/artifacts/{artifact_id}/references"))
        .header("Authorization", format!("Bearer {with_read}"))
        .header("x-project-id", common::PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let body: Value = ok.json().await.unwrap();
    assert_eq!(body["items"].as_array().map(Vec::len), Some(0));

    // 无 delegation → 401。
    let anon = http
        .get(format!("{base}/v1/artifacts/{artifact_id}/references"))
        .header("x-project-id", common::PROJECT_1.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
}

// ── 配置缺省：无受信 issuer = 维持全拒（fail-closed 生产姿态） ───────────

#[sqlx::test(migrations = "../migrations")]
async fn s21_empty_issuer_config_keeps_default_deny(pool: sqlx::PgPool) {
    let base = common::spawn_app_closed(pool).await;
    let http = reqwest::Client::new();
    // 业务写端点全拒。
    let res = http
        .post(format!("{base}/v1/artifacts"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .header("x-project-id", common::PROJECT_1.to_string())
        .json(&json!({"project_id": common::PROJECT_1, "name": "Denied", "artifact_type": "visualization"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401, "closed verifier must reject every token");
    // WP-03：/v1/generators 不再豁免——fail-closed 姿态同样覆盖该端点。
    let generators = http
        .get(format!("{base}/v1/generators"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    assert_eq!(generators.status(), 401);
    let anon = http
        .get(format!("{base}/v1/generators"))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
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
