//! GenerationJob 创建语义指纹 v1（ART-04G v3 §5.2 / §10.4 P3-3）。
//!
//! 指纹由 Rust 侧对**经过验证的 typed DTO** 计算：`lowercase_hex(SHA-256(UTF-8(canonical_json)))`。
//! 三个哈希语义绝不可混淆：
//!   * `request_fingerprint` — caller 业务操作身份（本模块）；
//!   * `request_hash`        — 现有 HTTP 请求原始内容哈希（传输层重放凭据）；
//!   * `input_sha256`        — 已持久化 input snapshot 的哈希。
//!
//! 规范化规则（v1，冻结；变更必须引入 fingerprint_version 2，不得原地修改）：
//!   * canonical JSON：紧凑、对象键**显式递归按 Unicode code point 排序**（P3-3，
//!     不依赖 serde_json 默认排序行为）、数组保序；
//!   * generator_id 使用 Unicode NFC；payload 内字符串保持原始字节语义，不做 NFC；
//!   * DTO 层 `options` 缺省与 `null` 等价；payload 内部字段缺省与 `null` 必须区分；
//!   * 数字：整数按整数、浮点按最短往返；`1e2` 与 `100.0` 等价；整数 `100` 与
//!     浮点 `100.0` 区分；NaN/Infinity 等非有限数值拒绝（失败关闭）；
//!   * 不包含：Idempotency-Key、request_hash、input_sha256、trace/request ID、
//!     expected_current_version_id、server generator/adapter 版本、执行策略、
//!     timestamp、target_version_id。

use serde_json::{Map, Value};

use crate::ValidatedCreateGenerationJobRequest;

/// 当前写入新 Job 身份时使用的指纹版本。
pub const CURRENT_JOB_FINGERPRINT_VERSION: i16 = 1;

/// v1 域分隔符。
pub const GENERATION_JOB_CREATE_DOMAIN: &str = "generation-job-create:v1";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobFingerprintError {
    #[error("unsupported job fingerprint version {0}")]
    UnsupportedVersion(i16),
    #[error("non-finite number in payload cannot be fingerprinted")]
    NonFiniteNumber,
}

/// 按请求指定的指纹版本计算规范指纹（64 位小写十六进制 SHA-256）。
pub fn canonical_job_fingerprint(
    version: i16,
    request: &ValidatedCreateGenerationJobRequest,
) -> Result<String, JobFingerprintError> {
    match version {
        1 => fingerprint_v1(request),
        other => Err(JobFingerprintError::UnsupportedVersion(other)),
    }
}

/// v1 指纹文档（紧凑 canonical JSON 的来源）。键以 code point 序插入，
/// 随后仍经 `canonicalize` 显式重排，双重保证输出与插入顺序无关。
fn document_v1(
    request: &ValidatedCreateGenerationJobRequest,
) -> Result<Value, JobFingerprintError> {
    let mut doc = Map::new();
    doc.insert(
        "artifact_id".into(),
        Value::String(request.artifact_id.to_string()),
    );
    doc.insert(
        "caller_request_id".into(),
        match request.caller_request_id {
            Some(caller) => Value::String(caller.to_string()),
            // 未键控（legacy）请求不走指纹路径；防御性归一为 null。
            None => Value::Null,
        },
    );
    doc.insert(
        "domain".into(),
        Value::String(GENERATION_JOB_CREATE_DOMAIN.into()),
    );
    // generator_id 已在 Validated 层完成 NFC。
    doc.insert(
        "generator_id".into(),
        Value::String(request.generator_id.clone()),
    );
    // payload 字符串保持原始字节语义：canonicalize 不做 NFC。
    doc.insert("input".into(), canonicalize(&request.input)?);
    // DTO 层缺省 ≡ null：options: None 与 Some(json!(null)) 归一为 null。
    doc.insert(
        "options".into(),
        request.options.clone().unwrap_or(Value::Null),
    );
    doc.insert(
        "project_id".into(),
        Value::String(request.project_id.to_string()),
    );
    doc.insert(
        "tenant_id".into(),
        Value::String(request.tenant_id.to_string()),
    );
    Ok(Value::Object(doc))
}

fn fingerprint_v1(
    request: &ValidatedCreateGenerationJobRequest,
) -> Result<String, JobFingerprintError> {
    let doc = document_v1(request)?;
    let canonical = serde_json::to_string(&doc)
        .expect("canonical generation-job document is always serializable");
    Ok(sha256_hex(canonical.as_bytes()))
}

/// P3-3：显式递归按 Unicode code point 排序。UTF-8 字节序与 Unicode
/// code point 序完全一致，因此按字节比较即为按 code point 比较。
/// 该重排不依赖 serde_json::Map 的默认行为（BTreeMap 或 preserve_order 皆可）。
fn canonicalize(value: &Value) -> Result<Value, JobFingerprintError> {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            let mut out = Map::with_capacity(keys.len());
            for key in keys {
                out.insert(key.to_owned(), canonicalize(&map[key])?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => {
            // 数组保序：顺序是语义的一部分。
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(canonicalize(item)?);
            }
            Ok(Value::Array(out))
        }
        Value::Number(n) => {
            // 非有限数值拒绝（失败关闭）。serde_json 解析层面已拒绝 NaN/Infinity
            // 字面量；此检查防御程序化构造的 Value。
            if n.as_f64().is_some_and(|f| !f.is_finite()) {
                return Err(JobFingerprintError::NonFiniteNumber);
            }
            // serde_json 的数字表示自带规范形：整数保持整数（100），浮点经 ryu
            // 最短往返（100.0、1e2 → 同一 f64 → "100.0"）。
            Ok(value.clone())
        }
        // 字符串保持原始字节语义（payload 不做 NFC）；null/bool 原样。
        other => Ok(other.clone()),
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
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

/// 供测试与审计使用的规范 JSON 文档（不含哈希）。
pub fn canonical_job_document(
    version: i16,
    request: &ValidatedCreateGenerationJobRequest,
) -> Result<Value, JobFingerprintError> {
    match version {
        1 => document_v1(request),
        other => Err(JobFingerprintError::UnsupportedVersion(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use artifact_domain::TenantScope;
    use serde_json::json;
    use uuid::Uuid;

    const TENANT: Uuid = Uuid::from_u128(0x0001);
    const PROJECT: Uuid = Uuid::from_u128(0x0101);
    const ARTIFACT: Uuid = Uuid::from_u128(0x0201);
    const CALLER: Uuid = Uuid::from_u128(0x0301);

    /// 通过真实 envelope 校验（`validate_job_input_envelope`）的最小合法输入。
    fn envelope(diagram: Value) -> Value {
        json!({
            "schema": "smark.seecode.input",
            "schema_version": "1",
            "diagram": diagram,
        })
    }

    fn golden_request() -> ValidatedCreateGenerationJobRequest {
        ValidatedCreateGenerationJobRequest::try_new(
            &TenantScope::new(TENANT, PROJECT),
            ARTIFACT,
            Some(CALLER),
            "seecode".into(),
            envelope(json!({
                "type": "architecture",
                "title": "golden prompt",
                "meta": {"b": 2, "a": [1, 2, 3]},
                "z": "末"
            })),
            None,
            INPUT_SCHEMA_VERSION_PLACEHOLDER.to_string(),
            4096,
        )
        .expect("golden request is valid")
    }

    // input_schema_version：golden 用固定值；真实常量见 service::job_input。
    const INPUT_SCHEMA_VERSION_PLACEHOLDER: &str = "1";

    /// 冻结的 v1 golden vector（AS2-01）：规范化规则或字段集一旦变化，本测试
    /// 必须失败并强制引入 fingerprint_version 2，而不是原地漂移。
    /// golden 值已用独立实现（Python hashlib + 显式 sort_keys 紧凑序列化）复核。
    #[test]
    fn v1_golden_vector_is_frozen() {
        let doc = canonical_job_document(1, &golden_request()).unwrap();
        let canonical = serde_json::to_string(&doc).unwrap();
        assert_eq!(
            canonical,
            concat!(
                "{\"artifact_id\":\"00000000-0000-0000-0000-000000000201\",",
                "\"caller_request_id\":\"00000000-0000-0000-0000-000000000301\",",
                "\"domain\":\"generation-job-create:v1\",",
                "\"generator_id\":\"seecode\",",
                "\"input\":{\"diagram\":{\"meta\":{\"a\":[1,2,3],\"b\":2},",
                "\"title\":\"golden prompt\",\"type\":\"architecture\",\"z\":\"末\"},",
                "\"schema\":\"smark.seecode.input\",\"schema_version\":\"1\"},",
                "\"options\":null,",
                "\"project_id\":\"00000000-0000-0000-0000-000000000101\",",
                "\"tenant_id\":\"00000000-0000-0000-0000-000000000001\"}"
            )
        );
        assert_eq!(
            canonical_job_fingerprint(1, &golden_request()).unwrap(),
            "64e2d6651752208dd15c40d8010488f6818a43770c8b71af58f493adf6e126f3"
        );
    }

    /// AS2-02：每个语义字段的突变都必须改变指纹。
    #[test]
    fn each_semantic_field_changes_the_fingerprint() {
        let base = canonical_job_fingerprint(1, &golden_request()).unwrap();
        let mutations: Vec<fn(&mut ValidatedCreateGenerationJobRequest)> = vec![
            |r| r.artifact_id = Uuid::from_u128(0x0202),
            |r| r.caller_request_id = Some(Uuid::from_u128(0x0302)),
            |r| r.generator_id = "other-generator".into(),
            |r| r.input = envelope(json!({"type": "architecture", "title": "changed"})),
            |r| r.options = Some(json!({"format": "svg"})),
            |r| r.project_id = Uuid::from_u128(0x0102),
            |r| r.tenant_id = Uuid::from_u128(0x0002),
        ];
        for mutate in mutations {
            let mut request = golden_request();
            mutate(&mut request);
            assert_ne!(
                canonical_job_fingerprint(1, &request).unwrap(),
                base,
                "a semantic field change must change the generation-job fingerprint"
            );
        }
    }

    /// AS2-03：对象键序、空白、options absent ≡ null 等价。
    #[test]
    fn key_order_and_options_absence_are_equivalent() {
        // 键序不同、内容相同 → 同一指纹（显式递归排序）。
        let a = ValidatedCreateGenerationJobRequest::try_new(
            &TenantScope::new(TENANT, PROJECT),
            ARTIFACT,
            Some(CALLER),
            "seecode".into(),
            envelope(
                json!({"type": "architecture", "meta": {"alpha": 1, "beta": {"y": 2, "x": 1}}}),
            ),
            None,
            "1".to_string(),
            4096,
        )
        .unwrap();
        let b = ValidatedCreateGenerationJobRequest::try_new(
            &TenantScope::new(TENANT, PROJECT),
            ARTIFACT,
            Some(CALLER),
            "seecode".into(),
            envelope(
                json!({"meta": {"beta": {"x": 1, "y": 2}, "alpha": 1}, "type": "architecture"}),
            ),
            None,
            "1".to_string(),
            4096,
        )
        .unwrap();
        assert_eq!(
            canonical_job_fingerprint(1, &a).unwrap(),
            canonical_job_fingerprint(1, &b).unwrap()
        );

        // options absent ≡ 显式 null（DTO 层归一）。
        let mut with_null = golden_request();
        with_null.options = Some(Value::Null);
        assert_eq!(
            canonical_job_fingerprint(1, &golden_request()).unwrap(),
            canonical_job_fingerprint(1, &with_null).unwrap()
        );

        // 顶层空白（紧凑序列化）不影响指纹：紧凑输出无分隔符空白。
        let canonical =
            serde_json::to_string(&canonical_job_document(1, &golden_request()).unwrap()).unwrap();
        assert!(!canonical.contains("\": "), "{canonical}");
        assert!(!canonical.contains(", "), "{canonical}");
    }

    /// AS2-04：payload 字符串保持原始字节语义（不做 NFC）；NFD 与 NFC 字节
    /// 不同 → 指纹不同。generator_id 则必须 NFC 等价。
    #[test]
    fn payload_strings_keep_bytes_but_generator_id_is_nfc() {
        let nfc_input =
            envelope(json!({"type": "architecture", "nodes": [{"id": "a", "label": "caf\u{e9}"}]}));
        let nfd_input = envelope(
            json!({"type": "architecture", "nodes": [{"id": "a", "label": "cafe\u{301}"}]}),
        );
        let a = ValidatedCreateGenerationJobRequest::try_new(
            &TenantScope::new(TENANT, PROJECT),
            ARTIFACT,
            Some(CALLER),
            "seecode".into(),
            nfc_input,
            None,
            "1".to_string(),
            4096,
        )
        .unwrap();
        let b = ValidatedCreateGenerationJobRequest::try_new(
            &TenantScope::new(TENANT, PROJECT),
            ARTIFACT,
            Some(CALLER),
            "seecode".into(),
            nfd_input,
            None,
            "1".to_string(),
            4096,
        )
        .unwrap();
        assert_ne!(
            canonical_job_fingerprint(1, &a).unwrap(),
            canonical_job_fingerprint(1, &b).unwrap(),
            "payload strings must keep raw byte semantics (no NFC)"
        );

        let gen_nfc = ValidatedCreateGenerationJobRequest::try_new(
            &TenantScope::new(TENANT, PROJECT),
            ARTIFACT,
            Some(CALLER),
            "caf\u{e9}-gen".into(),
            envelope(json!({"type": "architecture"})),
            None,
            "1".to_string(),
            4096,
        )
        .unwrap();
        let gen_nfd = ValidatedCreateGenerationJobRequest::try_new(
            &TenantScope::new(TENANT, PROJECT),
            ARTIFACT,
            Some(CALLER),
            "cafe\u{301}-gen".into(),
            envelope(json!({"type": "architecture"})),
            None,
            "1".to_string(),
            4096,
        )
        .unwrap();
        assert_eq!(gen_nfc.generator_id, gen_nfd.generator_id);
        assert_eq!(
            canonical_job_fingerprint(1, &gen_nfc).unwrap(),
            canonical_job_fingerprint(1, &gen_nfd).unwrap(),
            "generator_id must be NFC-normalized"
        );
    }

    /// AS2-05：排除字段不在指纹中——指纹文档的键集合被钉死，任何新增传输层
    /// 字段（Idempotency-Key、request_hash、input_sha256、trace/request ID、
    /// expected_current_version_id、generator/adapter 版本、执行策略、时间戳、
    /// target_version_id）都不可能进入指纹。
    #[test]
    fn transport_credentials_do_not_change_the_fingerprint() {
        let doc = canonical_job_document(1, &golden_request()).unwrap();
        let mut keys: Vec<&str> = doc
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "artifact_id",
                "caller_request_id",
                "domain",
                "generator_id",
                "input",
                "options",
                "project_id",
                "tenant_id",
            ],
            "the fingerprint document must contain exactly the frozen v1 field set"
        );
        // options 空对象与 absent/null 是不同语义（归一规则只覆盖 null ≡ absent）。
        let mut with_empty_options = golden_request();
        with_empty_options.options = Some(json!({}));
        assert_ne!(
            canonical_job_fingerprint(1, &golden_request()).unwrap(),
            canonical_job_fingerprint(1, &with_empty_options).unwrap(),
            "empty options object is a different semantic from absent/null"
        );
    }

    /// AS2-06：数字规范化——1e2 ≡ 100.0；整数 100 ≠ 浮点 100.0；数组保序。
    #[test]
    fn number_normalization_rules() {
        let doc = |input: Value| {
            ValidatedCreateGenerationJobRequest::try_new(
                &TenantScope::new(TENANT, PROJECT),
                ARTIFACT,
                Some(CALLER),
                "seecode".into(),
                input,
                None,
                "1".to_string(),
                4096,
            )
            .unwrap()
        };
        let exp = canonical_job_fingerprint(
            1,
            &doc(envelope(
                json!({"type": "architecture", "meta": {"n": 1e2}}),
            )),
        )
        .unwrap();
        let f = canonical_job_fingerprint(
            1,
            &doc(envelope(
                json!({"type": "architecture", "meta": {"n": 100.0}}),
            )),
        )
        .unwrap();
        let int = canonical_job_fingerprint(
            1,
            &doc(envelope(
                json!({"type": "architecture", "meta": {"n": 100}}),
            )),
        )
        .unwrap();
        assert_eq!(exp, f, "1e2 and 100.0 are the same f64 value");
        assert_ne!(int, f, "integer 100 and float 100.0 are distinct");

        // 数组顺序是语义。
        let arr1 = canonical_job_fingerprint(
            1,
            &doc(envelope(
                json!({"type": "architecture", "meta": {"a": [1, 2]}}),
            )),
        )
        .unwrap();
        let arr2 = canonical_job_fingerprint(
            1,
            &doc(envelope(
                json!({"type": "architecture", "meta": {"a": [2, 1]}}),
            )),
        )
        .unwrap();
        assert_ne!(arr1, arr2, "array order is preserved and semantic");

        // payload 内部字段缺省与 null 必须区分。
        let absent =
            canonical_job_fingerprint(1, &doc(envelope(json!({"type": "architecture"})))).unwrap();
        let explicit_null = canonical_job_fingerprint(
            1,
            &doc(envelope(
                json!({"type": "architecture", "meta": {"missing": null}}),
            )),
        )
        .unwrap();
        assert_ne!(absent, explicit_null);
    }

    /// AS2-06 补充：非有限数值无法进入指纹（JSON 层面即被拒绝）。
    #[test]
    fn non_finite_numbers_are_unrepresentable() {
        assert!(serde_json::Number::from_f64(f64::NAN).is_none());
        assert!(serde_json::Number::from_f64(f64::INFINITY).is_none());
        assert!(serde_json::Number::from_f64(f64::NEG_INFINITY).is_none());
        assert!(serde_json::from_str::<Value>("NaN").is_err());
        assert!(serde_json::from_str::<Value>("1e999").is_err());
    }

    /// AS2-07：未知版本失败关闭。
    #[test]
    fn unknown_fingerprint_version_fails_closed() {
        assert_eq!(
            canonical_job_fingerprint(2, &golden_request()),
            Err(JobFingerprintError::UnsupportedVersion(2))
        );
    }
}
