//! 集成测试公共设施：进程内启动 API、测试委托登记、固定 fixture 作用域。
// 每个 [[test]] 目标各自包含本模块，未用到的助手按目标触发 dead_code；
// 测试工具模块整体豁免。
#![allow(dead_code)]

use artifact_application::{
    AdapterCapabilities, DelegationVerifier, canonical_request_fingerprint,
};
use artifact_domain::{Action, Actor, ActorType, Delegation, TenantScope};
use artifact_infrastructure::test_support::{TestDelegationVerifier, TestTokenBinding};
use sha2::Digest;
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

/// 固定 fixture 作用域（每个测试库独立，无冲突风险）。
pub const TENANT: Uuid = Uuid::from_u128(0x0001);
pub const PROJECT_1: Uuid = Uuid::from_u128(0x0101);
pub const PROJECT_2: Uuid = Uuid::from_u128(0x0102);
/// 第二租户（跨租户隔离测试用）及其项目。
pub const TENANT_OTHER: Uuid = Uuid::from_u128(0x0002);
pub const PROJECT_OTHER: Uuid = Uuid::from_u128(0x0201);
/// LMMS 数字租户镜像（契约 v0.2 P0-01，仅审计语义）。
pub const LMMS_TID: i64 = 42;
/// /v1/generators 授权测试用替身凭证（WP-03：该端点要求 generation:read）。
pub const GENERATORS_TOKEN: &str = "test-generators-token";

/// 构造一个（可选携带操作绑定的）测试 Delegation，供声明层一致性判定使用。
pub fn delegation_for(
    tenant_id: Uuid,
    project_id: Uuid,
    caller_request_id: Option<Uuid>,
    request_hash: Option<&str>,
) -> Delegation {
    Delegation {
        delegation_id: Uuid::new_v4().to_string(),
        jti: Uuid::new_v4().to_string(),
        scope: TenantScope::new(tenant_id, project_id),
        actor: Actor::new("user-1", ActorType::User),
        actions: HashSet::from([Action::ArtifactCreate, Action::ArtifactRead]),
        expires_at: chrono::Utc::now() + chrono::Duration::seconds(300),
        lmms_tid: Some(LMMS_TID.to_string()),
        artifact_id: None,
        operation_id: None,
        caller_request_id,
        request_hash: request_hash.map(str::to_string),
    }
}

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

/// WP-AS-05：注入指定存储端口（loopback fake file-service 适配器）启动 API。
pub async fn spawn_app_with_storage(
    pool: sqlx::PgPool,
    verifier: Arc<dyn DelegationVerifier>,
    storage: Arc<dyn artifact_application::StoragePort>,
) -> String {
    let state = artifact_api::app_state_with_storage(pool, verifier, storage);
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
    let verifier = TestDelegationVerifier::new();
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

/// v1 指纹（测试侧计算；AS 服务层独立重算，两者一致即跨层证明，WP-04 §5.3）。
pub fn v1_fingerprint(method: &str, path: &str, caller: Uuid, body: &serde_json::Value) -> String {
    canonical_request_fingerprint(method, path, Some(caller), body)
}

/// 为具体写请求铸造一枚 V10 绑定 token（caller 随机 + v1 指纹），返回 caller。
/// `body` 必须与随后 HTTP 请求发送的序列化体完全一致（用
/// `serde_json::to_value(&dto)` 构造，保证未知字段/null 语义对齐）。
#[allow(clippy::too_many_arguments)]
pub fn mint_write_token(
    verifier: &Arc<TestDelegationVerifier>,
    token: &str,
    tenant: Uuid,
    project: Uuid,
    actions: &[Action],
    method: &str,
    path: &str,
    body: &serde_json::Value,
) -> Uuid {
    let caller = Uuid::new_v4();
    let hash = v1_fingerprint(method, path, caller, body);
    verifier.allow_with_binding(
        token,
        tenant,
        project,
        "test-user",
        ActorType::User,
        actions,
        TestTokenBinding {
            artifact_id: None,
            operation_id: None,
            caller_request_id: Some(caller),
            request_hash: Some(hash),
        },
    );
    caller
}

/// 为具体写请求铸造一枚 V10 绑定 + lmms_tid 追溯 claim 的 token（WP-AS-05）。
#[allow(clippy::too_many_arguments)]
pub fn mint_write_token_with_tid(
    verifier: &Arc<TestDelegationVerifier>,
    token: &str,
    tenant: Uuid,
    project: Uuid,
    actions: &[Action],
    method: &str,
    path: &str,
    body: &serde_json::Value,
    lmms_tid: &str,
) -> Uuid {
    let caller = Uuid::new_v4();
    let hash = v1_fingerprint(method, path, caller, body);
    verifier.allow_with_tid_binding(
        token,
        tenant,
        project,
        "test-user",
        ActorType::User,
        actions,
        TestTokenBinding {
            artifact_id: None,
            operation_id: None,
            caller_request_id: Some(caller),
            request_hash: Some(hash),
        },
        Some(lmms_tid),
    );
    caller
}

/// 完全受控的绑定登记（负面测试：缺 hash / 错 hash / artifact_id / operation_id）。
#[allow(clippy::too_many_arguments)]
pub fn mint_bound_token(
    verifier: &Arc<TestDelegationVerifier>,
    token: &str,
    tenant: Uuid,
    project: Uuid,
    actions: &[Action],
    binding: TestTokenBinding,
) {
    verifier.allow_with_binding(
        token,
        tenant,
        project,
        "test-user",
        ActorType::User,
        actions,
        binding,
    );
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

/// 数据库错误消息（受控函数用稳定的异常消息表达拒绝原因）。
pub fn pg_error_message(err: &sqlx::Error) -> String {
    match err.as_database_error() {
        Some(db) => db.message().to_string(),
        None => err.to_string(),
    }
}

/// 角色是集群级对象，sqlx::test 清理测试库时不会带走它们。每个建角色的测试结束时
/// 必须自行删除，否则每运行一次就会在集群里留下一批带随机名的角色。
pub async fn drop_test_roles(pool: &sqlx::PgPool, roles: &[&str]) {
    for role in roles {
        // DROP OWNED 清掉该角色在当前库拥有的对象与获得的授权，否则 DROP ROLE 会被依赖挡住。
        let _ = sqlx::query(&format!("DROP OWNED BY \"{role}\""))
            .execute(pool)
            .await;
        sqlx::query(&format!("DROP ROLE \"{role}\""))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("test role {role} must be removable: {e}"));
    }
}

pub fn sha256_hex(text: &str) -> String {
    let digest = sha2::Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 把 seecode 临时注册为可用并带 adapter 版本。生产 seed 是 unavailable；
/// 这里只在 sqlx::test 为每个测试新建的独立库里生效，不是任何生产绕过开关。
pub async fn sql_enable_seecode(pool: &sqlx::PgPool, adapter_version: &str) {
    sqlx::query(
        "UPDATE artifact.generator_registry \
         SET status = 'available', version = '0.6.3-test', adapter_version = $1 \
         WHERE generator_id = 'seecode'",
    )
    .bind(adapter_version)
    .execute(pool)
    .await
    .unwrap();
}

/// Worker 申报的能力（与 [`sql_enable_seecode`] 对应）。
pub fn seecode_capabilities(adapter_version: &str) -> AdapterCapabilities {
    AdapterCapabilities::from([("seecode".to_string(), adapter_version.to_string())])
}

#[derive(Debug, Clone, Copy)]
pub struct JobOptions {
    pub max_attempts: i32,
    pub max_execution_seconds: i32,
    pub queue_ttl_seconds: i32,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            max_execution_seconds: 300,
            queue_ttl_seconds: 86_400,
        }
    }
}

/// 测试用的最小合法输入信封。
pub fn minimal_input() -> serde_json::Value {
    serde_json::json!({
        "schema": "smark.seecode.input",
        "schema_version": "1",
        "diagram": {"type": "architecture", "title": "fixture", "nodes": [], "edges": []}
    })
}

/// 直接调用受控函数建一个 queued Job（绕过 API，供协调与 API 测试布景）。
/// 调用前必须已经 [`sql_enable_seecode`]：生成器不可用时数据库拒绝入队。
pub async fn sql_create_job(
    pool: &sqlx::PgPool,
    tenant: Uuid,
    project: Uuid,
    artifact_id: Uuid,
    options: &JobOptions,
) -> Uuid {
    sql_create_job_with(
        pool,
        tenant,
        project,
        artifact_id,
        options,
        None,
        &sha256_hex("request"),
    )
    .await
    .0
}

/// 同上，并可指定 Idempotency-Key 与 request hash。返回 (job_id, replayed)。
pub async fn sql_create_job_with(
    pool: &sqlx::PgPool,
    tenant: Uuid,
    project: Uuid,
    artifact_id: Uuid,
    options: &JobOptions,
    idempotency_key: Option<&str>,
    request_hash: &str,
) -> (Uuid, bool) {
    let requested = Uuid::new_v4();
    let (created, replayed) = try_sql_create_job_with(
        pool,
        requested,
        tenant,
        project,
        artifact_id,
        options,
        idempotency_key,
        request_hash,
    )
    .await
    .unwrap();
    if !replayed {
        assert_eq!(
            created, requested,
            "a fresh request must create its own job"
        );
    }
    (created, replayed)
}

/// [`sql_create_job_with`] 的可失败版本：供断言数据库拒绝路径的测试使用。
/// 返回 (job_id, replayed)；replayed 为 caller_request_replay 或 idempotency_replay。
#[allow(clippy::too_many_arguments)]
pub async fn try_sql_create_job_with(
    pool: &sqlx::PgPool,
    job_id: Uuid,
    tenant: Uuid,
    project: Uuid,
    artifact_id: Uuid,
    options: &JobOptions,
    idempotency_key: Option<&str>,
    request_hash: &str,
) -> Result<(Uuid, bool), sqlx::Error> {
    try_sql_create_job_with_identity(
        pool,
        job_id,
        tenant,
        project,
        artifact_id,
        options,
        idempotency_key,
        request_hash,
        None,
    )
    .await
}

/// AS-2：可指定 caller identity 三元组的创建调用（直接走受控函数 v2）。
/// `identity = None` 即 legacy 未键控语义。
#[allow(clippy::too_many_arguments)]
pub async fn try_sql_create_job_with_identity(
    pool: &sqlx::PgPool,
    job_id: Uuid,
    tenant: Uuid,
    project: Uuid,
    artifact_id: Uuid,
    options: &JobOptions,
    idempotency_key: Option<&str>,
    request_hash: &str,
    identity: Option<(Uuid, &str, i16)>,
) -> Result<(Uuid, bool), sqlx::Error> {
    let (caller_request_id, fingerprint, version) = match identity {
        Some((caller, fingerprint, version)) => (Some(caller), Some(fingerprint), Some(version)),
        None => (None, None, None),
    };
    let (resolved_id, resolution): (Uuid, String) = sqlx::query_as(
        "SELECT * FROM artifact.create_generation_job_with_input \
         ($1, $2, $3, $4, 'seecode', $5, '{}'::jsonb, '1', $6, $7, \
          $8, $9, $10, $11, $12, $13)",
    )
    .bind(job_id)
    .bind(tenant)
    .bind(project)
    .bind(artifact_id)
    .bind(minimal_input())
    .bind(request_hash)
    .bind(idempotency_key)
    .bind(caller_request_id)
    .bind(fingerprint)
    .bind(version)
    .bind(options.max_attempts)
    .bind(options.max_execution_seconds)
    .bind(options.queue_ttl_seconds)
    .fetch_one(pool)
    .await?;
    Ok((resolved_id, resolution != "created"))
}

/// 模拟 ART-03D 的输出登记：建一个 READY Version 和一个已通过安全检查的主 Output，
/// 并把它设为 Job 的目标版本。返回主 Output 的 sha256。
pub async fn sql_register_ready_output(
    pool: &sqlx::PgPool,
    tenant: Uuid,
    project: Uuid,
    artifact_id: Uuid,
    job_id: Uuid,
) -> String {
    let version_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO artifact.artifact_version \
         (id, tenant_id, project_id, artifact_id, state, generator_id, created_by) \
         VALUES ($1, $2, $3, $4, 'pending', 'seecode', 'fixture')",
    )
    .bind(version_id)
    .bind(tenant)
    .bind(project)
    .bind(artifact_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE artifact.generation_job SET target_version_id = $2 WHERE id = $1")
        .bind(job_id)
        .bind(version_id)
        .execute(pool)
        .await
        .unwrap();
    let sha256 = sha256_hex("svg-output");
    sqlx::query(
        "INSERT INTO artifact.artifact_output \
         (id, tenant_id, project_id, artifact_id, version_id, output_role, format, mime_type, \
          size_bytes, sha256, file_object_id, safety_status) \
         VALUES ($1, $2, $3, $4, $5, 'primary', 'svg', 'image/svg+xml', 10, $6, 'fixture-object', 'passed')",
    )
    .bind(Uuid::new_v4())
    .bind(tenant)
    .bind(project)
    .bind(artifact_id)
    .bind(version_id)
    .bind(&sha256)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE artifact.artifact_version SET state = 'ready', ready_at = now() WHERE id = $1",
    )
    .bind(version_id)
    .execute(pool)
    .await
    .unwrap();
    sha256
}
