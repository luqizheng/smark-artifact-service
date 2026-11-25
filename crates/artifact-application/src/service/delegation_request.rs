//! 委托请求绑定辅助：规范化请求指纹（O-6，算法 **v1**）与委托↔请求一致性
//! 判定（V10，AS-4 接线消费；本 WP-03 仅冻结规范与基础类型，不接线）。
//!
//! # request_hash 规范 v1（冻结；契约 §3.1.1 / `biz.request_hash_alg`）
//!
//! ```text
//! request_hash(v1) = lower_hex(sha256(
//!     METHOD_UPPER || "\n" ||
//!     PATH         || "\n" ||
//!     CALLER       || "\n" ||
//!     CANONICAL_JSON(body)
//! ))
//! ```
//!
//! * **METHOD_UPPER**：HTTP method 的 ASCII 大写形式（`post` → `POST`）。
//!   method 参与指纹，杜绝跨 method 混淆。
//! * **PATH**：请求目标路径（含 `/v1` 前缀），**实例化形态**——路径参数以
//!   实际值代入（UUID 用小写连字符形式）。UTF-8 字节原样参与；不做百分号
//!   解码/重编码、不归一化大小写、**不含 query 与 fragment**。v1 的全部写
//!   端点路径均由固定段 + UUID 构成，签发方必须使用无百分号编码的规范形式
//!   （同一路径的两种编码形态视为不同请求——按字节比对，差异即拒绝，
//!   fail-closed）。
//! * **CALLER**：顶层 `caller_request_id` 的小写连字符 UUID 字符串；无则空串。
//! * **CANONICAL_JSON(body)**：`serde_json::Value` 紧凑序列化——
//!   * 对象键按 **UTF-8 字节序**字典序排序（serde_json 默认 `BTreeMap`，
//!     本 workspace 未启用 `preserve_order`）；无空白分隔；
//!   * 整数按十进制（i64/u64 范围）；浮点按 ryu 最短往返表示——**签发方必须
//!     拒签含浮点的 body**（跨语言表示不确定性，AS 侧不可检测）；
//!   * 字符串按 UTF-8 原样输出，**不做 Unicode NFC/NFD 规范化**（编码差异
//!     在字节层即表现为不同指纹，方向 fail-closed）；
//!   * `null`、缺失键与空串是三个不同值；
//!   * body 为空时取字面量 `null`。
//! * **重复键**：AS 解析后 last-wins；**签发方必须拒签**原始 JSON 含重复
//!   对象键的请求（解析后无法检测，属签发侧义务）。
//! * **query**：v1 不参与指纹；v1 全部写端点**不得携带 query**。未来引入带
//!   query 的写端点必须先将规范升版（v2）并同步修订契约——这是契约红线，
//!   不是实现建议。
//! * **版本**：claim `biz.request_hash_alg` 缺省视为 `v1`；显式给出时仅接受
//!   [`REQUEST_HASH_ALGORITHM_V1`]，未知算法在验证器 fail-closed 拒绝。
//!   算法演进必须升版并保持旧版本可判定，禁止原地改变 v1 语义。
//!
//! # 与 AI-PM 签发端的一致性要求
//!
//! AI-PM 签发侧必须实现同一算法（PC-O6-3），并以本模块测试中的**冻结向量**
//! 为交叉基准（同输入 → 同 64 位小写十六进制）。request_hash 与顶层
//! `caller_request_id` 配对出现（§3.1.1 红线）；AS 在 AS-4 接线的写路径入口
//! 重算并与 claim 比对（V10；**本阶段未接线**，不得宣称目标绑定已生效）。

use sha2::{Digest, Sha256};
use uuid::Uuid;

use artifact_domain::{Delegation, DomainError};

/// request_hash 算法版本标识（`biz.request_hash_alg`；PC-O6-2）。
pub const REQUEST_HASH_ALGORITHM_V1: &str = "v1";

/// 计算请求指纹（算法 [`REQUEST_HASH_ALGORITHM_V1`]；64 位小写十六进制）。
///
/// 本函数是 v1 规范的**唯一权威实现**；任何语义变更必须升版（v2）而非修改
/// 本函数（v1 与测试冻结向量绑定，禁止原地演进）。
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

/// 判定已验证委托是否确为"本次请求"签发（声明层比对；AS-4 接线的唯一调用点，
/// 当前无生产调用点——V10 未接线，不得宣称目标绑定已生效）。
/// 任一不一致即拒绝，绝不降级放行：
/// * 写操作委托必须携带 biz 配对（caller_request_id + request_hash）；
/// * caller_request_id 必须与请求一致；
/// * request_hash 必须与 AS 按 v1 规范重算的指纹一致。
///
/// （`artifact_id`/`operation_id` 与请求目标的比对属 V10 接线的同族义务，
/// 由 AS-4 一并实现；本函数当前仅覆盖 caller/hash 两轴。）
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

    // ── request_hash v1 冻结向量（PC-O6-3 交叉基准；禁止修改期望值） ─────
    //
    // 这些常量由 v1 规范唯一确定，是 AI-PM 签发侧的跨语言对齐基准
    // （Rust serde_json 未启用 preserve_order：BTreeMap 键序 + ryu 浮点 +
    // UTF-8 原样）。修改 v1 语义必须升版 v2，而不是改这里。

    /// 向量 1 固定输入。
    fn vector_caller() -> Uuid {
        Uuid::parse_str("0b2e5f1a-9c4d-4e8a-b7f6-2a1d3c5e7f90").unwrap()
    }

    #[test]
    fn v1_frozen_vector_minimal_body() {
        // 输入：POST /v1/artifacts + caller 0b2e5f1a-9c4d-4e8a-b7f6-2a1d3c5e7f90
        //       + body {"artifact_type":"visualization","name":"wp03-vector"}
        let h = canonical_request_fingerprint(
            "post",
            "/v1/artifacts",
            Some(vector_caller()),
            &serde_json::json!({"artifact_type": "visualization", "name": "wp03-vector"}),
        );
        assert_eq!(
            h, "64cc551d71fd091999486c83377cc492b2e38aa5946d23c93d771f49de5f4d8f",
            "v1 frozen vector must stay stable"
        );
    }

    #[test]
    fn v1_key_order_and_method_case_are_canonicalized() {
        // 键序与方法大小写不影响指纹（与向量 1 同输入）。
        let a = canonical_request_fingerprint(
            "POST",
            "/v1/artifacts",
            Some(vector_caller()),
            &serde_json::json!({"name": "wp03-vector", "artifact_type": "visualization"}),
        );
        assert_eq!(
            a,
            "64cc551d71fd091999486c83377cc492b2e38aa5946d23c93d771f49de5f4d8f"
        );
    }

    #[test]
    fn v1_frozen_vector_null_body_and_missing_caller() {
        // body 为空 → 字面量 null；caller 缺省 → 空串段。
        let h = canonical_request_fingerprint(
            "POST",
            "/v1/generation-jobs",
            None,
            &serde_json::Value::Null,
        );
        assert_eq!(
            h,
            "645127e060bbdab9760aafb82c012f212a1da5d582f6f8a9912e8cdd539ec266"
        );
    }

    #[test]
    fn v1_frozen_vector_unicode_is_byte_exact() {
        // UTF-8 原样、无 NFC/NFD 规范化。
        let h = canonical_request_fingerprint(
            "PATCH",
            "/v1/artifacts/5c1e0000-0000-4000-8000-000000000001",
            Some(vector_caller()),
            &serde_json::json!({"标题": "中文✓"}),
        );
        assert_eq!(
            h,
            "51dae3324e1caaef6466b5f782c2d8a079e871917e7237197bb893f991975019"
        );
    }

    #[test]
    fn v1_number_null_and_string_semantics_are_distinct() {
        // 1 / 1.0 / "1" 三个不同值；缺失键 ≠ null。
        let int_form =
            canonical_request_fingerprint("POST", "/v1/x", None, &serde_json::json!({"n": 1}));
        let float_form =
            canonical_request_fingerprint("POST", "/v1/x", None, &serde_json::json!({"n": 1.0}));
        let string_form =
            canonical_request_fingerprint("POST", "/v1/x", None, &serde_json::json!({"n": "1"}));
        let null_value =
            canonical_request_fingerprint("POST", "/v1/x", None, &serde_json::json!({"n": null}));
        let missing_key =
            canonical_request_fingerprint("POST", "/v1/x", None, &serde_json::json!({}));
        assert_ne!(int_form, float_form);
        assert_ne!(int_form, string_form);
        assert_ne!(float_form, string_form);
        assert_ne!(null_value, missing_key);
    }

    #[test]
    fn v1_method_and_path_changes_alter_fingerprint() {
        let base = canonical_request_fingerprint(
            "POST",
            "/v1/artifacts",
            Some(vector_caller()),
            &serde_json::json!({"name": "x"}),
        );
        // 跨 method 混淆：不同 method → 不同指纹。
        let other_method = canonical_request_fingerprint(
            "PATCH",
            "/v1/artifacts",
            Some(vector_caller()),
            &serde_json::json!({"name": "x"}),
        );
        // 路径不同（含实例化 UUID 差异）→ 不同指纹。
        let other_path = canonical_request_fingerprint(
            "POST",
            "/v1/artifacts/5c1e0000-0000-4000-8000-000000000002",
            Some(vector_caller()),
            &serde_json::json!({"name": "x"}),
        );
        assert_ne!(base, other_method);
        assert_ne!(base, other_path);
    }
}
