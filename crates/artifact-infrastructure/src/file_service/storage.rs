//! `StoragePort` 的真实实现：LMMS file-service HTTP 适配器（WP-AS-05）。
//!
//! 契约依据：WP 契约 v0.3 §12 裁决 —— 在 LMMS upload-intent / finalize
//! 协议（P1）落地前，以"**上传 + 校验 + 补偿**"适配器近似：
//!
//! * **上传**（[`StoragePort::upload_object`]，服务身份代理）：multipart
//!   `POST /file/upload`，`is_temporary=false`（"上传即正式"，LMMS 无
//!   finalize —— no-op 见 [`StoragePort::copy_or_finalize_object`]）。
//! * **校验**：[`StoragePort::inspect_object`] 下载并计算 sha256 ——
//!   file-service 不计算 hash（`FileInfo.hash` 恒 None）。
//! * **补偿**：[`StoragePort::delete_object`]（404 幂等视为已删除）。
//!
//! # 身份与租户（A3A-06 §5.2 + F-24/P1-05 残余风险）
//!
//! 一切调用以 [`ServiceJwtMinter`] mint 的 `aud=file-service`、带 `tid` 的
//! 短时令牌发出（F-24：file-service 以 `unwrap_or(1)` 回落解析租户，无 tid
//! 令牌会被静默归入租户 1 —— 本适配器绝不发出无 tid 令牌）。`upload_object`
//! 的 `lmms_tid` 来自委托 claim 追溯字段（仅对象面路由，不参与 AS 授权）。
//!
//! # 残余风险（P1-05，记录在案）
//!
//! file-service 侧 `user_id` 自声明、租户信任基于 tid claim：AS 上传的
//! `user_id` 恒为 None（服务身份上传）；信任边界收敛依赖 LMMS 受信调用方
//! 契约（P1-05），本适配器不放大它（不传 user_id、不传 meta.tenant_id）。

use std::sync::Arc;
use std::time::Duration;

use artifact_application::{ObjectInspection, StorageError, StoragePort, UploadIntent};
use async_trait::async_trait;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::service_jwt::{ServiceJwtError, ServiceJwtMinter};

/// file-service 适配器配置。`Debug` 不含敏感值（本配置无敏感值，保持一致姿态）。
#[derive(Debug, Clone)]
pub struct FileServiceConfig {
    /// LMMS file-service 基址（如 `http://127.0.0.1:8083`）。
    pub file_service_url: String,
    /// AS 侧上传字节上限（与 file-service `max_file_size` 独立配置，
    /// 本侧先拒绝，避免大对象无谓出站）。
    pub upload_max_bytes: i64,
    /// 单次请求超时。
    pub request_timeout: Duration,
}

impl FileServiceConfig {
    pub fn validate(&self) -> Result<(), String> {
        validate_service_base_url(&self.file_service_url)?;
        if self.upload_max_bytes <= 0 {
            return Err("upload_max_bytes must be positive".into());
        }
        Ok(())
    }
}

pub struct FileServiceStorage {
    http: reqwest::Client,
    tokens: ServiceJwtMinter,
    config: Arc<FileServiceConfig>,
}

impl std::fmt::Debug for FileServiceStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileServiceStorage")
            .field("config", &self.config)
            .field("tokens", &"<minter>")
            .finish()
    }
}

/// LMMS `FileInfo` 的最小反序列化面（只取适配器需要的字段）。
#[derive(serde::Deserialize)]
struct FileObjectInfo {
    id: String,
    size: i64,
    #[serde(default)]
    content_type: Option<String>,
}

#[derive(serde::Deserialize)]
struct PresignResponse {
    url: String,
}

impl FileServiceStorage {
    pub fn new(config: FileServiceConfig, tokens: ServiceJwtMinter) -> Result<Self, StorageError> {
        config.validate().map_err(StorageError::Unavailable)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.request_timeout)
            .build()
            .map_err(|e| StorageError::Unavailable(format!("http client build failed: {e}")))?;
        Ok(Self {
            http,
            tokens,
            config: Arc::new(config),
        })
    }

    fn object_url(&self, object_id: &str) -> String {
        format!(
            "{}/file/{}",
            self.config.file_service_url.trim_end_matches('/'),
            object_id
        )
    }

    fn map_status_error(status: reqwest::StatusCode, op: &str) -> StorageError {
        match status {
            reqwest::StatusCode::NOT_FOUND => StorageError::ObjectNotFound,
            s => StorageError::Unavailable(format!("file-service {op} returned {s}")),
        }
    }

    async fn auth_token(&self, lmms_tid: i64) -> Result<String, StorageError> {
        self.tokens
            .file_service_token(lmms_tid)
            .await
            .map_err(map_jwt_error)
    }

    async fn info_object(
        &self,
        token: &str,
        object_id: &str,
    ) -> Result<FileObjectInfo, StorageError> {
        let url = format!("{}/info", self.object_url(object_id));
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| {
                StorageError::Unavailable(format!("file-service info request failed: {e}"))
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Self::map_status_error(status, "info"));
        }
        resp.json()
            .await
            .map_err(|e| StorageError::Unavailable(format!("file-service info parse failed: {e}")))
    }
}

fn map_jwt_error(e: ServiceJwtError) -> StorageError {
    match e {
        ServiceJwtError::AuthFailed(reason) => {
            StorageError::Unavailable(format!("service token auth failed (fail-closed): {reason}"))
        }
        ServiceJwtError::Unavailable(reason) => StorageError::Unavailable(reason),
    }
}

fn validate_service_base_url(raw: &str) -> Result<(), String> {
    let parsed = url::Url::parse(raw)
        .map_err(|e| format!("file_service_url '{raw}' is not a valid URL: {e}"))?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1");
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        _ => Err(format!(
            "file_service_url must be https (loopback http is allowed only for \
             127.0.0.1/localhost/[::1]); got '{raw}'"
        )),
    }
}

/// LMMS multipart 上传的 metadata 字段（UploadFileMeta 最小面）。
/// `user_id`/`tenant_id` 恒 None：服务身份上传，租户由 tid claim 权威决定。
#[derive(serde::Serialize)]
struct UploadMeta<'a> {
    filename: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    is_temporary: bool,
}

#[async_trait]
impl StoragePort for FileServiceStorage {
    async fn upload_object(
        &self,
        lmms_tid: i64,
        data: Bytes,
        filename: &str,
        content_type: Option<&str>,
    ) -> Result<String, StorageError> {
        if data.len() as i64 > self.config.upload_max_bytes {
            return Err(StorageError::Unavailable(format!(
                "upload exceeds AS-side limit ({} > {} bytes)",
                data.len(),
                self.config.upload_max_bytes
            )));
        }
        let token = self.auth_token(lmms_tid).await?;
        let meta = UploadMeta {
            filename,
            content_type,
            user_id: None,
            is_temporary: false,
        };
        let metadata_json = serde_json::to_string(&meta)
            .map_err(|e| StorageError::Unavailable(format!("metadata encode failed: {e}")))?;
        let part_file =
            reqwest::multipart::Part::bytes(data.to_vec()).file_name(filename.to_string());
        let form = reqwest::multipart::Form::new()
            .text("metadata", metadata_json)
            .part("file", part_file);
        let url = format!(
            "{}/file/upload",
            self.config.file_service_url.trim_end_matches('/')
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await
            .map_err(|e| StorageError::Unavailable(format!("file-service upload failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Self::map_status_error(status, "upload"));
        }
        let info: FileObjectInfo = resp.json().await.map_err(|e| {
            StorageError::Unavailable(format!("file-service upload parse failed: {e}"))
        })?;
        Ok(info.id)
    }

    async fn head_object(
        &self,
        lmms_tid: i64,
        object_id: &str,
    ) -> Result<ObjectInspection, StorageError> {
        let token = self.auth_token(lmms_tid).await?;
        let info = self.info_object(&token, object_id).await?;
        Ok(ObjectInspection {
            object_id: info.id,
            size_bytes: info.size,
            sha256: None,
            detected_mime: info.content_type.unwrap_or_default(),
        })
    }

    async fn inspect_object(
        &self,
        lmms_tid: i64,
        object_id: &str,
    ) -> Result<ObjectInspection, StorageError> {
        let token = self.auth_token(lmms_tid).await?;
        let info = self.info_object(&token, object_id).await?;
        // 校验 = 下载并计算 sha256（file-service 不提供 hash attestation）。
        let resp = self
            .http
            .get(self.object_url(object_id))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| StorageError::Unavailable(format!("file-service download failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Self::map_status_error(status, "download"));
        }
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await.map_err(|e| {
            StorageError::Unavailable(format!("file-service download read failed: {e}"))
        })?;
        if bytes.len() as i64 != info.size {
            return Err(StorageError::Unavailable(format!(
                "object size changed between info and download ({:?} != {:?})",
                info.size,
                bytes.len()
            )));
        }
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let sha256 = hex_lower(&hasher.finalize());
        let detected_mime = headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or(info.content_type)
            .unwrap_or_default();
        Ok(ObjectInspection {
            object_id: info.id,
            size_bytes: bytes.len() as i64,
            sha256: Some(sha256),
            detected_mime,
        })
    }

    async fn create_download_intent(
        &self,
        lmms_tid: i64,
        object_id: &str,
    ) -> Result<String, StorageError> {
        let token = self.auth_token(lmms_tid).await?;
        let url = format!(
            "{}/file/presign",
            self.config.file_service_url.trim_end_matches('/')
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({
                "file_id": object_id,
                "operation": "get",
                "expires_in_seconds": 600,
            }))
            .send()
            .await
            .map_err(|e| StorageError::Unavailable(format!("file-service presign failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Self::map_status_error(status, "presign"));
        }
        let parsed: PresignResponse = resp.json().await.map_err(|e| {
            StorageError::Unavailable(format!("file-service presign parse failed: {e}"))
        })?;
        Ok(parsed.url)
    }

    async fn create_upload_intent(
        &self,
        _tenant_id: Uuid,
        _project_id: Uuid,
        _suggested_name: &str,
    ) -> Result<UploadIntent, StorageError> {
        // 契约裁决（v0.3 §12）：LMMS 无 upload-intent 协议（P1）。API 用户
        // 路径不代理上传——调用方先自行上传对象，再以 source_object_id 创建
        // Version 会话。此方法在 P1 前保持明确不可用（诚实拒绝）。
        Err(StorageError::Unavailable(
            "upload-intent protocol is an LMMS P1 requirement; clients upload objects \
             to file-service directly and reference them via source_object_id"
                .into(),
        ))
    }

    async fn copy_or_finalize_object(
        &self,
        _lmms_tid: i64,
        object_id: &str,
    ) -> Result<String, StorageError> {
        // LMMS 无 finalize（上传即正式，is_temporary=false）。P1 两段协议
        // 落地后替换为真实 finalize；当前 no-op 返回原 id。
        Ok(object_id.to_string())
    }

    async fn delete_object(&self, lmms_tid: i64, object_id: &str) -> Result<(), StorageError> {
        let token = self.auth_token(lmms_tid).await?;
        let resp = self
            .http
            .delete(self.object_url(object_id))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| StorageError::Unavailable(format!("file-service delete failed: {e}")))?;
        let status = resp.status();
        match status {
            // 补偿幂等：对象已不存在 = 目标状态已达成。
            reqwest::StatusCode::NOT_FOUND => Ok(()),
            s if s.is_success() => Ok(()),
            s => Err(Self::map_status_error(s, "delete")),
        }
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation_rejects_remote_http() {
        let cfg = FileServiceConfig {
            file_service_url: "http://file-service.internal:8083".into(),
            upload_max_bytes: 1024,
            request_timeout: Duration::from_secs(5),
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_validation_accepts_https_and_loopback() {
        for raw in [
            "https://file-service.internal",
            "http://127.0.0.1:8083",
            "http://localhost:8083",
        ] {
            let cfg = FileServiceConfig {
                file_service_url: raw.into(),
                upload_max_bytes: 1024,
                request_timeout: Duration::from_secs(5),
            };
            assert!(cfg.validate().is_ok(), "{raw} must be accepted");
        }
    }

    #[test]
    fn config_validation_rejects_non_positive_limit() {
        let cfg = FileServiceConfig {
            file_service_url: "http://127.0.0.1:8083".into(),
            upload_max_bytes: 0,
            request_timeout: Duration::from_secs(5),
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn sha256_hex_is_lowercase_64() {
        let digest = Sha256::digest(b"hello");
        let hex = hex_lower(&digest);
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, hex.to_lowercase());
    }
}
