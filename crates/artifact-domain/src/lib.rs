//! Artifact Service 领域模型。
//!
//! 权威来源：AI-PM `ART-01-artifact-service-api-and-data-model-v1.0.md`（V1 契约）与
//! `ADR-ART-001`。本 crate 只承载纯领域规则（实体、状态机、不变量、动作集合），
//! 不依赖任何数据库、HTTP 或 AI-PM 业务 crate。

pub mod action;
pub mod artifact;
pub mod audit;
pub mod error;
pub mod generator;
pub mod job;
pub mod output;
pub mod reference;
pub mod scope;
pub mod version;

pub use action::{Action, Delegation};
pub use artifact::{Artifact, ArtifactType, LifecycleState};
pub use audit::{AuditEvent, AuditOutcome};
pub use error::DomainError;
pub use generator::{GeneratorDescriptor, GeneratorStatus};
pub use job::{GenerationJob, JobState};
pub use reference::{ArtifactReference, OwnerType, RelationType};
pub use scope::{Actor, ActorType, TenantScope};
pub use version::{ArtifactVersion, VersionState};
