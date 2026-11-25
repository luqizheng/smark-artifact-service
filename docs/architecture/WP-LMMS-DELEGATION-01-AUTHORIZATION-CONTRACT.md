# WP-LMMS-DELEGATION-01 — AI-PM → Artifact Service 授权委托契约（R1 修订草案 v0.2）

| | |
|---|---|
| 状态 | **草案（DRAFT）/ WAITING FOR INDEPENDENT SECURITY REVIEW**——本文档是**第二轮独立安全评审**的送审对象；评审通过前**不得标记 FINAL**、不得据以实现或对外宣称"已支持"。 |
| 修订 | v0.1 → v0.2（2026-11-25，R1）：按独立安全评审修正 P0-01～P0-05、补齐 P1-01～P1-07、冻结 Q1–Q6 裁决、更新 F-20～F-28。全部修订点附源码证据（见 CURRENT-STATE-AUDIT）。 |
| 前置 | 现状证据见 [CURRENT-STATE-AUDIT](WP-LMMS-DELEGATION-01-CURRENT-STATE-AUDIT.md)（基线：lmms tag `v1.8.2`@`93fc3a2e`、ai-pm GitHub `master`@`d0e72fa6`、as `main`@`b5095cf`）；安全挑战见 [SECURITY-REVIEW](WP-LMMS-DELEGATION-01-SECURITY-REVIEW.md)；落地步骤见 [IMPLEMENTATION-PLAN](WP-LMMS-DELEGATION-01-IMPLEMENTATION-PLAN.md) |
| 输入约束 | LMMS 1.8.2 只读不改；不假设其存在未证实接口；AS 不复制 AI-PM 项目角色体系；AS 不查 AI-PM 数据库 |

---

## 1. 目标与非目标

**目标**：让 AI-PM 把"经过业务授权的用户请求"以**短时、受限、可验证、操作绑定**的凭证委托给 Artifact Service，使后者能在 LMMS 身份与文件体系内完成 Artifact/Version/Job 操作，且不获得用户不应拥有的权限。

**非目标**：
- 不改变 LMMS 1.8.2 的任何接口或数据；
- 不把 AI-PM 的 Project 角色体系复制进 Artifact Service；
- 不开放 Generation Create 生产链路（generator 注册表保持 `unavailable`）；
- 不在本工作包内实现代码（先审计与设计）。

## 2. 方案比较（已裁决：候选 B）

### 候选 A：LMMS token-service 签发委托 Token（修复 resource token）

token-service 为 `POST /tokens/resource` 挂鉴权、扩 claims（`aud/tid/project/action[]/delegation 绑定`），AI-PM 持 service-jwt 调用签发。

- ✅ 最符合"LMMS 是身份与凭证中枢"的理想形态；撤销/内省可集中在 token-service。
- ❌ **需修改 lmms 仓库（本工作包禁止）**；1.8.2 的 resource token 端点未鉴权、claims 缺 aud/tid、TTL 输入无边界校验（F-28）、全平台无消费者（审计 §1.4），工作量与评审周期在 LMMS 侧。
- 结论：**列为 LMMS 后续版本需求（P1）**，不作为本次依赖。

### 候选 B（裁决采用）：AI-PM 签发委托断言 Token，AS 以 AI-PM JWKS 验签

AI-PM 在完成业务意图授权判断后，用**自有的短密钥签名服务**签发短时 operation-bound delegation token（`iss=ai-pm`，`aud=artifact-service`，含 tenant UUID/project/actions/操作绑定/jti/exp）；Artifact Service 把 AI-PM 列为受信签发方（拉取 AI-PM JWKS 验签 + 写路径在线撤销检查），验证通过后执行。

- ✅ 与职责边界精确对齐：**LMMS 负责身份**（用户 JWT 验签不变）、**AI-PM 负责业务授权决策并对其决策签名**、**AS 只验证凭证并执行**——AS 的 `DelegationVerifier` 端口本来就是"受信签发方"抽象，不绑定 LMMS；
- ✅ 不需要 LMMS 改动；复用已验证的机制形态（RS256 + JWKS + kid-strict，AI-PM 已用同套库验 LMMS token）；
- ✅ 签发决策点（project 成员资格 + 业务状态）与签名点同在 AI-PM，无跨服务竞态；
- ⚠️ 新增信任关系"AS 信任 ai-pm 作为委托签发方"——需要密钥托管、JWKS 端点、密钥轮换与缓存陈旧策略（§4/§6）；
- ⚠️ 撤销职责落在 AI-PM（Q1 裁决：写路径在线撤销检查 + 短 TTL，见 §5）。

### 候选 C：opaque token + AS 反查 AI-PM（introspection）

- ❌ 不作为默认（AS 新增 HTTP 出站依赖、可用性强耦合）；候选 B 落地后其**受控子集**（写路径 jti 撤销查询，Q1 裁决）按 §5.3 引入。

### 候选 D：AI-PM 直传用户 LMMS JWT 给 AS

- ❌ 否决。用户 JWT `aud=coder-micro-services` 为全平台共享值且 TTL 长（缺省 3600s），AS 无法从中获得 project 级授权（LMMS JWT 无 project 概念），且将用户长时凭证暴露进 AS 审计与日志面。工作包禁止清单第一类反模式。

**裁决：采用候选 B。** 以下契约按候选 B 展开；候选 A 的 claims 设计与其兼容，LMMS 未来上线后可平滑替换签发方（AS 侧只需增删受信 issuer，且 Q6 裁决 V1 单 issuer）。

## 3.0 上游用户 JWT 验证链（P0-02，delegation 签发的强制前置）

**事实基线（全部可证明，见审计 §1.2）**：
- LMMS v1.8.2 用户 JWT 的 `aud` 为签发侧硬编码常量 **`"coder-micro-services"`**（`services/token-service/src/issuer.rs:194`），保留 claims 过滤防止调用方覆盖（issuer.rs:211），配置目录无 audience 覆盖项；
- 该值是**全平台共享 audience**，**v1.8.2 不存在 AI-PM 专用 audience**；`issue_access_token`（issuer.rs:104-144）/`issue_refresh_token`（:146-181）/`issue_resource_token`（:241-280）均不设置标准 `aud`；
- AI-PM 现状 `require_bearer` 调用 `verify_token(token, jwks, None)`（`apps/pm-api/src/auth.rs:289`）——**audience 不校验**；验签库第三参本身就是 `expected_aud: Option<&str>`（`digua_common_jwt` 1.8.1 `verify.rs:65-94`），启用只需改调用，不需升级依赖。

**V1 验证链（delegation 签发接口的**唯一**准入路径，七项全过才可进入签发决策）**：

| # | 检查 | 规则 | 失败行为 |
|---|---|---|---|
| 1 | 验签 | `verify_token(token, jwks, Some("coder-micro-services"))`——audience 精确单值匹配 | 401 |
| 2 | purpose | `purpose == "user_login"`（签发侧权威声明，`issuer.rs:223-235` 强制）；未知/缺失/其他枚举值（`api_key`/`auth_code`/`service`/`resource`）一律拒绝 | 403 `IAM_PERM_3003` 族 |
| 3 | 跨租户标记 | `ct == false` | 403 `IAM_PERM_3004` |
| 4 | 服务身份 | `roles` 不含 `service` | 403 `IAM_PERM_3003` |
| 5 | 主体验证 | 经既有 `resolve_principal`/`require_active`（DB 实时 principal/tenant 映射与激活状态） | 既有错误码 |
| 6 | 身份类别 | **仅人类 principal** 可达签发接口：`dev-off`、`cpk`/client-credential、Agent/Service kind、未知 auth_mode 一律默认拒发 | 403 |
| 7 | aud 语义说明 | "缺少 aud 拒绝、aud 不匹配拒绝、aud 类型错误拒绝、仅凭签名有效+tid 存在认定有效——均不允许"。注意：`set_audience` 只拒绝缺失/异值 aud；**数组型 aud 若包含期望值会通过库检查**（jsonwebtoken 语义），故 V4 的"精确单值字符串"约束必须由验证器对 claims 做显式后置类型检查（见 §3.2 V4），不得只依赖库参数 | 401 |

**⛔ 实施阻断项（不得自行假设）**：v1.8.2 **不能证明存在 AI-PM 专用 audience**。V1 采用共享 aud `coder-micro-services` + purpose 强制的组合验证链是当前唯一可证明的形态；其弱点是"未来 LMMS 若新增携带同 aud 的其他 token 类型，需依赖 purpose 检查兜底"。**"audience 隔离依赖 purpose 而非 audience 本身"这一残余风险必须写入第二轮评审议题**；获得 AI-PM 专用 audience 属 LMMS 后续需求（随候选 A 批次）。在上述验证链落地并通过测试前，**delegation issuer 不得启用**（F-25 生效条件）。

**适用范围**：检查链 1–7 适用于**所有 delegation 签发接口**（§3.3）；AI-PM 自身普通业务端点的既有验签行为不属于本契约修订范围（其 aud 缺口已单独记为 F-25，随签发链修复同批处理）。

## 3.1 委托 Token 契约（候选 B）

### 3.1.1 claims（RS256 JWT，`aud=artifact-service`）

| Claim | 类型 | 必填 | 语义 | 来源/约束 |
|---|---|---|---|---|
| `iss` | string | ✅ | `ai-pm`（固定标识，AS 受信 issuer 清单条目） | 签发器常量 |
| `sub` | string | ✅ | 实际用户（LMMS `sub` 原样传递） | AI-PM `AuthenticatedIdentity.subject` |
| `aud` | string | ✅ | `artifact-service`（**精确单值，禁止数组**） | 签发器常量 |
| `tenant_id` | **uuid** | ✅ | **AI-PM 内部租户 UUID**（P0-01 修正：不再是 LMMS 数值 `tid` 镜像） | `AuthedPrincipal.tenant_id`（`auth.rs:174`，`ensure_tenant` 由 `(provider, tid)` 镜像生成，`auth.rs:520-546`）；AS `TenantScope.tenant_id` 即 `Uuid`（`scope.rs:10-13`）；**不得取自请求体** |
| `lmms_tid` | string | 可选 | **仅来源追溯**：LMMS 数值 `tid` 的字符串形式（如 `"1"`） | 可选审计字段；**AS 不得将其用于任何授权判定或 scope 装配**（类型与信任边界均不允许，审计 §3.2②） |
| `project_id` | uuid | ✅ | 目标项目 | AI-PM DB 验证过的成员关系，非客户端裸传 |
| `artifact_id` | uuid | 条件必填 | 操作绑定：限定单 Artifact 的委托 | 绑定规则见 §3.1.2；存在时 AS 校验目标一致性 |
| `operation_id` | string | 条件必填 | 操作绑定：稳定操作标识（如 `generation_job:{uuid}`、`version-commit:{artifact}:{n}`） | 绑定规则见 §3.1.2 |
| `caller_request_id` | string | 条件必填 | 操作绑定：调用方请求幂等键（AS-2 语义） | 创建 Job 时必填；与 `operation_id` 二选一或并存 |
| `actions` | string[] | ✅ | 允许的 AS 动作，⊆ AS `Action` 集（15 个精确动作，`action.rs:6-22`） | 由 §3.3 意图映射决定，**禁止发通配、禁止客户端提交**；非空、无重复 |
| `iat` / `exp` | int | ✅ | 签发/过期 | **TTL 建议 120 秒，硬上限 300 秒**（Q1 裁决）：`exp - iat ≤ 300` |
| `nbf` | int | 可选 | 生效时间 | 存在时 AS 必须校验 |
| `jti` | **uuid** | ✅ | 委托唯一标识（格式必须为 UUID） | 撤销查询主键；AS 审计记录 |
| `delegation_id` | uuid | ✅ | 委托记录 ID（与 AS `Delegation.delegation_id` 对齐） | AI-PM 侧签发记录外键 |
| `act` | object | 可选 | `{"sub":"ai-pm-service"}` 委托链（谁代表谁） | 区分服务代理与用户直发 |
| `biz` | object | 可选 | 来源业务标识：`{"task_run_id":...}` | 与 AS-2 durable caller identity 串联；不承担授权语义 |
| `purpose` | string | ✅ | `delegation`（固定） | 与 LMMS purpose 命名空间区分，AS 验证其值 |

示例 payload（注意 `tenant_id` 为 UUID；`lmms_tid` 仅追溯）：

```json
{
  "iss": "ai-pm", "sub": "u-8f2...", "aud": "artifact-service",
  "tenant_id": "0f8e...", "lmms_tid": "1", "project_id": "5c1e...",
  "artifact_id": null, "operation_id": "generation_job:9d2c...",
  "caller_request_id": "0b2e...",
  "actions": ["generation:request"],
  "iat": 1764030000, "exp": 1764030120, "jti": "9a77...", "delegation_id": "d-31...",
  "biz": {"task_run_id": "run-..."},
  "purpose": "delegation"
}
```

### 3.1.2 操作绑定与重放语义（P0-04）

**原则：短 TTL ≠ 一次性。** TTL 只限制时间窗，不限制窗内复用次数。防重放由三层共同提供：

1. **claims 操作绑定**（上表条件字段）：签发器按业务意图填入，AS 验证请求目标与绑定一致——`artifact_id` 存在时请求目标 artifact 必须相等；`operation_id` 存在时服务操作必须映射到同一稳定操作；不匹配 → 403 `SCOPE_DENIED`。绑定使"同一 token 被用于不同操作"失效。
2. **服务端单次消费 / 幂等键**（按接口区分）：

| 接口族 | 防护机制 | 重试语义 |
|---|---|---|
| Job 创建（`POST /generation-jobs`） | **服务端单次消费**：`caller_request_id` 部分唯一索引（migration 0005）+ `create_generation_job_with_input` v2 重放判定 | 同键同指纹 → `caller_request_replay` 零写入返回原 job；同键异指纹 → 409 `CALLER_REQUEST_ID_REUSED` |
| Artifact/Version 创建 | AS-1 `artifact_key`/Idempotency-Key 单事务幂等 | 重放返回原对象 |
| 取消（`cancel`） | `operation_id` 绑定具体 job + 状态机守卫（取消后该 job 无新写入）；重复取消幂等 | 合法网络重试 |
| 读操作（GET 族） | 无单次消费；靠 TTL + actions + scope 收敛 | 重试安全（天然幂等） |

3. **在线撤销检查**（Q1 裁决，写路径强制，见 §5.3）：`jti` 已撤销 → 403。

**五种场景的判定**（实现与测试必须逐项覆盖）：
| 场景 | 判定 |
|---|---|
| ① 同一操作的合法网络重试 | 绑定字段一致 + 幂等键命中 → 幂等成功（不产生第二次副作用） |
| ② 同一 Token 被用于不同操作 | 操作绑定不一致 → 403；未绑定字段而操作要求绑定 → 403（缺绑定视为不匹配，默认拒发/拒执行） |
| ③ 同一写操作被重复提交 | 服务端幂等键单次消费（表 2），非"信任 token 未用过" |
| ④ Job 取消后继续使用原 Token | 该 job 的写入被状态机阻断；token 对**其他**操作的残余能力按 §5.3 撤销策略收敛（取消成功时 AI-PM 撤销本次 delegation 的 jti） |
| ⑤ Worker 重试与新的业务操作 | Worker 不持有 delegation token（§3.3 红线 4）；新业务操作必须重新经 AI-PM 意图授权签发新 token |

**`caller_request_id` 的边界声明**：AS-2 的 caller 三元组幂等**仅覆盖 Job 创建路径**，不是所有 Artifact API 的通用重放防护；其余写路径的防重放 = 操作绑定 + 各自幂等机制 + TTL/撤销窗口。文档与实现不得宣称"caller_request_id 已提供全局防重放"。

### 3.2 AS 验证矩阵（实现 `DelegationVerifier` 时必须全部执行，任一失败 → 401/403）

逐项对应 P1-01 七步验签测试矩阵（每项一个测试用例，见 IMPLEMENTATION-PLAN §5）：

| # | 检查 | 规则 |
|---|---|---|
| V1 | 算法 | 固定 RS256；header `alg≠RS256` / `none` 拒绝（算法降级拒绝） |
| V2 | kid | header `kid` **必须存在**（缺失即拒绝，**不得回退 default key**——`digua_common_jwt` 现行为有回退，验证器必须覆盖）；`kid` 不在 JWKS → 拒绝；**未知 kid 触发一次强制刷新后复检，仍未知 → 拒绝**（§4.1） |
| V3 | issuer | `iss == "ai-pm"` 精确匹配（受信清单恰一项，Q6） |
| V4 | audience | `aud == "artifact-service"` 精确单值字符串匹配（数组/缺失/异值/类型错误拒绝） |
| V5 | 时间 | `exp > now`；`exp - iat ≤ 300s`；`iat ≤ now + 60s`（时钟偏差上限 **±60 秒**，超界拒绝——签发侧 iat 显著晚于当前时间的 token 拒绝）；`nbf` 存在时必须校验（`nbf > now + 60s` 拒绝） |
| V6 | jti | 存在且为合法 UUID 格式（非 UUID 拒绝） |
| V7 | purpose | `purpose == "delegation"` |
| V8 | actions | 非空、无重复、逐项 ∈ AS `Action` 枚举 15 值（`action.rs:46-65`）；含 `*`/通配样式/未知值/空串拒绝；类型非字符串数组拒绝 |
| V9 | scope 装配 | `tenant_id`（UUID）取自凭证 claim（P0-01）；`project_id` 以 `X-Project-Id`/请求体**定位**并与凭证核对 |
| V10 | 操作绑定 | §3.1.2：`artifact_id`/`operation_id`/`caller_request_id` 存在时与请求目标一致性校验 |
| V11 | 撤销（写路径） | §5.3：写动作执行前在线查询 jti 撤销状态；查询不可用 → **fail-closed** |
| V12 | 审计 | 每次验证结果（含拒绝）入 `audit_event`：`jti/delegation_id/subject/actions/判定原因`（不记 token 本体） |

错误码沿用现有：`UNAUTHENTICATED`(401，`artifact-contract/src/error.rs:43,70`)、`SCOPE_DENIED`(403，:71)、撤销服务不可用新增 `REVOCATION_UNAVAILABLE`(503，fail-closed 语义，见 §5.3)、AS-2 幂等族（409/5xx）。

**Header 语义（P1-03，Q2 裁决固化）**：
1. tenant/project **授权 scope 唯一来源 = 已验证 delegation claims**（tenant 恒取凭证，`apps/artifact-api/src/auth.rs:79`）；
2. `X-Project-Id` 头 / 请求体字段**只用于定位请求目标**（缺失/坏 UUID → 400，auth.rs:66-78），**不能构造可信 scope**；
3. 请求目标与 token scope 不一致 → 统一 403 `SCOPE_DENIED`（`Delegation::require`，`action.rs:100-105`）;
4. **不得使用请求体 tenant 字段覆盖凭证 tenant**——请求体中的 tenant id 仅作一致性提示或被忽略，绝不进入持久层 scope。

### 3.3 签发侧红线（P0-03：动作授权不能由客户端控制）

1. **禁止公共签发接口** `issue_delegation(claims)` 或任何允许调用方提交完整 claims（尤其 `actions`）的 HTTP 形态。签发只能发生在 AI-PM **内部受控业务意图接口**之后：
   - 每个业务意图（如"为此 task_run 启动生成"、"提交 version"、"读取 job 状态"）映射一个**服务端写死的精确 Action 集**（意图→Action 映射表为代码常量，随 15 动作逐一给出授权依据与测试，见 IMPLEMENTATION-PLAN §5）；
   - 服务端根据**认证用户（§3.0 验证链）、tenant、project（DB 成员关系）、目标资源、业务状态**（run 租约、项目闸门、任务归属）决定能否签发；
   - 默认拒发清单：未知动作、无意图映射、非人类身份（`dev-off`/`cpk`/Agent/Service）、§3.0 任一检查失败、目标资源不在认证用户可见范围。
2. AI-PM 是**唯一**委托签发方（Q6：V1 单 active issuer）；决策与签名在同一服务内；
3. AS 不解释 Project 角色；只做"动作 ∈ actions ∧ 资源 ∈ scope ∧ 目标符合绑定"判断；
4. LMMS 不参与委托链路（1.8.2 现实约束）；用户身份真实性由"AI-PM 已按 §3.0 验证 LMMS JWT"背书；
5. Worker/执行层**永不接触** delegation token：Job 创建后执行与凭证解耦（AS-2：输入快照 + lease + caller 三元组）；
6. 任何服务不得以调用方传入的 tenant/project 构造授权（Q2）。

## 4. 密钥与 JWKS（候选 B 的信任锚）

### 4.1 AI-PM 侧

- 专用 RS256 签名密钥对（与平台验签库兼容优先），**独立于 `AI_PM_SECRET_KEY`**（后者是数据加密密钥，`secret.rs` AES-GCM 用途）：独立环境变量/安全注入（建议 `AI_PM_DELEGATION_SIGNING_KEY`），仅私钥入 pm-api 进程，不入库不入 git 不入文档不入测试（对齐 F-21/F-22 教训）；
- 暴露 `GET /.well-known/jwks.json`（仅公钥、只读、内网、无敏感 header 依赖）；
- 轮换：双 kid 并存窗口 ≥ 2×最大 TTL（≥10 分钟）；泄露处置：立即下线受信 kid（AS 受信清单收缩）+ 强制 AS 侧刷新。

### 4.2 AS 侧 JWKS 缓存策略（P1-02，Q5 裁决固化）

| 项 | 策略 |
|---|---|
| 刷新节奏 | 周期刷新间隔 **5 分钟**（缺省配置；`digua_common_jwt` 缺省 3600s 不满足本契约） |
| **最大缓存陈旧时间** | **15 分钟**：`now - last_successful_refresh > 15min` 的缓存**不得继续作为可信密钥来源**——验签请求一律 fail-closed（401/503），直至刷新成功 |
| last-successful-refresh | 持久化于进程内（`fetched_at` 语义），每次验证路径可读取；对外健康端点可报告 |
| 刷新失败行为 | 窗口内（≤15min）继续用 last-known-good 键验签（轮换窗口容忍）；**超窗后刷新仍失败 → fail-closed**，不得"keeping stale"无限期服务（`cache.rs:146` 现行为必须覆盖） |
| 未知 kid | 触发**一次**强制刷新并复检（轮换窗口内新 kid 立即可用）；复检仍未知 → 拒绝；刷新受速率限制（≥60s 间隔）防打爆 |
| 缺失 kid | 直接拒绝（不回退 default key，覆盖 `verify.rs:80-82` 现行为） |
| 密钥轮换窗口 | 双 kid 并存 ≥10 分钟（≥2×max TTL）；AS 在窗口内任一次成功刷新即拿到新 kid |
| 被泄露/撤销 kid | AI-PM 下线 kid → AS 侧依赖：① 下次刷新后旧 kid 消失即拒；② 紧急场景走 §5.3 撤销（按 jti 批量撤销受影响 delegation） |
| 服务重启 | 缓存从空开始：启动期阻塞重试拉取（有界），失败则**受保护端点 fail-closed**（对齐 AI-PM 现行为 `main.rs:49-67`），不得以空缓存放行任何请求 |

## 5. Token 生命周期

| 阶段 | 行为 | 责任方 |
|---|---|---|
| 签发 | 业务意图通过 §3.3 授权判断 + §3.0 验证链 → 即时签发，**TTL 建议 120s，硬上限 300s**（Q1），一次业务操作一枚，操作绑定字段按意图填入 | AI-PM |
| 传递 | 仅存在于 AI-PM→AS 的单跳 `Authorization` 头；不落 AI-PM 库（签发记录存 jti/元数据，不存 token 本体）、不进 AS 持久层、不入日志 | 双方 |
| 验证 | §3.2 V1–V12（写路径含在线撤销检查） | AS |
| 使用 | 每个 AS API 请求独立携带；Job 创建时把 caller 三元组/输入快照固化进 DB（AS-2），**此后执行不再需要该 token** | AS |
| 过期 | TTL 自然过期，AS 无状态拒绝 | — |
| 撤销 | **Q1 裁决（冻结）**：写操作必须在线检查 jti 撤销状态（§5.3）；读操作依赖短 TTL，残余权限窗口 ≤ TTL（建议 120s，上限 300s） | AI-PM + AS |
| 审计 | AS 侧每次验证结果（含拒绝）入 `audit_event`；AI-PM 侧记录签发/撤销事件与业务关联（jti 级） | 双方 |

### 5.1 AI-PM delegation 状态存储（P0-05）

新表（ai-pm 自有 schema，独立迁移，AS 不加表）：

- `pm_delegation`（签发记录）：`jti`(PK)、`delegation_id`、`tenant_id`、`principal_id`、`project_id`、`actions`、`operation_id`/`artifact_id`/`caller_request_id`（绑定字段）、`issued_at`、`expires_at`、`status`（`active|revoked|expired`）；
- `pm_delegation_revocation`（撤销记录）：`jti`(PK/FK)、`revoked_at`、`revoked_by`、`reason`（`user_removed|user_disabled|admin_emergency|job_cancelled|key_incident`）、`retain_until`；
- 签发即写 `pm_delegation`（写入失败 → 不签发，fail-closed）；撤销 = `status` 翻转 + 撤销记录写入；
- **保留期限**：撤销记录保留 ≥ **7 天**（≥ 足量覆盖最大 TTL 窗口 + 争议追溯期）；签发记录保留按 AI-PM 既有审计留存策略；过期清理为后台维护任务。

### 5.2 撤销查询接口（AI-PM 提供，AS 消费）

- `GET /internal/delegation-revocations/{jti}` → `{revoked: bool}`（内部端点，service 身份认证，不暴露公网）；
- 响应仅含布尔与必要时 reason 代码，**不回显 token 或完整 claims**。

### 5.3 AS 写路径检查时序与失效行为（Q1 裁决）

1. **检查点**：V1–V10 全过后、**业务写入执行前**，对 `generation:request / generation:cancel / artifact:create / artifact:update_metadata / artifact:archive / artifact:delete / version:create / version:commit / version:set_current / reference:write / reference:remove`（全部写动作）执行一次 jti 撤销查询；读动作（`*:read`/`output:download`）**不查**（残余窗口 ≤ TTL，Q1 明示接受）。
2. **fail-closed**：撤销查询超时/不可用/非 200 → 拒绝写入，503 `REVOCATION_UNAVAILABLE`（**不是 401**，语义=暂不可判定而非凭证无效）；**禁止**任何"查询失败视为未撤销"的 fail-open 分支。
3. **时延与缓存**：写路径每请求一次查询，**禁止进程级缓存撤销结果**（撤销即时性优先于时延；对齐 AI-PM cpk 通道"每请求直查、无缓存窗口"先例，`auth.rs:226-230`）；查询超时上限建议 2s。
4. **网络分区/超时**：AI-PM 不可达 → 全部写路径 503（fail-closed），读路径继续（TTL 收敛）——可用性影响如实声明：**AI-PM 停机 = AS 写平面停机**（评审已知情接受；如不可接受需回到候选 C 全量 introspection，属架构升级）。
5. **重启**：AS 重启不缓存撤销状态，首个写请求即重新查询。
6. **已签发 token 的失效行为矩阵**：

| 事件 | 新签发 | 已签发 token（写路径） | 已签发 token（读路径） |
|---|---|---|---|
| 用户被移出 Project | 立即拒发（DB 成员关系实时） | AI-PM 在移除事务内撤销该用户该 project 全部 active jti → 下一次写请求 403 | 残余 ≤ TTL 自然过期 |
| 用户被禁用（is_active=false） | 立即拒发（`require_active`） | 同上：禁用事务内批量撤销 → 写路径即时 403 | 残余 ≤ TTL |
| 管理员紧急撤销 | 按需（封禁用户/项目） | jti/按用户/按项目批量撤销 → 即时 403 | 残余 ≤ TTL |
| Job 取消 | 不受影响 | AI-PM 对本次 delegation jti 执行撤销（§3.1.2 场景④） | 残余 ≤ TTL |
| 签名密钥泄露 | 停用 kid | 全量 jti 撤销 + AS 受信 kid 收缩 | kid 下线后刷新即拒 |

**与 WP §八挑战 1 的显式答案（Q1 后）**：写路径残余权限窗口 = 一次撤销查询间隔（≈0，撤销即时生效）；读路径残余窗口 ≤ TTL（建议 120s / 上限 300s）。"TTL 足够短所以无需撤销"**不成立**，不再作为设计选项。

## 6. 真实调用时序

### 6.1 主时序（WP §六 11 步）

```mermaid
sequenceDiagram
    autonumber
    actor U as 用户
    participant GW as lmms api-gateway
    participant IAM as lmms iam/token
    participant PM as AI-PM (pm-api)
    participant AS as Artifact Service
    participant W as Worker(执行层)
    participant FS as lmms file-service

    U->>GW: 登录 (POST /iam/auth/login)
    GW->>IAM: 密码校验 → 签发用户 JWT (aud=coder-micro-services, tid, sub, jti)
    IAM-->>U: RS256 JWT
    U->>PM: 业务请求 (Bearer 用户JWT)
    PM->>PM: §3.0 验证链 (aud 强制 + purpose=user_login + ct/roles); DB 验证成员资格与业务状态
    PM->>PM: 意图映射 → 签发 operation-bound delegation (tenant_id=内部UUID, actions=精确集, jti, exp≤+120s建议/300s上限)
    PM->>PM: 写 pm_delegation 签发记录 (fail-closed)
    PM->>AS: 创建 Generation Job (Bearer delegation; caller_request_id)
    AS->>AS: V1-V10 验证; 写路径: 查询 ai-pm 撤销端点 (fail-closed)
    AS->>AS: 落库 job + caller 三元组 (不可变)
    AS-->>PM: 201 job_id (或 caller_request_replay 幂等返回)
    W->>AS: claim_generation_job(worker_id, lease, adapters)  [DB 函数, 无用户凭证]
    AS-->>W: job 输入快照 + 一次性 lease token
    W->>FS: 上传产物 (Bearer service-jwt aud=file-service, tid=job 租户)
    FS-->>W: file_id
    AS->>FS: 校验/finalize 产物对象 (协议 P1 就绪前: 直传+补偿)
    AS->>AS: Version 创建 + 状态机推进 (job succeeded)
    PM->>AS: 查询 job (新 delegation token, generation:read)
    PM->>PM: 业务验收 (人工闸门: DB 验证 human + 产品成员)
```

> 注：第 13-14 步（file-service 写入）在 1.8.2 下的现实形态是"AS/执行层持 `aud=file-service` service-jwt 调 `/file/upload`，DB 事务失败补偿删对象"（LMMS 自身即此模式，file.rs:98-110）；**该链路受 F-24 信任边界限制（`unwrap_or(1)` 回落、user_id 自声明），接入前必须与 LMMS 约定受信调用方契约并保持 fail-closed**（P1-05）。upload-intent/finalize 两段协议是 LMMS 后续需求（P1），就绪前 AS `StoragePort` 的 finalize 语义以"上传 + 校验 + 补偿"适配器近似，且该适配器实现前 Version 链路保持 503 fail-closed 不变。

### 6.2 长任务（执行时间 > Token 有效期）

**本契约下问题被架构消解，而非用续期解决：**

1. Job 创建即固化：输入快照（`input_payload/input_sha256`）、caller 身份三元组、deadline/max_execution_seconds 全部落库（AS-2/0003/0005 已实现）——**执行不再依赖任何用户会话或委托 token**；
2. Worker 以 DB 角色 + 单次 lease token 执行，不持用户 delegation（红线 5）；
3. AS/执行层访问 file-service 用**服务身份** service-jwt（`aud=file-service`，TTL 1h，`ServiceJwtCache` 自动刷新）——与用户委托彻底解耦，天然覆盖长任务；
4. **禁止**以延长 delegation token TTL 覆盖长任务；若未来出现"用户身份必须参与执行"的场景，正确做法是执行到该步时由 AI-PM 重新走意图授权签发一枚窄绑定 token（如仅 `version:commit` + artifact_id + operation_id 绑定），而不是续期原票；
5. 取消：`POST /generation-jobs/{id}/cancel` 需携带**新签发的** delegation（`generation:cancel` + 同 scope + `operation_id` 绑定该 job）；取消成功 → AI-PM 撤销相关 jti；取消后 job 状态机保证不再产出新 Version（触发器守卫）。

### 6.3 失败与恢复时序要点

- AS 崩溃恢复：job 行 + 输入快照在 DB，worker `recover-once` 恢复租约——不依赖原用户会话（挑战 8 的结构保证）；
- AI-PM 重试：同 `caller_request_id` + 同指纹 → AS 零写入幂等返回原 job；不同指纹 → 409；
- AI-PM 不可用：AS **写路径 fail-closed**（503 `REVOCATION_UNAVAILABLE`，§5.3）；读路径继续（TTL 收敛）；
- LMMS 暂时不可用：AI-PM 登录/JWKS 拉取失败 → AI-PM fail-closed 拒绝（现状已如此）；AS 验签走 AI-PM JWKS（§4.2 陈旧策略内容忍，超窗 fail-closed），仅 file 链路依赖 file-service——挑战 9 逐项矩阵见 SECURITY-REVIEW。

## 7. Q1–Q6 冻结裁决（R1 独立安全评审）

以下为本轮评审**冻结的裁决建议**（frozen adjudications）。**契约整体保持 DRAFT**：这些裁决仍属"评审裁决建议"，除非所有必要实现细节都已明确并经过**第二轮独立安全复审**，本契约不得标记为 FINAL。

| # | 裁决 | 内容 | 落点 |
|---|---|---|---|
| Q1 | **写操作在线撤销** | 写操作必须在线检查 jti 撤销状态；撤销查询不可用 fail-closed；delegation **TTL 建议 120 秒，绝不超过 300 秒**；读操作依赖短 TTL（最大残余权限窗口 = TTL ≤ 300s）；移出 Project/禁用/紧急撤销的失效行为见 §5.3 矩阵 | §3.1.1、§5 |
| Q2 | **Header 仅定位** | `X-Project-Id` 仅资源定位与一致性检查输入；claims 是唯一授权依据；目标与 scope 不一致统一拒绝；请求体不得覆盖凭证 tenant | §3.2 V9 |
| Q3 | **全量 delegation** | 所有受保护 API——包括只读 GET——均要求 delegation（V1 沿用现有 15 个精确 Action）；禁止 wildcard；不允许经只读接口绕过 Token 验证 | §3.1.1、§3.2 |
| Q4 | **15 精确动作** | 沿用 `action.rs:6-22` 的 15 个精确 Action；不允许 wildcard；每个 Handler/Service 的 Action 映射必须有测试 | §3.2 V8 |
| Q5 | **JWKS 缓存陈旧上限** | 最大缓存陈旧时间明确化（15 分钟，§4.2）；超期缓存不得作为可信密钥来源；刷新失败超窗 fail-closed；未知 kid 触发一次刷新；缺失 kid 拒绝 | §4.2 |
| Q6 | **V1 单 issuer** | V1 只允许一个 active issuer：AI-PM；LMMS 未来上线委托签发（候选 A）时按"受信 issuer 清单"机制平滑过渡，届时需复审 | §3.3、§2 |

## 8. 遗留开放点（非裁决项，随实现细化）

| # | 问题 | 状态 |
|---|---|---|
| O-1 | AI-PM `pm_delegation` 表迁移的具体 DDL 与索引设计 | 实施计划阶段 1 任务产出，随实现评审 |
| O-2 | 撤销查询端点的认证形态（复用 service-jwt 模式 or 内部 mTLS——LMMS 无 mTLS，V1 用 service-jwt） | 阶段 1 细化 |
| O-3 | `act` 委托链字段是否 V1 启用 | 建议 V1 恒为服务代理形态，可先固定不签；复审确认 |
| O-4 | 意图接口的 REST 形态与错误码表 | 阶段 1 设计产出 |
| O-5 | **AS 侧 jti 消费绑定表（`artifact.delegation_token_use`，本 PR 迁移 0006）**：本实现为"同一 jti 不同请求 → 硬拒 / 并发重放 → DB 原子仲裁"提供了超出 §3.1.2 分层的纵深防御层（含 purge 护栏）。与"AS 不加表"（§5.1）的冻结裁决冲突，**提请第二轮评审裁决**：保留（修订契约，作为声明绑定的服务端兜底 + 复用取证）或移除（AS-4 接线前可无损删除，声明层绑定 + 各端点幂等 + 在线撤销已覆盖同等威胁）。**默认未接线**：无任何路由消费该表 | 本 PR 附带，待裁决 |
| O-6 | 请求指纹扩展 claim `biz.request_hash`（canonical fingerprint 规范冻结于 `canonical_request_fingerprint`）：为意图接口落地前提供内容级绑定；若采纳需并入 §3.1.1 claims 表并由 AI-PM 签发侧实现 | 本 PR 附带（AS 侧解析 + 校验 + 测试），待裁决 |

---

## 9. 实现与测试证据映射（本次提交，待独立评审）

> 本节为**实现附注**（随代码提交）：验证器与 JWKS 缓存已按 §3.1.1/§3.2/§4.2 实现；
> 全部条目可在 PR diff 与 CI 测试运行中独立复核。**实现 ≠ 已通过安全评审**；
> AS-4 接线（§5.3 写路径撤销检查、V10 目标一致性、意图接口）仍为后续任务。

| 契约条目 | 实现 | 测试证据 |
|---|---|---|
| V1 算法固定 RS256 | `AiPmDelegationVerifier::verify` header.alg 检查 | s02 签名拒绝族；`ai_pm` 单测 |
| V2 kid 存在/未知 kid 单次限频刷新/缺失 kid 拒绝 | header.kid 必填；`JwksCache::lookup` 60s 限频 | s05（含出站次数断言）、s05b（轮换双活） |
| V3 issuer 精确匹配（Q6 单 issuer） | 受信清单匹配；配置层强制恰一项 | s03；`config` 单测 `multiple_active_issuers_are_rejected` |
| V4 audience 精确单值 | `set_audience` + 固定字符串反序列化（数组型即拒） | s04 |
| V5 时间规则（±60s、TTL≤300、exp>iat、nbf） | `validate_time_claims` + Validation leeway=60 | s07/s08/s09；`ai_pm` 单测（边界 300/301） |
| V6 jti 必须 UUID | `claims.jti.parse::<Uuid>()` | `ai_pm` 单测 + s 族 |
| V7 purpose == delegation | 固定值精确比较 | 拒绝矩阵 |
| V8 actions 非空/无重复/∈15 动作 | 逐项 parse + 重复检测 | s12；`ai_pm` 单测 |
| V9 scope 装配（P0-01 tenant UUID） | claims→`TenantScope`；header 仅定位 | s10 跨租户 404、s11 claim/头不一致 403、s18 缺头 400 |
| V10 操作绑定字段携带 | `artifact_id`/`operation_id`/`caller_request_id` 解析（UUID 强校验）；目标一致性属 AS-4 接线 | s16（指纹扩展层）；V10 服务层用例待接线 |
| V11 写路径在线撤销（Q1） | `DelegationRevocationChecker` 端口 + `FailClosedRevocationChecker`（端点未上线前写路径 fail-closed 503 `REVOCATION_UNAVAILABLE`） | 端口单测；客户端实现属任务 1.7 |
| V12 审计 | 复用 `audit_and_enrich_layer`（401/403 拒绝入 `audit_event`，无 token 本体） | 错误消息无 token 断言（rejects_with） |
| §4.2 JWKS 缓存策略（Q5） | 5min 软刷新 / 15min 最大陈旧 / 60s 刷新限频 / 重启空缓存 fail-closed | `jwks` 单测 + 常量契约测试；s06 断供 503 `DELEGATION_VERIFIER_UNAVAILABLE` |
| §3.1.2 场景①③⑤（合法重试/重复提交/worker 无 token） | 绑定表存储（O-5）+ 既有 AS-1/AS-2 幂等 | s13/s14/s15/s19/s20；AS-2 既有回归 |
| §3.1.2 场景④（取消后残余） | TTL 收敛 + jti 改绑硬拒 | s17 |
| 配置缺省全拒 | 空 issuer 清单 → `ClosedDelegationVerifier` | s21；`default_verifier_is_fail_closed` 回归 |

测试运行：`cargo test --workspace`（226 通过；本机 Docker PG 高并发下 `sqlx::test` 建库可能 `PoolTimedOut`，属环境饱和——`--test-threads=8` 或 CI Linux 均稳定）。
