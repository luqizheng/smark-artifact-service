# ART-04E — Durable Artifact Creation Identity and Crash-Recovery Contract

**Status:** Draft / Architecture specification / Not approved for implementation  
**Date:** 2026-11-25  
**Parent:** ART-04 / ART-04D (AS-1)  
**Scope:** Stable identity for keyed Artifact creation, request fingerprinting, concurrency convergence, crash recovery, API behavior, and acceptance tests.  
**Non-goals:** GenerationJob caller identity (AS-2), Job→Version atomic binding (AS-3), Worker/runtime changes, AI-PM operation ledger implementation, generator enablement, and production rollout.

## 1. Decision summary

AS-1 closes the gap between a unique key and a recoverable create intent.

The existing schema already has:

- nullable `artifact.artifact.artifact_key`;
- partial unique index `uq_artifact_project_key(tenant_id, project_id, artifact_key)`;
- scoped Artifact identity constraint `UNIQUE(id, tenant_id, project_id)`.

The unique index prevents two Artifact rows from owning the same non-null key in one tenant/project. It does **not** prove that a retry carries the same semantic create request, and it cannot independently recover a request whose idempotency reservation outlived the process that was creating the Artifact.

**Decision: add a dedicated `artifact.artifact_creation_identity` table.** Do not make mutable Artifact metadata the durable request fingerprint, and do not rely on the Idempotency-Key cache as the creation identity.

The creation identity row and Artifact row MUST be inserted in one PostgreSQL transaction. The identity is immutable and scoped by `(tenant_id, project_id, artifact_key)`. The durable identity is authoritative; Idempotency-Key is only a transport retry optimization.

## 2. Identity and scope

### 2.1 Key semantics

For AI-PM-managed stable Artifacts, `artifact_key` is:

- supplied by AI-PM and generated before the first remote call;
- stable for one tenant/project/business slot;
- non-empty, bounded, normalized according to the API contract, and never silently rewritten;
- never reused for a different logical Artifact, including after archive or soft-delete;
- immutable after creation.

The database uniqueness scope remains exactly `(tenant_id, project_id, artifact_key)`. A key is not globally unique and must never be resolved outside the authenticated tenant/project scope.

The existing partial unique index remains in place. It is a valuable second-line invariant and must not be weakened or replaced with an application-only check.

### 2.2 Distinct identifiers

| Identifier | Owner | Purpose | Lifetime |
|---|---|---|---|
| `artifact_key` | AI-PM | Stable logical Artifact identity | Business lifetime; never reused |
| `artifact_id` | Artifact Service | Generic Artifact row identity | Artifact retention lifetime |
| `creation_fingerprint` | Artifact Service computes from caller intent | Detect same-key semantic mismatch | Same as creation identity |
| `Idempotency-Key` | Caller | Short-window HTTP retry/replay | Existing bounded retention policy |

An `Idempotency-Key` MUST NOT be used as a substitute for `artifact_key`. A caller may rotate a transport key after a timeout and still recover the same Artifact through `artifact_key`.

## 3. Durable schema

Add a new migration; do not rewrite migrations 0001–0003.

Proposed table (final SQL names/types must follow the repository's migration conventions):

```sql
CREATE TABLE artifact.artifact_creation_identity (
    tenant_id              uuid        NOT NULL,
    project_id             uuid        NOT NULL,
    artifact_key           text        NOT NULL,
    artifact_id             uuid        NOT NULL,
    fingerprint_version    smallint    NOT NULL,
    request_fingerprint    char(64)    NOT NULL,
    created_at              timestamptz NOT NULL DEFAULT now(),
    created_by              text        NOT NULL,

    CONSTRAINT artifact_creation_identity_pk
        PRIMARY KEY (tenant_id, project_id, artifact_key),

    CONSTRAINT artifact_creation_identity_artifact_unique
        UNIQUE (tenant_id, project_id, artifact_id),

    CONSTRAINT artifact_creation_identity_artifact_fk
        FOREIGN KEY (artifact_id, tenant_id, project_id)
        REFERENCES artifact.artifact (id, tenant_id, project_id),

    CONSTRAINT artifact_creation_identity_key_check
        CHECK (length(btrim(artifact_key)) BETWEEN 1 AND 200),

    CONSTRAINT artifact_creation_identity_fingerprint_version_check
        CHECK (fingerprint_version > 0),

    CONSTRAINT artifact_creation_identity_fingerprint_check
        CHECK (request_fingerprint ~ '^[0-9a-f]{64}$')
);
```

Additional implementation requirements:

1. Keep `uq_artifact_project_key`; the table's primary key is a second, explicit durable identity constraint.
2. Add a reverse lookup index only if query plans demonstrate it is needed; the unique `(tenant_id, project_id, artifact_id)` already supports scoped lookup by Artifact.
3. The identity table is append-only. Runtime roles receive only the minimum SELECT/INSERT or controlled-function EXECUTE permissions required by the chosen access pattern; no UPDATE/DELETE.
4. Add a trigger or use a controlled database function so the identity row cannot be rebound to another Artifact or have its key/fingerprint rewritten.
5. Artifact deletion/archive does not release the key. Identity rows are retained for at least as long as the Artifact identity can be referenced or reconciled.
6. The identity row's composite FK enforces that the referenced Artifact belongs to the same tenant/project.
7. Do not store the original input/spec body in this table. Store only the canonical fingerprint and the minimal identity metadata.
8. Add a **deferred constraint trigger** for newly inserted or key-mutated Artifact rows: at transaction commit, any Artifact row with non-null `artifact_key` that was inserted/changed in that transaction must have its matching creation-identity row. The trigger must allow pre-existing legacy rows to remain identity-less until touched, and must not permit a new keyed Artifact to commit without identity. The identity insert may occur after the Artifact insert in the same transaction because the check is deferred.
9. Enforce `artifact_key` immutability after creation in the same migration. A key must not be changed to evade the permanent identity mapping; a separate future migration/reconciliation process is required for any exceptional correction.

### 3.1 Why a separate table

A pair of columns on `artifact.artifact` would be smaller, but it couples durable request provenance to a mutable resource row and creates a legacy migration ambiguity: existing keyed Artifacts have no trustworthy original request snapshot from which to reconstruct a fingerprint.

A separate append-only identity table provides a clear boundary between mutable Artifact metadata and immutable create-intent provenance. It also permits explicit treatment of legacy keyed rows that have no identity record, without inventing a fingerprint from potentially edited metadata.

## 4. Canonical request fingerprint

### 4.1 Versioning

Use an explicit `fingerprint_version` (initially `1`). The canonicalization algorithm is part of the public integration contract and must have golden test vectors.

The fingerprint is:

`lowercase_hex(SHA-256(UTF-8(canonical_json_v1)))`

The canonical document includes:

- a fixed domain separator, e.g. `"artifact-create:v1"`;
- authenticated `tenant_id` and `project_id`;
- `artifact_key`;
- immutable creation semantics: `artifact_type`, `name`, `description`, and `category` (with explicit null handling);
- any other field that the create endpoint treats as part of the initial resource definition and that must not silently differ on retry.

It MUST NOT include:

- Idempotency-Key;
- request timestamp, trace ID, HTTP headers unrelated to semantics;
- generated `artifact_id`;
- mutable state such as `lifecycle_state`, `current_version_id`, `row_revision`, or updated timestamps;
- server-generated defaults unless those defaults are first materialized into the canonical request in a deterministic, versioned way;
- raw file bytes or secrets. If future create semantics include a file/source reference, include its stable content digest and immutable reference identity, not a temporary signed URL.

### 4.2 Canonicalization rules

Version 1 MUST specify and test all of the following:

- UTF-8 encoding;
- deterministic object-key ordering;
- no insignificant whitespace;
- explicit distinction between absent and null only where the API semantics distinguish them; otherwise normalize them to one representation;
- deterministic string escaping;
- no floating-point values in the fingerprinted create fields;
- exact Unicode normalization policy (recommend NFC, applied consistently before validation and hashing);
- exact trimming policy (recommend reject leading/trailing whitespace for `artifact_key`; do not silently trim names/descriptions unless the API already defines that behavior);
- deterministic defaults for omitted optional fields.

The API implementation must calculate the fingerprint from the validated semantic DTO, not from raw JSON bytes. Equivalent JSON property ordering/whitespace must produce the same fingerprint.

If canonicalization semantics change, introduce fingerprint version 2. Never reinterpret or rewrite existing version-1 fingerprints in place. On lookup, canonicalize the incoming semantic DTO using the **stored identity's fingerprint version**, then compare hashes. This lets a current server compare a retry against an older identity without changing the old record. If an old version's canonicalizer can no longer be evaluated, fail closed with an explicit recoverability error rather than treating it as a new create.

### 4.3 Actor identity

The authenticated tenant/project scope is authoritative. `created_by` is audit metadata, not an independent idempotency dimension unless the product contract explicitly says that changing the actor changes the resource intent. A retry through a rotated service credential must not mint a second Artifact. Authorization must still be revalidated on every request.

## 5. Create transaction and concurrency algorithm

### 5.1 Required atomic boundary

For a new keyed Artifact, one database transaction MUST:

1. validate that the tenant/project scope is authorized and active;
2. insert the Artifact row with its stable `artifact_key`;
3. insert the matching immutable `artifact_creation_identity` row with fingerprint version/hash;
4. write the relevant audit event, if the existing audit policy requires it for Artifact creation;
5. commit.

The Artifact and identity row either both exist or neither exists. No externally visible intermediate state is permitted.

For keyed requests that carry an `Idempotency-Key`, the idempotency reservation, Artifact, creation identity, and completed response snapshot/resource_id MUST also be committed in this same transaction. This prevents a second race in which two different artifact keys reuse the same Idempotency-Key concurrently. A keyed create must not use the current two-transaction "reserve first, create later, complete last" flow.

The old reservation-first flow is the known stale-reservation crash window. The new keyed path must replace it with one transaction, not merely reorder two separate transactions.

### 5.2 Request algorithm

For a keyed create request:

1. Authenticate and authorize the tenant/project. Validate and canonicalize the request; compute the versioned Artifact creation fingerprint and, when supplied, the existing endpoint-specific Idempotency-Key request hash.
2. Start one transaction. For a supplied Idempotency-Key, atomically insert its reservation row (using the existing unique scope `tenant_id/project_id/endpoint/idempotency_key`) or lock/read the existing row:
   - a different request hash returns the existing idempotency conflict;
   - a completed matching row replays its response;
   - an incomplete legacy row may proceed only under the cutover/drain rule in §6.1.
3. Resolve `(tenant_id, project_id, artifact_key)` in `artifact_creation_identity` inside the same transaction:
   - if an identity exists, canonicalize the incoming semantic DTO using that stored identity's fingerprint version; a matching hash resolves the existing Artifact, while a mismatch returns HTTP 409 `ARTIFACT_KEY_CONFLICT`;
   - if no identity exists, insert the Artifact and its identity row in this transaction.
4. If a key collision occurs, roll back the entire transaction (including any newly inserted idempotency reservation), start a fresh transaction, and resolve both the Idempotency-Key record and durable Artifact identity again. Never continue in an aborted PostgreSQL transaction:
   - identity exists and the incoming request matches its stored-version fingerprint: resolve the winner;
   - identity exists and does not match: return 409;
   - Artifact with that key exists but no identity row exists: return 409 `ARTIFACT_KEY_LEGACY_UNVERIFIABLE`; never infer equivalence from mutable metadata;
   - neither exists: bounded retry, then a retryable server error.
5. Before commit, write the completed idempotency `response_snapshot` and `resource_id` (when an Idempotency-Key was supplied). Commit the idempotency record, Artifact, and identity together.
6. Return the committed response. A response lost after commit is recovered from the completed Idempotency-Key row or, after its expiry, from `artifact_key` + creation fingerprint.

The read-before-insert is an optimization, not the concurrency guarantee. Unique constraints plus transaction rollback and fresh-transaction re-read are the guarantee.

### 5.3 Isolation and retry

READ COMMITTED is sufficient if each conflict path rolls back and re-reads in a fresh transaction. Do not attempt to continue using a transaction that has already received a PostgreSQL unique-violation error.

A serialization failure/deadlock may be retried with a strict bounded retry policy. Do not convert arbitrary database errors into “key already exists”.

### 5.4 Idempotency-Key interaction

- For keyed creates, the Idempotency-Key row and durable Artifact identity are coordinated in one transaction. Do not leave a newly reserved Idempotency-Key committed without the Artifact/identity operation also committing or rolling back.
- Same `artifact_key` + same fingerprint returns the same Artifact even if the Idempotency-Key differs or its cache entry expired.
- Same `artifact_key` + different fingerprint returns 409 even if a new Idempotency-Key is supplied.
- The Idempotency-Key remains a transport replay optimization; it is not the durable authority for whether the Artifact exists.
- Requests without `artifact_key` retain the existing Idempotency-Key behavior in AS-1; they are outside this durable-key recovery contract.
- Do not silently repurpose an existing Idempotency-Key with a different request hash. Preserve the existing idempotency conflict semantics, including concurrent reuse of one Idempotency-Key for two different artifact keys.

## 5.5 Deferred database invariants

The migration must enforce both directions of the identity relationship:

- A deferred constraint trigger on newly inserted/key-mutated Artifact rows verifies at commit that a non-null `artifact_key` has a matching identity row for the same tenant/project/Artifact.
- A deferred constraint trigger on identity-row insertion verifies at commit that the referenced Artifact has exactly the same `artifact_key` and scope.
- Runtime permissions and append-only triggers prevent identity update/delete/rebinding.
- A BEFORE UPDATE guard rejects any change to an existing Artifact's `artifact_key`.

This allows the Artifact and identity inserts to occur in either order within one transaction while preventing either side from committing alone or being paired with a mismatched key. Existing legacy rows are not retroactively rejected merely because they predate the identity table.

## 6. Legacy rows and migration safety

Existing Artifacts with non-null `artifact_key` but no creation identity row are legacy-unverifiable. The migration MUST NOT manufacture a request fingerprint from current `name`, `description`, `artifact_type`, or `category`, because these fields may have changed since creation.

Required migration behavior:

1. Create the new identity table and constraints without rewriting Artifact rows.
2. Inventory keyed Artifacts lacking identity rows, scoped by tenant/project.
3. If the inventory is empty, record that fact in migration/rollout evidence.
4. If any rows exist, do not auto-backfill them. New creates using those keys return the explicit legacy conflict response until a separately approved reconciliation establishes provenance.
5. No cross-tenant existence information may be exposed by inventory or API responses.

The repository's current product context says there are no real customers or historical business data, but that is not a substitute for checking the actual target database at rollout. If unexpected non-test data is found, stop rollout and obtain an explicit product decision; do not run a guessed backfill.

### 6.1 Cutover gate for old reservation-first writers

The current implementation reserves Idempotency-Key in a separate operation before inserting the Artifact. An old handler may therefore still be in flight while a new handler tries to recover its stale reservation.

Before enabling the new keyed-create transaction path:

1. deploy/operate a write-quiescence gate for keyed Artifact creation;
2. stop admitting new keyed creates through the old handler and drain all in-flight old create handlers;
3. confirm no old handler can subsequently insert an Artifact without a creation-identity row;
4. reconcile pre-existing incomplete Idempotency-Key records under the explicit policy below;
5. only then enable the new handler/database invariant.

An incomplete legacy idempotency row with no matching Artifact identity is not automatically proof that no old request is still executing. It may be reused only after the old writer drain is proven. If that cannot be proven, keep the key blocked and require operator reconciliation; do not race old and new writers.

## 7. API contract

The existing Artifact create endpoint remains the entry point. For AI-PM-managed stable Artifacts, `artifact_key` is required by the integration contract. The general-purpose API may continue to permit omission if existing clients rely on it.

Recommended response behavior:

| Condition | HTTP | Stable code | Behavior |
|---|---:|---|---|
| New key + new fingerprint | 201 | — | Create and return Artifact |
| Existing key + same fingerprint | 200 | — | Return the existing Artifact; include an explicit replay/resolution indicator if the API's response conventions support it |
| Existing key + different fingerprint | 409 | `ARTIFACT_KEY_CONFLICT` | No mutation |
| Legacy Artifact key without identity | 409 | `ARTIFACT_KEY_LEGACY_UNVERIFIABLE` | No mutation; operator reconciliation required |
| Scope unauthorized/not visible | 403 or existing non-disclosing policy | Existing auth code | Do not reveal whether the key exists |
| Temporary DB conflict after bounded retries | 503 | Existing retryable code | Caller retries with the same stable key |

A dedicated lookup endpoint is optional, not required for the core create-recovery guarantee. If added, it MUST be scope-protected and return only the Artifact in the authenticated tenant/project. Avoid an unauthenticated key-existence oracle. The create endpoint itself can resolve the durable identity and return the existing Artifact, which is sufficient for AI-PM reconciliation.

OpenAPI must document that `artifact_key` is immutable, tenant/project scoped, and never reusable. Document the 200 replay behavior and 409 mismatch behavior. Keep response fields and error envelope consistent with the existing API.

## 8. Authorization, RLS, and audit

- Authenticate and authorize on every create/replay request, including requests that resolve an existing key.
- The caller must have current access to the requested tenant/project before the service reveals the Artifact representation.
- Every identity lookup includes both tenant_id and project_id derived from trusted authenticated context; never trust caller-supplied scope without the existing authorization boundary.
- RLS/controlled-function policies must cover the new identity table consistently with existing Artifact tables. Do not grant broad table access to application roles merely to simplify the feature.
- A 404/403 policy may be used to avoid cross-scope existence disclosure; it must match the service's established authorization contract.
- Audit records must not contain raw input bodies, secrets, signed URLs, or credentials. Record key/fingerprint identifiers only if the existing audit data classification permits them.

## 9. Acceptance tests

All tests below are required before AS-1 can be marked implementation-complete.

### 9.1 Canonicalization

1. Same semantic request with different JSON property order produces the same fingerprint.
2. Same semantic request with different insignificant JSON whitespace produces the same fingerprint.
3. Explicit null/omitted optional fields follow the documented normalization rule.
4. Unicode-equivalent input follows the selected NFC policy.
5. A semantic field change produces a different fingerprint.
6. Fingerprint golden vectors are stable across supported runtime versions.

### 9.2 Atomicity and crash recovery

7. Crash before Artifact insert: neither Artifact nor identity row exists; retry creates exactly one.
8. Crash after Artifact insert but before identity insert: transaction rollback leaves neither row.
9. Crash after identity insert but before commit: transaction rollback leaves neither row.
10. Commit succeeds but HTTP response is lost: retry with same key/fingerprint returns the same Artifact ID.
11. Idempotency reservation exists but process dies before Artifact transaction: retry bypasses stale reservation using durable key resolution and creates exactly one Artifact.
12. Artifact transaction commits but Idempotency-Key completion fails: retry still resolves the same Artifact.
13. Idempotency-Key cache expires: same artifact_key/fingerprint still resolves the same Artifact.
14. Same artifact_key with a new Idempotency-Key and same fingerprint returns the same Artifact.
15. Same artifact_key with a new Idempotency-Key and a different fingerprint returns 409.

### 9.3 Concurrency

16. N concurrent same-key/same-fingerprint requests produce exactly one Artifact and one identity row; all successful callers receive the same Artifact ID.
17. Concurrent same-key requests with different fingerprints produce exactly one winner; all non-matching callers receive 409.
18. A unique violation on the Artifact key index is followed by a fresh transaction and durable identity re-read; no generic 500 and no duplicate.
19. A unique violation on the identity table is handled identically.
20. No transaction attempts to continue after PostgreSQL has aborted it due to unique violation.

### 9.4 Scope and legacy safety

21. Same artifact_key in different projects is independent, subject to authorization.
22. A caller without project access cannot infer whether a key exists.
23. A legacy Artifact key with no identity row returns the explicit legacy conflict and is not fingerprinted from mutable metadata.
24. Archived/deleted Artifact keys are not silently reusable.
25. Identity rows cannot be updated, deleted, or rebound by runtime roles.
26. Cross-tenant/project FK violations are rejected by PostgreSQL.

### 9.5 Regression

27. Existing unkeyed Artifact creation retains current documented behavior.
28. Existing Idempotency-Key behavior for unkeyed requests remains unchanged.
29. Existing artifact_key unique index and its scope semantics remain unchanged.
30. Existing Artifact metadata update, archive, version, and output workflows are not weakened.
31. Concurrent requests reusing one Idempotency-Key for different artifact_key values produce one idempotency winner and a deterministic request-hash conflict; they must not create two Artifacts.
32. For a keyed request, a failure before transaction commit leaves no newly committed Idempotency-Key reservation, Artifact, or creation identity.
33. A deferred trigger rejects commit of a new keyed Artifact without a matching identity row.
34. A deferred trigger rejects commit of an identity row whose artifact_key differs from the referenced Artifact.
35. An old reservation-first writer cannot overlap the new handler during cutover; deployment evidence demonstrates the old writer drain gate.


## 10. Failure-injection evidence

The implementation PR must attach reproducible test evidence for at least:

- process termination at each transaction boundary listed in 9.2;
- response loss after commit;
- stale Idempotency-Key reservation;
- Idempotency-Key cache expiry;
- same-key concurrent requests with identical and conflicting payloads;
- same Idempotency-Key concurrently reused across different artifact_key values;
- deferred identity-pair constraints and old-writer drain/cutover behavior;
- scope-isolation attempts;
- legacy-key collision.

Tests must assert database row counts and returned Artifact IDs, not only HTTP status codes.

## 11. Implementation sequence and gates

1. Reconfirm current default-branch schema, application create path, idempotency implementation, RLS/DB roles, audit helper, and test harness before writing migration code.
2. Add migration for the append-only identity table, constraints, indexes, and role/RLS policy.
3. Add canonicalization implementation with golden vectors.
4. Implement a single transactional create-or-resolve database/application path.
5. Change handler ordering so durable key resolution is not blocked by stale Idempotency-Key reservations.
6. Add conflict recovery using a fresh transaction.
7. Add API/OpenAPI contract tests and the full concurrency/crash-injection suite.
8. Run an independent review focused on transaction atomicity, legacy behavior, RLS, and failure recovery.

**AS-1 remains not implemented until all acceptance evidence exists.** Completing this document does not approve production code, SeeCode, Worker rollout, or the wider ART-04 implementation. AS-2, AS-3, AS-4, and AI-PM PM-1..PM-3 remain separate gates.

## 12. Open questions to resolve during independent review

1. Confirm whether Artifact create currently permits mutation of `artifact_key`; if so, enforce immutability without breaking legitimate legacy workflows.
2. Confirm the exact existing idempotency reservation/response-cache transaction semantics and adapt handler ordering without regressing non-keyed clients.
3. Confirm whether audit insertion can be part of the same transaction through the existing controlled function.
4. Confirm the service's canonical API behavior for a replayed create (200 existing resource vs 201 original-create semantics). The recommended contract is 200 for a durable key replay.
5. Confirm the final fingerprint field set against the actual CreateArtifactRequest DTO and any server-generated defaults.

No open question permits weakening the atomic Artifact + creation-identity transaction or treating the Idempotency-Key cache as durable authority.
