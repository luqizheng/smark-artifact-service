//! Artifact Service Rust SDK（ART-01 §10.1）。
//!
//! 只依赖本仓库 `artifact-contract`（稳定 API DTO）与 HTTP 栈；
//! 不依赖 pm-domain / pm-application / pm-infrastructure。
//! 兼容性由 Consumer Contract Tests（`tests/sdk_contract.rs`）保证。

use artifact_contract::{
    ArtifactPage, ArtifactResponse, CreateArtifactRequest, CreateGenerationJobRequest,
    CreateReferenceRequest, GenerationJobResponse, HealthResponse, ListArtifactsQuery,
    ReadyResponse, ReferenceResponse, SetCurrentVersionRequest, UpdateArtifactRequest,
};
use reqwest::header::{ACCEPT, HeaderMap as ReqHeaderMap, HeaderValue};
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use std::time::Duration;
use uuid::Uuid;

pub use artifact_contract as contract;

/// SDK 层错误：携带稳定 code / status，供调用方按 retryable 决策。
#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    /// 服务返回的错误信封（4xx/5xx）。
    #[error("artifact service error {status} {code}: {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
        request_id: Option<String>,
        retryable: bool,
    },
    /// 网络传输/超时错误（可重试）。
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("unexpected response (status {status}): {body}")]
    Unexpected { status: u16, body: String },
}

impl SdkError {
    pub fn code(&self) -> &str {
        match self {
            SdkError::Api { code, .. } => code,
            SdkError::Transport(_) => "TRANSPORT_ERROR",
            SdkError::Unexpected { .. } => "UNEXPECTED_RESPONSE",
        }
    }

    pub fn retryable(&self) -> bool {
        match self {
            SdkError::Api { retryable, .. } => *retryable,
            SdkError::Transport(_) => true,
            SdkError::Unexpected { .. } => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactClientConfig {
    pub base_url: String,
    pub timeout: Duration,
    pub connect_timeout: Duration,
}

impl ArtifactClientConfig {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(5),
        }
    }
}

/// 凭证注入由调用方显式提供（SDK 不缓存、不刷新任何凭证）。
#[derive(Debug, Clone, Default)]
pub struct CallContext {
    pub bearer_token: Option<String>,
    pub project_id: Option<Uuid>,
    pub idempotency_key: Option<String>,
    pub if_match: Option<String>,
}

impl CallContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    pub fn project(mut self, project_id: Uuid) -> Self {
        self.project_id = Some(project_id);
        self
    }

    pub fn idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    pub fn if_match(mut self, etag: impl Into<String>) -> Self {
        self.if_match = Some(etag.into());
        self
    }
}

#[derive(Clone)]
pub struct ArtifactClient {
    base_url: String,
    http: Client,
}

impl ArtifactClient {
    pub fn new(config: ArtifactClientConfig) -> Result<Self, SdkError> {
        let http = Client::builder()
            .timeout(config.timeout)
            .connect_timeout(config.connect_timeout)
            .build()?;
        Ok(Self {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            http,
        })
    }

    /// 无凭证只读面：健康、就绪、Generator 注册表。
    pub async fn health(&self) -> Result<HealthResponse, SdkError> {
        self.get("/v1/health", &CallContext::new()).await
    }

    pub async fn ready(&self) -> Result<ReadyResponse, SdkError> {
        self.get("/v1/ready", &CallContext::new()).await
    }

    pub async fn generators(&self) -> Result<Vec<artifact_contract::GeneratorResponse>, SdkError> {
        #[derive(serde::Deserialize)]
        struct Generators {
            generators: Vec<artifact_contract::GeneratorResponse>,
        }
        let wrapped: Generators = self.get("/v1/generators", &CallContext::new()).await?;
        Ok(wrapped.generators)
    }

    // ── Artifact（需委托凭证） ─────────────────────────────────────────

    pub async fn create_artifact(
        &self,
        request: CreateArtifactRequest,
        ctx: &CallContext,
    ) -> Result<ArtifactResponse, SdkError> {
        self.post_json("/v1/artifacts", &request, ctx).await
    }

    pub async fn get_artifact(
        &self,
        artifact_id: Uuid,
        ctx: &CallContext,
    ) -> Result<ArtifactResponse, SdkError> {
        self.get(&format!("/v1/artifacts/{artifact_id}"), ctx).await
    }

    pub async fn list_artifacts(
        &self,
        query: ListArtifactsQuery,
        ctx: &CallContext,
    ) -> Result<ArtifactPage, SdkError> {
        let _ = ctx.project_id.ok_or_else(|| SdkError::Unexpected {
            status: 0,
            body: "list_artifacts requires CallContext.project_id".into(),
        })?;
        let mut url = reqwest::Url::parse(&format!("{}/v1/artifacts", self.base_url))
            .expect("base url is valid");
        if let Some(limit) = query.limit {
            url.query_pairs_mut()
                .append_pair("limit", &limit.to_string());
        }
        if let Some(cursor) = query.cursor {
            url.query_pairs_mut().append_pair("cursor", &cursor);
        }
        if let Some(t) = query.artifact_type {
            url.query_pairs_mut().append_pair("artifact_type", &t);
        }
        if let Some(s) = query.lifecycle_state {
            url.query_pairs_mut().append_pair("lifecycle_state", &s);
        }
        let res = self
            .request(Method::GET, url.as_str(), ctx, None::<&serde_json::Value>)
            .await?;
        self.parse(res).await
    }

    pub async fn patch_artifact(
        &self,
        artifact_id: Uuid,
        request: UpdateArtifactRequest,
        ctx: &CallContext,
    ) -> Result<ArtifactResponse, SdkError> {
        self.patch_json(&format!("/v1/artifacts/{artifact_id}"), &request, ctx)
            .await
    }

    pub async fn set_current_version(
        &self,
        artifact_id: Uuid,
        request: SetCurrentVersionRequest,
        ctx: &CallContext,
    ) -> Result<ArtifactResponse, SdkError> {
        self.post_json(
            &format!("/v1/artifacts/{artifact_id}/current-version"),
            &request,
            ctx,
        )
        .await
    }

    pub async fn create_reference(
        &self,
        artifact_id: Uuid,
        request: CreateReferenceRequest,
        ctx: &CallContext,
    ) -> Result<ReferenceResponse, SdkError> {
        self.post_json(
            &format!("/v1/artifacts/{artifact_id}/references"),
            &request,
            ctx,
        )
        .await
    }

    pub async fn remove_reference(
        &self,
        artifact_id: Uuid,
        reference_id: Uuid,
        ctx: &CallContext,
    ) -> Result<(), SdkError> {
        let res = self
            .request(
                Method::DELETE,
                &format!(
                    "{}/v1/artifacts/{artifact_id}/references/{reference_id}",
                    self.base_url
                ),
                ctx,
                None::<&serde_json::Value>,
            )
            .await?;
        expect_empty(res).await
    }

    pub async fn request_generation(
        &self,
        artifact_id: Uuid,
        request: CreateGenerationJobRequest,
        ctx: &CallContext,
    ) -> Result<GenerationJobResponse, SdkError> {
        self.post_json(
            &format!("/v1/artifacts/{artifact_id}/generation-jobs"),
            &request,
            ctx,
        )
        .await
    }

    // ── HTTP 基元 ──────────────────────────────────────────────────────

    async fn get<T: DeserializeOwned>(&self, path: &str, ctx: &CallContext) -> Result<T, SdkError> {
        let res = self
            .request(
                Method::GET,
                &format!("{}{path}", self.base_url),
                ctx,
                None::<&serde_json::Value>,
            )
            .await?;
        self.parse(res).await
    }

    async fn post_json<T: DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
        ctx: &CallContext,
    ) -> Result<T, SdkError> {
        let res = self
            .request(
                Method::POST,
                &format!("{}{path}", self.base_url),
                ctx,
                Some(body),
            )
            .await?;
        self.parse(res).await
    }

    async fn patch_json<T: DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
        ctx: &CallContext,
    ) -> Result<T, SdkError> {
        let res = self
            .request(
                Method::PATCH,
                &format!("{}{path}", self.base_url),
                ctx,
                Some(body),
            )
            .await?;
        self.parse(res).await
    }

    async fn request<B: serde::Serialize>(
        &self,
        method: Method,
        url: &str,
        ctx: &CallContext,
        body: Option<&B>,
    ) -> Result<reqwest::Response, SdkError> {
        let mut headers = ReqHeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(project) = ctx.project_id
            && let Ok(value) = HeaderValue::from_str(&project.to_string())
        {
            headers.insert("x-project-id", value);
        }
        if let Some(key) = &ctx.idempotency_key
            && let Ok(value) = HeaderValue::from_str(key)
        {
            headers.insert("idempotency-key", value);
        }
        if let Some(etag) = &ctx.if_match
            && let Ok(value) = HeaderValue::from_str(etag)
        {
            headers.insert("if-match", value);
        }
        let mut builder = self.http.request(method, url).headers(headers);
        if let Some(token) = &ctx.bearer_token {
            builder = builder.bearer_auth(token);
        }
        if let Some(body) = body {
            builder = builder.json(body);
        }
        Ok(builder.send().await?)
    }

    async fn parse<T: DeserializeOwned>(&self, res: reqwest::Response) -> Result<T, SdkError> {
        let status = res.status();
        if !status.is_success() {
            return Err(self.error_from_response(res).await);
        }
        let bytes = res.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|e| SdkError::Unexpected {
            status: status.as_u16(),
            body: format!("response deserialization failed: {e}"),
        })
    }

    async fn error_from_response(&self, res: reqwest::Response) -> SdkError {
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        match serde_json::from_str::<artifact_contract::ErrorEnvelope>(&body) {
            Ok(envelope) => SdkError::Api {
                status,
                code: envelope.error.code,
                message: envelope.error.message,
                request_id: envelope.error.request_id,
                retryable: envelope.error.retryable,
            },
            Err(_) => SdkError::Unexpected { status, body },
        }
    }
}

async fn expect_empty(res: reqwest::Response) -> Result<(), SdkError> {
    let status = res.status();
    if status.is_success() || status == StatusCode::NO_CONTENT {
        return Ok(());
    }
    // 204 之外的成功/失败都可能带错误信封（如 404 RESOURCE_NOT_FOUND）。
    let status_code = status.as_u16();
    let body = res.text().await.unwrap_or_default();
    match serde_json::from_str::<artifact_contract::ErrorEnvelope>(&body) {
        Ok(envelope) => Err(SdkError::Api {
            status: status_code,
            code: envelope.error.code,
            message: envelope.error.message,
            request_id: envelope.error.request_id,
            retryable: envelope.error.retryable,
        }),
        Err(_) => Err(SdkError::Unexpected {
            status: status_code,
            body,
        }),
    }
}
