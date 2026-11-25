# AS-2 实施计划：GenerationJob Durable Caller Identity

## Context

在 `feat/as-2-durable-generation-job-caller-identity` 分支（基于 main c11e79a + AS-1 5541d3b，已 checkout、工作树干净）按冻结契约 ART-04G v2（§1-§9）+ ART-04I（=v3 §10：C-6 前移、P3-1..4、AS2-52/53、54 项矩阵）实现 AS-2，最终交付 Draft PR（目标 main）+ A-H 格式报告。

三条用户硬性风险必须落实并验证：
1. caller replay 不得绕过 Idempotency-Key 冲突检查（§9.1 key holder 硬门禁，读前查与异常块共用同一 helper）。
2. 数据库权限真实收紧：撤销 APP 对 `artifact.generation_job` 的表级 INSERT/UPDATE，仅保留受控 SECURITY DEFINER 函数路径。
3. 并发测试必须真实 PostgreSQL 16（sqlx::test，禁 Mock/sleep；AS2-49/50 用事务阻塞/提交顺序做确定性屏障）。

## 盘点结论（实现差异清单）

| 现状 | 差距 |
|---|---|
| `create_generation_job_with_input`（0003 L271-389，13 参，returns (uuid,bool)） | 需 DROP+CREATE v2：16 参（+caller 三元，插在 p_idempotency_key 后）、returns (uuid, resolution text)、caller 解析+key 硬门禁、generator 检查后移到 replay gates 之后、key 规范化+长度校验前移 |
| `guard_job_transition`（0001 L350-370） | 追加三列任何变化（含 NULL→非 NULL）即拒绝 |
| `deploy/db-roles.sql` | APP 对 generation_job 有表级 INSERT/UPDATE（GRANT ON ALL TABLES）→ 必须显式 REVOKE；create 函数 EXECUTE 改绑 16 参新签名 |
| Domain `GenerationJob`（job.rs） | 无 caller 三字段 |
| DTO/`to_job_dto`/`JobRow`/`JOB_COLUMNS` | 无 caller_request_id |
| `JobRepository::get` | 缺 `get_by_caller_request` |
| OpenAPI/SDK | 无 lookup 路由、无 caller 字段、无新错误码 |
| 测试 | 无 AS-2 测试；common.rs `try_sql_create_job_with` 需适配 16 参签名 |

## Phase 1：Migration 0005 + db-roles

**新文件 `migrations/0005_generation_job_caller_identity.sql`**（additive，不动 0001-0004）：
1. `ALTER TABLE artifact.generation_job ADD COLUMN caller_request_id uuid / request_fingerprint char(64) / fingerprint_version smallint`。
2. CHECK `generation_job_caller_identity_pairing_check`：三列同空或同非空、version>0、fingerprint `~ '^[0-9a-f]{64}$'`。
3. `CREATE UNIQUE INDEX uq_generation_job_caller_request ON (tenant_id, project_id, caller_request_id) WHERE caller_request_id IS NOT NULL`。
4. `CREATE OR REPLACE FUNCTION artifact.guard_job_transition`：保留现有 ownership/状态机校验，追加三列任何变化 → P0001 拒绝。
5. 私有 key-gate helper `artifact.assert_generation_job_key_gate(p_tenant, p_project, v_key, p_request_hash, p_existing_job_id)`：v_key NULL 直接返回；按 key 查 holder；holder≠existing 且 hash 不同 → 23505 `IDEMPOTENCY_KEY_REUSED`；hash 相同 → P0001 `IDEMPOTENCY_KEY_CROSS_IDENTITY`。读前查与异常块共用（满足"同一 SQL helper"）。
6. DROP 旧 13 参 create 函数；CREATE v2（16 参，`RETURNS TABLE(job_id uuid, resolution text)`），SECURITY DEFINER、`SET search_path = pg_catalog, artifact`，函数体冻结顺序：
   - ① 全量输入校验（现函数保留）+ caller 三元配对/格式校验 + `v_key := nullif(btrim(p_idempotency_key),'')` 与长度≤255 校验**前移至此**（超限 22023 `INVALID_IDEMPOTENCY_KEY`）；
   - ② caller 读前查（p_caller_request_id 非 NULL）：version 不等 → P0001 `CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH`；fingerprint 不等 → 23505 `CALLER_REQUEST_ID_REUSED`；然后 `assert_generation_job_key_gate(..., v_existing.id)`；通过 → `RETURN (id,'caller_request_replay')`（零写入，不重评 generator）；
   - ③ caller 未命中 → 现 key 解析逻辑（同 v_key 同 hash → `idempotency_replay`；异 hash → 23505）；
   - ④ generator registry 检查（UNKNOWN_GENERATOR 22023 / GENERATOR_UNAVAILABLE P0001）——**在 replay gates 之后**；
   - ⑤ input_sha256 原逻辑；
   - ⑥ 单条 INSERT（含 caller 三列）；
   - ⑦ EXCEPTION unique_violation（子事务回滚）：新语句快照 caller 重读 → 命中则 version→fingerprint→key gate（同一 helper）→ replay；未命中则按 v_key 重读（原逻辑）；均未命中原样 RAISE；防御条款：key 命中 hash 相同但胜者 caller≠传入 → P0001 fail-closed；
   - ⑧ `RETURN (p_job_id,'created')`。
   - 注释声明 P3-1 快照时点语义（gate 后并发绑定=合法串行序，禁止加锁"修复"）。
7. 三个新函数 `REVOKE ALL FROM PUBLIC`；历史清点 `RAISE NOTICE`（不回填）。

**修改 `deploy/db-roles.sql`**：
- `REVOKE INSERT, UPDATE ON artifact.generation_job FROM __APP_ROLE__`（保留 SELECT；身份列写入只能经 SECURITY DEFINER 函数）。
- APP 的 create EXECUTE 改绑 16 参新签名；key-gate helper 不授予任何运行角色。
- WORKER/MAINTENANCE 不变；默认权限同步（migrator 后建表不再给 APP INSERT 全量？——不动全局默认，仅对该表显式 REVOKE，与 audit_event 先例一致）。

**修改 `migrations.sha256`**：追加 0005 行。

## Phase 2：Fingerprint canonicalizer（Rust）

**新文件 `crates/artifact-application/src/service/generation_fingerprint.rs`**：
- `CURRENT_JOB_FINGERPRINT_VERSION: i16 = 1`；`canonical_job_fingerprint(version, &ValidatedCreateGenerationJobRequest)`。
- 公式：`lowercase_hex(SHA-256(UTF-8(canonical_json)))`；文档字段 domain="generation-job-create:v1"、tenant_id、project_id、artifact_id、caller_request_id、generator_id（NFC）、input、options（DTO 层 absent≡null）。
- canonical JSON：**显式递归按键（Unicode code point）重排**（P3-3，不依赖 BTreeMap 默认）、紧凑、数组保序、payload 字符串字节精确（不做 NFC）、整数按整数/浮点最短往返、拒绝非有限数。
- 单元测试：AS2-01 golden vector 冻结、AS2-02 逐字段突变、AS2-03 键序/空白/absent≡null 等价、AS2-04 Unicode 字节不同→不同指纹、AS2-05 排除字段突变不变、AS2-06 数字规则（1e2≡100.0、100≠100.0）、AS2-07 未知版本 fail-closed。

## Phase 3：Domain + Port + Infrastructure

**修改 `crates/artifact-domain/src/job.rs`**：GenerationJob 增加 `caller_request_id/request_fingerprint/fingerprint_version: Option<...>`。

**修改 `crates/artifact-domain/src/error.rs`**：新增变体 `CallerRequestIdReused`（→409 CALLER_REQUEST_ID_REUSED）、`FingerprintVersionMismatch`（→500 CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH，不可重试）、`IdempotencyKeyCrossIdentity`（→500 IDEMPOTENCY_KEY_CROSS_IDENTITY）、`InvalidIdempotencyKey`（→400 INVALID_IDEMPOTENCY_KEY）。

**新文件 `crates/artifact-application/src/port/keyed_job_create.rs`**：
- `ValidatedCreateGenerationJobRequest`（校验+generator_id NFC；caller_request_id、指纹在此计算）；
- `KeyedJobCreateOutcome { Created, ResolvedByCallerRequest, ResolvedByIdempotency }`；
- `KeyedJobCreateError`（CallerRequestIdReused/FingerprintVersionMismatch/IdempotencyKeyCrossIdentity/IdempotencyConflict/UnknownGenerator/GeneratorUnavailable/InvalidRequest/InvalidIdempotencyKey/RetryablePersistenceFailure/IntegrityFailure）+ `to_domain_error()`；
- `trait KeyedGenerationJobCreatePort::create_or_resolve_keyed_job`（一次调用=一个完整事务，不泄露 sqlx::Transaction）。

**新文件 `crates/artifact-infrastructure/src/postgres/keyed_job_create.rs`**：`PgKeyedGenerationJobCreator`，**token-first 分类**（§9.5 冻结映射）：先匹配稳定 message token，再 SQLSTATE；仅未命中 token 的 23505/40001/40P01 有界重试（≤3+退避）。

**修改 `crates/artifact-infrastructure/src/postgres/mod.rs`**：JOB_COLUMNS/JobRow 加 caller_request_id；`PgJobRepository::get_by_caller_request`（谓词 tenant+project+caller，走部分唯一索引；未命中 NotFound）。

**修改 `crates/artifact-application/src/port/repo.rs`**：JobRepository trait 加方法；`to_job_dto` 补 caller_request_id；`state.rs` 装配新端口。

## Phase 4：HTTP / OpenAPI / SDK

**修改 `crates/artifact-contract/src/dto.rs`**：`CreateGenerationJobRequest.caller_request_id: Uuid`（必填）；`GenerationJobResponse.caller_request_id: Option<Uuid>`。
**修改 `crates/artifact-contract/src/error.rs`**：http_status 增 4 个新码。
**修改 `apps/artifact-api/src/routes/mod.rs`**：在 `/v1/generation-jobs/{job_id}` **之前**注册 `GET /v1/generation-jobs/by-caller-request/{caller_request_id}`。
**修改 `apps/artifact-api/src/routes/artifacts.rs`**：`get_job_by_caller_request`（Scoped + Action::GenerationRead；非法 UUID→400 稳定原因码；不回显输入）；create 路由保持 fail-closed 503 不接线。
**修改 `openapi/openapi.yaml`**：create 请求 required 加 caller_request_id；GenerationJob 加可空 caller_request_id；新增 lookup 路由（200/400/401/403/404）；POST 加 200 replay + `X-Generation-Job-Resolution` 头文档；409 描述加 CALLER_REQUEST_ID_REUSED；Job 路径 key 生命周期文案（P2-5）。
**修改 `crates/artifact-client/src/lib.rs`**：新增 `get_generation_job_by_caller_request`（不加 get/cancel job —— AS-4 边界）。

## Phase 5：权限收紧测试

**修改 `tests/tests/roles_separation.rs`**：AS2-42（经函数创建成功 + APP 直连 INSERT generation_job → 42501 + APP 直连 UPDATE caller 三列 → 42501）、AS2-43（新函数 EXECUTE 仅 APP；PUBLIC/WORKER/MAINTENANCE 无）、helper 函数无 EXECUTE。
**修改 `tests/tests/migration_guard.rs`**：AS2-44（三列/CHECK/部分唯一索引/触发器存在性 + 语义抽查）；`released_migrations_are_unchanged` 自动覆盖 0005 清单。
**检查 `tests/tests/worker_roles.rs`** 等是否有 APP 角色直写 generation_job（盘点结论：直写均在 admin pool，预期无需改动；实施时核实）。

## Phase 6：54 项验收矩阵

**新文件 `tests/tests/as2_caller_identity.rs`**（DB 函数/端口级，真实 PG16 sqlx::test）：
- A 组 AS2-01..07（UNIT，在 fingerprint 模块内）。
- B 组 AS2-08..13：校验失败无残留、提交后重放、**AS2-10 用 pg_terminate_backend 真实 kill 事务**、key 绑他行、generator unavailable 后恢复创建、generator 禁用后 replay 仍可用。
- C 组 AS2-14..19 + 49/50/53 并发：tokio 并发 join_all 同 pool；**确定性屏障不用 sleep**——AS2-50 用三连接事务顺序（T2 commit → T3 未提交占位 → T1 INSERT 阻塞在 caller 索引 → T3 commit → T1 异常块 gate 必见 T2）实现，测试注释列明 T1/T2/T3 操作顺序/阻塞点/提交点/断言，并以 `pg_blocking_pids()` 断言 T1 阻塞于 T3（可观测锁等待证明）；**AS2-53 按 v3 §10.6 屏障重设计**：连接 2 开事务 create（INSERT 已执行未提交）→ 连接 1 完整返回 replay（gate 在 R2 commit 前执行）→ 连接 2 提交；断言 R1=200 replay A、R2=202 created、K2 只绑定 B、无改写；AS2-19 用临时 RAISE(40001/40P01) 触发器做真实故障注入，验证有界重试后 503 不误报冲突；**AS2-51 显式**：GENERATOR_UNAVAILABLE → 503 GeneratorUnavailable，不落入 IntegrityFailure。
- **AS2-10 三阶段**：(a) INSERT 前 kill；(b) 函数返回后 COMMIT 前 kill（专用连接 BEGIN+显式调用+pg_terminate_backend）→ 以 DB 状态断言回滚（Job/caller identity/key 均不存在）；(c) 完整提交对照组 → 三项均持久化。
- D 组 AS2-20..24：跨 tenant/project 独立创建、403、404 不泄露、400、lookup 与 POST 并发（N-6）——HTTP 层在 generation_api.rs。
- E 组 AS2-25..31：unkeyed 语义逐字保留（含 common.rs 适配）、legacy NULL 身份 lookup 404、换 key replay 不改写原 key/hash、28a/28b、0003 清洗语义不触碰、purge 正文清理后 replay（存储 hash 判定）、AS-1 全量回归。
- F 组 AS2-32..35：终态身份保留、直接 UPDATE 拒绝（触发器）、NULL→非 NULL 补录拒绝、唯一性不可复用。
- AS2-45：Job 创建事务前后 `idempotency_record` 行数不变。
- **AS2-46：迁移证据（行数一致/无回填/无生产调用方 grep/pg_stat_activity）→ 标 BLOCKED/人工门禁，不标 PASS**，写入 PR。

**修改 `tests/tests/generation_api.rs`**：lookup 命中 200 含 caller_request_id + poll_url；404/403/400。
**修改 `tests/tests/sdk_contract.rs`**：AS2-39/40（SDK lookup 200/404/403/传输分类；caller_request_id 必填序列化、响应可空反序列化）。
**修改 `tests/tests/contract_openapi.rs`**：AS2-37/38 parity。

## Phase 7：质量与交付

- `cargo fmt --check`；`cargo clippy --workspace --all-targets --all-features -- -D warnings`；`cargo test --workspace`（DATABASE_URL → PostgreSQL 16，docker compose 起 PG16）。
- Docker build（Dockerfile / Dockerfile.worker）+ compose 校验；现有回归全绿（不得删改既有测试）。
- 架构实施记录：docs/architecture 新增 AS-2 实施记录（迁移证据、矩阵结果、边界声明）。
- 提交（清晰可追溯 commit）→ 推送分支 → 创建 Draft PR（目标 main，引用 ART-04G v3，列 54 项矩阵逐项结果、未关闭风险与人工门禁）。
- 最终报告 A-H 八项"是否"声明全部为否。

## 验证方式

1. 单元：fingerprint golden/突变测试。
2. 集成：`cargo test --workspace`（PG16，sqlx::test 每测试独立库）。
3. 权限：roles_separation/migration_guard 扩展断言实际 42501/权限位。
4. 并发：as2_caller_identity.rs 真实多连接事务屏障（无 sleep）。
5. 契约：contract_openapi/sdk_contract parity。
6. CI 等价命令本地全跑 + Docker/compose。

## 关键边界（违规即打回）

不实施 AS-3/target_version_id；不接线 HTTP create 202/200；不加 SDK get/cancel job；不启用 Generator/Worker；不触碰 LMMS/File Service/SeeCode；不改 main、不改 docs 分支、不合并 PR #8；fingerprint_version 仅 v1 + 失败关闭；Job 创建路径永不写 idempotency_record。

## v3 一致性检查修订（2026-11-25，优先于上文冲突内容）

一致性检查已完成（详见会话记录），ART-04G v3 = docs 分支远端 HEAD c334028（§10 为最终规范，优先于 §§5-9）。修订：

1. AS2-53 屏障重设计（v3 §10.6）——见 C 组。
2. AS2-50 显式 T1/T2/T3 顺序 + pg_blocking_pids 证明——见 C 组。
3. AS2-10 三阶段终止——见 C 组。
4. AS2-52 拆分：52（' K2 ' → 409）、52a（''/SP-only ≡ 无 key；HTAB/CR/LF 不折叠）、52b（>255 两路径均 400 INVALID_IDEMPOTENCY_KEY）。
5. 信任边界文档：函数头注释 + 实施记录声明 tenant/project 由 Application 层 Scoped 授权解析，DB 函数无法独立验证 HTTP 授权，仅保证 scope 内配对/唯一/不可变。
6. 错误分层映射表（v3 §9.5 + ART-04I §4.4 逐字对照）落入实施记录；DB token 不直接决定 HTTP status。
7. AS2-51 显式测试（GENERATOR_UNAVAILABLE → 503，非 IntegrityFailure）。
8. F-3 复核：生产调用方 grep=0 已落盘；pg_stat_activity drain + 行数一致 = AS2-46 部署人工门禁（BLOCKED）。
9. 54 项矩阵按 §10.6 终版编号（01..27、28a/b、29..46、47..53）逐项给测试位置。
10. 夹具权限策略：全部直写夹具经 owner 连接（非 APP 角色），身份列写入仅经函数；不恢复 APP 宽表权限。
