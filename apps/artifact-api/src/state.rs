//! 应用状态与依赖装配。

use artifact_application::{ArtifactService, DelegationVerifier, GeneratorService};
use artifact_infrastructure::delegation::ClosedDelegationVerifier;
use artifact_infrastructure::postgres::{
    PgArtifactRepository, PgAuditSink, PgGeneratorRepository, PgIdempotencyStore, PgJobRepository,
    PgKeyedArtifactCreator, PgReferenceRepository, PgVersionRepository,
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
            audience: entry.audience.clone(),
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

/// 测试装配入口：仅集成测试传入 TestDelegationVerifier；
/// 生产 `main` 只调用 [`app_state`]（fail-closed）。
pub fn app_state_with_verifier(pool: PgPool, verifier: Arc<dyn DelegationVerifier>) -> AppState {
    let artifacts = Arc::new(ArtifactService {
        repo: Arc::new(PgArtifactRepository { pool: pool.clone() }),
        versions: Arc::new(PgVersionRepository { pool: pool.clone() }),
        references: Arc::new(PgReferenceRepository { pool: pool.clone() }),
        jobs: Arc::new(PgJobRepository { pool: pool.clone() }),
        generators: Arc::new(PgGeneratorRepository { pool: pool.clone() }),
        storage: Arc::new(ProtocolBlockedStorage::new()),
        idempotency: Arc::new(PgIdempotencyStore { pool: pool.clone() }),
        keyed_create: Arc::new(PgKeyedArtifactCreator { pool: pool.clone() }),
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
