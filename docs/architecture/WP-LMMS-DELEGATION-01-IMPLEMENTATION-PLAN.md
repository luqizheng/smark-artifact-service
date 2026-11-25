# WP-LMMS-DELEGATION-01 — 实施计划（审计结论确认后执行）

| | |
|---|---|
| 状态 | 计划草案——**本文件不授权任何实现**；执行以评审确认的 [AUTHORIZATION-CONTRACT](WP-LMMS-DELEGATION-01-AUTHORIZATION-CONTRACT.md)（候选 B）为前提 |
| 范围红线（继承 WP §八） | 不修改 lmms；不开放 Generation Create；不启用 Generator/Worker/SeeCode 生产生成链路；不执行 0004/0005 生产迁移；不引入 AS-3 |

## 1. 总体判断（承接审计 §0）

**结论 B**：LMMS 1.8.2 具备其职责内全部能力（身份/JWKS/S2S service-jwt/文件租户隔离），缺的是"把 AI-PM 的授权决策固化为可验证凭证"这一层——按职责边界落在 AI-PM（签发）与 Artifact Service（验证）。

**必须修改的仓库**：
1. `ai-pm`：delegation 签发器 + JWKS 端点 + artifact-service 客户端（主要工作量）；
2. `smark-artifact-service`：真实 `DelegationVerifier` + AS-4 创建接线 + 文件存储过渡适配器（次要工作量，且部分被上游协议阻塞）；
3. `lmms`：**本次不改**；后续版本需求清单见 §7。

## 2. 分阶段实施

### 阶段 1：委托链路最小闭环（P0，完成后 AI-PM↔AS 首次真实联调）

| # | 任务 | 仓库 | 产出/验收 |
|---|---|---|---|
| 1.1 | AI-PM delegation 签发器：RS256 密钥对（env 注入，独立于 `AI_PM_SECRET_KEY`）、`issue_delegation(claims)`、TTL≤300s、jti/delegation_id/biz 字段、签发审计事件 | ai-pm | 单测：claims 形状/TTL 上限/无 project 成员资格拒发 |
| 1.2 | AI-PM `GET /.well-known/jwks.json`（仅公钥、内网） | ai-pm | kid 轮换双活窗口测试 |
| 1.3 | AI-PM artifact-service 客户端（对齐 AS `artifact-client` SDK 既有形态：base URL/超时/Idempotency/`caller_request_id` 注入；凭证=每次签发的 delegation token，**不缓存不重用**） | ai-pm | 集成测试：创建 job 幂等重放 |
| 1.4 | AS `LmmsPmDelegationVerifier`（实现既有 `DelegationVerifier` 端口）：契约 §3.2 七步全检，JWKS kid-strict 缓存，fail-closed | as | 拒绝矩阵单测（aud/iss/exp/TTL/purpose/actions）；`default_verifier_is_fail_closed` 更新为"无签发方配置时仍 fail-closed" |
| 1.5 | AS 配置面：`ARTIFACT_API_DELEGATION_ISSUERS`（iss→jwks_url 清单），缺省为空 = 维持全拒 | as | 配置缺省 fail-closed 测试 |
| 1.6 | AI-PM 侧业务挂接：在既有授权判断（`require_project_visible` 等）通过后签发并调用 AS；错误码映射（401/403/409/503 透传） | ai-pm | 集成测试 |
| 1.7 | 联调验收（dev 环境） | 双仓 | 手工脚本 + 双方审计事件对账 |

> 1.6 的写路径暂限于 `artifact:create / version:create 占位 503 / generation:request（仍被 generator unavailable 503 挡住）/ artifact:read`——**Generation Create 对外依然不可用**，本阶段只打通"凭证与幂等"，不打通"生成"。

### 阶段 2：AS-4 创建接线 + 只读查询（P0.5）

| # | 任务 | 仓库 | 验收 |
|---|---|---|---|
| 2.1 | 将 `PgKeyedGenerationJobCreator`（`create_generation_job_with_input` v2）接入 HTTP `request_generation`（信任边界：tenant/project 仅来自已验证上下文——0005 L22-27 的声明落地） | as | 既有 AS-2 全套测试通过 + 新增 HTTP 层错误映射测试 |
| 2.2 | `by-caller-request` 查询挂接 AI-PM 重试路径 | 双仓 | 断网重试恢复用例 |
| 2.3 | 取消链路联调（`generation:cancel` + 裁决 Q1 的撤销语义） | 双仓 | SECURITY-REVIEW 挑战 5 用例 |

### 阶段 3：文件链路（P1，受上游协议约束）

| # | 任务 | 仓库 | 说明 |
|---|---|---|---|
| 3.1 | AS `FileServiceStorage` 过渡适配器：`StoragePort` → lmms `POST /file/upload`（持 `aud=file-service` service-jwt，tid=job 租户），对象写成功→DB 事务→失败补偿删除；`inspect_object` 以 file info hash/size 校验近似 | as | SECURITY-REVIEW 挑战 6/7 全部用例；适配器上线前 Version 链路保持 503 |
| 3.2 | AI-PM/AS 服务身份 service-jwt 配置（复用 `ServiceJwtCache` 模式） | 双仓 | 契约 §6.1 第 13-14 步 |
| 3.3 | LMMS upload-intent/finalize 协议（**LMMS 后续版本需求**，随候选 A 批次）→ 届时替换 3.1 内部实现，`StoragePort` 语义不变 | lmms（未来） | AS 对外契约零变更 |

### 明确不做（本 WP）

- Generation Create 对外开放、generator `unavailable` 解除、worker 执行循环（ART-03B-02/03）、SeeCode 链路（ART-03）；
- 0004/0005 生产迁移执行（仅随应用部署流程走，且每次迁移后重跑 `deploy/db-roles.sql`——OPEN-ISSUES #23）；
- AS-3（artifact 验收流/回调）任何内容。

## 3. Migration 影响评估

| 迁移 | 状态 | 本计划影响 |
|---|---|---|
| 0001–0003 | 已在主干发布 | 无新迁移需求——委托链路（阶段 1/2）**不需要任何 schema 变更**：AS 侧全部依赖（`Delegation` 领域模型、audit_event、caller 三元组、幂等）已就绪 |
| 0004（AS-1）/ 0005（AS-2） | 代码就绪，生产执行受控（部署流程含 `db-roles.sql` 重跑） | 阶段 1/2 前必须在目标环境执行；hash 由 `migrations.sha256` + CI 迁移不可变守卫保护 |
| 新增迁移 | **无** | 若裁决 Q1 需要服务端撤销，AI-PM 侧自建撤销表（jti 黑名单，属于 ai-pm schema），AS 不加表 |

回滚：迁移不可变约定下无 down 脚本；回滚=应用回退到上一镜像（验证器配置清空即回到全拒姿态，天然安全）。

## 4. API / SDK 改造清单

| 层 | 变更 | 类型 |
|---|---|---|
| AI-PM REST | 新增内部 JWKS 端点；业务端点**零变更**（签发内嵌于现有授权后） | 新增 |
| AS HTTP | 零契约变更（验证器替换不改变路由/错误码；AS-4 接线启用既有 503 路径的真实实现） | 内部 |
| AS OpenAPI | `bearerDelegation` 已声明；补充"受信 issuer=ai-pm"说明（草案期标注） | 文档 |
| `artifact-client` SDK | 为 AI-PM 提供调用便利（AI-PM 亦可手写 reqwest；SDK 已含超时/幂等语义） | 可选 |
| LMMS | 无（1.8.2 冻结） | 无 |

## 5. 自动化测试矩阵（新增部分）

| 用例族 | 层级 | 仓库 |
|---|---|---|
| 签发器：claims 形状/TTL 上限/无授权拒发/审计事件 | 单测 | ai-pm |
| JWKS 端点：kid 轮换双活、错误 key 隔离 | 单测+集成 | ai-pm |
| 验证器：§3.2 七步拒绝矩阵、JWKS 断供 fail-closed、轮换窗口 | 单测 | as |
| 端到端：签发→创建 artifact→重放幂等→取消→审计对账（dev compose） | 集成 | 双仓 |
| 信任边界回归：scope 谓词、角色探针、`default_verifier` 无配置全拒 | 单测 | as（既有，迁移后必跑） |
| 失效关闭：file-service 停机 → Job 不误标成功 | 集成 | as（阶段 3） |

## 6. 部署与回滚

1. 部署顺序（dev）：ai-pm 迁移（如有）→ ai-pm（含 JWKS 端点+密钥 env）→ AS 迁移 0004/0005 + `db-roles.sql` → AS（issuer 清单指向 ai-pm JWKS）→ 联调脚本；
2. 安全默认：AS issuer 清单为空 = 全拒；AI-PM 未配置签名密钥 = 签发器禁用（业务端点不触达 AS）——两侧独立上线互不影响；
3. 回滚：任一侧回退镜像即断链回到 fail-closed；无数据回滚需求（无新 schema）；
4. 密钥事件：轮换 AI-PM 签名 key（双 kid 窗口）→ 收缩 AS 受信 kid 清单。

## 7. P0 / P1 / P2 清单

**P0（阻断最小闭环）**
- P0-1：AI-PM delegation 签发器 + JWKS（任务 1.1/1.2）
- P0-2：AS 真实验证器 + issuer 配置（任务 1.4/1.5）
- P0-3：AS-4 创建接线（任务 2.1）
- P0-4：`ai-pm/.test-env.md` 明文凭据轮换与移除（F-22，安全卫生，与链路并行立即处理）
- P0-5：开放问题 Q1（撤销即时性语义）裁决——影响 1.4 是否内嵌在线撤销查询

**P1（闭环后第一批）**
- P1-1：AS 文件过渡适配器 + 服务身份 service-jwt（任务 3.1/3.2）
- P1-2：LMMS 后续版本需求包 ①：resource token 端点鉴权 + aud/tid/project/action claims（候选 A）；② upload-intent/finalize 协议；③ 撤销端点鉴权 + 撤销检查接入验证路径（F-23）；④ file-service 归属校验/权限码消费（F-24）
- P1-3：F-20（补 ART-04G 文档或改引用）、F-21（lmms 历史私钥清理——LMMS 侧行动项）
- P1-4：在线撤销端点（若 Q1 判 P0 则升级）

**P2**
- P2-1：网关/Principal aud 校验（F-25）
- P2-2：签名密钥自动轮换 runbook（F-26）
- P2-3：AS↔LMMS 服务间 mTLS/传输加固（依赖 LMMS 传输层演进）
- P2-4：`artifact-client` SDK 面向 AI-PM 的便捷封装完善

## 8. 下一阶段可直接执行的任务（评审通过后）

1. 任务 1.1–1.5（委托闭环骨架，双侧可并行）；
2. Q1/Q2 裁决会（撤销语义 + X-Project-Id 去留）；
3. P0-4 安全卫生（不依赖任何裁决，可立即执行）；
4. 阶段 2 接线设计评审（AS-4）。

## 9. 风险与依赖

| 风险 | 缓释 |
|---|---|
| AI-PM 私钥托管失误 | 密钥独立、env 注入、不入库不入 git、轮换预案（契约 §4） |
| AS 验签引入可用性耦合 | JWKS 缓存 + fail-closed 策略（契约 Q5）；AS 当前无外呼依赖，新增 1 个内网只读 GET |
| LMMS 后续需求排期不可控 | 候选 B 全程不依赖；P1-2 仅提升安全余量与协议正规性 |
| "设计通过≠实现完成" | 本 WP 交付仅为文档；每阶段有独立验收测试清单，CI 绿不等于安全（WP §十） |
