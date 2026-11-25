# ART-04I — 第三轮独立挑战评审（ART-04G v2 / AS-2 Implementation Readiness）

> **Status:** 独立架构挑战评审结论 + AS-2 冻结与实施授权建议
> **Date:** 2026-11-25
> **Reviewer:** 独立架构评审（第三轮：对 ART-04G v2 §9 修订稿的反向验证；不因 ART-04H 已给出修订方案而默认修订正确）
> **Baselines:**
> - 评审分支 `docs/art-04g-as2-review-contract` HEAD = `5bf69b1dd98735bb280d148319cde5295edad472`（ART-04G v2，本轮评审对象）✓ 已核对
> - `main` = `c11e79a51a145ded026632630bb0b52aa44dc311` ✓ 未变
> - AS-1 `feat/as-1-durable-artifact-creation-identity`（PR #8 HEAD）= `5541d3be8c90cfcf9283b6e59426c8a3c2bfe099` ✓ 未变
> - ART-04H 报告提交 = `723fb3d`（本轮 diff 基点）
> **代码零变更证明:** `git diff 723fb3d..5bf69b1 -- ':!docs'` 输出为空——v2 修订区间仅改 ART-04G 文档（+129/-3）。因此 ART-04H 已逐条核实的 F-1..F-15 源码事实（文件+行号）在当前 HEAD 全部保持有效，本轮不重复罗列，仅增量引用。
> **约束遵守:** 未修改任何业务代码、Migration、OpenAPI、SDK；未创建 AS-2 实施分支；未修改/合并 PR #8；未启动 AS-3；未启用 Generator、Worker 或生产生成；未触碰 LMMS/File Service。唯一新增交付物为本文档。

---

## 1. 最终结论表

| 评审对象 | 结论 | 说明 |
|---|---|---|
| **ART-04G v2 文档质量** | **APPROVED（有条件）** | §9 对 C-1..C-5 的修订全部成立；须将 C-6（P2，本轮新发现）与 P3-1..P3-4 作为 v3 冻结修订并入，均为局部文本修订，无需第四轮全文评审，以 diff 复核收尾 |
| **C-1 P1 修复** | **APPROVED** | §9.1 读前查门禁 + 异常块同构门禁经本轮并发时序逐项证明：所有并发结果均等价于某个合法串行序，无可利用仲裁缺口。残余 C-6 为非规范 key 格式的门禁漏洞（P2，冻结前必须并入） |
| **C-2..C-5** | **APPROVED** | §9.2/§9.3/§9.4/§9.5 逐条复核通过，含本轮新增的写入路径盘点证据 |
| **AS-2 技术设计** | **APPROVED** | 数据模型、指纹 v1、并发方案、错误映射、权限信任边界、验收矩阵整体成立 |
| **AS-2 代码实施授权** | **APPROVED（条件执行）** | 条件：C-6 + P3 清单并入 v3 冻结版并 diff 复核后，方可创建实施分支、按 Phase 1..6 启动。本授权不解除任何既有门禁 |
| **AS-3 / AR04-02** | **维持 NOT APPROVED / DEFERRED** | §9.7.4 未改变边界；无新证据推翻 |

**缺陷统计：P0 = 0；P1 = 0；P2 = 1（C-6，冻结前必须并入）；P3 = 4。**

按本轮评审原则（P0=0、P1=0、且 C-1 并发仲裁得到可证明解决方可建议 implementation-ready）：三项条件全部满足，**建议 AS-2 在 v3 冻结修订并入后进入 implementation-ready**。C-6 定级论证见 §5——它不是架构/并发正确性缺陷（无可利用的并发窗口、无数据损坏、无错误 Job 返回），而是冻结 SQL 文本中 key 规范化的机械化缺口，已给出一行级修订。

---

## 2. 基线核对

| 对象 | 任务书声明 | 实际核对 | 一致 |
|---|---|---|---|
| ART-04G v2 提交 | `5bf69b1` | `5bf69b1dd98735bb280d148319cde5295edad472` | ✓ |
| 修订区间 | — | `723fb3d..5bf69b1` 仅改 `docs/architecture/ART-04G-...md`（+129/-3），docs 外零变更 | ✓ |
| main / AS-1 | — | `c11e79a` / `5541d3b` 未变 | ✓ |

**F-1..F-15 一致性:** 当前源码树与 ART-04H 核验时完全相同（同 HEAD 代码层），F-1..F-14 与 ART-04G §2 声明一致、F-15（serde_json 默认特性）成立，无任何不一致项。关键证据沿用：`generation_job` schema（`migrations/0001_init.sql` L313-348）、key 部分唯一索引（`migrations/0003_job_coordination_fixes.sql` L139-141）、创建函数（0003 L271-389，GENERATOR 检查 L325-333 先于 key 解析 L341-355）、`guard_job_transition`（0001 L350-370）、`request_generation`（`crates/artifact-application/src/service/artifact.rs` L574-623）、HTTP fail-closed 503（`apps/artifact-api/src/routes/artifacts.rs` L486-529）、`Scoped` 授权（`apps/artifact-api/src/auth.rs` L52-85）、DTO/SDK/OpenAPI（`crates/artifact-contract/src/dto.rs` L196-221、`crates/artifact-client/src/lib.rs` L320-332、`openapi/openapi.yaml` L383-470/L771-813）、DB 角色（`deploy/db-roles.sql` L50-96）、AS-1 端口与分类器（`crates/artifact-infrastructure/src/postgres/keyed_create.rs` L45-59、L189-195、L222-235）、指纹版本分派（`crates/artifact-application/src/service/creation_fingerprint.rs` L29-49）。

---

## 3. 核心挑战一：C-1 并发仲裁严格反向验证

### 3.1 §9.1 与 ART-04H Option C 的对应核对

§9.1 完整落地了 ART-04H §3.4 的方案：caller 读前查命中后依次检查 version → fingerprint → **key holder 硬门禁**；`v_holder.id = v_existing.id`（绑定自身）放行；绑定他行 + raw hash 不同 → `IDEMPOTENCY_KEY_REUSED`；绑定他行 + hash 相同 → `IDEMPOTENCY_KEY_CROSS_IDENTITY`（fail-closed 防御）；优先级冻结（身份冲突 > key 冲突 > replay 成功）与 ART-04H 状态组合表 1/3/4/5/9 行为逐行一致；异常块 step 7 被要求执行同一门禁且**共用同一 SQL helper**。额外修正了 ART-04H 未指定的 ERRCODE（见 §4.4）。**结构对应：通过。**

### 3.2 普通请求路径七问

| # | 问题 | 裁决 | 依据 |
|---|---|---|---|
| 1 | caller 未命中时保持原 key 语义？ | ✓ | §9.1 明文"caller 未命中：继续既有 Idempotency-Key 读前查逻辑"，step 3 原文逐字保留；P1-3 顺序（dedup 先于 generator 检查）保持 |
| 2 | caller 命中、指纹一致时必然检查当前 key？ | ✓（附 C-6） | 门禁在 `p_idempotency_key IS NOT NULL` 时必然执行——但使用**原始参数**而非规范化 key，非规范格式存在缺口（§5 C-6） |
| 3 | key 未绑定时允许新 key 恢复？ | ✓ | gate SELECT 未命中 → replay（合法轮换恢复，AS2-27 语义不变） |
| 4 | key 绑定自身时允许 replay？ | ✓ | `v_holder.id = v_existing.id` → 门禁放行 |
| 5 | key 绑定他行 + raw hash 不同 → 必然 409？ | ✓ | 已提交状态下单条 SELECT 语句快照必然可见（READ COMMITTED 语句级快照，见 3.3 论证 1），确定性 409 |
| 6 | key 绑定他行 + raw hash 相同 → 结构不变量冲突识别？ | ✓ | `IDEMPOTENCY_KEY_CROSS_IDENTITY` P0001 fail-closed；该分支结构上不可达（同 hash ⇒ 同 body ⇒ 同 caller_request_id ⇒ 同一行），保留防御正确 |
| 7 | caller fingerprint 不同时先返回 caller 冲突？ | ✓ | 优先级冻结：version → fingerprint 检查先于 key 门禁（避免身份冲突被 key 冲突误报，误导恢复算法） |

### 3.3 并发时序逐项证明（六时序 + 快照论证）

**关键问题：§9.1 的读前查 holder 检查是否真正具有事务安全性？**
**结论：具有。** 以下给出每个时序的 SQL 级时间线、唯一索引竞争点、快照行为与最终结果，并证明所有结果均等价于某个合法串行序。

**时序 A：预置交叉（AS2-47/48/49 基线，全确定性）**

预置（均已提交）：Job A=(caller A, F_A, key K1, hash H_A)、Job B=(caller B, F_B, key K2, hash H_B)。并发发送 R1={caller A, F_A, K2} 与 R2={caller B, F_B, K1}：

```
R1: step2a SELECT caller A → 命中(已提交) → 指纹一致
    gate  SELECT key K2 → 命中 Job B(已提交), v_holder.id≠v_existing.id, hash H_B≠H_A
    → RAISE IDEMPOTENCY_KEY_REUSED (409)                      [确定性]
R2: 对称 → 409                                                [确定性]
```
无 INSERT 参与，无竞态面。**AS2-47/48/49 的预期（双 409、无行改写）在此状态下是确定的。**

**时序 B：replay 与并发 create 同 key 交错（任务书 5.2.4）**

预置：Job A=(caller A, F_A, key K1) 已提交；caller B 不存在；key K2 未绑定。

```
T1 = R{caller A, F_A, K2}（replay 路径）     T2 = R{caller B, F_B, K2}（create 路径）
─────────────────────────────────────     ─────────────────────────────────────
step2a: SELECT caller A → 命中 A
  指纹一致
gate: SELECT key K2 → 未命中
                                          step2a: SELECT caller B → 未命中
                                          step3: SELECT key K2 → 未命中
RETURN (A, 'caller_request_replay')
(T1 事务结束——replay 路径零写入)
                                          step6: INSERT {caller B, F_B, K2}
                                            → caller/key 索引均无冲突 → commit
                                            → Job B 绑定 K2
```

- **结果:** T1 → 200 replay A；T2 → 202 created B。
- **仲裁判定:** 该结果**逐语句等价于串行序 (T1 → T2)**——T1 门禁评估时 K2 确实未绑定，T1 未执行任何绑定动作（replay 零写入），T2 随后独占绑定 K2。若交错反向（T2 先 commit），T1 的 gate 语句快照必见 K2→B（论证 1）→ T1 得 409，等价于串行序 (T2 → T1)。
- **不变量核对:** "一个 key 至多绑定一个 Job" ✓（K2 仅绑定 B）；"同 key 双 create 必然一胜一 409" ✓（由唯一索引仲裁，见时序 C）；T1 返回的是自己身份对应的 Job，无错误 Job、无跨身份泄露。
- **判定:** benign。这不是违反幂等语义的成功结果，而是两个合法串行序之一。**若在此判 P1，等于要求 READ COMMITTED 提供可串行化保证——超出系统声明的隔离模型（§5.3.2 冻结 READ COMMITTED）。**

**时序 C：双 create 同 key（AS2-16 保持）**

T1={caller C(新), F_C, K2}、T2={caller D(新), F_D, K2}。双方读前查均 miss → 双 INSERT → key 索引竞争：PostgreSQL 中后插者在索引上等待先插者事务结局；先者 commit → 后者 `unique_violation 23505`（若先者 abort → 后者照常成功插入，无违例）→ 异常块子事务回滚 → caller 重读 miss（D≠C）→ key 重读命中胜者，hash 不同（body 含不同 caller）→ **409 IDEMPOTENCY_KEY_REUSED** ✓ 与 0003 现行为完全一致。

**时序 D：同 caller 并发双提交（AS2-14 保持）**

T1、T2 同为 {caller C(新), F_C, key Ki}（key 各自独立）。caller 索引仲裁：胜者 commit；败者 `unique_violation` → caller 重读命中胜者 → 指纹一致 → 门禁：胜者 key 绑定在胜者自身（`v_holder.id = v_existing.id`）或 Ki 未绑定 → **replay** ✓ resolution ∈ {created, caller_request_replay}，无重写。

**时序 E：异常块 caller 命中 + key 竞争性绑定他行（AS2-50）**

预置：无 Job A；Job B=(caller B, K2) 由 T2 并发创建中；T1={caller A(新), F_A, K2}，T3={caller A, F_A, K5} 并发创建中。

```
T1: step2a SELECT caller A → miss
    step3 SELECT key K2 → miss (T2 未提交)
    step6 INSERT {caller A, F_A, K2}
      → caller 索引上等待 T3 的 in-flight 插入
                                T3: commit → Job A3={caller A, K5}
    → unique_violation(23505) [caller 索引]
    异常块: 子事务回滚 → caller 重读(新语句快照) → 命中 A3
      → 指纹一致 → 门禁: SELECT key K2
        → 若 T2 已提交: 命中 Job B, id≠A3.id, hash 不同
          → RAISE IDEMPOTENCY_KEY_REUSED (409) ✓ 门禁在异常分支生效
        → 若 T2 尚未提交: 门禁未命中 → replay A3
          （等价于串行序 "T3→T1 先于 T2"，benign，与时序 B 同构）
```

**判定:** T2 提交先于 T1 门禁语句 ⇒ 确定性 409（门禁在异常分支被证明生效）；否则落回 benign 合法串行序。**无可利用缺口。**

**时序 F：异常块 caller 命中且胜者即 key 持有者**

T1={caller A(新), F_A, K2} INSERT 在 key 索引上败给并发 T4={caller A(新), F_A, K2}（同身份同 key 双发）。异常块 caller 重读命中 T4 胜者行，指纹一致 → 门禁：K2 的 v_holder 即胜者自身 → 放行 → **replay 胜者** ✓ 双发收敛同一 Job。

**四条快照/仲裁论证（可证明性核心）：**

1. **门禁读不存在"已提交绑定不可见"的窗口。** READ COMMITTED 下每条语句在语句开始时取新快照，包含此前全部已提交事务；门禁是单条 SELECT，评估时可见所有已提交绑定。反向（gate 判 409 但绑定其实已存在）不可能发生。
2. **replay 路径零写入 ⇒ 无约束冲突面。** caller 命中后的返回不执行任何 INSERT/UPDATE，事务内后续不可能产生唯一约束冲突；唯一残留窗口是"gate 通过后、事务结束前他人并发绑定"，其结果等价于合法串行序（时序 B 证明），不产生双绑定、不产生错误 Job。
3. **create-vs-create 竞争全部由唯一索引仲裁。** PostgreSQL 唯一索引的插入-等待-违例机制保证：违例发生 ⇔ 竞争者已 commit（abort 则无违例）；异常块每条重读语句取违例后新快照，必然看到胜者。
4. **状态单调性使重读权威。** 身份列/指纹/key/hash 由触发器不可变（0005 骨架）且无删除路径（F-5：Attempt FK RESTRICT、purge 只清正文）——绑定只增不改不消，重读结果无"过期"问题。

**C-1 裁决：APPROVED——§9.1 的门禁在并发下可证明正确，不需要额外锁或约束仲裁。**

### 3.4 异常块六问

| # | 问题 | 裁决 | 依据 |
|---|---|---|---|
| 1 | 异常块新语句是否获得正确新快照？ | ✓ | READ COMMITTED 语句级快照；子事务回滚后的每条 SELECT 均在违例点之后取快照，胜者必然可见（论证 3） |
| 2 | 是否可能读到 caller winner 但 key holder 已变化？ | 仅单调"增加" | 绑定不可变、无删除（论证 4）：holder 只会在后续语句中**新增可见**（更严，仍正确），不可能消失；"gate 后新增绑定"= benign 合法串行序（时序 B/E） |
| 3 | 检查 key 后、返回 replay 前是否可能再冲突？ | 否 | replay 返回无任何写入，无冲突面（论证 2） |
| 4 | 两个不同唯一索引的异常能否区分？ | 无需区分 | 异常块以"caller 重读 → key 重读"顺序处理：caller 索引违例 ⇒ caller 重读命中；key 索引违例且无 caller 匹配 ⇒ caller 重读 miss → 落入原 key 重读逻辑；双索引同时违例 ⇒ caller 重读命中即覆盖。约束名判别不必要 |
| 5 | SQLSTATE 23505 是否足以表达业务冲突？ | 否，token-first 必要 | 23505 同时承载真实唯一违例与业务冲突（0003 L353 先例）；§9.5 冻结 token-first 是唯一正确的消歧方案 ✓ |
| 6 | 是否会把结构性错误误判为业务冲突？ | 否 | token 为 RAISE 固定字面量（无 format()/用户输入拼接），引擎错误消息不可能与 token 精确匹配 → 未命中 token 的 23505 进入有界重试（残留路径仅"违例后双 miss"，结构上不可达，重试后 503 可接受） |

### 3.5 最终仲裁方案确认

无需新增机制。冻结方案 = **唯一索引仲裁（create 路径）+ 语句快照门禁（replay 路径）+ 异常块同构门禁（共用 helper）+ 不可变/无删除支撑的重读权威性**。该组合的正确性由 §3.3 四条论证覆盖全部并发交错。任何"增加重试""增加锁"的加强都属于对 benign 窗口的过度工程，反而会引入死锁/串行化风险面。

### 3.6 对"key holder 查询 ≠ 原子仲裁"质疑的正式裁决（TOCTOU 三问）

> 本节回应如下质疑：§9.1 增加了 key holder 查询，但普通 SELECT 本身不等于原子仲裁——必须证明 (i) 检查 key 时没有竞争者；(ii) 检查完成后不会有另一事务抢先绑定 key；(iii) 即使发生唯一约束竞争，最终也不会错误返回 caller replay。若普通查询存在 TOCTOU 竞态，应继续修订而非启动 AS-2。

**先承认事实，再证明无害：普通 SELECT 的 TOCTOU 窗口存在**（gate 语句通过后、事务结束前，另一事务可提交 key 绑定——§3.3 时序 B 已如实记录）。争议点不在"竞态是否存在"，而在"竞态可达的结果是否违反冻结不变量"。以下逐问裁决。

**问 (i)："检查 key 时没有竞争者"是否成立？**

绝对意义的"无竞争者"在任何隔离级别都**不可观察**——在途（未提交）事务对一切读取不可见，这是数据库系统的普遍边界，不是本方案弱点。需要证明且可证明的是更强的一条：**任一潜在竞争者在 gate 语句时刻的三种状态，全部收敛到合法结局**（状态穷举）：

| 竞争者状态（相对 gate 语句） | 可见性 | 结局 |
|---|---|---|
| 已提交 | READ COMMITTED 语句快照（语句开始时取）必然包含其绑定 | gate 命中 → 409（确定性，不可逃逸） |
| 在途，之后提交 | gate 不可见（合法） | 落入问 (ii) 的窗口 → benign 串行等价（见下） |
| 在途，abort / 未开始 | 绑定从未存在 | 无影响 |

**问 (ii)："检查完成后不会被抢先绑定"是否成立？**

不成立——窗口内**可以**被抢先绑定。但证明其结果空间与合法串行序完全重合，故不构成缺陷：

1. **replay 路径零写入**（§3.3 论证 2）：T1 不触碰任何索引/约束；K2 的绑定权完全由抢先者 T2 自身的 INSERT 经唯一索引仲裁（T2 对所有其他 create 竞争者而言仍是唯一胜者）。K2 最终**至多绑定一个 Job**——单绑定不变量在一切交错下保持。
2. **结果状态空间 = 两个合法串行序的并**：抢先发生于 gate 后 ⇒ 结局逐语句等价于串行序 (T1;T2)——"replay 在前、绑定在后"，任何正确实现（包括逐请求串行执行的系统）都会产出同样结局；抢先发生于 gate 前 ⇒ 等价于 (T2;T1) ⇒ T1 得 409。该并发交错连 **SERIALIZABLE** 隔离也允许（T1→T2 为 rw 反依赖，无环），即它根本不是隔离性弱点，而是合法串行序的直接体现。
3. **"双成功"不违反幂等语义**：幂等语义要求的是"同 key 的两次 *create* 必然一胜一 409"（唯一索引保证）与"key 至多绑定一个 Job"（保持）。时序 B 中 T1 根本不是 create、不绑定任何 key——它是自己持久身份的只读重放。按任务书自身的 P1 判据（"随后产生**违反幂等语义的成功结果**"），该结局不含违规结果，不构成 P1。
4. **彻底关闭该窗口的唯一方法是让 replay 路径绑定/预留 key**——被冻结契约本身禁止：AS2-27"原 Job 的 key/hash 不被改写"、Q7/Q10 replay 收敛只读性。`pg_advisory_xact_lock` 类加锁**不改变结局集合**（锁序本身就是时序，两种结局依然按取锁顺序出现），只为只读路径引入跨事务耦合与延迟，零语义收益。故"继续修订"在此方向上没有可实施的更优解；任何强行关闭窗口的修订都等于推翻已冻结的 replay 语义。

**问 (iii)："唯一约束竞争下不会错误返回 caller replay"是否成立？**

成立。定义**错误 replay** = 响应为 replay(A) 且 gate 语句快照内已存在"key 绑定他行且 raw hash 不同"。由 §3.3 四引理排除：违例蕴含胜者已提交（引理 1/3）→ 异常块 gate 语句快照必然包含该绑定（引理 2）→ 绑定集合单调不可消失（引理 4）→ replay 返回前零写入、无新增违例面（引理 2）。对"违例 + caller 命中 + 门禁"的终态穷举（§3.3 时序 D/E/F）：holder=胜者自身 → replay 正确收敛；holder=他行且 gate 前已提交 → 409；holder 在 gate 语句后才出现 → benign 串行等价；holder 不存在 → replay。**四类穷尽，无"错误 replay"可达路径。**

**可证伪性框架（供专家挑战）：** 若专家能在上述状态穷举之外构造一个交错，使 (a) key 双绑定、(b) 跨身份错误 Job 返回、或 (c) "gate 快照内已提交的他行绑定 + 不同 raw hash"仍返回 replay——任一成立，则 C-1 回退 P1 并继续修订。在该反例出现之前，"普通 SELECT 存在 TOCTOU 窗口"为真、"窗口导致违反不变量的成功结果"为假。

**小结：** 通过标准复核——P0=0 ✓；P1=0 ✓（C-6 为确定性 key 格式缺口，非并发缺陷，P2 并入 v3 冻结）；52 项验收标准可执行 ✓（51 项可自动化——其中 AS2-14..19/49/50 需真实 PostgreSQL 并发、42/43/45 需权限隔离、AS2-46 与 §9.7.1 drain 检查为部署人工门禁；P3-2 时序钉死后 AS2-49/50 无 flaky）。C-6 回归断言按最小变更并入 AS2-28b/AS2-47（保持 52 项），或作为新增项扩展矩阵——由文档作者在 v3 冻结时定夺，实质要求不变：C-6 必须有自动化覆盖。

---

## 4. C-2..C-5 逐项复核

### 4.1 C-2（Fingerprint 版本）— APPROVED

任务书六问对照 §9.2：

1. **v1-only 版本不一致失败关闭？** ✓ §9.2.1 冻结"传入版本 vs 存储版本，不等即失败关闭"，并诚实声明该分支是完整性防御而非恢复机制（§9.2.4）。
2. **5xx 是否符合系统错误语义？** ✓ §9.2.2：5xx 完整性错误 + 不可自动重试 + 告警。这不是调用方可修复错误，也不是暂时性故障，5xx（而非 4xx）语义正确。
3. **调用方如何恢复？** ✓ lookup 通道（Q10/N-6：lookup 不比较指纹、不受 key 影响）。
4. **未来 v2 是否必须保留 v1 canonicalizer？** ✓ §9.2.3：按存储行版本重算 + 独立迁移/兼容设计；移除仍存活版本 = P0。
5. **是否存在已创建但无法恢复的升级窗口？** POST-replay 窗口存在（直至端口获得按存储版本重算能力），但 **lookup 全程可用 ⇒ 不存在完全不可恢复状态**；窗口闭合被门禁化（§9.2.3 要求升级前独立设计与验收）。
6. **升级期间是否需要双版本支持？** ✓ 必须（§9.2.3），且明确区分：**AS-2 现在只实现 v1 单版本 + 失败关闭；双版本/按存储版本重算仅为未来 v2 引入时的强制要求**——边界清晰，未给当前实施增加隐性范围。

ART-04H 指出的 §5.2 与 §5.3.1 表述张力被 §9.2.4 显式消除。**通过。**

### 4.2 C-3（Canonical JSON）— APPROVED

1. **serde_json 默认 Map 排序满足定义？** ✓ 默认特性下 `Map<String, Value>` = BTreeMap（F-15：根 `Cargo.toml` L42 `serde_json = "1"`，workspace 全员 `{ workspace = true }` 无特性开关）。
2. **code point 排序与 UTF-8 字节排序一致？** ✓ UTF-8 编码保序性（byte order ≡ code point order），`String` 的 `Ord` 即字节序。
3. **数字规范化能否被真实保证？** ✓ 整数 i64/u64 原样；浮点 ryu 最短往返；行为随锁定版本确定，golden vectors（AS2-01）+ 锁文件钉死。
4. **1e2 / 100.0 / 100 预期？** ✓ `1e2` 与 `100.0` 均解析 f64 → 序列化 `100.0`（同指纹）；`100` → `100` ≠ `100.0`（不同指纹）。§9.3.2 与 AS2-06 一致，与 serde_json 实际行为逐字吻合（ART-04H §4.2 已验证）。
5. **特性是否可能经依赖树间接启用？** **是，风险真实**（cargo feature 统一化：任一依赖开启 `preserve_order` 即全局生效）。§9.3.1 的防护 = "特性变化必须显式重排 + 全量 golden vectors 通过，不得静默改变 v1 输出"。判定：契约层面充分（golden + AS2-03 键序等价测试会抓住任何漂移）；P3-3 建议实现层面直接采用**显式递归按键重排**而非依赖 BTreeMap 默认，从根上免疫 feature 统一化。
6. **absent/null 区分一致？** ✓ §9.3.2 自洽：DTO 层 absent `options` ≡ null（指纹含 `"options": null`）；payload 内部 absent 与 null 保持区分（按解析结果原样规范化）。
7. **golden vectors 跨版本稳定性？** ✓ 规则冻结 + serde_json 版本锁定 + AS2-07 未知版本失败关闭 + §9.2.3 canonicalizer 存活期硬规则，覆盖单版本内与跨 fingerprint 版本两个维度。

**结论：规范可被当前 serde_json 准确实现，无需替代方案；P3-3 为实现层加固建议。**

### 4.3 C-4（信任边界与 DB 权限）— APPROVED（附本轮盘点证据）

任务书八问，答案全部来自源码盘点：

| # | 问题 | 事实 |
|---|---|---|
| 1 | APP 直接 INSERT？ | **是**：`deploy/db-roles.sql` L50 `GRANT SELECT, INSERT, UPDATE ON ALL TABLES`（含 generation_job，无对该表的例外 REVOKE） |
| 2 | APP 直接 UPDATE？ | **是**（同上） |
| 3 | SECURITY DEFINER 是否唯一受控入口？ | **当前否**（APP 保有表级 DML）；v2 §9.4.2 已把"收窄为仅函数路径"定为优先方案并要求实施 PR 盘点后裁决——正确地未盲目撤权 |
| 4 | Worker 是否需要直接修改？ | **否**：db-roles L88 `REVOKE ALL ON ALL TABLES FROM WORKER`；Worker 全部写入经 SECURITY DEFINER 协调函数（L72-78 EXECUTE 授权） |
| 5 | Repository 是否绕过受控函数？ | **否**：生产 Rust 代码对 `artifact.generation_job` 的直接 INSERT/UPDATE 为**零**（本轮 grep 证实：16 处直接写全部位于 `tests/`——`tests/tests/common.rs` L323、`migration_invariants.rs` L202/L323/L332-353、`worker_coordination.rs`、`migration_guard.rs`）；`request_generation` 从不触达 DB（artifact.rs L574-623）；cancel 走 `request_generation_job_cancel` 函数（db-roles L84） |
| 6 | 收窄是否破坏既有功能？ | 生产路径不受影响；**tests 直写需要调整**（改经函数或专用测试角色）——正是 §9.4.2 要求先盘点的原因，盘点结果支持收窄 |
| 7 | 列级授权或新受控函数？ | 现有函数集已覆盖全部生产写路径（create/cancel/claim/heartbeat/complete/fail/recover/purge），无需新增；表级 REVOKE INSERT/UPDATE + 保留函数 EXECUTE 即为最小方案（SECURITY DEFINER 以 owner 写入，不需要 APP 表级 DML） |
| 8 | APP 直插伪造 fingerprint 是否可能？ | **当前可能**（表级 INSERT + CHECK 只验 64hex 格式，不验内容）——v2 §9.4.1 诚实声明信任边界：指纹内容正确性唯一边界是 Rust typed DTO/canonicalizer，DB 只负责配对/格式/唯一/不可变/事务收敛；§9.4.3 强化 AS2-42（必须验证直插按最终权限被拒绝或严格受控）；§9.7.2 明文禁止"触发器保 UPDATE"被误写成"指纹不可伪造" ✓ |

**通过：v2 的处理（显式信任边界 + 门禁化收窄 + 强制盘点）与源码事实吻合。**

### 4.4 C-5（SQLSTATE 与 token 映射）— APPROVED

冻结映射逐项核对（0003 现值对照）：

| token | v2 SQLSTATE | 0003 现状 | Rust 分类（token-first） | 终态 | 误判风险 |
|---|---|---|---|---|---|
| IDEMPOTENCY_KEY_REUSED | 23505 | 一致（L353、L385） | Terminal(IdempotencyConflict) | 409 | 无 |
| CALLER_REQUEST_ID_REUSED | 23505 | 新 | Terminal(CallerRequestIdReused) | 409 | 无 |
| CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH | P0001 | 新 | Terminal(FingerprintVersionMismatch)，不可重试 | 5xx | 无 |
| IDEMPOTENCY_KEY_CROSS_IDENTITY | P0001 | 新（防御分支） | Terminal(IntegrityFailure) | 5xx | 无 |
| GENERATOR_UNAVAILABLE | P0001 保持 | 一致（L332） | Terminal(GeneratorUnavailable) | **503**（AS2-51 钉死不落入 IntegrityFailure） | 无 |
| UNKNOWN_GENERATOR | 22023 保持 | 一致（L328） | Terminal(InvalidRequest) | 400 | 无 |
| 未命中 token 的 23505/40001/40P01 | — | AS-1 分类器将三者归 Retry（keyed_create.rs L50） | 有界重试后 | 503 | 可接受（仅剩结构上不可达的"违例后双 miss"路径） |

- token-first 与现有机制兼容性：AS-1 分类器在 Artifact 路径上继续按 SQLSTATE 分类（其 23505 只来自真实索引竞争，无业务 RAISE）；Job 路径新增 token 层在 SQLSTATE 分类**之前**执行——两条路径互不干扰 ✓。
- §9.5.4 修正了"23505 一律 Retry"的隐患 ✓；§9.5.3 明确反例（P0001 的 GENERATOR_UNAVAILABLE）✓。

**通过。**

---

## 5. 新发现缺陷

### C-6（P2，冻结前必须并入）：§9.1 门禁使用原始 `p_idempotency_key`，未与 step 3 的 key 规范化对齐

**源码/文本证据：**

- §5.3.1 step 3 冻结的规范化为 `v_key := nullif(btrim(p_idempotency_key), '')` + 长度校验（≤255，超限 `INVALID_IDEMPOTENCY_KEY`）——0003 L337-344 现行行为相同。
- §9.1 门禁 SQL 直接使用 `p_idempotency_key`（`IF p_idempotency_key IS NOT NULL ... WHERE idempotency_key = p_idempotency_key`），**且位于 step 3 之前执行**——replay 路径在 key 规范化/校验发生前返回。

**缺陷场景：** Job B 已绑定 K2（经规范化存储）。请求 {caller A, F_A, key=' K2 '}（带空白填充）：caller 命中 A、指纹一致 → 门禁以 `' K2 '` 查询 → 与任何存储 key（一律 btrim 后非空）不匹配 → 门禁放行 → **200 replay，绕过本应触发的 409**。同理：空串 key 在 replay 路径被当作"有 key 但未命中"处理（行为上等价无 key，良性）；超长 key 在 replay 路径跳过长度校验（create 路径会 400）。

**定级论证（P2 而非 P1）：**

- 无并发窗口可利用（与 C-1 的仲裁缺口不同，这是确定性的文本缺陷）；
- 无数据损坏、无双绑定、无错误 Job 返回——危害等同"未向调用方标记 key 误用"，但暴露面窄：RFC 7230 `field-value` 语法要求去除首尾 OWS，经 HTTP 层的 key 不会带首尾空白，**实际暴露面仅为绕过 HTTP 直接调用函数的路径（测试/未来内部服务）**；
- 修订为机械一行级改动，不影响 §9.1 已证明的并发仲裁结构。

**最小修订方案（并入 v3）：**

1. 将 key 规范化与长度校验**前移至 step 1（输入校验段）**：`v_key := nullif(btrim(p_idempotency_key), '')` + `v_key` 非空时校验长度 ≤ 255，超限 `RAISE INVALID_IDEMPOTENCY_KEY`（22023）。create 路径 step 3 改用已有 `v_key`，语义不变。
2. §9.1 门禁（读前查与异常块两处，共用 helper）以 `v_key` 替换 `p_idempotency_key`；`v_key IS NULL` 时门禁跳过（与"无 key"一致）。
3. 新增验收项 **AS2-52**（见 §6）。

**需修改章节：** ART-04G §9.1（门禁 SQL 文本 + "v_key 前移"说明）、§5.3.1 step 1/step 3 对应实现注意项。

### P3 清单

| ID | 级别 | 内容 | 需修改章节 |
|---|---|---|---|
| P3-1 | P3 | §9.1 应显式声明门禁为"语句快照时点语义"：gate 通过后、事务结束前的并发绑定属合法串行序（时序 B/E），不得被实现者当作缺陷"修复"（加锁/加约束仲裁即过度工程），也不得被测试断言为必然 409 | §9.1 优先级冻结段后补一段快照语义说明 |
| P3-2 | P3 | AS2-49/AS2-50 必须钉死测试时序以避免 flaky：AS2-49 预置状态须全部 commit 后再并发发送（则双 409 确定）；AS2-50 须保证竞争性 key 绑定（T2）在败者异常块门禁语句之前 commit（则 409 确定），否则结果集 ∈ {409, replay} 均合法 | §9.6 对应行补"时序前提" |
| P3-3 | P3 | canonicalizer 实现建议显式递归按键重排（BTreeMap 排序作为断言而非依赖），从根上免疫 cargo feature 统一化 | §9.3.1 加一句实现建议 |
| P3-4 | P3 | §6 矩阵 AS2-42 原文为旧语义（"APP 可 INSERT…触发器拦截身份改写"），§9.4.3 已强化——冻结时应在 §6 矩阵内同步改写 AS2-42 预期，避免实现者只读旧表（§9 优先级条款虽已兜底，但矩阵是实施 PR 的执行清单） | §9.6 或 v3 §6 的 AS2-42 行 |

---

## 6. 验收矩阵核验（§9.6 声称 52 项）

### 6.1 编号完整性

`AS2-01..27`（27 项）+ `AS2-28a/28b`（2 项）+ `AS2-29..46`（18 项）+ `AS2-47..51`（5 项）= **52 项** ✓。编号连续、无重复、无缺号。v2 对 ART-04H "46→51" 的算术更正是成立的（28 拆分净增 1 + 新增 5 = 52；ART-04H 因把 AS2-51 标为"建议"而少计）。原 AS2-28 已删除并由 28a/28b 取代 ✓。

### 6.2 执行类别划分

| 类别 | 项目 |
|---|---|
| 纯 UNIT（golden vectors/规范化） | AS2-01..07 |
| **必须真实 PostgreSQL** | AS2-08..19、25..46（DB 行为、约束、触发器、函数、权限、迁移）及 28a/28b/47/48 |
| **必须并发压力测试** | AS2-14..19（既有）、**AS2-49、AS2-50**（C-1 并发场景，见 P3-2 时序要求） |
| **必须权限隔离测试** | AS2-42（经函数创建 + APP 直插按最终权限拒绝）、43（触发器）、45（MAINTENANCE 清理） |
| HTTP/SDK parity | AS2-20..24 |
| **部署人工门禁（不可自动化为 PASS）** | AS2-46（迁移证据：行数一致/无回填）+ §5.7.3/§9.7.1 的迁移窗口 drain 检查（pg_stat_activity 证据） |

### 6.3 C-1 并发场景覆盖核对

- AS2-47/48：预置交叉（确定性 409）✓
- AS2-49：并发交叉 ✓（需 P3-2 时序钉死）
- AS2-50：异常块门禁 ✓（需 P3-2 时序钉死）
- **缺失 1（benign interleaving 钉死）：** 无测试钉死时序 B/E 的"合法 200 replay"结局——不钉死则实现者可能写出期望必然 409 的 flaky 测试，或过度加锁。**新增 AS2-53：** 预置 Job A(caller A, K1)；并发发送 R1={caller A, F_A, K2}（replay）与 R2={caller B, F_B, K2}（create），R1 的门禁先于 R2 commit 执行 → **R1=200 replay A、R2=202 created 均为合法终态**；断言 K2 最终绑定唯一、无行改写。
- **缺失 2（C-6 回归）：** **新增 AS2-52：** Job B 绑定 K2 后，请求 {caller A, F_A, key=' K2 '}（填充空白）→ **409 IDEMPOTENCY_KEY_REUSED**（门禁按规范化 v_key 匹配）；key='' → 等价无 key，replay 放行；key 长度 >255 → 400 INVALID_IDEMPOTENCY_KEY（replay 与 create 路径一致）。

矩阵增补后总数：**54 项**（52 + AS2-52 + AS2-53）。实施 PR 按此逐项交付证据。

---

## 7. AS-2 工作包边界复核

§9.7.4 重申全部边界；本轮逐项复核无变化、无隐性依赖：

- 不启用 Generator / 不修改 Worker / 不动 LMMS / 不动 File Service ✓（§9 未引入任何相关需求）；
- 不实现 target_version_id / AS-3 / AS-4 全链路 / PM-1..3 ✓；
- 不影响 AS-1 既有实现 ✓（AS2-31 回归保留；C-1 门禁只触碰 Job 路径函数，Artifact 路径 `create_or_resolve_keyed` 与 `idempotency_record` 清理链路零改动；db-roles 若按 §9.4 收窄仅影响 `generation_job` 一表——但这属于 AS-2 迁移交付物范围（§5.7.2 角色权限迁移），不是越界）；
- 唯一新触碰面：`deploy/db-roles.sql`（可能的 generation_job 表级 REVOKE）+ tests 直写调整——均在 AS-2 交付物清单内，且 §9.4.2 要求先盘点后撤权，不构成隐性依赖。

---

## 8. 缺陷清单汇总

| ID | 级别 | 缺陷 | 证据 | 最小修订 | 章节 |
|---|---|---|---|---|---|
| C-6 | **P2** | §9.1 门禁用原始 key 参数，绕过 step 3 规范化：填充空白 key 可跳过 C-1 门禁；空串/超长 key 在 replay 路径跳过校验 | §5.3.1 step 3（`nullif(btrim())`，0003 L337-344 同） vs §9.1 门禁 SQL；RFC 7230 field-value 剥 OWS 限定暴露面 | v_key 规范化+长度校验前移 step 1；门禁（两处，共用 helper）改用 v_key | §9.1、§5.3.1 |
| P3-1 | P3 | 门禁快照时点语义未声明，防过度工程/flaky 测试 | §3.3 时序 B/E 证明 | §9.1 补说明 | §9.1 |
| P3-2 | P3 | AS2-49/50 测试时序未钉死 | §6.3 | §9.6 补时序前提 | §9.6 |
| P3-3 | P3 | canonicalizer 免疫 feature 统一化的显式重排建议 | §4.2 #5 | §9.3.1 加实现建议 | §9.3 |
| P3-4 | P3 | §6 矩阵 AS2-42 旧文与 §9.4.3 强化不一致 | §9.4.3 | v3 同步改写矩阵行 | §9.6/§6 |

**P0 = 0，P1 = 0。** 修复 C-1 后残余的全部为 P2/P3 级局部修订，且 C-6/P3 均为文本级、无架构返工。

---

## 9. 冻结与授权结论

1. **C-1 并发仲裁已被证明解决**：§3.3 六时序 + 四论证覆盖全部并发交错，所有结果均等价于合法串行序；不存在"两个事务在检查时都看不见对方进而产生违反幂等语义的成功结果"的可利用组合——benign 双成功结局本身就是合法串行序（时序 B），不构成违例。
2. **建议冻结 AS-2 契约：是（v3）。** 冻结条件 = 将 C-6 修订（§5 方案）与 P3-1..P3-4 并入 ART-04G v3，同时把验收矩阵更新为 54 项（+AS2-52/53）。全部为局部文本修订，**建议以 diff 复核收尾，无需第四轮全文评审**。
3. **AS-2 代码实施授权：APPROVED（条件执行）** —— v3 冻结修订并入并 diff 复核后，方可创建独立实施分支、按 Phase 1（数据模型与 Migration）→ Phase 2（PostgreSQL 事务）→ Phase 3（Application Service）→ Phase 4（HTTP/SDK）→ Phase 5（54 项矩阵全量 + 真实 PostgreSQL 并发/故障注入证据）→ Phase 6（独立审查）推进；PR 保持 Draft。本评审不启动任何实施。
4. **不解除的既有门禁**：不启用生成、不动 Worker、不触碰 LMMS/File Service、不合并 PR #8、不启动 AS-3/AS-4/PM-1..3；AR04-02 维持 NOT APPROVED / DEFERRED。
5. 按"宁可 CHANGES REQUIRED 也不接受未经证明的并发正确性"原则的对照：本轮 C-1 的并发正确性**已给出可证明的仲裁分析**（非默认采信），残余问题均不属并发正确性类别——因此授权结论不是为推进进度而放水，而是评审条件（P0=0、P1=0、C-1 已证明）的满足。

---

## 10. 约束遵守声明

- 未修改任何业务代码、Migration、OpenAPI、SDK；
- 未创建 AS-2 实施分支（仅向现有文档分支 `docs/art-04g-as2-review-contract` 新增本文档）；
- 未修改、未合并 PR #8；
- 未启动 AS-3，未开发 Generator/Worker/SeeCode，未启用生产生成，未触碰 LMMS/File Service；
- 全部核查为只读操作 + git diff 基线证明。

**本报告 commit SHA 见提交记录（推送后于会话中报告）。**
