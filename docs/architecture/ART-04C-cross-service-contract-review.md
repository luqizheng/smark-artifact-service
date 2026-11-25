# ART-04C — Cross-Service Contract Review (ART-04 / ART-04A / ART-04B)

> **Decision:** Changes Required / Not approved for implementation.
> **Date:** 2026-11-25
> **Scope:** Independent cross-review of the Artifact Service GenerationJob contract and AI-PM Visualization mapping/saga proposal. Static inspection only; no implementation changes.

## 1. Executive result

The high-level authority split is sound:
- AI-PM owns business intent, Visualization identity, VizSpec semantics, approvals and business visibility.
- Artifact Service owns generic Artifact/Version/Output/GenerationJob technical state.
- Worker executes only the job it claims.
- LMMS remains the file and identity substrate.

However, the current ART-04B saga is not yet safely replayable at every remote step. Two P1 contract mismatches were confirmed against Artifact Service main, and one additional P1 crash-recovery gap exists in the newly proposed stable-key create flow.

## 2. Findings

### AR04C-01 — P1: viz_artifact is not a supported Reference owner_type

ART-04B §5 proposes owner_type = "viz_artifact" and owner_id = viz.artifact.id.

Current Artifact Service implementation does not support that value:
- migrations/0001_init.sql constrains artifact_reference.owner_type to requirement/design/adr/task/knowledge/project.
- crates/artifact-domain/src/reference.rs parses only those same values.
- ArtifactService::create_reference rejects an unknown owner type before insertion.

Therefore the proposed request is rejected. Do not add viz_artifact to the generic service merely to encode one consumer's business type unless that generic vocabulary extension is independently justified and reviewed.

**Required correction:**
- Remove the proposed per-viz.artifact Reference creation from ART-04B.
- The AI-PM version-binding table remains the authoritative per-business-version association.
- If a generic Reference is genuinely useful, create at most a project-level Reference using the already-supported owner_type=project and the project's stable ID, with a relation type approved by ART-01. It is optional and must not be mistaken for per-version provenance.
- Alternatively, propose a generic opaque external-owner reference contract in a separate API design; do not silently invent a value in AI-PM.

### AR04C-02 — P1: Reference replay behavior in OpenAPI does not match implementation

The current OpenAPI describes repeated creation of the same Reference as returning the existing record. The database has a partial unique index over active reference identity:
(tenant_id, project_id, artifact_id, owner_type, owner_id, relation_type).

But the current implementation:
- ArtifactService::create_reference always calls ReferenceRepository::insert;
- PgReferenceRepository::insert performs a plain INSERT;
- the API handler always returns HTTP 201 on success;
- a duplicate active tuple reaches the unique constraint and becomes an error; no existing row is re-read and returned.

Thus the documentation's "same reference repeated creation returns existing record" is not true of the current implementation. The uniqueness constraint prevents duplicate rows, but uniqueness alone is not an idempotent response contract.

**Required correction:**
- Either implement create-or-return-existing semantics with a race-safe insert/on-conflict/read path and contract tests, or explicitly document 409 conflict and give AI-PM a deterministic exact lookup/reconciliation path.
- Until one of these is approved and implemented, ART-04B must not state that retrying Reference creation is safe or idempotent.
- The current list endpoint returns at most 100 active References and has no cursor/filter; it is not a scalable exact lookup contract for an Artifact with more than 100 References.

### AR04C-03 — P1: artifact_key recovery must bypass a stale Idempotency-Key reservation

ART-04A §8 correctly proposes a stable project-scoped artifact_key, exact lookup, and same-key create convergence under the existing unique index.

But the current Artifact create path first reserves Idempotency-Key, then inserts the Artifact, then completes the reservation in separate operations:
- ArtifactService::create_artifact calls IdempotencyStore::reserve before ArtifactRepository::insert;
- if the process dies after reservation but before insert/complete, a retry with the same key receives the current "still in progress" conflict;
- the Artifact row may not exist, so artifact_key lookup returns empty.

ART-04A §8.4 currently says to retry POST with the same artifact_key but does not explicitly define what happens to the stale Idempotency-Key reservation. Reusing the same stale key can remain wedged until expiry.

**Required correction:**
- Make stable artifact_key the correctness boundary for logical Artifact creation, just as caller_request_id is for GenerationJob.
- The create implementation must resolve/check the stable artifact_key before allowing a stale idempotency reservation to block progress; concurrent same-key creates must converge on one Artifact.
- The recovery algorithm must explicitly say that after lookup returns empty, AI-PM may retry with a fresh/omitted short-lived Idempotency-Key while retaining the exact same artifact_key and immutable identity fields, OR the server must safely supersede stale reservations. Do not mint a new artifact_key.
- Add crash-injection tests for reserve → crash before Artifact insert, insert → crash before idempotency completion, and concurrent same artifact_key requests.

### AR04C-04 — P2: Reference identity is not version provenance

ART-04B correctly states that ArtifactReference is Artifact-level only. Preserve that statement. The exact association between viz.artifact.id and immutable version_id must remain in AI-PM's version-binding table; neither a project-level Reference nor an Artifact-level Reference may substitute for it.

### AR04C-05 — P2: Enforce one-to-one Job ownership in AI-PM mapping

ART-04B's viz.artifact_service_version_binding stores nullable generation_job_id, but its listed constraints do not make a Job unique across business-version rows. The intended model is one GenerationJob result per AI-PM Visualization business-version record.

Add a partial unique constraint/index on non-null generation_job_id (or document a deliberate many-to-one model with a different result mapping). This prevents accidental binding of one Job result to multiple business records.

### AR04C-06 — P2: Operation ledger must not imply a Reference is mandatory

Because Reference is not currently required to identify the exact business version and its replay contract is unresolved, the saga should treat it as optional metadata, not a prerequisite for GenerationJob creation. The mandatory recovery chain is:
1. durable AI-PM operation record;
2. resolve stable generic Artifact identity;
3. create/recover GenerationJob by caller_request_id;
4. bind exact target_version_id after success.

A failed optional Reference operation must not strand an otherwise valid generation operation.

## 3. Five-step saga review

| Step | Identity/recovery mechanism | Review |
|---|---|---|
| AI-PM business record + operation ledger | One AI-PM local DB transaction | Sound target; implementation must refactor current helper transaction boundary |
| Generic Artifact create | Stable artifact_key + exact lookup | Contract proposed; stale Idempotency-Key interaction still needs closure |
| ArtifactReference create | Active tuple unique index | Not safely replayable by current API; owner_type proposal invalid |
| GenerationJob create | caller_request_id + lookup + fingerprint | Sound proposal; not implemented |
| Job result → exact Version | immutable target_version_id + Version GET | Sound proposal; target Version/READY/output commit invariant must be implemented atomically |

## 4. Required document corrections before implementation

1. ART-04B must remove owner_type=viz_artifact and must not claim Reference create is idempotent.
2. ART-04B must make Reference optional unless a valid owner type and replay/reconciliation contract are selected.
3. ART-04A §8 must explicitly close the stale Idempotency-Key reservation window for Artifact creation.
4. ART-04B must add uniqueness for non-null generation_job_id in its version-binding proposal.
5. ART-04 / ART-04A / ART-04B remain Draft until the corrected contract is reviewed and API feasibility is demonstrated.

## 5. Evidence inspected

Artifact Service main:
- migrations/0001_init.sql: artifact_key unique index; Reference owner_type check and active-reference unique index; GenerationJob target_version_id FK; 24-hour idempotency_record.
- crates/artifact-application/src/service/artifact.rs: Artifact create reserve → insert → complete; Reference create plain insert; GenerationJob create currently fails closed when generator unavailable.
- crates/artifact-infrastructure/src/postgres/mod.rs: Reference repository plain INSERT.
- crates/artifact-domain/src/reference.rs: supported OwnerType and RelationType vocabulary.
- apps/artifact-api/src/routes/artifacts.rs: Reference create always returns 201; Reference list caps at 100 without cursor; GenerationJob route remains fail-closed.
- crates/artifact-contract/src/dto.rs: current ListArtifactsQuery has no artifact_key filter; current GenerationJob request/response omit caller_request_id/target_version_id.

AI-PM:
- migrations/0020_visualization_artifact.sql
- crates/pm-application/src/visualization/artifact_service.rs
- crates/pm-application/src/visualization/seecode_compiler.rs
- crates/pm-application/src/visualization/render_job.rs
- apps/pm-api/src/viz_handlers.rs

This review is based on repository source, not a runtime deployment or production data assertion.
