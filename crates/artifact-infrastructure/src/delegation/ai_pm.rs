//! AI-PM 委托断言 Token 验证器（WP 契约 v0.2 §3，候选 B）。
//!
//! 实现契约 §3.2 的全部验证步骤（任何一步失败即拒绝，绝不降级）：
//! 1. RS256 验签（kid-strict，JWKS 缓存见 [`jwks`]；`alg` 必须恰为 RS256）；
//! 2. `iss` ∈ 受信 issuer 清单（按**已验证后**的 iss 匹配配置；peek 只用于
//!    选配置，iss 本身由签名验证强制）且 `aud == DELEGATION_AUDIENCE` 常量
//!    （`artifact-service`，编译期固定，配置不可覆盖——WP-03 裁决）；
//! 3. 时间验证（P0-04，见 [`time`] 模块内常量与 `validate_time_claims`）；
//! 4. `purpose == "delegation"`；`actions` 非空且逐项 ∈ AS Action 枚举；
//!    租户映射按 P0-01：`tenant_id`（AS 原生 UUID）+ `tid`（LMMS 数字镜像）
//!    双必填，映射的唯一权威是 AI-PM 租户注册表，AS 不做任何换算；
//! 5. 组装 [`Delegation`]：scope = (tenant_id, project_id) 均来自凭证；
//!    `X-Project-Id` 一致性由 Scoped 提取器 + `Delegation::require` 校验；
//! 6. `require(action, scope)` 仍由服务层入口执行（现状不变）；
//! 7. jti 单次消费仲裁由既有业务幂等机制承接（AS-1 artifact_key/Idempotency、
//!    AS-2 caller 三元组部分唯一索引，WP-03 O-5 裁决：AS 不加表）。
//!
//! 错误消息只含判定原因，绝不包含 token 本体（SECURITY-REVIEW 挑战 10）。

use artifact_application::{DelegationVerifier, VerificationError};
use artifact_domain::{Action, ActorType, Delegation, TenantScope};
use async_trait::async_trait;
use base64::Engine as _;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use uuid::Uuid;

use super::jwks::{JwksCache, JwksLookup};

/// 契约 v0.2 §3.1：委托 Token 固定受众（WP-03：常量钉死，无配置面）。
pub const DELEGATION_AUDIENCE: &str = "artifact-service";
/// 契约 v0.2 §3.1：purpose 固定值。
pub const DELEGATION_PURPOSE: &str = "delegation";
/// P0-04：最大 Token TTL（秒）。
pub const MAX_TOKEN_TTL_SECS: i64 = 300;
/// P0-04：时钟偏差容忍（秒），同时约束 iat 未来偏差与 exp/nbf 判定。
pub const CLOCK_SKEW_LEEWAY_SECS: i64 = 60;

/// 受信签发方配置（来自 `ARTIFACT_API_DELEGATION_ISSUERS`，见 api config）。
///
/// 信任边界（WP-03 audience 固定裁决）：期望受众**不是**本结构的字段——
/// 验证器内部恒以 [`DELEGATION_AUDIENCE`] 常量比对，配置层无法改变受众
/// 信任根（R2-F-06 修复；构造后不存在可被外部输入改变受众的路径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedIssuer {
    /// `iss` claim 的精确值（如 `ai-pm`）。受信 issuer 清单机制（Q6）按设计
    /// 可配置，且恰一项 active（受信根本体；受众是契约常量，两者边界不同）。
    pub issuer: String,
    /// 固定 JWKS URL（https；回环 http 豁免仅在 dev 配置解析层放行）。
    pub jwks_url: String,
}

impl TrustedIssuer {
    pub fn new(issuer: impl Into<String>, jwks_url: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            jwks_url: jwks_url.into(),
        }
    }
}

/// 契约 v0.2 §3.1.1 claims（不使用 deny_unknown_fields：向前兼容要求未知
/// claim 不致命；但下述必填字段缺一或类型不符即反序列化失败 → 拒绝）。
/// `iss`/`aud` 的授权判定由 jsonwebtoken + trusted 配置强制，字段保留以
/// 维持类型严格性；`artifact_id`/`operation_id`/`act.sub` 为契约操作绑定
/// 字段，AS-4 接线的目标一致性校验（V10）消费前仅做形状验证。
#[allow(dead_code)] // iss/aud/artifact_id/operation_id/act 保留形状校验，V10 接线消费
#[derive(Debug, Deserialize)]
struct DelegationClaims {
    iss: String,
    sub: String,
    aud: String,
    purpose: String,
    /// P0-01：AI-PM 内部租户 UUID（授权作用域唯一来源）。
    tenant_id: Uuid,
    /// 仅来源追溯：LMMS 数值 tid 的字符串形式；**不得用于授权判定**。
    #[serde(default)]
    lmms_tid: Option<String>,
    /// 委托目标项目（AI-PM DB 验证过的成员关系）。
    project_id: Uuid,
    /// 操作绑定（V10）：限定单 Artifact。
    #[serde(default)]
    artifact_id: Option<Uuid>,
    /// 操作绑定（V10）：稳定操作标识。
    #[serde(default)]
    operation_id: Option<String>,
    /// 操作绑定：AS-2 caller_request_id（AS-2 类型为 uuid，非 UUID 拒绝）。
    #[serde(default)]
    caller_request_id: Option<String>,
    actions: Vec<String>,
    iat: i64,
    exp: i64,
    #[serde(default)]
    nbf: Option<i64>,
    /// V6：必须为合法 UUID。
    jti: String,
    /// 委托记录 ID 必填（uuid 形式）。
    delegation_id: Uuid,
    #[serde(default)]
    actor_type: Option<String>,
    #[serde(default)]
    act: Option<ActClaim>,
    /// 扩展字段：`task_run_id` 透传；`request_hash` 为本实现提供的
    /// 请求指纹扩展（超出契约，见 PR 说明与遗留开放点 O-5）。
    #[serde(default)]
    biz: Option<BizClaim>,
}

/// 委托链（谁代表谁）。当前仅做形状验证，审计展示属 AS-4 接线范畴。
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ActClaim {
    #[serde(default)]
    sub: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BizClaim {
    /// 透传字段：与 AS-2 durable caller identity 关联，审计接线消费。
    #[allow(dead_code)]
    #[serde(default)]
    task_run_id: Option<String>,
    /// 扩展（O-6，WP-03 修订后保留）：签发方对规范化请求的 SHA-256 指纹；
    /// 与顶层 `caller_request_id` 配对存在（契约 §3.1.1 配对红线）。
    /// AS-4/V10 接线前仅做形状与配对校验，不作为放行依据。
    #[serde(default)]
    request_hash: Option<String>,
    /// 指纹算法版本（PC-O6-2）：缺省 = `v1`；存在时仅接受
    /// [`artifact_application::REQUEST_HASH_ALGORITHM_V1`]，未知算法拒绝
    /// （fail-closed：新算法上线必须先扩展本验证器）。
    #[serde(default)]
    request_hash_alg: Option<String>,
}

/// P1：ActorType 确定性映射。缺省 = user；白名单外拒绝。
fn map_actor_type(raw: Option<&str>) -> Result<ActorType, VerificationError> {
    match raw {
        None => Ok(ActorType::User),
        Some("user") => Ok(ActorType::User),
        Some("agent") => Ok(ActorType::Agent),
        Some("service") => Ok(ActorType::Service),
        Some(other) => Err(VerificationError::Unauthenticated(format!(
            "unknown actor_type '{other}' (expected user|agent|service)"
        ))),
    }
}

/// P0-04 时间规则（jsonwebtoken 已按 leeway 验证 exp/nbf 与 now 的关系；
/// 这里补齐它不覆盖的确定性规则）：
/// * `exp > iat`（严格）；
/// * `exp - iat ≤ MAX_TOKEN_TTL_SECS`（防自签长票）；
/// * `iat ≤ now + CLOCK_SKEW_LEEWAY_SECS`（未来 iat 超偏差拒绝）；
/// * `nbf`（若存在）`≤ exp`（jsonp 六步之外的不变量）。
fn validate_time_claims(claims: &DelegationClaims, now: i64) -> Result<(), VerificationError> {
    if claims.exp <= claims.iat {
        return Err(VerificationError::Unauthenticated(
            "token exp must be greater than iat".into(),
        ));
    }
    let ttl = claims.exp - claims.iat;
    if ttl > MAX_TOKEN_TTL_SECS {
        return Err(VerificationError::Unauthenticated(format!(
            "token ttl {ttl}s exceeds the {MAX_TOKEN_TTL_SECS}s maximum"
        )));
    }
    if claims.iat > now + CLOCK_SKEW_LEEWAY_SECS {
        return Err(VerificationError::Unauthenticated(
            "token iat is too far in the future".into(),
        ));
    }
    if let Some(nbf) = claims.nbf
        && nbf > claims.exp
    {
        return Err(VerificationError::Unauthenticated(
            "token nbf must not be after exp".into(),
        ));
    }
    Ok(())
}

/// 操作绑定形状校验（契约 §3.1.1 + WP-03 修订 PC-O6-1/2）：
/// * 顶层 `caller_request_id`（存在时）必须是合法 UUID（AS-2 类型要求）；
/// * 扩展 `biz.request_hash` 存在时：必须与顶层 `caller_request_id` 配对，
///   且为 64 位小写十六进制（规范化请求指纹，冻结规范见
///   `artifact_application::canonical_request_fingerprint`，算法 v1）；
/// * `biz.request_hash_alg`（存在时）必须为受支持的算法版本（当前仅 `v1`）；
///   alg 与 hash 同空同非空（半配对视为形状异常，fail-closed）。
fn validate_operation_binding(claims: &DelegationClaims) -> Result<(), VerificationError> {
    // 顶层 caller_request_id 存在时必须是合法 UUID（AS-2 caller identity 类型）。
    if let Some(raw) = claims.caller_request_id.as_deref()
        && raw.parse::<Uuid>().is_err()
    {
        return Err(VerificationError::Unauthenticated(
            "caller_request_id must be a valid UUID (AS-2 caller identity type)".into(),
        ));
    }
    let Some(biz) = claims.biz.as_ref() else {
        return Ok(());
    };
    if let Some(hash) = biz.request_hash.as_deref() {
        if claims.caller_request_id.is_none() {
            return Err(VerificationError::Unauthenticated(
                "biz.request_hash without top-level caller_request_id (pairing required)".into(),
            ));
        }
        let valid = hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        if !valid {
            return Err(VerificationError::Unauthenticated(
                "biz.request_hash must be lowercase 64-hex sha256".into(),
            ));
        }
    }
    if let Some(alg) = biz.request_hash_alg.as_deref() {
        if biz.request_hash.is_none() {
            return Err(VerificationError::Unauthenticated(
                "biz.request_hash_alg without biz.request_hash (pairing required)".into(),
            ));
        }
        if alg != artifact_application::REQUEST_HASH_ALGORITHM_V1 {
            return Err(VerificationError::Unauthenticated(format!(
                "unsupported biz.request_hash_alg '{alg}' (this build supports only '{}')",
                artifact_application::REQUEST_HASH_ALGORITHM_V1
            )));
        }
    }
    Ok(())
}

/// 从未验证的 token peek `iss`（仅用于选择受信配置；iss 的真实性由
/// RS256 验签 + `set_issuer` 强制）。
fn peek_issuer(token: &str) -> Result<String, VerificationError> {
    let mut parts = token.splitn(3, '.');
    let (_header, payload, _sig) = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s)) if !h.is_empty() && !p.is_empty() && !s.is_empty() => (h, p, s),
        _ => {
            return Err(VerificationError::Unauthenticated(
                "malformed bearer token".into(),
            ));
        }
    };
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| VerificationError::Unauthenticated("malformed token payload".into()))?;
    let value: serde_json::Value = serde_json::from_slice(&decoded)
        .map_err(|_| VerificationError::Unauthenticated("token payload is not JSON".into()))?;
    let iss = value
        .get("iss")
        .and_then(|v| v.as_str())
        .ok_or_else(|| VerificationError::Unauthenticated("missing iss claim".into()))?
        .to_string();
    Ok(iss)
}

/// 候选 B 生产验证器：AI-PM 签发、AS 以 AI-PM JWKS 验签。
pub struct AiPmDelegationVerifier {
    issuers: Vec<TrustedIssuer>,
    jwks: JwksCache,
}

impl AiPmDelegationVerifier {
    /// `issuers` 必须非空且 issuer 互异（配置解析层已保证；这里防御性再查）。
    /// 受众不在本层校验：它是 [`DELEGATION_AUDIENCE`] 编译期常量，无配置面。
    pub fn new(issuers: Vec<TrustedIssuer>) -> Result<Self, String> {
        let mut seen = std::collections::HashSet::new();
        for issuer in &issuers {
            if issuer.issuer.is_empty() {
                return Err("trusted issuer id must not be empty".into());
            }
            if !seen.insert(issuer.issuer.clone()) {
                return Err(format!("duplicate trusted issuer '{}'", issuer.issuer));
            }
        }
        if issuers.is_empty() {
            return Err("at least one trusted issuer is required".into());
        }
        Ok(Self {
            issuers,
            jwks: JwksCache::new(),
        })
    }

    /// 测试装配：外部注入 HTTP 客户端与缓存 TTL（回环 JWKS 服务器用）。
    pub fn with_jwks_cache(issuers: Vec<TrustedIssuer>, jwks: JwksCache) -> Result<Self, String> {
        let verifier = Self::new(issuers)?;
        Ok(Self {
            issuers: verifier.issuers,
            jwks,
        })
    }

    fn trusted_issuer(&self, iss: &str) -> Result<&TrustedIssuer, VerificationError> {
        self.issuers
            .iter()
            .find(|candidate| candidate.issuer == iss)
            .ok_or_else(|| VerificationError::Unauthenticated(format!("untrusted issuer '{iss}'")))
    }
}

#[async_trait]
impl DelegationVerifier for AiPmDelegationVerifier {
    async fn verify(&self, bearer_token: &str) -> Result<Delegation, VerificationError> {
        // ① 头部：RS256 + kid 必填（kid-strict）。
        let header =
            decode_header(bearer_token).map_err(|e| invalid_token("malformed token header", e))?;
        if header.alg != Algorithm::RS256 {
            return Err(VerificationError::Unauthenticated(
                "token alg must be RS256".into(),
            ));
        }
        let kid = header.kid.ok_or_else(|| {
            VerificationError::Unauthenticated("token header is missing kid".into())
        })?;

        // ② issuer 选择（配置固定；不做动态发现）。
        let peeked = peek_issuer(bearer_token)?;
        let trusted = self.trusted_issuer(&peeked)?;

        // ③ JWKS：kid-strict 查找（未知 kid 单次限频刷新；失败关闭）。
        let jwk = match self.jwks.lookup(&trusted.jwks_url, &kid).await {
            JwksLookup::Known(jwk) => jwk,
            JwksLookup::UnknownKid(reason) => {
                return Err(VerificationError::Unauthenticated(format!(
                    "unknown kid '{kid}' for issuer '{}': {reason}",
                    trusted.issuer
                )));
            }
            JwksLookup::Unavailable(reason) => {
                return Err(VerificationError::VerifierUnavailable(reason));
            }
        };
        let decoding_key = DecodingKey::from_jwk(&jwk)
            .map_err(|e| invalid_token("jwk is not usable for RS256 verification", e))?;

        // ④ 验签 + iss/aud/exp/nbf（leeway = 60s）。受众恒为契约常量
        // `artifact-service`（WP-03：不可被任何配置覆盖）。
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[DELEGATION_AUDIENCE]);
        validation.set_issuer(&[trusted.issuer.as_str()]);
        validation.leeway = CLOCK_SKEW_LEEWAY_SECS as u64;
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.required_spec_claims = ["exp".into(), "iat".into()].into_iter().collect();
        let token_data = decode::<DelegationClaims>(bearer_token, &decoding_key, &validation)
            .map_err(|e| invalid_token("token signature or standard claims rejected", e))?;
        let claims = token_data.claims;

        // ⑤ 契约级校验（aud 已由验证强制为契约常量受众；这里复核固定语义，
        // 与库校验双保险；数组型 aud 在 claims 反序列化层即被拒绝）。
        if claims.aud != DELEGATION_AUDIENCE {
            return Err(VerificationError::Unauthenticated(format!(
                "token audience must be exactly '{DELEGATION_AUDIENCE}' (single string)"
            )));
        }
        if claims.purpose != DELEGATION_PURPOSE {
            return Err(VerificationError::Unauthenticated(format!(
                "token purpose must be '{DELEGATION_PURPOSE}'"
            )));
        }
        validate_time_claims(&claims, chrono::Utc::now().timestamp())?;
        validate_operation_binding(&claims)?;

        if claims.actions.is_empty() {
            return Err(VerificationError::Unauthenticated(
                "token actions must not be empty".into(),
            ));
        }
        // V8：无重复（契约明确要求；重复条目视为形状异常）。
        let mut seen_actions = std::collections::HashSet::new();
        let mut actions = std::collections::HashSet::new();
        for raw in &claims.actions {
            if !seen_actions.insert(raw.as_str()) {
                return Err(VerificationError::Unauthenticated(format!(
                    "duplicate action '{raw}' in token actions"
                )));
            }
            let action = Action::parse(raw).ok_or_else(|| {
                VerificationError::Unauthenticated(format!("unknown action '{raw}'"))
            })?;
            actions.insert(action);
        }
        // PC-O5-1（WP-03 契约红线，O-5 移除的强制前置条件）：授予写动作的
        // delegation 必须携带顶层 caller_request_id（操作绑定）。缺失即拒绝
        // （fail-closed）；并发仲裁由业务幂等层（AS-1/AS-2 唯一约束）承接，
        // 不依赖签发方自律，也不引入 AS 侧持久化绑定状态。
        if actions.iter().any(|a| a.is_write()) && claims.caller_request_id.is_none() {
            return Err(VerificationError::Unauthenticated(
                "delegation grants write actions but carries no caller_request_id \
                 (operation binding is mandatory for write paths, contract §3.1.1)"
                    .into(),
            ));
        }
        // V6：jti 必须为合法 UUID（撤销查询主键；非 UUID 拒绝）。
        let jti_uuid = claims.jti.parse::<Uuid>().map_err(|_| {
            VerificationError::Unauthenticated("token jti must be a valid UUID".into())
        })?;
        let caller_request_id = claims
            .caller_request_id
            .as_deref()
            .map(|raw| {
                raw.parse::<Uuid>().map_err(|_| {
                    VerificationError::Unauthenticated(
                        "caller_request_id must be a valid UUID (AS-2 caller identity type)".into(),
                    )
                })
            })
            .transpose()?;
        let actor_type = map_actor_type(claims.actor_type.as_deref())?;

        Ok(Delegation {
            delegation_id: claims.delegation_id.to_string(),
            jti: jti_uuid.to_string(),
            scope: TenantScope::new(claims.tenant_id, claims.project_id),
            actor: artifact_domain::Actor::new(claims.sub, actor_type),
            actions,
            expires_at: chrono::DateTime::from_timestamp(claims.exp, 0).ok_or_else(|| {
                VerificationError::Unauthenticated("token exp out of range".into())
            })?,
            lmms_tid: claims.lmms_tid.clone(),
            artifact_id: claims.artifact_id,
            operation_id: claims.operation_id.clone(),
            caller_request_id,
            request_hash: claims.biz.as_ref().and_then(|b| b.request_hash.clone()),
        })
    }
}

/// 归一化 token 级拒绝：消息只含原因 + 库错误类别，无 token 内容。
fn invalid_token(reason: &str, err: impl std::fmt::Display) -> VerificationError {
    // jsonwebtoken 错误类型无敏感内容（不含 token 本体），类别名可安全入消息。
    let rendered = err.to_string();
    let category = rendered.split(':').next().unwrap_or("invalid").trim();
    VerificationError::Unauthenticated(format!("{reason} ({category})"))
}

/// 暴露缓存 TTL 常量供部署文档引用。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_issuer_is_rejected() {
        let verifier =
            AiPmDelegationVerifier::new(vec![TrustedIssuer::new("ai-pm", "https://pm/jwks")])
                .unwrap();
        let err = verifier.trusted_issuer("lmms").unwrap_err();
        assert!(
            matches!(err, VerificationError::Unauthenticated(msg) if msg.contains("untrusted issuer"))
        );
    }

    #[test]
    fn empty_or_duplicate_issuer_config_is_refused() {
        assert!(AiPmDelegationVerifier::new(vec![]).is_err());
        assert!(
            AiPmDelegationVerifier::new(vec![
                TrustedIssuer::new("ai-pm", "https://a/jwks"),
                TrustedIssuer::new("ai-pm", "https://b/jwks"),
            ])
            .is_err()
        );
    }

    #[test]
    fn time_rules_reject_bad_windows() {
        let now = chrono::Utc::now().timestamp();
        let base = |iat: i64, exp: i64| DelegationClaims {
            iss: "ai-pm".into(),
            sub: "u".into(),
            aud: DELEGATION_AUDIENCE.into(),
            purpose: DELEGATION_PURPOSE.into(),
            tenant_id: Uuid::from_u128(1),
            lmms_tid: Some("1".into()),
            project_id: Uuid::from_u128(2),
            artifact_id: None,
            operation_id: None,
            caller_request_id: None,
            actions: vec!["artifact:read".into()],
            iat,
            exp,
            nbf: None,
            jti: Uuid::new_v4().to_string(),
            delegation_id: Uuid::from_u128(3),
            actor_type: None,
            act: None,
            biz: None,
        };
        // exp <= iat 拒绝。
        assert!(validate_time_claims(&base(200, 200), now).is_err());
        // TTL 超限拒绝。
        assert!(validate_time_claims(&base(now, now + MAX_TOKEN_TTL_SECS + 1), now).is_err());
        // TTL 恰好 300s 通过。
        assert!(validate_time_claims(&base(now, now + MAX_TOKEN_TTL_SECS), now).is_ok());
        // 未来 iat 超出偏差拒绝。
        assert!(
            validate_time_claims(&base(now + CLOCK_SKEW_LEEWAY_SECS + 1, now + 120), now).is_err()
        );
        // 小偏差内未来 iat 通过。
        assert!(validate_time_claims(&base(now + 10, now + 120), now).is_ok());
        // nbf > exp 拒绝。
        let mut c = base(now, now + 300);
        c.nbf = Some(now + 301);
        assert!(validate_time_claims(&c, now).is_err());
    }

    #[test]
    fn biz_pairing_rules() {
        let mk = |caller: Option<Uuid>, hash: Option<&str>| DelegationClaims {
            iss: "ai-pm".into(),
            sub: "u".into(),
            aud: DELEGATION_AUDIENCE.into(),
            purpose: DELEGATION_PURPOSE.into(),
            tenant_id: Uuid::from_u128(1),
            lmms_tid: Some("1".into()),
            project_id: Uuid::from_u128(2),
            artifact_id: None,
            operation_id: None,
            caller_request_id: caller.map(|u| u.to_string()),
            actions: vec!["artifact:read".into()],
            iat: 0,
            exp: 300,
            nbf: None,
            jti: Uuid::new_v4().to_string(),
            delegation_id: Uuid::from_u128(3),
            actor_type: None,
            act: None,
            biz: Some(BizClaim {
                task_run_id: None,
                request_hash: hash.map(str::to_string),
                request_hash_alg: None,
            }),
        };
        // 无指纹扩展：任意 caller_request_id（含缺省）都合法。
        assert!(validate_operation_binding(&mk(None, None)).is_ok());
        assert!(validate_operation_binding(&mk(Some(Uuid::new_v4()), None)).is_ok());
        // 指纹必须与顶层 caller_request_id 配对。
        assert!(
            validate_operation_binding(&mk(Some(Uuid::new_v4()), Some(&"a".repeat(64)))).is_ok()
        );
        assert!(validate_operation_binding(&mk(None, Some(&"a".repeat(64)))).is_err());
        // 大写十六进制拒绝（指纹必须小写规范化）。
        assert!(
            validate_operation_binding(&mk(Some(Uuid::new_v4()), Some(&"A".repeat(64)))).is_err()
        );
        // 长度错误拒绝。
        assert!(
            validate_operation_binding(&mk(Some(Uuid::new_v4()), Some(&"a".repeat(63)))).is_err()
        );

        // caller_request_id 非 UUID 拒绝（AS-2 类型边界）。
        let mut bad_caller = mk(None, None);
        bad_caller.caller_request_id = Some("not-a-uuid".into());
        assert!(validate_operation_binding(&bad_caller).is_err());

        // ── PC-O6-2：算法版本标识 ──────────────────────────────────────
        // 已知算法 v1 + hash：合法。
        let mut ok_alg = mk(Some(Uuid::new_v4()), Some(&"a".repeat(64)));
        ok_alg.biz.as_mut().unwrap().request_hash_alg =
            Some(artifact_application::REQUEST_HASH_ALGORITHM_V1.into());
        assert!(validate_operation_binding(&ok_alg).is_ok());
        // 未知算法拒绝（fail-closed：新算法必须先扩展验证器）。
        let mut bad_alg = mk(Some(Uuid::new_v4()), Some(&"a".repeat(64)));
        bad_alg.biz.as_mut().unwrap().request_hash_alg = Some("v2".into());
        assert!(validate_operation_binding(&bad_alg).is_err());
        // alg 无 hash 半配对拒绝。
        let mut orphan_alg = mk(Some(Uuid::new_v4()), None);
        orphan_alg.biz.as_mut().unwrap().request_hash_alg =
            Some(artifact_application::REQUEST_HASH_ALGORITHM_V1.into());
        assert!(validate_operation_binding(&orphan_alg).is_err());
    }

    #[test]
    fn write_actions_require_caller_request_id() {
        // PC-O5-1 红线的判定矩阵：11 写动作 / 4 读动作分类冻结。
        for action in [
            "artifact:create",
            "artifact:update_metadata",
            "artifact:archive",
            "artifact:delete",
            "version:create",
            "version:commit",
            "version:set_current",
            "reference:write",
            "reference:remove",
            "generation:request",
            "generation:cancel",
        ] {
            let action = Action::parse(action).unwrap();
            assert!(action.is_write(), "{action} must classify as write");
        }
        for action in [
            "artifact:read",
            "version:read",
            "generation:read",
            "output:download",
        ] {
            let action = Action::parse(action).unwrap();
            assert!(!action.is_write(), "{action} must classify as read");
        }
    }

    #[test]
    fn actor_type_mapping_is_total_and_strict() {
        assert_eq!(map_actor_type(None).unwrap(), ActorType::User);
        assert_eq!(map_actor_type(Some("user")).unwrap(), ActorType::User);
        assert_eq!(map_actor_type(Some("agent")).unwrap(), ActorType::Agent);
        assert_eq!(map_actor_type(Some("service")).unwrap(), ActorType::Service);
        assert!(map_actor_type(Some("admin")).is_err());
    }
}
