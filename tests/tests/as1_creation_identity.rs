//! AS-1：持久化创建身份、幂等、事务原子性与崩溃恢复的 PostgreSQL 集成测试。
//!
//! 全部测试跑在真实 PostgreSQL（sqlx::test 为每个用例建独立库并执行 migrations），
//! 通过进程内 HTTP 面或 SQL 直连验证 ART-04E 冻结的行为，不做「单元测试冒充
//! 并发/事务验证」。

mod common;

use artifact_contract::ArtifactResponse;
use common::{PROJECT_1, PROJECT_2, TENANT};
use reqwest::StatusCode;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

const TOKEN_P1: &str = "as1-token-project-1";
const TOKEN_P2: &str = "as1-token-project-2";
const ENDPOINT: &str = "POST /v1/artifacts";

struct Api {
    base: String,
    http: reqwest::Client,
}

fn create_body(project: Uuid, artifact_key: Option<&str>, name: &str) -> Value {
    json!({
        "project_id": project,
        "name": name,
        "artifact_type": "visualization",
        "artifact_key": artifact_key,
        "description": null,
        "category": null,
    })
}

impl Api {
    async fn spawn(pool: PgPool) -> Self {
        let verifier = common::verifier_with(&[
            (TOKEN_P1, TENANT, PROJECT_1, common::read_actions()),
            (TOKEN_P2, TENANT, PROJECT_2, common::read_actions()),
        ]);
        let base = common::spawn_app(pool, verifier).await;
        Self {
            base,
            http: reqwest::Client::new(),
        }
    }

    fn create_request(
        &self,
        project: Uuid,
        artist_key: Option<&str>,
        name: &str,
        idempotency_key: Option<&str>,
        token: &str,
    ) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(format!("{}/v1/artifacts", self.base))
            .bearer_auth(token)
            .header("x-project-id", project.to_string())
            .json(&create_body(project, artist_key, name));
        if let Some(key) = idempotency_key {
            req = req.header("idempotency-key", key);
        }
        req
    }

    async fn create(
        &self,
        project: Uuid,
        artifact_key: Option<&str>,
        name: &str,
        idempotency_key: Option<&str>,
        token: &str,
    ) -> (
        StatusCode,
        Option<ArtifactResponse>,
        Option<String>,
        Option<String>,
    ) {
        let res = self
            .create_request(project, artifact_key, name, idempotency_key, token)
            .send()
            .await
            .unwrap();
        let status = res.status();
        let resolution = res
            .headers()
            .get("x-artifact-resolution")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let replay = res
            .headers()
            .get("x-idempotent-replay")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body: Option<ArtifactResponse> = res.json().await.ok();
        (status, body, resolution, replay)
    }
}

async fn count_artifacts(pool: &PgPool, project: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM artifact.artifact WHERE tenant_id=$1 AND project_id=$2",
    )
    .bind(TENANT)
    .bind(project)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn count_identities(pool: &PgPool, project: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM artifact.artifact_creation_identity WHERE tenant_id=$1 AND project_id=$2",
    )
    .bind(TENANT)
    .bind(project)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn identity_fingerprint(
    pool: &PgPool,
    project: Uuid,
    artifact_key: &str,
) -> Option<(Uuid, i16, String)> {
    sqlx::query_as(
        "SELECT artifact_id, fingerprint_version, request_fingerprint \
         FROM artifact.artifact_creation_identity \
         WHERE tenant_id=$1 AND project_id=$2 AND artifact_key=$3",
    )
    .bind(TENANT)
    .bind(project)
    .bind(artifact_key)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn idempotency_row(
    pool: &PgPool,
    project: Uuid,
    key: &str,
) -> Option<(Option<Value>, Option<Uuid>, String)> {
    sqlx::query_as(
        "SELECT response_snapshot, resource_id, request_hash FROM artifact.idempotency_record \
         WHERE tenant_id=$1 AND project_id=$2 AND endpoint=$3 AND idempotency_key=$4",
    )
    .bind(TENANT)
    .bind(project)
    .bind(ENDPOINT)
    .bind(key)
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// 模拟「迁移前就已存在」的 keyed Artifact（无创建身份行）。
/// 0004 的延迟约束触发器会拒绝新插入的孤立 keyed 行，因此这里临时停用
/// 用户触发器插入历史 fixture，随后立即恢复——只用于布景，不是产品代码路径。
async fn insert_legacy_keyed_artifact(pool: &PgPool, project: Uuid, key: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("ALTER TABLE artifact.artifact DISABLE TRIGGER USER")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO artifact.artifact \
         (id, tenant_id, project_id, artifact_key, name, artifact_type, created_by) \
         VALUES ($1, $2, $3, $4, 'legacy', 'visualization', 'fixture')",
    )
    .bind(id)
    .bind(TENANT)
    .bind(project)
    .bind(key)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("ALTER TABLE artifact.artifact ENABLE TRIGGER USER")
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn insert_incomplete_reservation(pool: &PgPool, project: Uuid, key: &str, hash: &str) {
    sqlx::query(
        "INSERT INTO artifact.idempotency_record \
         (id, tenant_id, project_id, endpoint, idempotency_key, request_hash) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(TENANT)
    .bind(project)
    .bind(ENDPOINT)
    .bind(key)
    .bind(hash)
    .execute(pool)
    .await
    .unwrap();
}

async fn request_hash_for(project: Uuid, key: &str, name: &str) -> String {
    // 与 HTTP handler 相同：对请求体 JSON 的原始序列化字节做 SHA-256。
    // reqwest `.json(&Value)` 与本函数的 `to_string` 使用同一序列化器。
    let body = create_body(project, Some(key), name);
    common::sha256_hex(&serde_json::to_string(&body).unwrap())
}

// ── §9.1 创建身份语义 ──────────────────────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn keyed_create_persists_identity_in_same_transaction(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (status, body, resolution, replay) = api
        .create(PROJECT_1, Some("report.q3"), "Q3", None, TOKEN_P1)
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(resolution.as_deref(), Some("created"));
    assert_eq!(replay, None);
    let artifact = body.expect("created artifact");
    assert_eq!(artifact.artifact_key.as_deref(), Some("report.q3"));

    let (artifact_id, version, fingerprint) = identity_fingerprint(&pool, PROJECT_1, "report.q3")
        .await
        .expect("durable identity must exist");
    assert_eq!(artifact_id, artifact.id);
    assert_eq!(version, 1);
    assert_eq!(fingerprint.len(), 64);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn same_key_same_fingerprint_resolves_existing(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (first_status, first, _, _) = api
        .create(PROJECT_1, Some("report.q3"), "Q3", None, TOKEN_P1)
        .await;
    assert_eq!(first_status, StatusCode::CREATED);
    let first = first.unwrap();

    let (status, second, resolution, replay) = api
        .create(PROJECT_1, Some("report.q3"), "Q3", None, TOKEN_P1)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resolution.as_deref(), Some("existing_key"));
    assert_eq!(replay, None, "existing_key is not an idempotency replay");
    assert_eq!(second.unwrap().id, first.id);
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn same_key_different_fingerprint_conflicts_without_mutation(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (_, first, _, _) = api
        .create(PROJECT_1, Some("report.q3"), "Q3", None, TOKEN_P1)
        .await;
    let first = first.unwrap();

    let res = api
        .create_request(PROJECT_1, Some("report.q3"), "Q3 renamed", None, TOKEN_P1)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let error: Value = res.json().await.unwrap();
    assert_eq!(error["error"]["code"], "ARTIFACT_KEY_CONFLICT");
    assert_eq!(error["error"]["retryable"], false);

    let row = sqlx::query(
        "SELECT count(*) AS total, (array_agg(id))[1] AS only_id \
         FROM artifact.artifact WHERE tenant_id=$1 AND project_id=$2 AND artifact_key=$3",
    )
    .bind(TENANT)
    .bind(PROJECT_1)
    .bind("report.q3")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("total"), 1);
    assert_eq!(row.get::<Option<Uuid>, _>("only_id"), Some(first.id));
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn different_keys_with_same_payload_are_distinct_artifacts(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (s1, a1, _, _) = api
        .create(PROJECT_1, Some("key.a"), "Same", None, TOKEN_P1)
        .await;
    let (s2, a2, _, _) = api
        .create(PROJECT_1, Some("key.b"), "Same", None, TOKEN_P1)
        .await;
    assert_eq!(s1, StatusCode::CREATED);
    assert_eq!(s2, StatusCode::CREATED);
    assert_ne!(a1.unwrap().id, a2.unwrap().id);
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 2);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 2);
}

#[sqlx::test(migrations = "../migrations")]
async fn unicode_equivalent_requests_share_one_fingerprint(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    // "café" 的 NFC 形式与 NFD 形式必须被规范化为同一创建指纹。
    let nfc_key = "caf\u{e9}";
    let nfd_key = "cafe\u{301}";
    let (s1, a1, _, _) = api
        .create(PROJECT_1, Some(nfc_key), "caf\u{e9}", None, TOKEN_P1)
        .await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, a2, resolution, _) = api
        .create(PROJECT_1, Some(nfd_key), "cafe\u{301}", None, TOKEN_P1)
        .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(resolution.as_deref(), Some("existing_key"));
    assert_eq!(a1.unwrap().id, a2.unwrap().id);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
}

// ── §9.2 Idempotency-Key ───────────────────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn completed_idempotency_replays_and_is_atomically_recorded(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (status, body, resolution, replay) = api
        .create(PROJECT_1, Some("report.q3"), "Q3", Some("idem-1"), TOKEN_P1)
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(resolution.as_deref(), Some("created"));
    assert_eq!(replay, None);
    let created = body.unwrap();

    let (snapshot, resource_id, _) = idempotency_row(&pool, PROJECT_1, "idem-1")
        .await
        .expect("idempotency record");
    assert!(snapshot.is_some(), "completion snapshot must be stored");
    assert_eq!(resource_id, Some(created.id));

    let (status, replay_body, resolution, replay_header) = api
        .create(PROJECT_1, Some("report.q3"), "Q3", Some("idem-1"), TOKEN_P1)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resolution.as_deref(), Some("idempotency_replay"));
    assert_eq!(replay_header.as_deref(), Some("true"));
    assert_eq!(replay_body.unwrap().id, created.id);
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn idempotency_key_with_different_body_conflicts(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (_, created, _, _) = api
        .create(PROJECT_1, Some("report.q3"), "Q3", Some("idem-1"), TOKEN_P1)
        .await;
    let created = created.unwrap();

    let res = api
        .create_request(
            PROJECT_1,
            Some("report.q3"),
            "Q3 other",
            Some("idem-1"),
            TOKEN_P1,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let error: Value = res.json().await.unwrap();
    assert_eq!(error["error"]["code"], "IDEMPOTENCY_KEY_REUSED");

    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
    let (_, resource_id, _) = idempotency_row(&pool, PROJECT_1, "idem-1").await.unwrap();
    assert_eq!(resource_id, Some(created.id));
}

#[sqlx::test(migrations = "../migrations")]
async fn expired_completed_idempotency_still_replays(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (_, created, _, _) = api
        .create(
            PROJECT_1,
            Some("report.q3"),
            "Q3",
            Some("idem-expired"),
            TOKEN_P1,
        )
        .await;
    let created = created.unwrap();
    sqlx::query(
        "UPDATE artifact.idempotency_record SET expires_at = now() - interval '1 day' \
         WHERE tenant_id=$1 AND project_id=$2 AND idempotency_key=$3",
    )
    .bind(TENANT)
    .bind(PROJECT_1)
    .bind("idem-expired")
    .execute(&pool)
    .await
    .unwrap();

    let (status, body, resolution, _) = api
        .create(
            PROJECT_1,
            Some("report.q3"),
            "Q3",
            Some("idem-expired"),
            TOKEN_P1,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resolution.as_deref(), Some("idempotency_replay"));
    assert_eq!(body.unwrap().id, created.id);
}

#[sqlx::test(migrations = "../migrations")]
async fn stale_incomplete_reservation_is_completed_by_durable_resolution(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let hash = request_hash_for(PROJECT_1, "report.q3", "Q3").await;
    insert_incomplete_reservation(&pool, PROJECT_1, "idem-stale", &hash).await;

    // 数据库没有任何 Artifact/身份：重试可以用持久化 key 解析并接管预留。
    let (status, body, resolution, _) = api
        .create(
            PROJECT_1,
            Some("report.q3"),
            "Q3",
            Some("idem-stale"),
            TOKEN_P1,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    assert_eq!(resolution.as_deref(), Some("created"));
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
    let (snapshot, resource_id, stored_hash) = idempotency_row(&pool, PROJECT_1, "idem-stale")
        .await
        .unwrap();
    assert_eq!(stored_hash, hash);
    assert!(snapshot.is_some());
    assert_eq!(resource_id, Some(body.unwrap().id));
}

#[sqlx::test(migrations = "../migrations")]
async fn legacy_key_without_identity_is_not_adopted(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let legacy_id = insert_legacy_keyed_artifact(&pool, PROJECT_1, "legacy.key").await;

    let res = api
        .create_request(PROJECT_1, Some("legacy.key"), "legacy", None, TOKEN_P1)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let error: Value = res.json().await.unwrap();
    assert_eq!(error["error"]["code"], "ARTIFACT_KEY_LEGACY_UNVERIFIABLE");

    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 0);
    let id: Uuid = sqlx::query_scalar(
        "SELECT id FROM artifact.artifact WHERE tenant_id=$1 AND project_id=$2 AND artifact_key='legacy.key'",
    )
    .bind(TENANT)
    .bind(PROJECT_1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(id, legacy_id);
}

#[sqlx::test(migrations = "../migrations")]
async fn forged_completed_replay_fails_closed(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (_, created, _, _) = api
        .create(
            PROJECT_1,
            Some("key.real"),
            "Real",
            Some("idem-forged"),
            TOKEN_P1,
        )
        .await;
    let created = created.unwrap();

    // 篡改完成记录：resource_id 指向另一个 key 的 Artifact（模拟损坏/伪造状态）。
    let other = insert_legacy_keyed_artifact(&pool, PROJECT_1, "key.other").await;
    sqlx::query(
        "UPDATE artifact.idempotency_record SET resource_id = $2 \
         WHERE tenant_id=$1 AND project_id=$3 AND idempotency_key='idem-forged'",
    )
    .bind(TENANT)
    .bind(other)
    .bind(PROJECT_1)
    .execute(&pool)
    .await
    .unwrap();

    let res = api
        .create_request(
            PROJECT_1,
            Some("key.real"),
            "Real",
            Some("idem-forged"),
            TOKEN_P1,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a corrupted completion must fail closed, never replay"
    );
    let error: Value = res.json().await.unwrap();
    assert_eq!(error["error"]["code"], "INTERNAL_ERROR");

    // 失败关闭不得产生任何变更。
    let row = sqlx::query(
        "SELECT resource_id, response_snapshot FROM artifact.idempotency_record \
         WHERE tenant_id=$1 AND project_id=$2 AND idempotency_key='idem-forged'",
    )
    .bind(TENANT)
    .bind(PROJECT_1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<Option<Uuid>, _>(0), Some(other));
    assert!(row.get::<Option<Value>, _>(1).is_some());
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 2);
    assert_eq!(
        sqlx::query_scalar::<_, Option<Uuid>>(
            "SELECT artifact_id FROM artifact.artifact_creation_identity \
             WHERE tenant_id=$1 AND project_id=$2 AND artifact_key='key.real'",
        )
        .bind(TENANT)
        .bind(PROJECT_1)
        .fetch_one(&pool)
        .await
        .unwrap(),
        Some(created.id)
    );
}

// ── §9.3 并发与崩溃恢复 ────────────────────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn concurrent_same_key_creates_exactly_one_artifact(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let base = api.base.clone();
        let http = api.http.clone();
        tasks.push(tokio::spawn(async move {
            let res = http
                .post(format!("{base}/v1/artifacts"))
                .bearer_auth(TOKEN_P1)
                .header("x-project-id", PROJECT_1.to_string())
                .json(&json!({
                    "project_id": PROJECT_1,
                    "name": "race",
                    "artifact_type": "visualization",
                    "artifact_key": "race.key",
                }))
                .send()
                .await
                .unwrap();
            (
                res.status(),
                res.headers()
                    .get("x-artifact-resolution")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string),
            )
        }));
    }
    let mut created = 0;
    let mut existing = 0;
    for task in tasks {
        let (status, resolution) = task.await.unwrap();
        match status {
            StatusCode::CREATED => {
                assert_eq!(resolution.as_deref(), Some("created"));
                created += 1;
            }
            StatusCode::OK => {
                assert_eq!(resolution.as_deref(), Some("existing_key"));
                existing += 1;
            }
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(created, 1, "exactly one request wins the insert");
    assert_eq!(existing, 7, "every other request resolves the winner");
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn concurrent_same_key_different_payload_has_single_winner(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let mut tasks = Vec::new();
    for name in ["alpha", "beta", "gamma", "delta"] {
        let base = api.base.clone();
        let http = api.http.clone();
        tasks.push(tokio::spawn(async move {
            let res = http
                .post(format!("{base}/v1/artifacts"))
                .bearer_auth(TOKEN_P1)
                .header("x-project-id", PROJECT_1.to_string())
                .json(&json!({
                    "project_id": PROJECT_1,
                    "name": name,
                    "artifact_type": "visualization",
                    "artifact_key": "contended.key",
                }))
                .send()
                .await
                .unwrap();
            res.status()
        }));
    }
    let mut statuses = Vec::new();
    for task in tasks {
        statuses.push(task.await.unwrap());
    }
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::CREATED)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::CONFLICT)
            .count(),
        3
    );
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn concurrent_same_idempotency_key_yields_one_artifact(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let base = api.base.clone();
        let http = api.http.clone();
        tasks.push(tokio::spawn(async move {
            let res = http
                .post(format!("{base}/v1/artifacts"))
                .bearer_auth(TOKEN_P1)
                .header("x-project-id", PROJECT_1.to_string())
                .header("idempotency-key", "shared-idem")
                .json(&json!({
                    "project_id": PROJECT_1,
                    "name": "idem race",
                    "artifact_type": "visualization",
                    "artifact_key": "shared.key",
                }))
                .send()
                .await
                .unwrap();
            res.status()
        }));
    }
    for task in tasks {
        let status = task.await.unwrap();
        assert!(
            status == StatusCode::CREATED || status == StatusCode::OK,
            "unexpected {status}"
        );
    }
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM artifact.idempotency_record \
             WHERE tenant_id=$1 AND project_id=$2 AND idempotency_key='shared-idem'",
        )
        .bind(TENANT)
        .bind(PROJECT_1)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn transaction_rollback_leaves_no_partial_state(pool: PgPool) {
    // 故障注入：一个带 artifact_key 的 Artifact 若没有配对身份行，
    // DEFERRABLE 约束触发器必须在 COMMIT 时拒绝整个事务。
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO artifact.artifact \
         (id, tenant_id, project_id, artifact_key, name, artifact_type, created_by) \
         VALUES ($1, $2, $3, 'rolled.back', 'rolled', 'visualization', 'fixture')",
    )
    .bind(Uuid::new_v4())
    .bind(TENANT)
    .bind(PROJECT_1)
    .execute(&mut *tx)
    .await
    .unwrap();
    let err = tx
        .commit()
        .await
        .expect_err("commit must fail without a durable identity");
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("P0001"));

    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 0);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 0);

    // 身份行已写入但进程在 COMMIT 前崩溃（显式回滚）：两表都不留残留。
    let mut tx = pool.begin().await.unwrap();
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO artifact.artifact \
         (id, tenant_id, project_id, artifact_key, name, artifact_type, created_by) \
         VALUES ($1, $2, $3, 'pre.commit.crash', 'crash', 'visualization', 'fixture')",
    )
    .bind(id)
    .bind(TENANT)
    .bind(PROJECT_1)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO artifact.artifact_creation_identity \
         (tenant_id, project_id, artifact_key, artifact_id, fingerprint_version, request_fingerprint, created_by) \
         VALUES ($1, $2, 'pre.commit.crash', $3, 1, repeat('c', 64), 'fixture')",
    )
    .bind(TENANT)
    .bind(PROJECT_1)
    .bind(id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 0);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 0);
}

// ── §9.4 作用域、不可变、兼容 ──────────────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn equivalent_json_serializations_share_one_fingerprint(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    // 仅键序/空白不同、语义相同的两个原始 Body：Idempotency-Key request_hash
    // 会不同（字节敏感），但创建指纹必须一致（语义敏感）。
    let first = format!(
        r#"{{"project_id":"{p}","name":"Wire","artifact_type":"visualization","artifact_key":"wire.key"}}"#,
        p = PROJECT_1
    );
    let second = format!(
        "{{ \"artifact_key\" : \"wire.key\",\n  \"artifact_type\":\"visualization\",\n  \"name\":\"Wire\", \"project_id\":\"{p}\" }}",
        p = PROJECT_1
    );
    let send = |body: String| {
        let http = api.http.clone();
        let base = api.base.clone();
        async move {
            http.post(format!("{base}/v1/artifacts"))
                .bearer_auth(TOKEN_P1)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .unwrap()
        }
    };
    let res1 = send(first).await;
    assert_eq!(res1.status(), StatusCode::CREATED);
    let first_id = res1.json::<ArtifactResponse>().await.unwrap().id;

    let res2 = send(second).await;
    assert_eq!(res2.status(), StatusCode::OK);
    assert_eq!(
        res2.headers()
            .get("x-artifact-resolution")
            .and_then(|v| v.to_str().ok()),
        Some("existing_key")
    );
    assert_eq!(res2.json::<ArtifactResponse>().await.unwrap().id, first_id);
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn artifact_key_cannot_be_mutated_by_direct_sql(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (_, created, _, _) = api
        .create(PROJECT_1, Some("immutable.key"), "I", None, TOKEN_P1)
        .await;
    let created = created.unwrap();

    let err = sqlx::query("UPDATE artifact.artifact SET artifact_key='renamed.key' WHERE id=$1")
        .bind(created.id)
        .execute(&pool)
        .await
        .expect_err("artifact_key is immutable");
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("P0001"));

    // V1 也不允许把历史 unkeyed 行改成 keyed（adoption 流程另行审批）。
    let unkeyed = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "unkeyed").await;
    let err = sqlx::query("UPDATE artifact.artifact SET artifact_key='adopted.key' WHERE id=$1")
        .bind(unkeyed)
        .execute(&pool)
        .await
        .expect_err("NULL -> non-NULL artifact_key is rejected in V1");
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("P0001"));
}

#[sqlx::test(migrations = "../migrations")]
async fn creation_identity_rows_are_append_only(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    api.create(PROJECT_1, Some("append.only"), "A", None, TOKEN_P1)
        .await;

    let err = sqlx::query(
        "UPDATE artifact.artifact_creation_identity SET request_fingerprint=repeat('a', 64) \
         WHERE tenant_id=$1 AND project_id=$2 AND artifact_key='append.only'",
    )
    .bind(TENANT)
    .bind(PROJECT_1)
    .execute(&pool)
    .await
    .expect_err("identity rows cannot be rewritten");
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("P0001"));

    let err = sqlx::query(
        "DELETE FROM artifact.artifact_creation_identity \
         WHERE tenant_id=$1 AND project_id=$2 AND artifact_key='append.only'",
    )
    .bind(TENANT)
    .bind(PROJECT_1)
    .execute(&pool)
    .await
    .expect_err("identity rows cannot be deleted");
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("P0001"));
}

#[sqlx::test(migrations = "../migrations")]
async fn identity_must_match_the_referenced_artifact_key(pool: PgPool) {
    let artifact_id = common::sql_insert_artifact(&pool, TENANT, PROJECT_1, "unkeyed").await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO artifact.artifact_creation_identity \
         (tenant_id, project_id, artifact_key, artifact_id, fingerprint_version, request_fingerprint, created_by) \
         VALUES ($1, $2, 'ghost.key', $3, 1, repeat('0', 64), 'fixture')",
    )
    .bind(TENANT)
    .bind(PROJECT_1)
    .bind(artifact_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    let err = tx
        .commit()
        .await
        .expect_err("identity must not commit with a mismatched artifact_key");
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("P0001"));
}

#[sqlx::test(migrations = "../migrations")]
async fn identity_cannot_cross_tenant_or_project_scope(pool: PgPool) {
    let foreign = common::sql_insert_artifact(&pool, TENANT, PROJECT_2, "foreign").await;
    let err = sqlx::query(
        "INSERT INTO artifact.artifact_creation_identity \
         (tenant_id, project_id, artifact_key, artifact_id, fingerprint_version, request_fingerprint, created_by) \
         VALUES ($1, $2, 'cross.key', $3, 1, repeat('0', 64), 'fixture')",
    )
    .bind(TENANT)
    .bind(PROJECT_1)
    .bind(foreign)
    .execute(&pool)
    .await
    .expect_err("composite FK rejects cross-project identity references");
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("23503"));
}

#[sqlx::test(migrations = "../migrations")]
async fn completion_write_failure_rolls_back_artifact_identity_and_reservation(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    // 故障注入：在同一事务写完成快照时失败 → 幂等预留、Artifact、身份全部回滚。
    sqlx::query(
        "CREATE FUNCTION artifact.as1_test_fail_completion() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'injected completion failure' USING ERRCODE = 'P0001'; END $$",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER as1_test_fail_completion BEFORE UPDATE ON artifact.idempotency_record \
         FOR EACH ROW EXECUTE FUNCTION artifact.as1_test_fail_completion()",
    )
    .execute(&pool)
    .await
    .unwrap();

    let res = api
        .create_request(
            PROJECT_1,
            Some("fail.key"),
            "F",
            Some("idem-fail"),
            TOKEN_P1,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);

    sqlx::query("DROP TRIGGER as1_test_fail_completion ON artifact.idempotency_record")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION artifact.as1_test_fail_completion()")
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 0);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 0);
    let reservations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.idempotency_record WHERE tenant_id=$1 AND idempotency_key='idem-fail'",
    )
    .bind(TENANT)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        reservations, 0,
        "the reservation must roll back with the rest"
    );

    // 失败后同一 key 可以安全重试并创建恰好一个 Artifact。
    let res = api
        .create_request(
            PROJECT_1,
            Some("fail.key"),
            "F",
            Some("idem-fail"),
            TOKEN_P1,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn archived_or_deleted_key_resolves_same_artifact_instead_of_reusing(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (_, created, _, _) = api
        .create(PROJECT_1, Some("lifecycle.key"), "L", None, TOKEN_P1)
        .await;
    let created = created.unwrap();
    sqlx::query(
        "UPDATE artifact.artifact SET lifecycle_state='deleted', deleted_at=now() WHERE id=$1",
    )
    .bind(created.id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body, resolution, _) = api
        .create(PROJECT_1, Some("lifecycle.key"), "L", None, TOKEN_P1)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resolution.as_deref(), Some("existing_key"));
    assert_eq!(body.unwrap().id, created.id, "the key is never re-minted");
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn replay_requires_fresh_authorization_in_the_authorized_scope(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (_, created, _, _) = api
        .create(
            PROJECT_1,
            Some("auth.key"),
            "A",
            Some("idem-auth"),
            TOKEN_P1,
        )
        .await;
    let created = created.unwrap();

    // 另一个 project 的 token 不能通过重放窥探或取得该 Artifact。
    let res = api
        .create_request(
            PROJECT_1,
            Some("auth.key"),
            "A",
            Some("idem-auth"),
            TOKEN_P2,
        )
        .send()
        .await
        .unwrap();
    assert!(
        res.status() == StatusCode::FORBIDDEN || res.status() == StatusCode::BAD_REQUEST,
        "unexpected {}",
        res.status()
    );
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    let _ = created;
}

#[sqlx::test(migrations = "../migrations")]
async fn one_idempotency_key_cannot_serve_two_artifact_keys(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (s1, a1, _, _) = api
        .create(PROJECT_1, Some("key.one"), "One", Some("shared"), TOKEN_P1)
        .await;
    assert_eq!(s1, StatusCode::CREATED);

    let res = api
        .create_request(PROJECT_1, Some("key.two"), "One", Some("shared"), TOKEN_P1)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let error: Value = res.json().await.unwrap();
    assert_eq!(error["error"]["code"], "IDEMPOTENCY_KEY_REUSED");

    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
    assert!(
        identity_fingerprint(&pool, PROJECT_1, "key.two")
            .await
            .is_none()
    );
    let _ = a1;
}

#[sqlx::test(migrations = "../migrations")]
async fn keyed_artifact_insert_without_identity_is_rejected_at_commit(pool: PgPool) {
    // 模拟切换窗口内残留的旧二进制：只插入 keyed Artifact、从不写创建身份。
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO artifact.artifact \
         (id, tenant_id, project_id, artifact_key, name, artifact_type, created_by) \
         VALUES ($1, $2, $3, 'residual.old.writer', 'old', 'file', 'old-binary')",
    )
    .bind(Uuid::new_v4())
    .bind(TENANT)
    .bind(PROJECT_1)
    .execute(&mut *tx)
    .await
    .unwrap();
    let err = tx
        .commit()
        .await
        .expect_err("a keyed Artifact must never commit without its identity");
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("P0001"));
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 0);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn crash_mid_keyed_transaction_rolls_back_all_writes(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let hash = request_hash_for(PROJECT_1, "crash.mid", "C").await;

    // 真实故障注入：在 keyed 事务已写入预留、Artifact、身份之后，
    // 由另一连接终止该 backend（等价于进程崩溃），COMMIT 永远不会执行。
    let mut tx = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let artifact_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO artifact.idempotency_record \
         (id, tenant_id, project_id, endpoint, idempotency_key, request_hash) \
         VALUES ($1, $2, $3, $4, 'idem-crash-mid', $5)",
    )
    .bind(Uuid::new_v4())
    .bind(TENANT)
    .bind(PROJECT_1)
    .bind(ENDPOINT)
    .bind(&hash)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO artifact.artifact \
         (id, tenant_id, project_id, artifact_key, name, artifact_type, created_by) \
         VALUES ($1, $2, $3, 'crash.mid', 'C', 'visualization', 'fixture')",
    )
    .bind(artifact_id)
    .bind(TENANT)
    .bind(PROJECT_1)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO artifact.artifact_creation_identity \
         (tenant_id, project_id, artifact_key, artifact_id, fingerprint_version, request_fingerprint, created_by) \
         VALUES ($1, $2, 'crash.mid', $3, 1, repeat('d', 64), 'fixture')",
    )
    .bind(TENANT)
    .bind(PROJECT_1)
    .bind(artifact_id)
    .execute(&mut *tx)
    .await
    .unwrap();

    let killed: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(killed, "the in-flight keyed transaction must be terminated");
    drop(tx);

    // 崩溃事务的全部写入必须消失。
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 0);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 0);
    let reservations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifact.idempotency_record WHERE idempotency_key='idem-crash-mid'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reservations, 0);

    // 重试恰好创建一个 Artifact 与一个身份。
    let (status, body, resolution, replay) = api
        .create(
            PROJECT_1,
            Some("crash.mid"),
            "C",
            Some("idem-crash-mid"),
            TOKEN_P1,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(resolution.as_deref(), Some("created"));
    assert!(body.is_some());
    assert!(replay.is_none());
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn audit_failure_after_commit_keeps_the_create_response(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    // 故障注入：审计写入在 COMMIT 之后 best-effort，失败不得改变成功响应。
    sqlx::query("ALTER TABLE artifact.audit_event RENAME TO audit_event_as1_off")
        .execute(&pool)
        .await
        .unwrap();
    let result = api
        .create(PROJECT_1, Some("audit.key"), "A", None, TOKEN_P1)
        .await;
    sqlx::query("ALTER TABLE artifact.audit_event_as1_off RENAME TO audit_event")
        .execute(&pool)
        .await
        .unwrap();

    let (status, body, resolution, replay) = result;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(resolution.as_deref(), Some("created"));
    assert!(body.is_some());
    assert!(replay.is_none());
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn concurrent_one_idempotency_key_two_artifact_keys_has_one_winner(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let mut tasks = Vec::new();
    for index in 0..8 {
        let base = api.base.clone();
        let http = api.http.clone();
        // 同一个 Idempotency-Key，两组不同的 artifact_key（因此 request_hash 不同）。
        let artifact_key = if index % 2 == 0 { "race.a" } else { "race.b" };
        tasks.push(tokio::spawn(async move {
            let res = http
                .post(format!("{base}/v1/artifacts"))
                .bearer_auth(TOKEN_P1)
                .header("x-project-id", PROJECT_1.to_string())
                .header("idempotency-key", "shared-race")
                .json(&json!({
                    "project_id": PROJECT_1,
                    "name": "Shared",
                    "artifact_type": "visualization",
                    "artifact_key": artifact_key,
                }))
                .send()
                .await
                .unwrap();
            (res.status(), res.json::<Value>().await.unwrap())
        }));
    }
    let mut successful_keys = std::collections::HashSet::new();
    let mut success = 0;
    let mut conflicts = 0;
    for task in tasks {
        let (status, body) = task.await.unwrap();
        match status {
            StatusCode::CREATED | StatusCode::OK => {
                success += 1;
                successful_keys.insert(body["artifact_key"].as_str().unwrap().to_string());
            }
            StatusCode::CONFLICT => {
                assert_eq!(body["error"]["code"], "IDEMPOTENCY_KEY_REUSED");
                conflicts += 1;
            }
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(success + conflicts, 8);
    assert!(
        success >= 1,
        "the winning Idempotency-Key body must succeed"
    );
    assert!(conflicts >= 1, "the other artifact_key must be rejected");
    assert_eq!(
        successful_keys.len(),
        1,
        "only one artifact_key may own this Idempotency-Key"
    );
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn same_key_in_different_projects_is_isolated(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (s1, a1, _, _) = api
        .create(PROJECT_1, Some("shared.key"), "P1", None, TOKEN_P1)
        .await;
    let (s2, a2, _, _) = api
        .create(PROJECT_2, Some("shared.key"), "P1", None, TOKEN_P2)
        .await;
    assert_eq!(s1, StatusCode::CREATED);
    assert_eq!(s2, StatusCode::CREATED);
    assert_ne!(a1.unwrap().id, a2.unwrap().id);
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 1);
    assert_eq!(count_artifacts(&pool, PROJECT_2).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn cross_project_token_is_denied_without_disclosure(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    // 有 PROJECT_2 授权的 token 不能向 PROJECT_1 创建。
    let res = api
        .create_request(PROJECT_1, Some("shared.key"), "P1", None, TOKEN_P2)
        .send()
        .await
        .unwrap();
    assert!(
        res.status() == StatusCode::FORBIDDEN || res.status() == StatusCode::BAD_REQUEST,
        "unexpected {}",
        res.status()
    );
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn unkeyed_create_remains_legacy_compatible(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (status, body, resolution, replay) =
        api.create(PROJECT_1, None, "unkeyed", None, TOKEN_P1).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(resolution.as_deref(), Some("created"));
    assert_eq!(replay, None);
    let artifact = body.unwrap();
    assert!(artifact.artifact_key.is_none());
    assert_eq!(count_identities(&pool, PROJECT_1).await, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn unkeyed_idempotency_replay_keeps_legacy_behaviour(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (s1, a1, _, _) = api
        .create(PROJECT_1, None, "unkeyed", Some("unkeyed-idem"), TOKEN_P1)
        .await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, a2, resolution, replay) = api
        .create(PROJECT_1, None, "unkeyed", Some("unkeyed-idem"), TOKEN_P1)
        .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(resolution.as_deref(), Some("idempotency_replay"));
    assert_eq!(replay.as_deref(), Some("true"));
    assert_eq!(a1.unwrap().id, a2.unwrap().id);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn invalid_artifact_keys_are_rejected_before_persistence(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    for key in ["", " padded ", &"x".repeat(201), "bad\u{7}key"] {
        let res = api
            .create_request(PROJECT_1, Some(key), "invalid", None, TOKEN_P1)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "key {key:?}");
    }
    assert_eq!(count_artifacts(&pool, PROJECT_1).await, 0);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn keyed_create_without_idempotency_key_persists_only_identity(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    let (status, _, _, _) = api
        .create(PROJECT_1, Some("no.idem"), "No idem", None, TOKEN_P1)
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM artifact.idempotency_record WHERE tenant_id=$1")
            .bind(TENANT)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 0);
    assert_eq!(count_identities(&pool, PROJECT_1).await, 1);
}

// ── §9.5 清理与维护 ───────────────────────────────────────────────────

#[sqlx::test(migrations = "../migrations")]
async fn purge_removes_only_completed_expired_rows(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    api.create(PROJECT_1, Some("k1"), "one", Some("done-expired"), TOKEN_P1)
        .await;
    api.create(PROJECT_1, Some("k2"), "two", Some("done-fresh"), TOKEN_P1)
        .await;
    insert_incomplete_reservation(
        &pool,
        PROJECT_1,
        "open-expired",
        &common::sha256_hex("open-expired"),
    )
    .await;

    sqlx::query(
        "UPDATE artifact.idempotency_record SET expires_at = now() - interval '1 day' \
         WHERE idempotency_key IN ('done-expired', 'open-expired')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let purged: i32 = sqlx::query_scalar("SELECT artifact.purge_expired_idempotency_records(100)")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(purged, 1);

    let remaining: Vec<String> = sqlx::query_scalar(
        "SELECT idempotency_key FROM artifact.idempotency_record WHERE tenant_id=$1 ORDER BY idempotency_key",
    )
    .bind(TENANT)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(remaining, vec!["done-fresh", "open-expired"]);
}

#[sqlx::test(migrations = "../migrations")]
async fn purge_respects_batch_size(pool: PgPool) {
    let hash = common::sha256_hex("batch");
    for index in 0..5 {
        sqlx::query(
            "INSERT INTO artifact.idempotency_record \
             (id, tenant_id, project_id, endpoint, idempotency_key, request_hash, response_snapshot, resource_id, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $7, '{}'::jsonb, $6, now() - interval '1 day')",
        )
        .bind(Uuid::new_v4())
        .bind(TENANT)
        .bind(PROJECT_1)
        .bind(ENDPOINT)
        .bind(format!("batch-{index}"))
        .bind(Uuid::new_v4())
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();
    }
    let first: i32 = sqlx::query_scalar("SELECT artifact.purge_expired_idempotency_records(2)")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(first, 2);
    let second: i32 = sqlx::query_scalar("SELECT artifact.purge_expired_idempotency_records(2)")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(second, 2);
}

#[sqlx::test(migrations = "../migrations")]
async fn purge_rejects_invalid_batch_size(pool: PgPool) {
    let err = sqlx::query_scalar::<_, i32>("SELECT artifact.purge_expired_idempotency_records(0)")
        .fetch_one(&pool)
        .await
        .expect_err("batch size must be positive");
    assert_eq!(common::pg_error_code(&err).as_deref(), Some("22023"));
}

#[sqlx::test(migrations = "../migrations")]
async fn purge_skips_rows_locked_by_an_in_flight_request(pool: PgPool) {
    let api = Api::spawn(pool.clone()).await;
    api.create(
        PROJECT_1,
        Some("k-lock"),
        "lock",
        Some("locked-idem"),
        TOKEN_P1,
    )
    .await;
    sqlx::query(
        "UPDATE artifact.idempotency_record SET expires_at = now() - interval '1 day' \
         WHERE idempotency_key='locked-idem'",
    )
    .execute(&pool)
    .await
    .unwrap();

    // 模拟正在重放该 key 的请求：持有行锁。
    let mut request_tx = pool.begin().await.unwrap();
    sqlx::query(
        "SELECT id FROM artifact.idempotency_record \
         WHERE tenant_id=$1 AND project_id=$2 AND idempotency_key='locked-idem' FOR UPDATE",
    )
    .bind(TENANT)
    .bind(PROJECT_1)
    .fetch_one(&mut *request_tx)
    .await
    .unwrap();

    let purged: i32 = sqlx::query_scalar("SELECT artifact.purge_expired_idempotency_records(100)")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        purged, 0,
        "cleanup must skip the row locked by the live request"
    );

    request_tx.commit().await.unwrap();
    let purged: i32 = sqlx::query_scalar("SELECT artifact.purge_expired_idempotency_records(100)")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(purged, 1);
}
