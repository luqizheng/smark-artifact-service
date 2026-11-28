//! File Storage Port（ART-01 §6 + WP-AS-05 修订）。
//!
//! Artifact Service 不直接持有长期对象存储凭据、不绕过 file-service。
//! ART-01 §6 的最低能力集合按 WP 契约 v0.3 §12 裁决以"上传 + 校验 + 补偿"
//! 适配器近似（LMMS 无 upload-intent / checksum attestation / finalize /
//! verification / cleanup 协议——P1 差异）：
//!
//! * **上传**：[`StoragePort::upload_object`] 由持服务身份的一方（Worker）
//!   代理上传；API 用户路径不代理字节——调用方先自行上传对象，再以
//!   `source_object_id` 创建 Version 会话（`create_upload_intent` 在 LMMS
//!   补齐 upload-intent 协议（P1）前保持明确不可用）。
//! * **校验**：commit 时 [`StoragePort::inspect_object`] 下载并计算 sha256，
//!   与会话声明的 `source_sha256` 比对（file-service 不计算 hash）。
//! * **补偿**：[`StoragePort::delete_object`] 仅用于 AS 自身上传的输出对象
//!   失败回滚；调用方自有对象不因 AS 侧失败被删除。
//! * **F-24**：所有调用以带 `tid` 的短时服务令牌发出（见
//!   [`crate::file_service::service_jwt`]），绝不触发 file-service 的
//!   `unwrap_or(1)` 租户回落。

use async_trait::async_trait;
use bytes::Bytes;

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
    /// sha256（小写 hex，64 位）。仅 `inspect_object`（下载后计算）提供；
    /// `head_object` 的元数据来源不含 hash（LMMS P1：checksum attestation）。
    pub sha256: Option<String>,
    pub detected_mime: String,
}

#[async_trait]
pub trait StoragePort: Send + Sync {
    /// 上传一个对象（服务身份代理）。返回 file-service 分配的 object id。
    async fn upload_object(
        &self,
        lmms_tid: i64,
        data: Bytes,
        filename: &str,
        content_type: Option<&str>,
    ) -> Result<String, StorageError>;

    /// 预检对象存在性与大小上限（不下载内容；sha256 未知）。
    async fn head_object(
        &self,
        lmms_tid: i64,
        object_id: &str,
    ) -> Result<ObjectInspection, StorageError>;

    /// 完整校验：下载对象并计算 sha256（commit 协议的"校验"步骤）。
    async fn inspect_object(
        &self,
        lmms_tid: i64,
        object_id: &str,
    ) -> Result<ObjectInspection, StorageError>;

    /// LMMS `POST /file/presign`（下载预签，短 TTL）。后端不支持时返回
    /// [`StorageError::Unavailable`]（诚实拒绝，不伪装）。
    async fn create_download_intent(
        &self,
        lmms_tid: i64,
        object_id: &str,
    ) -> Result<String, StorageError>;

    /// ART-01 §6 create_upload_intent：LMMS 无 upload-intent 协议（P1）。
    /// API 用户路径不代理上传（见模块注释），本方法保持明确不可用。
    async fn create_upload_intent(
        &self,
        tenant_id: uuid::Uuid,
        project_id: uuid::Uuid,
        suggested_name: &str,
    ) -> Result<UploadIntent, StorageError>;

    /// 将临时对象安全登记为不可变正式对象。LMMS 无 finalize 概念（上传即
    /// 正式，`is_temporary=false`）；本实现返回原 object id（no-op），
    /// P1 落地两段协议后替换为真实 finalize。
    async fn copy_or_finalize_object(
        &self,
        lmms_tid: i64,
        object_id: &str,
    ) -> Result<String, StorageError>;

    /// 删除对象（补偿：仅用于 AS 自身上传的输出对象；404 幂等视为成功）。
    async fn delete_object(&self, lmms_tid: i64, object_id: &str) -> Result<(), StorageError>;
}

// 对象面操作的 LMMS 数字租户 tid 约定：全部方法的 `lmms_tid` 来自委托
// claim 的追溯字段（仅用于出站令牌 mint 路由 + file-service 自身的租户
// scope 校验，**绝不参与 AS 授权判定或 scope 装配**——契约 §3.1 的
// lmms_tid 红线）。缺失/非法 → fail-closed（`StorageError::Unavailable`）。
