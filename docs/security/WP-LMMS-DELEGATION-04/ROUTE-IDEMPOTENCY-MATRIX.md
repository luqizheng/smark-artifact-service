# WP-LMMS-DELEGATION-04 — 路由→操作矩阵与幂等分类（实现后版本）

**基线**：`8fd5173061f935513246b952195e88cea39e8515`（WP-03 HEAD）
**本文档**：任务书 §4 要求的路由矩阵 + §6 幂等分类。Discovery 阶段（改动前）的缺口清单见 PR 描述与 `TEST-EVIDENCE.md` §1。
**路由清单对账**：`apps/artifact-api/src/routes/mod.rs:21`（19 条）↔ `openapi/openapi.yaml`（19 path）双向一致，无未登记路由。

## 1. 绑定轴（全部写路由统一）

| 轴 | 强制点 | 拒绝语义 |
|---|---|---|
| issuer/aud/purpose/actions/scope/jti | `AiPmDelegationVerifier`（验证层，401）+ `Delegation::require`（服务层，403）| 契约 §3.2 V1–V9（未改动）|
| **query 禁带（v1 契约红线，WP-04-R1 P1）** | HTTP adapter 统一入口 `write_binding()`（全部 9 个写 handler 唯一入口），先于任何副作用 | 400 `INVALID_REQUEST`；`?x=1` 与裸 `?` 均拒绝（`Uri::query()` 保留存在性）|
| **caller_request_id** | 服务层 `enforce_write_binding`（`operation_binding.rs`），`Delegation::require` 之后、任何副作用之前 | 403 `SCOPE_DENIED`；Job 创建路由 body caller 与 claim 交叉比对 |
| **request_hash（v1 冻结）** | 同上：`canonical_request_fingerprint(method, 实例化path, caller, 原始bodyValue)` 重算 vs `biz.request_hash` | 403；method/path/目标/body 任一变异 → 指纹不同 |
| **artifact_id** | 同上：claim 存在时请求目标必须相等；**W2–W8 为目标寻址路由（含 W8：与路径 artifact ID 精确比对，R1 P2 修正）**；真正无目标路由（W1/W9）出现该 claim → 403（裁决 D2）；缺省时不拒绝（决策项 D4）| 403（失配/无目标出现）|
| **operation_id** | 仅 W9 落地 `generation_job:{job_id}` 精确比对（契约 §5.3 语法）；其余七条路由语法未冻结（裁决 D1：存在时**不比对也不拒绝**，现状保持；未完成语义绑定的路由清单见 TEST-EVIDENCE §7.4）| W9 不匹配 → 403 |
| 幂等仲裁 | 每路由独立机制（§3） | 见分类表 |

## 2. 写路由矩阵（9 条）

缩写：`binding` = `enforce_write_binding`（`crates/artifact-application/src/service/operation_binding.rs`）；`require` = `Delegation::require`。强制顺序：`require` → `binding` → 输入校验 → 副作用。

### W1 · POST /v1/artifacts（createArtifact）
| 项 | 值 |
|---|---|
| Handler → 服务入口 | `artifacts::create_artifact` → `ArtifactService::create_artifact`（artifact.rs） |
| 动作 | `artifact:create`；scope：tenant=token，project=**body**（唯一 Authed 路由） |
| 目标 | 新建 Artifact（无既有目标） |
| 绑定 claim | caller + request_hash；artifact_id 存在 → 403（D2，无目标路由）；operation_id 存在 → 不比对不拒绝（D1 待裁决） |
| v1 指纹输入 | POST /v1/artifacts + claim caller + 原始 body Value（`ValidJson.raw`） |
| caller 来源 | token claim（PC-O5-1 验证层红线 401） |
| 分类① keyed（`artifact_key` 存在） | **自然键幂等**：`uq_artifact_project_key` + `artifact_creation_identity`；`PgKeyedArtifactCreator` 单事务（预留→身份→插入→完成→COMMIT） |
| 分类② unkeyed + `Idempotency-Key` | **操作键幂等（单事务）**：`PgUnkeyedArtifactCreator`（WP-04 修复：预留→插入→完成一次 COMMIT；业务失败整体回滚零残留） |
| 分类③ unkeyed 无头 | **非幂等（明示）**：直通 insert，重试产生重复 Artifact；矩阵中如实标注，无幂等声明 |
| 同键重试 | ① 200 existing_key/idempotency_replay；② 200 idempotency_replay；③ 201（重复创建，分类即此语义） |
| 同键异指纹 | 409 `IDEMPOTENCY_KEY_REUSED`（①②均为确定性） |
| 并发重复 | ①② 恰 1 个 created，其余 resolved/replayed（DB 唯一约束 + 有界重试） |
| 提交前失败 | ①② 零残留（单事务回滚）；重试 = 全新尝试 |
| 证据 | `wp04 :: a1/a2/d1/d3/d4/d5/d6/d7`、`as1 :: 38 例`、`artifact_flow :: idempotency_replay_and_reuse_conflict` |

### W2 · PATCH /v1/artifacts/{artifact_id}（patchArtifact）
| 项 | 值 |
|---|---|
| 服务入口 | `ArtifactService::patch_artifact`；动作 `artifact:update_metadata`；Scoped（project=X-Project-Id） |
| 目标 | 路径 artifact（进 v1 指纹） |
| 分类 | **非重试型（CAS）**：`row_revision` 乐观锁；重试（同 If-Match）确定性 409 `VERSION_CONFLICT`，无重复副作用；无幂等声明（§6.1 第三类） |
| If-Match 边界 | If-Match 头**不在** v1 指纹成分（契约冻结成分仅 method/path/caller/body；裁决 D3）：同 caller+同 body 的重放按首次结果/CAS 收敛，CAS 仅约束首次应用 |
| 证据 | `wp04 :: a3/b1/b2/b3/d2`、`artifact_flow :: create_get_patch_etag_roundtrip` |

### W3 · POST /v1/artifacts/{artifact_id}/versions（createVersionSession）
- 动作 `version:create`；目标 = 路径 artifact。分类：**P0 阻断（零写入）**——StoragePort 恒 `ProtocolBlocked` → 503 `STORAGE_UNAVAILABLE`；无可保护副作用，幂等性 N/A。绑定通过后到达存储门（`wp04 :: a9`）；绑定失败 403 先于存储门（`wp04 :: e1`）。

### W4 · POST .../versions/{version_id}/commit（commitVersion）
- 动作 `version:commit`；目标 = 路径 artifact+version（均进指纹）。同 W3：P0 阻断、无 body（指纹 body 段 = null）。证据 `wp04 :: a9`。

### W5 · POST /v1/artifacts/{artifact_id}/current-version（setCurrentVersion）
- 动作 `version:set_current`；目标 = 路径 artifact + body version_id（进指纹）。
- 分类：**状态收敛幂等（无 expected）/ CAS（带 expected）**：指针覆写；重试 200（状态收敛，revision 递增）或 409（expected 不匹配），均确定性、无重复副作用。
- 证据 `wp04 :: a4`、`artifact_flow :: current_version_pointer_invariant_via_api`。

### W6 · POST /v1/artifacts/{artifact_id}/references（createReference）
- 动作 `reference:write`；目标 = 路径 artifact。
- 分类：**自然键幂等**：`uq_artifact_reference_active`（部分唯一索引 `WHERE removed_at IS NULL`）；重复（同 owner/relation 活跃引用）→ 23505 → 400 确定性；恰一个活跃引用。
- 证据 `wp04 :: a5/b4/e1`、`artifact_flow :: reference_create_remove`。

### W7 · DELETE .../references/{reference_id}（removeReference）
- 动作 `reference:remove`；目标 = 路径 artifact+reference（进指纹）；无 body（null）。
- 分类：**状态幂等**：`removed_at IS NULL` 谓词软删；重试 → 404 确定性；无重复副作用。证据 `wp04 :: a6`。

### W8 · POST /v1/artifacts/{artifact_id}/generation-jobs（requestGeneration）
- 动作 `generation:request`；**目标 = 路径 artifact（目标寻址路由；artifact_id claim 与路径 ID 精确比对——失配 403、匹配通过、缺省不拒绝（D4），R1 P2 修正：W8 不是无目标路由）**；请求侧 caller = **body.caller_request_id**（与 claim 交叉比对，`wp04 :: e2`）。
- 现状：generator 注册表 gate → 恒 503 `GENERATOR_UNAVAILABLE`（AS-2 keyed job 端口**未接入 AppState**，ART-02 P0；生产零写入——**本 WP 明示不开启**，migration 0005 机制原样保留）。
- 幂等分类：当前零写入 → N/A；AS-2 语义（caller 三元组部分唯一索引 + 单受控函数事务）未被回归，HTTP 接线属独立后续工作。
- 证据 `wp04 :: a7/e2/g1/g2/g3`、`generation_api :: as2_41_http_create_stays_fail_closed`。

### W9 · POST /v1/generation-jobs/{job_id}/cancel（cancelJob）
- 动作 `generation:cancel`；目标 = 路径 job（进指纹）；无 body（null）；operation_id 契约语法 `generation_job:{job_id}` 强制比对（存在时）。
- 分类：**状态机幂等**：受控函数 `request_generation_job_cancel` 单语句原子；重试 = terminal 空操作 200，不重复审计；并发由行锁序列化。
- 证据 `wp04 :: a8/b6/d8/d9`、`generation_api :: cancel_queued_job_and_terminal_noop`。

## 3. `caller_request_id` 边界声明（契约 §3.1.2 #6 重申）

`caller_request_id` 是**幂等/绑定键，不是防重放保证**：token 在 TTL 窗口内对同一绑定请求可多次提交，由端点幂等机制收敛；跨请求/跨目标的复用由 V10 指纹比对拒绝（403）。本文档与实现均不宣称"caller_request_id 已提供全局防重放"。

## 4. 读路由（10 条，非本 WP 范围）

health/ready 无认证（元端点）；其余 8 条受保护读（WP-03 R2-F-03 修复后动作映射，契约 §3.2 表）——未改动，天然幂等。
