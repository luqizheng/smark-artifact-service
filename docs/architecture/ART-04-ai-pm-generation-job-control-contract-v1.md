# ART-04 — AI-PM GenerationJob Control Contract

> **Status:** Cross-service design proposal / Draft; not an implementation approval.
> **Date:** 2026-11-25
> **Authority:** Artifact Service owns the GenerationJob API and technical execution state. AI-PM owns the business task/run and its intent.
> **Related:** ART-01 V1 API, ART-01 V1 API Errata 001, ART-03A, ART-03B-01 implementation report.

## 1. Purpose

Define how an AI-PM business Task/Run requests, observes, waits for, and cancels an Artifact Service GenerationJob.

This document does not create a second Job system and does not change the existing Artifact / Version / GenerationJob resource model. It clarifies cross-service ownership and consumer behavior.

## 2. Core decision: business control vs execution authority

**AI-PM is the business controller; Artifact Service is the technical execution authority.**

| Concern | Authority |
|---|---|
| Whether a business task needs an artifact generated | AI-PM |
| What content/structured input should be generated | AI-PM Agent or human author, validated by AI-PM |
| Whether the business task should continue, wait, or request cancellation | AI-PM |
| Whether a GenerationJob is queued/running/cancel-requested/succeeded/failed/cancelled/timed-out | Artifact Service |
| Worker claim, Attempt, lease, heartbeat, retry, timeout and recovery | Artifact Service |
| Process termination and death confirmation | Artifact Service Worker/runtime boundary |
| Artifact / Version / Output metadata and technical READY state | Artifact Service |
| Whether the result satisfies a Requirement/Design/ADR or is accepted/published | AI-PM and its human approval workflow |

AI-PM does not write Artifact Service tables, directly control Worker processes, or set GenerationJob status. Artifact Service does not decide whether an AI-PM Task is complete or whether a generated result is approved for business use.

A GenerationJob reaching `succeeded` means technical generation and output commit succeeded. It does not mean business acceptance.

## 3. Resource relationship

One AI-PM Task/Run step may create one or more GenerationJobs. Each GenerationJob belongs to an Artifact and targets a specific pending Version according to the Artifact Service contract.

AI-PM must persist the external correlation in its own database, at minimum:

- AI-PM task/run identifier and stable step identifier;
- Artifact Service `artifact_id` and `generation_job_id`; the internal Job model has `target_version_id`, but the current public GenerationJob response does not expose it. AI-PM therefore cannot yet persist an authoritative Job-to-Version mapping from the V1 response;
- stable `Idempotency-Key` used for creation;
- request hash or immutable request reference;
- last observed GenerationJob status and observation timestamp (a local read model, not an authority);
- correlation/trace identifier.

The Artifact Service stores only generic scope and external reference identifiers. It must not interpret AI-PM Task, Requirement, Design, or ADR semantics.


## 3.1 Artifact identity, business reference, and Version mapping

The generic Artifact is created/selected through Artifact Service; AI-PM then creates a generic ArtifactReference for the relevant business object(s), after checking that those objects exist and the caller is authorized. Typical links include Design, Requirement, ADR, Task/Run, or a Visualization business record. Artifact Service stores opaque owner_type/owner_id/relation_type values only.

Do not equate these identities:
- AI-PM business Visualization record (`viz.artifact.id`): carries VizSpec, slot/nature, business state and approval semantics;
- Artifact Service `artifact_id`: generic logical resource identity;
- Artifact Service `version_id`: immutable generated/file version identity;
- GenerationJob `job_id`: one technical execution request.

For Visualization integration, AI-PM retains the business record and its approval workflow, and stores a durable mapping to the generic Artifact and the specific Version used by the business record. The generic ArtifactReference alone is an Artifact-level association; it must not be misrepresented as a Version-level approval or provenance record. The exact mapping table/columns and migration cutover belong to a separate AI-PM migration design and must be verified against the live schema before implementation.

No indefinite dual-write between legacy `viz.*` storage and Artifact Service is allowed. Migration/cutover must define the read/write authority per phase, rollback window, and reconciliation procedure before any production data movement.

## 4. V1 interaction sequence

### 4.1 Request generation

1. AI-PM validates the business object, project access, generation intent, and input.
2. AI-PM obtains a short-lived project/action-scoped delegation for the Artifact API.
3. AI-PM creates or selects the logical Artifact, then calls:

   `POST /v1/artifacts/{artifact_id}/generation-jobs`

4. AI-PM supplies a stable `Idempotency-Key` derived from a durable AI-PM run-step identity. It must reuse the same key and exact request body when retrying an uncertain request.
5. Artifact Service validates the request and generator availability, creates the Job and immutable input snapshot, and returns `202` with the GenerationJob identity when the creation path is enabled. The request may carry `expected_current_version_id` as a current-pointer concurrency precondition; it is not a Job revision or a target Version ID. ART-01's internal Job model includes `target_version_id`, but the public GenerationJob response currently omits it. This is a consumer-contract gap: before AI-PM needs exact version-level provenance, the API should expose `target_version_id` (or an equivalent terminal result reference) as an additive response field. AI-PM must not guess the output Version by reading the Artifact's current pointer, because concurrent generation or later pointer changes can make that association ambiguous.
6. AI-PM persists the returned Job identity and moves its own Run step into a waiting-for-external-execution state.

If the HTTP response is lost after the server accepted the request, AI-PM retries with the same Idempotency-Key and byte-equivalent semantic request body while the server's idempotency record is guaranteed to be retained. The OpenAPI currently promises retention of at least 24 hours, not an unlimited lifetime, and provides no lookup-by-idempotency-key endpoint. Therefore, after the documented minimum retention window has elapsed, AI-PM must not blindly replay an uncertain create: it must hold the Run step for reconciliation/manual resolution until a durable request-correlation lookup contract exists. This is an open integration gap, not a solved recovery guarantee.

**Current implementation constraint:** the GenerationJob creation API currently returns `503 GENERATOR_UNAVAILABLE` because the SeeCode Adapter is not implemented and no production generation path is enabled. This proposal does not authorize bypassing that gate.

### 4.2 Wait for completion

V1 uses polling; no webhook or message-broker notification is assumed.

- AI-PM queries `GET /v1/generation-jobs/{job_id}`.
- Polling runs in the AI-PM orchestration/background worker, not in a long-held HTTP request.
- AI-PM persists the latest observed state and resumes the waiting Run step when a terminal state is observed.
- Poll interval/backoff is client policy and must be bounded; transient network errors do not imply Job failure.
- A local AI-PM timeout means “stop waiting / escalate / request cancellation according to business policy”; it must not fabricate Artifact Job state.
- Future event notification may reduce polling, but polling/reconciliation remains the recovery path unless a separately reviewed event contract is introduced.

### 4.3 Request cancellation

AI-PM sends:

`POST /v1/generation-jobs/{job_id}/cancel`

The existing API semantics remain authoritative:

| Job state at cancellation | Artifact Service behavior | AI-PM behavior |
|---|---|---|
| queued / retry_wait | Immediately transitions to `cancelled` | Observe terminal state; then close or compensate its business Run |
| running | Records cancellation intent; response remains `running` with `cancel_requested=true` | Mark its own Run as cancelling/waiting-for-stop; continue observing |
| terminal | Returns the existing terminal state unchanged | Reconcile with the actual terminal result |

**A successful Cancel API response is not proof that execution has stopped.** For a running Job, AI-PM must wait until Artifact Service reports a terminal state.

Cancellation races with completion. The database transaction/state-machine winner is authoritative:
- if cancellation wins, the Job becomes `cancelled` after Worker/runtime termination is confirmed;
- if successful completion wins first, the Job remains `succeeded`; cancellation does not roll back a committed Version or delete its Output;
- if the result is uncertain or the Worker cannot be confirmed stopped, AI-PM must keep the business Run in a waiting/reconciliation state rather than claiming cancellation completed.

AI-PM may decide to ignore or detach a successfully generated result after its business task was cancelled. That is a business decision; it must not rewrite the Artifact Service Job history.

### 4.4 Resume business workflow

On terminal Job observation:

- `succeeded`: fetch Artifact/Version/Output metadata, validate business applicability in AI-PM, then continue the Task/Run. Business approval remains separate.
- `failed` / `timed_out`: AI-PM decides whether to retry by creating a new Job, revise input, or fail the business step. It must not mutate/reopen the terminal Job.
- `cancelled`: close the waiting step as cancelled, subject to the parent Task/Run policy.

A new generation attempt requested by AI-PM after a terminal Job is a **new GenerationJob** with a new idempotency key and a new immutable input snapshot. Artifact Service internal retry attempts remain within the same Job and are not new business requests.

## 5. State ownership and optimistic concurrency

Do not use one shared status field or shared database across the two services.

- AI-PM uses its own transaction/concurrency mechanism to protect its Task/Run step and its mapping to the external Job.
- Artifact Service remains the sole writer and authority for GenerationJob status.
- Artifact Service serializes conflicting Job transitions in its own database transaction/state-machine functions. Existing cancel-vs-complete arbitration and Attempt/lease fencing remain required.
- Artifact metadata ETag / If-Match protects Artifact metadata updates; it is not a substitute for Job transition arbitration.
- AI-PM does not need to send an expected Job revision for ordinary cancel/poll in V1. Cancel is a command against the current Job state, and the server returns the authoritative resulting/current state.
- Worker Attempt ID + lease token fencing prevents an expired or stale Worker from committing after a newer Attempt or cancellation has won.

**Conclusion:** optimistic concurrency is sufficient for AI-PM's own business Run record and Artifact metadata CAS, but it does not replace Artifact Service's transactional state arbitration, lease, Attempt fencing, or runtime death confirmation.

## 6. SDK requirements

The Rust `artifact-client` currently exposes `request_generation`, but the inspected client surface does not yet expose the corresponding Job read/cancel operations. Before AI-PM production integration, add and contract-test:

- `get_generation_job(job_id, context)`;
- `cancel_generation_job(job_id, context)`;
- creation idempotency propagation through `CallContext.idempotency_key` (the inspected SDK request primitive already emits the `Idempotency-Key` header);
- stable typed GenerationJob response and error handling for `cancel_requested`, terminal status, 404, 409, 503, and retryable transport failures.

SDK methods must remain thin API clients. They must not contain AI-PM business state transitions or poll forever internally. The AI-PM orchestration layer owns wait/retry policy.

## 7. Authorization and trust boundary

- AI-PM performs business authorization and issues a short-lived, scoped delegation.
- Artifact Service validates delegation audience, tenant/project scope, action and expiry.
- Worker does not receive the user's delegation and does not receive AI-PM business credentials.
- Worker uses its own minimal DB function role and its separately scoped File Service service identity.
- Neither service accesses the other's database.
- A project ID or external business object ID in a request is a locator, not proof of authorization.

The existing LMMS delegation and File Service upload-finalize P0 gates remain unchanged.

## 8. Non-goals

This contract does not:
- move AI-PM Task/Run orchestration into Artifact Service;
- make AI-PM directly claim or own a Worker;
- introduce RabbitMQ, Redis, shared SQL polling, or cross-service database writes;
- require webhook delivery in V1;
- move Visualization business approval, VizSpec, Relation Registry, Live Graph, Requirement/Design/ADR semantics into Artifact Service;
- authorize SeeCode Adapter implementation or production generation enablement;
- resolve the ART-03B-02/03 runtime lifecycle review blockers.

## 9. Acceptance checklist

- [ ] AI-PM can create a Job with a durable idempotency key and recover an uncertain create response without duplicate Jobs.
- [ ] AI-PM persists the mapping from Task/Run step to Artifact/Job/Version IDs.
- [ ] AI-PM waits asynchronously and can resume after process restart using the persisted Job ID.
- [ ] AI-PM cancellation distinguishes cancel-requested from cancelled.
- [ ] Cancel-vs-complete race tests prove exactly one terminal outcome and AI-PM reconciliation handles either winner.
- [ ] AI-PM never writes Artifact Service Job status or database rows.
- [ ] SDK has typed get/cancel methods and tests for all documented outcomes.
- [ ] Consumer contract tests run against the pinned Artifact Service API version.
- [ ] No webhook/broker/shared-database dependency is silently introduced.
- [ ] Existing LMMS delegation, File Service finalize, and runtime isolation gates remain enforced.

## 10. Current implementation snapshot (read-only repository audit, 2026-11-25)

Observed in `smark-artifact-service` main:
- GenerationJob query and cancel API routes are implemented and covered by tests.
- Running cancel returns `status=running` and `cancel_requested=true`; Worker confirmation is required for terminal cancellation.
- Job/Attempt, lease, retry, cancellation coordination and recovery foundation are implemented.
- The Worker currently has no SeeCode Adapter capability and therefore does not claim generation Jobs.
- GenerationJob creation still fails closed with `503 GENERATOR_UNAVAILABLE`.
- Version upload/commit remains blocked by the File Service upload-intent/checksum/finalize protocol.
- Production delegation remains blocked by LMMS resource-token authentication/claims P0.
- The inspected Rust SDK includes `request_generation` but does not yet expose `get_generation_job` or `cancel_generation_job`.

Observed in `ai-pm` main:
- Existing Visualization business semantics, VizSpec, Relation Registry, Live Graph, slot behavior and approval remain in AI-PM.
- Existing `viz.artifact` is a Visualization-specific business record, not the generic Artifact Service resource. The integration design explicitly maps generic identity/version/output to Artifact Service while keeping Visualization semantics and business approval in AI-PM.
- Existing Visualization handlers and persistence remain in the repository. The inspected code search did not find a live consumer adapter calling this independent Artifact Service API/SDK; treat this as a static search result, not proof about external/unindexed consumers.
- Migration must be staged: do not rename/reuse `viz.*` tables as generic Artifact Service tables and do not introduce long-lived dual writes. Business approval must continue to reference the AI-PM business artifact identity and be linked to the generic Artifact/Version through stable mapping/reference data.

This snapshot must be rechecked before implementation; it is not a runtime deployment claim.
