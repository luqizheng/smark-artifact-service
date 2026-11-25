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
