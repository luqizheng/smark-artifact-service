# ART-04 — Independent Architecture Review Report

> **Review date:** 2026-11-25
> **Reviewed PR:** [#7](https://github.com/luqizheng/smark-artifact-service/pull/7)
> **Reviewed head:** `0b14b80333b69fc500513bc9b273bd95566bf916`
> **Scope:** Cross-service AI-PM ↔ Artifact Service GenerationJob control contract; static repository/API/SDK review only.
> **Verdict:** **Changes Required / Not approved for implementation.**

## 1. Executive summary

The core responsibility split is sound and consistent with ART-01:

- AI-PM owns business Task/Run intent, business authorization, input preparation, waiting/cancellation decisions, and business acceptance.
- Artifact Service is the sole authority for Artifact/Version/Reference/GenerationJob technical resources and Job state transitions.
- Worker executes claimed Attempts and has no AI-PM business authority.
- LMMS remains the identity/file infrastructure.

The proposed V1 polling approach is compatible with the existing API and avoids introducing an unapproved message broker or shared database.

However, two cross-service correctness gaps must be closed before implementation: (1) an uncertain create request cannot be safely reconciled after the currently documented minimum Idempotency-Key retention window; (2) the internal Job model has `target_version_id`, but the public GenerationJob response does not expose a version reference, so AI-PM cannot reliably associate the exact generated Version with its business Run step. There are also SDK and migration mapping tasks that must be explicit implementation prerequisites.

## 2. Findings

### AR04-01 — P1: Idempotent create has no durable reconciliation after retention

**Evidence**
- OpenAPI documents Idempotency-Key retention as at least 24 hours.
- Same key + same body returns the original Job; same key + different body returns 409.
- No API endpoint was found to look up a GenerationJob by caller-supplied request/correlation ID or idempotency key.
- ART-04 initially described retrying an uncertain create with the same key without bounding that guarantee.

**Failure scenario**
1. AI-PM persists a run-step intent and sends create-generation request.
2. Artifact Service commits the Job, but the HTTP response is lost.
3. AI-PM remains unavailable or unable to reconcile longer than the guaranteed idempotency retention.
4. A later retry may no longer be guaranteed to return the original Job; a duplicate Job could be created.

**Required correction**
- V1 client must retry the same key/body only inside the server-guaranteed retention period.
- After that period, AI-PM must stop automatic replay and leave the step in reconciliation/manual-resolution state.
- Before production integration, add a durable client request identity lookup contract (preferred: immutable caller_request_id stored with Job and queryable under tenant/project scope), or explicitly increase/define retention to cover the full recovery horizon. A finite idempotency cache alone is not a durable business correlation ledger.

**Status:** Document now records this as an open gap; API design not yet changed.

### AR04-02 — P1: Job-to-Version provenance is not exposed by the public response

**Evidence**
- ART-01 internal GenerationJob model includes `target_version_id`.
- OpenAPI GenerationJob response currently requires id/artifact_id/generator_id/status/etc., but has no `target_version_id` or terminal result Version reference.
- The create request's `expected_current_version_id` is a current-pointer concurrency precondition, not a target Version identifier.
- ART-01 says successful generation creates a new ArtifactVersion/Output and current pointer switching is conditional.

**Failure scenario**
AI-PM receives Job success but cannot unambiguously identify which Version was produced if another Job or user creates/sets a newer current Version before AI-PM reads Artifact metadata. Reading `current_version_id` is not a reliable substitute for a Job result reference.

**Required correction**
- Add an optional/additive `target_version_id` (or equivalent explicit result reference) to GenerationJob response and ensure it is populated consistently from Job creation through terminal states.
- Consumer contract tests must assert that a successful Job maps to exactly one immutable Version and its Output manifest.
- Do not infer the Job's Version from the mutable Artifact current pointer.

**Status:** Document now records the API gap; OpenAPI/DTO/SDK changes are not in this documentation PR.

### AR04-03 — P1: AI-PM business artifact migration mapping must remain separate from generic Artifact identity

**Evidence**
- AI-PM's `viz.artifact` contains Visualization-specific semantics: VizSpec, slot, nature, state, source hashes/provenance and business approval relationships.
- The AI-PM integration design says generic Artifact/Version/Output management migrates to Artifact Service, while VizSpec, Relation Registry, Live Graph, slot behavior and business approval remain in AI-PM.
- Artifact Service Reference is an opaque Artifact-level relation; it is not a replacement for AI-PM's exact Version-level approval/provenance association.

**Required correction**
- Keep the AI-PM business Visualization identity and approval state in AI-PM.
- Persist a durable mapping from the AI-PM business record to generic `artifact_id` and exact `version_id`.
- Define the per-phase read/write authority, cutover, rollback and reconciliation before production data migration. Do not rename/reuse `viz.*` tables as generic Artifact tables and do not introduce indefinite dual writes.
- The exact mapping schema and migration belong in a separate AI-PM migration design after live schema verification.

**Status:** ART-04 now states this boundary; schema/migration remains open.

### AR04-04 — P2: SDK is incomplete for a real AI-PM consumer

**Evidence**
- Rust SDK has `request_generation`.
- `CallContext.idempotency_key` is already propagated by the shared request primitive as the `Idempotency-Key` header; this part is implemented.
- Inspected SDK surface lacks `get_generation_job` and `cancel_generation_job`.

**Required correction**
- Add typed get/cancel methods and tests for success, terminal states, cancel_requested, 404/403, 503, and retryable transport errors.
- Keep SDK methods thin; polling, retry/backoff, cancellation wait and Run resumption belong to AI-PM orchestration.
- Pin the OpenAPI/API version used by the SDK and add consumer contract tests.

**Status:** Corrected in ART-04: idempotency header propagation is already present; get/cancel remain missing.

### AR04-05 — P2: Creation and Reference sequencing needs explicit consumer implementation

**Evidence**
- Artifact Service API has separate Artifact create, GenerationJob create, and Reference create operations.
- AI-PM must validate business-object existence/authorization before creating a Reference.
- The inspected AI-PM repository has active Visualization-specific artifact handlers and storage, but no confirmed live adapter calling the independent Artifact Service API/SDK was found in the static search.

**Required correction**
- AI-PM orchestration must persist a local operation record before external calls, then create/select generic Artifact, establish ArtifactReference(s), request GenerationJob, and persist returned identifiers with idempotent/reconcilable steps.
- Failure between these calls must be compensated or resumed; do not assume a distributed transaction across the services.
- Existing `viz.artifact` must not be mistaken for the generic Artifact Service resource.

**Status:** Partially addressed by ART-04; exact orchestration saga/compensation design remains implementation prerequisite.

## 3. Verified existing behavior

Static review of the current Artifact Service repository confirms:

- OpenAPI defines `POST /v1/artifacts/{artifact_id}/generation-jobs`, `GET /v1/generation-jobs/{job_id}`, and `POST /v1/generation-jobs/{job_id}/cancel`.
- Running cancel returns `status=running` with `cancel_requested=true`; this is not terminal cancellation.
- Queued/retry_wait cancellation can become terminal immediately; terminal Job state is not rewritten by later cancel.
- GenerationJob create remains fail-closed with `503 GENERATOR_UNAVAILABLE` while the SeeCode Adapter is unavailable.
- Worker coordination has atomic claim, Attempt/lease fencing, DB transaction arbitration for cancel/complete, and recovery foundations.
- Production enablement remains blocked by LMMS delegation and File Service upload/finalize P0s, plus Worker/runtime isolation and generator implementation gates.
- SDK's shared request primitive propagates Idempotency-Key already; no need to duplicate this behavior in the new methods.

## 4. Review of the optimistic concurrency proposal

The ART-04 conclusion is accepted with a strict scope:

- AI-PM uses its own optimistic concurrency/transactions for its own Task/Run step and external-resource mapping.
- Artifact ETag/If-Match protects Artifact metadata/current-pointer changes.
- GenerationJob state arbitration remains server-owned and transactional; Worker Attempt/lease fencing and runtime death confirmation remain necessary.
- A cross-service optimistic lock cannot replace idempotency, durable correlation, cancellation confirmation or immutable Version provenance.

## 5. Final verdict and gates

**Changes Required / Not approved for implementation.**

Before AI-PM production integration:
1. Resolve AR04-01 durable create reconciliation.
2. Resolve AR04-02 exact Job-to-Version result reference.
3. Freeze AI-PM business-record ↔ generic Artifact/Version mapping and migration cutover.
4. Implement SDK get/cancel and consumer contract tests.
5. Design resumable, idempotent multi-call orchestration/compensation for Artifact create → Reference create → GenerationJob create.
6. Keep all existing LMMS delegation, File Service finalize, SeeCode Adapter and ART-03B-02/03 runtime gates intact.

This review does not approve SDK/API/DB/Worker implementation and does not authorize production generation.
