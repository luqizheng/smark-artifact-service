//! Job 输入 envelope 的服务端校验（ART-03A A3A-01/A3A-02）。
//!
//! 输入是受限结构化数据，不是大文件。校验在应用层完成（第一道防线），
//! DB 的 CHECK 约束是第二道。拒绝时只返回稳定错误码与字段名，绝不回显输入正文。

use serde_json::Value;
use std::fmt;

pub const MAX_ENVELOPE_BYTES: usize = 256 * 1024;
pub const MAX_DEPTH: usize = 16;
pub const MAX_NODES: usize = 500;
pub const MAX_EDGES: usize = 2000;
pub const MAX_TEXT_BYTES: usize = 4 * 1024;
pub const MAX_COLLECTION_ENTRIES: usize = 4096;

/// 拒绝的字段名（大小写不敏感）：不接受任意 URL/路径/脚本/命令/环境变量/外部资源。
const FORBIDDEN_KEYS: &[&str] = &[
    "url",
    "uri",
    "href",
    "src",
    "path",
    "filepath",
    "file_path",
    "command",
    "cmd",
    "script",
    "exec",
    "shell",
    "env",
    "environment",
    "token",
    "password",
    "secret",
    "credential",
    "delegation",
    "authorization",
    "auth",
    "js",
    "javascript",
    "onclick",
    "onload",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputValidationError {
    pub code: &'static str,
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

/// 校验规范化后的 input+options envelope。
///
/// `envelope_bytes` 是 HTTP 原始 body 的字节数（未被 JSON 解析放大前）。
pub fn validate_job_input_envelope(
    envelope_bytes: usize,
    input: &Value,
    options: Option<&Value>,
) -> Result<(), InputValidationError> {
    if envelope_bytes > MAX_ENVELOPE_BYTES {
        return Err(err("INPUT_TOO_LARGE", "request body"));
    }
    validate_json(input, "input", 1, &mut 0)?;
    if let Some(opts) = options {
        validate_json(opts, "options", 1, &mut 0)?;
        // 集合中属性数量有明确上限（防止 options 成为另一条膨胀通道）。
        if let Value::Object(map) = opts
            && map.len() > MAX_COLLECTION_ENTRIES
        {
            return Err(err("TOO_MANY_PROPERTIES", "options"));
        }
    }
    // 节点/边是 SeeCode 输入的核心规模上限。
    if let Some(nodes) = input.pointer("/diagram/nodes").and_then(Value::as_array)
        && nodes.len() > MAX_NODES
    {
        return Err(err("TOO_MANY_NODES", "input.diagram.nodes"));
    }
    if let Some(edges) = input.pointer("/diagram/edges").and_then(Value::as_array)
        && edges.len() > MAX_EDGES
    {
        return Err(err("TOO_MANY_EDGES", "input.diagram.edges"));
    }
    Ok(())
}

fn validate_json(
    value: &Value,
    path: &str,
    depth: usize,
    entries: &mut usize,
) -> Result<(), InputValidationError> {
    if depth > MAX_DEPTH {
        return Err(err("TOO_DEEP", path));
    }
    *entries += 1;
    if *entries > MAX_COLLECTION_ENTRIES {
        return Err(err("TOO_MANY_ENTRIES", path));
    }
    match value {
        Value::Null => Ok(()),
        Value::Bool(_) | Value::Number(_) => Ok(()),
        Value::String(s) => {
            if s.len() > MAX_TEXT_BYTES {
                return Err(err("TEXT_TOO_LONG", path));
            }
            Ok(())
        }
        Value::Array(items) => {
            if items.len() > MAX_COLLECTION_ENTRIES {
                return Err(err("TOO_MANY_ENTRIES", path));
            }
            for (i, item) in items.iter().enumerate() {
                validate_json(item, &format!("{path}[{i}]"), depth + 1, entries)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            if map.len() > MAX_COLLECTION_ENTRIES {
                return Err(err("TOO_MANY_PROPERTIES", path));
            }
            for (key, child) in map {
                let lowered = key.to_ascii_lowercase();
                if FORBIDDEN_KEYS.contains(&lowered.as_str()) {
                    return Err(err("FORBIDDEN_FIELD", format!("{path}.{key}")));
                }
                validate_json(child, &format!("{path}.{key}"), depth + 1, entries)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_constrained_envelope() {
        let input = json!({
            "schema": "smark.seecode.input",
            "schema_version": "1",
            "diagram": {"type": "architecture", "nodes": [], "edges": []}
        });
        assert!(validate_job_input_envelope(256, &input, Some(&json!({"theme": "light"}))).is_ok());
    }

    #[test]
    fn rejects_oversized_body() {
        let input = json!({});
        let e = validate_job_input_envelope(MAX_ENVELOPE_BYTES + 1, &input, None).unwrap_err();
        assert_eq!(e.code, "INPUT_TOO_LARGE");
    }

    #[test]
    fn rejects_deep_nesting() {
        let mut nested = json!("leaf");
        for _ in 0..(MAX_DEPTH + 2) {
            nested = json!({"children": [nested]});
        }
        let e = validate_job_input_envelope(256, &nested, None).unwrap_err();
        assert_eq!(e.code, "TOO_DEEP");
    }

    #[test]
    fn rejects_forbidden_fields() {
        for key in ["url", "PATH", "Command", "delegation", "onload"] {
            let input = json!({"diagram": {key: "evil"}});
            let e = validate_job_input_envelope(256, &input, None).unwrap_err();
            assert_eq!(e.code, "FORBIDDEN_FIELD", "key {key}");
        }
    }

    #[test]
    fn rejects_too_many_nodes_edges_and_long_text() {
        let nodes = json!({"diagram": {"nodes": vec![json!({}); MAX_NODES + 1]}});
        assert_eq!(
            validate_job_input_envelope(256, &nodes, None)
                .unwrap_err()
                .code,
            "TOO_MANY_NODES"
        );
        let edges = json!({"diagram": {"edges": vec![json!({}); MAX_EDGES + 1]}});
        assert_eq!(
            validate_job_input_envelope(256, &edges, None)
                .unwrap_err()
                .code,
            "TOO_MANY_EDGES"
        );
        let text = json!({"title": "x".repeat(MAX_TEXT_BYTES + 1)});
        assert_eq!(
            validate_job_input_envelope(256, &text, None)
                .unwrap_err()
                .code,
            "TEXT_TOO_LONG"
        );
    }
}
