//! ⚠️ 非生产实现 — WP-LMMS-DELEGATION-02 独立评审 PoC（R2 测试有效性复核）⚠️
//!
//! 对交付测试矩阵（tests/tests/delegation_security.rs）**未覆盖**的验证器
//! 负面路径逐项独立验证。全部通过 = 代码层防御存在（与本次评审源码审查结论
//! 一致）；任何失败 = 评审 FINDINGS 升级。
//!
//! 场景：
//!   P1  V7  purpose=user_login（LMMS 用户 Token 形态混淆）→ 必须拒绝
//!   P2  V6  jti 非 UUID → 必须拒绝
//!   P3  V8  actions 含通配 "*" → 必须拒绝
//!   P4  V8  actions 含未知动作 → 必须拒绝
//!   P5  V8  actions 重复条目 → 必须拒绝
//!   P6  V8  actions 空数组 → 必须拒绝
//!   P7  V4  aud 为数组（含期望值）→ 必须拒绝（契约"精确单值，禁止数组"）
//!   P8  配置弱化演示：audience 配置为 "coder-micro-services" 时，
//!       aud=coder-micro-services 的 delegation token **被接受** —— 证明
//!       契约固定受众仅由默认值保证（R2-F-06 证据）。
//!   P9  正常对照：合法 delegation token 被接受（PoC 自身有效性）。

use std::sync::{Arc, LazyLock};

use axum::routing::get;
use base64::Engine as _;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use rand::rngs::OsRng;
use rsa::RsaPrivateKey;
use rsa::pkcs1::{EncodeRsaPrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use serde_json::{Value, json};
use uuid::Uuid;

use artifact_application::DelegationVerifier;
use artifact_infrastructure::delegation::{AiPmDelegationVerifier, TrustedIssuer};

static KEY: LazyLock<RsaPrivateKey> = LazyLock::new(|| {
    RsaPrivateKey::new(&mut OsRng, 2048).expect("test rsa keygen")
});
const KID: &str = "poc-key-1";

fn jwk_entry(key: &RsaPrivateKey, kid: &str) -> Value {
    let n = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.n().to_bytes_be());
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.e().to_bytes_be());
    json!({"kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid, "n": n, "e": e})
}

/// 极简 JWKS 回环服务（仅评审 PoC）。
async fn spawn_jwks(initial: Vec<Value>) -> String {
    let snapshot = Arc::new(initial);
    let app = axum::Router::new().route(
        "/jwks.json",
        get(move || {
            let snapshot = Arc::clone(&snapshot);
            async move { axum::Json(json!({ "keys": *snapshot })) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.expect("jwks") });
    format!("http://{addr}/jwks.json")
}

fn mint(claims: Value) -> String {
    let header = Header {
        alg: Algorithm::RS256,
        kid: Some(KID.to_string()),
        ..Header::default()
    };
    let pem = KEY
        .to_pkcs1_pem(LineEnding::LF)
        .expect("pem")
        .to_string();
    let enc = EncodingKey::from_rsa_pem(pem.as_bytes()).expect("encoding key");
    encode(&header, &claims, &enc).expect("mint")
}

fn base_claims() -> Value {
    let now = chrono::Utc::now().timestamp();
    json!({
        "iss": "ai-pm",
        "sub": "user-1",
        "aud": "artifact-service",
        "purpose": "delegation",
        "tenant_id": Uuid::from_u128(0x11).to_string(),
        "project_id": Uuid::from_u128(0x22).to_string(),
        "actions": ["artifact:read"],
        "iat": now,
        "exp": now + 120,
        "jti": Uuid::new_v4().to_string(),
        "delegation_id": Uuid::from_u128(0x33).to_string(),
    })
}

fn claims_with(overrides: Value) -> Value {
    let mut base = base_claims();
    let overrides = overrides.as_object().expect("obj").clone();
    for (k, v) in overrides {
        base[k] = v;
    }
    base
}

async fn verifier_for(jwks_url: &str, audience: &str) -> Arc<AiPmDelegationVerifier> {
    let mut issuer = TrustedIssuer::new("ai-pm", jwks_url);
    // TrustedIssuer.audience 是 pub 字段：模拟运维通过配置注入非契约受众。
    issuer.audience = audience.to_string();
    Arc::new(AiPmDelegationVerifier::new(vec![issuer]).expect("verifier"))
}

async fn expect_reject(verifier: &AiPmDelegationVerifier, token: &str, label: &str) {
    if verifier.verify(token).await.is_ok() {
        panic!("✗ {label}: ACCEPTED — R2 FINDING escalation required");
    }
    println!("✓ {label}: rejected");
}

#[tokio::test]
async fn poc_negative_paths_and_audience_config_weakening() {
    let jwks = spawn_jwks(vec![jwk_entry(&KEY, KID)]).await;
    let verifier = verifier_for(&jwks, "artifact-service").await;

    // P9 对照：合法 token 必须被接受（证明 PoC 链路有效）。
    let ok = verifier.verify(&mint(base_claims())).await;
    assert!(ok.is_ok(), "P9 control: valid delegation must verify");
    println!("✓ P9 control: valid delegation accepted");

    // P1 V7：LMMS 用户 Token 形态（purpose=user_login）→ 混淆必须被拒绝。
    expect_reject(
        &verifier,
        &mint(claims_with(json!({"purpose": "user_login"}))),
        "P1 V7 purpose=user_login (LMMS token shape confusion)",
    )
    .await;
    // P1b：purpose 缺失。
    let mut no_purpose = base_claims();
    no_purpose
        .as_object_mut()
        .unwrap()
        .remove("purpose");
    expect_reject(&verifier, &mint(no_purpose), "P1b V7 purpose missing").await;

    // P2 V6：jti 非 UUID。
    expect_reject(
        &verifier,
        &mint(claims_with(json!({"jti": "not-a-uuid"}))),
        "P2 V6 jti non-UUID",
    )
    .await;

    // P3-P6 V8：actions 形状。
    expect_reject(
        &verifier,
        &mint(claims_with(json!({"actions": ["*"]}))),
        "P3 V8 wildcard action",
    )
    .await;
    expect_reject(
        &verifier,
        &mint(claims_with(json!({"actions": ["artifact:read", "artifact:read"]}))),
        "P5 V8 duplicate actions",
    )
    .await;
    expect_reject(
        &verifier,
        &mint(claims_with(json!({"actions": []}))),
        "P6 V8 empty actions",
    )
    .await;
    expect_reject(
        &verifier,
        &mint(claims_with(json!({"actions": ["artifact:read", "unknown:action"]}))),
        "P4 V8 unknown action",
    )
    .await;

    // P7 V4：数组型 aud（含期望值）——契约要求"精确单值，禁止数组"。
    expect_reject(
        &verifier,
        &mint(claims_with(json!({"aud": ["artifact-service"]}))),
        "P7 V4 array-typed aud",
    )
    .await;

    // P8 配置弱化演示：audience 配置为 coder-micro-services 时，
    // aud=coder-micro-services + purpose=delegation 的 token 被接受。
    let misconf = verifier_for(&jwks, "coder-micro-services").await;
    let token = mint(claims_with(json!({"aud": "coder-micro-services"})));
    match misconf.verify(&token).await {
        Ok(_) => println!(
            "✓ P8 audience misconfiguration ACCEPTED (demonstrates R2-F-06: \
             contract-fixed audience is default-only, not enforced)"
        ),
        Err(_) => panic!(
            "P8: audience misconfiguration was rejected — R2-F-06 must be \
             re-graded (weakening stronger than assessed)"
        ),
    }
}
