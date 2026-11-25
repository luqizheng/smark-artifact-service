# ART-04G — AS-2（GenerationJob Durable Caller Identity）独立架构评审与实施契约

> **Status:** ART-04H 挑战后修订版（v2，待独立复核；原 §5 冻结候选已由 §9 修订条款覆盖）
> **Date:** 2026-11-25
> **Reviewer:** 独立架构评审（基于源码与迁移的真实核查，非仅静态审计转述）
> **Baseline:** `main` + AS-1 分支 `feat/as-1-durable-artifact-creation-identity` @ `5541d3be8c90cfcf9283b6e59426c8a3c2bfe099`（PR #8，Draft，未合并）
> **Scope:** 仅 AR04-01（GenerationJob Durable Caller Identity）。不含 AS-3（AR04-02）、AS-4、PM-1..PM-3。
> **约束遵守:** 本评审未修改任何代码、Migration、OpenAPI、SDK；未创建实施分支；未合并 PR #8。

---

## 1. 评审结论

| 对象 | 结论 |
|---|---|
| **AR04-01（ART-04A §2 设计文本）** | **CHANGES REQUIRED** — 设计方向正确，但有 5 处必须先冻结的规格缺口/缺陷（见 §4）。全部缺口在本文件 §5 中给出冻结解。 |
| **AR04-01 → AS-2 代码实施** | **暂缓启动（CHANGES REQUIRED）** — ART-04H 新增 C-1 P1 与 C-2..C-5 P2 已按 §9 修订；须先由独立评审确认 §9 修订完整、无新 P1，再另行冻结并授权 AS-2 实施。 |
| **AR04-02（target_version_id 暴露与完成不变量）** | **NOT APPROVED / DEFERRED to AS-3** — 详见 §7.2。不得在 AS-2 中实施任何 AR04-02 内容。 |

**无 P0 缺陷。** 5 个 P1、5 个 P2，全部可通过本契约冻结解决，不需要推翻设计方向。

---

## 2. 审查基线与证据（源码核查结果，非转述）

以下事实全部在本分支源码中逐条核实：

| # | 事实 | 证据位置 |
|---|---|---|
| F-1 | `generation_job` 无 `caller_request_id` 列；`idempotency_key` 为可空列，带部分唯一索引 `(tenant_id, project_id, idempotency_key) WHERE idempotency_key IS NOT NULL` | `migrations/0001_init.sql` L313-348、`migrations/0003_job_coordination_fixes.sql` L139-141 |
| F-2 | `create_generation_job_with_input` 为 SECURITY DEFINER、固定 `search_path = pg_catalog, artifact`，当前去重**只按 Idempotency-Key**（读前查 + `unique_violation` 异常块内重读胜者），无 caller 身份参数 | `migrations/0003...sql` L271-389 |
| F-3 | 该函数**无任何生产调用方**：`request_generation` 应用路径在 generator 检查后直接返回 `GeneratorUnavailable`，从不触达数据库函数；仅测试代码调用它 | `crates/artifact-application/src/service/artifact.rs` L574-623、grep 全仓 `.rs` 仅 `tests/` 命中 |
| F-4 | GenerationJob 的 Idempotency-Key 去重**不使用** `idempotency_record` 表，也不存在 reservation-first 两段流程：Job 行本身即预留（key 存在 Job 行上，无 TTL 清理）；`idempotency_record` 仅被 Artifact 路径使用 | `migrations/0003...sql` L341-389、`migrations/0001_init.sql` L393-410 |
| F-5 | `generation_job` **无任何删除路径**：Attempt FK 为 `ON DELETE RESTRICT`；`purge_expired_generation_job_input` 只在终态 7 天后置空 input/options 正文（保留 sha256 与元数据）；恢复扫描只做 UPDATE（取消） | `migrations/0002...sql` L58、`migrations/0003...sql` L908-933、L821+ |
| F-6 | `guard_job_transition` 触发器已强制 ownership 列不可变 + 状态机转移合法，但**不**保护即将新增的身份列 | `migrations/0001_init.sql` L350-370 |
| F-7 | HTTP 层：`Scoped` extractor 从 Bearer delegation 提取 tenant、从 `X-Project-Id` 提取 project；`get_job`/`cancel_job` 路由存在且走 `Action::GenerationRead/GenerationCancel`；generation create 路由 `_headers: HeaderMap` 被忽略（未解析 Idempotency-Key），整体 fail-closed 503 | `apps/artifact-api/src/auth.rs` L52-82、`apps/artifact-api/src/routes/mod.rs` L57-64、`routes/artifacts.rs` L486-529 |
| F-8 | 请求体 hash（`request_hash`）为原始 HTTP Body SHA-256，仅在 Artifact create 路径计算；Raw body 校验器已产出 `request_hash` | `routes/artifacts.rs` L28-51 |
| F-9 | Rust DTO：`CreateGenerationJobRequest = { generator_id, input, options?, expected_current_version_id? }`；`GenerationJobResponse` 无 `caller_request_id`、无 `target_version_id` | `crates/artifact-contract/src/dto.rs` L196-221 |
| F-10 | SDK 仅有 `request_generation`，无 `get_generation_job`/`cancel_generation_job`/lookup 方法；SDK 复用 contract DTO | `crates/artifact-client/src/lib.rs` L320-332 |
| F-11 | OpenAPI：POST create 文档化 202 + 409 `IDEMPOTENCY_KEY_REUSED`；`GenerationJob` schema 无 caller 字段；无 by-caller-request 路由 | `openapi/openapi.yaml` L382-470、L771-813 |
| F-12 | DB 角色：APP = SELECT/INSERT/UPDATE 全表（含 `generation_job`）+ create 函数 EXECUTE；WORKER 仅协调函数；AS-1 新增 MAINTENANCE 角色仅持清理函数 EXECUTE；`artifact_creation_identity` 显式 REVOKE UPDATE/DELETE | `deploy/db-roles.sql` L50-96 |
| F-13 | AS-1 已冻结的先例：独立身份表 + append-only 触发器 + 双向 DEFERRED 配对触发器 + 版本化语义指纹（Rust 计算）+ 单事务 keyed-create 端口 + `X-Artifact-Resolution` 头 + 201/200 解析语义 | `migrations/0004_artifact_creation_identity.sql`、`crates/artifact-application/src/service/creation_fingerprint.rs`、`crates/artifact-infrastructure/src/postgres/keyed_create.rs`、`routes/artifacts.rs` L229-241 |
| F-14 | 已知教训（AS-1）：READ COMMITTED 下跨语句双快照可把并发赢家误判；对策是单语句原子查询 / 回滚后新事务重读 | AS-1 Lessons Learned（project memory） |

---

## 3. 13 个必答问题逐项裁决

**Q1 `caller_request_id` 是否真正代表一次不可变的业务 GenerationJob 创建意图？**
是。裁决采纳 ART-04A §2.2 定义并补充精确语义：它是 **AI-PM 在首次出站请求前生成并本地持久化的 UUID，恰好对应一次"预期创建一个 GenerationJob"的业务意图**；不是 Task、Run、多步编排或 HTTP 请求。同一意图的所有重试/对账复用同一 ID；终态后发起新一代生成必须换新 ID。Artifact Service 将其视为不透明 UUID：只强制作用域唯一性、指纹一致性、持久性，**不解释**其业务语义（ART-04 §2 权威边界不变）。生命周期 = 所引用 Job 的可查询/可审计生命周期（当前实现下即永久，见 Q11）。

**Q2 `Idempotency-Key` 与 `caller_request_id` 是否严格独立？**
是，且必须保持三个独立 hash 语义（冻结，见 §5.2/§5.6）：
- `request_hash`（原始 HTTP Body SHA-256）= 传输层幂等冲突语义，语义**不变**；
- `input_sha256`（DB 侧 schema_version + JSONB 规范文本）= 输入快照完整性，语义**不变**；
- `request_fingerprint`（新，Rust 侧版本化语义哈希）= 持久创建身份收敛，**独立**于前两者。
`caller_request_id` 位于请求体中 ⇒ 自动纳入 raw body hash ⇒ "同一个短命 key 不得跨持久身份重放"由现有 raw-hash 冲突语义结构性保证（同 key 不同 caller_request_id 必然不同 body hash → 409）。指纹**不得**包含 Idempotency-Key（否则 key 轮换会破坏恢复）。测试 E-6/E-7 固化两条方向。

**Q3 caller_request_id 存 GenerationJob 表还是独立 correlation table？**
**结论：存 `generation_job` 行上（列 + 部分唯一索引 + 行内 CHECK 配对 + 触发器不可变），不建独立表。** 依据现有 schema/索引/外键/删除策略：
1. 关系严格 1:1（一个 caller_request_id 永远对应一个 Job 行）；无"一个 Job 多次创建意图"或"一个意图多 Job"的形态。
2. AS-1 选择独立表的理由——**历史 keyed Artifact 行无法重建指纹**——对 Job 不成立：`caller_request_id` 是全新列，任何携带它的 Job 必然由新事务写入指纹（行内 CHECK 强制成对），不存在 legacy-unverifiable 类别，因此不需要 AS-1 的双向 DEFERRED 配对触发器机制。
3. 删除策略（F-5）：Job 无删除路径，Attempt FK 已 RESTRICT；行级身份随 Job 天然持久。独立表 + `ON DELETE RESTRICT` 的额外收益只是"阻止一个当前不存在的未来删除功能"，代价是多一张表、成对触发器、第二次插入以及 AS-1 教训 F-14 的双快照风险面。
4. 单行单 INSERT 即"身份注册 = Job 创建"，从结构上消除双快照误判（F-14），并复用同表已有的 Idempotency-Key 部分唯一索引模式。
5. 不可变性由扩展现有 `guard_job_transition` 触发器在数据库层强制（APP 角色对 `generation_job` 持有 UPDATE，F-12，触发器是必要边界）。
**前置补偿门禁（冻结）**：未来任何 Job 归档/删除/物理清理设计，必须先提出 caller identity tombstone 方案并通过独立评审（呼应 ART-04A §2.3）；在此之前引入 Job 删除路径视为违反本契约。

**Q4 `(tenant_id, project_id, caller_request_id)` 唯一性是否足够？是否需要认证 caller namespace？**
**足够，V1 不加 caller namespace。** tenant 来自已验证 delegation、project 来自校验头（F-7），调用方伪造不了作用域。当前产品形态一个 project 一个授权调用方（AI-PM）；跨调用方共用 namespace 时 UUIDv4 碰撞可忽略，而"同 ID 不同指纹 → 409"与"同 key 不同 body → 409"已构成完整性边界。将 delegation subject 计入指纹会破坏"凭据轮换不产生第二个身份"（ART-04E §4.3 同款原则）；计入唯一键会让凭据轮换无法对账。**冻结**：若产品未来支持同 project 多个独立 API client 且要求隔离 namespace，必须引入**服务端派生**的 caller namespace 并升级指纹版本，独立评审；不得接受调用方自报 namespace 作为授权声明（ART-04A §2.3 原文保留）。Lookup/read 授权与既有 `GET /v1/generation-jobs/{job_id}` 一致：project 级 GenerationRead 即可读，不引入 subject 维度（见 Q9）。

**Q5 Canonical request fingerprint 是否有明确、可实现、版本化定义？**
ART-04A 原文方向正确但字段清单不完整且有一处错误（见 Q6/P1-1）。本评审在 §5.2 给出**精确、可实现、版本化**的 v1 定义：域分隔符 `generation-job-create:v1`；覆盖 tenant/project、artifact_id、caller_request_id、generator_id、input、options；由 Rust 从**校验后的类型化 DTO**计算（复用 AS-1 `canonical_fingerprint` 的工程模式与 golden vector 冻结测试）；DB 函数只做格式校验（64 hex）与版本比对。同时明确三个 hash 的分工（Q2），`input_sha256` 与 `request_hash` 语义原样保留。

**Q6 Fingerprint 是否正确排除传输字段且不破坏 Idempotency-Key 冲突语义？**
排除清单见 §5.2"禁止包含"。两处对 ART-04A 原文的**正式修订**：
1. **`expected_current_version_id` 从指纹中排除（P1-1）**。它是创建时点的乐观并发前置条件（attempt-local），不是生成的不可变语义：若纳入，(a) 前置条件失败（409 VERSION_CONFLICT、未创建 Job）后刷新指针重试会撞上 409 `CALLER_REQUEST_ID_REUSED`，而该 ID 在数据库中**根本不存在**——错误语义荒谬；(b) 恢复算法 §2.5.5"409 后禁止换 ID"将把调用方卡死。排除后：无 Job 时刷新前置条件重试收敛为同一 ID 创建；有 Job 时一律以已提交的 Job 为准（重放分支不重评前置条件）。ART-04A §2.2 括号中的 `expected_current_version_id` 必须在 AS-2 PR 中修订。
2. 传输/服务器派生字段排除（Idempotency-Key、trace/request ID、generator_version/adapter_version、执行策略参数、时间戳）。
Idempotency-Key 冲突语义**不被破坏**：raw `request_hash` 的同 key 冲突路径原样保留；指纹不含 key，key 轮换重放合法（E-6）；跨身份复用 key 被 raw hash 拦截（E-7）。

**Q7 并发创建时唯一约束竞争者能否通过新事务重新读取赢家并验证 fingerprint？**
能，方案冻结于 §5.3：单条 INSERT 携带 `(caller_request_id, request_fingerprint, fingerprint_version)`；`unique_violation` 落入 PL/pgSQL 异常块（子事务自动回滚，外层事务合法延续——现有函数 F-2 已采用该模式），异常块内**先**按 caller_request_id 重读胜者（READ COMMITTED 下新语句 = 新快照，胜者必已提交：唯一违例只可能来自已提交行或等待后提交的并发插入），指纹一致 → 重放；不一致 → 409。未按 caller 命中则退回 Idempotency-Key 解析（同现有逻辑）。禁止在异常块外继续使用已失败子事务的语句结果；禁止把 23505 一律当重试（P1-5）。

**Q8 stale/incomplete Idempotency-Key reservation 是否会阻止 durable caller identity 恢复？**
**不会——按本契约在结构上不可能发生。** 现状 F-4：Job 创建路径没有 reservation-first 流程，Idempotency-Key 直接存于 Job 行，崩溃未提交 = 什么都不存在，无 stale reservation 可言。**冻结（P2-1）**：AS-2/AS-4 **禁止**把 Job 创建接入 `idempotency_record` 的 reserve→create→complete 流程；验收矩阵 R-5 固化"Job 创建事务不写 `idempotency_record`"。ART-04A §2.3/§5 中"stale reservation"表述针对的是 Artifact 路径的机制假设，AS-2 PR 需要在契约映射表中说明该场景在 Job 路径由单事务结构满足。

**Q9 Lookup endpoint 是否具备完整 delegation 与 scope 隔离？**
是，冻结于 §5.4：与 `GET /v1/generation-jobs/{job_id}` 完全同一 `Scoped` 链路（Bearer delegation → tenant；`X-Project-Id` → project）+ `Action::GenerationRead`；查询谓词强制 `(tenant_id, project_id, caller_request_id)`（命中部分唯一索引）；范围内不存在一律 404，**绝不**泄露跨 scope 存在性；路径参数必须解析为 UUID（非法 → 400）；只读、返回当前权威 Job 状态，不创建、不重试、不触发生成。授权粒度与既有 Job 读取一致（project 级），不引入 subject 维度（Q4）。

**Q10 Lookup 404 后重试 POST 的竞态是否安全？会不会对已存在 Job 误报 404？**
安全，不会误报。依据：创建是**单行单事务**（§5.3），身份与 Job 同行同事务提交（Q3）；lookup 读取同一张表、同一行（经部分唯一索引），READ COMMITTED 新语句快照必然可见已提交行（单库、无只读副本，架构无引入）；提交前 lookup/并发 POST 得到 404 → 重试 POST → 与胜者在唯一索引上收敛（Q7）。ART-04A §2.5 的"404 后重试安全"前提——"create 与 lookup 共享同一持久唯一性事务"——由单行设计直接满足。验收矩阵 N-6 固化"提交后任意时刻 lookup 不得 404"。

**Q11 失败、取消、超时、归档或未来删除 Job 时，caller identity 是否仍满足持久性与不可复用？**
满足，冻结语义如下：
- 终态（succeeded/failed/cancelled/timed_out）**不清除**身份列；`purge_expired_generation_job_input` 只置空 input/options 正文，不触碰身份与指纹（指纹比较只需存储的 hash，输入正文被清理后重放判定依然正确——固化测试 R-6）。
- 当前无归档/删除路径（F-5）；身份列不可变由触发器强制，APP 直连 SQL 也无法改写（M-2）。
- 唯一索引使身份在行存在期间不可复用；行永久存在 ⇒ 身份永久有效。
- **未来门禁**：任何归档/删除设计必须先提交 tombstone 方案（Q3 补偿门禁），防止"删行 = 身份复活"。
- ART-04A §2.3 的"correlation tombstone 保留至审计视野"要求转记为上述门禁条款。

**Q12 SECURITY DEFINER 创建函数、权限与事务边界是否与现有 Job 创建机制兼容？**
兼容。迁移 0005 沿用 0003 先例（DROP + CREATE 换签名）：保持 SECURITY DEFINER、固定 `search_path = pg_catalog, artifact`、函数体单事务、`REVOKE EXECUTE FROM PUBLIC` 后按签名对 APP 精确授权（db-roles.sql 同步更新，F-12）；WORKER 无需任何新授权（claim/complete 不涉及 caller 身份）；MAINTENANCE 角色与 `purge_expired_idempotency_records` 不受影响。Rust 侧按 AS-1 §5.6/§5.7 先例建专用端口（§5.3.3），函数调用即完整事务，不在 Application Service 外再包事务、不泄露 `sqlx::Transaction`。

**Q13 能否在不启用 Generator、不改 Worker、不触碰 LMMS File Service 的前提下独立完成 AS-2？**
能。改动面 = 迁移 0005 + db-roles 授权 + Rust domain/DTO/port + lookup 路由 + OpenAPI + SDK 类型与 lookup 方法。Generator registry 保持 `unavailable`（创建函数内既有 GENERATOR_UNAVAILABLE 门禁原样保留，生产生成不启用）；Worker 协调函数零改动；LMMS/File Service 零接触。注意推论：HTTP 层的 keyed-create 全链路（202/200）在 generator 可用前无法端到端走通，因此并发/故障注入验收在 **DB 函数 + 端口级（真实 PostgreSQL，sqlx::test）** 执行（AS-1 同款先例）；HTTP 契约测试覆盖 lookup 路由、DTO 序列化、OpenAPI parity；HTTP create 接线与 typed get/cancel SDK 方法归属 AS-4 门禁（§7.1 边界 9）。

---

## 4. 缺陷清单

**P0：无。**

### P1（实施前必须按 §5 冻结解执行）

| ID | 缺陷 | 冻结解 |
|---|---|---|
| P1-1 | ART-04A §2.2 将 `expected_current_version_id` 纳入指纹：前置条件失败后刷新指针的重试会对**不存在的身份**返回 409，且与恢复算法"409 后禁止换 ID"互相卡死；409 语义产生"已有 Job 冲突"与"前置条件过时"二义性 | §5.2：指纹排除该字段；在 AS-2 PR 中正式修订 ART-04A §2.2；AS-3 实施前置条件时只在"无既有 Job"分支评估 |
| P1-2 | ART-04A/04D 对存储模型留双解（"Job 行或独立 correlation table"），未冻结；若实施选独立表将引入成对触发器 + 双快照风险面（F-14）且无对应收益 | §5.1：冻结为 Job 行列 + 行内 CHECK 配对 + 部分唯一索引 + 触发器不可变 + 未来删除/归档 tombstone 门禁 |
| P1-3 | 去重与 generator 可用性检查的顺序未定义；现函数先查 generator（F-2），将导致"generator 临时不可用期间的恢复重放"收到 503 而不是已存在 Job（违反 ART-04A §2.3 重放语义） | §5.3：冻结函数内顺序 = 输入校验 → caller 身份解析 → Idempotency-Key 解析 → generator 检查 → INSERT |
| P1-4 | Job 侧解析/重放响应语义未定义（202 vs 200、重放标记、`CALLER_REQUEST_ID_REUSED` 错误码未入库） | §5.5：202 created / 200 replay；`X-Generation-Job-Resolution: created|caller_request_replay|idempotency_replay`；新增稳定错误码 `CALLER_REQUEST_ID_REUSED`（409）与 `CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH`（失败关闭） |
| P1-5 | 现函数用 ERRCODE 23505 承载应用冲突 `IDEMPOTENCY_KEY_REUSED`；AS-1 Rust 分类器把 23505 一律映射为可重试（`keyed_create.rs` L50），照搬会把确定性冲突变成有界重试后 5xx，且可能对同一 key 制造重复插入压力 | §5.3.3：端口按**稳定消息 token** 映射应用冲突为终态；23505/40001/40P01 仅在非消息命中时进入重试分类 |

### P2（随 AS-2 落地，不阻塞启动）

| ID | 事项 | 处理 |
|---|---|---|
| P2-1 | ART-04A §2.3/§5 的"stale idempotency_record reservation"表述假设 Job 路径存在 reserve→insert 流程，与现状（F-4）不符 | 契约映射表说明该场景由单事务结构满足；禁止 AS-4 引入 reservation-first（Q8） |
| P2-2 | 多 client/项目的 caller namespace 为未来产品决策 | 记录为产品门禁（Q4），V1 不实现 |
| P2-3 | JSON payload 指纹的字符串/数字规范化规则未定义（AS-1 的 NFC 规则只覆盖标量身份字段） | §5.2 冻结 payload 规则（字符串字节精确、数字最短往返、拒绝非有限值）+ golden vector 冻结测试 |
| P2-4 | 迁移切换：ART-04E 9 步 drain 流程是否适用于 AS-2 需要明确，避免照搬 | §5.7：基于 F-3（无生产调用方）证明不需要 ingress 停写 drain，但须在实施时**重新验证**该前提并提供证据 |
| P2-5 | OpenAPI 中 Idempotency-Key 保留期表述（24h）为 Artifact 路径语义；Job 路径 key 生命周期 = Job 生命周期，未明示 | §5.5 OpenAPI 文案补充，消除调用方对 Job 侧 key 过期的错误假设 |

---

## 5. AS-2 冻结契约（implementation-ready）

### 5.1 数据模型（迁移 0005，加法式，不重写 0001–0004）

```sql
-- 0005_generation_job_caller_identity.sql（骨架，最终 SQL 遵循仓库迁移约定）

ALTER TABLE artifact.generation_job
    ADD COLUMN caller_request_id   uuid,
    ADD COLUMN request_fingerprint char(64),
    ADD COLUMN fingerprint_version smallint;

-- 行内配对：三者同空或同非空；指纹格式与版本受 CHECK 约束。
ALTER TABLE artifact.generation_job
    ADD CONSTRAINT generation_job_caller_identity_pairing_check CHECK (
        (caller_request_id IS NULL AND request_fingerprint IS NULL AND fingerprint_version IS NULL)
        OR (caller_request_id IS NOT NULL AND request_fingerprint IS NOT NULL
            AND fingerprint_version IS NOT NULL
            AND fingerprint_version > 0
            AND request_fingerprint ~ '^[0-9a-f]{64}$')
    );

-- 作用域唯一：创建收敛与 lookup 共用同一索引。
CREATE UNIQUE INDEX uq_generation_job_caller_request
    ON artifact.generation_job (tenant_id, project_id, caller_request_id)
    WHERE caller_request_id IS NOT NULL;

-- CREATE OR REPLACE FUNCTION artifact.guard_job_transition(): 在现有
-- ownership 不可变与状态机校验之外，追加：
--   * caller_request_id 任何变化（含 NULL→非 NULL、非 NULL→NULL/改值）→ 拒绝；
--   * request_fingerprint / fingerprint_version 任何变化 → 拒绝。
-- （APP 角色对 generation_job 持有 UPDATE，触发器是身份不可变的数据库边界。）
```

要点：
1. 历史与 legacy Job 保持三列全 NULL，合法；**禁止**任何 NULL→非 NULL 的身份补录（无 adoption 流程；与 AS-1 artifact_key 同款原则）。
2. 不建独立 correlation table（Q3 裁决）；不加第二个查询索引（部分唯一索引即 lookup 索引）。
3. 历史清点：列新增后所有现存 Job 的 caller_request_id 必然为 NULL，迁移内以 `RAISE NOTICE` 记录清点结果（不回填、不制造身份）。
4. `idempotency_key` 列与其部分唯一索引原样保留。

### 5.2 Canonical request fingerprint v1（精确定义）

**计算位置**：Rust（应用层），基于**校验后的类型化 DTO**（非原始 JSON 字节）；DB 函数仅校验 64 位小写 hex 与版本比对。复用 AS-1 `creation_fingerprint` 的工程结构（版本分派 + golden vector 冻结 + 未知版本 fail-closed）。

**公式**：`lowercase_hex(SHA-256(UTF-8(canonical_json)))`，其中 canonical_json 为按字典序（Unicode code point）键排序、紧凑（无空白）序列化的 JSON 对象，字段如下：

| 字段 | 值 |
|---|---|
| `domain` | `"generation-job-create:v1"`（固定域分隔符） |
| `tenant_id` / `project_id` | 服务端授权上下文解析的 UUID 字符串（绝不取调用方自报值） |
| `artifact_id` | 路径解析后的目标 Artifact UUID 字符串 |
| `caller_request_id` | 本身份 UUID 字符串（自描述：指纹永远绑定其所属身份，杜绝跨 ID 比对） |
| `generator_id` | 标量字符串，Unicode NFC 规范化 |
| `input` | 校验后的 SeeCode 输入信封的 canonical JSON（规则见下） |
| `options` | canonical JSON；DTO 层 absent ≡ null（`Option<Value>` 反序列化后同一表示），payload 内部保留 absent/null 区分 |

**payload 内 canonical JSON 规则（v1 冻结）**：UTF-8；对象键按 Unicode code point 排序；紧凑序列化；数组保序（`diagram.nodes/edges` 顺序即语义）；字符串**字节精确**（payload 内不做 NFC——与 AS-1 标量字段 NFC 规则的差异是有意的：payload 是生成器语义内容，重试确定性由"语义体逐字节一致"承担）；整数按整数、浮点按最短往返序列化、拒绝非有限数（NaN/Inf 校验期拒绝）。

**禁止包含（冻结）**：`Idempotency-Key`、`request_hash`、`input_sha256`、trace/request ID、`expected_current_version_id`（P1-1 修订）、服务器解析的 `generator_version`/`adapter_version`、执行策略参数（max_attempts/max_execution_seconds/queue TTL，非调用方可见）、时间戳、`target_version_id`（AS-3 范畴）。

**版本纪律（沿 ART-04E §4.2）**：字段集或规则变更 ⇒ 新增 `fingerprint_version=2` 与新规范化函数，禁止原地修改 v1；比较时按**存储身份的版本**解析；当前单调用模型下版本不一致（存储 v1、请求按 v2 计算）→ `CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH` 失败关闭；引入 v2 必须附带"按存储版本多轮规范化"的端口契约修订与独立评审。

**与既有两个 hash 的关系（冻结）**：`request_hash`（原始 body，传输幂等）与 `input_sha256`（DB 输入快照完整性）语义、算法、产生位置均**不变**；`request_fingerprint` 是第三条独立链路。

### 5.3 创建事务与并发方案

#### 5.3.1 `create_generation_job_with_input` v2（0005 内 DROP + CREATE，沿用 0003 先例）

```sql
CREATE FUNCTION artifact.create_generation_job_with_input(
    p_job_id                 uuid,
    p_tenant_id              uuid,
    p_project_id             uuid,
    p_artifact_id            uuid,
    p_generator_id           text,
    p_input_payload          jsonb,
    p_options_payload        jsonb,
    p_input_schema_version   text,
    p_request_hash           text,          -- 语义不变：原始 body hash
    p_idempotency_key        text,
    p_caller_request_id      uuid           DEFAULT NULL,
    p_request_fingerprint    text           DEFAULT NULL,   -- 64 hex，caller 非空时必填
    p_fingerprint_version    smallint       DEFAULT NULL,
    p_max_attempts           integer        DEFAULT 3,
    p_max_execution_seconds  integer        DEFAULT 300,
    p_queue_ttl_seconds      integer        DEFAULT 86400
) RETURNS TABLE (job_id uuid, resolution text)            -- 'created' |
                                                          -- 'caller_request_replay' |
                                                          -- 'idempotency_replay'
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
...
$$;
```

**函数内执行顺序（冻结，P1-3）**：
1. 输入校验（现函数全量保留：payload 类型/大小、schema 版本、request hash 格式、attempts/执行时长/TTL 上限）；新增：`p_caller_request_id IS NOT NULL` ⇒ `(p_request_fingerprint, p_fingerprint_version)` 必须非空且格式合法；三者必须同空或同非空（DB CHECK 双保险）。
2. **caller 身份解析**（`p_caller_request_id IS NOT NULL` 时）：按 `(tenant_id, project_id, caller_request_id)` 读前查 → 命中：存储 `fingerprint_version` 与传入版本相等且 hash 相等 ⇒ `RETURN (id, 'caller_request_replay')`；hash 不等 ⇒ `RAISE 'CALLER_REQUEST_ID_REUSED'`；版本不等 ⇒ `RAISE 'CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH'`（失败关闭）。**重放分支不重评 generator 可用性与乐观前置条件**。
3. **Idempotency-Key 解析**（原逻辑逐字保留）：同 key 同 request_hash ⇒ `RETURN (id, 'idempotency_replay')`；同 key 不同 hash ⇒ `RAISE 'IDEMPOTENCY_KEY_REUSED'`。
4. **generator registry 检查**（原逻辑保留）：未知 → `UNKNOWN_GENERATOR`；非 available → `GENERATOR_UNAVAILABLE`（生产 fail-closed 门禁不松动）。
5. `input_sha256` 计算（原逻辑不变）。
6. 单条 INSERT 写入 Job 行（含 caller 三列，caller 为空时三列为 NULL）。
7. `EXCEPTION WHEN unique_violation`（子事务自动回滚）：
   - 先按 caller_request_id 重读胜者：命中 ⇒ 指纹一致 → `RETURN (winner.id, 'caller_request_replay')`；不一致 → `RAISE 'CALLER_REQUEST_ID_REUSED'`；
   - 未命中 → 按 idempotency_key 重读（原异常块逻辑保留）⇒ 同 hash 重放 / 不同 hash `RAISE 'IDEMPOTENCY_KEY_REUSED'`；
   - 均未命中 ⇒ 原样 RAISE（视为完整性异常）；
   - 防御条款（结构上不可达，注释声明）：按 key 命中且 hash 相等但胜者 caller_request_id 与传入不同 ⇒ fail-closed RAISE，绝不返回跨身份重放。
8. 正常路径 `RETURN (p_job_id, 'created')`。

#### 5.3.2 隔离与重试（沿 ART-04E §5.3 / Lessons Learned）
READ COMMITTED 足够：唯一性竞争全部经"子事务回滚 + 新语句快照重读"解决；单行单 INSERT 结构上消除 AS-1 的双快照误判面。`40001`/`40P01` 由 Rust 端口做有界重试（≤3 次 + 退避）；不把任意 DB 错误归入"已存在"。

#### 5.3.3 应用端口（沿 AS-1 §5.6/§5.7 先例）

```rust
#[async_trait]
pub trait KeyedGenerationJobCreatePort: Send + Sync {
    /// 一次函数调用 = 一个完整 PostgreSQL 事务（SECURITY DEFINER 函数内完成
    /// caller 解析、key 解析、generator 检查、INSERT、冲突收敛）。
    /// 不在 Application Service 外包事务；不泄露 sqlx::Transaction。
    async fn create_or_resolve_keyed_job(
        &self,
        request: ValidatedCreateGenerationJobRequest, // 校验后 DTO + caller_request_id + 指纹(v1)
        idempotency: Option<IdempotencyInput>,        // 复用 AS-1 类型：endpoint + key + raw request_hash
    ) -> Result<KeyedJobCreateOutcome, KeyedJobCreateError>;
}

pub enum KeyedJobCreateOutcome {
    Created(GenerationJob),
    ResolvedByCallerRequest(GenerationJob), // 指纹一致解析（含 key 轮换/过期后的恢复）
    ResolvedByIdempotency(GenerationJob),   // 仅 unkeyed legacy 路径
}

pub enum KeyedJobCreateError {
    CallerRequestIdReused,                    // → HTTP 409 CALLER_REQUEST_ID_REUSED
    FingerprintVersionMismatch,               // → 失败关闭（5xx 完整性错误 + 告警信号）
    IdempotencyConflict,                      // → HTTP 409 IDEMPOTENCY_KEY_REUSED（现有语义）
    UnknownGenerator / GeneratorUnavailable,  // → 现有 400/503 语义不变
    InvalidRequest(String),
    RetryablePersistenceFailure(String),      // 有界重试耗尽 → 503
    IntegrityFailure(String),                 // → 5xx，绝不盲目重放
}
```

错误映射（P1-5 冻结）：按**稳定消息 token**（`CALLER_REQUEST_ID_REUSED`、`IDEMPOTENCY_KEY_REUSED`、`GENERATOR_UNAVAILABLE`、`UNKNOWN_GENERATOR` 等）映射为终态；仅未命中 token 的 `23505/40001/40P01` 进入重试分类；其余按 `keyed_create.rs` 既有分类策略。

### 5.4 Lookup API 精确语义（冻结）

- 路由：`GET /v1/generation-jobs/by-caller-request/{caller_request_id}`（axum 路由 + OpenAPI 新增）。
- 鉴权：与 `GET /v1/generation-jobs/{job_id}` 完全一致——`Scoped`（Bearer delegation → tenant；`X-Project-Id` → project）+ `Action::GenerationRead`。
- 参数：路径段必须可解析为 UUID，否则 400（稳定原因码，不回显输入）。
- 查询：`WHERE tenant_id = :tenant AND project_id = :project AND caller_request_id = :caller`（Repository 新增 `JobRepository::get_by_caller_request`，走 `uq_generation_job_caller_request`）。
- 结果：命中 → 200 + 标准 `GenerationJob` 响应（含新增 `caller_request_id` 字段）；未命中 → 404（与既有 404 语义一致），**不泄露**跨 tenant/project 存在性；无缓存语义；只读，不创建、不重试、不触发生成、不改变任何状态。
- 与 POST 的竞态安全性见 Q7/Q10/N-6：已提交 Job 在任何后续读取中不可能 404。

### 5.5 HTTP / OpenAPI / SDK 契约（冻结）

**HTTP POST `POST /v1/artifacts/{artifact_id}/generation-jobs`**（接线仍归 AS-4，本节冻结契约语义）：
- 请求体：`caller_request_id` 变为**必填**（OpenAPI `required` 数组追加）。理由：该端点当前 fail-closed 503、无任何生产消费方（F-3/F-11），现在收紧避免未来破坏性变更；legacy 兼容保留在 DB 函数层（NULL 参数），不在 HTTP 层。
- 响应：新建 = **202** + `X-Generation-Job-Resolution: created`；身份/幂等解析 = **200** + `X-Generation-Job-Resolution: caller_request_replay | idempotency_replay`（沿用 AS-1 头命名风格；ART-04A §2.3 的 `X-Idempotent-Replay` 为示例非强制）。
- 错误：409 `CALLER_REQUEST_ID_REUSED`（新增稳定码，与 `IDEMPOTENCY_KEY_REUSED` 并列入错误契约与测试）；400/403/404/413/503 语义全部不变。
- Idempotency-Key 文案（P2-5）：Job 路径 key 生命周期 = 所在 Job 行生命周期（≥ 24h 承诺自然满足），与 Artifact 路径 24h 清理语义分开表述。

**OpenAPI**：`CreateGenerationJobRequest` 增必填 `caller_request_id`（uuid）；`GenerationJob` schema 增可空 `caller_request_id`（legacy Job 为 null，description 说明）；新增 by-caller-request 路由（200/404/401/403）；POST 增 200 重放响应与 resolution 头文档；409 描述追加 `CALLER_REQUEST_ID_REUSED`。

**DTO（contract）**：`CreateGenerationJobRequest.caller_request_id: Uuid`（必填）；`GenerationJobResponse.caller_request_id: Option<Uuid>`；`to_job_dto` 映射补齐。**不**新增 `target_version_id`（AS-3 边界，§7）。

**SDK（client）**：contract DTO 变更自动传导；新增 `get_generation_job_by_caller_request(caller_request_id, ctx)` 类型化方法（endpoint 在 AS-2 已完整可用，含 sdk_contract 测试）；`get_generation_job`/`cancel_generation_job` 与 create 接线仍归 AS-4（§7 边界 9）。

### 5.6 Idempotency-Key 与 durable identity 分离规则（冻结汇总）

1. 两个标识用途/生命周期完全独立（ART-04A §2.1 表格采纳）：key = 传输重放优化；caller_request_id = 持久业务对账身份。
2. 指纹不含 key；raw request_hash 含 body（含 caller_request_id）⇒ 同 key 跨身份重放被现有冲突语义拦截。
3. 同 caller_request_id + 同指纹 ⇒ 解析原 Job（200），无论 key 相同、轮换还是（对 Job 路径）无 TTL 过期。
4. 同 caller_request_id + 不同指纹 ⇒ 409 `CALLER_REQUEST_ID_REUSED`，禁止换 ID 绕过（ART-04A §2.5.5 采纳）。
5. Job 创建路径**永不**写 `idempotency_record`（Q8/P2-1）；`purge_expired_idempotency_records` 及其权限模型不受 AS-2 影响。
6. 未携带 caller_request_id 的 DB 级创建（legacy/测试）行为与今日逐字节一致（R-1..R-4）。

### 5.7 Migration 与兼容策略（冻结）

1. **单一加法迁移** `0005_generation_job_caller_identity.sql`：列 + CHECK + 部分唯一索引 + `CREATE OR REPLACE guard_job_transition` + `DROP/CREATE create_generation_job_with_input`（先 DROP 0003 旧签名）+ 历史清点 NOTICE + 函数 REVOKE PUBLIC（沿用 0003 L937 惯例）。**不重写 0001–0004，不回填任何身份。**
2. **db-roles.sql 同步更新**：APP 的 create 函数 EXECUTE 授权改绑新签名；不新增角色；MAINTENANCE/WORKER 授权不变。
3. **切换策略（P2-4）**：不适用 ART-04E 9 步 ingress 停写 drain——前提是 F-3（create 函数无生产调用方，Job 行仅由测试与未来 AS-4 写入）。实施 PR 必须附证据：(a) 全仓 grep 证明无生产调用方；(b) 部署时 `pg_stat_activity` 检查无 in-flight create 事务；(c) 迁移前后 Job 行数一致。若实施时发现任何生产调用方出现，**停止并上报**，升级为完整 drain 流程。
4. **兼容矩阵**：unkeyed DB 创建行为不变（R-1..R-4）；`generation_job.input` 清理（7 天）不影响指纹比较（R-6）；AS-1 全部 57 项验收语义不受影响（回归门禁 G-1）。
5. `migrations.sha256` 与 migration_guard 测试按仓库既有机制更新。

---

## 6. 自动化验收矩阵（AS2-##，全部以真实 PostgreSQL sqlx::test 为准）

> 执行层级：**DB** = 对 0005 后的函数/约束直接集成测试（含并发与故障注入）；**HTTP** = lookup 路由与 API 契约测试；**SDK** = client 契约测试；**UNIT** = 指纹 golden/突变测试。实施 PR 必须逐项给出测试位置与执行结果；不得将未执行项标记为通过。

**A. 指纹与规范化（UNIT）**
| ID | 场景 | 预期 |
|---|---|---|
| AS2-01 | v1 golden vector 冻结（固定输入集） | hash 与冻结值一致，规则变更即失败 |
| AS2-02 | 语义字段逐项突变（tenant/project/artifact_id/generator_id/input 各字段/options） | 每次突变都改变指纹 |
| AS2-03 | 等价 JSON（键序/空白差异、options absent vs null） | 同一指纹 |
| AS2-04 | payload 内 Unicode 等价但字节不同（无 NFC） | **不同**指纹（规则冻结） |
| AS2-05 | 排除字段突变（Idempotency-Key、trace ID、expected_current_version_id、执行策略参数） | 指纹不变 |
| AS2-06 | 浮点/非有限数 | 非有限值校验拒绝；1e2 与 100.0 同指纹、100 与 100.0 不同指纹（golden 固化） |
| AS2-07 | 未知指纹版本 | fail-closed `UnsupportedVersion`，不静默按 v1 |

**B. 原子性与故障注入（DB）**
| ID | 场景 | 预期 |
|---|---|---|
| AS2-08 | 插入前异常回滚（约束/校验失败） | 无 Job、无身份残留；同 ID 重试可创建恰好一个 Job |
| AS2-09 | 事务提交后 HTTP 响应丢失 | 同 ID 同指纹重放返回同一 job_id（'caller_request_replay'） |
| AS2-10 | 创建事务中途 kill（tx abort） | 数据库无半行；重试收敛一个 Job |
| AS2-11 | Idempotency-Key 已在**其他** Job 上使用（不同 hash） | 409 IDEMPOTENCY_KEY_REUSED，且 caller 身份不被创建（无部分提交） |
| AS2-12 | generator unavailable 时创建 | 无 Job 无身份；恢复后同 ID 重试创建成功（404-then-POST 安全） |
| AS2-13 | 重放路径在 generator 被禁用后重试 | 仍返回 'caller_request_replay'（顺序冻结 P1-3），不是 503 |

**C. 并发收敛（DB）**
| ID | 场景 | 预期 |
|---|---|---|
| AS2-14 | N 并发同 ID 同指纹（tokio 并发调函数/端口） | 恰好 1 行；所有成功方同一 job_id； resolution ∈ {created, caller_request_replay} |
| AS2-15 | 并发同 ID 不同指纹 | 1 个胜者；其余 409 CALLER_REQUEST_ID_REUSED |
| AS2-16 | 并发同 key 不同 caller_request_id | 1 个胜者；另一方 409 IDEMPOTENCY_KEY_REUSED（raw hash 差异） |
| AS2-17 | 双索引竞争（同 ID 同 key 与不同 winner 组合） | 异常块按 caller 优先解析；无 500、无重复 |
| AS2-18 | unique_violation 后继续使用失败子事务 | 不存在此路径（代码审查 + N-1..N-5 通过佐证） |
| AS2-19 | 40001/40P01 注入 | 端口有界重试后成功或 503，不误报冲突 |

**D. 作用域与授权隔离（DB/HTTP）**
| ID | 场景 | 预期 |
|---|---|---|
| AS2-20 | 同 caller_request_id 不同 tenant/project | 各自独立创建，互不泄露 |
| AS2-21 | 无项目授权者调用 lookup | 403，不泄露存在性 |
| AS2-22 | lookup 跨 scope UUID | 404（不泄露） |
| AS2-23 | lookup 非法路径参数 | 400 稳定原因码 |
| AS2-24 | lookup 与 POST 创建并发 | 提交后 lookup 必达；提交前 404 + 重试收敛（N-6 联测） |

**E. 兼容与回归（DB）**
| ID | 场景 | 预期 |
|---|---|---|
| AS2-25 | unkeyed 创建（caller 三列 NULL） | 行为与 0003 语义逐项一致（同 key 同 hash 重放 / 异 hash 409 / 无 key 正常创建） |
| AS2-26 | 历史 Job（NULL 身份）经 lookup | 任何 UUID 均 404 |
| AS2-27 | 同 key 重放但换新 key + 同 ID 同指纹 | 'caller_request_replay'；原 Job 的 key/hash 不被改写 |
| AS2-28 | 同 key 不同 body hash（含 caller 字段差异） | 409 IDEMPOTENCY_KEY_REUSED（现有语义不变） |
| AS2-29 | 0003 重复键清洗语义（空/重复 key 置 NULL） | 0005 不触碰；既有迁移不变量测试通过 |
| AS2-30 | 终态输入正文清理（purge_expired_generation_job_input）后同 ID 重放 | 指纹比较不受影响（存储 hash 判定） |
| AS2-31 | AS-1 全量回归（artifact keyed/unkeyed、idempotency 清理、角色分离） | AS-1 57 项语义全部保持 |

**F. 保留与不可复用（DB）**
| ID | 场景 | 预期 |
|---|---|---|
| AS2-32 | Job → failed/cancelled/timed_out 后身份 | 身份列原值保留；同 ID 指纹重放仍解析原 Job；不产生新 Job |
| AS2-33 | 直接 SQL UPDATE 身份列（APP 角色模拟） | 触发器拒绝（M-2 佐证） |
| AS2-34 | NULL→非 NULL 身份补录 | 触发器拒绝（无 adoption 流程） |
| AS2-35 | 身份列存在期间唯一性 | 数据库层不可复用（部分唯一索引证明） |

**G. HTTP / OpenAPI / SDK 契约（HTTP/SDK/CONTRACT）**
| ID | 场景 | 预期 |
|---|---|---|
| AS2-36 | lookup 命中（已建 Job） | 200 + GenerationJob（含 caller_request_id）+ poll_url 可用 |
| AS2-37 | OpenAPI parity：DTO ↔ schema 双向（含 required caller_request_id、可空响应字段、resolution 头、新路由、409 码） | contract_openapi 测试通过 |
| AS2-38 | 错误码入库 | `CALLER_REQUEST_ID_REUSED`、`CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH` 映射与文档齐全 |
| AS2-39 | SDK `get_generation_job_by_caller_request` | 命中 200 / 404 / 403 / 传输错误分类正确 |
| AS2-40 | SDK 请求类型 | caller_request_id 必填序列化；GenerationJobResponse 反序列化含可空 caller_request_id |
| AS2-41 | HTTP create 路由现状 | 仍 fail-closed 503 GENERATOR_UNAVAILABLE（无启用）；不因 AS-2 改变 |

**H. 权限与迁移不变量（DB）**
| ID | 场景 | 预期 |
|---|---|---|
| AS2-42 | APP 可 INSERT generation_job（经函数）但触发器拦截身份改写 | 通过 |
| AS2-43 | 新 create 函数 EXECUTE 仅 APP；PUBLIC/WORKER/MAINTENANCE 无 | 角色分离测试通过 |
| AS2-44 | 部分唯一索引/CHECK/触发器存在性迁移不变量 | migration_guard 扩展通过 |
| AS2-45 | Job 创建事务不写 idempotency_record | 断言该表行数在 Job 创建前后不变（Q8 固化） |
| AS2-46 | 迁移证据：迁移前后 Job 行数一致、无身份回填 | 部署证据清单 |

> 共 46 项自动化验收（含并发、故障注入、真实 PostgreSQL 集成）。AS2-37/41 明确划出 AS-4 接线门禁，避免"文档规格通过"被误述为"HTTP 实现通过"。

---

## 7. AS-2 与 AS-3 的严格边界

### 7.1 AS-2 交付（本契约 §5 全集）
1. `caller_request_id` 持久化（列 + 约束 + 触发器不可变）；
2. canonical request fingerprint v1（§5.2，含 golden vectors）；
3. `(tenant_id, project_id, caller_request_id)` 作用域唯一；
4. 原子创建与并发收敛（§5.3：函数 v2 + 专用端口）；
5. caller ID 精确 lookup（§5.4 路由 + Repository 方法）；
6. Idempotency-Key 与 durable identity 分离（§5.6）;
7. HTTP/OpenAPI/SDK 契约支持（§5.5：DTO、OpenAPI、SDK 类型与 lookup 方法）；
8. 迁移 0005 + db-roles 更新 + §6 验收矩阵全量证据。

**明确不在 AS-2（违规即打回）**：
9. `request_generation` 应用路径接线与 HTTP 202/200 全链路（AS-4）；
10. SDK `get_generation_job`/`cancel_generation_job`（AS-4）；
11. 服务器端接受/校验 `expected_current_version_id` 前置条件（AS-3；AS-2 保持该 DTO 字段原样忽略）；
12. `target_version_id` 的分配、不可变约束、部分唯一索引、响应暴露（AS-3）；
13. Generator 启用、Worker 修改、LMMS/File Service、SeeCode Adapter（独立 P0 门禁）。

### 7.2 AR04-02 单独结论：NOT APPROVED / DEFERRED
AR04-02（exact Job-to-Version provenance）**不在本次批准范围**，理由：
1. 其核心不变量依赖 AS-3 的"创建时原子分配 target Version"设计，后者尚未评审冻结；
2. 完成事务（complete_generation_job_attempt 校验 READY Version + Output SHA）无法在 SeeCode 不可用（F-2/F-13 现状）下针对真实端到端生成验证；
3. ART-04A §3 的规格本身（部分唯一索引、succeeded⇒non-null、READY 校验、快照语义）方向合理，但必须随 AS-3 设计文档独立评审，**不得因 AR04-01 通过而默认通过**。
AS-2 PR 中出现任何 `target_version_id` 暴露/分配/约束代码即为越界，评审拒绝。

---

## 8. 实施顺序与门禁

1. **契约冻结**：以本文件 §5 为 AS-2 唯一实施规格；AS-2 PR 附 ART-04A §2.2 修订（P1-1）与 §2.3 机制映射说明（P2-1）。
2. **Phase 1** 迁移 0005（列/约束/索引/触发器/函数 v2/清点）+ db-roles 更新 + 迁移不变量测试。
3. **Phase 2** 指纹 v1 实现 + golden vectors（UNIT）。
4. **Phase 3** 端口 `KeyedGenerationJobCreatePort`（PostgreSQL 适配器）+ 并发/故障注入 DB 测试。
5. **Phase 4** DTO/OpenAPI/SDK/lookup 路由 + 契约测试。
6. **Phase 5** §6 矩阵 46 项全量执行 + AS-1 回归（AS2-31）+ 独立审查（对齐 ART-04E §11 先例）。
7. PR 说明必须附：本评审文档、全部验收项状态、未解决 P0/P1 标记（目标为无）、迁移证据（§5.7.3）、Draft 状态提交。

**当前状态：AS-2 实施尚未获授权。** ART-04H 已裁定 CHANGES REQUIRED；本文件 §9 为针对该裁决的规范性修订稿，须经独立复核确认后，才可将 AS-2 标记为 implementation-ready 并另行启动实施。任何情况下均不解除既有门禁：不启用生成、不动 Worker、不触碰 LMMS/File Service、不合并 PR #8、不启动 AS-3/AS-4/PM-1..PM-3。

---

## 9. ART-04H 挑战后规范性修订（v2，优先级高于 §§5–8 的冲突表述）

> 修订依据：ART-04H-independent-challenge-review-as2-frozen-contract.md。
> 效力：本节是 ART-04G §5 的组成部分。若本节与 §§5–8 任何旧文字冲突，以本节为准；实现者不得只阅读旧 §5。此修订不改变 AS-3 / AS-4 边界，也不构成代码实施授权。

### 9.1 C-1（P1）：caller 命中后的 Idempotency-Key 冲突硬门禁

冻结原则：caller identity 用于确定候选 Job；它无权静默吞掉一个已绑定到其他 Job 的 Idempotency-Key 冲突。身份冲突优先于 key 冲突；但任何成功 replay 返回前，都必须验证当前 key 没有与其他 Job 发生冲突。

将 §5.3.1 原 step 2 替换为以下逻辑（变量名须与最终 SQL 签名一致）：

    -- caller identity 读前查：先判定持久身份是否冲突
    IF p_caller_request_id IS NOT NULL THEN
        SELECT * INTO v_existing
          FROM artifact.generation_job
         WHERE tenant_id = p_tenant_id
           AND project_id = p_project_id
           AND caller_request_id = p_caller_request_id;

        IF FOUND THEN
            IF v_existing.fingerprint_version <> p_fingerprint_version THEN
                RAISE EXCEPTION USING ERRCODE = 'P0001',
                    MESSAGE = 'CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH';
            END IF;
            IF v_existing.request_fingerprint <> p_request_fingerprint THEN
                RAISE EXCEPTION USING ERRCODE = '23505',
                    MESSAGE = 'CALLER_REQUEST_ID_REUSED';
            END IF;

            -- 指纹一致：成功返回前执行 key 冲突硬门禁
            IF v_idempotency_key IS NOT NULL THEN
                SELECT * INTO v_holder
                  FROM artifact.generation_job
                 WHERE tenant_id = p_tenant_id
                   AND project_id = p_project_id
                   AND idempotency_key = v_idempotency_key;

                IF FOUND AND v_holder.id <> v_existing.id THEN
                    IF v_holder.request_hash <> p_request_hash THEN
                        RAISE EXCEPTION USING ERRCODE = '23505',
                            MESSAGE = 'IDEMPOTENCY_KEY_REUSED';
                    END IF;
                    RAISE EXCEPTION USING ERRCODE = 'P0001',
                        MESSAGE = 'IDEMPOTENCY_KEY_CROSS_IDENTITY';
                END IF;
            END IF;

            RETURN QUERY SELECT v_existing.id, 'caller_request_replay';
            RETURN;
        END IF;
    END IF;

    -- caller 未命中：继续既有 Idempotency-Key 读前查逻辑。
    -- 同 key + 同 raw request_hash => idempotency_replay；
    -- 同 key + 不同 raw request_hash => IDEMPOTENCY_KEY_REUSED。

异常块 step 7 必须执行同一门禁：unique_violation 子事务回滚后，以新语句快照重读 caller。若命中，依次检查 fingerprint_version、request_fingerprint，再对当前请求 Idempotency-Key 执行与读前查完全相同的 holder 检查；key 绑定他行且 raw request_hash 不同，必须返回 IDEMPOTENCY_KEY_REUSED，不得 replay。caller 未命中时，保留原有按 Idempotency-Key 重读逻辑。step 2 与 step 7 的 key 门禁必须共用同一 SQL helper 或等价实现，避免逻辑漂移。

优先级冻结：
- caller 命中但 fingerprint/version 冲突：caller 冲突优先；
- caller 命中且 fingerprint/version 一致，但 key 绑定其他 Job 且 raw hash 不同：Idempotency-Key 冲突；
- caller 命中且 key 未绑定、绑定自身，或请求未带 key：允许 caller replay；
- caller 未命中：继续原 Idempotency-Key 解析；之后才检查 generator availability。
同 caller + 未绑定新 key、同 caller + 无 key 的恢复语义不变。Lookup 不受 Idempotency-Key 影响。

### 9.2 C-2（P2）：Fingerprint 版本演进与失败关闭

1. AS-2 v1 实施时，数据库函数比较传入 fingerprint_version 与存储版本；不相等即失败关闭。v1-only 阶段该分支是完整性防御，不是跨版本恢复机制。
2. 版本不匹配映射为 5xx 完整性错误，不可自动重试；调用方应改用 caller lookup 获取既有 Job，并告警/人工处理，不得更换 caller_request_id 绕过。
3. 若未来引入 v2，Rust 端必须保留所有仍有存活 Job 使用的 canonicalizer，并按存储行的版本重算后比较；版本升级前必须有独立迁移/兼容设计与验收。移除仍被存活记录引用的 canonicalizer 属于 P0 完整性事件。
4. 当前 v1 SQL 函数在数据库内比较版本，不能声称它已支持“按存储版本重算”；该能力仅是未来版本升级必须实现的规则。

### 9.3 C-3（P2）：Canonical JSON 的实现约束与三链路差异

1. v1 canonicalizer 使用固定版本 serde_json 默认 Map 排序语义；必须禁用 preserve_order 与 arbitrary_precision。若未来依赖树改变这些特性，必须显式对所有 payload object key 按 Unicode code point 顺序重排，并通过全部 golden vectors；不得静默改变 v1 输出。
2. payload 字符串按解析后的 Unicode 字符串内容原样编码，不做 NFC/NFD 归一化；数组保序；对象键排序；DTO 层 absent options 等同 null，但 payload 内部 absent 与 null 保持区分；非有限数拒绝。数字行为以锁定的 serde_json 版本和 golden vectors 为准：1e2 与 100.0 同指纹，整数 100 与浮点 100.0 不同指纹。
3. request_fingerprint、request_hash、input_sha256 是三条独立链路。1e2 与 100.0 可产生相同语义 fingerprint，但经 JSONB 保存/渲染后产生不同 input_sha256；这不是矛盾，不得把其中任一 hash 替代另一条链路。

### 9.4 C-4（P2）：指纹写入信任边界与 DB 权限

1. Rust typed DTO/canonicalizer 是 request_fingerprint 内容正确性的唯一计算与信任边界；DB 负责配对 CHECK、格式 CHECK、作用域唯一性、不可变性和事务收敛，不验证语义 fingerprint 是否由 payload 正确计算。
2. 因 APP 当前对 generation_job 具有表级 INSERT/UPDATE，实施 PR 必须明确回答是否将 APP 的直接 INSERT/UPDATE 收窄为仅通过受控 SECURITY DEFINER create function 写入/更新。优先方案是撤销 APP 对该表的直接 INSERT/UPDATE，只保留业务实际需要的最小列级/函数级权限；但不得在未盘点所有 repository、worker、测试和维护路径前盲目撤权。
3. 必须证明生产写入路径不能绕过 Rust 指纹计算；任何保留的直接 SQL 写入路径必须有明确调用者、最小权限、测试和审计证据。AS2-42 应验证经受控函数可创建，并验证 APP 直连 INSERT 按最终权限策略被拒绝或严格受控，不能只验证触发器拦截 UPDATE。

### 9.5 C-5（P2）：SQLSTATE 与稳定消息 token 的错误映射

1. SQL 函数为新增业务错误显式指定稳定 message token 与 SQLSTATE，不得依赖 PostgreSQL 默认异常码。
2. 冻结映射：
   - CALLER_REQUEST_ID_REUSED：SQLSTATE 23505；
   - IDEMPOTENCY_KEY_REUSED：SQLSTATE 23505；
   - CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH：SQLSTATE P0001；
   - IDEMPOTENCY_KEY_CROSS_IDENTITY（结构不变量防御）：SQLSTATE P0001；
   - 输入/格式校验错误：SQLSTATE 22023，沿用现有校验语义。
   现有 GENERATOR_UNAVAILABLE / UNKNOWN_GENERATOR token 与 SQLSTATE 语义不得因本修订改变。
3. Rust 端口错误分类必须 token-first：先识别稳定 message token，再看 SQLSTATE。否则现有分类器可能把带 P0001 的 GENERATOR_UNAVAILABLE 归入 IntegrityFailure，或把带 23505 的业务冲突误归入可重试错误。
4. 仅未命中任何已知业务 token 的 23505、40001、40P01 才进入有界重试/并发冲突分类；其余按完整性/未知错误处理。不得将所有 23505 一律 Retry。

### 9.6 验收矩阵增补：总数修正为 52 项

ART-04H 的“46 → 51”总数存在算术错误：原 AS2-28 拆成 28a/28b 净增 1 项，再新增 AS2-47..AS2-51 共 5 项，故总数为 52 项（不是 51）。以下项目纳入 §6 的强制验收矩阵：

| ID | 场景 | 预期 |
|---|---|---|
| AS2-28a | caller_request_id 未命中时，同 key + 不同 raw body hash | 409 IDEMPOTENCY_KEY_REUSED |
| AS2-28b | caller_request_id 命中且 fingerprint 一致，但当前 key 绑定其他 Job 且 raw hash 不同 | 409 IDEMPOTENCY_KEY_REUSED |
| AS2-47 | Job A(caller A, K1) 与 Job B(caller B, K2) 已存在；POST caller A + F_A + K2 | 409；无新行；A/B 的 key/hash/fingerprint 均不变 |
| AS2-48 | 与 AS2-47 对称：POST caller B + F_B + K1 | 409；无新行；两 Job 均不变 |
| AS2-49 | 并发混合提交 {caller A + F_A + K2} 与 {caller B + F_B + K1} | 两请求均不得成功 replay；按已提交 key holder 返回 409；无行/身份/key 改写 |
| AS2-50 | unique_violation 异常块 caller 命中且指纹一致，同时当前 key 竞争性绑定到其他 Job | 异常块必须执行同一 key 门禁并返回 409，不得 replay |
| AS2-51 | token-first 映射：GENERATOR_UNAVAILABLE 使用既有 P0001 时 | 映射为现有 503 业务语义，不得落入 IntegrityFailure |

原 AS2-28 删除并由 28a/28b 取代；AS2-01..27、AS2-29..46 保持。实施 PR 必须按 52 项逐项编号、给出测试位置与执行证据；未执行不得标记 PASS。

### 9.7 迁移与实施前新增检查

1. §5.7 的“无 ingress drain”仅在 F-3 仍成立时有效；实施 PR 必须重新验证没有生产调用方，并检查迁移窗口内无 in-flight create transaction。任何生产调用方出现即停止并升级完整 drain。
2. APP 对 generation_job 的直接 INSERT/UPDATE 权限必须按 §9.4 做调用路径盘点与最小权限裁决；不得把“触发器保护 UPDATE”误写成“指纹内容不可伪造”。
3. AS-2 实施 PR 必须附 AS2-01..AS2-52 逐项矩阵、真实 PostgreSQL 并发/故障注入证据、OpenAPI/SDK parity 证据、migration checksum 与角色权限证据；保持 Draft，等待独立代码评审。
4. 本修订不改变 AS-3 / AR04-02 的 NOT APPROVED / DEFERRED，也不允许合并 PR #8、启用 Generator、修改 Worker、LMMS/File Service 或 SeeCode。

### 9.8 修订后的冻结状态

ART-04H 的 C-1 P1 已在文档层给出明确修订方案；C-2..C-5 P2 已纳入规范性约束。但文档作者不能自行把独立挑战评审改判为 APPROVED：本版仅作为修订候选，必须由独立评审针对 §9 复核 C-1 的读前查/异常块并发语义、SQLSTATE/token 映射及权限信任边界。复核通过后，另行签发 AS-2 implementation-ready 冻结结论。
