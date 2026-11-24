//! 集成测试公共设施：进程内启动 API、测试委托登记、固定 fixture 作用域。
// 每个 [[test]] 目标各自包含本模块，未用到的助手按目标触发 dead_code；
// 测试工具模块整体豁免。
#![allow(dead_code)]

use artifact_application::DelegationVerifier;
use artifact_domain::{Action, ActorType};
use artifact_infrastructure::test_support::TestDelegationVerifier;
use std::sync::Arc;
use uuid::Uuid;

/// 固定 fixture 作用域（每个测试库独立，无冲突风险）。
pub const TENANT: Uuid = Uuid::from_u128(0x0001);
pub const PROJECT_1: Uuid = Uuid::from_u128(0x0101);
pub const PROJECT_2: Uuid = Uuid::from_u128(0x0102);

/// 进程内启动 API（独立端口），返回 base_url。
pub async fn spawn_app(pool: sqlx::PgPool, verifier: Arc<dyn DelegationVerifier>) -> String {
    let state = artifact_api::app_state_with_verifier(pool, verifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, artifact_api::router(state))
            .await
            .expect("server task");
    });
    format!("http://{addr}")
}

/// 默认 fail-closed 装配（用于默认拒绝测试）。
pub async fn spawn_app_closed(pool: sqlx::PgPool) -> String {
    spawn_app(pool, artifact_api::default_verifier()).await
}

/// 建一个登记了指定动作的测试委托验证器。
pub fn verifier_with(tokens: &[(&str, Uuid, Uuid, &[Action])]) -> Arc<TestDelegationVerifier> {
    let mut verifier = TestDelegationVerifier::new();
    for (token, tenant, project, actions) in tokens {
        verifier.allow(
            *token,
            *tenant,
            *project,
            "test-user",
            ActorType::User,
            actions,
        );
    }
    Arc::new(verifier)
}

pub fn read_actions() -> &'static [Action] {
    &[Action::ArtifactRead, Action::ArtifactCreate]
}

pub fn all_actions() -> Vec<Action> {
    vec![
        Action::ArtifactRead,
        Action::ArtifactCreate,
        Action::ArtifactUpdateMetadata,
        Action::VersionCreate,
        Action::VersionRead,
        Action::VersionCommit,
        Action::VersionSetCurrent,
        Action::ReferenceWrite,
        Action::ReferenceRemove,
        Action::GenerationRequest,
        Action::GenerationRead,
        Action::GenerationCancel,
    ]
}

/// 直接 SQL 建一个 artifact（绕过 API，供不变量测试布景）。
pub async fn sql_insert_artifact(
    pool: &sqlx::PgPool,
    tenant: Uuid,
    project: Uuid,
    name: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO artifact.artifact (id, tenant_id, project_id, name, artifact_type, created_by) \
         VALUES ($1, $2, $3, $4, 'visualization', 'fixture')",
    )
    .bind(id)
    .bind(tenant)
    .bind(project)
    .bind(name)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// 直接 SQL 建一个版本（state 可选 pending/ready）。
pub async fn sql_insert_version(
    pool: &sqlx::PgPool,
    tenant: Uuid,
    project: Uuid,
    artifact_id: Uuid,
    state: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    match state {
        "ready" => {
            sqlx::query(
                "INSERT INTO artifact.artifact_version \
                 (id, tenant_id, project_id, artifact_id, state, manifest, ready_at, created_by) \
                 VALUES ($1, $2, $3, $4, 'ready', '{}', now(), 'fixture')",
            )
            .bind(id)
            .bind(tenant)
            .bind(project)
            .bind(artifact_id)
            .execute(pool)
            .await
            .unwrap();
        }
        _ => {
            sqlx::query(
                "INSERT INTO artifact.artifact_version \
                 (id, tenant_id, project_id, artifact_id, state, manifest, created_by) \
                 VALUES ($1, $2, $3, $4, 'pending', '{}', 'fixture')",
            )
            .bind(id)
            .bind(tenant)
            .bind(project)
            .bind(artifact_id)
            .execute(pool)
            .await
            .unwrap();
        }
    }
    id
}

pub fn pg_error_code(err: &sqlx::Error) -> Option<String> {
    if let sqlx::Error::Database(db) = err {
        return db.code().map(|c| c.to_string());
    }
    None
}
