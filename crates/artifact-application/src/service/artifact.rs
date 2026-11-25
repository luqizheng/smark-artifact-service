//! Artifact 应用服务：编排授权动作校验、幂等、仓储与审计。
//!
//! 授权语义（ART-01 §4.3/§5.5）：tenant 一律取自委托凭证；请求侧
//! project_id 只是资源定位输入，与凭证 project 范围不一致即 SCOPE_DENIED。
//! API 层负责把「凭证 tenant + 请求 project」组装为 [`TenantScope`] 传入。

use crate::port::audit::AuditSink;
use crate::port::idempotency::IdempotencyStore;
use crate::port::repo::{
    ArtifactListFilter, ArtifactPatch, ArtifactRepository, GeneratorRepository, JobCancelOutcome,
    JobRepository, NewArtifact, ReferenceRepository, VersionRepository,
};
use crate::port::storage::{StorageError, StoragePort};
use artifact_contract::{
    ArtifactPage, ArtifactResponse, ArtifactVersionResponse, CreateArtifactRequest,
    CreateGenerationJobRequest, CreateReferenceRequest, CreateVersionSessionRequest,
    GenerationJobResponse, ListArtifactsQuery, ReferenceResponse, SetCurrentVersionRequest,
    UpdateArtifactRequest,
};
use artifact_domain::{
    Action, Artifact, ArtifactReference, ArtifactType, ArtifactVersion, Delegation, DomainError,
    GenerationJob, GeneratorDescriptor, GeneratorStatus, OwnerType, RelationType, TenantScope,
};
use base64::Engine as _;
use std::sync::Arc;
use uuid::Uuid;

pub const ENDPOINT_CREATE_ARTIFACT: &str = "POST /v1/artifacts";

/// 创建结果：fresh=首次创建；replayed=同幂等键同请求体的重放。
pub struct CreateOutcome<T> {
    pub resource: T,
    pub replayed: bool,
}

#[derive(Clone)]
pub struct ArtifactService {
    pub repo: Arc<dyn ArtifactRepository>,
    pub versions: Arc<dyn VersionRepository>,
    pub references: Arc<dyn ReferenceRepository>,
    pub jobs: Arc<dyn JobRepository>,
    pub generators: Arc<dyn GeneratorRepository>,
    pub storage: Arc<dyn StoragePort>,
    pub idempotency: Arc<dyn IdempotencyStore>,
    pub audit: Arc<dyn AuditSink>,
}

impl ArtifactService {
    // ── Artifact 元数据 ────────────────────────────────────────────────

    pub async fn create_artifact(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        request_id: Option<&str>,
        req: CreateArtifactRequest,
        idempotency_key: Option<String>,
        request_hash: String,
    ) -> Result<CreateOutcome<ArtifactResponse>, DomainError> {
        ArtifactType::parse(&req.artifact_type).ok_or_else(|| {
            DomainError::InvalidRequest(format!("unknown artifact_type '{}'", req.artifact_type))
        })?;
        if req.name.trim().is_empty() {
            return Err(DomainError::InvalidRequest("name must not be blank".into()));
        }
        delegation.require(Action::ArtifactCreate, scope)?;

        let reservation = match &idempotency_key {
            Some(key) => Some(
                self.idempotency
                    .reserve(
                        scope.tenant_id,
                        scope.project_id,
                        ENDPOINT_CREATE_ARTIFACT,
                        key,
                        &request_hash,
                    )
                    .await?,
            ),
            None => None,
        };
        if let Some(outcome) = &reservation
            && !outcome.fresh
        {
            if outcome.conflicting_hash {
                return Err(DomainError::IdempotencyKeyReused(
                    "this Idempotency-Key was already used with a different request body".into(),
                ));
            }
            if let Some(snapshot) = &outcome.replay_response {
                let dto: ArtifactResponse = serde_json::from_value(snapshot.clone())
                    .map_err(|e| DomainError::Internal(format!("idempotency snapshot: {e}")))?;
                return Ok(CreateOutcome {
                    resource: dto,
                    replayed: true,
                });
            }
            return Err(DomainError::IdempotencyKeyReused(
                "an identical request with this key is still in progress; retry later".into(),
            ));
        }

        let artifact = self
            .repo
            .insert(NewArtifact {
                tenant_id: scope.tenant_id,
                project_id: scope.project_id,
                artifact_key: req.artifact_key,
                name: req.name,
                description: req.description,
                artifact_type: req.artifact_type,
                category: req.category,
                created_by: delegation.actor.subject.clone(),
            })
            .await;
        let artifact = match artifact {
            Ok(a) => a,
            Err(err) => {
                // 业务失败：幂等记录以错误码收尾，同 key 同 body 重试可复得一致结果。
                if let Some(outcome) = &reservation {
                    let _ = self
                        .idempotency
                        .complete(
                            outcome.record_id,
                            serde_json::json!({ "error": err.code() }),
                            None,
                        )
                        .await;
                }
                return Err(err);
            }
        };
        let dto = to_artifact_dto(&artifact);
        self.audit_allowed(
            scope,
            delegation,
            "artifact.create",
            artifact.id,
            request_id,
        )
        .await;
        if let Some(outcome) = &reservation {
            self.idempotency
                .complete(
                    outcome.record_id,
                    serde_json::to_value(&dto).expect("artifact dto is serializable"),
                    Some(artifact.id),
                )
                .await?;
        }
        Ok(CreateOutcome {
            resource: dto,
            replayed: false,
        })
    }

    pub async fn get_artifact(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
    ) -> Result<ArtifactResponse, DomainError> {
        delegation.require(Action::ArtifactRead, scope)?;
        let artifact = self
            .repo
            .get((scope.tenant_id, scope.project_id), artifact_id)
            .await?;
        Ok(to_artifact_dto(&artifact))
    }

    pub async fn list_artifacts(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        query: ListArtifactsQuery,
    ) -> Result<ArtifactPage, DomainError> {
        delegation.require(Action::ArtifactRead, scope)?;
        let (before_created_at, before_id) = decode_cursor(query.cursor.as_deref())
            .map_err(|e| DomainError::InvalidRequest(format!("invalid cursor: {e}")))?;
        let filter = ArtifactListFilter {
            limit: query.limit.unwrap_or(50).clamp(1, 200),
            before_created_at,
            before_id,
            artifact_type: query.artifact_type,
            lifecycle_state: query.lifecycle_state,
        };
        let items = self
            .repo
            .list((scope.tenant_id, scope.project_id), &filter)
            .await?;
        let next_cursor = if items.len() as i64 == filter.limit {
            items
                .last()
                .map(|last| encode_cursor(last.created_at, last.id))
        } else {
            None
        };
        Ok(ArtifactPage {
            items: items.iter().map(to_artifact_dto).collect(),
            next_cursor,
        })
    }

    pub async fn patch_artifact(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
        expected_revision: i64,
        req: UpdateArtifactRequest,
        request_id: Option<&str>,
    ) -> Result<ArtifactResponse, DomainError> {
        delegation.require(Action::ArtifactUpdateMetadata, scope)?;
        if req.name.as_ref().is_some_and(|n| n.trim().is_empty()) {
            return Err(DomainError::InvalidRequest("name must not be blank".into()));
        }
        let artifact = self
            .repo
            .patch(
                (scope.tenant_id, scope.project_id),
                artifact_id,
                expected_revision,
                ArtifactPatch {
                    name: req.name,
                    description: req.description,
                    category: req.category,
                },
            )
            .await?;
        self.audit_allowed(
            scope,
            delegation,
            "artifact.update_metadata",
            artifact.id,
            request_id,
        )
        .await;
        Ok(to_artifact_dto(&artifact))
    }

    pub async fn set_current_version(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
        req: SetCurrentVersionRequest,
        request_id: Option<&str>,
    ) -> Result<ArtifactResponse, DomainError> {
        delegation.require(Action::VersionSetCurrent, scope)?;
        let artifact = self
            .repo
            .get((scope.tenant_id, scope.project_id), artifact_id)
            .await?;
        if let Some(expected) = req.expected_current_version_id
            && artifact.current_version_id != Some(expected)
        {
            return Err(DomainError::VersionConflict(
                "expected_current_version_id does not match the current pointer".into(),
            ));
        }
        let version = self
            .versions
            .get(
                (scope.tenant_id, scope.project_id),
                artifact_id,
                req.version_id,
            )
            .await?;
        if !version.can_be_current() {
            return Err(DomainError::VersionConflict(format!(
                "version {} state is '{}'; only READY versions can become current",
                version.id,
                version.state.as_str()
            )));
        }
        let updated = self
            .repo
            .set_current_version((scope.tenant_id, scope.project_id), artifact_id, version.id)
            .await?;
        self.audit_allowed(
            scope,
            delegation,
            "artifact.set_current_version",
            artifact_id,
            request_id,
        )
        .await;
        Ok(to_artifact_dto(&updated))
    }

    // ── Version 会话（P0 阻断：file-service 上传协议未落地） ──────────

    pub async fn create_version_session(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
        _req: CreateVersionSessionRequest,
    ) -> Result<ArtifactVersionResponse, DomainError> {
        delegation.require(Action::VersionCreate, scope)?;
        let artifact = self
            .repo
            .get((scope.tenant_id, scope.project_id), artifact_id)
            .await?;
        if !artifact.accepts_writes() {
            return Err(DomainError::InvalidRequest(
                "artifact is deleted and does not accept new versions".into(),
            ));
        }
        match self
            .storage
            .create_upload_intent(scope.tenant_id, scope.project_id, "artifact-source")
            .await
        {
            Err(StorageError::ProtocolBlocked(reason)) => {
                Err(DomainError::StorageUnavailable(reason))
            }
            Err(other) => Err(storage_to_domain(other)),
            // 不可达：生产 StoragePort 当前必然 ProtocolBlocked；一旦未来
            // file-service 协议就绪，必须先实现完整版本提交协议再放开本路径。
            Ok(_) => Err(DomainError::Internal(
                "storage accepted an upload intent but the version commit protocol is not implemented".into(),
            )),
        }
    }

    pub async fn commit_version_session(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
        version_id: Uuid,
    ) -> Result<ArtifactVersionResponse, DomainError> {
        delegation.require(Action::VersionCommit, scope)?;
        self.repo
            .get((scope.tenant_id, scope.project_id), artifact_id)
            .await?;
        let version = self
            .versions
            .get((scope.tenant_id, scope.project_id), artifact_id, version_id)
            .await?;
        match self
            .storage
            .inspect_object(
                version
                    .source_object_id
                    .as_deref()
                    .unwrap_or("artifact-source"),
            )
            .await
        {
            Err(StorageError::ProtocolBlocked(reason)) => {
                Err(DomainError::StorageUnavailable(reason))
            }
            Err(other) => Err(storage_to_domain(other)),
            Ok(_) => Err(DomainError::Internal(
                "storage inspection succeeded but the atomic commit protocol is not implemented"
                    .into(),
            )),
        }
    }

    // ── References ─────────────────────────────────────────────────────

    pub async fn create_reference(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
        req: CreateReferenceRequest,
        request_id: Option<&str>,
    ) -> Result<CreateOutcome<ReferenceResponse>, DomainError> {
        delegation.require(Action::ReferenceWrite, scope)?;
        let artifact = self
            .repo
            .get((scope.tenant_id, scope.project_id), artifact_id)
            .await?;
        if !artifact.accepts_writes() {
            return Err(DomainError::InvalidRequest(
                "artifact is deleted and does not accept new references".into(),
            ));
        }
        let owner_type = OwnerType::parse(&req.owner_type).ok_or_else(|| {
            DomainError::InvalidRequest(format!("unknown owner_type '{}'", req.owner_type))
        })?;
        let relation_type = RelationType::parse(&req.relation_type).ok_or_else(|| {
            DomainError::InvalidRequest(format!("unknown relation_type '{}'", req.relation_type))
        })?;
        if req.owner_id.trim().is_empty() {
            return Err(DomainError::InvalidRequest(
                "owner_id must not be blank".into(),
            ));
        }
        let reference = self
            .references
            .insert(
                (scope.tenant_id, scope.project_id),
                artifact_id,
                owner_type,
                &req.owner_id,
                relation_type,
                &delegation.actor.subject,
            )
            .await?;
        self.audit_allowed(
            scope,
            delegation,
            "reference.create",
            reference.id,
            request_id,
        )
        .await;
        Ok(CreateOutcome {
            resource: to_reference_dto(&reference),
            replayed: false,
        })
    }

    pub async fn remove_reference(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
        reference_id: Uuid,
        request_id: Option<&str>,
    ) -> Result<(), DomainError> {
        delegation.require(Action::ReferenceRemove, scope)?;
        self.references
            .soft_delete(
                (scope.tenant_id, scope.project_id),
                artifact_id,
                reference_id,
            )
            .await?;
        self.audit_allowed(
            scope,
            delegation,
            "reference.remove",
            reference_id,
            request_id,
        )
        .await;
        Ok(())
    }

    // ── GenerationJobs ─────────────────────────────────────────────────

    pub async fn request_generation(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
        req: CreateGenerationJobRequest,
        request_id: Option<&str>,
    ) -> Result<GenerationJobResponse, DomainError> {
        delegation.require(Action::GenerationRequest, scope)?;
        let artifact = self
            .repo
            .get((scope.tenant_id, scope.project_id), artifact_id)
            .await?;
        if !artifact.accepts_writes() {
            return Err(DomainError::InvalidRequest(
                "artifact is deleted and does not accept generation jobs".into(),
            ));
        }
        let generator = self
            .generators
            .get(&req.generator_id)
            .await
            .map_err(|e| match e {
                DomainError::NotFound => {
                    DomainError::InvalidRequest(format!("unknown generator '{}'", req.generator_id))
                }
                other => other,
            })?;
        if generator.status != GeneratorStatus::Available {
            let err = DomainError::GeneratorUnavailable(format!(
                "generator '{}' is '{}' — its adapter is not implemented in ART-02",
                generator.generator_id,
                generator.status.as_str()
            ));
            self.audit
                .record(denial_event(
                    "generation.request",
                    err.code(),
                    request_id,
                    Some(scope),
                    Some(delegation),
                ))
                .await
                .ok();
            return Err(err);
        }
        Err(DomainError::GeneratorUnavailable(
            "generator reports available but no worker is running".into(),
        ))
    }

    pub async fn get_job(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        job_id: Uuid,
    ) -> Result<GenerationJobResponse, DomainError> {
        delegation.require(Action::GenerationRead, scope)?;
        let job = self
            .jobs
            .get((scope.tenant_id, scope.project_id), job_id)
            .await?;
        Ok(to_job_dto(&job))
    }

    pub async fn cancel_job(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        job_id: Uuid,
        request_id: Option<&str>,
    ) -> Result<GenerationJobResponse, DomainError> {
        delegation.require(Action::GenerationCancel, scope)?;
        let (job, outcome) = self
            .jobs
            .cancel((scope.tenant_id, scope.project_id), job_id)
            .await?;
        // 终态 Job 上的取消是空操作，不记审计；其余两种结果都改变了 Job 的状态。
        if outcome != JobCancelOutcome::Terminal {
            self.audit_allowed(scope, delegation, "generation.cancel", job_id, request_id)
                .await;
        }
        Ok(to_job_dto(&job))
    }

    // ── 审计 ───────────────────────────────────────────────────────────

    async fn audit_allowed(
        &self,
        scope: &TenantScope,
        delegation: &Delegation,
        action: &str,
        object_id: Uuid,
        request_id: Option<&str>,
    ) {
        let event = artifact_domain::AuditEvent {
            id: Uuid::new_v4(),
            tenant_id: Some(scope.tenant_id),
            project_id: Some(scope.project_id),
            actor_subject: Some(delegation.actor.subject.clone()),
            actor_type: Some(actor_type_str(delegation)),
            action: action.to_string(),
            object_type: object_type_for(action).to_string(),
            object_id: Some(object_id),
            outcome: artifact_domain::AuditOutcome::Allowed,
            error_code: None,
            detail: serde_json::json!({ "delegation_id": delegation.delegation_id }),
            request_id: request_id.map(str::to_string),
            created_at: chrono::Utc::now(),
        };
        if let Err(e) = self.audit.record(event).await {
            tracing::warn!(action, error = %e, "audit record failed");
        }
    }
}

/// 供 API 层在授权拒绝时记录审计（ART-01 §5.4）。
pub fn denial_event(
    action: &str,
    error_code: &str,
    request_id: Option<&str>,
    scope: Option<&TenantScope>,
    delegation: Option<&Delegation>,
) -> artifact_domain::AuditEvent {
    artifact_domain::AuditEvent {
        id: Uuid::new_v4(),
        tenant_id: scope.map(|s| s.tenant_id),
        project_id: scope.map(|s| s.project_id),
        actor_subject: delegation.map(|d| d.actor.subject.clone()),
        actor_type: delegation.map(actor_type_str),
        action: action.to_string(),
        object_type: "auth".to_string(),
        object_id: None,
        outcome: artifact_domain::AuditOutcome::Denied,
        error_code: Some(error_code.to_string()),
        detail: delegation
            .map(|d| serde_json::json!({ "delegation_id": d.delegation_id }))
            .unwrap_or_else(|| serde_json::json!({})),
        request_id: request_id.map(str::to_string),
        created_at: chrono::Utc::now(),
    }
}

fn actor_type_str(delegation: &Delegation) -> String {
    format!("{:?}", delegation.actor.actor_type).to_lowercase()
}

fn object_type_for(action: &str) -> &'static str {
    match action.split('.').next().unwrap_or("") {
        "artifact" => "artifact",
        "reference" => "reference",
        "version" => "artifact_version",
        _ => "generation_job",
    }
}

pub fn storage_to_domain(err: StorageError) -> DomainError {
    match err {
        StorageError::ProtocolBlocked(reason) => DomainError::StorageUnavailable(reason),
        StorageError::Unavailable(reason) => DomainError::StorageUnavailable(reason),
        StorageError::ObjectNotFound => DomainError::NotFound,
    }
}

// ── DTO 转换 ───────────────────────────────────────────────────────────

pub fn to_artifact_dto(a: &Artifact) -> ArtifactResponse {
    ArtifactResponse {
        id: a.id,
        tenant_id: a.tenant_id,
        project_id: a.project_id,
        artifact_key: a.artifact_key.clone(),
        name: a.name.clone(),
        description: a.description.clone(),
        artifact_type: a.artifact_type.as_str().to_string(),
        category: a.category.clone(),
        lifecycle_state: a.lifecycle_state.as_str().to_string(),
        current_version_id: a.current_version_id,
        created_by: a.created_by.clone(),
        created_at: a.created_at,
        updated_at: a.updated_at,
        deleted_at: a.deleted_at,
        etag: format!("\"r{}\"", a.row_revision),
    }
}

pub fn to_version_dto(v: &ArtifactVersion) -> ArtifactVersionResponse {
    ArtifactVersionResponse {
        id: v.id,
        artifact_id: v.artifact_id,
        version_number: v.version_number,
        state: v.state.as_str().to_string(),
        source_format: v.source_format.clone(),
        source_object_id: v.source_object_id.clone(),
        source_sha256: v.source_sha256.clone(),
        generator_id: v.generator_id.clone(),
        generator_version: v.generator_version.clone(),
        adapter_version: v.adapter_version.clone(),
        manifest: v.manifest.clone(),
        created_by: v.created_by.clone(),
        created_at: v.created_at,
        ready_at: v.ready_at,
        failure_code: v.failure_code.clone(),
    }
}

pub fn to_reference_dto(r: &ArtifactReference) -> ReferenceResponse {
    ReferenceResponse {
        id: r.id,
        artifact_id: r.artifact_id,
        owner_type: r.owner_type.as_str().to_string(),
        owner_id: r.owner_id.clone(),
        relation_type: r.relation_type.as_str().to_string(),
        created_by: r.created_by.clone(),
        created_at: r.created_at,
    }
}

pub fn to_job_dto(j: &GenerationJob) -> GenerationJobResponse {
    GenerationJobResponse {
        id: j.id,
        artifact_id: j.artifact_id,
        generator_id: j.generator_id.clone(),
        status: j.status.as_str().to_string(),
        error_code: j.error_code.clone(),
        attempt_count: j.attempt_count,
        max_attempts: j.max_attempts,
        cancel_requested: j.cancel_requested(),
        created_at: j.created_at,
        poll_url: format!("/v1/generation-jobs/{}", j.id),
    }
}

pub fn to_generator_dto(g: &GeneratorDescriptor) -> artifact_contract::GeneratorResponse {
    artifact_contract::GeneratorResponse {
        generator_id: g.generator_id.clone(),
        display_name: g.display_name.clone(),
        version: g.version.clone(),
        adapter_version: g.adapter_version.clone(),
        supported_input_formats: g.supported_input_formats.clone(),
        supported_output_formats: g.supported_output_formats.clone(),
        capabilities: g.capabilities.clone(),
        status: g.status.as_str().to_string(),
        status_reason: g.status_reason.clone(),
    }
}

// ── cursor：base64url("{created_at_rfc3339}|{artifact_id}") ────────────

pub fn encode_cursor(created_at: chrono::DateTime<chrono::Utc>, id: Uuid) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(format!("{}|{id}", created_at.to_rfc3339()))
}

fn decode_cursor(
    cursor: Option<&str>,
) -> Result<(Option<chrono::DateTime<chrono::Utc>>, Option<Uuid>), String> {
    let Some(cursor) = cursor else {
        return Ok((None, None));
    };
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|e| format!("not base64url: {e}"))?;
    let text = String::from_utf8(raw).map_err(|e| format!("not utf-8: {e}"))?;
    let (ts, id) = text.split_once('|').ok_or("missing '|' separator")?;
    let created_at = chrono::DateTime::parse_from_rfc3339(ts)
        .map_err(|e| format!("bad timestamp: {e}"))?
        .with_timezone(&chrono::Utc);
    let id = Uuid::parse_str(id).map_err(|e| format!("bad id: {e}"))?;
    Ok((Some(created_at), Some(id)))
}
