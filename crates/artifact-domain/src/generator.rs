//! Generator 注册信息（ART-01 §7）。
//!
//! Mermaid 与 SeeCode 是后续正式 Generator Adapter；本阶段仅维护注册表
//! 与能力描述，未实现的 Adapter 必须如实上报 `unavailable`。

use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratorStatus {
    Available,
    Unavailable,
    Disabled,
}

impl GeneratorStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            GeneratorStatus::Available => "available",
            GeneratorStatus::Unavailable => "unavailable",
            GeneratorStatus::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GeneratorDescriptor {
    pub generator_id: String,
    pub display_name: String,
    pub version: String,
    pub adapter_version: Option<String>,
    pub supported_input_formats: Vec<String>,
    pub supported_output_formats: Vec<String>,
    /// capabilities / resource_profile / security_profile 等结构化描述。
    pub capabilities: Value,
    pub status: GeneratorStatus,
    pub status_reason: Option<String>,
    pub registered_at: Option<Uuid>,
}
