# ART-04F — Independent Source Review of AS-1 (ART-04E)

**Status:** Changes Required / Architecture implementation gate NOT passed  
**Date:** 2026-11-25  
**Reviewed baseline:** Artifact Service default branch `main`, fetched directly from GitHub on 2026-11-25  
**Specification reviewed:** ART-04E at the ART-04 branch head before this review  
**Scope:** Verify ART-04E against the actual Artifact creation path, persistence ports, idempotency implementation, DB roles, and migration schema.  
**Non-goals:** No code changes, migrations, API changes, SDK changes, Worker changes, or production data inspection.

## 1. Executive decision

ART-04E identifies the correct core failure: a durable Artifact creation identity cannot depend on the short-lived Idempotency-Key reservation. However, the current code architecture makes the proposed transaction substantially more invasive than the draft currently communicates.

**Decision: AS-1 remains Changes Required. Do not begin implementation.**

The implementation must introduce a single atomic persistence boundary for keyed Artifact creation. It is not sufficient to wrap the existing calls in a transaction at the application service level: the existing repository and idempotency ports each own separate pool-backed operations, and the idempotency implementation starts and commits its own transaction.

The review also found four contract details that need to be made explicit:

1. The current audit sink is deliberately best-effort and is not part of the Artifact creation transaction.
2. Existing Idempotency-Key request hashes are computed from raw request-body bytes, while the new Artifact creation fingerprint must be computed from validated semantic fields. These are two distinct hashes with different purposes.
3. The application role currently receives broad SELECT/INSERT/UPDATE table privileges, and no RLS policy was found in the reviewed schema. The new identity table must have explicit append-only grants/revokes and scope-safe SQL access; do not claim RLS protection that does not exist.
4. The current idempotency reservation commits independently, and the current create handler can leave an incomplete reservation after a process crash. The new path must not overlap old reservation-first writers during cutover.

## 2. Verified source baseline

The following default-branch files were fetched from GitHub. Blob SHA values are recorded to make this review reproducible.

| File | Blob SHA | Relevant verified fact |
|---|---|---|
| `crates/artifact-application/src/service/artifact.rs` | `842c8985f0f1d6dc2f8774a4c509399f69e875ef` | create flow reserves Idempotency-Key, inserts Artifact, audits, then completes idempotency in separate calls |
| `crates/artifact-application/src/port/idempotency.rs` | `8160aeac357de67883fe49a1eb2d69e80c2cbfda` | IdempotencyStore exposes separate reserve and complete methods |
| `crates/artifact-application/src/port/repo.rs` | `38f9b24c782bebf0c9fc9417bc948ca07d92f742` | ArtifactRepository exposes a separate insert method |
| `crates/artifact-infrastructure/src/postgres/mod.rs` | `0a5a7a89a1bbb72bc64bb0054d010f902cfd8f92` | PgArtifactRepository, PgIdempotencyStore and PgAuditSink each use PgPool-backed independent operations |
| `apps/artifact-api/src/routes/artifacts.rs` | `601005128107adaa8b095b0fe22e76fc062228cb` | ValidJson hashes original body bytes; handler forwards the hash and Idempotency-Key |
| `crates/artifact-contract/src/dto.rs` | `a20c62050762d83c9dd50eec3aa1f0d143f313f8` | CreateArtifactRequest has project_id, name, artifact_type, optional artifact_key, description, category |
| `migrations/0001_init.sql` | `73ff9302b509d96f3d89cb1f4601fdba6ecb851c` | artifact_key is nullable; scoped partial unique index exists; no artifact_key immutability trigger is present |
| `migrations/0003_job_coordination_fixes.sql` | `c0a829725d07fb71c3351c9f786a42d30a73352d` | Existing job coordination functions are SECURITY DEFINER; unrelated to Artifact create transaction |
| `deploy/db-roles.sql` | `847b1f9ace1c0532d61e8d2de9492bec9105a02f` | APP role gets SELECT/INSERT/UPDATE on all artifact tables and matching default privileges; audit_event UPDATE/DELETE are revoked |

The reviewed default-branch baseline is not the ART-04 branch. ART-04E is a proposed design on the feature branch and has not been implemented in the default branch.

## 3. Findings

### F-01 — Existing create path cannot provide the proposed atomic boundary by local wrapping

**Severity: P0 / Blocking**

Observed call sequence in `ArtifactService::create_artifact`:

1. `IdempotencyStore::reserve(...).await`
2. `ArtifactRepository::insert(...).await`
3. `audit_allowed(...).await`
4. `IdempotencyStore::complete(...).await`

The PgIdempotencyStore::reserve implementation opens a transaction, inserts/selects the idempotency record, and commits before returning. PgArtifactRepository::insert then executes an independent INSERT through its own pool. PgIdempotencyStore::complete performs a separate UPDATE through its pool.

Consequences:

- reservation may commit while Artifact insert never happens;
- Artifact may commit while idempotency completion fails;
- the application cannot atomically roll back all three using the current trait calls;
- two concurrent requests that reuse the same Idempotency-Key for different artifact_key values can be serialized by the idempotency unique constraint, but the independent reservation still does not atomically couple the winning key to the Artifact row.

**Required correction**

Introduce one purpose-built application port/use-case for keyed Artifact creation, implemented by one PostgreSQL transaction owner. That operation must coordinate:

- idempotency key reservation or existing-row resolution;
- durable artifact_key identity resolution;
- Artifact insertion;
- creation identity insertion;
- completed idempotency response snapshot/resource_id.

All of those database writes must use the same SQLx transaction and commit once. Do not try to pass a transaction through the current independent methods without redesigning their ownership contract. Do not create a generic cross-repository transaction abstraction unless other real use cases require it.

The existing unkeyed Artifact create path may retain its existing behavior in AS-1, but the exact split must be explicit and tested.

### F-02 — Audit is explicitly best-effort; ART-04E must not imply audit atomicity

**Severity: P1 / Contract correction**

`AuditSink::record` documentation states that audit is best-effort and audit failure must not change the business result (while leaving a log). PgAuditSink uses its own pool operation. The Artifact create service calls `audit_allowed` separately from the repository and idempotency calls.

**Required correction**

Remove audit_event from the required Artifact + creation identity + Idempotency-Key atomic commit. Preserve the existing best-effort audit policy unless a separate product/security decision changes it.

The new contract may require an audit attempt after successful commit, but it must not claim the audit row is transactionally guaranteed or make a failed audit write roll back a committed Artifact. If a durable audit guarantee is later required, design a transactional outbox/audit change as a separate decision.

### F-03 — Two hashes are required; raw JSON body hash is not the semantic creation fingerprint

**Severity: P1 / Contract correction**

The HTTP `ValidJson<T>` extractor computes SHA-256 over the original request bytes before deserialization. This is the current Idempotency-Key `request_hash`. Thus JSON whitespace or property-order changes produce different transport hashes.

The proposed ART-04E fingerprint is different: it is a versioned hash over validated, canonical semantic create fields.

**Required correction**

Keep the two values separate:

- `idempotency_request_hash`: preserve current raw-body hashing and existing Idempotency-Key conflict semantics for backward compatibility during AS-1;
- `artifact_creation_fingerprint`: compute from a validated canonical semantic DTO using a versioned canonicalizer.

Do not silently replace the existing idempotency hash algorithm in this change. If the product later wants semantic Idempotency-Key equivalence, that is a separate API compatibility change.

The canonical semantic field set must be explicitly reconciled with `CreateArtifactRequest`: `project_id`, `artifact_key`, `name`, `artifact_type`, `description`, and `category`. Tenant ID comes from the authenticated delegation, not from caller-controlled JSON. The service's authorization scope already treats tenant as credential-derived and project_id as resource-location input.

### F-04 — New identity table permissions need explicit append-only treatment; RLS is not the current boundary

**Severity: P1 / Security**

The reviewed `deploy/db-roles.sql` grants APP role SELECT, INSERT, UPDATE on all tables in schema artifact and matching default privileges. It explicitly revokes UPDATE/DELETE only for audit_event. No row-level security policy was found in the reviewed 0001 schema.

**Required correction**

For `artifact_creation_identity`:

- explicitly revoke UPDATE and DELETE from APP and WORKER roles;
- grant only the required SELECT/INSERT (or controlled-function EXECUTE) permissions;
- ensure future default privileges do not re-grant mutation rights;
- retain tenant_id/project_id predicates in every application query and enforce the composite FK;
- document that current isolation is application query scoping plus composite constraints, not RLS.

If the design chooses SECURITY DEFINER functions, pin search_path, validate caller scope inside the function, revoke PUBLIC EXECUTE, and grant only the exact function signature to the APP role. Do not add broad table privileges as a shortcut.

### F-05 — artifact_key is not currently immutable at the database layer

**Severity: P1 / Invariant gap**

The Artifact table has a partial unique index on `(tenant_id, project_id, artifact_key)`. The reviewed triggers touch updated_at/row_revision and guard current_version_id. No trigger rejects changes to artifact_key. The public PATCH DTO does not currently expose artifact_key, but the APP role has table UPDATE permission and other SQL paths could mutate it.

**Required correction**

Add a database-level immutability guard for an existing non-null artifact_key. Define behavior for NULL→non-NULL if legacy workflows need to establish a key; such a transition must create the durable identity in the same transaction and must not be a backdoor around the create-intent fingerprint. Prefer to prohibit key assignment after initial Artifact insertion in V1 unless a separately specified adoption flow exists.

A deferred constraint trigger alone is not enough to make the key immutable; the immutability guard and the deferred Artifact↔identity pairing checks are separate invariants.

### F-06 — Existing expiry column is not proof that an Idempotency-Key record has expired or is reusable

**Severity: P1 / Recovery correctness**

The idempotency table contains `expires_at`, but the reviewed PgIdempotencyStore reserve/select path does not test `expires_at`. It inserts with ON CONFLICT DO NOTHING, then reads the existing row. A completed snapshot is replayed; a row with no snapshot is treated as still in progress. No expiry-aware reuse behavior is present in that code path.

**Required correction**

Define expiration as actual retention cleanup/deletion or an explicit atomic expired-row replacement policy, not merely `now() > expires_at`. AS-1 must not assume an expired row can be reused. The durable artifact_key path must still resolve the Artifact after idempotency retention cleanup. Any replacement policy must serialize concurrent requests and preserve the existing request-hash conflict behavior.

### F-07 — Current request scope and canonicalizer inputs need a precise trusted-source boundary

**Severity: P1 / Contract correction**

CreateArtifactRequest contains project_id, while the handler constructs TenantScope using the delegation's tenant_id and the request's project_id. The delegation checks ArtifactCreate against that scope.

**Required correction**

Fingerprint project_id only after successful scope authorization, using the canonical validated project scope. Tenant_id must be sourced from the verified delegation. Never accept tenant_id as a fingerprint scope input from the JSON request. The fingerprint must not become an authorization shortcut: every retry/replay still requires fresh authorization.

## 4. Required revised transaction shape

For keyed creates only, the preferred design is one focused PostgreSQL transaction owner, exposed as a purpose-built application port. Conceptual sequence:

1. Validate request DTO and authorize current delegation/scope.
2. Compute the semantic Artifact creation fingerprint and preserve the existing raw-body idempotency request hash separately.
3. Begin one SQL transaction.
4. Resolve or atomically reserve the endpoint-scoped Idempotency-Key:
   - if same key exists with different raw-body hash, return the existing conflict;
   - if same hash has a completed response, replay it;
   - if same hash is an incomplete pre-cutover record, do not treat it as safely reusable until old writers are drained and the record has been reconciled under the approved cutover policy.
5. Resolve durable `(tenant_id, project_id, artifact_key)` identity:
   - same semantic fingerprint: return the same Artifact;
   - different fingerprint: 409;
   - legacy Artifact key without identity: fail closed.
6. If no durable identity exists, insert Artifact + identity in this transaction.
7. Write the completed Idempotency-Key response snapshot/resource_id in this same transaction, if a key was supplied.
8. Commit once.
9. Attempt best-effort audit after commit; audit failure is logged but does not alter the successful create result.

The exact SQL ordering must account for unique-key races. Any transaction that receives a PostgreSQL unique violation must roll back before re-reading the winner in a fresh transaction.

## 5. Deferred constraint and migration review

ART-04E's deferred bidirectional Artifact↔identity invariant is directionally correct, but implementation must explicitly address:

- deferred constraint triggers are evaluated at commit and must only reject newly inserted/key-transitioned rows, not untouched legacy rows;
- Artifact insert and identity insert may happen in either order within the same transaction;
- identity insertion must validate exact Artifact ID, tenant, project, and artifact_key equality;
- existing keyed rows without identity must remain legacy-unverifiable and must not be silently backfilled;
- trigger execution must be tested with the actual migration owner and APP role privileges;
- trigger errors must map to stable API conflict/invariant error codes rather than a generic 500;
- the unique Artifact index and identity primary key must remain consistent under concurrent same-key inserts.

The new identity table must be included in the deployment role script's explicit grants/revokes. Do not rely only on current default privileges, because those currently grant APP UPDATE to every new table.

## 6. Cutover and legacy reservation handling

The old handler and new transaction owner must not write keyed creates concurrently.

Before enabling the new path:

1. stop admission of keyed Artifact creates through the old handler;
2. drain all in-flight old create requests and prove no old process can write afterward;
3. inventory existing keyed Artifacts without identity rows;
4. inventory incomplete Idempotency-Key records for POST /v1/artifacts;
5. reconcile incomplete records only after the old-writer drain is proven;
6. deploy migration, grants/revokes, and new handler in a controlled sequence;
7. verify old binaries are no longer serving keyed creates before reopening traffic.

Do not infer that a record is reusable solely because `expires_at` is in the past.

## 7. Acceptance criteria additions

Add the following tests to ART-04E's acceptance matrix:

1. Keyed create uses one SQL transaction for idempotency reservation, Artifact, creation identity, and completed idempotency response.
2. Failure at any point before commit leaves none of those new rows committed.
3. Best-effort audit failure after commit does not change the create response or roll back the Artifact.
4. Raw-body idempotency hash remains byte-sensitive under existing behavior; semantic creation fingerprint is stable across equivalent JSON serialization.
5. Same Idempotency-Key with same raw-body hash and different artifact_key cannot create two Artifacts.
6. Same Idempotency-Key with different raw-body hash preserves the existing idempotency conflict.
7. APP role cannot UPDATE or DELETE creation identity rows.
8. APP role cannot mutate an existing artifact_key, including direct SQL attempts.
9. NULL→non-NULL artifact_key assignment is rejected unless a separately specified atomic identity-adoption operation is used.
10. A row whose expires_at is in the past but remains in the table is not silently treated as absent/reusable.
11. Current delegation authorization is rechecked before replaying an existing Artifact.
12. No code path claims RLS isolation unless RLS is actually introduced and tested.

## 8. Review disposition and next actions

**Disposition: Changes Required.**

Before AS-1 can be approved for implementation:

1. Revise ART-04E to incorporate F-01 through F-07.
2. Decide whether keyed and unkeyed creates use separate application paths in V1; recommended: keep the existing unkeyed path unchanged and introduce a new atomic keyed-create use case.
3. Specify the purpose-built application port and PostgreSQL transaction ownership; do not expose SQLx Transaction through unrelated repositories.
4. Define audit as best-effort post-commit, preserving current policy.
5. Define separate raw-body Idempotency-Key hash and semantic Artifact creation fingerprint.
6. Specify the APP role grants/revokes and explicitly state the current lack of RLS.
7. Re-review the revised specification and its cutover sequence.

This review does not authorize implementation. It does not approve ART-04B/AS-2/AS-3/AS-4, Worker runtime, SeeCode activation, or production generation.


## 9. Follow-up specification convergence (2026-11-25)

ART-04E has since been revised to close the design gaps identified in F-01..F-07. The following decisions are now explicit:

- **Expiry:** `expires_at` is only cleanup eligibility. Completed records replay while physically present; cleanup physically deletes only completed expired rows under row locking. Incomplete rows are never TTL-deleted and require post-drain reconciliation.
- **Legacy incomplete records:** after old writers are drained, reconcile only when the durable Artifact identity and raw Idempotency-Key request hash prove the same operation. No Artifact/no identity may be abandoned only through an explicit operator-approved action. Artifact without identity or inconsistent evidence remains blocked.
- **Old-writer cutover:** gate keyed writes on every instance, drain in-flight requests, remove old instances, apply identity migration/invariant backstop, deploy the new build while gated, verify schema/build compatibility, reconcile legacy records, run DB-backed smoke tests, then reopen traffic. If the platform cannot prove the drain, rollout is blocked.
- **Application port:** a purpose-built `create_or_resolve_keyed` operation owns one SQLx transaction and returns typed outcomes (`Created`, `ResolvedExisting`, `ReplayedByIdempotency`, typed conflicts, retryable failure). Existing unkeyed create remains on its current path in V1.
- **Replay integrity:** a completed Idempotency-Key row is not blindly replayed; its resource_id must resolve to an Artifact and matching durable identity/fingerprint in the currently authorized scope.
- **Acceptance:** ART-04E now defines 56 acceptance cases, including cleanup races, incomplete-row reconciliation, replay integrity, old-writer drain evidence, and response semantics.

These are specification corrections, not implementation evidence. The source-review findings remain historically valid. **Disposition remains Changes Required until a final independent review of the revised ART-04E confirms that these policies are mutually consistent and implementable against the repository's current schema and deployment model.** No implementation is authorized by this follow-up.


### Additional adversarial clarification

The original cutover wording assumed the old binary could honor a new per-instance keyed-create gate. That is not a safe assumption. ART-04E now requires an ingress-level pause of the entire `POST /v1/artifacts` route during cutover (temporarily pausing unkeyed creates as well), unless a gate-capable compatibility release is deployed to every instance first. The migration is not applied until in-flight create requests are zero and all old instances are removed.

The Idempotency-Key cleanup worker is also explicitly prohibited from receiving broad table DELETE privileges. Cleanup must use a narrowly scoped maintenance function that deletes only completed expired rows in bounded batches; incomplete-row reconciliation remains operator-controlled.

One acceptance case was corrected: failure to write the completed idempotency response occurs before commit and therefore rolls back Artifact, identity, and reservation together. The prior wording that allowed Artifact commit followed by a separate idempotency completion failure contradicted the required single-transaction boundary.
