//! Artifact Service 应用层：端口（ports）与应用服务。
//!
//! 本 crate 不依赖 sqlx / axum / 任何具体实现；数据库、file-service、
//! 委托验证的具体适配都在 artifact-infrastructure。

pub mod port;
pub mod service;

pub use port::audit::AuditSink;
pub use port::auth::{DelegationVerifier, VerificationError};
pub use port::idempotency::{IdempotencyOutcome, IdempotencyStore};
pub use port::keyed_create::{
    IdempotencyInput, KeyedArtifactCreateError, KeyedArtifactCreateOutcome,
    KeyedArtifactCreationPort, ValidatedCreateArtifactRequest,
};
pub use port::keyed_job_create::{
    KeyedGenerationJobCreatePort, KeyedJobCreateError, KeyedJobCreateOutcome,
    ValidatedCreateGenerationJobRequest,
};
pub use port::revocation::{
    DelegationRevocationChecker, FailClosedRevocationChecker, RevocationCheckUnavailable,
};
pub use port::unkeyed_create::{
    UnkeyedArtifactCreate, UnkeyedArtifactCreateOutcome, UnkeyedArtifactCreationPort,
};

pub use port::repo::{
    ArtifactListFilter, ArtifactPatch, ArtifactRepository, GeneratorRepository, JobCancelOutcome,
    JobRepository, NewArtifact, NewVersion, ReferenceRepository, VersionCommitOutput,
    VersionRepository,
};
pub use port::storage::{ObjectInspection, StorageError, StoragePort, UploadIntent};
pub use port::worker::{
    AdapterCapabilities, CommitOutputOutcome, CompleteOutcome, ConfirmCancelOutcome, FailOutcome,
    GenerationOutputFacts, HeartbeatOutcome,
    WorkerCoordinationPort,
};
pub use service::artifact::{
    ArtifactCreateOutcome, ArtifactService, CreateResolution, to_artifact_dto, to_generator_dto,
    to_job_dto, to_reference_dto, to_version_dto,
};
pub use service::creation_fingerprint::{
    CURRENT_FINGERPRINT_VERSION, FingerprintError, canonical_fingerprint, current_fingerprint,
    normalize_optional_text, normalize_text,
};
pub use service::delegation_request::{
    REQUEST_HASH_ALGORITHM_V1, assert_delegation_matches_request, canonical_request_fingerprint,
};
pub use service::generation_fingerprint::{
    CURRENT_JOB_FINGERPRINT_VERSION, GENERATION_JOB_CREATE_DOMAIN, JobFingerprintError,
    canonical_job_document, canonical_job_fingerprint,
};
pub use service::generator::GeneratorService;
pub use service::job_input::{
    INPUT_SCHEMA, INPUT_SCHEMA_VERSION, InputValidationError, MAX_COLLECTION_ITEMS, MAX_DEPTH,
    MAX_EDGES, MAX_ENVELOPE_BYTES, MAX_JSON_NODES, MAX_NODES, MAX_TEXT_BYTES,
    validate_job_input_envelope,
};
pub use service::operation_binding::{
    WriteOperationBinding, WriteOperationType, enforce_write_binding,
};
pub use service::worker::{
    MAX_DIAGNOSTIC_CHARS, is_valid_error_code, run_coordination_maintenance, sanitize_diagnostic,
};
