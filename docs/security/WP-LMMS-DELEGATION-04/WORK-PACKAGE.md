# WP-LMMS-DELEGATION-04 — Operation Binding & Idempotency Enforcement

**Status:** DRAFT / implementation task specification  
**Repository:** `luqizheng/smark-artifact-service`  
**Parent baseline:** `8fd5173061f935513246b952195e88cea39e8515` (WP-03 / PR #14 head)  
**Branch:** `docs/wp-lmms-delegation-04-operation-binding`  
**Dependency:** WP-03 remains an unmerged stacked PR targeting `docs/wp-lmms-delegation-01`. This work must remain stacked on WP-03.  
**Security posture:** all production delegation writes remain closed by default. This task does not authorize production credentials, live AI-PM integration, deployment, migration execution against shared environments, or merging any PR.

## 1. Objective

Close the AS-side operation-binding and operation-level idempotency gaps left explicitly open by WP-03:

1. Connect request/operation binding validation to every write path (the existing V10 helper currently has no production call sites).
2. Ensure a delegation token cannot be replayed against a different operation, target, request body, tenant, project, or HTTP route.
3. Define and enforce idempotency independently for every write endpoint. A required `caller_request_id` claim is an identifier, not by itself a replay-prevention mechanism.
4. Reuse the existing AS-1 Artifact-create and AS-2 Generation-create idempotency mechanisms where they already provide the required atomic guarantees; do not regress or duplicate them.
5. Produce independent, adversarial tests proving concurrency and replay properties.

This WP is AS-only. It must not implement AI-PM token issuance, V11 revocation wiring, LMMS changes, or production integration.

## 2. Hard boundaries

- Do not modify LMMS.
- Do not implement AI-PM endpoints, signing, JWKS publication, revocation storage, or revocation APIs.
- Do not wire V11 in this WP; track it as a separate blocker.
- Do not open production writes or change the default closed verifier behavior.
- Do not add a new database table or migration merely to recreate the WP-02 binding table removed by O-5. Any proposed schema change requires a written design rationale, compatibility analysis, and explicit approval before implementation.
- Do not weaken or replace the fixed `artifact-service` audience, issuer allowlist, action checks, tenant/project isolation, or `caller_request_id` write-token requirement.
- Do not treat `caller_request_id`, `jti`, request hash, or token signature alone as proof of idempotent execution.
- Do not silently change contract v0.3 semantics or frozen request-hash v1 vectors. A required contract change must be separately documented with rationale and compatibility consequences.
- Do not merge PR #12, PR #14, or any WP-04 PR.
- Work in a dedicated worktree/branch. Never overwrite or clean unrelated user changes, stashes, or worktrees.
- No force-push. No destructive Git operation.

## 3. Mandatory discovery before coding

At the exact parent baseline:

1. Read the complete delegation contract v0.3 and WP-02/WP-03 security records.
2. Enumerate every externally reachable write route from router registration and OpenAPI; reconcile both directions.
3. Trace each route through handler, application service, domain port, persistence transaction, and side effects.
4. Inventory all existing idempotency guarantees, unique constraints, transaction boundaries, and retry behavior, including AS-1 and AS-2.
5. Locate every call site (or absence) for `assert_delegation_matches_request`, request-hash v1 computation, and `caller_request_id` propagation.
6. Produce a route-to-operation matrix before changing code.

Do not infer route coverage from tests alone. Do not assume that a handler-level check protects alternate application-service entry points or worker paths.

## 4. Required route-to-operation matrix

For every write route, record at least:

- HTTP method and canonical route template;
- OpenAPI operationId;
- handler and application-service entry point;
- required action;
- tenant/project source and enforcement point;
- target resource identity (including parent-child relationships);
- operation identity/type;
- token claim fields used for binding;
- canonical request-hash input and version;
- caller request ID source;
- persistence uniqueness/locking mechanism;
- transaction boundary covering idempotency decision and side effect;
- same-request retry result;
- same-ID/different-request result;
- concurrent duplicate result;
- failure-before-commit and retry behavior;
- evidence/test name.

Every row must have a concrete enforcement point and a test. Mark unsupported or ambiguous cases as blockers, not as assumed-safe.

## 5. Operation binding requirements

### 5.1 Enforcement placement

- Bind at the application-service boundary immediately before any state-changing side effect, or at a shared application-layer guard that every relevant entry point must pass.
- HTTP-only validation is insufficient if a service can be invoked through another adapter.
- Validation must happen before database mutation, file/object storage mutation, queue publication, or external side effect.
- All failures are fail-closed and produce stable, non-sensitive error envelopes.

### 5.2 Bound dimensions

For each write operation, verify the applicable dimensions against the authenticated delegation context:

- issuer and fixed audience;
- tenant and project;
- required action;
- HTTP method and canonical route/operation;
- operation type;
- target resource ID and relevant parent resource IDs;
- `caller_request_id`;
- request-hash algorithm/version and digest;
- any additional contract-defined operation claims.

A token authorized for one artifact, version, generation, or operation must not authorize another target merely because the action string is the same.

Do not invent new token claims. First establish the exact v0.3 claim contract and current issuance expectations. If the current claims cannot express a required binding dimension, stop and produce a contract decision record rather than weakening the check or adding an undocumented claim.

### 5.3 Request hash

- Use the frozen v1 canonicalization rules and vectors from WP-03.
- Verify method/path/body using the contract-defined rules; query handling must remain consistent with the frozen contract.
- Reject unsupported algorithm versions and malformed/missing paired fields.
- Ensure the hash is computed from the actual request/operation data, not trusted from caller-controlled headers.
- Add cross-layer tests that prove the verifier's expected digest matches the service's actual operation representation.
- Never normalize Unicode, numbers, JSON key ordering, nulls, duplicate keys, or paths differently from the frozen v1 contract.

## 6. Idempotency requirements

### 6.1 Per-operation semantics

For every write route, explicitly classify it as:

- **Idempotent by natural key** — a database constraint/transaction guarantees one logical result;
- **Idempotent by operation key** — an operation key is durably and atomically associated with the request fingerprint and result;
- **Non-retryable** — retries are explicitly rejected and documented, with no claim of idempotency.

Default classification is **not yet proven** until the implementation and tests demonstrate it.

### 6.2 Required behavior

For any endpoint classified as idempotent:

1. First valid request performs exactly one logical operation.
2. Same operation key + same request fingerprint returns the documented stable retry result without repeating side effects.
3. Same operation key + different fingerprint is rejected deterministically.
4. Concurrent identical requests result in exactly one logical operation.
5. A crash/timeout around commit has a deterministic retry outcome.
6. The idempotency decision and protected database side effect share an atomic transaction or an equally strong, documented mechanism.
7. External side effects must use an outbox or another proven atomic handoff; do not claim DB atomicity covers an external call.

### 6.3 Existing mechanisms

- Preserve AS-1 Artifact creation semantics and tests.
- Preserve AS-2 Generation creation caller tuple uniqueness and tests.
- Prove that the token's `caller_request_id` is bound to the same operation key used by persistence.
- Do not generalize AS-1/AS-2 guarantees to other operations without evidence.
- If a write operation currently has no safe retry strategy, document it as non-retryable or propose a separately reviewed design. Do not introduce an unreviewed generic idempotency framework.

## 7. Security test requirements

Tests must be adversarial and exercise the real application/service path, not only pure helper functions.

At minimum:

- correct token + exact operation succeeds;
- wrong method, route, operation type, target ID, parent ID, tenant, or project is rejected;
- same action but different target is rejected;
- request body mutation changes the binding and is rejected;
- frozen v1 vectors pass and every specified mutation vector fails;
- missing/malformed caller request ID on write is rejected;
- same caller request ID + same operation + same fingerprint retries safely;
- same caller request ID + different operation/fingerprint is rejected;
- concurrent duplicate requests produce one logical write;
- retry after simulated commit/response loss is deterministic;
- failure before commit leaves no partial idempotency record or side effect;
- failure after durable commit is recoverable by retry;
- no queue/event/object-store side effect occurs before authorization and idempotency arbitration;
- cross-project and cross-tenant attempts reveal no protected resource data;
- all write routes in router and OpenAPI are covered, with no unlisted route.

Use isolated PostgreSQL integration tests where persistence or concurrency is involved. Add mutation tests where practical to prove each binding dimension is actually enforced. Tests must fail if the V10 call is removed or bypassed.

## 8. Deliverables

1. Route-to-operation and idempotency matrix.
2. Architecture/design note documenting enforcement placement and transaction guarantees.
3. Production call-site integration for V10 across all applicable write paths, or a precise blocker report if the current contract cannot support it.
4. Per-operation idempotency implementation only where justified and approved by the contract.
5. Adversarial unit, integration, concurrency, and mutation tests.
6. Updated OpenAPI/SDK only if externally observable behavior changes.
7. Security evidence report with exact commands, environment, test counts, and results.
8. Explicit remaining-blockers list. V11 and AI-PM issuance remain open and must not be represented as solved.

## 9. Acceptance criteria

WP-04 is not complete unless all are true:

- [ ] Route inventory reconciles router and OpenAPI in both directions.
- [ ] Every write route has a reviewed operation-binding/idempotency classification.
- [ ] V10 has real production call sites on every applicable write path.
- [ ] Binding occurs before all side effects.
- [ ] All bound dimensions are verified against actual operation data.
- [ ] Frozen request-hash v1 vectors remain unchanged and pass cross-layer tests.
- [ ] Existing AS-1/AS-2 behavior is preserved.
- [ ] Every idempotent operation passes duplicate, mismatch, concurrency, and retry-after-failure tests.
- [ ] No operation is described as idempotent without a durable enforcement mechanism.
- [ ] All security tests pass on isolated PostgreSQL.
- [ ] fmt, clippy with warnings denied, workspace tests, release builds, migration immutability guard, and Docker/Compose CI pass.
- [ ] An independent reviewer verifies the implementation and tests.
- [ ] Production remains closed; V11 and AI-PM integration remain explicitly blocked.

## 10. Execution protocol

1. Start with discovery and matrix; report findings before broad implementation.
2. If a contract ambiguity is found, stop at that boundary and submit a decision record.
3. Implement in small commits on a dedicated feature branch based on this WP-04 documentation branch.
4. Keep the PR Draft and target the WP-03 branch `feat/wp-lmms-delegation-03-as-remediation`, not `main`.
5. Run local tests, push only when explicitly authorized, and report the exact HEAD.
6. Request an independent security review after CI.
7. Do not merge any PR.

## 11. Required final report

Include:

- baseline and final HEAD SHA;
- branch and worktree isolation evidence;
- changed files and commit list;
- complete write-route matrix;
- V10 call-site inventory before/after;
- per-operation idempotency classification and database guarantees;
- exact security/concurrency/mutation test evidence;
- CI run URL and per-job conclusions;
- unresolved design decisions and blockers;
- explicit statement that no production integration, credential action, migration against shared environments, or merge occurred.
