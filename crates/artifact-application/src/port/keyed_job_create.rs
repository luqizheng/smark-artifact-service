//! GenerationJob keyed-create 端口（ART-04G v3 §5.3 / §9 / §10.1）。
//!
//! 与 Artifact keyed-create（AS-1）同构：这个端口的实现必须拥有**完整**的
//! 数据库事务——caller identity 解析、Idempotency-Key 冲突硬门禁、generator
//! 可用性检查、INSERT 与并发仲裁全部在单个 PostgreSQL 事务（受控函数）内完成。
//! Application Service 不得在端口外再包一层事务；端口契约不暴露
//! `sqlx::Transaction` 或行类型。
//!
//! 错误分层（ART-04G v3 §9.5 / ART-04I §4.4）：数据库稳定消息 token 在
//! Infrastructure 侧映射为本枚举（Terminal / Retryable），HTTP status 由
//! `to_domain_error()` → artifact-contract 的稳定错误码映射决定；数据库
//! token 不直接决定 HTTP status。

use serde_json::Value;
use uuid::Uuid;

use artifact_domain::{DomainError, GenerationJob, TenantScope};

use crate::service::creation_fingerprint::normalize_text;
use crate::service::generation_fingerprint::{JobFingerprintError, canonical_job_fingerprint};

/// 已通过校验并完成规范化的 GenerationJob 创建请求（ART-04G v3 §9.4.1）。
///
/// 指纹必须基于本类型计算（即「验证和规范化后的 DTO」），而不是原始 HTTP 请求体。
/// `generator_id` 在此完成 Unicode NFC；`input`/`options` 内容保持原始字节语义，
/// 不做 NFC。`options` 在 DTO 层缺省与 `null` 等价（归一为 `None`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedCreateGenerationJobRequest {
    pub tenant_id: Uuid,
    pub project_id: Uuid,
    pub artifact_id: Uuid,
    /// Durable caller identity；`None` = legacy 未键控语义（R-1..R-4 路径）。
    pub caller_request_id: Option<Uuid>,
    /// 已 NFC 规范化。
    pub generator_id: String,
    /// payload 原始字节语义（不做 NFC）。
    pub input: Value,
    /// DTO 层 absent ≡ null；`Some(null)` 已归一为 `None`。
    pub options: Option<Value>,
    /// input snapshot 的 schema 版本（进入 input_sha256，不进入指纹）。
    pub input_schema_version: String,
}

impl ValidatedCreateGenerationJobRequest {
    /// 校验并规范化。任何失败都不会进入持久化路径。
    ///
    /// `envelope_bytes` 是原始请求体大小（HTTP 层测量），用于与
    /// `validate_job_input_envelope` 一致的体积校验。
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        scope: &TenantScope,
        artifact_id: Uuid,
        caller_request_id: Option<Uuid>,
        generator_id: String,
        input: Value,
        options: Option<Value>,
        input_schema_version: String,
        envelope_bytes: usize,
    ) -> Result<Self, DomainError> {
        let generator_id = normalize_text(&generator_id);
        if generator_id.is_empty() {
            return Err(DomainError::InvalidRequest(
                "generator_id must not be blank".into(),
            ));
        }
        if generator_id.chars().count() > 128 {
            return Err(DomainError::InvalidRequest(
                "generator_id must be at most 128 characters".into(),
            ));
        }
        if !input.is_object() {
            return Err(DomainError::InvalidRequest(
                "input must be a JSON object".into(),
            ));
        }
        // DTO 层：options 缺省与 null 等价；Some(null) → None。
        let options = match options {
            Some(Value::Null) => None,
            other => other,
        };
        if options.as_ref().is_some_and(|opts| !opts.is_object()) {
            return Err(DomainError::InvalidRequest(
                "options must be a JSON object when present".into(),
            ));
        }
        let input_schema_version = input_schema_version.trim().to_string();
        if input_schema_version.is_empty() || input_schema_version.chars().count() > 32 {
            return Err(DomainError::InvalidRequest(
                "input_schema_version must be 1..=32 characters".into(),
            ));
        }
        crate::validate_job_input_envelope(envelope_bytes, &input, options.as_ref())
            .map_err(|err| DomainError::InvalidRequest(err.to_string()))?;

        Ok(Self {
            tenant_id: scope.tenant_id,
            project_id: scope.project_id,
            artifact_id,
            caller_request_id,
            generator_id,
            input,
            options,
            input_schema_version,
        })
    }

    pub fn scope(&self) -> TenantScope {
        TenantScope::new(self.tenant_id, self.project_id)
    }

    /// 按指定版本计算 caller 语义指纹（ART-04G v3 §5.2）。
    /// `caller_request_id` 为 `None` 时不存在指纹（legacy 路径）。
    pub fn fingerprint(&self, version: i16) -> Result<Option<String>, JobFingerprintError> {
        match self.caller_request_id {
            None => Ok(None),
            Some(_) => Ok(Some(canonical_job_fingerprint(version, self)?)),
        }
    }
}

/// 端口的成功结果（ART-04G v3 §5.5 resolution 三值）。
#[derive(Debug, Clone)]
pub enum KeyedJobCreateOutcome {
    /// 本次事务创建了新的 Job。
    Created(GenerationJob),
    /// caller_request_id + 指纹命中已有 Job；零写入重放。
    ResolvedByCallerRequest(GenerationJob),
    /// Idempotency-Key + raw hash 命中已有 Job（legacy 语义）。
    ResolvedByIdempotency(GenerationJob),
}

/// 端口的失败结果。只有 `RetryablePersistenceFailure` 建议客户端重试；
/// 完整性错误（FingerprintVersionMismatch / IdempotencyKeyCrossIdentity）
/// 必须失败关闭并上报告警信号，绝不盲目重试或重放（ART-04G v3 §9.2.2）。
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum KeyedJobCreateError {
    /// caller_request_id 已绑定到指纹不同的操作（23505，409）。
    #[error("caller request id already bound to a different operation")]
    CallerRequestIdReused,
    /// 存储的 fingerprint_version 与传入不一致（P0001，5xx，不可重试）。
    #[error("stored fingerprint version does not match: {0}")]
    FingerprintVersionMismatch(String),
    /// Idempotency-Key 与 caller 身份交叉冲突（P0001，5xx，独立语义）。
    #[error("idempotency key cross identity conflict: {0}")]
    IdempotencyKeyCrossIdentity(String),
    /// Idempotency-Key 被不同 raw body 复用（23505，409）。
    #[error("idempotency key was reused with a different request body")]
    IdempotencyConflict,
    /// generator 不在注册表中（22023，400）。
    #[error("unknown generator")]
    UnknownGenerator,
    /// generator 已注册但当前不可用（P0001，503）。
    #[error("generator is registered but currently unavailable")]
    GeneratorUnavailable,
    /// 请求校验失败（22023/22001 族，400）。
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Idempotency-Key 规范化后非法（22023，400；ART-04G v3 §10.1）。
    #[error("invalid idempotency key: {0}")]
    InvalidIdempotencyKey(String),
    #[error("retryable persistence failure: {0}")]
    RetryablePersistenceFailure(String),
    /// 结构不变量破坏或未知数据库状态。必须失败关闭并上报完整性信号。
    #[error("persistence integrity failure: {0}")]
    IntegrityFailure(String),
}

impl KeyedJobCreateError {
    /// HTTP 映射的唯一入口（ART-04G v3 §9.5 + ART-04I §4.4 冻结映射）。
    pub fn to_domain_error(&self) -> DomainError {
        match self {
            Self::CallerRequestIdReused => DomainError::CallerRequestIdReused(
                "this caller_request_id is already bound to a different operation".into(),
            ),
            Self::FingerprintVersionMismatch(message) => {
                DomainError::FingerprintVersionMismatch(message.clone())
            }
            Self::IdempotencyKeyCrossIdentity(message) => {
                DomainError::IdempotencyKeyCrossIdentity(message.clone())
            }
            Self::IdempotencyConflict => DomainError::IdempotencyKeyReused(
                "this Idempotency-Key was already used with a different request body".into(),
            ),
            Self::UnknownGenerator => DomainError::InvalidRequest("unknown generator id".into()),
            Self::GeneratorUnavailable => DomainError::GeneratorUnavailable(
                "generator is registered but currently unavailable".into(),
            ),
            Self::InvalidRequest(message) => DomainError::InvalidRequest(message.clone()),
            Self::InvalidIdempotencyKey(message) => {
                DomainError::InvalidIdempotencyKey(message.clone())
            }
            Self::RetryablePersistenceFailure(message) => {
                DomainError::PersistenceUnavailable(message.clone())
            }
            Self::IntegrityFailure(message) => DomainError::Internal(message.clone()),
        }
    }
}

/// GenerationJob keyed create/resolve 的唯一入口（ART-04G v3 §5.3）。
///
/// `request_hash` 是传输层对原始 HTTP 请求体的 SHA-256（小写 64 位十六进制）：
/// 它是 Idempotency-Key 冲突判定的依据，即使请求未携带 Idempotency-Key 也必须
/// 提供（受控函数要求）。它与 `request_fingerprint`（caller 业务操作身份）和
/// `input_sha256`（持久化输入快照哈希）是三个不同语义，绝不混用。
#[async_trait::async_trait]
pub trait KeyedGenerationJobCreatePort: Send + Sync {
    /// 在**一个** PostgreSQL 事务（受控函数）内完成：
    /// key 规范化与校验 → caller identity 解析 + key holder 硬门禁 →
    /// Idempotency-Key 解析 → generator 检查 → INSERT → 并发唯一约束仲裁。
    ///
    /// 并发唯一约束冲突时必须在子事务回滚后以新语句快照重读竞争胜者；
    /// 无法确认合法结果时失败关闭。
    async fn create_or_resolve_keyed_job(
        &self,
        request: ValidatedCreateGenerationJobRequest,
        request_hash: String,
        idempotency_key: Option<String>,
    ) -> Result<KeyedJobCreateOutcome, KeyedJobCreateError>;
}
