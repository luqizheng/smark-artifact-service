//! Artifact Service 应用层：端口（ports）与应用服务。
//!
//! 本 crate 不依赖 sqlx / axum / 任何具体实现；数据库、file-service、
//! 委托验证的具体适配都在 artifact-infrastructure。

pub mod port;
pub mod service;

pub use port::audit::AuditSink;
pub use port::auth::{DelegationVerifier, VerificationError};
pub use port::idempotency::{IdempotencyOutcome, IdempotencyStore};

pub use port::repo::{
    ArtifactListFilter, ArtifactPatch, ArtifactRepository, GeneratorRepository, JobRepository,
    NewArtifact, ReferenceRepository, VersionRepository,
};
pub use port::storage::{ObjectInspection, StorageError, StoragePort, UploadIntent};
pub use port::worker::{FailOutcome, WorkerCoordinationPort};
pub use service::artifact::{
    ArtifactService, to_artifact_dto, to_generator_dto, to_job_dto, to_reference_dto,
    to_version_dto,
};
pub use service::generator::GeneratorService;
pub use service::job_input::{
    InputValidationError, MAX_DEPTH, MAX_EDGES, MAX_ENVELOPE_BYTES, MAX_NODES, MAX_TEXT_BYTES,
    validate_job_input_envelope,
};
