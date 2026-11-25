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

The Artifact creation fingerprint is:

`lowercase_hex(SHA-256(UTF-8(canonical_json_v1)))`

This is distinct from the existing endpoint Idempotency-Key `request_hash`, which is currently computed over the raw HTTP JSON body bytes. AS-1 MUST preserve that byte-sensitive Idempotency-Key behavior for compatibility; it must not silently replace it with semantic canonicalization. The keyed-create transaction therefore handles two independent hashes:

- `idempotency_request_hash`: the existing raw-body SHA-256 used only for Idempotency-Key reuse/conflict semantics;
- `artifact_creation_fingerprint`: this versioned semantic hash used for stable artifact_key identity.

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

1. Validate the request DTO and re-check current delegation authorization for the tenant/project scope. Tenant ID comes from verified delegation; project ID is the validated project locator from the request.
2. Insert the Artifact row with its stable `artifact_key`.
3. Insert the matching immutable `artifact_creation_identity` row with fingerprint version/hash.
4. If an Idempotency-Key is supplied, atomically resolve/reserve its endpoint-scoped record and write its completed response snapshot/resource_id in this same transaction.
5. Commit once.

The Artifact, identity row, and (when supplied) Idempotency-Key record + completed response either all commit or all roll back. No externally visible intermediate state is permitted.

**Audit is intentionally excluded from this transaction.** The current AuditSink contract is best-effort and states that audit failure must not change the business result. After commit, attempt the existing audit write; log failures without rolling back or changing the successful create response.

For keyed requests, the existing two-transaction "reserve first, create later, complete last" flow MUST be replaced by one transaction owner. A keyed request must not commit a standalone reservation before its Artifact operation. This also prevents two different artifact keys from being created concurrently under the same endpoint-scoped Idempotency-Key.

### 5.2 Request algorithm

For a keyed create request:

1. Authenticate and authorize the tenant/project. Validate and canonicalize the request; compute the versioned Artifact creation fingerprint and, when supplied, the existing endpoint-specific Idempotency-Key request hash.
2. Start one transaction. For a supplied Idempotency-Key, atomically insert its reservation row (using the existing unique scope `tenant_id/project_id/endpoint/idempotency_key`) or lock/read the existing row:
   - a different request hash returns the existing idempotency conflict;
   - a completed matching row is a transport replay candidate, but before returning it the transaction MUST resolve its resource_id within the authorized tenant/project and verify the matching durable artifact_key identity and stored-version semantic fingerprint. A missing Artifact, missing identity, or mismatch is an integrity/reconciliation error; do not blindly replay a stale response snapshot.
   - an incomplete legacy row is not proof that the Artifact operation did not commit. It may be reconciled/reused only after the old-writer drain in §6.1 and an explicit check of both Artifact and identity state; if the key is still ambiguous, fail closed for operator reconciliation.
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

## 5.6 Application port and transaction ownership

The current `ArtifactService`, `ArtifactRepository`, and `IdempotencyStore` are separate ports. The current PostgreSQL implementations each use pool-backed operations; `PgIdempotencyStore::reserve` begins and commits its own transaction. Therefore the current calls cannot be made atomic by simply adding a transaction around the application service.

For keyed requests, introduce one purpose-built application port/use case (name to follow repository conventions, e.g. `KeyedArtifactCreatePort::create_or_resolve`) whose PostgreSQL implementation owns the complete SQLx transaction. It must coordinate idempotency reservation/resolution, durable identity resolution, Artifact + identity insertion, and completed idempotency response in one transaction. Do not leak a SQLx Transaction through unrelated repository traits.

Recommended V1 compatibility split:
- `artifact_key IS NOT NULL`: use the new atomic keyed-create use case;
- `artifact_key IS NULL`: preserve the existing unkeyed create behavior and existing Idempotency-Key contract, unless a separate migration decision broadens scope.

The split must be explicit in tests and telemetry. The new keyed path must still perform fresh authorization before returning a replayed Artifact.

## 5.7 Keyed-create application port contract

The application layer must expose one purpose-built operation for keyed creates. The name may follow repository conventions, but its semantics must be equivalent to:

```rust
async fn create_or_resolve_keyed(
    scope: AuthorizedTenantProjectScope,
    request: ValidatedCreateArtifactRequest,
    idempotency: Option<IdempotencyInput>,
) -> Result<KeyedArtifactCreateOutcome, KeyedArtifactCreateError>;
```

Where:

- `AuthorizedTenantProjectScope` is built only after current delegation authorization; tenant_id is credential-derived and project_id is the validated requested project.
- `ValidatedCreateArtifactRequest` contains the normalized DTO and stable non-null artifact_key.
- `IdempotencyInput` contains the endpoint-scoped key and the existing raw-body request hash; it does not contain a caller-selected tenant.
- The PostgreSQL adapter owns one SQLx transaction for the entire keyed operation. It must not call the existing pool-committing `PgIdempotencyStore::reserve/complete` or `PgArtifactRepository::insert` methods inside that transaction.

The outcome must distinguish at least:

- `Created { artifact }`: this transaction created the Artifact and durable identity.
- `ResolvedExisting { artifact }`: same artifact_key and semantic fingerprint already existed; this request resolved it (including after Idempotency-Key cache expiry or with a rotated Idempotency-Key).
- `ReplayedByIdempotency { artifact }`: a matching completed Idempotency-Key response was found and safely resolved to the same scoped Artifact.
- typed conflicts: Idempotency-Key request-hash mismatch, artifact_key semantic-fingerprint mismatch, and legacy-unverifiable key.
- retryable persistence failure after bounded transaction retries.

For every successful outcome with an Idempotency-Key, the transaction must leave a completed idempotency row whose response snapshot/resource_id resolves to the same Artifact. If an existing durable Artifact is resolved under a new Idempotency-Key, the transaction must complete that new idempotency record before commit. No success response may be returned before commit.

The port returns domain outcomes, not raw SQLx errors. Unique-constraint races are resolved by rollback and fresh transaction; the port must never continue in a PostgreSQL transaction after a constraint error. The handler maps outcomes to the HTTP behavior in §7.

The unkeyed path remains on the existing application ports in V1 and is not silently routed through this new port.

## 5.5 Deferred database invariants

The migration must enforce both directions of the identity relationship:

- A deferred constraint trigger on newly inserted/key-mutated Artifact rows verifies at commit that a non-null `artifact_key` has a matching identity row for the same tenant/project/Artifact.
- A deferred constraint trigger on identity-row insertion verifies at commit that the referenced Artifact has exactly the same `artifact_key` and scope.
- Runtime permissions and append-only triggers prevent identity update/delete/rebinding.
- A BEFORE UPDATE guard rejects any change to an existing Artifact's non-null `artifact_key`.
- In V1, NULL→non-NULL key assignment on an existing Artifact is also rejected. If a future adoption workflow needs to assign a key to an existing unkeyed Artifact, it must be a separately specified atomic operation that creates the immutable identity and fingerprint in the same transaction; it is not part of AS-1.

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

### 6.2 Idempotency-Key retention, expiry, and reconciliation

For AS-1, `expires_at` is a retention eligibility timestamp, not an automatic logical deletion or permission to reuse a key.

V1 policy:

1. A completed Idempotency-Key row remains authoritative for transport replay while it physically exists, including after `expires_at`. Matching raw-body hash replays its stored response; a different hash retains the existing conflict behavior.
2. A cleanup job may physically delete only completed rows with `expires_at <= now()`. Deletion must be bounded and use row-level locking / `FOR UPDATE SKIP LOCKED` (or equivalent) so it cannot race a request currently resolving the same key.
3. Incomplete rows are never deleted automatically by TTL cleanup. They require reconciliation after the old-writer drain.
4. For an incomplete legacy row after the old-writer drain, the reconciler checks the durable Artifact identity and the legacy Artifact table in the same scoped context:
   - matching identity and matching raw request hash: complete the idempotency response with that Artifact;
   - no Artifact and no identity: mark the record as safely abandoned or delete it under an explicit operator-approved reconciliation action, after which a retry may reserve the key;
   - Artifact exists without identity, or evidence is inconsistent: keep the row blocked and require operator investigation; do not infer a fingerprint.
5. Once a completed row is physically deleted, that Idempotency-Key may be used again under the existing retention contract. The durable `artifact_key` still prevents a duplicate logical Artifact and checks the semantic fingerprint.
6. The cleanup job must not delete or mutate `artifact_creation_identity` rows. Durable identity retention is independent of Idempotency-Key retention.
7. The cleanup worker must not receive broad table DELETE privileges. Expose a narrowly scoped maintenance function (or equivalent privileged maintenance path) that deletes only completed, expired Idempotency-Key rows in bounded batches under the required row-lock semantics. Revoke PUBLIC execution and grant only the designated maintenance principal. Incomplete-row reconciliation is a separate operator-controlled operation and must not be callable as routine TTL cleanup.

The cleanup/reconciliation mechanism, metrics, and operator runbook are required implementation artifacts. The exact cleanup batch size is operational configuration, not an API contract.

### 6.1 Cutover gate for old reservation-first writers

The current implementation reserves Idempotency-Key in a separate operation before inserting the Artifact. An old handler may therefore still be in flight while a new handler tries to recover its stale reservation.

Before enabling the new keyed-create transaction path, perform a controlled, observable cutover. The current old binary has no keyed-create maintenance gate, so the gate MUST NOT assume that an old handler can honor a newly introduced application flag.

Preferred V1 cutover:

1. At the ingress/API gateway, temporarily disable the entire `POST /v1/artifacts` create route and return a stable retryable maintenance response. This intentionally pauses unkeyed creates too; it avoids relying on code that does not yet contain the gate.
2. Wait for the in-flight POST create request count across all instances to reach zero. Stop and remove every old application instance. Verify the deployment inventory contains no old binary and no old process can receive traffic.
3. Apply the identity migration and DB grants/revokes only after the old writers are drained. The deferred invariant trigger then provides a database backstop: any residual old binary attempting a keyed Artifact insert without an identity row must fail at commit.
4. Deploy the new binary with the create route still gated. Verify all instances report the new build/schema compatibility version and the new transactional keyed-create port is active.
5. Reconcile pre-existing incomplete Idempotency-Key records under §6.2 and inventory legacy keyed Artifacts without identity rows.
6. Run scoped keyed and unkeyed smoke tests through a controlled internal route, verifying Artifact + identity + completed idempotency state for keyed requests.
7. Reopen `POST /v1/artifacts` only after all checks pass. Record deployment versions, gate transitions, drain metrics, migration version, and reconciliation counts as rollout evidence.

If a full route pause is operationally unacceptable, first deploy a gate-capable compatibility release to every instance, verify all instances run it, then enable its keyed-create gate and drain keyed requests before proceeding. Do not assume old binaries honor a gate they were never built to support.

If the platform cannot prove the old-writer drain or prevent old binaries from returning, do not apply the migration or enable the new path. A database trigger is a final invariant backstop, not a substitute for draining old writers.

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
### 8.1 Audit transaction boundary

The current AuditSink contract is best-effort: audit failure must not change the business result. Audit is not part of the keyed-create atomic transaction in AS-1. Attempt the audit write after the database commit; log failures. A transactional audit guarantee would require a separate explicit policy change (for example, a transactional outbox) and is outside AS-1.


- Authenticate and authorize on every create/replay request, including requests that resolve an existing key.
- The caller must have current access to the requested tenant/project before the service reveals the Artifact representation.
- Every identity lookup includes both tenant_id and project_id derived from trusted authenticated context; never trust caller-supplied scope without the existing authorization boundary.
- The current reviewed schema does not establish RLS as the tenant-isolation boundary. Current access relies on explicit tenant_id/project_id query predicates, delegation checks, and composite constraints. Do not claim RLS protection unless a separate RLS design is introduced and tested.
- The current db-roles script grants APP SELECT/INSERT/UPDATE on all Artifact schema tables, including future tables through default privileges. The new identity table MUST explicitly REVOKE UPDATE and DELETE from APP and WORKER roles and grant only required SELECT/INSERT (or exact controlled-function EXECUTE). Ensure default privileges do not re-grant mutation rights.
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
12. Writing the completed Idempotency-Key response fails before transaction commit: Artifact, identity, and idempotency reservation all roll back; retry can safely create or resolve the same key.
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
36. The keyed-create transaction commits idempotency reservation, Artifact, creation identity, and completed response snapshot/resource_id together.
37. Best-effort audit failure after commit does not change the successful create response or roll back the Artifact.
38. Existing raw-body Idempotency-Key hashing remains byte-sensitive; the semantic Artifact creation fingerprint is stable across equivalent JSON serialization.
39. APP role cannot UPDATE or DELETE creation identity rows.
40. APP role cannot mutate an existing artifact_key through direct SQL.
41. NULL→non-NULL artifact_key assignment on an existing Artifact is rejected unless a separately approved adoption operation is used.
42. An idempotency row whose expires_at is in the past but remains stored is not silently treated as absent or reusable.
43. Fresh delegation authorization is required before replaying an existing Artifact.
44. No test or documentation claims RLS isolation unless RLS is explicitly introduced and tested.
45. Completed Idempotency-Key replay verifies that resource_id resolves to an Artifact and matching durable identity in the authorized scope before returning a response.
46. A completed idempotency snapshot pointing to a missing Artifact or mismatched identity fails closed and emits an integrity signal.
47. Completed rows past expires_at remain replayable until the cleanup transaction physically deletes them.
48. Cleanup and an incoming request racing for the same Idempotency-Key cannot both win; row locking produces one deterministic outcome.
49. TTL cleanup never deletes incomplete Idempotency-Key rows.
50. Post-drain reconciliation completes an incomplete row only when matching Artifact identity and raw request hash are proven.
51. Post-drain reconciliation does not auto-delete or reuse a row when an Artifact exists without durable identity.
52. The ingress gate rejects all POST /v1/artifacts creates consistently during the migration window, including unkeyed creates, unless every instance has first been upgraded to a gate-capable compatibility release.
53. The drain procedure records zero in-flight create requests and verifies no old application instance remains before migration.
54. The database deferred invariant rejects a residual old binary's keyed Artifact insert without a matching identity row.
55. The new application build remains gated until schema compatibility and atomic keyed-create smoke checks pass.
56. New and existing-key responses use the frozen 201/200 semantics and a stable resolution indicator/header.


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
2. Add migration for the append-only identity table, constraints, indexes, and explicit APP/WORKER grants/revokes. Do not describe the current design as RLS-protected.
3. Add canonicalization implementation with golden vectors.
4. Implement a single transactional create-or-resolve database/application path.
5. Change handler ordering so durable key resolution is not blocked by stale Idempotency-Key reservations.
6. Add conflict recovery using a fresh transaction.
7. Add API/OpenAPI contract tests and the full 56-case concurrency/crash-injection/retention/cutover suite.
8. Run an independent review focused on transaction atomicity, legacy behavior, RLS, and failure recovery.

**AS-1 remains not implemented until all acceptance evidence exists.** Completing this document does not approve production code, SeeCode, Worker rollout, or the wider ART-04 implementation. AS-2, AS-3, AS-4, and AI-PM PM-1..PM-3 remain separate gates.

## 12. Decisions frozen for the next review

1. **HTTP replay semantics:** new Artifact is 201; durable key resolution or completed idempotency replay is 200. Both return the same Artifact representation and an explicit `resolution` field (`created`, `existing_key`, or `idempotency_replay`) if consistent with the API response conventions. If the current response envelope cannot add this field compatibly, use a response header rather than changing existing required fields.
2. **Fingerprint defaults:** canonicalization is computed from the validated DTO after applying deterministic server defaults. Before implementation, record the actual defaults in golden test vectors from `CreateArtifactRequest`; no default may depend on wall-clock time, random values, or request order.
3. **Old-writer drain:** use the ingress-level POST route pause (or first deploy a gate-capable compatibility release), drain in-flight create requests, remove old instances, apply the migration invariant backstop, verify the new build, and run gated smoke tests as defined in §6.1. If the deployment platform cannot prove these steps, rollout is blocked.
4. **Idempotency expiry:** `expires_at` is retention eligibility only. Completed rows are replayable until physically deleted by the locked cleanup process; incomplete rows are never TTL-deleted and require explicit post-drain reconciliation under §6.2.
5. **Keyed-create port:** implement the purpose-built `create_or_resolve_keyed` operation and typed outcomes defined in §5.7. Keep unkeyed create on its existing path in V1.

These decisions close the design questions for the next independent review; they do not approve implementation.
