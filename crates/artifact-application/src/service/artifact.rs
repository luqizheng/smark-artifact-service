//! Artifact 应用服务：编排授权动作校验、幂等、仓储与审计。
//!
//! 授权语义（ART-01 §4.3/§5.5）：tenant 一律取自委托凭证；请求侧
//! project_id 只是资源定位输入，与凭证 project 范围不一致即 SCOPE_DENIED。
//! API 层负责把「凭证 tenant + 请求 project」组装为 [`TenantScope`] 传入。

use crate::port::audit::AuditSink;
use crate::port::idempotency::IdempotencyStore;
use crate::port::keyed_create::{
    IdempotencyInput, KeyedArtifactCreateOutcome, KeyedArtifactCreationPort,
    ValidatedCreateArtifactRequest,
};
use crate::port::repo::{
    ArtifactListFilter, ArtifactPatch, ArtifactRepository, GeneratorRepository, JobCancelOutcome,
    JobRepository, NewArtifact, NewVersion, ReferenceRepository, VersionCommitOutput,
    VersionRepository,
};
use crate::port::storage::{StorageError, StoragePort};
use crate::port::unkeyed_create::{
    UnkeyedArtifactCreate, UnkeyedArtifactCreateOutcome, UnkeyedArtifactCreationPort,
};
use crate::service::operation_binding::{WriteOperationBinding, enforce_write_binding};
use artifact_contract::{
    ArtifactPage, ArtifactResponse, ArtifactVersionResponse, CreateArtifactRequest,
    CreateGenerationJobRequest, CreateReferenceRequest, CreateVersionSessionRequest,
    GenerationJobResponse, ListArtifactsQuery, ReferenceResponse, SetCurrentVersionRequest,
    UpdateArtifactRequest,
};
use artifact_domain::{
    Action, Artifact, ArtifactReference, ArtifactType, ArtifactVersion, Delegation, DomainError,
    GenerationJob, GeneratorDescriptor, GeneratorStatus, OwnerType, RelationType, TenantScope,
    VersionState,
};
use base64::Engine as _;
use std::sync::Arc;
use uuid::Uuid;

pub const ENDPOINT_CREATE_ARTIFACT: &str = "POST /v1/artifacts";

/// 创建结果的稳定解析语义（ART-04E §7）。HTTP 层以 `X-Artifact-Resolution`
/// 暴露；`replayed` 仅为兼容既有 `x-idempotent-replay` 行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateResolution {
    Created,
    ExistingKey,
    IdempotencyReplay,
}

impl CreateResolution {
    pub fn as_str(self) -> &'static str {
        match self {
            CreateResolution::Created => "created",
            CreateResolution::ExistingKey => "existing_key",
            CreateResolution::IdempotencyReplay => "idempotency_replay",
        }
    }
}

/// 创建结果：fresh=首次创建；replayed=同幂等键同请求体的重放。
pub struct CreateOutcome<T> {
    pub resource: T,
    pub replayed: bool,
}

/// Artifact 创建结果：在通用创建结果上增加稳定解析语义。
pub struct ArtifactCreateOutcome {
    pub resource: ArtifactResponse,
    pub replayed: bool,
    pub resolution: CreateResolution,
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
    pub keyed_create: Arc<dyn KeyedArtifactCreationPort>,
    pub unkeyed_create: Arc<dyn UnkeyedArtifactCreationPort>,
    pub audit: Arc<dyn AuditSink>,
}

impl ArtifactService {
    // ── Artifact 元数据 ────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub async fn create_artifact(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        request_id: Option<&str>,
        req: CreateArtifactRequest,
        idempotency_key: Option<String>,
        request_hash: String,
        binding: &WriteOperationBinding,
    ) -> Result<ArtifactCreateOutcome, DomainError> {
        // 授权 → V10 操作绑定（WP-04：全部写路径副作用之前强制），再输入校验。
        delegation.require(Action::ArtifactCreate, scope)?;
        enforce_write_binding(delegation, binding)?;
        ArtifactType::parse(&req.artifact_type).ok_or_else(|| {
            DomainError::InvalidRequest(format!("unknown artifact_type '{}'", req.artifact_type))
        })?;
        if req.name.trim().is_empty() {
            return Err(DomainError::InvalidRequest("name must not be blank".into()));
        }

        // AS-1：带 artifact_key 的创建走持久化创建身份 + 单事务端口。
        // 不得让 unkeyed 旧流程意外进入新路径。
        if let Some(artifact_key) = req.artifact_key.clone() {
            return self
                .create_keyed_artifact(
                    delegation,
                    scope,
                    request_id,
                    req,
                    artifact_key,
                    idempotency_key,
                    request_hash,
                )
                .await;
        }

        // unkeyed + Idempotency-Key：单事务端口（WP-04 G2-1/G2-2 修复——
        // 幂等决定与副作用原子提交；业务失败不留任何幂等痕迹）。
        if let Some(key) = idempotency_key.clone() {
            let unkeyed = UnkeyedArtifactCreate {
                tenant_id: scope.tenant_id,
                project_id: scope.project_id,
                name: req.name,
                description: req.description,
                artifact_type: req.artifact_type,
                category: req.category,
                created_by: delegation.actor.subject.clone(),
            };
            let idempotency = IdempotencyInput {
                endpoint: ENDPOINT_CREATE_ARTIFACT.to_string(),
                key,
                request_hash,
            };
            let outcome = self
                .unkeyed_create
                .create_or_replay_unkeyed(unkeyed, idempotency)
                .await?;
            let (artifact, resolution, replayed) = match outcome {
                UnkeyedArtifactCreateOutcome::Created(artifact) => {
                    (artifact, CreateResolution::Created, false)
                }
                UnkeyedArtifactCreateOutcome::Replayed(artifact) => {
                    (artifact, CreateResolution::IdempotencyReplay, true)
                }
            };
            let dto = to_artifact_dto(&artifact);
            if !replayed {
                self.audit_allowed(
                    scope,
                    delegation,
                    "artifact.create",
                    artifact.id,
                    request_id,
                )
                .await;
            }
            return Ok(ArtifactCreateOutcome {
                resource: dto,
                replayed,
                resolution,
            });
        }

        // unkeyed 且无 Idempotency-Key：非幂等直通（矩阵分类：重试产生重复
        // Artifact；该限制在 ROUTE-IDEMPOTENCY-MATRIX 中明示）。
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
            .await?;
        let dto = to_artifact_dto(&artifact);
        self.audit_allowed(
            scope,
            delegation,
            "artifact.create",
            artifact.id,
            request_id,
        )
        .await;
        Ok(ArtifactCreateOutcome {
            resource: dto,
            replayed: false,
            resolution: CreateResolution::Created,
        })
    }

    /// AS-1 keyed create/resolve。授权已在调用方完成且发生在任何重放之前；
    /// 端口负责单事务内的幂等解析、Artifact 插入与持久化身份写入。
    #[allow(clippy::too_many_arguments)]
    async fn create_keyed_artifact(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        request_id: Option<&str>,
        req: CreateArtifactRequest,
        artifact_key: String,
        idempotency_key: Option<String>,
        request_hash: String,
    ) -> Result<ArtifactCreateOutcome, DomainError> {
        let validated = ValidatedCreateArtifactRequest::try_new(
            scope,
            artifact_key,
            req.name,
            req.description,
            req.artifact_type,
            req.category,
            delegation.actor.subject.clone(),
        )?;
        let idempotency = idempotency_key.map(|key| IdempotencyInput {
            endpoint: ENDPOINT_CREATE_ARTIFACT.to_string(),
            key,
            request_hash,
        });

        let outcome = match self
            .keyed_create
            .create_or_resolve_keyed(validated, idempotency)
            .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                if matches!(
                    err,
                    crate::port::keyed_create::KeyedArtifactCreateError::IntegrityFailure(_)
                ) {
                    tracing::error!(
                        code = "artifact_creation_integrity",
                        error = %err,
                        "keyed artifact creation failed closed: durable state is inconsistent"
                    );
                }
                return Err(err.to_domain_error());
            }
        };

        let (artifact, resolution) = match outcome {
            KeyedArtifactCreateOutcome::Created(artifact) => (artifact, CreateResolution::Created),
            KeyedArtifactCreateOutcome::ResolvedExisting(artifact) => {
                (artifact, CreateResolution::ExistingKey)
            }
            KeyedArtifactCreateOutcome::ReplayedByIdempotency(artifact) => {
                (artifact, CreateResolution::IdempotencyReplay)
            }
        };

        let dto = to_artifact_dto(&artifact);
        if resolution != CreateResolution::IdempotencyReplay {
            self.audit_allowed(
                scope,
                delegation,
                "artifact.create",
                artifact.id,
                request_id,
            )
            .await;
        }
        Ok(ArtifactCreateOutcome {
            resource: dto,
            replayed: resolution == CreateResolution::IdempotencyReplay,
            resolution,
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

    /// 版本详情读取（WP-03 R2-F-03 修复：动作检查前置于任何数据读取，
    /// Q3"所有受保护 API 含 GET 均要求 delegation"）。
    pub async fn get_version(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
        version_id: Uuid,
    ) -> Result<ArtifactVersionResponse, DomainError> {
        delegation.require(Action::VersionRead, scope)?;
        let version = self
            .versions
            .get((scope.tenant_id, scope.project_id), artifact_id, version_id)
            .await?;
        Ok(to_version_dto(&version))
    }

    /// 版本列表读取（WP-03 R2-F-03 修复：`version:read` 动作强制 + 范围谓词）。
    pub async fn list_versions(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
        limit: i64,
    ) -> Result<Vec<ArtifactVersionResponse>, DomainError> {
        delegation.require(Action::VersionRead, scope)?;
        let versions = self
            .versions
            .list((scope.tenant_id, scope.project_id), artifact_id, limit)
            .await?;
        Ok(versions.iter().map(to_version_dto).collect())
    }

    /// 引用列表读取（WP-03 R2-F-03 修复）。动作映射依据：冻结契约 Q4 固定
    /// 15 动作、无 `reference:read`；引用是 Artifact 的内容元数据，读取映射到
    /// `artifact:read`（精确动作，非通配），已在契约 §3.1.1 读端点动作表中登记。
    pub async fn list_references(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
    ) -> Result<Vec<ReferenceResponse>, DomainError> {
        delegation.require(Action::ArtifactRead, scope)?;
        let references = self
            .references
            .list((scope.tenant_id, scope.project_id), artifact_id, 100)
            .await?;
        Ok(references.iter().map(to_reference_dto).collect())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn patch_artifact(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
        expected_revision: i64,
        req: UpdateArtifactRequest,
        request_id: Option<&str>,
        binding: &WriteOperationBinding,
    ) -> Result<ArtifactResponse, DomainError> {
        delegation.require(Action::ArtifactUpdateMetadata, scope)?;
        enforce_write_binding(delegation, binding)?;
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
        binding: &WriteOperationBinding,
    ) -> Result<ArtifactResponse, DomainError> {
        delegation.require(Action::VersionSetCurrent, scope)?;
        enforce_write_binding(delegation, binding)?;
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

    // ── Version 会话（WP-AS-05：file-service 对象 + 提交协议） ──────────
    //
    // 契约依据（WP v0.3 §12）：LMMS 无 upload-intent/finalize（P1）。流程：
    // 调用方先把对象上传到 file-service（自行持身份），再以
    // `source_object_id`+`source_sha256` 创建 pending 会话；commit 时 AS
    // 下载重算 sha256（"校验"）并在单事务写入 output + pending→ready。
    // 失败路径不删除调用方对象（重试幂等）；补偿删除仅用于 AS 自身上传
    // 的输出对象（Worker，WP-AS-06）。

    pub async fn create_version_session(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
        req: CreateVersionSessionRequest,
        binding: &WriteOperationBinding,
        request_id: Option<&str>,
    ) -> Result<ArtifactVersionResponse, DomainError> {
        delegation.require(Action::VersionCreate, scope)?;
        enforce_write_binding(delegation, binding)?;
        let artifact = self
            .repo
            .get((scope.tenant_id, scope.project_id), artifact_id)
            .await?;
        if !artifact.accepts_writes() {
            return Err(DomainError::InvalidRequest(
                "artifact is deleted and does not accept new versions".into(),
            ));
        }
        let object_id = req
            .source_object_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                DomainError::InvalidRequest(
                    "source_object_id is required (upload the object to file-service first)".into(),
                )
            })?;
        let declared_sha = normalize_sha256(req.source_sha256.as_deref())?;
        let lmms_tid = object_plane_tid(delegation)?;
        // fail-early：对象必须已存在且在调用方租户范围内（404 不泄露存在性）。
        let head = self
            .storage
            .head_object(lmms_tid, object_id)
            .await
            .map_err(storage_to_domain)?;
        if head.size_bytes <= 0 {
            return Err(DomainError::InvalidRequest("source object is empty".into()));
        }
        let version = self
            .versions
            .insert(
                (scope.tenant_id, scope.project_id),
                NewVersion {
                    tenant_id: scope.tenant_id,
                    project_id: scope.project_id,
                    artifact_id,
                    source_format: req.source_format.clone(),
                    source_object_id: object_id.to_owned(),
                    source_sha256: declared_sha,
                    created_by: delegation.actor.subject.clone(),
                },
            )
            .await?;
        self.audit_allowed(scope, delegation, "version.create", version.id, request_id)
            .await;
        Ok(to_version_dto(&version))
    }

    pub async fn commit_version_session(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
        version_id: Uuid,
        binding: &WriteOperationBinding,
        request_id: Option<&str>,
    ) -> Result<ArtifactVersionResponse, DomainError> {
        delegation.require(Action::VersionCommit, scope)?;
        enforce_write_binding(delegation, binding)?;
        self.repo
            .get((scope.tenant_id, scope.project_id), artifact_id)
            .await?;
        let version = self
            .versions
            .get((scope.tenant_id, scope.project_id), artifact_id, version_id)
            .await?;
        if version.state != VersionState::Pending {
            return Err(DomainError::VersionConflict(format!(
                "version is in state '{}' and cannot be committed",
                version.state.as_str()
            )));
        }
        let declared_sha = version.source_sha256.clone().ok_or_else(|| {
            DomainError::InvalidRequest("version session has no declared source_sha256".into())
        })?;
        let object_id = version.source_object_id.clone().ok_or_else(|| {
            DomainError::Internal("pending version is missing its source object id".into())
        })?;
        let lmms_tid = object_plane_tid(delegation)?;
        // 校验：下载对象并重算 sha256，与会话声明比对（file-service 不提供
        // hash attestation——P1 差异，本实现即契约"校验"步骤）。
        let inspection = self
            .storage
            .inspect_object(lmms_tid, &object_id)
            .await
            .map_err(storage_to_domain)?;
        let actual_sha = inspection.sha256.clone().ok_or_else(|| {
            DomainError::Internal("storage inspection did not produce sha256".into())
        })?;
        if !constant_sha_eq(&actual_sha, &declared_sha) {
            return Err(DomainError::InvalidRequest(format!(
                "source object sha256 mismatch (declared {declared_sha}, actual {actual_sha})"
            )));
        }
        if inspection.size_bytes <= 0 {
            return Err(DomainError::InvalidRequest("source object is empty".into()));
        }
        // finalize 近似：LMMS 无 finalize 协议，no-op 返回原 id（P1 差异）。
        let finalized = self
            .storage
            .copy_or_finalize_object(lmms_tid, &object_id)
            .await
            .map_err(storage_to_domain)?;
        let committed = self
            .versions
            .commit_ready(
                (scope.tenant_id, scope.project_id),
                artifact_id,
                version_id,
                VersionCommitOutput {
                    format: version
                        .source_format
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                    object_id: finalized,
                    size_bytes: inspection.size_bytes,
                    sha256: actual_sha,
                    content_type: Some(inspection.detected_mime).filter(|s| !s.is_empty()),
                },
            )
            .await?;
        self.audit_allowed(scope, delegation, "version.commit", version_id, request_id)
            .await;
        Ok(to_version_dto(&committed))
    }

    // ── References ─────────────────────────────────────────────────────

    pub async fn create_reference(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        artifact_id: Uuid,
        req: CreateReferenceRequest,
        request_id: Option<&str>,
        binding: &WriteOperationBinding,
    ) -> Result<CreateOutcome<ReferenceResponse>, DomainError> {
        delegation.require(Action::ReferenceWrite, scope)?;
        enforce_write_binding(delegation, binding)?;
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
        binding: &WriteOperationBinding,
    ) -> Result<(), DomainError> {
        delegation.require(Action::ReferenceRemove, scope)?;
        enforce_write_binding(delegation, binding)?;
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
        binding: &WriteOperationBinding,
    ) -> Result<GenerationJobResponse, DomainError> {
        delegation.require(Action::GenerationRequest, scope)?;
        // V10：body caller_request_id 与 token claim 交叉比对 + v1 指纹绑定
        // （WP-04；先于 generator 注册表查询与任何可能的 job 写入）。
        enforce_write_binding(delegation, binding)?;
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

    /// Caller Request Lookup（ART-04G v3 §5.4）：与 get_job 相同的身份验证与授权
    /// 模型（delegation tenant scope + X-Project-Id project scope +
    /// `Action::GenerationRead`）。查询谓词强制 tenant + project + caller；
    /// 跨 scope 与不存在统一返回 NotFound（不泄露存在性），且绝不修改状态。
    pub async fn get_job_by_caller_request(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        caller_request_id: Uuid,
    ) -> Result<GenerationJobResponse, DomainError> {
        delegation.require(Action::GenerationRead, scope)?;
        let job = self
            .jobs
            .get_by_caller_request((scope.tenant_id, scope.project_id), caller_request_id)
            .await?
            .ok_or(DomainError::NotFound)?;
        Ok(to_job_dto(&job))
    }

    pub async fn cancel_job(
        &self,
        delegation: &Delegation,
        scope: &TenantScope,
        job_id: Uuid,
        request_id: Option<&str>,
        binding: &WriteOperationBinding,
    ) -> Result<GenerationJobResponse, DomainError> {
        delegation.require(Action::GenerationCancel, scope)?;
        // V10：含契约冻结语法 `generation_job:{job_id}` 的 operation_id 比对
        // （契约 §5.3；先于受控取消函数调用）。
        enforce_write_binding(delegation, binding)?;
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

/// 会话声明的对象指纹：64 位十六进制（大小写归一为小写）。
fn normalize_sha256(raw: Option<&str>) -> Result<String, DomainError> {
    let trimmed = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            DomainError::InvalidRequest("source_sha256 is required (64 hex characters)".into())
        })?;
    let lower = trimmed.to_ascii_lowercase();
    if lower.len() != 64 || !lower.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(DomainError::InvalidRequest(
            "source_sha256 must be exactly 64 hex characters".into(),
        ));
    }
    Ok(lower)
}

/// 指纹比对（声明 vs 重算）；定长等价比较（sha256 十六进制，大小写不敏感）。
fn constant_sha_eq(actual: &str, declared: &str) -> bool {
    actual.eq_ignore_ascii_case(declared)
}

/// 对象面出站路由的 LMMS 数字 tid（WP-AS-05）：来自委托 claim 追溯字段。
/// 缺失 → fail-closed 503（新令牌携带 lmms_tid 后重试可解）。
fn object_plane_tid(delegation: &Delegation) -> Result<i64, DomainError> {
    delegation.lmms_tid_i64().ok_or_else(|| {
        DomainError::StorageUnavailable(
            "delegation lacks lmms_tid traceability claim; object-plane \
             tenant routing is unavailable (fail-closed)"
                .into(),
        )
    })
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
        caller_request_id: j.caller_request_id,
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
