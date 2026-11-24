//! File Storage Port（ART-01 §6）。
//!
//! Artifact Service 不直接持有长期对象存储凭据、不绕过 file-service。
//! 最低能力集合来自 ART-01 §6；当前 file-service 缺少 upload-intent /
//! checksum attestation / finalize / object verification / cleanup（ART-02
//! P0 存储协议阻断），因此生产实现暂以 `ProtocolBlocked` 明确拒绝。

use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// P0：file-service 协议能力未落地，禁止伪成功。
    #[error("file-service upload/finalize protocol is not available (ART-02 P0): {0}")]
    ProtocolBlocked(String),
    #[error("storage unavailable: {0}")]
    Unavailable(String),
    #[error("object not found")]
    ObjectNotFound,
}

/// 单对象、短时、限方法/大小的上传授权（ART-01 §6 create_upload_intent）。
#[derive(Debug, Clone)]
pub struct UploadIntent {
    pub upload_url: String,
    pub method: String,
    pub object_id: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub max_bytes: i64,
}

#[derive(Debug, Clone)]
pub struct ObjectInspection {
    pub object_id: String,
    pub size_bytes: i64,
    pub sha256: String,
    pub detected_mime: String,
}

#[async_trait]
pub trait StoragePort: Send + Sync {
    async fn create_upload_intent(
        &self,
        tenant_id: uuid::Uuid,
        project_id: uuid::Uuid,
        suggested_name: &str,
    ) -> Result<UploadIntent, StorageError>;

    async fn inspect_object(&self, object_id: &str) -> Result<ObjectInspection, StorageError>;

    async fn create_download_intent(&self, object_id: &str) -> Result<String, StorageError>;

    /// 将临时对象安全登记为不可变正式对象（如 file-service 支持）。
    async fn copy_or_finalize_object(&self, object_id: &str) -> Result<String, StorageError>;

    async fn delete_object(&self, object_id: &str) -> Result<(), StorageError>;

    async fn head_object(&self, object_id: &str) -> Result<ObjectInspection, StorageError>;
}
