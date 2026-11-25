# WP-LMMS-DELEGATION-01 — AI-PM → Artifact Service 授权委托契约（草案 v0.1）

| | |
|---|---|
| 状态 | **草案（DRAFT）**——本文档是待评审的设计候选，**不是最终规范**。任何条款在评审确认前不得据以实现或对外宣称"已支持"。 |
| 前置 | 现状证据见 [CURRENT-STATE-AUDIT](WP-LMMS-DELEGATION-01-CURRENT-STATE-AUDIT.md)；安全挑战见 [SECURITY-REVIEW](WP-LMMS-DELEGATION-01-SECURITY-REVIEW.md)；落地步骤见 [IMPLEMENTATION-PLAN](WP-LMMS-DELEGATION-01-IMPLEMENTATION-PLAN.md) |
| 输入约束 | LMMS 1.8.2 只读不改；不假设其存在未证实接口；AS 不复制 AI-PM 项目角色体系；AS 不查 AI-PM 数据库 |

---

## 1. 目标与非目标

**目标**：让 AI-PM 把"经过业务授权的用户请求"以**短时、受限、可验证**的凭证委托给 Artifact Service，使后者能在 LMMS 身份与文件体系内完成 Artifact/Version/Job 操作，且不获得用户不应拥有的权限。

**非目标**：
- 不改变 LMMS 1.8.2 的任何接口或数据；
- 不把 AI-PM 的 Project 角色体系复制进 Artifact Service；
- 不开放 Generation Create 生产链路（generator 注册表保持 `unavailable`）；
- 不在本工作包内实现代码（先审计与设计）。

## 2. 方案比较（必须裁决的架构选择）

### 候选 A：LMMS token-service 签发委托 Token（修复 resource token）

token-service 为 `POST /tokens/resource` 挂鉴权、扩 claims（`aud/tid/project/action[]/delegation 绑定`），AI-PM 持 service-jwt 调用签发。

- ✅ 最符合"LMMS 是身份与凭证中枢"的理想形态；撤销/内省可集中在 token-service。
- ❌ **需修改 lmms 仓库（本工作包禁止）**；1.8.2 的 resource token 端点未鉴权、claims 缺 aud/tid、全平台无消费者（审计 §1.4），工作量与评审周期在 LMMS 侧；file/网关的 `tid` 强制校验还会拒绝现有形状的 resource token。
- 结论：**列为 LMMS 后续版本需求（P1）**，不作为本次依赖。

### 候选 B（推荐）：AI-PM 签发委托断言 Token，AS 以 AI-PM JWKS 验签

AI-PM 在完成项目授权判断后，用**自有的短密钥签名服务**签发短时 delegation token（`iss=ai-pm`，`aud=artifact-service`，含 tid/project/actions/jti/exp）；Artifact Service 把 AI-PM 列为受信签发方（拉取 AI-PM JWKS 验签），验证范围后执行。

- ✅ 与职责边界精确对齐：**LMMS 负责身份**（用户 JWT 验签不变）、**AI-PM 负责业务授权决策并对其决策签名**、**AS 只验证凭证并执行**——AS 的 `DelegationVerifier` 端口本来就是"受信签发方"抽象，不绑定 LMMS；
- ✅ 不需要 LMMS 改动；复用已验证的机制形态（RS256 + JWKS + kid-strict，AI-PM 已用同套库验 LMMS token；LMMS `ServiceJwtCache` 证明 aud 绑定短时 JWT 模式在平台内成熟可用）；
- ✅ 签发决策点（project 成员资格）与签名点同在 AI-PM，无跨服务竞态；
- ⚠️ 新增信任关系"AS 信任 ai-pm 作为委托签发方"——需要密钥托管、JWKS 端点、密钥轮换设计（本契约 §4/§6）；
- ⚠️ 撤销职责落在 AI-PM（TTL 极短 + 可选在线撤销查询），见 §5。

### 候选 C：opaque token + AS 反查 AI-PM（introspection）

AI-PM 签发不透明令牌，AS 每次调用在线回查 AI-PM。

- ✅ 撤销即时性最好。
- ❌ AS 需新增 HTTP 出站依赖（当前无任何外呼）；可用性强耦合（AI-PM 抖动 → AS 全拒绝，LMMS 不可用时问题复写一遍）；每请求一次 RTT。
- 结论：不作为默认；候选 B 落地后若强撤销需求出现，可作为可选增强（仅 cancel/写路径在线校验）。

### 候选 D：AI-PM 直传用户 LMMS JWT 给 AS

- ❌ 否决。用户 JWT `aud=coder-micro-services` 全域有效且 TTL 长（3600s），AS 无法从中获得 project 级授权（LMMS JWT 无 project 概念），等于把 AS 的授权判断架空；且长时凭证落进 AS 审计与日志面。这正是工作包禁止清单第一类反模式。

**裁决请求：采用候选 B。** 以下契约按候选 B 展开；候选 A 的 claims 设计与其兼容，LMMS 未来上线后可平滑替换签发方（AS 侧只需增删受信 issuer）。

## 3. 委托 Token 契约（候选 B）

### 3.1 claims（RS256 JWT，`aud=artifact-service`）

| Claim | 类型 | 必填 | 语义 | 来源/约束 |
|---|---|---|---|---|
| `iss` | string | ✅ | `ai-pm`（固定标识，AS 受信 issuer 清单条目） | 签发器常量 |
| `sub` | string | ✅ | 实际用户（LMMS `sub` 原样传递） | AI-PM `AuthenticatedIdentity.subject` |
| `aud` | string | ✅ | `artifact-service` | 签发器常量 |
| `tid` | int64 | ✅ | 租户（= LMMS `tid` 镜像） | `AuthedPrincipal.tenant_id`；**不得取自请求体** |
| `project_id` | uuid | ✅ | 目标项目 | AI-PM DB 验证过的成员关系，非客户端裸传 |
| `artifact_id` | uuid | 可选 | 限定单 Artifact 的更窄委托（如续传/替换场景） | 存在时 AS 校验目标一致性 |
| `actions` | string[] | ✅ | 允许的 AS 动作，⊆ AS `Action` 集（如 `artifact:create`,`version:create`,`generation:request`,`generation:cancel`,`artifact:read`） | AI-PM 按操作语义最小化授予，**禁止发通配** |
| `iat` / `exp` | int | ✅ | 签发/过期 | **TTL 2–5 分钟**（与 ART-01 §5 既定方向一致）；`exp - iat ≤ 300s` |
| `jti` | uuid | ✅ | 委托唯一标识 | AS 审计记录；撤销查询主键 |
| `delegation_id` | uuid | 建议 | 委托记录 ID（与 AS `Delegation.delegation_id` 对齐） | AI-PM 侧可追溯 |
| `act` | object | 可选 | `{"sub":"ai-pm-service"}` 委托链（谁代表谁） | 区分服务代理与用户直发 |
| `biz` | object | 建议 | 委托来源业务标识：`{"task_run_id":...,"caller_request_id":...}` | 与 AS-2 durable caller identity 串联 |
| `purpose` | string | ✅ | `delegation`（固定） | 与 LMMS purpose 命名空间区分，AS 验证其值 |

示例 payload：

```json
{
  "iss": "ai-pm", "sub": "u-8f2...", "aud": "artifact-service",
  "tid": 1, "project_id": "5c1e...", "actions": ["artifact:create", "version:create"],
  "iat": 1764030000, "exp": 1764030240, "jti": "9a77...", "delegation_id": "d-31...",
  "biz": {"task_run_id": "run-...", "caller_request_id": "0b2e..."},
  "purpose": "delegation"
}
```

### 3.2 AS 验证步骤（实现 `DelegationVerifier` 时必须全部执行，任一失败 → 401/403）

1. RS256 验签：kid-strict， issuer=ai-pm 的 JWKS（缓存 + 轮换容错），对齐 AS 已有 kid-strict 生态；
2. `iss ∈ 受信清单` 且 `aud == "artifact-service"`（**显式比较，不得复用 aud=None 的宽松路径**）；
3. `exp > now` 且 `exp - iat ≤ 300s`（超 TTL 拒收，防"自签长票"）；
4. `purpose == "delegation"`；`actions` 非空且逐项 ∈ AS `Action` 枚举；
5. 组装 `Delegation`：`scope.tenant_id = tid`（凭证）；project 以 `X-Project-Id`/请求体为准并与**凭证核对**（不一致 403 `SCOPE_DENIED`）——与现有 `Scoped` 提取器语义一致；
6. `require(action, scope)` 在每个服务方法入口执行（现状代码已就位，不变）；
7. 审计事件记录 `jti/delegation_id/subject/actions`（不记 token 本体）。

错误码沿用现有：`UNAUTHENTICATED`(401)、`SCOPE_DENIED`(403)、AS-2 幂等族（409/5xx）。

### 3.3 职责边界规则（红线）

1. AI-PM 是**唯一**委托签发方；签发前必须完成 DB 验证的项目成员资格判断（`require_project_visible` / MCP `authorize`），**决策与签名在同一服务内**；
2. AS 不解释 Project 角色；只做"动作 ∈ actions ∧ 资源 ∈ scope"判断；
3. LMMS 不参与委托链路（1.8.2 现实约束）；用户身份真实性由"AI-PM 已验证 LMMS JWT"背书，AS 信任 AI-PM 的签发即信任该背书；
4. Worker/执行层**永不接触** delegation token：Job 创建后执行与凭证解耦（AS-2 已实现：输入快照 + lease 机制 + caller 身份三元组）；
5. 任何服务不得以调用方传入的 tenant/project 绕过验证（三方现状均已满足，回归测试锁定）。

## 4. 密钥与 JWKS（候选 B 的信任锚）

- AI-PM 持专用签名密钥对（与 `AI_PM_SECRET_KEY` 数据加密密钥**分开**），建议 Ed25519 或 RS256（与平台验签库兼容性优先选 RS256）；私钥仅存于 pm-api 进程 env/secret 注入，不入库不入 git（对齐 F-22 教训）；
- pm-api 暴露 `GET /.well-known/jwks.json`（仅公钥、只读、内网）；AS 配置 `delegation issuer = ai-pm + jwks url`，kid-strict 缓存；
- 轮换：双 kid 并存窗口 ≥ 2×最大 TTL（≥10 分钟），参照 LMMS keystore 模式但规模小得多；
- 泄露处置：轮换 + 全量拒绝旧 kid（AS 受信 kid 清单即时收缩）。

## 5. Token 生命周期

| 阶段 | 行为 | 责任方 |
|---|---|---|
| 签发 | 业务请求通过授权判断 → 即时签发，TTL ≤ 300s，**一次业务操作一枚**，不缓存不重用 | AI-PM |
| 传递 | 仅存在于 AI-PM→AS 的单跳 `Authorization` 头；不落 AI-PM 库、不进 AS 持久层、不入日志 | 双方 |
| 验证 | §3.2 七步 | AS |
| 使用 | 每个 AS API 请求独立携带；Job 创建时把 `caller_request_id`/指纹固化进 DB（AS-2），**此后执行不再需要该 token** | AS |
| 过期 | TTL 自然过期，AS 无状态拒绝；无需清理 | — |
| 撤销 | 分级：① 默认靠极短 TTL（撤销生效窗 ≤ 5 分钟）；② 需即时撤销的强场景（密钥泄露/管理员封禁），AI-PM 提供撤销查询端点（jti 黑名单），AS 对**写路径**可选在线校验（候选 C 增强的受控引入）；③ 用户在 AI-PM 被移出项目/封禁 → AI-PM 停止签发新 token（已签发等自然过期） | AI-PM（+AS 可选） |
| 审计 | AS 侧每次验证结果（含拒绝）入 `audit_event`；AI-PM 侧记录签发事件与业务关联 | 双方 |

**与 WP §八挑战 1 的显式答案**：用户被撤销 Project 权限后，已签发 token 在 ≤TTL 窗口内仍可用于**已授权动作**——这是短 TTL 设计接受的残余风险（SECURITY-REVIEW 挑战 1 给出等级与缓释）；撤销即时性需求若被评审为 P0，则启用 ②。

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
    PM->>PM: 本地 JWKS 验签用户JWT; DB 验证 project 成员资格
    PM->>PM: 签发 delegation token (iss=ai-pm, aud=artifact-service, tid, project_id, actions, jti, exp≤+300s)
    PM->>AS: 创建 Generation Job (Bearer delegation token; caller_request_id)
    AS->>AS: 验证委托 (7 步); 落库 job + caller 三元组 (不可变)
    AS-->>PM: 201 job_id (或 caller_request_replay 幂等返回)
    W->>AS: claim_generation_job(worker_id, lease, adapters)  [DB 函数, 无用户凭证]
    AS-->>W: job 输入快照 + 一次性 lease token
    W->>FS: 上传产物 (Bearer service-jwt aud=file-service, tid=job 租户)
    FS-->>W: file_id
    AS->>FS: 校验/finalize 产物对象 (协议 P1 就绪前: 直传+补偿)
    AS->>AS: Version 创建 + 状态机推进 (job succeeded)
    PM->>AS: 查询 job (新 delegation token 或只读凭证)
    PM->>PM: 业务验收 (人工闸门: DB 验证 human + 产品成员)
```

> 注：第 13-14 步（file-service 写入）在 1.8.2 下的现实形态是"AS/执行层持 `aud=file-service` service-jwt 调 `/file/upload`，DB 事务失败补偿删对象"（LMMS 自身即此模式，file.rs:98-110）；upload-intent/finalize 两段协议是 LMMS 后续需求（P1），就绪前 AS `StoragePort` 的 finalize 语义以"上传 + 校验 + 补偿"适配器近似，且该适配器实现前 Version 链路保持 503 fail-closed 不变。

### 6.2 长任务（执行时间 > Token 有效期）

**本契约下问题被架构消解，而非用续期解决：**

1. Job 创建即固化：输入快照（`input_payload/input_sha256`）、caller 身份三元组、deadline/max_execution_seconds 全部落库（AS-2/0003/0005 已实现）——**执行不再依赖任何用户会话或委托 token**；
2. Worker 以 DB 角色 + 单次 lease token 执行，不持用户 delegation（候选 B 红线 4）；
3. AS/执行层访问 file-service 用**服务身份** service-jwt（`aud=file-service`，TTL 1h，`ServiceJwtCache` 自动刷新）——与用户委托彻底解耦，天然覆盖长任务；
4. **禁止**以延长 delegation token TTL 覆盖长任务（WP §六红线）；若未来出现"用户身份必须参与执行"的场景，正确做法是执行到该步时由 AI-PM 重新签发一枚窄 token（如仅 `artifact:version:finalize` + artifact_id 绑定），而不是续期原票；
5. 取消：`POST /generation-jobs/{id}/cancel` 需携带新的有效 delegation（`generation:cancel` + 同 scope）；取消后 job 状态机保证不再产出新 Version（触发器守卫），已签发 delegation 的残余窗口见 SECURITY-REVIEW 挑战 5。

### 6.3 失败与恢复时序要点

- AS 崩溃恢复：job 行 + 输入快照在 DB，worker `recover-once` 恢复租约——不依赖原用户会话（挑战 8 的结构保证）；
- AI-PM 重试：同 `caller_request_id` + 同指纹 → AS 零写入幂等返回原 job（`caller_request_replay`）；不同指纹 → 409 `CALLER_REQUEST_ID_REUSED`；
- LMMS 暂时不可用：AI-PM 登录/JWKS 拉取失败 → AI-PM fail-closed 拒绝（现状已如此）；AS 不依赖 LMMS 在线（验签走 AI-PM JWKS），仅 file 链路依赖 file-service——挑战 9 逐项矩阵见 SECURITY-REVIEW。

## 7. 开放问题（待评审裁决，不阻塞草案方向）

| # | 问题 | 选项 | 建议 |
|---|---|---|---|
| Q1 | 撤销即时性是否为 P0 | 仅短 TTL（默认）/ 增加写路径在线撤销查询 | 先短 TTL 上线，撤销端点作为快速跟进项 |
| Q2 | `X-Project-Id` 头去留（ART-02 OPEN-ISSUES #6） | 保留作一致性校验 / project 改从 claims 取 | 保留校验语义（不一致 403），claims 为准后评估降级 |
| Q3 | 只读操作（GET job/artifact）是否也要求 delegation token | 同权 / 只读放宽（如 AI-PM 服务只读票） | 同权（行为与审计一致），性能允许后考虑只读短票 |
| Q4 | `actions` 粒度是否需要 `version:*` 拆分 | 沿用 15 动作集 / 细化 | 沿用现集合，避免 AS 侧枚举变更 |
| Q5 | AS 验签 JWKS 拉取失败时的降级 | fail-closed / 缓存最近已知键 | fail-closed + 缓存内仍 kid-strict 验签（密钥轮换窗口容忍），缓存过期即拒 |
| Q6 | LMMS 未来上线委托签发（候选 A）后是否迁移 | 双 issuer 并存 / 切换 | 双 issuer 清单平滑过渡，无契约结构变化 |
