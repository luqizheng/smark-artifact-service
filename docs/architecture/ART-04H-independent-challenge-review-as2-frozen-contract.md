# ART-04H — AS-2 冻结契约二次独立挑战评审（ART-04G §5 复核）

> **Status:** 独立架构挑战评审结论（CHANGES REQUIRED）
> **Date:** 2026-11-25
> **Reviewer:** 独立架构评审（第二轮：对 ART-04G 提出的 AS-2 冻结契约进行独立挑战，判断其是否真正 implementation-ready；不重复 ART-04G 原有的 13 项问题裁决）
> **Baseline:** `main` @ `c11e79a51a145ded026632630bb0b52aa44dc311`；AS-1 分支 `feat/as-1-durable-artifact-creation-identity`（PR #8 HEAD）@ `5541d3be8c90cfcf9283b6e59426c8a3c2bfe099`；评审分支 `docs/art-04g-as2-review-contract` @ `c35eddd45eea7c390c972df30c0a0ddc7e1dd05a`（= AS-1 HEAD + ART-04G 文档提交）
> **评审对象:** `docs/architecture/ART-04G-independent-review-as2-generation-job-caller-identity.md` §5 冻结契约
> **Scope:** 仅 AR04-01 / AS-2 契约复核。不含 AS-3（AR04-02）、AS-4、PM-1..PM-3。
> **约束遵守:** 本评审未修改任何代码、Migration、OpenAPI、SDK；未创建实施分支；未合并 PR #8；未启动 AS-2/AS-3 代码实施；未开发 Generator、Worker 或 SeeCode；未启用生产生成。仅新增本文档并推送至现有评审文档分支。

---

## 1. 总体结论

**CHANGES REQUIRED。**

| 对象 | 结论 |
|---|---|
| ART-04G §5 作为 AS-2 唯一实施契约 | **修订后方可冻结**：存在 1 个新 P1 级自相矛盾（caller 身份解析优先返回绕过 Idempotency-Key 冲突门禁），不得按现文直接实施 |
| 新缺陷 | 无新 P0；**1 个新 P1（C-1）**；**4 个新 P2（C-2..C-5）** |
| ART-04G 是否需要修订 | 是：§5.3.1 step 2/7（key 冲突门禁）、§5.6 规则 2 措辞、§5.2 版本纪律澄清、§5.7 信任边界与 ERRCODE 冻结、§6 矩阵扩容。全部为收敛性修订，不推翻任何已裁决方向 |
| 是否增加验收项 | 是：新增 AS2-47/48/49/50，AS2-28 拆分为 28a/28b；建议随附 AS2-51 |
| AR04-01 是否可以冻结 | 完成新 P1（C-1）修订后冻结；按现文为 CHANGES REQUIRED |
| AR04-02 / AS-3 | **维持 NOT APPROVED / DEFERRED**（ART-04G §7.2 三条理由经核查无新证据推翻） |

设计方向本身（单行身份列 + 行内 CHECK 配对 + 部分唯一索引 + 触发器不可变、指纹 v1 Rust 侧版本化计算、单事务 keyed-create 端口、lookup 路由、三链路 hash 分离、迁移策略）逐项经源码证据验证成立；本次全部挑战中**没有任何一项推翻 ART-04G 的已裁决方向**，但 §5.3.1 的解析顺序存在与契约自身（§5.6 规则 2、Q2、AS2-28）矛盾的缺口，必须先修订冻结。

---

## 2. 审查基线核对（SHA 与源码证据）

基线 SHA 核对结果：**无偏差**。

| 对象 | 声明 | 实际 | 一致 |
|---|---|---|---|
| main | — | `c11e79a51a145ded026632630bb0b52aa44dc311` | ✓ |
| AS-1 PR #8 HEAD | `5541d3be8c90cfcf9283b6e59426c8a3c2bfe099` | `5541d3be8c90cfcf9283b6e59426c8a3c2bfe099` | ✓ |
| 评审分支 | — | `c35eddd` = AS-1 HEAD + ART-04G 文档提交 | ✓ |

ART-04G §2 的事实清单（F-1..F-14）逐条重新核实，**全部与源码一致**：

| # | 事实 | 证据位置（本轮核实） | 一致 |
|---|---|---|---|
| F-1 | `generation_job` 无 `caller_request_id`；`idempotency_key` 可空 + 部分唯一索引 | `migrations/0001_init.sql` L313-348、`migrations/0003_job_coordination_fixes.sql` L139-141 | ✓ |
| F-2 | 创建函数 SECURITY DEFINER、固定 search_path、只按 key 去重（读前查 + 异常块重读） | 0003 L271-389 | ✓ |
| F-3 | 创建函数无生产调用方；`request_generation` 在 generator 检查后直接返回 `GeneratorUnavailable` | `crates/artifact-application/src/service/artifact.rs` L574-623 | ✓ |
| F-4 | Job 路径不使用 `idempotency_record`；key 存于 Job 行 | 0003 L341-389、0001 L393-410 | ✓ |
| F-5 | Job 无删除路径；Attempt FK `ON DELETE RESTRICT`；purge 只置空 input/options 正文（保留 sha256） | `migrations/0002_job_coordination.sql` L58、0003 L908-933（L925-926 仅置空正文） | ✓ |
| F-6 | `guard_job_transition` 仅保护 ownership + 状态机 | 0001 L350-370 | ✓ |
| F-7 | `Scoped`：tenant 来自 Bearer delegation、project 来自 `X-Project-Id`；create 路由 `_headers` 被忽略、fail-closed 503 | `apps/artifact-api/src/auth.rs` L52-85、`routes/artifacts.rs` L486-529 | ✓ |
| F-8 | `request_hash` = 原始 HTTP Body SHA-256（body 含 caller_request_id；Idempotency-Key 是头，**不入** hash） | `routes/artifacts.rs` L42-53 | ✓ |
| F-9 | DTO 无 caller 字段、无 target_version_id | `crates/artifact-contract/src/dto.rs` L196-221 | ✓ |
| F-10 | SDK 仅 `request_generation` | `crates/artifact-client/src/lib.rs` L320-332 | ✓ |
| F-11 | OpenAPI POST 202/409 文档；`GenerationJob` schema 无 caller 字段 | `openapi/openapi.yaml` L383-470、L771-813 | ✓ |
| F-12 | APP 表级 SELECT/INSERT/UPDATE 全表 + create 函数 EXECUTE；AS-1 身份表 REVOKE UPDATE/DELETE | `deploy/db-roles.sql` L50-96 | ✓ |
| F-13 | AS-1 先例（版本分派指纹、单事务端口、golden vector、token 需求） | `crates/artifact-application/src/service/creation_fingerprint.rs`、`crates/artifact-infrastructure/src/postgres/keyed_create.rs` | ✓ |
| F-14 | AS-1 分类器把 23505 一律归 Retry | `keyed_create.rs` L45-59（L50） | ✓ |
| P2-5 事实 | Idempotency-Key 参数描述"保留 ≥24h"被 Job 路由共享引用 | `openapi.yaml` L528（参数对象）+ L398（Job 路由引用） | ✓ |

另核实一项 ART-04G 未列出但对本评审关键的事实：

- **F-15（新）**：workspace `serde_json = "1"` 为默认特性（根 `Cargo.toml` L42）——未启用 `preserve_order` / `arbitrary_precision`，即 `serde_json::Map` = BTreeMap。这是 §5.2 canonical JSON 规则当前可实现的前提（详见 §4.3）。

---

## 3. 挑战一：Caller identity 与 Idempotency-Key 冲突优先级 — **挑战成立（新 P1，编号 C-1）**

### 3.1 缺陷确认

ART-04G §5.3.1 冻结的函数内执行顺序为：输入校验 → **caller 身份解析（命中即无条件 RETURN）** → Idempotency-Key 解析 → generator 检查 → INSERT。

用户构造场景（本文化为一般形式）：

- Job A = (caller_request_id=A, fingerprint F_A, key=K1, request_hash=H_A)；
- Job B = (caller_request_id=B, fingerprint F_B, key=K2, request_hash=H_B)；
- 新请求 = (caller_request_id=A, fingerprint=F_A, key=K2)。

关键结构事实（源码依据 F-8）：Idempotency-Key 是 HTTP 头，不参与 request_hash；caller_request_id 在 body 中，参与 request_hash。因此该新请求的 body 与 Job A 原始 body 逐字节一致（仅头不同）⇒ 其 request_hash = H_A。

**§5.3.1 现文行为**：step 2 caller 读前查命中 Job A、指纹一致 ⇒ 无条件 `RETURN (A, 'caller_request_replay')`；step 3 的 Idempotency-Key 解析**永不执行**；K2 与 Job B 的绑定冲突（K2 的持有者 request_hash=H_B ≠ 本请求 H_A）**从未被评估**。

**结论：是的，当前冻结算法会直接返回 Job A，绕过 K2 的 `IDEMPOTENCY_KEY_REUSED` 检查。**

### 3.2 五个必答问题

1. **是否违反既有 Idempotency-Key 冲突语义？** 是。既有不变量（0003 L349-353/L381-385 + OpenAPI L394/L415/L528）："同 key + 不同请求体 → 409"是无条件的；ART-04G 把它弱化为"同 key + 不同请求体 **且** 请求未在 caller 身份解析中命中 → 409"。且与 ART-04G 自身三个冻结陈述矛盾：§5.6 规则 2（"同 key 跨身份重放被现有冲突语义拦截"）、Q2（"同 key 不同 caller_request_id 必然不同 body hash → 409"）、AS2-28（"409……现有语义不变"）在该组合下均不成立。注意：E-7 方向（**不存在的** caller 身份 + 用别人的 key）仍然被拦截（step 2 miss → step 3 hit → 409 ✓）；被绕过的只有"**已存在的** caller 身份 + 用别人的 key"方向。
2. **caller identity 是否有权覆盖 Idempotency-Key 冲突？** 有权决定"返回哪个 Job"，**无权静默吞掉 key 冲突门禁**。合法恢复的完整集合 = {原 key、未绑定的轮换新 key、无 key} ∪ {lookup（从不比较指纹与 key，Q10/N-6）}——没有任何一种合法恢复需要"呈现绑定在别的 Job 上的 key"。被 409 的唯一组合是调用方 key 误用，409 是正确反馈且可通过 lookup / 换正确 key 恢复，身份收敛不变量不受损。反之也不能把 key 检查整体提前（见 3.4 组合 5）：身份冲突更根本，被 key 冲突抢先误报会误导恢复算法（"换 key 重试" vs ART-04A §2.5.5 的"禁止换 ID"）。
3. **如何调整查询顺序和并发事务？** 见 3.4 修订方案（Option C）：身份解析先行用于确定候选 Job，但 key 冲突作为**成功返回前的硬门禁**，在 step 2 命中分支与 step 7 异常分支**同构**实现。
4. **如何确保同 caller + 新 key 的合法恢复仍然成立？** 见 3.4 状态组合表行 1/2/3：key 门禁分别命中"key 绑在 A 自身 / key 未绑定 / 无 key"，全部放行；AS2-27（key 轮换恢复）语义不变；lookup 通道任何时候兜底。
5. **是否需要新增自动化验收项？** 是：AS2-47/48/49/50 + AS2-28 拆分（见 §6.2）。

### 3.3 完整状态组合表

存储：Job A=(caller A, F_A, K1, H_A)、Job B=(caller B, F_B, K2, H_B)。新请求 R=(caller X, fingerprint f, key k, raw hash h)。

| # | R = (X, f, k) | §5.3.1 现文行为 | 正确语义（建议冻结） | 依据 |
|---|---|---|---|---|
| 1 | A, =F_A, K1（A 原 key） | 200 replay A | 同左 ✓ | key 绑在 A 自身，身份优先（重放分支不重评 raw-hash 严格性是 §5.6 规则 3 的既定设计） |
| 2 | A, =F_A, 新 K3（未绑定） | 200 replay A | 同左 ✓ | 合法 key 轮换恢复（E-6 / AS2-27） |
| 3 | A, =F_A, 无 key | 200 replay A | 同左 ✓ | 门禁跳过 |
| **4** | **A, =F_A, K2（B 的 key）** | **200 replay A ← K2 冲突被静默跳过（缺陷）** | **409 IDEMPOTENCY_KEY_REUSED** | k 绑定 B 且 h=H_A≠H_B（h 含 caller A 的 body ≠ B 的 body，必然不等） |
| 5 | A, ≠F_A, 任意 k | 409 CALLER_REQUEST_ID_REUSED | 同左 ✓ | 身份冲突最根本，必须优先于 key 冲突报出（不能采用"key 检查整体提前"方案的原因） |
| 6 | A, version ≠ 存储 | 5xx FINGERPRINT_VERSION_MISMATCH | 同左 ✓ | 失败关闭（见 §4.2） |
| 7 | C（不存在）, 任意, K2 | 409 IDEMPOTENCY_KEY_REUSED | 同左 ✓ | E-7 方向本来就成立 |
| 8 | C（不存在）, 任意, K1 | 409 | 同左 ✓ | body 含 caller C ⇒ h≠H_A |
| 9 | C（不存在）, 任意, 新/无 key | created | 同左 ✓ | — |
| 10 | B, =F_B, K2 | 200 replay B | 同左 ✓ | — |
| **11** | **B, =F_B, K1（A 的 key）** | **200 replay B ← 同款缺陷** | **409 IDEMPOTENCY_KEY_REUSED** | 对称方向 |

争议组合仅 4/11 两种：身份命中决定"返回哪个 Job"没有问题，但 **key 绑定在别的 Job 上且请求体不同** 是传输层冲突，不应被静默吞掉。无害性边界（不升级为 P0 的理由）：该缺陷不造成数据损坏（无行创建、key 绑定不被改写）、无跨 scope 泄露（响应只含身份匹配的 Job）、无权限提升；损失仅为"未向调用方标记 key 误用"，但它使冻结契约在唯一实施规格层面自相矛盾（AS2-28 按现文无法始终成立），必须在实施前修订 ⇒ **P1**。

### 3.4 修订方案（Option C，冻结 SQL 级）

原则：**身份解析先行用于确定候选 Job；key 冲突作为任何成功返回之前的硬门禁；身份冲突（指纹/版本不一致）优先于 key 冲突报出。**

修订 §5.3.1 step 2（读前查分支）：

```sql
-- step 2a：caller 身份解析（读前查）
IF p_caller_request_id IS NOT NULL THEN
    SELECT * INTO v_existing FROM artifact.generation_job
     WHERE tenant_id = p_tenant_id AND project_id = p_project_id
       AND caller_request_id = p_caller_request_id;
    IF FOUND THEN
        IF v_existing.fingerprint_version <> p_fingerprint_version THEN
            RAISE 'CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH';  -- 失败关闭
        END IF;
        IF v_existing.request_fingerprint <> p_request_fingerprint THEN
            RAISE 'CALLER_REQUEST_ID_REUSED';  -- 身份冲突最根本，先于 key 冲突报出
        END IF;
        -- 指纹一致：身份有权重放，但先过 key 冲突门禁
        IF v_key IS NOT NULL THEN
            SELECT * INTO v_holder FROM artifact.generation_job
             WHERE tenant_id = p_tenant_id AND project_id = p_project_id
               AND idempotency_key = v_key;
            IF FOUND AND v_holder.id <> v_existing.id THEN
                IF v_holder.request_hash <> p_request_hash THEN
                    RAISE 'IDEMPOTENCY_KEY_REUSED';  -- key 绑在别的 Job 且请求体不同
                END IF;
                RAISE 'IDEMPOTENCY_KEY_CROSS_IDENTITY';  -- 防御条款（结构上不可达，见下）
            END IF;
        END IF;
        RETURN QUERY SELECT v_existing.id, 'caller_request_replay';
        RETURN;
    END IF;
END IF;
-- step 2b：未命中 caller → step 3 原 Idempotency-Key 解析逻辑逐字保留
```

**结构性事实（注释声明）**：同 scope 内，若 key 绑定行的 request_hash = 本请求 request_hash，则两者 body 逐字节一致 ⇒ 其 caller_request_id 必等于本请求 caller_request_id ⇒ 该行即 v_existing 自身；故"id 不同 + hash 相同"分支不可达，fail-closed 防御条款保留（与 §5.3.1 step 7 原防御条款同款）。

**step 7 异常块必须同构**：caller 重读命中 + 指纹一致后，**先施加同一 key 门禁**再返回 replay。覆盖的并发时序：本请求 {caller A, key K2} 与 T1={caller A, K5}、T2={caller B, K2} 同时竞争——本请求 INSERT 在 caller 或 key 索引上任一处失败后，异常块 caller 重读命中 A、指纹一致，但 K2 已绑定 B（hash 不同）⇒ 必须 409 而非 replay；若 K2 未被绑定则正常 replay。原 step 7 的"caller 未命中 → 按 key 重读"分支保留不变。

**并发收敛各既有验收项不受影响**（逐项推演）：AS2-14（N 并发同 ID 同指纹、各异 key）：败者在异常块 caller 重读命中、指纹一致、各自 key 未绑定或绑在胜者自身 ⇒ replay ✓；AS2-15 不变（身份冲突在门禁之前报出）；AS2-16（并发同 key 不同 caller）：败者 caller 重读 miss → key 重读命中、hash 不同（body 含不同 caller）⇒ 409 ✓；AS2-17 双索引竞争按 caller 优先 + 门禁收敛 ✓。

---

## 4. 其余五项挑战裁决

### 4.1 挑战二：Fingerprint 版本与请求兼容 — 方向成立，表述张力（新 P2，编号 C-2）

核实发现与 AS-1 先例的关键差异：

- **AS-1 端口**是"读存储版本 → Rust 按存储版本重算 → 比对"：`keyed_create.rs` L191 `canonical_fingerprint(identity.fingerprint_version, request)`（L374 同款），`creation_fingerprint.rs` L31 注释明示"解析既有身份时使用**存储的版本**"。
- **ART-04G §5.3.1** 把比较放进 DB 函数：比较的是"传入版本 vs 存储版本"，不等即 MISMATCH 失败关闭。单函数设计下（读前查在函数内部），Rust 无法先读存储版本再适配 ⇒ §5.2"比较时按存储身份的版本解析"在 v1 时代**不可达**，只能实现 §5.3.1 的"版本相等才比对"。两句冻结文本之间存在表述张力。

逐问回答：

1. **存储 v1、当前计算 v2 时如何恢复？** POST 重放 → MISMATCH 失败关闭（5xx）直至端口契约修订（"按存储版本多轮规范化"= AS-1 模式）落地；**lookup 不比较指纹 ⇒ 恢复通道始终存在**。
2. **是否真的能按存储版本计算？** 能——指纹从类型化 DTO 计算（非原始字节），DTO 保留全部 v1 字段即可按 v1 规则重算；这正是"Rust 侧计算"设计的价值所在。
3. **版本不匹配是否可能造成永久无法恢复？** 否（lookup 兜底 + 门禁化修订恢复 POST 通道）。
4. **未来升级是否需同时保留多个 canonicalizer？** 是（契约已隐含，建议明示为硬规则）。

**失败关闭策略判定：v1-only 时代足够**——所有写入者都是 v1，版本不一致结构上不可能（除行损坏），该分支实际是完整性防御而非迁移机制。

修订建议：(a) §5.2 明确"按存储版本解析"归属 v2 引入门禁而非 v1 现行为，消除与 §5.3.1 的张力；(b) 明确 MISMATCH 的 5xx 为**不可重试**语义（调用方应改走 lookup），否则恢复算法会按暂时性故障无限重试形成重试风暴；(c) 门禁补一句："凡存在以其写入的行，对应 canonicalizer 不得移除；移除仍存活的 canonicalizer 视为 P0 完整性事件"。

### 4.2 挑战三：Canonical JSON 可实现性 — 可实现，golden vectors 可稳定复现（新 P2，编号 C-3）

- **键排序**：workspace `serde_json = "1"` 默认特性（F-15）⇒ `Map` = BTreeMap，`String` Ord = UTF-8 字节序 = Unicode code point 序 ✓。**但 AS-1 的防御技巧对 AS-2 失效**：AS-1 全部键是 ASCII 常量、按字典序插入即可免疫 preserve_order（`creation_fingerprint.rs` L43 注释）；AS-2 payload 键是用户提供的任意 Unicode 键，一旦任何人启用 preserve_order，canonical 即漂移。AS2-01（golden）/AS2-03（键序等价）会抓住回归 ✓，但契约应显式冻结特性约束。
- **数字**：serde_json 默认——整数 i64/u64 原样；浮点 ryu 最短往返。`1e2` 与 `100.0` 都解析为 f64 → 序列化均为 `100.0` ⇒ **同指纹** ✓；`100`（整数）→ `100` ≠ `100.0` ⇒ **不同指纹** ✓。AS2-06 两条期望与 serde_json 实际行为逐字吻合。非有限值在校验层 walk Value 树显式拒绝，可实现 ✓。超大整数字面量溢出 u64/i64 转 f64——确定性由 serde_json 版本固定，golden vectors 冻结 ✓。
- **字符串字节精确**：serde_json 不做任何规范化（无 NFC）、仅必要转义、非 ASCII 原样输出 ✓ AS2-04 成立。
- **JSONB 不破坏语义的结构性原因**：指纹链路根本不经过 JSONB——Rust 从解析后 Value 计算、以 char(64) 存储比对，DB 只做 hex 格式与版本检查 ✓。但应在契约映射表固化一个反直觉事实：`1e2` 与 `100.0` 经 jsonb（numeric 保留 scale）渲染为 `100` 与 `100.0` ⇒ **input_sha256 不同而指纹相同**——三链路独立（§5.6）本来就允许，实现者易误判为矛盾。

修订建议：(a) §5.2 显式冻结"canonicalizer 实现禁用 preserve_order / arbitrary_precision，或必须对 payload 对象显式按键重排"；(b) 写入上述 jsonb/input_sha256 差异说明。**结论：Rust 实现可以稳定产生完全一致的 golden vectors。**

### 4.3 挑战四：单行存储模型与永久身份 — 裁决成立（新 P2，编号 C-4）

- Q3 论据逐条核实为真：Attempt FK `ON DELETE RESTRICT`（0002 L58）；purge 只置空 input/options 正文、保留 sha256（0003 L925-926）⇒ AS2-30"指纹比较只需存储 hash"成立；关系 1:1、caller 列全新无 legacy-unverifiable 类别 ⇒ 否决独立身份表、避免成对触发器 + 双快照风险面的论证充分。
- **触发器覆盖**：§5.1 骨架"任何变化（含 NULL→非 NULL、非 NULL→NULL/改值）→ 拒绝"覆盖 adoption / 抹除 / 改值三向 ✓（AS2-33/34 固化）。
- **DB 权限绕过核查**（F-12）：APP 持有 generation_job 表级 INSERT+UPDATE（db-roles.sql L50）——触发器拦 UPDATE 身份列 ✓，但 **APP 表级 INSERT 一行带伪造指纹的身份不被任何机制阻止**（CHECK 只验 64hex 格式；指纹语义一致性按设计只能在 Rust 算，DB 无法验证）。生产代码零直接 INSERT（仅 `tests/tests/migration_invariants.rs` L202/L323 测试用例），实际写入走 SECURITY DEFINER 函数（owner 身份）⇒ 与 AS-1 `artifact_creation_identity` 同款信任边界：**指纹内容正确性由应用层唯一写入路径承担，DB 承担唯一性/配对/不可变**。
- **tombstone**：当前不需要；Q3/Q11 的未来删除 tombstone 门禁已冻结且完备。

修订建议：§5.7 显式记录该信任边界，并将"是否 REVOKE APP 对 generation_job 的表级 INSERT/UPDATE、仅保留函数 EXECUTE（AS2-42'经函数'的措辞已暗示期望）"列为实施 PR 必答项（需附 grep 证据）。

### 4.4 挑战五：并发与异常处理 — 机制成立，两个细化缺口（并入 C-4/C-5）

- PL/pgSQL exception 子事务回滚 + 新语句快照重读：0003 真实先例（L357-386）✓；单行单 INSERT 结构上消除 AS-1 双快照误判面 ✓；Q7 论证成立。
- 双索引竞争：异常块"caller 先读、key 后读"覆盖两种失败源 ✓；但 step 7 caller 命中分支存在与 §3 同款的门禁缺口（由修订方案 + AS2-50 覆盖）。
- **P1-5 属实**：`keyed_create.rs` L50 确实把 23505 一律归 Retry；0003 用 ERRCODE 23505 承载应用冲突 `IDEMPOTENCY_KEY_REUSED`（L353）⇒ 照搬 AS-1 分类器会把确定性 409 变成有界重试后 503 + 重复插入压力。token-first 映射方案正确。
- **新发现（C-5）**：① §5.3.1 各新 RAISE 未冻结 ERRCODE；② AS-1 分类器把 P0001 归 IntegrityFailure（`keyed_create.rs` L52），而 0003 的 `GENERATOR_UNAVAILABLE` 用 P0001（L332）——token 映射必须**优先于** ERRCODE 分类命中，否则业务 503 会误报为 5xx 完整性错误。§5.3.3 的 token 列表已含 GENERATOR_UNAVAILABLE ✓，但应冻结 ERRCODE（建议沿袭 0003：应用冲突 23505、门禁 P0001、校验 22023）并增加"token 优先级高于 ERRCODE 分类"的显式测试（建议 AS2-51）。
- 错误结局枚举：token 命中 → 终态 409 ✓；未命中 23505/40001/40P01 → 有界重试后 503 ✓；除 §3 缺口外无错误 409 / 错误 503 的结构性风险。

### 4.5 挑战六：Lookup 与 POST 竞态 — 全部收敛，成立

- 404 → POST → 提交前 lookup 404 → 重试 POST → 唯一索引收敛 ✓（Q7/Q10 论证成立，单行单事务是关键前提）。
- POST 提交后响应丢失 → lookup 必达（单库无只读副本、READ COMMITTED 新语句快照可见已提交行）✓。
- caller 已存在 + Generator 暂时不可用 → P1-3 顺序（step 2 先于 step 4）返回 replay 而非 503 ✓；0003 现状确实 generator 检查先于 key 解析（L325-333 先于 L341-355）⇒ P1-3 属实、修复方向正确。
- 跨 tenant/project 隔离：lookup 谓词三键强制 + 部分唯一索引同键；scope 由 `Scoped` 提取（auth.rs L58-85），调用方不可自报；404 不泄露存在性 ✓。
- 唯一边角：lookup 404 窗口内身份被不同指纹的竞争者占用 → 409 CALLER_REQUEST_ID_REUSED——这是真实冲突而非竞态缺陷，恢复算法按 ART-04A §2.5.5 处置（禁止换 ID），契约已覆盖。

---

## 5. 新缺陷清单汇总

**P0：无。**

| ID | 级别 | 缺陷 | 冻结解 |
|---|---|---|---|
| C-1 | **P1** | §5.3.1 step 2 caller 命中分支无条件 RETURN，绕过 Idempotency-Key 冲突门禁（组合 4/11 + 异常块并发变体），与 §5.6 规则 2 / Q2 / AS2-28 自相矛盾 | §3.4 Option C：key 冲突作为成功返回前硬门禁，step 2 与 step 7 同构；身份冲突优先于 key 冲突报出 |
| C-2 | P2 | §5.2"按存储版本解析"与 §5.3.1"版本相等才比对"表述张力；MISMATCH 5xx 的不可重试语义未声明 | §4.1 修订建议 (a)(b)(c) |
| C-3 | P2 | serde_json 特性约束未写入契约；jsonb 对 1e2/100.0 的 input_sha256 差异未说明 | §4.2 修订建议 (a)(b) |
| C-4 | P2 | 指纹内容正确性的应用层信任边界未显式声明；APP 表级 INSERT/UPDATE 收窄未列为实施必答项 | §4.3 修订建议 |
| C-5 | P2 | 新 RAISE 的 ERRCODE 未冻结；token 优先级高于 ERRCODE 分类缺显式测试 | §4.4 修订建议（AS2-51） |

---

## 6. 对 ART-04G 的修订清单与验收矩阵增补

### 6.1 文档修订条目

1. §5.3.1 step 2：替换为 §3.4 Option C（读前查分支含 key 门禁）。
2. §5.3.1 step 7：异常块 caller 命中 + 指纹一致分支增加同构 key 门禁。
3. §5.6 规则 2：措辞改为"同 key 跨身份重放（无论 caller 身份是否存在）均被拦截：caller 未命中 → key 解析 409；caller 命中同指纹但 key 绑定他行 → 409"。
4. §5.2 版本纪律：澄清"按存储版本解析"归属 v2 引入门禁；MISMATCH 5xx 不可重试、指引 lookup；canonicalizer 存活期硬规则。
5. §5.2 payload 规则：冻结 serde_json 特性约束；补 jsonb/input_sha256 与指纹的三链路独立说明。
6. §5.7：显式信任边界声明 + APP 表级 DML 收窄必答项。
7. §5.3.1：冻结各 RAISE 的 ERRCODE（建议沿袭 0003：应用冲突 23505、门禁 P0001、校验 22023）。
8. §5.3.3：token 映射优先级显式声明（先于 ERRCODE 分类）。

### 6.2 验收矩阵增补（46 → 51 项）

| ID | 场景 | 预期 |
|---|---|---|
| AS2-28a | 原 AS2-28，请求的 caller_request_id 无既有命中 | 409 IDEMPOTENCY_KEY_REUSED（现有语义不变） |
| AS2-28b | 原 AS2-28，请求的 caller_request_id 命中既有 Job 且指纹一致但 key 绑定他行 | 409 IDEMPOTENCY_KEY_REUSED（原"现有语义不变"仅在门禁落地后成立） |
| AS2-47 | Job A(caller A, K1) + Job B(caller B, K2) 已存在；POST caller A + F_A + key K2 | 409 IDEMPOTENCY_KEY_REUSED；断言无新行、A/B 行 key/hash/fingerprint 零改写 |
| AS2-48 | 同 setup；POST caller B + F_B + key K1（对称方向） | 409 IDEMPOTENCY_KEY_REUSED |
| AS2-49 | N 并发混合 {caller A + F_A + K2} 与 {caller B + F_B + K1}（互拿错 key） | 全部 409，无行创建（与 AS2-14 合法并发对照） |
| AS2-50 | 异常块双索引竞争：本请求 INSERT 撞 T1(caller A, K5) 与 T2(caller B, K2) 提交行 | caller 重读命中 A + 指纹一致 + key 门禁 → 409（门禁在 exception 分支同样生效） |
| AS2-51（建议） | token 优先级：携带 GENERATOR_UNAVAILABLE token 的 P0001 错误 | 端口映射为 503 业务语义，不落入 IntegrityFailure(5xx) |

### 6.3 其余不变量复核确认（无需改动）

Q3 单行模型、Q4 作用域唯一不加 namespace、Q6 指纹排除清单（含 P1-1 expected_current_version_id）、Q8 禁止 Job 路径接入 idempotency_record、Q9 lookup 授权、Q10 竞态安全、Q11 持久性、Q12 SECURITY DEFINER 兼容、Q13 独立完成性、§5.7 迁移策略（P2-4 前提重验证）——全部经本轮源码核查维持原裁决。

---

## 7. 六项评审结论（对应任务要求）

1. **ART-04G §5 是否可作为 AS-2 唯一实施契约**：修订后方可。数据模型、指纹 v1、并发方案、lookup、三链路分离、迁移策略逐项经源码证据验证成立；但 §5.3.1 现文存在 P1 级自相矛盾（C-1），**不得按现文直接实施**。
2. **是否存在新的 P0/P1/P2**：无新 P0；1 个新 P1（C-1）；4 个新 P2（C-2..C-5）。
3. **是否需要修订 ART-04G**：是（§6.1 八条，全部为收敛性修订，不推翻任何已裁决方向）。
4. **是否需要增加验收项**：是（AS2-47/48/49/50 + AS2-28 拆分 + 建议 AS2-51，矩阵 46 → 51 项）。
5. **AR04-01 是否可以冻结**：完成 C-1 修订后冻结；按现文为 CHANGES REQUIRED。设计方向本身经源码证据验证成立。
6. **AR04-02 / AS-3 是否保持 NOT APPROVED**：**维持**。ART-04G §7.2 三条理由（依赖 AS-3 未冻结的 target Version 设计、端到端完成事务在 SeeCode 不可用下不可验证、不得默认连带通过）经核查无新证据推翻。

**最终裁决：CHANGES REQUIRED。**

---

## 8. 约束遵守声明

本评审过程中：

- 未修改任何代码、Migration、OpenAPI、SDK；
- 未创建实施分支（仅向现有评审文档分支 `docs/art-04g-as2-review-contract` 新增本文档）；
- 未合并 PR #8；
- 未启动 AS-2 / AS-3 代码实施；
- 未开发 Generator、Worker 或 SeeCode；
- 未启用生产生成。

全部核查均为只读操作；本文档为唯一新增交付物。
