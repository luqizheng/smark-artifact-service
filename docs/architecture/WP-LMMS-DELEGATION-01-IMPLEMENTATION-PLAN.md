# WP-LMMS-DELEGATION-01 — 实施计划（R1 修订版）

| | |
|---|---|
| 状态 | 计划草案——**本文件不授权任何实现**；执行以**第二轮独立安全评审通过**的 [AUTHORIZATION-CONTRACT](WP-LMMS-DELEGATION-01-AUTHORIZATION-CONTRACT.md)（R1 修订草案 v0.2，候选 B + Q1–Q6 冻结裁决）为前提。**阶段 1（任务 1.1–1.5）仅在第二轮评审通过后启动。** |
| R1 修订要点 | 签发接口从"通用 issue_delegation(claims)"改为**受控业务意图接口**（P0-03）；新增 §3.0 上游验证链前置任务与 **audience 阻断项**（P0-02）；tenant 改 AI-PM 内部 UUID（P0-01）；新增操作绑定（P0-04）与**在线撤销**（Q1 已裁决，不再是开放问题）；JWKS 缓存陈旧策略（Q5）；新增 AI-PM 迁移 |
| 范围红线（继承 WP §八） | 不修改 lmms；不开放 Generation Create；不启用 Generator/Worker/SeeCode 生产生成链路；不执行 0004/0005 生产迁移；不引入 AS-3；不将用户 LMMS JWT 转发给 AS；不将 delegation token 传给 Worker；AS 不查 AI-PM 数据库；请求体不决定 tenant/project scope |

## 1. 总体判断（承接审计 §0）

**结论 B**：LMMS 1.8.2 具备其职责内大部分能力（身份/JWKS/S2S service-jwt/文件租户隔离——后者**有条件成立**，F-24），缺的是"把 AI-PM 的授权决策固化为可验证凭证"这一层——按职责边界落在 AI-PM（签发）与 Artifact Service（验证）。

**必须修改的仓库**：
1. `ai-pm`：上游验证链（aud 强制）+ 意图接口 + delegation 签发/撤销 + JWKS 端点 + 撤销存储迁移 + artifact-service 客户端（主要工作量）；
2. `smark-artifact-service`：真实 `DelegationVerifier`（P1-01 全矩阵 + P1-02 缓存策略 + V11 撤销查询客户端）+ 意图绑定的 AS-4 接线 + 文件存储过渡适配器（次要工作量，部分被上游协议阻塞）；
3. `lmms`：**本次不改**；后续版本需求清单见 §7（含 F-28 resource token TTL 缺陷修复）。

## 2. 分阶段实施

### 阶段 0：签发前置条件（P0，delegation issuer 启用的硬性闸门）

| # | 任务 | 仓库 | 产出/验收 |
|---|---|---|---|
| 0.1 | 上游用户 JWT 验证链（契约 §3.0 七项）：delegation 签发接口专属中间件/函数——`verify_token(…, Some("coder-micro-services"))` + purpose=user_login 强制 + ct=false + roles 排除 service + 人类身份类别检查（dev-off/cpk/Agent/Service 拒绝） | ai-pm | 七项逐项单测（aud 缺失/不匹配/数组型、purpose 枚举全表、ct、service roles、身份类别矩阵）；**F-25 的 AI-PM 侧修复** |
| 0.2 | **Audience 阻断项处置**（契约 §3.0 ⛔）：确认共享 aud + purpose 强制的组合方案被第二轮评审书面接受；如评审要求 AI-PM 专用 audience → 登记为 LMMS 后续需求并**暂停阶段 1** | 双方评审 | 评审纪要记录决议 |
| 0.3 | F-22 凭据轮换（P0）：轮换 `.test-env.md` 全部凭据并移出工作树（R1 事实更正：未进 git，但轮换要求不变） | ai-pm 运维 | 轮换确认（凭据零复述） |

### 阶段 1：委托链路最小闭环（P0，**第二轮评审通过后启动**；完成后 AI-PM↔AS 首次真实联调）

| # | 任务 | 仓库 | 产出/验收 |
|---|---|---|---|
| 1.1 | **意图接口**（P0-03）：受控业务意图端点（服务端写死意图→精确 Action 映射常量；调用方不可提交 actions）；签发前完成 §3.0 验证链 + DB 成员关系 + 目标资源/业务状态检查；默认拒发清单落地（未知动作/无映射/非人类身份/dev-off/cpk） | ai-pm | 每意图映射表 + 拒发矩阵单测 |
| 1.2 | delegation 签发器：RS256 密钥对（**独立 env `AI_PM_DELEGATION_SIGNING_KEY`，绝不复用 `AI_PM_SECRET_KEY`**，仅安全注入）；claims 按契约 §3.1.1（tenant_id=**内部 UUID**、可选 lmms_tid 追溯、operation-bound 条件字段、TTL **120s 缺省 / 300s 硬上限**）；**签发即写 `pm_delegation`（fail-closed）**；签发审计事件 | ai-pm | claims 形状/TTL 上限/无授权拒发/签发记录 fail-closed 单测 |
| 1.3 | **撤销能力（Q1 已裁决，P0）**：迁移新建 `pm_delegation` + `pm_delegation_revocation`（ai-pm schema）；内部查询端点 `GET /internal/delegation-revocations/{jti}`（service 身份认证）；移出项目/禁用/取消 Job 的事务内批量撤销 | ai-pm | 撤销事务原子性 + 批量撤销 + 保留期限清理任务测试 |
| 1.4 | AI-PM `GET /.well-known/jwks.json`（仅公钥、内网）；轮换双 kid 并存窗口 ≥10 分钟 | ai-pm | kid 轮换双活窗口测试 |
| 1.5 | AI-PM artifact-service 客户端（对齐 AS `artifact-client` SDK：base URL/超时/Idempotency/`caller_request_id` 注入；凭证=每次签发的单枚 delegation token，不缓存不重用；错误码 401/403/409/503 透传） | ai-pm | 集成测试：创建 job 幂等重放 |
| 1.6 | AS `AiPmDelegationVerifier`（实现既有 `DelegationVerifier` 端口）：契约 §3.2 **V1–V12 全矩阵**——含缺失 kid 拒绝（覆盖库的 default-key 回退）、未知 kid 单次刷新、iat/nbf/时钟偏差 ±60s、操作绑定 V10；**V11 撤销查询客户端**（写动作前查询，超时 2s，fail-closed 503 `REVOCATION_UNAVAILABLE`，禁止缓存撤销结果） | as | P1-01 矩阵逐项单测 + 撤销查询失败注入测试；`default_verifier_is_fail_closed` 更新为"无签发方配置时仍 fail-closed" |
| 1.7 | AS JWKS 缓存策略（Q5）：刷新 5min / **最大陈旧 15min**（超窗 fail-closed）/ last-successful-refresh 暴露 / 重启空缓存 fail-closed | as | 陈旧上限、刷新失败超窗、重启空缓存测试 |
| 1.8 | AS 配置面：`ARTIFACT_API_DELEGATION_ISSUERS`（iss→jwks_url+revocation_url 清单），缺省为空 = 维持全拒；AS-4 接线沿用阶段 2 | as | 配置缺省 fail-closed 测试 |
| 1.9 | AI-PM 侧业务挂接：意图接口在既有授权判断通过后签发并调用 AS | ai-pm | 集成测试 |
| 1.10 | 联调验收（dev 环境）：含撤销链路端到端（签发→写入→撤销→再写 403） | 双仓 | 手工脚本 + 双方审计事件对账 |

> 写路径暂限于 `artifact:create / version:create（占位 503）/ generation:request（仍被 generator unavailable 503 挡住）/ artifact:read 族`——**Generation Create 对外依然不可用**，本阶段只打通"凭证、绑定与撤销"，不打通"生成"。

### 阶段 2：AS-4 创建接线 + 只读查询（P0.5）

| # | 任务 | 仓库 | 验收 |
|---|---|---|---|
| 2.1 | 将 `PgKeyedGenerationJobCreator`（`create_generation_job_with_input` v2）接入 HTTP `request_generation`（信任边界：tenant/project 仅来自已验证上下文——0005 L22-27 的声明落地） | as | 既有 AS-2 全套测试通过 + 新增 HTTP 层错误映射测试 |
| 2.2 | `by-caller-request` 查询挂接 AI-PM 重试路径 | 双仓 | 断网重试恢复用例 |
| 2.3 | 取消链路联调（新签发 `generation:cancel` + `operation_id` 绑定 + 取消成功即撤销 jti） | 双仓 | SECURITY-REVIEW 挑战 5 全部用例 |

### 阶段 3：文件链路（P1，受上游协议约束）

| # | 任务 | 仓库 | 说明 |
|---|---|---|---|
| 3.1 | AS `FileServiceStorage` 过渡适配器：`StoragePort` → lmms `POST /file/upload`（持 `aud=file-service` service-jwt，**`ct=false`、tid=job 租户**），对象写成功→DB 事务→失败补偿删除；`inspect_object` 以 file info hash/size 校验近似；**Upload Intent 绑定目标资源与 Job、Finalize 验证归属与状态；F-24 未解决前保持 fail-closed** | as | SECURITY-REVIEW 挑战 6/7 全部用例；适配器上线前 Version 链路保持 503 |
| 3.2 | AI-PM/AS 服务身份 service-jwt 配置（复用 `ServiceJwtCache` 模式） | 双仓 | 契约 §6.1 第 13-14 步 |
| 3.3 | LMMS upload-intent/finalize 协议 + 租户 fallback 修复（**LMMS 后续版本需求**，随候选 A 批次）→ 届时替换 3.1 内部实现，`StoragePort` 语义不变 | lmms（未来） | AS 对外契约零变更 |

### 明确不做（本 WP）

- Generation Create 对外开放、generator `unavailable` 解除、worker 执行循环（ART-03B-02/03）、SeeCode 链路（ART-03）；
- 0004/0005 生产迁移执行（仅随应用部署流程走，且每次迁移后重跑 `deploy/db-roles.sql`——OPEN-ISSUES #23）；
- AS-3（artifact 验收流/回调）任何内容；
- LMMS 任何修改（含 F-23/F-24/F-25/F-28 的 LMMS 侧修复——全部列为后续需求）。

## 3. Migration 影响评估

| 迁移 | 状态 | 本计划影响 |
|---|---|---|
| AS 0001–0003 | 已在主干发布 | 委托链路（阶段 1/2）**AS 侧仍无 schema 变更**：`Delegation` 领域模型的操作绑定字段先由验证层携带（V10 在 verifier/服务层比对），暂不落库——若第二轮评审要求持久化绑定，届时新增迁移并评审 |
| AS 0006（**O-5 待裁决项，本 PR 附带**） | 仅存在于 PR 分支，未接线 | `artifact.delegation_token_use`（jti 唯一 + 配对 CHECK + purge 护栏）：本实现为"jti 改绑硬拒 / 并发重放 DB 原子仲裁"的纵深防御候选，与"AS 不加表"裁决冲突，提请第二轮评审保留或移除（默认无任何路由消费）；**评审通过前不得在共享环境执行** |
| AS 0004（AS-1）/ 0005（AS-2） | 代码就绪，生产执行受控（部署流程含 `db-roles.sql` 重跑） | 阶段 1/2 前必须在目标环境执行；hash 由 `migrations.sha256` + CI 迁移不可变守卫保护 |
| **AI-PM 新增迁移（R1 变更）** | 待实现 | **Q1 已裁决 → 撤销存储为 P0 必需**：`pm_delegation` + `pm_delegation_revocation`（jti 主键、状态、事务内批量撤销、7 天撤销记录保留），属 ai-pm 自有 schema，AS 不加表 |
| 回滚 | 迁移不可变约定下无 down 脚本 | AS 侧回滚=应用回退上一镜像（验证器配置清空即回到全拒姿态）；AI-PM 侧回滚=停用签发器（业务端点不触达 AS），撤销表保留不删 |

## 4. API / SDK 改造清单

| 层 | 变更 | 类型 |
|---|---|---|
| AI-PM REST | 新增意图接口 + 内部 JWKS 端点 + 内部撤销查询端点；既有业务端点零变更 | 新增 |
| AI-PM 验签 | §3.0 验证链为**签发接口专属**；普通业务端点的 aud 缺口（F-25）随批修复但不改授权语义 | 内部 |
| AS HTTP | 零契约变更（验证器替换不改变路由/错误码；新增 503 `REVOCATION_UNAVAILABLE`） | 内部 |
| AS OpenAPI | `bearerDelegation` 已声明；补充"受信 issuer=ai-pm、TTL≤300s、写路径在线撤销、REVOCATION_UNAVAILABLE 语义"说明（草案期标注） | 文档 |
| `artifact-client` SDK | 为 AI-PM 提供调用便利（AI-PM 亦可手写 reqwest；SDK 已含超时/幂等语义） | 可选 |
| LMMS | 无（1.8.2 冻结） | 无 |

## 5. 自动化测试矩阵（新增部分）

| 用例族 | 层级 | 仓库 |
|---|---|---|
| §3.0 验证链七项（aud 精确单值缺失/不匹配/数组型/purpose 枚举全表/ct/service roles/身份类别矩阵） | 单测 | ai-pm |
| 意图→Action 映射全覆盖（15 动作逐一有授权依据与用例）+ 调用方提交 actions 拒绝 + 默认拒发清单 | 单测 | ai-pm |
| 签发器：claims 形状（tenant UUID/lmms_tid 追溯/绑定字段）/TTL 上限 300 拒发 301/无授权拒发/签发记录 fail-closed/审计事件 | 单测 | ai-pm |
| 撤销存储：事务内批量撤销/原子性/保留期清理/查询端点认证 | 单测+集成 | ai-pm |
| JWKS 端点：kid 轮换双活、错误 key 隔离 | 单测+集成 | ai-pm |
| AS 验证器：P1-01 全矩阵（V1–V12 逐项）、缺失 kid 拒绝、未知 kid 单次刷新、时钟偏差 ±60s 边界、操作绑定五场景 | 单测 | as | **✅ 已实现（本 PR）**：`delegation_security` 23 用例 + `ai_pm`/`jwks`/`config`/`revocation` 单测，映射见契约 §9 |
| AS 撤销查询：撤销后写 403 / 查询超时与停机 → 写 503 读不受影响 / 禁缓存断言 | 单测+集成 | as | **部分**：`DelegationRevocationChecker` 端口 + fail-closed 默认已实现；HTTP 客户端随任务 1.7 |
| AS JWKS 缓存：15min 陈旧 fail-closed / 刷新失败超窗拒绝 / 重启空缓存 fail-closed | 单测 | as | **✅ 已实现**（`JwksCache` 策略 + 常量契约单测） |
| AS 撤销查询：撤销后写 403 / 查询超时与停机 → 写 503 读不受影响 / 禁缓存断言 | 单测+集成 | as |
| AS JWKS 缓存：15min 陈旧 fail-closed / 刷新失败超窗拒绝 / 重启空缓存 fail-closed | 单测 | as |
| 端到端：签发→创建 artifact→重放幂等→取消→撤销→审计对账（dev compose） | 集成 | 双仓 |
| 信任边界回归：scope 谓词、角色探针、`default_verifier` 无配置全拒、body tenant 不覆盖凭证 | 单测 | as（既有+新增，迁移后必跑） |
| 失效关闭：file-service 停机 → Job 不误标成功 | 集成 | as（阶段 3） |

## 6. 部署与回滚

1. 部署顺序（dev）：ai-pm 迁移（撤销表）→ ai-pm（JWKS 端点 + 签名密钥 env + §3.0 验证链）→ AS 迁移 0004/0005 + `db-roles.sql` → AS（issuer 清单指向 ai-pm JWKS + 撤销端点 URL）→ 联调脚本；
2. **上线检查单含 C 类证据取证**（F-27）：记录部署镜像 digest 与源码 tag 对照、启动版本日志、行为探测结果——缺 C 类证据不得声明"部署与评审基线一致"；
3. 安全默认：AS issuer 清单为空 = 全拒；AI-PM 未配置签名密钥 = 签发器禁用（业务端点不触达 AS）；AI-PM 撤销端点不可达 = AS 写平面 503（fail-closed，已知代价）——各侧独立上线互不影响；
4. 回滚：任一侧回退镜像即断链回到 fail-closed；AI-PM 撤销表数据保留；
5. 密钥事件：轮换 AI-PM 签名 key（双 kid 窗口）→ AS 受信 kid 收缩；紧急场景按 §5.3 矩阵批量撤销 jti。

## 7. P0 / P1 / P2 清单（R1 更新）

**P0（阻断最小闭环）**
- P0-A：阶段 0 全部任务——§3.0 验证链（0.1）、audience 阻断项评审处置（0.2）、F-22 凭据轮换（0.3）
- P0-B：意图接口 + 签发器 + 撤销能力（任务 1.1–1.3，Q1 已裁决为必做）
- P0-C：AS 验证器 V1–V12 + JWKS 缓存策略 + 撤销查询客户端（任务 1.6–1.7）
- P0-D：AS-4 创建接线（任务 2.1）
- P0-E：F-21（lmms 历史私钥轮换——LMMS 侧行动项，delegation 依赖 LMMS 签名密钥可信，需在联调环境确认轮换状态）

**P1（闭环后第一批）**
- P1-1：AS 文件过渡适配器 + 服务身份 service-jwt（任务 3.1/3.2；F-24 六条要求未满足前 fail-closed）
- P1-2：LMMS 后续版本需求包 ①：resource token 端点鉴权 + aud/tid/project/action claims + **F-28 TTL 边界校验与 as 转换消除**（候选 A 批次）；② upload-intent/finalize 协议 + 租户 fallback 修复；③ 撤销端点鉴权 + 撤销检查接入验证路径 + blacklist fail 策略复核（F-23）；④ file-service 归属校验/权限码消费（F-24）；⑤ 网关/Principal aud 白名单校验（F-25 的 LMMS 侧）
- P1-3：F-20（补 ART-04G 文档或改引用）；F-27 C 类部署证据取证流程
- P1-4：AI-PM 专用 audience 申请（如评审判定必须）→ 随 LMMS 需求包 ①

**P2**
- P2-1：签名密钥自动轮换 runbook（F-26）
- P2-2：AS↔LMMS 服务间 mTLS/传输加固（依赖 LMMS 传输层演进）
- P2-3：`artifact-client` SDK 面向 AI-PM 的便捷封装完善

## 8. 第二轮评审通过后可直接执行的任务

1. 任务 1.1–1.5（委托闭环骨架，双侧可并行）——**阶段 1 不早于第二轮评审通过**；
2. P0-A 安全卫生与验证链（0.1/0.3 不依赖裁决，可先行准备，0.2 以评审结论为准）；
3. 阶段 2 接线设计评审（AS-4）。

## 9. 风险与依赖

| 风险 | 缓释 |
|---|---|
| AI-PM 私钥托管失误 | 密钥独立于 `AI_PM_SECRET_KEY`、env/安全注入、不入库不入 git、轮换预案（契约 §4.1） |
| audience 隔离不足（共享 aud） | purpose 强制兜底 + 阻断项提交第二轮评审（契约 §3.0 ⛔）；如判不足 → LMMS 专用 audience 需求 |
| AI-PM 停机 → AS 写平面停机 | Q1 fail-closed 的结构性代价，评审知情项；可用性要求高时回候选 C（架构升级） |
| 撤销查询时延 | 内网单跳 + 2s 超时 + 仅写路径查询（读路径不查） |
| AS 验签引入可用性耦合 | JWKS 缓存陈旧策略（15min 窗口内容忍刷新失败，超窗 fail-closed，契约 §4.2）；AS 当前无外呼依赖，新增 2 个内网只读依赖（JWKS GET + 撤销 GET） |
| LMMS 后续需求排期不可控 | 候选 B 全程不依赖；P1-2 仅提升安全余量与协议正规性 |
| "设计通过≠实现完成" | 本 WP 交付仅为文档；每阶段有独立验收测试清单，CI 绿不等于安全（WP §十） |
