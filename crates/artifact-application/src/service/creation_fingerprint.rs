//! 版本化 Artifact 创建语义指纹（ART-04E §4.3–§4.6）。
//!
//! 指纹只覆盖「创建」语义：作用域、artifact_key、类型、名称、描述、分类。
//! 它**不是** Idempotency-Key 的原始 HTTP Body Hash：后者是传输层重放凭据，
//! 前者是持久化创建身份的语义版本。两者绝不可混用。
//!
//! 规范化规则（v1，冻结）：
//!   * 字符串先做 Unicode NFC 规范化，再参与校验、持久化与哈希；
//!   * 结构化 JSON 使用固定域名分隔符 `artifact-create:v1`，字段名按字典序，
//!     紧凑序列化（无空白），缺失与显式 null 统一为 `null`；
//!   * 不包含 created_at、请求 ID、已解析存储位置等可变上下文。
//!
//! 新增版本时只能追加 `fingerprint_version` 与新规范化函数，不得原地修改 v1。

use serde_json::{Map, Value, json};
use unicode_normalization::UnicodeNormalization;

use crate::ValidatedCreateArtifactRequest;

/// 当前写入新身份行时使用的指纹版本。
pub const CURRENT_FINGERPRINT_VERSION: i16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FingerprintError {
    #[error("unsupported fingerprint version {0}")]
    UnsupportedVersion(i16),
}

/// 按请求指定的指纹版本计算规范指纹（64 位小写十六进制 SHA-256）。
///
/// 解析既有身份时使用**存储的版本**，确保规范化规则升级后旧 key 仍可解析。
pub fn canonical_fingerprint(
    version: i16,
    request: &ValidatedCreateArtifactRequest,
) -> Result<String, FingerprintError> {
    match version {
        1 => Ok(fingerprint_v1(request)),
        other => Err(FingerprintError::UnsupportedVersion(other)),
    }
}

fn fingerprint_v1(request: &ValidatedCreateArtifactRequest) -> String {
    // ASCII 字典序插入；无论 serde_json 是否启用 preserve_order，输出字节都一致。
    let mut doc = Map::new();
    doc.insert(
        "artifact_key".into(),
        Value::String(request.artifact_key.clone()),
    );
    doc.insert(
        "artifact_type".into(),
        Value::String(request.artifact_type.clone()),
    );
    doc.insert(
        "category".into(),
        request
            .category
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    doc.insert(
        "description".into(),
        request
            .description
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    doc.insert("domain".into(), Value::String("artifact-create:v1".into()));
    doc.insert("name".into(), Value::String(request.name.clone()));
    doc.insert(
        "project_id".into(),
        Value::String(request.project_id.to_string()),
    );
    doc.insert(
        "tenant_id".into(),
        Value::String(request.tenant_id.to_string()),
    );
    let canonical = serde_json::to_string(&Value::Object(doc))
        .expect("canonical create document is always serializable");
    sha256_hex(canonical.as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 规范化单个字符串字段（Unicode NFC）。所有参与指纹与持久化的字符串
/// 必须走同一函数，避免「哈希用 NFC、落库用原串」的不一致。
pub fn normalize_text(value: &str) -> String {
    value.nfc().collect()
}

/// 规范化可空字符串字段。
pub fn normalize_optional_text(value: Option<&str>) -> Option<String> {
    value.map(normalize_text)
}

/// 供指纹文档与诊断使用的规范 JSON（不含哈希），仅用于测试与审计。
pub fn canonical_document(
    version: i16,
    request: &ValidatedCreateArtifactRequest,
) -> Result<Value, FingerprintError> {
    match version {
        1 => Ok(json!({
            "artifact_key": request.artifact_key,
            "artifact_type": request.artifact_type,
            "category": request.category,
            "description": request.description,
            "domain": "artifact-create:v1",
            "name": request.name,
            "project_id": request.project_id.to_string(),
            "tenant_id": request.tenant_id.to_string(),
        })),
        other => Err(FingerprintError::UnsupportedVersion(other)),
    }
}

/// 便捷函数：以当前版本计算指纹。
pub fn current_fingerprint(request: &ValidatedCreateArtifactRequest) -> String {
    canonical_fingerprint(CURRENT_FINGERPRINT_VERSION, request)
        .expect("current fingerprint version is always supported")
}

#[cfg(test)]
mod tests {
    use super::*;
    use artifact_domain::TenantScope;
    use uuid::Uuid;

    const TENANT: Uuid = Uuid::from_u128(0x0001);
    const PROJECT: Uuid = Uuid::from_u128(0x0101);

    fn golden_request() -> ValidatedCreateArtifactRequest {
        ValidatedCreateArtifactRequest {
            tenant_id: TENANT,
            project_id: PROJECT,
            artifact_key: "golden.key".into(),
            name: "Golden".into(),
            description: None,
            artifact_type: "visualization".into(),
            category: None,
            created_by: "test-user".into(),
        }
    }

    /// 冻结的 v1 golden vector：规范化规则或字段集一旦变化，本测试必须失败，
    /// 并强制引入 fingerprint_version 2，而不是原地漂移。
    #[test]
    fn v1_golden_vector_is_frozen() {
        assert_eq!(
            current_fingerprint(&golden_request()),
            "12582ea318bad295895061ec55d4f1dbd70b5f306e7652dec56ae21af532c810"
        );
    }

    #[test]
    fn each_semantic_field_changes_the_fingerprint() {
        let base = current_fingerprint(&golden_request());
        let mutations: Vec<fn(&mut ValidatedCreateArtifactRequest)> = vec![
            |r| r.artifact_key = "golden.other".into(),
            |r| r.artifact_type = "report".into(),
            |r| r.name = "Golden 2".into(),
            |r| r.description = Some("d".into()),
            |r| r.category = Some("c".into()),
            |r| r.project_id = Uuid::from_u128(0x0102),
            |r| r.tenant_id = Uuid::from_u128(0x0002),
        ];
        for mutate in mutations {
            let mut request = golden_request();
            mutate(&mut request);
            assert_ne!(
                current_fingerprint(&request),
                base,
                "a semantic field change must change the fingerprint"
            );
        }
    }

    /// 审计元数据 `created_by` 不是创建语义，旋转服务凭据不得产生第二个身份。
    #[test]
    fn created_by_is_not_part_of_the_fingerprint() {
        let mut rotated = golden_request();
        rotated.created_by = "another-service-principal".into();
        assert_eq!(
            current_fingerprint(&rotated),
            current_fingerprint(&golden_request())
        );
    }

    /// absent 与显式 null 归一为同一表示；空串不是 null，不做静默 trim。
    #[test]
    fn null_and_absent_are_equivalent_but_empty_string_is_not() {
        let mut absent = golden_request();
        absent.description = None;
        let mut explicit_null = golden_request();
        explicit_null.description = None;
        assert_eq!(
            current_fingerprint(&absent),
            current_fingerprint(&explicit_null)
        );

        let mut empty = golden_request();
        empty.description = Some(String::new());
        assert_ne!(current_fingerprint(&empty), current_fingerprint(&absent));
    }

    #[test]
    fn unicode_equivalent_requests_share_one_fingerprint() {
        let scope = TenantScope::new(TENANT, PROJECT);
        let nfc = ValidatedCreateArtifactRequest::try_new(
            &scope,
            "caf\u{e9}".into(),
            "caf\u{e9}".into(),
            None,
            "visualization".into(),
            None,
            "test-user".into(),
        )
        .unwrap();
        let nfd = ValidatedCreateArtifactRequest::try_new(
            &scope,
            "cafe\u{301}".into(),
            "cafe\u{301}".into(),
            None,
            "visualization".into(),
            None,
            "test-user".into(),
        )
        .unwrap();
        assert_eq!(current_fingerprint(&nfc), current_fingerprint(&nfd));
    }

    #[test]
    fn unknown_fingerprint_version_fails_closed() {
        assert_eq!(
            canonical_fingerprint(2, &golden_request()),
            Err(FingerprintError::UnsupportedVersion(2))
        );
    }
}
