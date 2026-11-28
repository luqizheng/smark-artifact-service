//! 应用状态与依赖装配。

use artifact_application::{ArtifactService, DelegationVerifier, GeneratorService};
use artifact_infrastructure::delegation::ClosedDelegationVerifier;
use artifact_infrastructure::postgres::{
    PgArtifactRepository, PgAuditSink, PgGeneratorRepository, PgIdempotencyStore, PgJobRepository,
    PgKeyedArtifactCreator, PgKeyedGenerationJobCreator, PgReferenceRepository,
    PgUnkeyedArtifactCreator, PgVersionRepository,
};
use artifact_infrastructure::storage::ProtocolBlockedStorage;
use sqlx::PgPool;
use std::sync::Arc;

use crate::build_info::BuildInfo;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub artifacts: Arc<ArtifactService>,
    pub generators: Arc<GeneratorService>,
    pub verifier: Arc<dyn DelegationVerifier>,
    pub build_info: BuildInfo,
    /// 存储装配模式（诚实面，/v1/ready 上报）：
    /// `file_service_adapter` / `protocol_blocked_p0` / `protocol_blocked_config_invalid`。
    pub storage_mode: &'static str,
}

/// 生产装配：fail-closed 验证器 + 协议阻断存储端口。
///
/// 不存在任何运行时开关；测试替身仅存在于 `test-doubles` feature
/// （见 `artifact-infrastructure::test_support`），发布二进制不包含。
pub fn default_verifier() -> Arc<dyn DelegationVerifier> {
    Arc::new(ClosedDelegationVerifier::new())
}

/// 按配置装配委托验证器（WP 契约 v0.2 §4）：
/// * 清单为空（默认）→ [`ClosedDelegationVerifier`]：全拒，生产姿态不变；
/// * 非空 → [`AiPmDelegationVerifier`]：候选 B（AI-PM 签发，AS 以其 JWKS 验签）。
///
/// https 强制与 issuer 唯一性已由 `Config::from_env` 校验；这里只做组装。
pub fn verifier_from_config(
    issuers: &[crate::config::DelegationIssuerConfig],
) -> Arc<dyn DelegationVerifier> {
    if issuers.is_empty() {
        return default_verifier();
    }
    let trusted = issuers
        .iter()
        .map(|entry| artifact_infrastructure::delegation::TrustedIssuer {
            issuer: entry.issuer.clone(),
            jwks_url: entry.jwks_url.clone(),
        })
        .collect();
    match artifact_infrastructure::delegation::AiPmDelegationVerifier::new(trusted) {
        Ok(verifier) => Arc::new(verifier),
        // Config::from_env 已校验；不可达的防御分支仍按 fail-closed 处理。
        Err(reason) => {
            tracing::error!(%reason, "delegation issuer assembly failed; staying fail-closed");
            default_verifier()
        }
    }
}

pub fn app_state(pool: PgPool) -> AppState {
    app_state_with_verifier(pool, default_verifier())
}

/// 按配置装配存储端口（WP-AS-05）：
/// * file-service 配置齐备 → [`FileServiceStorage`]（真实适配器，
///   "上传 + 校验 + 补偿"近似，契约 v0.3 §12）；
/// * 否则 → [`ProtocolBlockedStorage`]：Version 链路保持 503 fail-closed
///   （诚实拒绝，绝不伪造成功）。
///
/// `bootstrap_token` 已由 `Config::from_env` 校验非空；URL https/回环规则
/// 在适配器构造时二次校验（失败回落 ProtocolBlocked 并记 error）。
pub fn storage_from_config(
    file: Option<&crate::config::FileServiceSettings>,
) -> (Arc<dyn artifact_application::StoragePort>, &'static str) {
    match file {
        Some(settings) => {
            let assembly = (|| {
                let tokens =
                    artifact_infrastructure::file_service::service_jwt::ServiceJwtMinter::new(
                        artifact_infrastructure::file_service::service_jwt::ServiceJwtConfig {
                            token_service_url: settings.token_service_url.clone(),
                            bootstrap_token: settings.bootstrap_token.clone(),
                            request_timeout: settings.request_timeout,
                            refresh_skew: artifact_infrastructure::file_service::service_jwt::DEFAULT_REFRESH_SKEW,
                        },
                    )
                    .map_err(|e| artifact_application::StorageError::Unavailable(e.to_string()))?;
                artifact_infrastructure::file_service::storage::FileServiceStorage::new(
                    artifact_infrastructure::file_service::storage::FileServiceConfig {
                        file_service_url: settings.file_service_url.clone(),
                        upload_max_bytes: settings.upload_max_bytes,
                        request_timeout: settings.request_timeout,
                    },
                    tokens,
                )
            })();
            match assembly {
                Ok(storage) => (Arc::new(storage), "file_service_adapter"),
                Err(reason) => {
                    tracing::error!(%reason, "file-service storage assembly failed; version paths stay 503");
                    (
                        Arc::new(ProtocolBlockedStorage::new()),
                        "protocol_blocked_config_invalid",
                    )
                }
            }
        }
        None => (
            Arc::new(ProtocolBlockedStorage::new()),
            "protocol_blocked_p0",
        ),
    }
}

/// 测试装配入口：仅集成测试传入 TestDelegationVerifier；
/// 生产 `main` 只调用 [`app_state`]（fail-closed）。
pub fn app_state_with_verifier(pool: PgPool, verifier: Arc<dyn DelegationVerifier>) -> AppState {
    let (storage, mode) = storage_from_config(None);
    app_state_with_deps(pool, verifier, storage, mode)
}

/// 测试装配入口（WP-AS-05）：注入指定存储端口（loopback fake）+ 验证器。
pub fn app_state_with_storage(
    pool: PgPool,
    verifier: Arc<dyn DelegationVerifier>,
    storage: Arc<dyn artifact_application::StoragePort>,
) -> AppState {
    app_state_with_deps(pool, verifier, storage, "file_service_adapter")
}

fn app_state_with_deps(
    pool: PgPool,
    verifier: Arc<dyn DelegationVerifier>,
    storage: Arc<dyn artifact_application::StoragePort>,
    storage_mode: &'static str,
) -> AppState {
    let artifacts = Arc::new(ArtifactService {
        repo: Arc::new(PgArtifactRepository { pool: pool.clone() }),
        versions: Arc::new(PgVersionRepository { pool: pool.clone() }),
        references: Arc::new(PgReferenceRepository { pool: pool.clone() }),
        jobs: Arc::new(PgJobRepository { pool: pool.clone() }),
        generators: Arc::new(PgGeneratorRepository { pool: pool.clone() }),
        storage,
        idempotency: Arc::new(PgIdempotencyStore { pool: pool.clone() }),
        keyed_create: Arc::new(PgKeyedArtifactCreator { pool: pool.clone() }),
        unkeyed_create: Arc::new(PgUnkeyedArtifactCreator { pool: pool.clone() }),
        keyed_jobs: Arc::new(PgKeyedGenerationJobCreator { pool: pool.clone() }),
        audit: Arc::new(PgAuditSink { pool: pool.clone() }),
    });
    let generators = Arc::new(GeneratorService {
        repo: Arc::new(PgGeneratorRepository { pool: pool.clone() }),
    });
    AppState {
        pool,
        artifacts,
        generators,
        verifier,
        build_info: crate::build_info::build_info(),
        storage_mode,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生产默认验证器必须无条件拒绝（deny-by-default，ART-02 §4.5）。
    #[tokio::test]
    async fn default_verifier_is_fail_closed() {
        let verifier = default_verifier();
        let err = verifier.verify("any-token").await;
        assert!(err.is_err(), "default verifier must reject every token");
    }
}
