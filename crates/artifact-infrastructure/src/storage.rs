//! File Storage Port 的生产实现：**协议阻断**。
//!
//! ART-02 核验结论：file-service 尚无 upload-intent / checksum attestation /
//! finalize / object verification / cleanup 能力（P0 存储协议）。本实现把
//! StoragePort 的每个能力都如实拒绝，绝不把普通上传或 presigned PUT
//! 伪装成 Artifact Version 提交协议。

use artifact_application::{ObjectInspection, StorageError, StoragePort, UploadIntent};
use async_trait::async_trait;

pub const PROTOCOL_BLOCKED_REASON: &str = "file-service upload/finalize protocol is not implemented upstream \
     (missing upload-intent, checksum attestation, finalize, object verification, \
     cleanup/compensation) — blocked by ART-02 storage P0";

#[derive(Debug, Clone, Copy, Default)]
pub struct ProtocolBlockedStorage;

impl ProtocolBlockedStorage {
    pub fn new() -> Self {
        Self
    }

    fn blocked<T>(&self) -> Result<T, StorageError> {
        Err(StorageError::ProtocolBlocked(
            PROTOCOL_BLOCKED_REASON.to_string(),
        ))
    }
}

#[async_trait]
impl StoragePort for ProtocolBlockedStorage {
    async fn create_upload_intent(
        &self,
        _tenant_id: uuid::Uuid,
        _project_id: uuid::Uuid,
        _suggested_name: &str,
    ) -> Result<UploadIntent, StorageError> {
        self.blocked()
    }

    async fn inspect_object(&self, _object_id: &str) -> Result<ObjectInspection, StorageError> {
        self.blocked()
    }

    async fn create_download_intent(&self, _object_id: &str) -> Result<String, StorageError> {
        self.blocked()
    }

    async fn copy_or_finalize_object(&self, _object_id: &str) -> Result<String, StorageError> {
        self.blocked()
    }

    async fn delete_object(&self, _object_id: &str) -> Result<(), StorageError> {
        self.blocked()
    }

    async fn head_object(&self, _object_id: &str) -> Result<ObjectInspection, StorageError> {
        self.blocked()
    }
}
