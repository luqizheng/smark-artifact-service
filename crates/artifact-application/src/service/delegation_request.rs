//! 委托请求绑定辅助：规范化请求指纹与委托↔请求一致性判定（WP 契约 v0.2 §3.4）。
//!
//! 指纹规范（v0.2 冻结）：
//! ```text
//! request_hash = lower_hex(sha256(
//!     method_uppercase || "\n" ||
//!     path_with_prefix  || "\n" ||
//!     caller_request_id (小写 uuid；无则空串) || "\n" ||
//!     canonical_json(body)
//! ))
//! canonical_json = serde_json Value（BTreeMap 排序键、无空白）序列化；body 为空时取 `null`
//! ```
//! 签发方（AI-PM，同为 Rust + serde_json）与 AS 用同一算法；AS 在 AS-4 接线的
//! 写路径入口重算并与 claim `biz.request_hash` 与绑定存储中的指纹三方比对。
//! body 含浮点或重复键的请求不得签发委托（规范化不确定性 → 拒签，签发方责任）。

use sha2::{Digest, Sha256};
use uuid::Uuid;

use artifact_domain::{Delegation, DomainError};

/// 按契约 v0.2 冻结的指纹规范计算请求指纹（64 位小写十六进制）。
pub fn canonical_request_fingerprint(
    method: &str,
    path: &str,
    caller_request_id: Option<Uuid>,
    body: &serde_json::Value,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(method.to_ascii_uppercase());
    hasher.update(b"\n");
    hasher.update(path.as_bytes());
    hasher.update(b"\n");
    hasher.update(
        caller_request_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
    );
    hasher.update(b"\n");
    hasher.update(
        serde_json::to_string(body)
            .unwrap_or_else(|_| "null".into())
            .as_bytes(),
    );
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// 判定已验证委托是否确为"本次请求"签发（声明层比对；jti 绑定存储比对见
/// [`crate::DelegationBindingStore`]）。任一不一致即拒绝，绝不降级放行：
/// * 写操作委托必须携带 biz 配对（caller_request_id + request_hash）；
/// * caller_request_id 必须与请求一致；
/// * request_hash 必须与 AS 重算的规范化指纹一致。
pub fn assert_delegation_matches_request(
    delegation: &Delegation,
    caller_request_id: Uuid,
    request_hash: &str,
) -> Result<(), DomainError> {
    let Some(claimed_caller) = delegation.caller_request_id else {
        return Err(DomainError::ScopeDenied(
            "delegation token carries no operation binding (biz.caller_request_id missing); \
             write operations require an operation-bound token"
                .into(),
        ));
    };
    if claimed_caller != caller_request_id {
        return Err(DomainError::ScopeDenied(format!(
            "delegation token is not minted for caller_request_id {caller_request_id}"
        )));
    }
    let Some(claimed_hash) = delegation.request_hash.as_deref() else {
        return Err(DomainError::ScopeDenied(
            "delegation token carries no request fingerprint (biz.request_hash missing)".into(),
        ));
    };
    if claimed_hash != request_hash {
        return Err(DomainError::ScopeDenied(
            "request fingerprint does not match the delegation token binding; \
             the token is not minted for this request content"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use artifact_domain::{Action, Actor, ActorType, TenantScope};
    use std::collections::HashSet;

    fn delegation(caller: Option<Uuid>, hash: Option<&str>) -> Delegation {
        Delegation {
            delegation_id: Uuid::new_v4().to_string(),
            jti: Uuid::new_v4().to_string(),
            scope: TenantScope::new(Uuid::from_u128(0x11), Uuid::from_u128(0x22)),
            actor: Actor::new("user-1", ActorType::User),
            actions: HashSet::from([Action::GenerationRequest]),
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(300),
            lmms_tid: Some("1".into()),
            artifact_id: None,
            operation_id: None,
            caller_request_id: caller,
            request_hash: hash.map(str::to_string),
        }
    }

    #[test]
    fn fingerprint_is_deterministic_and_field_order_free() {
        let caller = Uuid::new_v4();
        let body_a = serde_json::json!({"a": 1, "b": [true, "x"]});
        let body_b = serde_json::json!({"b": [true, "x"], "a": 1});
        let h1 = canonical_request_fingerprint("post", "/v1/artifacts", Some(caller), &body_a);
        let h2 = canonical_request_fingerprint("POST", "/v1/artifacts", Some(caller), &body_b);
        assert_eq!(h1, h2, "method case and key order must not change the hash");
        assert_eq!(h1.len(), 64);
        let h3 = canonical_request_fingerprint("post", "/v1/artifacts", Some(caller), &body_a);
        assert_eq!(h1, h3);
    }

    #[test]
    fn different_body_or_caller_changes_fingerprint() {
        let caller = Uuid::new_v4();
        let base = canonical_request_fingerprint(
            "post",
            "/v1/x",
            Some(caller),
            &serde_json::json!({"k": "v"}),
        );
        let other_body = canonical_request_fingerprint(
            "post",
            "/v1/x",
            Some(caller),
            &serde_json::json!({"k": "v2"}),
        );
        let other_caller = canonical_request_fingerprint(
            "post",
            "/v1/x",
            Some(Uuid::new_v4()),
            &serde_json::json!({"k": "v"}),
        );
        assert_ne!(base, other_body);
        assert_ne!(base, other_caller);
    }

    #[test]
    fn matching_request_is_accepted() {
        let caller = Uuid::new_v4();
        let hash =
            canonical_request_fingerprint("post", "/v1/x", Some(caller), &serde_json::json!({}));
        let d = delegation(Some(caller), Some(&hash));
        assert!(assert_delegation_matches_request(&d, caller, &hash).is_ok());
    }

    #[test]
    fn unbound_or_tampered_requests_are_rejected() {
        let caller = Uuid::new_v4();
        let hash =
            canonical_request_fingerprint("post", "/v1/x", Some(caller), &serde_json::json!({}));
        let other_caller = Uuid::new_v4();

        // 缺绑定字段。
        let d = delegation(None, None);
        assert!(assert_delegation_matches_request(&d, caller, &hash).is_err());
        // 半配对（有 caller 无指纹）。
        let d = delegation(Some(caller), None);
        assert!(assert_delegation_matches_request(&d, caller, &hash).is_err());
        // caller 不一致。
        let d = delegation(Some(other_caller), Some(&hash));
        assert!(assert_delegation_matches_request(&d, caller, &hash).is_err());
        // 指纹被篡改（同 caller，不同请求内容）。
        let d = delegation(Some(caller), Some(&hash));
        assert!(assert_delegation_matches_request(&d, caller, &"0".repeat(64)).is_err());
    }
}
