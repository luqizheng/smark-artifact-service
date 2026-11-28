# WP-LMMS-DELEGATION-01 — 安全挑战评审（R1 修订版）

| | |
|---|---|
| 状态 | 评审报告（基于 2026-11-25 主干证据，R1 修订：独立安全评审意见已合入；协议对策以 [AUTHORIZATION-CONTRACT](WP-LMMS-DELEGATION-01-AUTHORIZATION-CONTRACT.md) **R1 修订草案 v0.2** 为准；整体保持 **WAITING FOR INDEPENDENT SECURITY REVIEW**） |
| 方法 | 对 WP §七 的 10 个挑战逐项独立挑战：风险等级 → 攻击/故障场景 → 当前代码防护 → 缺失机制 → 修复建议 → 自动化测试要求。另附审计附加发现（F-20～F-28，R1 评级）与 R1 独立评审决议（§13） |
| 证据基线 | CURRENT-STATE-AUDIT §1–§3（R1 修订版：lmms tag `v1.8.2`@`93fc3a2e`、ai-pm GitHub `master`@`d0e72fa6`、as `main`@`b5095cf`；含复核标注条目与 R1 重验记录） |

## 0. 风险总览矩阵

| 挑战 | 主题 | 等级 | 现状防护 | 核心缺口 | R1 对策落点 |
|---|---|---|---|---|---|
| 1 | 撤销权限后已签发 token | 中（设计后：写路径即时，读路径 ≤TTL） | 无 token 在签发（fail-closed） | 撤销链路三方皆缺 | **Q1 冻结**：写路径在线 jti 撤销 + fail-closed（契约 §5.3） |
| 2 | Token 窃取跨 Project 重放 / 窗内复用 | 中 | AS scope 谓词 + `require()` | 窗内同 token 跨操作复用无防护 | **P0-04**：operation-bound claims + 幂等单次消费（契约 §3.1.2） |
| 3 | Token 被其他服务使用 | 高（LMMS 侧） | — | 1.8.2 resource token 无 aud 且铸造端点公开、TTL 输入缺陷（F-28） | 委托契约不产生跨服务凭证；LMMS 后续需求包 |
| 4 | Worker 自行扩权 | 低 | DB 角色 0 表权限 + lease 模型 | 无 | 维持 |
| 5 | Job 取消后 token 残余写入 | 低-中（裁决后：写路径即时撤销） | 状态机触发器守卫 | 残余窗口依赖 TTL | **Q1**：取消成功即撤销本次 delegation jti（契约 §6.2） |
| 6 | File Upload Intent 绑定 | 高（未来项） | StoragePort 协议阻断（fail-closed） | 协议未实现（上游 LMMS 无 finalize）+ F-24 信任边界 | 阶段 3 + LMMS 需求包；接入前 fail-closed |
| 7 | 上传完成但事务失败 | 中（未来项） | 补偿删除模式（LMMS 自身先例） | AS 侧适配器未实现 | 阶段 3 |
| 8 | AS 重启恢复 | 低 | job + 快照全持久化，worker recover-once | 无 | 维持 |
| 9 | LMMS 不可用失效关闭 | 中 | AI-PM JWKS fail-closed；AS 不依赖 LMMS 在线 | file 链路依赖矩阵需固化；**R1 新增：AI-PM 不可用 = AS 写平面停机（撤销 fail-closed 的已知代价）** | 契约 §6.3 + 评审知情项 |
| 10 | 凭证入日志 | 低 | 三仓库 grep 均干净，脱敏有测试 | 保持回归锁定 | 维持 |

---

## 1. 挑战 1：用户被撤销 Project 权限后，已签发 Token 是否仍可使用？

- **风险等级：中**（残留窗口 = TTL ≤ 300s，且限已授权动作）。**R1 后设计结论：写路径残余窗口 ≈ 0（在线撤销），读路径残余窗口 ≤ TTL**。
- **场景**：管理员将用户移出 Project P；用户手中尚有一枚 `actions=["generation:request"], project=P` 的委托 token，在其 TTL 内仍可创建 Job。
- **当前代码防护**：无——当前没有任何 token 在签发（AS fail-closed），问题暂为理论态；AI-PM 侧权限判断每次请求实时读 DB（`tenant_role()`/`require_project_visible`），因此**新签发**立即被阻断。
- **缺失机制**：签发后的即时撤销；AS 侧无撤销查询。
- **R1 裁决（Q1 冻结）**："TTL 足够短所以无需撤销"**不成立**。契约 §5.3 已固化：AI-PM 建 `pm_delegation`/`pm_delegation_revocation` 状态存储 + 内部撤销查询端点；AS 对**全部写动作**在业务写入前在线查询 jti，查询不可用 **fail-closed**（503 `REVOCATION_UNAVAILABLE`，禁止 fail-open 分支，禁止进程级缓存撤销结果）；读动作靠 TTL（残余窗口 ≤ TTL，建议 120s/上限 300s）。移出 Project / 用户禁用 / 管理员紧急撤销均在事务内批量撤销 active jti（写路径即时生效矩阵见契约 §5.3）。
- **架构代价（如实声明，不降低安全要求）**：① AI-PM 每个写请求新增一次内网查询（时延 +1 RTT，超时上限 2s）；② **AI-PM 停机 = AS 写平面停机**（fail-closed 的结构性后果）——该项已列入 §0 矩阵与第二轮评审议题；如业务不可接受，唯一替代是候选 C 全量 introspection（架构升级，非降级）。
- **自动化测试要求**：撤销后 t<TTL 写路径 403；撤销服务停机 → 写 503 `REVOCATION_UNAVAILABLE` 且读不受影响；移出项目/禁用后写路径下一次请求即 403（AI-PM 事务内撤销）；TTL 过期 401；读路径残余窗口断言 ≤TTL。

## 2. 挑战 2：Token 被窃取后，能否在其他 Project 重放？

- **风险等级：中**（token 一旦真实存在即成为窃取目标）。
- **场景**：攻击者从 AI-PM→AS 的单跳截获 delegation token，试图换 `X-Project-Id` 操作 Project Q；或**在 TTL 窗口内将同一 token 用于原项目内的其他操作**（R1 补充场景，P0-04）。
- **当前代码防护**：AS `Scoped` 提取器将头/体内的 project 与**凭证 scope 核对**，不一致 `SCOPE_DENIED`（`action.rs:100-105`；测试 `as2_21_lookup_requires_project_authorization`、as1 `identity_cannot_cross_tenant_or_project_scope`）；租户恒取自凭证；审计记录拒绝事件。**但跨操作复用在当前领域模型上不受防护**（`Delegation` 无操作绑定字段，审计 §3.2②）。
- **R1 裁决（P0-04）**：契约 §3.1.2 引入 operation-bound claims（`artifact_id`/`operation_id`/`caller_request_id` 条件必填）+ AS 验证矩阵 V10 目标一致性校验 + 按接口的服务端单次消费/幂等键。五种场景（合法网络重试/跨操作复用/重复提交/取消后使用/Worker 重试）判定表与测试逐项对应。`caller_request_id` 的边界已显式声明：**仅覆盖 Job 创建路径，不是全局防重放**。
- **缺失机制（残余）**：传输层建议内网 + 服务间 mTLS 属 LMMS 后续（P2），不在本次范围。
- **测试要求**：跨 project/跨 tenant 重放用例（已有，保留回归）；同 token 跨操作复用 → 403；缺绑定字段执行绑定操作 → 403；同 jti 双次提交审计告警用例。

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
- **修复建议（R1 已裁决，Q1 冻结）**：取消 Job **应同时终结本次委托**——AI-PM 在取消成功后对该 delegation 的 `jti` 执行撤销（`pm_delegation_revocation` 写入，reason=`job_cancelled`）；AS 写路径在线撤销检查使残余写能力即时失效；读路径残余 ≤ TTL。`operation_id` 绑定该 job（契约 §6.2）保证取消后原 token 无法绑定到其他 job。
- **测试要求**：取消后同 token 创建新 Job → 403（撤销生效）或绑定不一致 403（双保险）；取消后原 job 无任何状态变更（已有守卫测试保留）；取消事务内撤销失败 → 签发侧审计告警（不阻断取消本身，靠在线检查兜底）。

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

评级为 **R1 独立安全评审指定**；F-27/F-28 为 R1 新增。**F-21/F-22 涉及的凭据内容不得在本报告、PR 评论或任何测试输出中复述**。凭据处置原则：凡曾进入远端 Git 历史的凭据按已暴露处理（轮换/吊销），仅删除当前文件不构成风险消除。

| # | 等级 | 发现 | 建议 |
|---|---|---|---|
| F-20 | P2（流程） | 0005/AS-2 报告引用的规范契约 `ART-04G-*` 文件不在仓库（溯源断链） | 补交文档或在 AS-2 报告中改指本 WP 文档族 |
| F-21 | P0（LMMS 运维，非本 WP 代码） | lmms 仓库 git（**远端**）内含历史签名私钥 `keys/private.pem`（kid=default 导入路径仍活跃；R1 复核 `git ls-files keys/` 于 v1.8.2 基线命中） | **凭据已进入远端历史，按已暴露处理**：轮换/吊销该密钥对 + keystore 重建 + 历史移除；仅删文件不算完成（LMMS 侧行动项，本 WP 不执行） |
| F-22 | P0（AI-PM 运维） | `ai-pm/.test-env.md` 含明文 SSH/root、LLM API key、GitLab 凭据（内容不复述）。**R1 事实更正：文件未被 git 跟踪、从未进入任何远端历史**（`git log --all` 零命中，`.gitignore:20` 显式忽略）——上一轮"已提交进 git"表述错误 | 评级维持 P0（理由修正为"活跃明文凭据 + 无法证明未泄露 + ignore 依赖脆弱"）：**立即轮换全部凭据 + 移出工作树**；轮换前不得视其为可继续使用的凭据 |
| F-23 | P1（LMMS） | `/tokens/revoke` 公开无鉴权；`token_revoked_jtis` 无生产读取方（R1 复核仍成立）；网关黑名单 fail-open 且仅 logout 写入 | LMMS 后续需求：撤销端点加鉴权 + 验证路径接入撤销检查 + blacklist fail 策略复核 |
| F-24 | P1（LMMS） | file-service：ct/scope 缺失调用方租户回落 1（`file.rs:90` `unwrap_or(1)`）、`user_id` 调用方声明不校验（:91）、FILE_* 权限码全平台未消费 | AS 文件链路接入前与 LMMS 明确"受信调用方契约"（file 路由 mod.rs 注释已声明该模型），并把归属校验上移到 AS 服务层；**未来文件链路要求：service token `ct=false`、租户来源唯一、禁缺失租户 fallback、归属由可信服务端上下文决定、Upload Intent 绑定目标资源与 Job、Finalize 验证归属与状态——就绪前保持 fail-closed** |
| F-25 | **P0（AI-PM delegation issuer 启用前）** | 网关/`Principal` 提取器不校验用户 JWT `aud`（LMMS 侧）；同一缺口的 AI-PM 侧形态：`require_bearer` 传 `None`（`auth.rs:289`，R1 复核） | **delegation issuer 启用的前置条件**：AI-PM 按 §3.0（契约）先行落地七项验证链（aud 强制 `coder-micro-services` + purpose 强制 + ct/roles/身份类别过滤）；LMMS 侧修复（aud 白名单校验）列为后续需求随 F-23 批次 |
| F-26 | P2 | LMMS 签名密钥轮换仅手动 API；无自动调度 | 运维 runbook 补轮换节奏 |
| F-27 | P1（证据缺口，R1 新增） | **部署产物一致性证据缺失**：无运行镜像 digest、无启动版本记录、无运行行为探测记录。"1.8.2 已在私有网络发布"为用户提供的环境事实（A 类），不构成部署一致性证据（C 类） | 提供 C 类证据（镜像 digest ↔ 源码 tag 对照、启动日志、行为探测）前，任何"部署已与源码一致"的声明不成立；delegation 上线检查单加入 C 类取证步骤 |
| F-28 | P1（LMMS 缺陷，R1 新增） | `POST /tokens/resource` `ttl_sec` 输入无边界校验（`resource.rs:12,25`），`issuer.rs:253` `ttl_sec as u64` 使负值回绕为超大正数 → 可签出实际永不过期的 resource token；端点 `public=true`；**当前全平台无消费者**（grep 仅签发器与测试） | 当前无实害；未来任何消费方接入前必须先修复（输入边界校验 + 拒绝非正值 + 消除 `as u64` 转换）。LMMS 后续需求，本 WP 不修改 |

## 12. 自动化测试矩阵（汇总，执行细节见 IMPLEMENTATION-PLAN §5）

| 类别 | 用例族 | 仓库 | 状态 |
|---|---|---|---|
| 验签器 | P1-01 全矩阵（RS256/alg 降级/kid 存在与未知/iss/aud 精确单值/exp/iat 偏差/nbf/TTL≤300/jti 格式/purpose/actions 非空无重复无未知/拒绝 wildcard/错误类型与缺失字段） | AS | 待实现（随验证器，逐项对应契约 §3.2 V1–V12） |
| JWKS 缓存 | P1-02：15min 陈旧上限 fail-closed、刷新失败超窗拒绝、未知 kid 单次刷新、缺失 kid 拒绝、重启空缓存 fail-closed | AS | 待实现（契约 §4.2） |
| scope | 跨 tenant/project 重放、X-Project-Id 不一致、body tenant 不覆盖凭证 | AS | ✅ 已有部分，保留回归 + 补操作绑定用例 |
| 操作绑定 | 同 token 跨操作复用 403、缺绑定字段 403、五场景判定表 | AS | 待实现（P0-04） |
| 幂等 | caller_request_id 重放/冲突/指纹版本 | AS | ✅ 已有（AS-2） |
| 角色 | APP/WORKER/MAINTENANCE 权限探针 | AS | ✅ 已有，迁移后必跑 |
| 意图签发 | 意图→Action 映射、调用方提交 actions 拒绝、非人类身份拒发、dev-off/cpk 拒发 | AI-PM | 待实现（P0-03） |
| 上游验证链 | §3.0 七项：aud 缺失/不匹配/类型错误、purpose 枚举、ct、service roles、身份类别 | AI-PM | 待实现（P0-02） |
| 文件链路 | intent 绑定/哈希校验/补偿/孤儿清理 | AS | 待实现（阶段 3） |
| 失效关闭 | JWKS 断供、file-service 停机、**撤销服务停机 → 写 503 读不受影响** | AI-PM / AS | **部分**：JWKS 断供 → 503 `DELEGATION_VERIFIER_UNAVAILABLE` 已实现（s06）；撤销查询客户端随任务 1.7；file 停机用例属阶段 3 |
| 撤销 | 撤销后写路径 403 / 移出项目事务内批量撤销 / TTL 过期 401 / 读路径残余 ≤TTL | AI-PM / AS | **部分**：`DelegationRevocationChecker` 端口 + fail-closed 默认（端点未上线前写路径 503）已实现；查询客户端随任务 1.7；过期 401 已覆盖（s07）；AI-PM 侧撤销存储待实现 |
| 日志卫生 | 错误消息不含凭证；F-21/F-22 凭据零复述 | AS | **✅ 已实现**（验证器错误消息仅含判定原因，测试断言无 token 本体；`FailClosedRevocationChecker`/错误文案均无凭证） |

## 13. R1 独立安全评审决议（P0/P1 修订落点汇总）

R1 评审提出 5 项 P0、7 项 P1；本修订全部落点如下（详细证据见审计与契约引用处）：

| 项 | 主题 | 修订落点 | 状态 |
|---|---|---|---|
| P0-01 | 统一租户 Claim 类型 | 契约 §3.1.1：`tenant_id` = AI-PM 内部 tenant UUID；LMMS 数值 `tid` 仅可作 `lmms_tid` 追溯字段；AS 不自行推导/查询 AI-PM DB | ✅ 已修订 |
| P0-02 | 上游用户 JWT Audience 强制 | 契约 §3.0：七项验证链；aud 可证明值 `coder-micro-services`（issuer.rs:194）；**AI-PM 专用 audience 不存在 → 实施阻断项如实标注**；F-25 升 P0（issuer 启用前置） | ✅ 已修订 |
| P0-03 | 细粒度 Artifact 动作授权 | 契约 §3.3：受控业务意图接口；意图→精确 Action 映射；默认拒发清单；15 动作真实定义复核（`action.rs:6-22`） | ✅ 已修订 |
| P0-04 | 写操作重放与操作绑定 | 契约 §3.1.2：operation-bound claims + 五场景判定 + 单次消费/幂等分层；caller_request_id 边界声明 | ✅ 已修订 |
| P0-05 | Q1 撤销策略 | 契约 §5：状态存储/撤销端点/写路径时序/fail-closed/失效矩阵/保留期限；"TTL 足够短无需撤销"废弃 | ✅ 已修订 |
| P1-01 | JWT 验签矩阵 | 契约 §3.2 V1–V12 + 测试矩阵逐项 | ✅ 已补齐 |
| P1-02 | JWKS 缓存与轮换 | 契约 §4.2：15min 陈旧上限/last-successful-refresh/失败超窗 fail-closed/未知 kid 单次刷新/重启 fail-closed | ✅ 已补齐 |
| P1-03 | Project Header 语义 | 契约 §3.2 V9 四条固化；错误码沿用 `UNAUTHENTICATED`/`SCOPE_DENIED` | ✅ 已固化 |
| P1-04 | 受保护 API | Q3/Q4 冻结：全部 API 含 GET 要求 delegation；15 精确动作；每 Handler 映射有测试 | ✅ 已固化 |
| P1-05 | LMMS File Service 风险 | 审计 §1.7/"文件租户隔离已就绪"表述修正为"有条件成立"；F-24 建议补全六条未来要求；契约 §6.1 注 | ✅ 已修订 |
| P1-06 | LMMS Resource Token TTL | F-28 新增（P1）：缺陷描述 + 无消费者事实 + 未来风险评估；不修改 LMMS | ✅ 已新增 |
| P1-07 | 部署证据分级 | 审计基线表 A/B/C 三级证据声明；F-27 新增（P1）；结论限定为 B 类证据 | ✅ 已补齐 |

**残余风险（供第二轮评审裁决）**：① audience 隔离依赖 purpose 而非专用 aud（§3.0 阻断项）；② AI-PM 停机 = AS 写平面停机（§5.3 fail-closed 代价）；③ 读路径残余窗口 ≤TTL（Q1 明示接受）；④ F-21/F-22 凭据轮换未完成前的暴露面。
