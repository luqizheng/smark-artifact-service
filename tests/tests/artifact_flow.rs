//! Artifact 元数据全流程：创建/幂等/读取/分页/并发更新/作用域边界/
//! current-version 不变量/引用。全部走 SDK（consumer contract 语义）。

mod common;

use artifact_client::{ArtifactClient, ArtifactClientConfig, CallContext};
use artifact_contract::{
    CreateArtifactRequest, CreateReferenceRequest, ListArtifactsQuery, SetCurrentVersionRequest,
    UpdateArtifactRequest,
};
use artifact_domain::Action;

fn create_request(name: &str, project: uuid::Uuid) -> CreateArtifactRequest {
    CreateArtifactRequest {
        project_id: project,
        name: name.to_string(),
        artifact_type: "visualization".to_string(),
        artifact_key: None,
        description: Some("integration fixture".to_string()),
        category: Some("architecture".to_string()),
    }
}

fn client_for(base: &str) -> ArtifactClient {
    ArtifactClient::new(ArtifactClientConfig::new(base)).unwrap()
}

fn ctx_for(token: &str, project: uuid::Uuid) -> CallContext {
    CallContext::new().token(token).project(project)
}

#[sqlx::test(migrations = "../migrations")]
async fn create_get_patch_etag_roundtrip(pool: sqlx::PgPool) {
    let verifier = common::verifier_with(&[(
        "tok-full",
        common::TENANT,
        common::PROJECT_1,
        &common::all_actions(),
    )]);
    let base = common::spawn_app(pool, verifier).await;
    let client = client_for(&base);
    let ctx = ctx_for("tok-full", common::PROJECT_1);

    let created = client
        .create_artifact(
            create_request("System Architecture", common::PROJECT_1),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(created.lifecycle_state, "active");
    assert_eq!(created.etag, "\"r1\"");
    assert!(created.current_version_id.is_none());
    assert_eq!(created.tenant_id, common::TENANT);

    let fetched = client.get_artifact(created.id, &ctx).await.unwrap();
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.etag, created.etag);

    // PATCH：缺 If-Match → 400。
    let missing = client
        .patch_artifact(
            created.id,
            UpdateArtifactRequest {
                name: None,
                description: None,
                category: None,
            },
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(missing.code(), "INVALID_REQUEST", "If-Match required");

    // PATCH：错误 If-Match → 409 VERSION_CONFLICT。
    let conflict = client
        .patch_artifact(
            created.id,
            UpdateArtifactRequest {
                name: Some("new".into()),
                description: None,
                category: None,
            },
            &ctx.clone().if_match("\"r999\""),
        )
        .await
        .unwrap_err();
    assert_eq!(conflict.code(), "VERSION_CONFLICT");

    // PATCH：正确 If-Match → 200 + 新 ETag。
    let patched = client
        .patch_artifact(
            created.id,
            UpdateArtifactRequest {
                name: Some("Renamed".into()),
                description: None,
                category: None,
            },
            &ctx.clone().if_match(&created.etag),
        )
        .await
        .unwrap();
    assert_eq!(patched.name, "Renamed");
    assert_eq!(patched.etag, "\"r2\"");
}

#[sqlx::test(migrations = "../migrations")]
async fn idempotency_replay_and_reuse_conflict(pool: sqlx::PgPool) {
    let verifier = common::verifier_with(&[(
        "tok-full",
        common::TENANT,
        common::PROJECT_1,
        &[Action::ArtifactCreate, Action::ArtifactRead],
    )]);
    let base = common::spawn_app(pool, verifier).await;
    let client = client_for(&base);
    let ctx = ctx_for("tok-full", common::PROJECT_1).idempotency_key("key-stable-1");

    let first = client
        .create_artifact(create_request("Idempotent", common::PROJECT_1), &ctx)
        .await
        .unwrap();
    // 同 key 同 body → 重放同一资源。
    let replay = client
        .create_artifact(create_request("Idempotent", common::PROJECT_1), &ctx)
        .await
        .unwrap();
    assert_eq!(replay.id, first.id);

    // 同 key 不同 body → 409 IDEMPOTENCY_KEY_REUSED。
    let reused = client
        .create_artifact(create_request("Different", common::PROJECT_1), &ctx)
        .await
        .unwrap_err();
    assert_eq!(reused.code(), "IDEMPOTENCY_KEY_REUSED");
}

#[sqlx::test(migrations = "../migrations")]
async fn list_pagination_with_cursor(pool: sqlx::PgPool) {
    let verifier = common::verifier_with(&[(
        "tok-full",
        common::TENANT,
        common::PROJECT_1,
        &[Action::ArtifactCreate, Action::ArtifactRead],
    )]);
    let base = common::spawn_app(pool, verifier).await;
    let client = client_for(&base);
    let ctx = ctx_for("tok-full", common::PROJECT_1);

    for i in 0..3 {
        client
            .create_artifact(
                create_request(&format!("item-{i}"), common::PROJECT_1),
                &ctx,
            )
            .await
            .unwrap();
    }
    let page1 = client
        .list_artifacts(
            ListArtifactsQuery {
                limit: Some(2),
                cursor: None,
                artifact_type: None,
                lifecycle_state: None,
            },
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(page1.items.len(), 2);
    let cursor = page1.next_cursor.expect("next cursor for full page");
    let page2 = client
        .list_artifacts(
            ListArtifactsQuery {
                limit: Some(2),
                cursor: Some(cursor),
                artifact_type: None,
                lifecycle_state: None,
            },
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(page2.items.len(), 1);
    assert!(page2.next_cursor.is_none());
    // 无重复。
    let ids: std::collections::HashSet<_> = page1
        .items
        .iter()
        .chain(page2.items.iter())
        .map(|a| a.id)
        .collect();
    assert_eq!(ids.len(), 3);
}

#[sqlx::test(migrations = "../migrations")]
async fn tenant_project_boundary_negatives(pool: sqlx::PgPool) {
    // PROJECT_1 全动作；PROJECT_2 仅 read；TENANT_B 用于跨租户。
    let verifier = common::verifier_with(&[
        (
            "tok-p1",
            common::TENANT,
            common::PROJECT_1,
            &common::all_actions(),
        ),
        (
            "tok-p2",
            common::TENANT,
            common::PROJECT_2,
            &[Action::ArtifactRead],
        ),
    ]);
    let base = common::spawn_app(pool, verifier).await;
    let client = client_for(&base);
    let ctx_p1 = ctx_for("tok-p1", common::PROJECT_1);
    let ctx_p2 = ctx_for("tok-p2", common::PROJECT_2);

    let created = client
        .create_artifact(create_request("P1 only", common::PROJECT_1), &ctx_p1)
        .await
        .unwrap();

    // 跨项目读取 → 404（避免 ID 探测）。
    let cross = client.get_artifact(created.id, &ctx_p2).await.unwrap_err();
    assert_eq!(cross.code(), "RESOURCE_NOT_FOUND");

    // 请求体 project ≠ 凭证 project → 403 SCOPE_DENIED。
    let forged = client
        .create_artifact(create_request("forged", common::PROJECT_2), &ctx_p1)
        .await
        .unwrap_err();
    assert_eq!(forged.code(), "SCOPE_DENIED");

    // 缺动作（PROJECT_2 无 artifact:create）→ 403。
    let denied = client
        .create_artifact(create_request("no-action", common::PROJECT_2), &ctx_p2)
        .await
        .unwrap_err();
    assert_eq!(denied.code(), "SCOPE_DENIED");
}

#[sqlx::test(migrations = "../migrations")]
async fn current_version_pointer_invariant_via_api(pool: sqlx::PgPool) {
    let verifier = common::verifier_with(&[(
        "tok-full",
        common::TENANT,
        common::PROJECT_1,
        &common::all_actions(),
    )]);
    let base = common::spawn_app(pool.clone(), verifier).await;
    let client = client_for(&base);
    let ctx = ctx_for("tok-full", common::PROJECT_1);

    let artifact = client
        .create_artifact(create_request("versioned", common::PROJECT_1), &ctx)
        .await
        .unwrap();

    // 版本不存在 → 404。
    let missing = client
        .set_current_version(
            artifact.id,
            SetCurrentVersionRequest {
                version_id: uuid::Uuid::new_v4(),
                expected_current_version_id: None,
            },
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(missing.code(), "RESOURCE_NOT_FOUND");

    // pending 版本 → 409 VERSION_CONFLICT（只有 READY 可成为 current）。
    let pending = common::sql_insert_version(
        &pool,
        common::TENANT,
        common::PROJECT_1,
        artifact.id,
        "pending",
    )
    .await;
    let not_ready = client
        .set_current_version(
            artifact.id,
            SetCurrentVersionRequest {
                version_id: pending,
                expected_current_version_id: None,
            },
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(not_ready.code(), "VERSION_CONFLICT");

    // pending → ready（数据库触发器允许的转移）。
    sqlx::query("UPDATE artifact.artifact_version SET state='ready', ready_at=now() WHERE id=$1")
        .bind(pending)
        .execute(&pool)
        .await
        .unwrap();
    let updated = client
        .set_current_version(
            artifact.id,
            SetCurrentVersionRequest {
                version_id: pending,
                expected_current_version_id: None,
            },
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(updated.current_version_id, Some(pending));
    assert_eq!(updated.etag, "\"r2\"");

    // 乐观并发：expected_current_version_id 不匹配 → 409。
    let stale = client
        .set_current_version(
            artifact.id,
            SetCurrentVersionRequest {
                version_id: pending,
                expected_current_version_id: Some(uuid::Uuid::new_v4()),
            },
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(stale.code(), "VERSION_CONFLICT");
}

#[sqlx::test(migrations = "../migrations")]
async fn reference_create_remove(pool: sqlx::PgPool) {
    let verifier = common::verifier_with(&[(
        "tok-full",
        common::TENANT,
        common::PROJECT_1,
        &[
            Action::ArtifactRead,
            Action::ArtifactCreate,
            Action::ReferenceWrite,
            Action::ReferenceRemove,
        ],
    )]);
    let base = common::spawn_app(pool.clone(), verifier).await;
    let client = client_for(&base);
    let ctx = ctx_for("tok-full", common::PROJECT_1);

    let artifact = client
        .create_artifact(create_request("referenced", common::PROJECT_1), &ctx)
        .await
        .unwrap();

    let reference = client
        .create_reference(
            artifact.id,
            CreateReferenceRequest {
                owner_type: "adr".to_string(),
                owner_id: "adr-0001".to_string(),
                relation_type: "illustrates".to_string(),
            },
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(reference.owner_id, "adr-0001");

    // 非法 owner_type → 400。
    let bad = client
        .create_reference(
            artifact.id,
            CreateReferenceRequest {
                owner_type: "unknown-domain".to_string(),
                owner_id: "x".to_string(),
                relation_type: "illustrates".to_string(),
            },
            &ctx,
        )
        .await
        .unwrap_err();
    assert_eq!(bad.code(), "INVALID_REQUEST");

    // 解除成功；重复解除 → 404。
    client
        .remove_reference(artifact.id, reference.id, &ctx)
        .await
        .unwrap();
    let again = client
        .remove_reference(artifact.id, reference.id, &ctx)
        .await
        .unwrap_err();
    assert_eq!(again.code(), "RESOURCE_NOT_FOUND");
}
