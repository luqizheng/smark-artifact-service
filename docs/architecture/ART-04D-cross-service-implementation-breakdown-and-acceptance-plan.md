# ART-04D — Cross-Service Implementation Breakdown and Acceptance Plan

**Status:** Draft / Implementation planning only / Not approved to start production generation  
**Date:** 2026-11-25  
**Depends on:** ART-04, ART-04A, ART-04C, AI-PM ART-04B  
**Scope:** Implementation decomposition, transaction boundaries, API/SDK contracts, and failure-injection acceptance. This document does not authorize enabling SeeCode generation.

## 1. Executive decision

Implement the existing architecture incrementally. Do not introduce a second job system, shared database, distributed lock, or long-running dual-write mode.

- Artifact Service owns generic Artifact identity, GenerationJob identity/state, Attempts/Leases, target Version, Output manifest, and technical READY.
- AI-PM owns Visualization business records, slot policy, VizSpec compilation, business approval, and the durable mapping/operation ledger.
- AI-PM's stable business operation ID is the durable caller identity. Artifact Service persists that identity and makes it queryable for the full resource-retention period; the 24-hour Idempotency-Key cache is only a transport replay optimization.
- The Job's target Version is allocated and bound atomically when the Job is created. A successful Job can never rely on reading mutable `artifact.current_version_id`.
- AI-PM commits its business draft and operation ledger in one local transaction before any remote call.
- No cross-service transaction and no shared database.

## 2. Verified implementation baseline (default branches, 2026-11-25)

This section records code facts, not the intended design.

### 2.1 Artifact Service

1. `migrations/0001_init.sql` already defines nullable `artifact.artifact.artifact_key` and partial unique index `uq_artifact_project_key(tenant_id, project_id, artifact_key)`. There is no durable creation-request fingerprint on the Artifact row and no key-lookup route.
2. `ArtifactService::create_artifact` reserves `Idempotency-Key` first, then inserts Artifact, then completes the idempotency record. If the process fails after reservation but before completion, the same key returns “still in progress”; if the cache reservation expires, there is no create-intent lookup by artifact_key.
3. `generation_job.target_version_id` already exists with a scoped FK to ArtifactVersion. `GenerationJobResponse` does not expose it.
4. `0003_job_coordination_fixes.sql` creates `create_generation_job_with_input`, which deduplicates only by `idempotency_key`; it has no durable caller request ID. The function does not allocate or bind `target_version_id`.
5. `complete_generation_job_attempt` checks that the already-bound target Version is READY and has a passed primary Output with the reported SHA before setting Job succeeded. The Output/Version commit is explicitly a separate ART-03D transaction boundary. This invariant must be preserved; do not weaken it to make Job completion easier.
6. The HTTP GenerationJob create handler currently ignores request headers (`_headers`) and the application `request_generation` path currently returns GeneratorUnavailable even after the generator availability check. The DB create function is not wired through this application path.
7. Server routes exist for GET and cancel Job. The SDK has `request_generation`; add typed GET/cancel methods and exact-result fields before AI-PM depends on them.
8. SeeCode remains registered as unavailable. The File Service upload/finalize P0 remains blocking; this plan does not enable a generator.

### 2.2 AI-PM

1. `viz.artifact` is a business-version row, not the generic Artifact container. It carries slot/version, VizSpec, source/spec hashes, provenance and business state.
2. `create_draft` currently opens and commits its own transaction, including audit. A future external-generation flow cannot call it and then separately create an operation ledger while claiming atomic intent persistence.
3. `viz.approval` remains a separate business approval state machine and must not be moved to Artifact Service.
4. The current Visualization Artifact Service module is local `viz.*` lifecycle/approval code. The proposed Artifact Service consumer adapter and durable operation ledger are not yet implemented.

## 3. Required identity model

| Identity | Owner | Lifetime / invariant |
|---|---|---|
| `artifact_key` | AI-PM chooses; Artifact Service enforces | Stable for one tenant/project/Visualization slot; never recycled |
| `artifact_id` | Artifact Service | Generic logical container identity |
| `caller_request_id` | AI-PM chooses; Artifact Service persists | One immutable AI-PM generation operation; durable beyond idempotency-cache expiry |
| `generation_job_id` | Artifact Service | One technical generation request; stable for retries/worker attempts |
| `target_version_id` | Artifact Service | Allocated at Job creation and immutable; exact output destination |
| `viz_artifact_id` | AI-PM | One Visualization business-version record |
| `version_binding_id` | AI-PM | Immutable link from one viz business version to one Artifact Service Version |

Required uniqueness:
- Artifact: unique `(tenant_id, project_id, artifact_key)` for non-null keys (already exists).
- Job: unique `(tenant_id, project_id, caller_request_id)` for non-null caller IDs.
- AI-PM version binding: unique non-null `generation_job_id`; also unique business-version identity and immutable `(artifact_id, version_id)` binding according to ART-04B.
- Do not use `current_version_id` as Job provenance.

The durable request fingerprint must be computed from a canonical, versioned request representation. It must cover all fields that define the operation (including artifact scope/key, generator and immutable input/options); it must exclude volatile transport fields (request ID, trace ID, idempotency key). Same durable identity + same fingerprint returns the existing resource. Same identity + different fingerprint returns a stable 409 conflict.

## 4. Artifact Service work packages

### AS-1 — Artifact stable-key convergence (P1, prerequisite)

**DB**
- Preserve the existing partial unique index.
- Add an immutable creation fingerprint associated with the stable key. Prefer a dedicated immutable creation-identity row or an immutable column; choose one after reviewing current repository patterns. Do not compare against mutable Artifact metadata.
- Ensure the key and fingerprint are committed with the Artifact in one database transaction.

**API**
- Add a scope-protected exact lookup by `artifact_key` (tenant comes only from delegation; project comes from the validated request scope).
- Make create converge on the durable key before relying on the short-lived Idempotency-Key reservation.
- Same key/fingerprint returns the existing Artifact; same key/different fingerprint returns 409; never create a second Artifact.
- Concurrent create loser must re-read and validate the committed winner after unique violation. A unique index without this convergence path is insufficient.

**Crash/concurrency tests**
- Crash after durable Artifact insert but before HTTP response.
- Crash after idempotency reservation but before Artifact insert.
- Expired/incomplete idempotency reservation followed by same stable key.
- Two concurrent same-key/same-fingerprint requests.
- Two concurrent same-key/different-fingerprint requests.
- Same key in different tenant/project scopes remains isolated.

### AS-2 — GenerationJob durable caller identity (P1, prerequisite)

**DB**
- Add nullable-then-required-for-new-AI-PM-calls `caller_request_id` and canonical `request_fingerprint` to GenerationJob (or a dedicated immutable request-identity table).
- Add unique scope index `(tenant_id, project_id, caller_request_id)`.
- Keep existing Idempotency-Key support as a shorter transport-level replay layer; it is not the durable identity.
- Update the SECURITY DEFINER create function so identity lookup and insert are one concurrency-safe operation. Resolve the unique-conflict loser by re-reading the winner and comparing fingerprint.
- Existing historical Jobs may retain NULL caller IDs; do not fabricate caller identities for them.

**API**
- Require `caller_request_id` for the AI-PM integration contract. It is generated once and persisted in AI-PM before the first remote call.
- Provide exact lookup by caller request ID, scoped by the authenticated tenant and project.
- Same ID/fingerprint returns the original Job regardless of Idempotency-Key expiry or omission. Same ID/different fingerprint returns 409.
- Return the durable caller ID in the Job response for reconciliation.

**Tests**
- Response lost after Job transaction commits; lookup recovers exact Job.
- Retry after 24-hour idempotency retention; no duplicate Job.
- Same caller ID/same fingerprint under concurrent calls; one Job only.
- Same caller ID/different fingerprint; deterministic 409.
- Crash/rollback before insert; retry creates exactly one Job.
- Cross-tenant/project lookup does not disclose existence.

### AS-3 — Allocate exact target Version atomically with Job (P1, prerequisite)

- Extend Job creation to accept `expected_current_version_id` as a concurrency precondition, lock the Artifact row, and validate the expected pointer.
- In the same database transaction, allocate one pending ArtifactVersion (using the existing parent-row version-number serialization), insert the Job with that immutable `target_version_id`, and persist caller identity/fingerprint.
- A Job must never exist without its target Version once the create transaction commits. The target Version is not inferred from current pointer.
- A failed/cancelled Job may leave its target Version in a non-READY terminal state according to the existing Version state machine; it must not be silently rebound to another Version.
- Expose `target_version_id` in `GenerationJobResponse` and OpenAPI. It is stable from creation onward.
- Enforce `succeeded ⇒ target_version_id IS NOT NULL`, target Version belongs to same Artifact/scope, target Version is READY, and a passed primary Output exists with matching SHA.
- Preserve the current ART-03D ordering: File Service upload/finalize + Output registration + Version READY happen under their established atomicity boundary; only then may Job completion transition to succeeded. Do not claim a distributed transaction with LMMS.
- Add contract tests proving Job response target Version equals the exact Version whose manifest/Output was committed, even when another Job later advances Artifact.current_version_id.

### AS-4 — Wire the API application path and SDK (P1/P2)

- Implement the application `request_generation` path using the durable create function; remove the unconditional GeneratorUnavailable fallback only when the adapter is actually registered and operational.
- Parse and validate `Idempotency-Key` in the generation route; additionally require `caller_request_id` in the body/header contract. Prefer the body field for explicit business identity; keep header transport semantics documented.
- Add typed SDK `get_generation_job`, `cancel_generation_job`, and caller-ID lookup methods; extend DTO with `caller_request_id` and `target_version_id`.
- Keep polling/backoff/reconciliation in AI-PM orchestration, not in the thin SDK.
- Add SDK tests for 200 terminal/in-progress, 403, 404, 409, 503, timeout and retryable transport errors.

## 5. AI-PM work packages

### PM-1 — Local intent transaction (P1)

Refactor the current draft creation path so the external-generation entry point can commit the Visualization business draft and Operation Ledger row in one AI-PM transaction. Audit intent must be included in that transaction where the existing audit model requires it. No HTTP call is allowed while this transaction is open.

Operation Ledger minimum fields:
- stable `operation_id` / `caller_request_id` (same value or an explicit immutable mapping);
- tenant/project, slot key, viz_artifact_id;
- canonical request fingerprint and immutable input/spec snapshot reference/hash;
- artifact_key, artifact_id (nullable until recovered), generation_job_id (nullable until recovered), target version_id (nullable until recovered);
- operation state, attempt/reconciliation metadata, timestamps and last sanitized error.

Use uniqueness to prevent two active generation operations for the same business-version intent. Do not reuse a caller ID after uncertain outcomes.

### PM-2 — Restart-safe saga orchestration (P1)

Persist intent first, then perform remote calls outside local DB transactions:

1. Resolve/create Artifact by `artifact_key`.
2. Persist `artifact_id` to the operation ledger.
3. Create GenerationJob using stable `caller_request_id`; persist `generation_job_id` and `target_version_id`.
4. Poll/query Job; on service restart, reconcile by caller ID before any create retry.
5. On succeeded, verify exact Job target Version and its READY Version/Output manifest; create immutable AI-PM version binding.
6. Apply Visualization business validation and approval flow in AI-PM. Technical READY does not mean business approved/published.

Reference is not part of the mandatory saga. Do not add a Reference call solely to link a viz row.

### PM-3 — Cancellation and failure semantics

- AI-PM cancellation means request cancellation and continue polling; `cancel_requested=true` or an HTTP 200 response does not mean the Worker has stopped.
- Do not mark the AI-PM operation cancelled until Artifact Service reports terminal `cancelled`.
- If Job completes first, retain the successful technical result; AI-PM business policy decides whether to use, reject, or supersede it.
- Internal Worker retries remain one GenerationJob. A new business attempt after terminal failure requires an explicit new operation identity.
- Reconciliation must never blindly create a new Job when the outcome of the previous create call is uncertain.

## 6. Required cross-service failure-injection matrix

| Failure point | Expected recovery |
|---|---|
| AI-PM commits draft + ledger, crashes before remote call | Resume from ledger; same operation ID |
| Artifact create commits, response lost | Resolve by artifact_key + fingerprint; no duplicate Artifact |
| AI-PM persists artifact_id, crashes before Job create | Resume Job create with same caller_request_id |
| Job create commits, response lost | Lookup by caller_request_id; recover exact Job + target Version |
| Idempotency cache expires | Durable caller identity still resolves Job |
| Worker crashes after claim | Existing lease/Attempt recovery; same Job, new Attempt |
| Cancel races with complete | Artifact Service transaction winner is authoritative; AI-PM reconciles terminal state |
| Output upload succeeds but finalize response lost | Follow LMMS File Service finalize/reconciliation contract; never fabricate READY |
| Version READY but Job response/poll lost | GET Job returns immutable target_version_id; AI-PM binds that exact Version |
| AI-PM crashes after Job succeeded but before business binding | Re-read Job by ID/caller ID; verify target Version; create binding idempotently |
| Business approval rejected | Keep technical Version; record business rejection in AI-PM; do not mutate technical READY |

## 7. Implementation sequence and gates

1. **Contract freeze:** approve ART-04A/04B/04D field names, canonical fingerprint definition, lookup semantics and target-Version allocation boundary.
2. **Artifact DB/API:** AS-1, AS-2, AS-3 with migrations and SQL concurrency tests. No Worker enablement.
3. **Artifact DTO/OpenAPI/SDK:** AS-4; run OpenAPI parity and SDK contract tests.
4. **AI-PM DB/application:** PM-1 with transaction tests; no remote generation enabled.
5. **AI-PM adapter/saga:** PM-2/PM-3 with a fake Artifact Service and deterministic fault injection.
6. **Cross-service contract suite:** run against real PostgreSQL + fake LMMS storage boundary; test restart/replay/cancel races.
7. **Separate P0 closure:** LMMS delegation token and File Service upload/finalize protocol, ART-03B runtime lifecycle/security feasibility, SVG safety, isolated Worker deployment.
8. Only after all gates pass may SeeCode be registered as available and production generation be enabled.

## 8. Explicit non-goals

- No shared AI-PM/Artifact Service database or cross-service SQL writes.
- No RabbitMQ/webhook requirement for V1; polling + durable reconciliation is sufficient.
- No long-lived dual-write or automatic historical `viz.*` backfill.
- No AI-PM ownership of Worker processes or direct Job status writes.
- No treating Artifact READY as AI-PM business approval.
- No production SeeCode generation while LMMS/File Service and Worker runtime P0 gates remain open.

## 9. Current verdict

**Implementation planning is sufficiently decomposed to estimate and assign work, but implementation approval remains blocked.** The first coding slice should be Artifact Service AS-1/AS-2/AS-3 database and API contracts, not Worker development. Each slice must land with migrations, SQL concurrency tests, API/OpenAPI contract tests and crash-injection evidence.

No application code, migrations, SDK, Worker, deployment configuration or runtime data is changed by this document.
