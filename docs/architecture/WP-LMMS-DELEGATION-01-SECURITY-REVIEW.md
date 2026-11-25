# WP-LMMS-DELEGATION-01 — 安全挑战评审

| | |
|---|---|
| 状态 | 评审报告（基于 2026-11-25 主干证据；协议对策以 [AUTHORIZATION-CONTRACT](WP-LMMS-DELEGATION-01-AUTHORIZATION-CONTRACT.md) 草案为准） |
| 方法 | 对 WP §七 的 10 个挑战逐项独立挑战：风险等级 → 攻击/故障场景 → 当前代码防护 → 缺失机制 → 修复建议 → 自动化测试要求。另附审计中的附加发现 |
| 证据基线 | CURRENT-STATE-AUDIT §1–§3（含复核标注条目） |

## 0. 风险总览矩阵

| 挑战 | 主题 | 等级 | 现状防护 | 核心缺口 |
|---|---|---|---|---|
| 1 | 撤销权限后已签发 token | 中 | 无 token 在签发（fail-closed） | 撤销链路三方皆缺 |
| 2 | Token 窃取跨 Project 重放 | 中 | AS scope 谓词 + `require()` | token 本身未存在；契约已内置 project 绑定 |
| 3 | Token 被其他服务使用 | 高（LMMS 侧） | — | 1.8.2 resource token 无 aud 且铸造端点公开 |
| 4 | Worker 自行扩权 | 低 | DB 角色 0 表权限 + lease 模型 | 无 |
| 5 | Job 取消后 token 残余写入 | 低-中 | 状态机触发器守卫 | 残余窗口依赖 TTL |
| 6 | File Upload Intent 绑定 | 高（未来项） | StoragePort 协议阻断（fail-closed） | 协议未实现（上游 LMMS 无 finalize） |
| 7 | 上传完成但事务失败 | 中（未来项） | 补偿删除模式（LMMS 自身先例） | AS 侧适配器未实现 |
| 8 | AS 重启恢复 | 低 | job + 快照全持久化，worker recover-once | 无 |
| 9 | LMMS 不可用失效关闭 | 中 | AI-PM JWKS fail-closed；AS 不依赖 LMMS 在线 | file 链路依赖矩阵需固化 |
| 10 | 凭证入日志 | 低 | 三仓库 grep 均干净，脱敏有测试 | 保持回归锁定 |

---

## 1. 挑战 1：用户被撤销 Project 权限后，已签发 Token 是否仍可使用？

- **风险等级：中**（残留窗口 = TTL ≤ 300s，且限已授权动作）。
- **场景**：管理员将用户移出 Project P；用户手中尚有一枚 `actions=["generation:request"], project=P` 的委托 token，在其 TTL 内仍可创建 Job。
- **当前代码防护**：无——当前没有任何 token 在签发（AS fail-closed），问题暂为理论态；AI-PM 侧权限判断每次请求实时读 DB（`tenant_role()`/`require_project_visible`），因此**新签发**立即被阻断。
- **缺失机制**：签发后的即时撤销；AS 侧无撤销查询。
- **修复建议**（对应契约 §5）：① 短 TTL 为默认缓释（≤5 分钟窗口，审计可追溯 jti 与签发事件）；② 评审若判"即时性 P0"，则 AI-PM 提供 jti 撤销查询端点，AS 在**写路径**（create/version/generation/cancel）在线校验，读路径靠 TTL；③ 用户全局封禁走 AI-PM 停止签发 + （若 ② 已建）jti 黑名单。
- **自动化测试要求**：撤销后 t<TTL 写路径 403（启用 ② 时）；TTL 过期 401；移出项目后新签发请求被拒（AI-PM 侧集成测试）；AS 审计含拒绝事件。

## 2. 挑战 2：Token 被窃取后，能否在其他 Project 重放？

- **风险等级：中**（token 一旦真实存在即成为窃取目标）。
- **场景**：攻击者从 AI-PM→AS 的单跳截获 delegation token，试图换 `X-Project-Id` 操作 Project Q。
- **当前代码防护**：AS `Scoped` 提取器将头/体内的 project 与**凭证 scope 核对**，不一致 `SCOPE_DENIED`（`action.rs:100-105`；测试 `as2_21_lookup_requires_project_authorization`、as1 `identity_cannot_cross_tenant_or_project_scope`）；租户恒取自凭证；审计记录拒绝事件。
- **缺失机制**：契约层面已覆盖（token 绑定 `tid+project_id+actions`，无通配）；传输层建议内网 + 服务间 mTLS 属 LMMS 后续（P2），不在本次范围。
- **修复建议**：保持"一次操作一枚 token"签发纪律 + 单跳传递；若引入候选 C 在线校验，jti 重放检测（同 jti 第二次出现告警+拒绝）作为增强。
- **测试要求**：跨 project/跨 tenant 重放用例（AS 已有，保留并纳入回归）；同 jti 双次提交审计告警用例（增强项）。

## 3. 挑战 3：Token 能否被其他服务使用？

- **风险等级：高（针对 LMMS 1.8.2 现状）**。
- **场景 A（LMMS resource token）**：`POST /tokens/resource` 网关 `public=true`、服务端无鉴权（`token.router.toml:19-23` + `resource.rs:21-24`，复核）——**任何能达内网者可铸造任意 resource token**；因无 `aud`，它也无法被限定受众（平台亦无消费者，实害为"铸造洪水 + 未来误消费埋雷"）。
- **场景 B（service-jwt 误用）**：`aud` 绑定正确实施（`verify_aud_principal_middleware`），user JWT `aud=coder-micro-services` 但**网关与 Principal 提取器不校验 aud**（`middleware/mod.rs:265`、`principal.rs:210`）——一个合法用户 JWT 可被带到任何接受"转发用户 JWT"的下游服务。
- **当前代码防护**：file-service 拒收 aud 不符 token（`user_jwt_rejected_on_file_routes`）；内部端点 aud 强制。
- **修复建议**：LMMS 后续需求（P1，随候选 A 一并）：resource 端点挂 service-jwt 鉴权 + 加 aud/tid claims；网关/提取器启用 aud 白名单校验。本次委托契约不产生任何跨服务可复用凭证（delegation token `aud=artifact-service` 固定）。
- **测试要求**：AS 验签器单测：aud≠artifact-service 拒绝、iss 不在受信清单拒绝（实现时新增）。

## 4. 挑战 4：Worker 是否可以自行扩大授权范围？

- **风险等级：低**（三仓库中防护最强的一环）。
- **场景**：被攻陷的 worker 尝试直接 UPDATE `generation_job` 状态、读取其他租户 job 输入、或调用未授权函数。
- **当前代码防护**：WORKER 角色 0 表权限、仅 7 个协调函数 EXECUTE（`db-roles.sql:74-98`）；租约 token 单次明文 + sha256 落库；complete/fail/heartbeat 需 `(job_id, attempt_id, lease_token)` 匹配；claim 按 `(generator_id, adapter_version)` 白名单分区 + `SKIP LOCKED`；运行时测试锁定（`roles_separation.rs`、`worker_roles.rs`）。未来 A3A-06 已决策 worker 文件身份为"短时、aud=file-service、按对象收敛的服务 token"，禁止持久用户委托。
- **缺失机制**：无实质缺口；leas 泄露窗口（明文一次传递）靠短租约 + 单次使用缓释。
- **修复建议**：维持现状；worker 执行循环实现（ART-03B-02）时保持"不持任何用户凭证"红线。
- **测试要求**：已有角色分离测试纳入迁移后必跑清单（`db-roles.sql` 重跑后自动回归）。

## 5. 挑战 5：Job 取消后，尚未过期的 Token 是否还能继续写入？

- **风险等级：低-中**。
- **场景**：用户取消 Job 后，在 delegation TTL 内用同 token 提交新 Job 或创建 Version。
- **当前代码防护**：① 取消后**该 Job** 的写入被状态机守卫阻断（`guard_job_transition` + confirm-cancel 协调函数）；② 但取消动作不撤销**已签发 token 的其他授权动作**——token 是能力凭证不是会话，取消 Job ≠ 吊销能力。
- **缺失机制**：即时撤销（同挑战 1 ②）。
- **修复建议**：业务语义裁决（契约 Q1）：若产品定义"取消 Job 应同时终结本次委托"，AI-PM 在取消成功后对该 `delegation_id`/`jti` 执行撤销（启用 ② 后为一次黑名单写入）；否则依赖 TTL 自然收敛。
- **测试要求**：取消后同 token 创建新 Job 的行为按裁决断言（403 或允许）；取消后原 job 无任何状态变更（已有守卫测试保留）。

## 6. 挑战 6：File Upload Intent 是否绑定目标资源和 Job？

- **风险等级：高（未来项——协议实现时必须解决）**。
- **场景**：若无绑定，执行层可将产物对象写到不属于本 Job 的 key，或复用他人 upload intent 塞入恶意字节，随后 Version finalize 指向被污染对象。
- **当前代码防护**：`StoragePort::create_upload_intent` 端口已定义但生产实现为协议阻断（fail-closed）——**当前无字节路径，实害为零**；LMMS file-service 侧对象 key 服务端生成 UUID（`file.rs:78-80`，复核），presign URL 有 TTL 且审计落库，但**无 intent→资源绑定语义**，且 `user_id` 调用方可声明（F-24）。
- **缺失机制**：upload-intent ↔ (job_id/artifact_id, tenant, project, content-type/size, checksum) 的绑定与校验；finalize 时的对象校验。
- **修复建议**：AS 适配器实现时（PLAN 阶段 3）：intent 由 AS 服务层签发（内部表或带签名的 intent token），key= 服务端生成 + 租户前缀，finalize 时 `inspect_object` 校验哈希/大小；LMMS upload-intent/finalize 协议（候选 A 同批 LMMS 需求，P1）就绪后迁移。
- **测试要求**：跨 job 复用 intent 拒绝；哈希不匹配 finalize 拒绝 + 对象隔离；中断后孤儿对象清理用例。

## 7. 挑战 7：上传完成但数据库事务失败，如何恢复？

- **风险等级：中（未来项）**。
- **场景**：对象已写存储、Version 行 INSERT 失败/进程崩溃 → 孤儿对象；反向（先 INSERT 后写对象失败）→ 悬挂 version 行。
- **当前代码防护**：LMMS 自身模式为先对象后元数据 + 失败补偿删除（`file.rs:98-110`，复核）；AS 侧 `artifact_version.state` 已有 `pending|ready|failed|quarantined` 触发器状态机可承载两段语义；当前无字节路径。
- **缺失机制**：AS 适配器的补偿与对账（孤儿清理、pending 超时转 failed）。
- **修复建议**：实现顺序=先写对象（intent key）→ DB 事务提交 version(ready) → 失败补偿删对象；后台对账任务扫孤儿（复用 LMMS `cleanup_expired` 模式）；崩溃恢复依赖持久 intent 记录而非内存。
- **测试要求**：注入式故障测试（对象写成功+DB 失败 → 对象被补偿删除；DB 成功+对象缺失 → version 进 quarantined/failed）；幂等重放不产生双对象。

## 8. 挑战 8：AS 重启后能否恢复任务而不依赖原用户会话？

- **风险等级：低（AS-1/AS-2 的核心设计成果）**。
- **场景**：AS/worker 进程在 Job 执行中崩溃重启。
- **当前代码防护**：job 行 + 输入快照 + caller 身份三元组全部持久化（0002/0003/0005）；`recover_expired_generation_jobs` + `recover-once` 子命令恢复租约；attempt/lease 模型支持超时重领；幂等重放按 caller_request_id 零写入返回。**全程无用户会话参与**。
- **缺失机制**：无。
- **测试要求**：已有（AS-2 三阶段故障注入、双索引竞态）；保留回归。

## 9. 挑战 9：LMMS 暂时不可用时，哪些操作必须失败关闭？

- **风险等级：中**。
- **故障矩阵**（按候选 B 架构）：

| 操作 | LMMS 组件依赖 | 必须行为 | 现状 |
|---|---|---|---|
| 用户登录 | iam/token | 失败关闭（无法登录） | ✅ 天然 |
| AI-PM 验签用户 JWT | JWKS（首次/刷新） | 缓存内继续服务，缓存到期且拉取失败 → fail-closed 401 | ✅ 启动期已 fail-closed（`main.rs:49-67`），需补**运行中缓存过期拉取失败**用例 |
| AI-PM 签发 delegation | 不依赖 LMMS 在线 | 正常签发 | 契约红利 |
| AS 验签 delegation | ai-pm JWKS | 同上（fail-closed，kid-strict 缓存容忍轮换窗口） | 实现时按契约 Q5 |
| AS/worker 写文件 | file-service | 503 STORAGE_UNAVAILABLE（现有码），Job 保持 queued/retry_wait，**不得**标记 succeeded | Port 已 fail-closed；适配器实现时保留语义 |
| Job 领取/心跳/完成 | 不依赖 LMMS | 正常 | ✅ 纯 DB |
| AI-PM 业务验收 | pm-api 自身 | 正常 | ✅ |

- **修复建议**：把上表固化为部署文档 + 集成测试场景（混沌用例：停 file-service 容器断言 Job 不误标成功）。
- **测试要求**：file-service 不可用 → version 创建 503 且 job 状态不变；JWKS 断供 → 401；恢复后自动回衔接续。

## 10. 挑战 10：如何避免把 Access Token / Resource Token / 长期密钥写入日志？

- **风险等级：低（当前纪律良好，需保持）**。
- **当前代码防护**：三仓库审计 grep（token/bearer/password/secret/dsn/credential 于 tracing 调用点）零命中；AS worker 明示不记录 lease token/输入正文/DSN 且 DSN Debug 脱敏有测试；AS 审计表只存 `delegation_id`+subject；AI-PM DB 只存哈希类凭证。
- **缺失机制**：无系统性缺口；风险在未来实现引入（如把 delegation token 放进错误消息、debug 日志）。
- **修复建议**：实现 `DelegationVerifier` 时拒绝消息只含判定原因（无 token 内容）——对齐现有 `ClosedDelegationVerifier` 消息风格；CI 增加日志卫生断言（对新增 tracing 宏调用做模式检查可选，成本高，暂以评审检查单代替）。
- **测试要求**：验签器单测断言错误信息不含 token 前缀；保留现有 `debug_output_does_not_expose_the_database_url` 回归。

---

## 11. 附加发现（超出 10 挑战的必记项）

| # | 等级 | 发现 | 建议 |
|---|---|---|---|
| F-20 | P1（流程） | 0005/AS-2 报告引用的规范契约 `ART-04G-*` 文件不在仓库（溯源断链） | 补交文档或在 AS-2 报告中改指本 WP 文档族 |
| F-21 | P0（LMMS 运维，非本 WP 代码） | lmms 仓库 git 内含历史签名私钥 `keys/private.pem`（kid=default 导入路径仍活跃） | 从 git 移除 + 轮换 active key + keystore 重建（LMMS 侧行动项，本 WP 不执行） |
| F-22 | P0（AI-PM 运维） | `ai-pm/.test-env.md` 明文 SSH/root/DeepSeek/GitLab 凭据已提交 | 立即轮换全部凭据 + 移出 git + 历史清理 |
| F-23 | P1（LMMS） | `/tokens/revoke` 公开无鉴权；`token_revoked_jtis` 无生产读取方；网关黑名单 fail-open 且仅 logout 写入 | LMMS 后续需求：撤销端点加鉴权 + 验证路径接入撤销检查 + blacklist fail 策略复核 |
| F-24 | P1（LMMS） | file-service：ct 调用方租户回落 1、`user_id` 调用方声明不校验、FILE_* 权限码全平台未消费 | AS 文件链路接入前与 LMMS 明确"受信调用方契约"（file 路由 mod.rs 注释已声明该模型），并把归属校验上移到 AS 服务层 |
| F-25 | P2 | 网关/`Principal` 提取器不校验用户 JWT `aud` | LMMS 后续需求（随 F-23 批次） |
| F-26 | P2 | LMMS 签名密钥轮换仅手动 API；无自动调度 | 运维 runbook 补轮换节奏 |

## 12. 自动化测试矩阵（汇总，执行细节见 IMPLEMENTATION-PLAN §5）

| 类别 | 用例族 | 仓库 | 状态 |
|---|---|---|---|
| 验签器 | aud/iss/exp/TTL/purpose/actions 拒绝矩阵 | AS | 待实现（随验证器） |
| scope | 跨 tenant/project 重放、X-Project-Id 不一致 | AS | ✅ 已有，保留回归 |
| 幂等 | caller_request_id 重放/冲突/指纹版本 | AS | ✅ 已有（AS-2） |
| 角色 | APP/WORKER/MAINTENANCE 权限探针 | AS | ✅ 已有，迁移后必跑 |
| 文件链路 | intent 绑定/哈希校验/补偿/孤儿清理 | AS | 待实现（阶段 3） |
| 失效关闭 | JWKS 断供、file-service 停机 | AI-PM / AS | 部分（补运行中断供用例） |
| 撤销 | 撤销后写路径 403 / TTL 过期 401 | AI-PM / AS | 待实现（裁决 Q1 后） |
| 日志卫生 | 错误消息不含凭证 | AS | 待实现（随验证器） |
