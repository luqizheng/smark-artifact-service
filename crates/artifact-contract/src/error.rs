//! 统一错误信封（ART-01 §4.1 / §8.1）：
//! `{ "error": { "code", "message", "request_id", "retryable", "details" } }`

use artifact_domain::DomainError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorEnvelope {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
    pub request_id: Option<String>,
    pub retryable: bool,
    #[serde(default)]
    pub details: Value,
}

impl ErrorEnvelope {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        request_id: Option<String>,
        retryable: bool,
        details: Value,
    ) -> Self {
        Self {
            error: ErrorDetail {
                code: code.into(),
                message: message.into(),
                request_id,
                retryable,
                details: empty_object_if_null(details),
            },
        }
    }

    pub fn unauthenticated(message: impl Into<String>, request_id: Option<String>) -> Self {
        Self::new("UNAUTHENTICATED", message, request_id, false, Value::Null)
    }

    pub fn from_domain(err: &DomainError, request_id: Option<String>) -> Self {
        let details = match err {
            DomainError::ScopeDenied(_)
            | DomainError::InvalidRequest(_)
            | DomainError::VersionConflict(_) => {
                let mut m = Map::new();
                m.insert("reason".to_string(), Value::String(err.to_string()));
                Value::Object(m)
            }
            _ => Value::Null,
        };
        Self::new(
            err.code(),
            err.to_string(),
            request_id,
            err.retryable(),
            details,
        )
    }

    /// ART-01 §8.1 稳定错误码对应的 HTTP 状态。
    pub fn http_status(&self) -> u16 {
        match self.error.code.as_str() {
            "INVALID_REQUEST" => 400,
            "UNAUTHENTICATED" => 401,
            "SCOPE_DENIED" => 403,
            "RESOURCE_NOT_FOUND" => 404,
            "VERSION_CONFLICT" | "IDEMPOTENCY_KEY_REUSED" => 409,
            "PAYLOAD_TOO_LARGE" => 413,
            "UNSUPPORTED_MEDIA_TYPE" => 415,
            "GENERATOR_VALIDATION_FAILED" => 422,
            "RATE_LIMITED" => 429,
            "STORAGE_UNAVAILABLE" | "GENERATOR_UNAVAILABLE" => 503,
            "GENERATION_TIMEOUT" => 504,
            _ => 500,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("error envelope is always serializable")
    }
}

fn empty_object_if_null(v: Value) -> Value {
    if v.is_null() {
        Value::Object(Map::new())
    } else {
        v
    }
}

/// API 层错误类型：认证失败（无有效委托）或领域错误。
#[derive(Debug, Clone)]
pub enum ApiError {
    Unauthenticated(String),
    Domain(DomainError),
}

impl ApiError {
    pub fn envelope(&self, request_id: Option<String>) -> ErrorEnvelope {
        match self {
            ApiError::Unauthenticated(msg) => ErrorEnvelope::unauthenticated(msg, request_id),
            ApiError::Domain(err) => ErrorEnvelope::from_domain(err, request_id),
        }
    }
}

impl From<DomainError> for ApiError {
    fn from(err: DomainError) -> Self {
        ApiError::Domain(err)
    }
}
