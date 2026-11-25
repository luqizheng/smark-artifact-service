//! API DTO（字段名与 `openapi/openapi.yaml` 保持一致，snake_case）。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

// ── Health / Ready / Generators ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub service: String,
    pub version: String,
    pub git_sha: String,
    pub built_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyResponse {
    pub status: String,
    pub components: ReadyComponents,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyComponents {
    pub database: String,
    /// 诚实上报：file-service 上传协议被 ART-02 P0 阻断。
    pub file_storage: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratorResponse {
    pub generator_id: String,
    pub display_name: String,
    pub version: String,
    pub adapter_version: Option<String>,
    pub supported_input_formats: Vec<String>,
    pub supported_output_formats: Vec<String>,
    pub capabilities: Value,
    pub status: String,
    pub status_reason: Option<String>,
}

// ── Artifact ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateArtifactRequest {
    pub project_id: Uuid,
    pub name: String,
    pub artifact_type: String,
    #[serde(default)]
    pub artifact_key: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateArtifactRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactResponse {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub project_id: Uuid,
    pub artifact_key: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub artifact_type: String,
    pub category: Option<String>,
    pub lifecycle_state: String,
    pub current_version_id: Option<Uuid>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    /// 乐观并发锚（与 ETag 响应头一致）。
    pub etag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactPage {
    pub items: Vec<ArtifactResponse>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListArtifactsQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub artifact_type: Option<String>,
    #[serde(default)]
    pub lifecycle_state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetCurrentVersionRequest {
    pub version_id: Uuid,
    /// 乐观并发：期望的当前 current_version_id（ART-01 §3.3）。
    #[serde(default)]
    pub expected_current_version_id: Option<Uuid>,
}

// ── Version / Output ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateVersionSessionRequest {
    #[serde(default)]
    pub source_format: Option<String>,
    #[serde(default)]
    pub source_object_id: Option<String>,
    #[serde(default)]
    pub source_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactVersionResponse {
    pub id: Uuid,
    pub artifact_id: Uuid,
    pub version_number: i64,
    pub state: String,
    pub source_format: Option<String>,
    pub source_object_id: Option<String>,
    pub source_sha256: Option<String>,
    pub generator_id: Option<String>,
    pub generator_version: Option<String>,
    pub adapter_version: Option<String>,
    pub manifest: Value,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub ready_at: Option<DateTime<Utc>>,
    pub failure_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionPage {
    pub items: Vec<ArtifactVersionResponse>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactOutputResponse {
    pub id: Uuid,
    pub version_id: Uuid,
    pub output_role: String,
    pub format: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub sha256: String,
    pub file_object_id: String,
    pub safety_status: String,
    pub created_at: DateTime<Utc>,
}

// ── Reference ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateReferenceRequest {
    pub owner_type: String,
    pub owner_id: String,
    pub relation_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceResponse {
    pub id: Uuid,
    pub artifact_id: Uuid,
    pub owner_type: String,
    pub owner_id: String,
    pub relation_type: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencePage {
    pub items: Vec<ReferenceResponse>,
    pub next_cursor: Option<String>,
}

// ── GenerationJob ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateGenerationJobRequest {
    pub generator_id: String,
    pub input: Value,
    #[serde(default)]
    pub options: Option<Value>,
    #[serde(default)]
    pub expected_current_version_id: Option<Uuid>,
}

/// GenerationJob 的调用方视图。输入正文永不出现在响应中。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationJobResponse {
    pub id: Uuid,
    pub artifact_id: Uuid,
    pub generator_id: String,
    pub status: String,
    /// 终态或最近一次失败原因的稳定机器码（例如 `GENERATION_TIMEOUT`）；成功时为空。
    pub error_code: Option<String>,
    pub attempt_count: i32,
    pub max_attempts: i32,
    /// running 期间已收到取消请求、尚未由 Worker 确认时为 true。
    pub cancel_requested: bool,
    pub created_at: DateTime<Utc>,
    pub poll_url: String,
}
