//! Artifact Service API 契约层：DTO 与统一错误信封。
//!
//! 唯一 API 描述来源是 `openapi/openapi.yaml`（OpenAPI 3.1）；本 crate 的
//! DTO 字段名与错误信封结构通过契约测试（`tests/contract_openapi.rs`）
//! 与规范对齐，禁止无声漂移。

pub mod dto;
pub mod error;

pub use dto::*;
pub use error::{ApiError, ErrorDetail, ErrorEnvelope};
