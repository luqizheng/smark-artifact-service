//! Job 输入 envelope 的服务端校验（ART-03A A3A-01/A3A-02）。
//!
//! 输入是受限结构化数据，不是大文件。校验在应用层完成（第一道防线），
//! DB 的 CHECK 约束是第二道。拒绝时只返回稳定错误码与结构位置，绝不回显输入正文。
//!
//! 当前范围是过渡形态，ART-03A 验收项「OpenAPI/JSON Schema 冻结」完成前：
//!   * 信封层是 allowlist：顶层字段、`schema` / `schema_version` 的取值固定；
//!   * `diagram` / `options` / `provenance` 内部只有通用结构上限、危险字段名拦截和
//!     URL 形态的值拦截。逐字段的 allowlist 必须等 SeeCode 输入 schema 冻结后才能确定，
//!     在那之前 Adapter 不得把原始 JSON 直接交给 CLI。

use serde_json::Value;
use std::fmt;

pub const MAX_ENVELOPE_BYTES: usize = 256 * 1024;
pub const MAX_DEPTH: usize = 16;
pub const MAX_NODES: usize = 500;
pub const MAX_EDGES: usize = 2000;
pub const MAX_TEXT_BYTES: usize = 4 * 1024;
/// 单个数组 / 对象的元素或属性数量上限。
pub const MAX_COLLECTION_ITEMS: usize = 4096;
/// 整个请求体的 JSON 节点总数上限。文档规定的最大输入
/// （500 节点 + 2000 边，每个节点 20 个字段、每条边 6 个字段）约 2.4 万个节点。
pub const MAX_JSON_NODES: usize = 65_536;

/// 当前唯一受支持的 SeeCode 输入信封。
pub const INPUT_SCHEMA: &str = "smark.seecode.input";
pub const INPUT_SCHEMA_VERSION: &str = "1";
/// 信封顶层允许的字段。
const ENVELOPE_FIELDS: &[&str] = &[
    "diagram",
    "options",
    "provenance",
    "schema",
    "schema_version",
];

/// 危险字段名（按词元匹配，大小写与分隔符不敏感）。
///
/// 这是纵深防御，不是主防线：它拦的是 `img_url`、`file-path`、`onClick` 这一类
/// 明显不该出现在渲染输入里的键。真正的保证来自 schema 冻结后的逐字段 allowlist。
const FORBIDDEN_KEY_TOKENS: &[&str] = &[
    "url",
    "uri",
    "href",
    "src",
    "path",
    "command",
    "cmd",
    "script",
    "exec",
    "shell",
    "env",
    "environment",
    "token",
    "password",
    "passwd",
    "secret",
    "credential",
    "credentials",
    "delegation",
    "authorization",
    "auth",
    "js",
    "javascript",
    "onclick",
    "onload",
    "onerror",
    "ontoggle",
    "onmouseover",
    "onmouseout",
    "onfocus",
    "onblur",
];

/// 带协议的 URL、协议相对 URL，或 CSS 的 `url(...)` / `@import` 形态
/// （ART-03 §4.2：不接受外部 URL）。
fn looks_like_url(s: &str) -> bool {
    let s = s.trim_start();
    if s.starts_with("//") {
        return true;
    }
    let lowered = s.to_ascii_lowercase();
    if lowered.starts_with("url(") || lowered.starts_with("@import") {
        return true;
    }
    let Some((scheme, _)) = s.split_once(':') else {
        return false;
    };
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http"
            | "https"
            | "ftp"
            | "ftps"
            | "file"
            | "sftp"
            | "javascript"
            | "data"
            | "blob"
            | "ws"
            | "wss"
            | "vbscript"
    )
}

fn is_control(c: char) -> bool {
    // 换行、回车、制表符是正常文本的一部分；其余控制字符一律拒绝。
    (c.is_control() && !matches!(c, '\n' | '\r' | '\t')) || c == '\u{7f}'
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputValidationError {
    pub code: &'static str,
    /// 结构位置。只有形如普通标识符的字段名才会出现，其余以位置代替，
    /// 避免把调用方塞进字段名里的内容回显到错误响应或日志。
    pub field: String,
}

impl fmt::Display for InputValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}", self.code, self.field)
    }
}

fn err(code: &'static str, field: impl Into<String>) -> InputValidationError {
    InputValidationError {
        code,
        field: field.into(),
    }
}

/// 校验生成请求的 `input` 信封与请求级 `options`。
///
/// `envelope_bytes` 是 HTTP 原始 body 的字节数。
pub fn validate_job_input_envelope(
    envelope_bytes: usize,
    input: &Value,
    options: Option<&Value>,
) -> Result<(), InputValidationError> {
    if envelope_bytes > MAX_ENVELOPE_BYTES {
        return Err(err("INPUT_TOO_LARGE", "request body"));
    }

    let root = input
        .as_object()
        .ok_or_else(|| err("INVALID_ENVELOPE", "input"))?;
    for key in root.keys() {
        if !ENVELOPE_FIELDS.contains(&key.as_str()) {
            return Err(err("UNKNOWN_FIELD", "input"));
        }
    }
    let text_field = |name: &str| root.get(name).and_then(Value::as_str);
    if text_field("schema") != Some(INPUT_SCHEMA) {
        return Err(err("UNSUPPORTED_SCHEMA", "input.schema"));
    }
    if text_field("schema_version") != Some(INPUT_SCHEMA_VERSION) {
        return Err(err("UNSUPPORTED_SCHEMA", "input.schema_version"));
    }

    let mut budget = NodeBudget::default();
    validate_value(input, "input", 1, &mut budget)?;

    let diagram = root
        .get("diagram")
        .and_then(Value::as_object)
        .ok_or_else(|| err("INVALID_ENVELOPE", "input.diagram"))?;
    match diagram.get("type") {
        Some(Value::String(kind)) if !kind.trim().is_empty() => {}
        _ => return Err(err("INVALID_ENVELOPE", "input.diagram.type")),
    }
    check_array_cap(diagram, "nodes", MAX_NODES, "TOO_MANY_NODES")?;
    check_array_cap(diagram, "edges", MAX_EDGES, "TOO_MANY_EDGES")?;

    if let Some(opts) = options {
        if opts.as_object().is_none() {
            return Err(err("INVALID_ENVELOPE", "options"));
        }
        validate_value(opts, "options", 1, &mut budget)?;
    }
    Ok(())
}

fn check_array_cap(
    diagram: &serde_json::Map<String, Value>,
    key: &str,
    cap: usize,
    code: &'static str,
) -> Result<(), InputValidationError> {
    let Some(value) = diagram.get(key) else {
        return Ok(());
    };
    match value.as_array() {
        Some(items) if items.len() <= cap => Ok(()),
        Some(_) => Err(err(code, format!("input.diagram.{key}"))),
        None => Err(err("INVALID_ENVELOPE", format!("input.diagram.{key}"))),
    }
}

#[derive(Default)]
struct NodeBudget(usize);

impl NodeBudget {
    fn enter(&mut self) -> Result<(), InputValidationError> {
        self.0 += 1;
        if self.0 > MAX_JSON_NODES {
            return Err(err("TOO_COMPLEX", "input"));
        }
        Ok(())
    }
}

fn validate_value(
    value: &Value,
    path: &str,
    depth: usize,
    budget: &mut NodeBudget,
) -> Result<(), InputValidationError> {
    if depth > MAX_DEPTH {
        return Err(err("TOO_DEEP", path));
    }
    budget.enter()?;
    match value {
        Value::String(s) => {
            if s.len() > MAX_TEXT_BYTES {
                return Err(err("TEXT_TOO_LONG", path));
            }
            if s.chars().any(is_control) {
                return Err(err("CONTROL_CHARACTER", path));
            }
            if looks_like_url(s) {
                return Err(err("URL_NOT_ALLOWED", path));
            }
            Ok(())
        }
        Value::Array(items) => {
            if items.len() > MAX_COLLECTION_ITEMS {
                return Err(err("TOO_MANY_ITEMS", path));
            }
            for (i, item) in items.iter().enumerate() {
                validate_value(item, &format!("{path}[{i}]"), depth + 1, budget)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            if map.len() > MAX_COLLECTION_ITEMS {
                return Err(err("TOO_MANY_ITEMS", path));
            }
            for (i, (key, child)) in map.iter().enumerate() {
                let label = key_label(key, i);
                if is_forbidden_key(key) {
                    return Err(err("FORBIDDEN_FIELD", format!("{path}.{label}")));
                }
                validate_value(child, &format!("{path}.{label}"), depth + 1, budget)?;
            }
            Ok(())
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
    }
}

/// 错误位置里使用的字段名：短 ASCII 标识符原样使用，其余用位置代替。
fn key_label(key: &str, index: usize) -> String {
    let plain = key.len() <= 32
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if plain {
        key.to_string()
    } else {
        format!("<key#{index}>")
    }
}

fn is_forbidden_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    normalized
        .split(' ')
        .filter(|token| !token.is_empty())
        .any(|token| FORBIDDEN_KEY_TOKENS.contains(&token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope(nodes: serde_json::Value, edges: serde_json::Value) -> Value {
        json!({
            "schema": INPUT_SCHEMA,
            "schema_version": INPUT_SCHEMA_VERSION,
            "diagram": {"type": "architecture", "title": "Example", "nodes": nodes, "edges": edges}
        })
    }

    fn validate(input: &Value) -> Result<(), InputValidationError> {
        let bytes = serde_json::to_vec(input).unwrap().len();
        validate_job_input_envelope(bytes, input, None)
    }

    fn rejected(input: &Value) -> InputValidationError {
        validate(input).expect_err("input must be rejected")
    }

    fn single_node(field: &str, value: serde_json::Value) -> Value {
        envelope(json!([{"id": "a", field: value}]), json!([]))
    }

    /// 文档规定的最大输入必须能提交：500 节点 + 2000 边，每个元素带真实数量的字段。
    /// 各上限互相独立；元素字段很多时，同时取满会先触达 256 KiB 的 body 上限。
    #[test]
    fn documented_maximum_is_accepted() {
        let nodes: Vec<_> = (0..MAX_NODES)
            .map(|i| {
                json!({
                    "id": format!("n{i}"), "label": format!("node {i}"), "type": "service",
                    "description": "x".repeat(40), "tier": "backend", "owner": "team",
                    "tags": ["a", "b"], "meta": {"k": "v", "j": 1}
                })
            })
            .collect();
        let edges: Vec<_> = (0..MAX_EDGES)
            .map(|i| {
                json!({
                    "from": format!("n{}", i % MAX_NODES),
                    "to": format!("n{}", (i + 7) % MAX_NODES),
                    "label": "calls", "kind": "sync"
                })
            })
            .collect();
        let input = envelope(json!(nodes), json!(edges));
        let bytes = serde_json::to_vec(&input).unwrap().len();
        assert!(bytes < MAX_ENVELOPE_BYTES, "{bytes} bytes");
        validate_job_input_envelope(
            bytes,
            &input,
            Some(&json!({"theme": "light", "font_mode": "system"})),
        )
        .expect("documented maximum must be accepted");
    }

    #[test]
    fn rejects_oversized_body_and_malformed_envelope() {
        assert_eq!(rejected(&json!([])).code, "INVALID_ENVELOPE");
        let input = envelope(json!([]), json!([]));
        let e = validate_job_input_envelope(MAX_ENVELOPE_BYTES + 1, &input, None).unwrap_err();
        assert_eq!(e.code, "INPUT_TOO_LARGE");
    }

    #[test]
    fn rejects_deep_nesting_and_oversized_text() {
        let mut nested = json!("leaf");
        for _ in 0..(MAX_DEPTH + 2) {
            nested = json!({"children": [nested]});
        }
        assert_eq!(rejected(&envelope(nested, json!([]))).code, "TOO_DEEP");
        let long = json!([{"id": "a", "label": "x".repeat(MAX_TEXT_BYTES + 1)}]);
        assert_eq!(rejected(&envelope(long, json!([]))).code, "TEXT_TOO_LONG");
    }

    #[test]
    fn rejects_node_and_edge_overflow() {
        let too_many_nodes = envelope(json!(vec![json!({}); MAX_NODES + 1]), json!([]));
        assert_eq!(rejected(&too_many_nodes).code, "TOO_MANY_NODES");
        let too_many_edges = envelope(json!([]), json!(vec![json!({}); MAX_EDGES + 1]));
        assert_eq!(rejected(&too_many_edges).code, "TOO_MANY_EDGES");
        let not_an_array = envelope(json!({"id": "a"}), json!([]));
        assert_eq!(rejected(&not_an_array).code, "INVALID_ENVELOPE");
    }

    #[test]
    fn envelope_is_an_allowlist() {
        assert_eq!(rejected(&json!({})).code, "UNSUPPORTED_SCHEMA");
        assert_eq!(
            rejected(&json!({"schema": "aipm.vizspec"})).code,
            "UNSUPPORTED_SCHEMA"
        );

        let mut wrong_version = envelope(json!([]), json!([]));
        wrong_version["schema_version"] = json!("2");
        assert_eq!(rejected(&wrong_version).field, "input.schema_version");

        let mut unknown = envelope(json!([]), json!([]));
        unknown["internal_notes"] = json!("x");
        let e = rejected(&unknown);
        assert_eq!((e.code, e.field.as_str()), ("UNKNOWN_FIELD", "input"));

        let mut missing_diagram = envelope(json!([]), json!([]));
        missing_diagram.as_object_mut().unwrap().remove("diagram");
        assert_eq!(rejected(&missing_diagram).field, "input.diagram");

        let mut no_type = envelope(json!([]), json!([]));
        no_type["diagram"]["type"] = json!("");
        assert_eq!(rejected(&no_type).field, "input.diagram.type");
    }

    /// 评审中列出的绕过手法逐一验证。
    #[test]
    fn denies_url_fields_and_url_shaped_values() {
        let icon = single_node("icon", json!("http://evil.example/x.png"));
        let e = rejected(&icon);
        assert_eq!(
            (e.code, e.field.as_str()),
            ("URL_NOT_ALLOWED", "input.diagram.nodes[0].icon")
        );

        for value in [
            "https://x",
            "JavaScript:alert(1)",
            "data:text/html,x",
            "//evil.example/x",
            " file:///etc/passwd",
            "URL(http://x)",
            "@import url(http://x)",
        ] {
            let e = rejected(&single_node("label", json!(value)));
            assert_eq!(e.code, "URL_NOT_ALLOWED", "{value}");
        }

        for key in [
            "img_url",
            "URL",
            "url ",
            "icon-url",
            "file_path",
            "file-path",
            "onClick",
            "Authorization",
        ] {
            let mut input = envelope(json!([]), json!([]));
            input["diagram"][key] = json!("x");
            let e = rejected(&input);
            assert_eq!(e.code, "FORBIDDEN_FIELD", "key {key:?}");
        }
    }

    #[test]
    fn rejects_control_characters_but_keeps_normal_text() {
        let with_control = single_node("label", json!("a\u{0007}b"));
        assert_eq!(rejected(&with_control).code, "CONTROL_CHARACTER");

        let normal = json!([{
            "id": "a",
            "label": "line one\nline two\tindented\r\n",
            "note": "Note: like this",
            "api": "/orders/{id}"
        }]);
        validate(&envelope(normal, json!([]))).expect("normal text is accepted");
    }

    /// 错误位置不回显调用方塞进字段名里的内容。
    #[test]
    fn error_field_does_not_echo_arbitrary_key_names() {
        let long_key = format!("{}-url", "k".repeat(40));
        let mut input = envelope(json!([]), json!([]));
        input["diagram"][&long_key] = json!("x");
        let e = rejected(&input);
        assert_eq!(e.code, "FORBIDDEN_FIELD");
        assert!(e.field.ends_with(".<key#1>"), "{}", e.field);
        assert!(!e.to_string().contains("kkkk"));

        let mut control = envelope(json!([]), json!([]));
        control["diagram"][&format!("{}-note", "k".repeat(40))] = json!("x\u{0007}");
        let e = rejected(&control);
        assert_eq!(
            (e.code, e.field.as_str()),
            ("CONTROL_CHARACTER", "input.diagram.<key#1>")
        );

        let mut unknown = envelope(json!([]), json!([]));
        unknown["token-ABCDEF-secret-value"] = json!(1);
        assert_eq!(rejected(&unknown).field, "input");
    }

    #[test]
    fn request_level_options_are_validated_too() {
        let input = envelope(json!([]), json!([]));
        let bytes = serde_json::to_vec(&input).unwrap().len();
        let with = |options: &Value| validate_job_input_envelope(bytes, &input, Some(options));

        assert_eq!(
            with(&json!({"image_url": "http://x"})).unwrap_err().code,
            "FORBIDDEN_FIELD"
        );
        assert_eq!(with(&json!("light")).unwrap_err().code, "INVALID_ENVELOPE");
        with(&json!({"theme": "light"})).expect("flat options are accepted");
    }

    #[test]
    fn bounds_total_json_nodes() {
        let mut input = envelope(json!([]), json!([]));
        input["diagram"]["matrix"] = json!(vec![vec![0; 200]; 200]);
        validate(&input).expect("40k nodes are within the budget");

        input["diagram"]["matrix"] = json!(vec![vec![0; 256]; 256]);
        assert_eq!(rejected(&input).code, "TOO_COMPLEX");
    }
}
