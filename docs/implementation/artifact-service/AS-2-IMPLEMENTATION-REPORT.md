# AS-2 — GenerationJob Durable Caller Identity：实施报告

- 分支：`feat/as-2-durable-generation-job-caller-identity`
- 基线：`main@c11e79a` + AS-1（`5541d3b`）
- 唯一规范性依据：ART-04G v3（§1–§9 + §10 冻结修订）。ART-04I 仅作为第三轮挑战评审记录，
  其结论已并入 v3 §10，不作为并列规范。
- 范围：仅 AS-2。未涉及 AS-3（`target_version_id` 原子绑定）、AS-4（HTTP 生成入口接线）、
  PM-1/2/3、AI-PM、SeeCode/LMMS/File Service 实施，未合并 PR #8。

## 1. 变更文件清单

新增：

| 文件 | 作用 |
|---|---|
| `migrations/0005_generation_job_caller_identity.sql` | 三列 + 配对/格式 CHECK + 部分唯一索引 + 身份不可变触发器 + key 门禁 helper + 创建函数 v2 |
| `crates/artifact-application/src/service/generation_fingerprint.rs` | 指纹 v1 canonicalizer（显式 code point 排序）与 golden/边界单测 |
| `crates/artifact-application/src/port/keyed_job_create.rs` | GenerationJob keyed-create 端口、validated DTO、outcome/error 分层 |
| `crates/artifact-infrastructure/src/postgres/keyed_job_create.rs` | 单事务 PostgreSQL 实现、token-first 分类、有界重试 |
| `tests/tests/as2_caller_identity.rs` | 34 项真实 PostgreSQL 16 集成/并发/故障注入测试 |

修改：

`migrations.sha256`、`deploy/db-roles.sql`、`crates/artifact-domain/src/{job.rs,error.rs}`、
`crates/artifact-contract/src/{dto.rs,error.rs}`、`crates/artifact-application/src/{lib.rs,port/mod.rs,port/repo.rs,service/mod.rs,service/artifact.rs}`、
`crates/artifact-infrastructure/src/postgres/mod.rs`、`crates/artifact-client/src/lib.rs`、
`apps/artifact-api/src/routes/{mod.rs,artifacts.rs}`、`openapi/openapi.yaml`、`tests/Cargo.toml`、
`tests/tests/{common.rs,generation_api.rs,contract_openapi.rs,sdk_contract.rs,migration_guard.rs,roles_separation.rs,worker_coordination.rs,worker_roles.rs}`。

## 2. Migration 0005（只做加法）

- `generation_job` 新增 `caller_request_id uuid`、`request_fingerprint char(64)`、`fingerprint_version smallint`。
- `generation_job_caller_identity_pairing_check`：三列同空或同非空；`fingerprint_version > 0`；
  `request_fingerprint ~ '^[0-9a-f]{64}$'`。
- `uq_generation_job_caller_request`：`(tenant_id, project_id, caller_request_id) WHERE caller_request_id IS NOT NULL`。
- `guard_job_transition`（`CREATE OR REPLACE`，签名不变）：三列任何变化（含 `NULL → 非 NULL`）`P0001` 拒绝。
- `artifact.assert_generation_job_key_gate(uuid, uuid, text, text, uuid)`：内部 key gate，读前查与异常块共用；
  invoker 权限、固定 `search_path`、`REVOKE PUBLIC`、不授予任何运行角色。
- `create_generation_job_with_input` 升级为 16 参、`RETURNS TABLE(job_id uuid, resolution text)`；
  13 参旧签名 `DROP`。返回 resolution ∈ `{created, caller_request_replay, idempotency_replay}`。
- 历史 Job 三列保持 `NULL`（`RAISE NOTICE` 清点，不回填、不伪造）。
- 0001–0004 未改动；`migrations.sha256` 已登记 0005。

迁移切换前置证据（§9.7.1 / §10.7.1）：

1. 生产写入路径盘点：`crates/**`、`apps/**` 内对 `create_generation_job_with_input` 的调用数为 **0**
   （仅 `tests/` 调用），即当前不存在生产调用方通过该函数写入 caller identity。
2. 迁移前 `pg_stat_activity` 无 in-flight 创建事务 —— **部署人工门禁（AS2-46），见 §7**。
3. 迁移前后 `SELECT count(*) FROM artifact.generation_job` 一致、`caller_request_id IS NOT NULL` 恒为 0
   —— 部署人工门禁（AS2-46），见 §7。

## 3. 创建事务（冻结顺序，§5.3.1 / §10.1）

单条受控函数调用即一个完整事务（`SECURITY DEFINER`、`SET search_path = pg_catalog, artifact`），
顺序：

1. `v_key := nullif(btrim(p_idempotency_key), '')` 与长度 ≤ 255 校验**最先**执行
   （C-6：早于一切 caller lookup / key lookup / replay 返回 / 异常块处理；超限 `22023 INVALID_IDEMPOTENCY_KEY`）。
2. 既有全量输入校验原样保留 + caller 三元配对/格式校验。
3. caller identity 解析：命中 → version 不等 `P0001 CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH`
   → fingerprint 不等 `23505 CALLER_REQUEST_ID_REUSED` → **key holder 硬门禁**（`v_key`）→ 通过返回 `caller_request_replay`（零写入）。
4. caller 未命中 → 既有 Idempotency-Key 解析（同 key 同 hash → `idempotency_replay`；异 hash → `23505`）。
5. generator registry 检查（`UNKNOWN_GENERATOR` 22023 / `GENERATOR_UNAVAILABLE` P0001）——
   **位于一切可判定 replay 之后**，因此 generator 临时不可用不会阻断已有身份的恢复。
6. `input_sha256`（原逻辑）→ 单条 `INSERT`（含 caller 三列）。
7. `EXCEPTION WHEN unique_violation`：子事务自动回滚 → 新语句快照重读 caller（version→fingerprint→
   **同一 key gate helper**）→ 未命中则按 `v_key` 重读 → 均未命中原样 `RAISE`；
   防御条款：key 命中且 hash 相同但胜者 caller 与传入不同 → `P0001 IDEMPOTENCY_KEY_CROSS_IDENTITY`（fail-closed）。
8. 成功返回 `(p_job_id, 'created')`。

并发语义：READ COMMITTED + 唯一索引仲裁；不使用 advisory lock / reservation-first / SERIALIZABLE；
gate 之后的并发绑定属语句快照时点语义下的合法串行序（§10.2 P3-1），不在代码或测试中额外加强串行化。

## 4. 指纹 v1

`request_fingerprint = lowercase_hex(SHA-256(UTF-8(canonical_json)))`，canonical JSON：紧凑、
对象键**显式递归按 Unicode code point 排序**、数组保序、payload 字符串字节精确（不做 NFC）、
generator_id NFC、DTO 层 `options` absent ≡ null、payload 内 absent ≠ null、数字按 serde_json 规范形
（`1e2` ≡ `100.0`，整数 `100` ≠ 浮点 `100.0`）、非有限数不可表示。

指纹文档字段（被 golden 测试钉死，任何新增传输层字段都不可进入）：
`artifact_id, caller_request_id, domain, generator_id, input, options, project_id, tenant_id`。

三个哈希语义严格区分：`request_fingerprint`（caller 业务操作身份）、`request_hash`（HTTP 原始体哈希，
key 冲突判定）、`input_sha256`（持久化输入快照哈希）。v1 frozen golden vector：

```
64e2d6651752208dd15c40d8010488f6818a43770c8b71af58f493adf6e126f3
```

该值已用独立实现（Python `hashlib` + `json.dumps(sort_keys=True, separators=(",",":"))`）复核一致。

## 5. 错误分层（DB token 不决定 HTTP status）

| DB 稳定 token | SQLSTATE | 端口类型 | DomainError | HTTP | 可重试 |
|---|---|---:|---|---:|---|
| `CALLER_REQUEST_ID_REUSED` | 23505 | `CallerRequestIdReused` | 同名 | 409 | 否 |
| `IDEMPOTENCY_KEY_REUSED` | 23505 | `IdempotencyConflict` | `IdempotencyKeyReused` | 409 | 否 |
| `CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH` | P0001 | `FingerprintVersionMismatch` | 同名 | 500 | **否（失败关闭）** |
| `IDEMPOTENCY_KEY_CROSS_IDENTITY` | P0001 | `IdempotencyKeyCrossIdentity` | 同名（独立语义） | 500 | 否 |
| `INVALID_IDEMPOTENCY_KEY` | 22023 | `InvalidIdempotencyKey` | 同名 | 400 | 否 |
| `UNKNOWN_GENERATOR` | 22023 | `UnknownGenerator` | `InvalidRequest` | 400 | 否 |
| `GENERATOR_UNAVAILABLE` | P0001 | `GeneratorUnavailable` | `GeneratorUnavailable` | 503 | 是 |
| 其余校验 token | 22023/22001 | `InvalidRequest` | `InvalidRequest` | 400/413 | 否 |
| 未命中 token 的 23505/40001/40P01 | — | `RetryablePersistenceFailure`（≤3 次 + 退避） | `PersistenceUnavailable` | 503 | 是 |
| 其他未知 DB 错误 | — | `IntegrityFailure` | `Internal` | 500 | 否 |

实现为 token-first（先匹配固定消息字面量，再按 SQLSTATE），见
`crates/artifact-infrastructure/src/postgres/keyed_job_create.rs::classify_db_error`。

## 6. 数据库安全（真实收紧，而非仅 Rust 层不提供入口）

- `deploy/db-roles.sql`：对 APP 显式 `REVOKE INSERT, UPDATE ON artifact.generation_job`（保留 `SELECT`）。
  Job 行与 caller identity 只能经 `SECURITY DEFINER` 创建函数写入；触发器负责身份不可变。
- 创建函数 `SECURITY DEFINER` + 固定 `search_path`；`REVOKE PUBLIC`；EXECUTE 仅授予 `__APP_ROLE__`
  （16 参新签名）。
- key gate helper 不授予 APP / WORKER / MAINTENANCE / PUBLIC。
- WORKER 仍只有 7 个协调函数；MAINTENANCE 只有 `purge_expired_idempotency_records(integer)`；
  PUBLIC 无任何新函数 EXECUTE。
- **受信任调用边界（显式声明，不虚构 DB 已完成授权）**：数据库函数无法在内部验证 HTTP 授权。
  `p_tenant_id` / `p_project_id` 必须由 Application 层从已授权上下文解析
  （delegation token → tenant；`X-Project-Id` → project）；数据库只负责 scope 内的配对、唯一性、
  不可变性与事务收敛。身份内容正确性的唯一边界是 Rust typed DTO canonicalizer。
- 未使用 RLS，也不声称依赖 RLS 实现 tenant isolation：隔离由 scope 谓词 + 唯一索引 + 授权模型共同保证。

实际权限断言（`tests/tests/roles_separation.rs`，SQL 级 `has_table_privilege` / `has_function_privilege`
与真实连接尝试）：

- APP：`SELECT` on `generation_job` = true；`INSERT` / `UPDATE` = false；直接 INSERT 与直接 UPDATE 身份列
  均返回 `42501`。
- APP：16 参创建函数 EXECUTE = true；`assert_generation_job_key_gate` = false。
- WORKER / MAINTENANCE / PUBLIC：gate helper 与创建函数均不可执行；WORKER/MAINTENANCE 无表 DML。

## 7. 54 项验收矩阵

图例：PASS = 自动化测试已执行通过；BLOCKED = 部署时人工门禁（不可在开发库证明）。

| 项 | 测试位置 | 结果 |
|---|---|---|
| AS2-01 | `generation_fingerprint::tests::v1_golden_vector_is_frozen` | PASS |
| AS2-02 | `each_semantic_field_changes_the_fingerprint` | PASS |
| AS2-03 | `key_order_and_options_absence_are_equivalent` | PASS |
| AS2-04 | `payload_strings_keep_bytes_but_generator_id_is_nfc` | PASS |
| AS2-05 | `transport_credentials_do_not_change_the_fingerprint`（键集合钉死） | PASS |
| AS2-06 | `number_normalization_rules` + `non_finite_numbers_are_unrepresentable` | PASS |
| AS2-07 | `unknown_fingerprint_version_fails_closed` | PASS |
| AS2-08 | `as2_08_validation_failure_leaves_no_residue` | PASS |
| AS2-09 | `as2_09_replay_after_commit_returns_same_job` | PASS |
| AS2-10 | 三阶段：`as2_10a_termination_before_insert_leaves_nothing` / `as2_10b_termination_before_commit_rolls_back` / `as2_10c_termination_after_commit_is_durable_and_replayable`（真实 `pg_terminate_backend` + `pg_stat_activity` 退出确认，见 §8） | PASS |
| AS2-11 | `as2_11_key_bound_to_other_job_rejects_without_identity` | PASS |
| AS2-12 | `as2_12_generator_unavailable_leaves_no_residue` | PASS |
| AS2-13 | `as2_13_replay_survives_generator_disabled` | PASS |
| AS2-14 | `as2_14_concurrent_same_caller_yields_single_job` | PASS |
| AS2-15 | `as2_15_concurrent_same_caller_different_fingerprint` | PASS |
| AS2-16 | `as2_16_concurrent_same_key_different_caller` | PASS |
| AS2-17 | `as2_17_dual_index_race_resolves_without_500` | PASS |
| AS2-18 | 代码审查：异常块后不重用失败子事务；AS2-17/50 佐证 | PASS（审查+佐证） |
| AS2-19 | `as2_19_transient_failure_retried_then_503`（触发器注入 40001，sequence 计 3 次） | PASS |
| AS2-20 | `as2_20_same_caller_isolated_across_scopes` | PASS |
| AS2-21 | `as2_21_lookup_requires_project_authorization` | PASS |
| AS2-22 | `as2_22_lookup_does_not_leak_across_scopes` | PASS |
| AS2-23 | `as2_23_lookup_rejects_invalid_uuid`（含不回显原始输入断言，见 §12） | PASS |
| AS2-24 | `as2_24_lookup_sees_only_committed_jobs` | PASS |
| AS2-25 | `as2_25_unkeyed_semantics_preserved` | PASS |
| AS2-26 | `as2_26_legacy_job_lookup_returns_none` | PASS |
| AS2-27 | `as2_27_replay_with_new_key_does_not_rewrite` | PASS |
| AS2-28a | `as2_28a_unkeyed_key_reuse_with_different_hash` | PASS |
| AS2-28b | `as2_28b_caller_hit_is_not_allowed_to_bypass_key_conflict` | PASS |
| AS2-29 | `as2_44_migration_0005_structural_invariants`（0003 唯一索引保留） | PASS |
| AS2-30 | `as2_30_replay_after_input_purge` | PASS |
| AS2-31 | 全量回归：`as1_creation_identity` 38 项 + 既有套件全部通过 | PASS |
| AS2-32 | `as2_32_terminal_job_identity_preserved` | PASS |
| AS2-33 | `as2_33_identity_columns_are_immutable` | PASS |
| AS2-34 | `as2_34_null_to_non_null_identity_is_rejected` | PASS |
| AS2-35 | `as2_35_partial_unique_index_blocks_duplicate_identity` | PASS |
| AS2-36 | `as2_36_lookup_serves_the_job_with_caller_identity` | PASS |
| AS2-37 | `as2_37_openapi_mirrors_caller_identity_contract` | PASS |
| AS2-38 | `as2_38_new_error_codes_are_mapped` | PASS |
| AS2-39 | `as2_39_sdk_caller_request_lookup` | PASS |
| AS2-40 | `as2_40_sdk_types_require_caller_identity` | PASS |
| AS2-41 | `as2_41_http_create_stays_fail_closed` | PASS |
| AS2-42 | `roles_separation::runtime_role_is_dml_only`（函数路径成功 + 直连 INSERT/UPDATE 42501） | PASS |
| AS2-43 | `runtime_role_is_dml_only` + `worker_roles::worker_role_cannot_touch_business_tables` | PASS |
| AS2-44 | `as2_44_migration_0005_structural_invariants` | PASS |
| AS2-45 | `as2_45_job_creation_never_writes_idempotency_record` | PASS |
| AS2-46 | 部署证据：迁移前后行数一致 / 无身份回填 / 无 in-flight 创建事务 | **BLOCKED（人工门禁）** |
| AS2-47 | `as2_47_48_cross_key_requests_are_rejected` | PASS |
| AS2-48 | `as2_47_48_cross_key_requests_are_rejected` | PASS |
| AS2-49 | `as2_49_precommitted_cross_requests_both_conflict`（先全部提交再并发） | PASS |
| AS2-50 | `as2_50_exception_block_reapplies_key_gate`（真实 caller 唯一索引竞争，见 §8） | PASS |
| AS2-51 | `as2_51_generator_unavailable_is_a_business_error` | PASS |
| AS2-52 | `as2_52_padded_key_hits_normalized_binding`（`' K2 '` → 409） | PASS |
| AS2-52a | `as2_52a_blank_keys_equal_none_and_htab_is_kept` | PASS |
| AS2-52b | `as2_52b_overlong_key_rejected_on_both_paths` | PASS |
| AS2-53 | `as2_53_benign_interleaving_is_allowed`（R1 gate 先于 R2 commit） | PASS |

AS2-46 人工验收步骤（部署执行方）：

```sql
-- 迁移前（维护窗口内、API/Worker 已停止写入）
SELECT count(*) FROM artifact.generation_job;
SELECT count(*) FROM pg_stat_activity
 WHERE state <> 'idle' AND query ILIKE '%create_generation_job_with_input%';
-- 迁移后
SELECT count(*) FROM artifact.generation_job;                                  -- 必须与迁移前相同
SELECT count(*) FROM artifact.generation_job WHERE caller_request_id IS NOT NULL; -- 必须为 0
```

## 8. 并发测试屏障（不使用 sleep）

- AS2-49：两个交叉请求涉及的 Job A/B 与 key 绑定全部先提交，再并发发起
  `{caller A, F_A, K2}` 与 `{caller B, F_B, K1}`；两侧 caller 命中后 key gate 必见已提交的外来绑定，
  因此**确定**双双 409（不依赖随机调度）。
- AS2-50（R1 修复后）：真实 `uq_generation_job_caller_request` 唯一索引竞争，无测试辅助表：
  1. T3 开启事务创建 caller A + K3 并保持未提交 —— caller A 唯一索引条目的未提交持有者；
  2. T1 请求 caller A + K50：读前查未命中（T3 未提交）→ INSERT 真实阻塞在 caller 唯一索引上，
     等待 T3 的事务（`wait_event = transactionid`）。阻塞位置以三项证据证明：
     `pg_blocking_pids(T1) == [T3]`、`wait_event_type='Lock'`、活跃语句即本创建调用
     （有界 `yield_now` 轮询 + 10s 上限，无 sleep）；
  3. T2（caller B + K50，不同 raw hash）在 T1 仍阻塞时提交（§10.3 时序前提）；
  4. T3 提交 → T1 确认 `unique_violation` 进入异常块 → 新语句快照重读 caller A（指纹一致）→
     key gate 必见 T2 已提交的绑定 → 409 `IDEMPOTENCY_KEY_REUSED`；终态断言：恰好 2 个 Job，
     caller A/caller B/K50 各只绑定 T3/T2/T2 的行，T1 无残留、无改写。
  - 索引处理顺序说明：PostgreSQL 按索引 OID 升序执行插入期唯一性检查；测试以同名同定义
    重建 `uq_generation_job_idempotency_key`（仅 OID 变化，唯一性语义与谓词逐字不变），
    使 caller 索引先于 key 索引被检查，避免 T1 的 speculative K50 条目反向阻塞 T2、
    破坏 §10.3 的提交顺序前提。
- AS2-10 三阶段（R1 修复后，分别实现、分别断言）：
  - 阶段 A：未提交的 `SHARE` 表锁作为 INSERT 前注入点（函数的 SELECT 仅需 ACCESS SHARE，
    不冲突；INSERT 的 ROW EXCLUSIVE 与 SHARE 冲突）→ `pg_terminate_backend` → 轮询
    `pg_stat_activity` 确认 backend 真正退出 → 断言无 Job / 无身份 / 无 key 绑定；
  - 阶段 B：显式 BEGIN + 函数返回 `'created'` 证明 INSERT 已执行 → 独立管理连接终止 →
    等待 backend 退出（不凭 `pg_terminate_backend` 返回值或客户端断开推断回滚）→
    断言数据库无持久化残留，重试收敛一个 Job；
  - 阶段 C：COMMIT 后终止连接（响应丢失）→ 断言 Job/身份/key 完整持久化，
    同 caller + 同指纹重放返回同一 Job、不创建重复。
- AS2-53：R2 创建事务先开启并 INSERT（未提交）→ R1 重放完整返回（gate 早于 R2 提交）→ R2 提交。
  断言 R1=200 `caller_request_replay` 且解析回 A，R2=created B，K2 最终只绑定 B，A 的身份/key/hash 未被改写。
- AS2-19：用 `BEFORE INSERT` 触发器注入 `40001`，以 sequence（非事务性）计数，验证端口恰好重试 3 次后
  返回 `RetryablePersistenceFailure`（503），不误报业务冲突。

## 9. API / SDK / OpenAPI

- `POST /v1/artifacts/{artifact_id}/generation-jobs`：**保持 fail-closed 503**（AS-4 门禁），
  不因 AS-2 开放真实生成；请求 DTO 必填 `caller_request_id`。
- 新增 `GET /v1/generation-jobs/by-caller-request/{caller_request_id}`：与 GET Job 同链路
  （delegation tenant + `X-Project-Id` + `Action::GenerationRead`）；跨 scope 与不存在统一 404；
  非法 UUID → 400；只读；路由注册在 `/v1/generation-jobs/{job_id}` 之前。
- `GenerationJobResponse.caller_request_id: Option<Uuid>`（legacy 为 null）。
- OpenAPI：create 请求 `required` 增加 `caller_request_id`；GenerationJob 增加可空字段；
  POST 增加 200 重放响应与 `X-Generation-Job-Resolution` 头（header 组件三值）；
  409 说明并入 `CALLER_REQUEST_ID_REUSED`；500 说明两个完整性错误码；Idempotency-Key 规范化语义写入参数描述；
  `Error.code` 枚举补齐服务端实际产出的全部稳定错误码（4 个 AS-2 新码 + AS-1 既有产出码），
  由 `contract_openapi::error_code_enum_covers_every_domain_error` 固化覆盖关系。
- SDK：新增 `ArtifactClient::get_generation_job_by_caller_request`。按 AS-4 边界，未新增 SDK 的
  get/cancel job 方法。

## 10. 边界声明

| 问题 | 回答 |
|---|---|
| 是否开放 GenerationJob 真实生成？ | **否** |
| 是否启用 Generator / Worker？ | **否** |
| 是否实施 AS-3（`target_version_id` 原子绑定）？ | **否** |
| 是否修改 AI-PM？ | **否** |
| 是否合并 PR #8？ | **否** |
| 是否合并 AS-2 PR？ | **否（保持 Draft）** |

## 11. 已知风险与未关闭事项

- AS2-46 依赖部署期人工证据（维护窗口、行数比对、in-flight 检查），开发库无法替代。
- `request_hash` 由传输层提供，本工作包未接线 HTTP create（AS-4 门禁），因此端口的 `request_hash`
  目前只有测试使用；AS-4 接线时必须取自真实请求体哈希。
- 指纹 v1 一经发布不得原地修改；未来语义变化必须新增 `fingerprint_version` 并保留 v1 解析。

## 12. R1 独立审查修复记录（2026-11-25）

针对独立审查提出的五项问题，本分支追加以下修复（不改变 §1–§11 的契约语义）：

| 问题 | 修复 |
|---|---|
| P1-01 AS2-50 未证明真实唯一索引竞争 | 重设计为三独立连接（T1 竞争者 / T2 key holder / T3 未提交 caller 索引持有者）的真实 `uq_generation_job_caller_request` 竞争；删除 `as2_barrier` 辅助表与触发器；以 `pg_blocking_pids`、`wait_event` 与活跃查询证明阻塞位置；T2 在 T1 异常块 gate 之前提交（§10.3）；结果确定 409 `IDEMPOTENCY_KEY_REUSED` |
| P1-02 AS2-10 只覆盖单阶段 | 拆分为三阶段独立测试（INSERT 前 / INSERT 后 COMMIT 前 / COMMIT 后），全部以 `pg_stat_activity` 确认 backend 真正退出后再断言数据库持久化状态；阶段 B 由原测试演化而来，断言更强（INSERT 已执行证据 + key 绑定断言） |
| P1-03 OpenAPI 错误码枚举缺失 | `Error.code` 枚举补齐 `CALLER_REQUEST_ID_REUSED`、`INVALID_IDEMPOTENCY_KEY`、`CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH`、`IDEMPOTENCY_KEY_CROSS_IDENTITY` 及 AS-1 已产出码（`ARTIFACT_KEY_CONFLICT`、`ARTIFACT_KEY_LEGACY_UNVERIFIABLE`、`PERSISTENCE_UNAVAILABLE`）；新增 parity 测试 `error_code_enum_covers_every_domain_error` 强制枚举覆盖全部 `DomainError::code()`；真实 HTTP 证据由 `as2_41`（503 GENERATOR_UNAVAILABLE 信封）与 `as2_23`（400 信封）覆盖；HTTP status 映射（contract `http_status()`）此前已正确，未改动 |
| P2-01 非法 UUID 回显原始输入 | `parse_path_uuid` 错误消息不再包含原始路径参数（保持 400 + `INVALID_REQUEST` 稳定码，合法 UUID 处理不变）；`as2_23` 增加哨兵值断言响应（message + details）不含输入 |
| P2-02 `.trae` 本地计划文件入库 | 确认为 TRAE 本地执行计划（正式记录已由本报告与 ART-04G v3 覆盖）：从 PR 变更中移除（本地文件保留），`.gitignore` 增加 `.trae/`；无正式内容需迁移 |

受本次修复影响的既有测试：`as2_10_*`（拆分为三阶段）、`as2_23`（增强断言）、
`as2_50`（重写）、`as2_37`/`as2_38`（保持不变，新增独立枚举 parity 测试）、
`as2_17`（R1 回归期间在 CI 上暴露的潜在 flake：原断言钉死 `tokio::join!` 的轮询顺序，
即假设第一方必为 created；改为顺序无关断言——恰好一方 created、另一方按 caller 解析，
与矩阵 AS2-17 语义一致，连续 8 次本地复跑通过）。
其余 AS-2 与 AS-1 测试不受影响，全量回归通过。
