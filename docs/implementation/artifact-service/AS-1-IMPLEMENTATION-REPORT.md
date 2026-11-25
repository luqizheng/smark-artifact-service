# AS-1 — Durable Artifact Creation Identity：实施报告

- 分支：`feat/as-1-durable-artifact-creation-identity`
- 基线：`docs/art-04-ai-pm-job-control-contract`（`2c3c7a5`）
- 契约：ART-04E（冻结）、ART-04F（独立审查）、ART-04D（实施拆分）
- 范围：仅 AS-1。未涉及 SeeCode Generator、Worker、AS-2/3/4、PM-1/2/3、LMMS。

## 1. 变更文件清单

新增：

| 文件 | 作用 |
|---|---|
| `migrations/0004_artifact_creation_identity.sql` | 持久化创建身份表、唯一约束、不可变/配对触发器、清理函数与授权 |
| `crates/artifact-application/src/port/keyed_create.rs` | 专用 keyed-create 端口、DTO 校验与规范化、指纹输入、typed outcomes |
| `crates/artifact-application/src/service/creation_fingerprint.rs` | 版本化 canonical fingerprint（v1）与 golden vector 单元测试 |
| `crates/artifact-infrastructure/src/postgres/keyed_create.rs` | 单事务 PostgreSQL 实现与唯一约束竞争恢复 |
| `tests/tests/as1_creation_identity.rs` | AS-1 数据库级集成/并发/故障注入测试 |

修改：

`apps/artifact-api/src/routes/artifacts.rs`、`apps/artifact-api/src/state.rs`、
`crates/artifact-application/{Cargo.toml,src/lib.rs,src/port/mod.rs,src/service/mod.rs,src/service/artifact.rs}`、
`crates/artifact-infrastructure/{Cargo.toml,src/postgres/mod.rs}`、
`crates/artifact-client/src/lib.rs`、`crates/artifact-contract/src/error.rs`、`crates/artifact-domain/src/error.rs`、
`deploy/{db-roles.sql,dev-grants.sh,docker-compose.yml}`、`migrations.sha256`、`openapi/openapi.yaml`、
`tests/Cargo.toml`、`tests/tests/{migration_invariants.rs,roles_separation.rs,worker_roles.rs}`。

## 2. Migration 说明（`0004_artifact_creation_identity.sql`）

- `artifact.artifact_creation_identity`：主键 `(tenant_id, project_id, artifact_key)`，
  唯一 `(tenant_id, project_id, artifact_id)`，复合外键指向 `artifact.artifact(tenant_id, project_id, id)`。
  `fingerprint_version` 固定 1，`request_fingerprint` 为 64 位小写 hex。
- `trg_creation_identity_append_only`：身份行禁止 UPDATE / DELETE（`P0001`）。
- `trg_artifact_identity_pairing`（`CREATE CONSTRAINT TRIGGER ... DEFERRABLE INITIALLY DEFERRED`）：
  新写入的 keyed Artifact 若无配对身份，COMMIT 时整体回滚。
- `trg_creation_identity_artifact_pairing`：身份行的 `artifact_key` 必须与被引用 Artifact 的
  `artifact_key` 完全一致（含拒绝 NULL），COMMIT 时校验。
- `trg_artifact_key_immutable`：已存在的非 NULL `artifact_key` 禁止修改；
  NULL→非 NULL 在 V1 同样拒绝（adoption 流程需另行设计与审批）。
- `artifact.purge_expired_idempotency_records(batch_size integer)`：
  `SECURITY DEFINER`、`SET search_path = pg_catalog, pg_temp`、`batch_size` 限制 1..1000、
  `FOR UPDATE SKIP LOCKED`、仅删除 `response_snapshot IS NOT NULL AND expires_at < now()`。
  `REVOKE ALL ... FROM PUBLIC`，仅授予 `__MAINTENANCE_ROLE__` EXECUTE。
- `deploy/db-roles.sql`：APP 仅 SELECT/INSERT/UPDATE，显式 `REVOKE UPDATE, DELETE`
  于身份表并写入 default privileges；maintenance 只有 schema USAGE + 清理函数 EXECUTE，
  无任何业务表直接访问权。
- `migrations.sha256` 已登记 0004 的 SHA-256；`migration_guard` 校验通过。

向前兼容性：0004 只新增表/约束/函数与授权，不修改既有表结构；旧二进制仍可写 unkeyed。
keyed 写入在旧二进制下会被延迟约束拒绝，这是设计中的切换门禁（见 §4）。

## 3. 事务设计说明

- 唯一事务所有者是 `KeyedArtifactCreationPort::create_or_resolve_keyed`
  （实现：`PgKeyedArtifactCreator`）。SQLx `Transaction` 不进入任何 Repository trait。
- 单事务顺序：
  1. 幂等预留检查/冲突判定（`INSERT ... ON CONFLICT DO NOTHING`，冲突时 `SELECT ... FOR UPDATE`）；
  2. 读取持久化身份；
  3. 以 `artifact_key` 索引探测历史/并发 Artifact；
  4. 插入 Artifact；
  5. 插入创建身份（指纹基于校验并 NFC 规范化后的 DTO）；
  6. `UPDATE idempotency_record` 写入完成快照与 `resource_id`；
  7. 一次 COMMIT。
- 任一 23505：**先回滚**，sleep 后以新事务重读身份/Artifact 解析竞争胜者；最多 3 次尝试，
  超限返回 `RetryablePersistenceFailure`（503），绝不吞掉冲突生成重复 Artifact。
- 完成记录重放：校验 `request_hash` 一致 → 解析 `resource_id` 对应 Artifact →
  校验同 scope 持久化身份一致；任一不满足返回 `IntegrityFailure`（失败关闭 + error 日志），
  绝不盲目返回快照。
- 指纹 = `SHA-256` over `{"artifact_key","artifact_type","category","description","domain","name","project_id","tenant_id"}`
  （键序固定、compact JSON、`domain="artifact-create:v1"`）。`created_by` 不参与，
  避免服务凭据轮换产生第二个身份。Idempotency-Key 的 `request_hash` 仍是原始 Body 字节哈希，
  与语义指纹完全独立。
- 审计在 COMMIT 之后 best-effort 执行，失败仅告警，不影响已成功的创建响应。

## 4. 部署与切换 Runbook（仓库无部署能力，以下为人工步骤）

旧版本不支持新 gate，不能假设它能识别开关；必须按 ART-04E §6.1 执行：

1. 在 ingress **暂停整个 `POST /v1/artifacts` 路由**（同时覆盖 keyed 与 unkeyed）。
   > 若只能按实例/按版本灰度 gate，则必须先发布一个「所有实例都支持 gate」的兼容版本，
   > 再进入后续步骤；否则禁止开始切换。
2. 排空在途创建请求：确认 ingress 与 APP 侧 `POST /v1/artifacts` in-flight 归零。
3. 确认所有旧实例已退出（无旧版本 pod/进程持有数据库连接）。
4. 应用数据库变更：执行 `0004`，随后以迁移主体执行 `deploy/db-roles.sql`
   （设置 `ARTIFACT_MAINTENANCE_DB_USER`，默认 `artifact_maintenance`）。
5. 部署新版本（创建路由仍保持关闭）。
6. 验证 schema 兼容：`migrations.sha256` 校验、新版本启动/健康检查通过。
7. 数据库级 smoke test（在关闭路由时由运维用受控主体执行）：
   - keyed create 正常：Artifact + identity + 幂等完成快照同事务存在；
   - unkeyed create 仍为旧行为；
   - 重复 keyed create（同指纹）解析同一 Artifact；不同指纹得到 409 语义。
8. 确认 Artifact / identity / idempotency 三者原子一致后，重新开放路由。
9. 记录切换时间、执行人、in-flight=0 证据、旧实例下线证据。

清理任务：由 maintenance 主体按固定周期调用
`SELECT artifact.purge_expired_idempotency_records(100);`，不得授予该主体表级 DELETE。

### 4.1 Legacy 未完成幂等行的对账（ART-04E §6.2.4）

前置：§4 步骤 1–3 已证明旧写入者排空。新 handler 的「身份优先解析」路径就是
排空后的对账机制：它持有同一条 raw-body hash 与 `artifact_key`，在同一事务内证明
Artifact/身份状态后才完成预留；不存在 Artifact 且不存在身份时才创建。

运维补充步骤（不自动执行）：

1. 盘点：`SELECT count(*) FROM artifact.idempotency_record WHERE response_snapshot IS NULL;`
   （不要导出跨租户明细）。
2. 优先让原客户端用**同一 Idempotency-Key 与同一 Body**重试，由 handler 完成 §6.2.4
   的判定；这是唯一能同时证明 raw hash 与 artifact_key 的路径。
3. 若该 key 已无法重试，且经查本 scope 无对应 Artifact/身份，可在变更审批下由迁移主体
   显式删除该**未完成**行（`DELETE ... WHERE id=$1 AND response_snapshot IS NULL`），
   之后重试可重新预留。maintenance 主体不得执行此操作。
4. 若存在 Artifact 但无身份行：保持阻塞，API 返回 `ARTIFACT_KEY_LEGACY_UNVERIFIABLE`，
   转入身份 adoption 的独立设计审批（不属于 AS-1）。

运营信号（V1 不引入新指标栈）：结构化日志事件 `artifact_creation_integrity` 表示
完整性失败关闭；另以固定 SQL 监控未完成行数量、无身份 keyed Artifact 数量与 503 比例。

## 5. 57 项验收矩阵

结果均为本机 **真实 PostgreSQL 16.16** 上执行 `cargo test` 的结果（`RUST_TEST_THREADS=4`）。
「人工/部署」项无法由代码自动化，必须由 §4 Runbook 的切换证据关闭。

### 9.1 Canonicalization

| # | 场景 | 实现 | 测试 | 结果 |
|---:|---|---|---|---|
| 1 | 键序不同 → 同指纹 | `creation_fingerprint.rs` 由 DTO 生成 | `equivalent_json_serializations_share_one_fingerprint` | PASS |
| 2 | 空白不同 → 同指纹 | 同上（不哈希原始字节） | 同上 | PASS |
| 3 | null/omitted 归一 | `ValidatedCreateArtifactRequest` 的 `Option` | `creation_fingerprint::tests::null_and_absent_are_equivalent_but_empty_string_is_not` | PASS |
| 4 | Unicode 等价 NFC | `normalize_text` (NFC) | `unicode_equivalent_requests_share_one_fingerprint` + 同名单测 | PASS |
| 5 | 语义字段变化 → 异指纹 | v1 字段集 | `creation_fingerprint::tests::each_semantic_field_changes_the_fingerprint` | PASS |
| 6 | golden vector 稳定 | v1 冻结常量 | `creation_fingerprint::tests::v1_golden_vector_is_frozen` | PASS |

### 9.2 Atomicity and crash recovery

| # | 场景 | 实现 | 测试 | 结果 |
|---:|---|---|---|---|
| 7 | Artifact 插入前/中途崩溃 | 单事务 | `crash_mid_keyed_transaction_rolls_back_all_writes`（`pg_terminate_backend` 终止在途事务） | PASS |
| 8 | Artifact 后、identity 前崩溃 | 延迟约束 + 单事务 | `transaction_rollback_leaves_no_partial_state` | PASS |
| 9 | identity 后、COMMIT 前崩溃 | 单事务回滚 | 同上（第二段）+ `crash_mid_keyed_transaction_rolls_back_all_writes` | PASS |
| 10 | 提交成功但响应丢失 | 身份解析 | `same_key_same_fingerprint_resolves_existing` | PASS |
| 11 | 预留存在但进程死亡 | 身份优先解析 + 完成预留 | `stale_incomplete_reservation_is_completed_by_durable_resolution` | PASS |
| 12 | 完成快照写入失败 | 同事务 | `completion_write_failure_rolls_back_artifact_identity_and_reservation`（触发器注入） | PASS |
| 13 | 幂等缓存过期 | 身份独立于 TTL | `expired_completed_idempotency_still_replays`、`same_key_same_fingerprint_resolves_existing` | PASS |
| 14 | 新 Idempotency-Key、同指纹 | 身份解析 | `same_key_same_fingerprint_resolves_existing` | PASS |
| 15 | 新 Idempotency-Key、异指纹 | 指纹比较 | `same_key_different_fingerprint_conflicts_without_mutation` | PASS |

### 9.3 Concurrency

| # | 场景 | 实现 | 测试 | 结果 |
|---:|---|---|---|---|
| 16 | N 并发同 key 同指纹 | 唯一约束 + 回滚重试 | `concurrent_same_key_creates_exactly_one_artifact`（8 并发） | PASS |
| 17 | 并发同 key 异指纹 | 同上 + 指纹判定 | `concurrent_same_key_different_payload_has_single_winner` | PASS |
| 18 | Artifact key 索引 23505 后新事务重读 | `try_once` 回滚重试 | 上述并发测试 + 代码审查 | PASS |
| 19 | identity 表 23505 同路径 | 同一重试分支 | 同一代码路径（无法与 #18 分离注入） | PASS（等价路径） |
| 20 | 失败事务不再使用 | `try_once` 立即 rollback 后重开 | 代码审查 + 并发测试无 500 | PASS |

### 9.4 Scope and legacy safety

| # | 场景 | 实现 | 测试 | 结果 |
|---:|---|---|---|---|
| 21 | 跨 project 同 key 独立 | 复合唯一键 | `same_key_in_different_projects_is_isolated` | PASS |
| 22 | 无权限不可探测 key | 授权先于解析 | `cross_project_token_is_denied_without_disclosure` | PASS |
| 23 | 历史 key 无身份 → 显式冲突 | `LegacyUnverifiable` | `legacy_key_without_identity_is_not_adopted` | PASS |
| 24 | 已删除 key 不被静默复用 | 身份仍解析原 Artifact | `archived_or_deleted_key_resolves_same_artifact_instead_of_reusing` | PASS |
| 25 | 身份行不可更新/删除/重绑 | append-only 触发器 + 授权 | `creation_identity_rows_are_append_only`、`roles_separation`（PL/pgSQL 42501） | PASS |
| 26 | 跨租户/项目 FK 拒绝 | 复合 FK | `identity_cannot_cross_tenant_or_project_scope` | PASS |

### 9.5 Regression

| # | 场景 | 实现 | 测试 | 结果 |
|---:|---|---|---|---|
| 27 | unkeyed 创建保持 | 未改旧路径 | `unkeyed_create_remains_legacy_compatible` | PASS |
| 28 | unkeyed 幂等保持 | 未改旧路径 | `unkeyed_idempotency_replay_keeps_legacy_behaviour`、`artifact_flow::idempotency_replay_and_reuse_conflict` | PASS |
| 29 | 既有 key 唯一索引语义不弱化 | 0001 索引保留 | `migration_invariants::artifact_key_unique_within_project_only` | PASS |
| 30 | 既有更新/归档/版本/输出流不受影响 | 未改相关路径 | 全量既有套件（`artifact_flow`、`generation_api`、`migration_invariants`） | PASS |
| 31 | 同幂等键服务不同 key 只产生一个赢家 | 预留先于创建 | `one_idempotency_key_cannot_serve_two_artifact_keys`、`concurrent_one_idempotency_key_two_artifact_keys_has_one_winner`（并发） | PASS |
| 32 | 提交前失败无任何新提交 | 单事务 | `completion_write_failure_rolls_back_artifact_identity_and_reservation` | PASS |
| 33 | 延迟触发器拒绝无身份 keyed 行 | `trg_artifact_identity_pairing` | `keyed_artifact_insert_without_identity_is_rejected_at_commit` | PASS |
| 34 | 延迟触发器拒绝 identity 与 Artifact key 不符 | `trg_creation_identity_artifact_pairing` | `identity_must_match_the_referenced_artifact_key` | PASS |
| 35 | 旧 reservation-first writer 不得与新 handler 重叠 | 部署门禁 | §4 Runbook 步骤 1–3、9 | 人工/部署 |
| 36 | 预留/Artifact/身份/完成快照同事务 | `PgKeyedArtifactCreator` | `keyed_create_persists_identity_in_same_transaction`、`completed_idempotency_replays_and_is_atomically_recorded` | PASS |
| 37 | 审计失败不影响已提交响应 | `audit_allowed` best-effort | `audit_failure_after_commit_keeps_the_create_response`（重命名审计表注入失败） | PASS |
| 38 | Body hash 字节敏感、语义指纹序列化稳定 | 两套哈希分离 | `equivalent_json_serializations_share_one_fingerprint` + 既有幂等测试 | PASS |
| 39 | APP 不能 UPDATE/DELETE 身份行 | 授权 | `roles_separation`（真实 42501） | PASS |
| 40 | APP 不能经 SQL 改 artifact_key | 不可变触发器 | `artifact_key_cannot_be_mutated_by_direct_sql`、`roles_separation` | PASS |
| 41 | NULL→非 NULL 被拒绝 | 触发器 | `artifact_key_cannot_be_mutated_by_direct_sql`（第二段） | PASS |
| 42 | 过期但未删除的幂等行不视为不存在 | 完成即重放 | `expired_completed_idempotency_still_replays` | PASS |
| 43 | 重放前必须重新授权 | 授权先于端口 | `replay_requires_fresh_authorization_in_the_authorized_scope` | PASS |
| 44 | 未引入/未声称 RLS 隔离 | 未启用 RLS | 代码与文档审查：本报告未声称 RLS | PASS |
| 45 | 重放校验 Artifact 与身份 | `verify_replay` | `completed_idempotency_replays_and_is_atomically_recorded`、`expired_completed_idempotency_still_replays` | PASS |
| 46 | 快照指向缺失/不匹配 → 失败关闭 | `IntegrityFailure` + error 日志 | `forged_completed_replay_fails_closed` | PASS |
| 47 | 过期完成行物理删除前可重放 | 清理条件 | `expired_completed_idempotency_still_replays` | PASS |
| 48 | 清理与请求竞争只有一个确定结果 | `FOR UPDATE` / `SKIP LOCKED` | `purge_skips_rows_locked_by_an_in_flight_request` | PASS |
| 49 | 清理从不删除未完成行 | `response_snapshot IS NOT NULL` | `purge_removes_only_completed_expired_rows` | PASS |
| 50 | 排空后仅在身份+raw hash 都证实时完成预留 | 端口顺序 | `stale_incomplete_reservation_is_completed_by_durable_resolution`（前提：§4 步骤 1–3） | PASS |
| 51 | 有 Artifact 无身份时不自动删除/复用 | `LegacyUnverifiable` 且不清理 | `legacy_key_without_identity_is_not_adopted` | PASS |
| 52 | 切换窗口内 ingress 一致拒绝所有创建（含 unkeyed） | 部署门禁 | §4 步骤 1 | 人工/部署 |
| 53 | 排空记录 in-flight=0 且无旧实例 | 部署门禁 | §4 步骤 2–3、9 | 人工/部署 |
| 54 | 数据库不变量拒绝残留旧二进制的 keyed 写入 | 延迟约束 | `keyed_artifact_insert_without_identity_is_rejected_at_commit` | PASS |
| 55 | 新版本在 schema 兼容与 smoke 通过前保持 gated | 部署门禁 | §4 步骤 5–9 | 人工/部署 |
| 56 | 201/200 语义 + resolution 头/字段 | handler/DTO | as1 套件全部解析 `X-Artifact-Resolution`；`contract_openapi` | PASS |
| 57 | 清理函数 SECURITY DEFINER / search_path / PUBLIC / 精确授权 / 无表 DELETE | 0004 + `db-roles.sql` | `roles_separation`（`prosecdef`、`proconfig`、PUBLIC ACL、以 maintenance 身份端到端执行清理且直接 DELETE 被拒） | PASS |

合计：**53 项自动化 PASS，4 项（35/52/53/55）为部署人工门禁**，0 项失败，0 项未执行。

## 6. 测试与 CI

本机（PostgreSQL 16.16，`cargo test --workspace`，`RUST_TEST_THREADS=4`）：

- 全部测试套件通过；AS-1 专项 38 项、fingerprint 单测 6 项、角色隔离 1 项（含 maintenance 端到端清理）、Worker 角色 2 项。
- `cargo fmt --all -- --check`：通过。
- `cargo clippy --workspace --all-targets -- -D warnings`：通过。
- 本机默认并行度下曾出现 `PoolTimedOut`（本地容器 `max_connections=100` 被大量并发测试占满），
  降低测试并行度后全部通过；CI runner 并行度低，不受影响。若未来 CI 提高并行度，
  应限制 DB 测试线程数。

CI 以 PR 上实际运行结果为准；本报告不声称 CI 已通过。

## 7. 未解决问题与风险

1. **P0/P1 代码缺陷：无已知。** 独立审查发现的 READ COMMITTED 双快照误判（identity 与 legacy
   分两条语句查询）已修复为单语句 `NOT EXISTS`，并由并发测试覆盖。剩余门禁类风险必须由
   §4 人工执行并用证据关闭（#35/#52/#53/#55）。
2. 历史 keyed Artifact 的 adoption 未实现（按任务要求排除），返回显式 `ARTIFACT_KEY_LEGACY_UNVERIFIABLE`。
   §4.1 给出运维对账步骤；未完成行不会被自动删除。
3. #19 无法独立注入 identity 表 23505（与 Artifact 唯一索引同路径），以等价路径 + 代码审查覆盖。
4. 未引入 RLS；隔离依赖复合外键与授权，符合 #44。
5. 已接受的行为差异（P2，不阻塞）：
   - `artifact_key` 为空串现在返回 400；旧实现会落库空串。契约只豁免「不发送」的客户端，
     显式空串视为非法输入。
   - 完成快照写入时不刷新 `expires_at`；被接管的过期旧预留完成后可能很快被清理，
     但 durable identity 仍会解析同一 Artifact，不产生重复。
   - 重放返回当前 Artifact 表示而非历史快照字节；ART-04E §5.2 要求不盲放旧快照。
   - `IntegrityFailure` 目前映射到既有的 `INTERNAL_ERROR`（500，`retryable` 沿用 ART-01 既有约定）；
     为实现「失败关闭」可考虑后续引入非重试内部错误码，但会改动冻结的错误码表，超出 AS-1。
   - `ResolvedExisting` 也写一条 `artifact.create` 审计（记录解析尝试，不改变业务结果）。
   - 0004 的历史清点只发 `NOTICE`，切换证据须由 §4 步骤 9 记录实际计数。

## 8. 下一阶段建议

AS-1 达到实现 + 数据库集成测试 + 57 项追踪 + 独立审查门禁后，方可建议进入 AS-2。
在独立审查完成前，不建议合并本 PR。
