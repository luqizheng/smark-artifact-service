# WP-LMMS-DELEGATION-02 — FINDINGS（缺陷分级清单）

| | |
|---|---|
| 状态 | R2 独立安全评审交付（2026-11-25） |
| 审查对象 | AS `40848f1`（PR #12 Draft）、AI-PM `6f101ce9`（PR #11 Draft）、LMMS `92691f33`（v1.8.2 线，只读） |
| 分级定义 | P0=可直接导致未授权访问/提权/跨租户/Token 混淆/重放成功/撤销绕过/授权失败仍写；P1=边界设计不完整、重要防御缺失、契约冲突或可致安全失效；P2=健壮性/可维护性/日志/配置/测试质量 |

## 汇总

| 等级 | 数量 | ID |
|---|---|---|
| **P0（已确认）** | **0** | 无 |
| P1 | 5 | R2-F-01 ~ R2-F-05 |
| P2 | 10 | R2-F-06 ~ R2-F-15 |
| 设计裁决事项 | 2 | O-5、O-6（见 [O5-O6-DECISIONS](O5-O6-DECISIONS.md)） |
| 尚缺证据 | 3 | R2-E-01 ~ R2-E-03 |

每个条目标注类型：**[已确认缺陷]** / **[潜在风险]** / **[尚缺证据]** / **[设计裁决事项]**。

---

## P1（阻断进入下一阶段的明确项）

### R2-F-01 [已确认缺陷/已声明缺口] V10 操作绑定目标一致性未实现，写路径不校验绑定字段

- **影响组件**：AS 全部写路由（`apps/artifact-api/src/routes/artifacts.rs`）、`crates/artifact-application/src/service/artifact.rs`
- **代码位置**：`crates/artifact-application/src/service/delegation_request.rs:58`（`assert_delegation_matches_request` 定义后无任何生产调用点，全仓 grep 证实）；`crates/artifact-domain/src/action.rs:95-99`（`Delegation.artifact_id/operation_id/caller_request_id` 仅被验证器形状解析，服务层零消费）
- **具体问题**：验证器只对绑定 claim 做"形状校验"（UUID/配对格式），没有任何代码把 token 的 `artifact_id`/`operation_id`/`caller_request_id` 与实际请求目标比对。契约 §3.1.2 场景②（"同一 Token 被用于不同操作 → 403"）对绑定字段**不成立**：一枚 `actions=["artifact:create"]`、scope 正确但绑定 `artifact_id=X` 的 token，可对 scope 内任意 artifact Y 成功创建资源。
- **攻击/故障场景**：持有一枚合法窄绑定 delegation 的调用方（或其被攻破的代理）在 ≤300s TTL 窗口内将同一 token 用于同 scope 同动作的不同目标——当前实现放行。
- **安全影响**：操作绑定承诺（契约 P0-04 核心）在 AS 侧无效化；写路径残余防护仅剩 scope + actions + TTL + 各端点幂等键。
- **缓解事实（如实记录）**：① 契约 §9 与 PR 说明均已声明"V10 属 AS-4 接线"，非隐瞒；② 当前实际可写的端点仅 artifact 创建/元数据/引用（Version/Generation 因 Storage/Generator 阻断 503），且生产默认空 issuer = 全拒，**当前不可利用**；③ caller_request_id 路径在 Job 创建由 AS-2 幂等（migration 0005）间接防护。
- **修复建议**：AS-4 接线时，在全部写路径（9 个写 handler/服务入口）调用 `assert_delegation_matches_request`（或等价的绑定一致性判定），绑定 claim 存在时与请求目标逐一比对（artifact_id、operation_id、caller_request_id、request_hash）。
- **验收标准**：① 每个写路由各有"绑定字段与请求目标不一致 → 403 SCOPE_DENIED"的集成测试（真实 HTTP + DB）；② 三个绑定字段各自独立变异测试；③ grep 断言 `assert_delegation_matches_request` 在每个写服务入口被调用（测试或编译期约束）。

### R2-F-02 [已确认缺陷/已声明缺口] V11 撤销闭环未实现：检查器未接入任何请求路径

- **影响组件**：`crates/artifact-application/src/port/revocation.rs`、`apps/artifact-api/src/state.rs`、全部写路径
- **代码位置**：`apps/artifact-api/src/state.rs:67-89`（`app_state_with_verifier` 组装 `ArtifactService` 时不含任何 `DelegationRevocationChecker` 字段）；`crates/artifact-application/src/port/revocation.rs:40-52`（`FailClosedRevocationChecker` 仅有端口单测，无消费者）；AI-PM 侧撤销端点不存在（PR #11 仅文档）
- **具体问题**：端口与 fail-closed 默认实现存在，但：① `AppState`/`ArtifactService` 未持有 checker；② 服务层写路径无任何撤销查询调用；③ AI-PM `GET /internal/delegation-revocations/{jti}` 未实现；④ 因此契约 §5.3 的"AI-PM 停机 = AS 写平面 503"fail-closed 语义**也未生效**（既没有查询，也就没有 fail-closed 点）。
- **攻击/故障场景**：用户被移出 Project / 被禁用 / 管理员紧急撤销 / Job 取消后，其已签发 delegation 在全部 TTL（≤300s）窗口内对**全部写路径**继续有效——Q1 裁决要求写路径残余窗口≈0，当前实际=TTL 全窗。
- **安全影响**：撤销能力整体缺失（读路径 TTL 收敛是知情接受，写路径不收敛违反 Q1）。
- **修复建议**：AI-PM 实现撤销存储+查询端点（阶段 1 任务 4）；AS 实现 checker 客户端（任务 1.7）并在 AS-4 接线中置于"V1–V10 全过后、业务写入前"；超时 2s；任何错误 → 503 `REVOCATION_UNAVAILABLE`（错误码映射已存在于 `crates/artifact-contract/src/error.rs:95`，运行路径不存在）。
- **验收标准**：① 写路径集成测试：撤销后下一写请求 403；② 撤销端点不可达/超时/非 200/畸形响应四类场景全部 503 `REVOCATION_UNAVAILABLE`（真实网络断言，非仅 mock）；③ 并发撤销与写入竞态测试（撤销事务与在途写入无部分写入）；④ 无进程级撤销缓存（代码审查断言）。

### R2-F-03 [已确认缺陷] 三个读 handler 绕过动作校验 + `/v1/generators` 完全无认证（违反 Q3/V8）

- **影响组件**：`apps/artifact-api/src/routes/artifacts.rs`、`apps/artifact-api/src/routes/mod.rs`
- **代码位置**：`routes/artifacts.rs:370-390`（`get_version` 直接调 `state.artifacts.versions.get(...)`，无 `require(Action::VersionRead)`）；`:392-411`（`list_versions` 同）；`:444-459`（`list_references` 直接调 `state.artifacts.references.list(...)`）；`routes/mod.rs:117-122`（`list_generators` handler 无 `Authed`/`Scoped` 提取器）
- **具体问题**：这些 handler 通过 `Scoped` 验证了 token 有效性并用凭证 tenant 组装 scope，但**没有动作检查**：持 `actions=["artifact:read"]`（无 `version:read`）的 delegation 可读取 scope 内任意 artifact 的版本列表/详情；持任意合法 token 者可匿名访问 `/v1/generators`。契约 Q3"所有受保护 API——包括只读 GET——均要求 delegation"、V8 动作模型被绕过。`list_generators` 连 token 都不需要。
- **攻击/故障场景**：合法低权限 token 动作提升（scope 内读越权）；generator 注册表（含 generator_id/版本/状态）未认证暴露。
- **安全影响**：动作授权模型在三个读端点失效；不跨租户、不写（P1 而非 P0）。当前生产姿态（空 issuer 全拒）下不可利用。
- **溯源说明**：`routes/*.rs` 不在 PR #12 diff 中，属 main 既有缺口（PR 未引入也未修复；但 R2 范围是完整授权闭环审计，必须记录）。
- **修复建议**：三个读路径下沉到服务层并补 `require(Action::VersionRead / ArtifactRead)`；`/v1/generators` 按 Q3 决策：挂 `Authed` + `generation:read`（推荐），或在契约中明确豁免并说明理由。
- **验收标准**：① 无 `version:read` 的 token 调两个版本读端点 → 403；② 无 token 调 `/v1/generators` → 401（或契约豁免依据入档）；③ 全路由审计测试：枚举 router 全部路由断言"业务路由均经动作检查"（防回归）。

### R2-F-04 [已确认缺陷] 实现以不存在的"契约 v0.2 §3.4"为规范性依据（契约引用失实 + 与 §5.1 冻结裁决冲突）

- **影响组件**：`migrations/0006_delegation_token_use.sql`、`crates/artifact-application/src/port/delegation_binding.rs`、`crates/artifact-infrastructure/src/postgres/delegation_binding.rs`、`crates/artifact-contract/src/error.rs`
- **代码位置**：`migrations/0006_delegation_token_use.sql:3`（"唯一规范性依据：…契约 v0.2 §3.4"）；`port/delegation_binding.rs:1`（"契约 v0.2 §3.4"）；`:9-11`（"契约 v0.2 §3.4.4"）；`error.rs:70`（"WP 契约 v0.2 §3.4"）
- **具体问题**：冻结契约 v0.2（`26a37f3`）不含 §3.4（实际小节为 §3.0–§3.3、§4–§9）；且 §5.1 冻结裁决明确"AS 不加表"。O-5 行（实现提交追加至 §8）如实声明了冲突并提请裁决——但迁移文件与代码注释以"契约 §3.4"自我授权，把**待裁决项**呈现为**已裁决规范**，对后续审计者构成误导。冻结裁决（Q1–Q6、§5.1 原文）未被改动（diff 已核对，`40848f1` 对契约仅追加 O-5/O-6 与 §9）。
- **安全影响**：不影响运行时（该表无消费者）；但契约一致性被破坏，是 O-5 裁决必须剥离的失实前提。
- **修复建议**：见 [O5-O6-DECISIONS](O5-O6-DECISIONS.md) 裁决（本评审建议移除绑定表与迁移 0006）；若保留任何相关代码，必须先完成契约修订并修正全部引用。
- **验收标准**：仓库内不再存在指向不存在契约小节的规范性引用（CI 文档检查可防回归）。

### R2-F-05 [已确认缺陷/闭环缺失] AI-PM 侧授权闭环零实现：签发链、验证链、撤销、JWKS 均不存在

- **影响组件**：ai-pm 仓库全部（PR #11 仅 1 个文档文件，61 行；无 CI check）
- **代码位置**：`ai-pm apps/pm-api/src/auth.rs:289`（`verify_token(token, jwks, None)`——aud 不校验，F-25 确认仍在）；ai-pm 全仓 grep "delegation" 无代码命中
- **具体问题**：完整闭环"LMMS 身份 → §3.0 验证链 → 业务意图授权 → 签发 → pm_delegation 存储 → 撤销端点 → JWKS 端点 → artifact-service 客户端"在 AI-PM 侧**零实现**。契约 §3.0 七项验证链（delegation 签发唯一准入）、P0-03 意图接口、Q1 撤销存储全部未建设。授权闭环当前只有 AS 验证段（V1–V9）+ 未接线的 V10/V11 端口。
- **安全影响**：P0-01（AI-PM audience 隔离）目前**无对象**——不是"隔离已做好"，而是"隔离机制尚未存在"；同 issuer 同密钥不同业务 Token 间的混淆防护依赖尚未落地的 §3.0 链。
- **性质判定**：与交付计划一致（AI-PM 侧明确"第二轮评审通过前不得实现"），属**阶段性缺口**而非隐瞒缺陷；但作为 R2 结论必须如实计入：**闭环未成立**。
- **修复建议/验收标准**：见 [RELEASE-GATE](RELEASE-GATE.md) 阻断条件 G5/G6（阶段 0 验证链 + 阶段 1 签发/撤销/JWKS + E2E 联调）。

---

## P2（健壮性/配置/证据质量）

### R2-F-06 [已确认缺陷] 契约固定受众仅靠默认值保证，配置可覆盖为任意非空值

- **代码位置**：`apps/artifact-api/src/config.rs:14-17`（`audience` 带 serde default，`validate_all:43-48` 仅查非空）；`state.rs:43-50`（直通 `TrustedIssuer.audience`）
- **问题**：`ARTIFACT_API_DELEGATION_ISSUERS` 中 `audience` 可配置为任意非空字符串（如 `coder-micro-services`），AS 将接受该 aud 的 delegation token。契约 §3.1.1"aud=artifact-service 精确单值"被实现为"默认值+可配置"。
- **PoC 证据**：R2 PoC P8（`poc/r2-verifier-poc`）——配置 `audience=coder-micro-services` 后 aud 为该值的 delegation token **被接受**。
- **缓解**：purpose==delegation 检查仍拦截普通用户 Token；受信签发方唯一（Q6）；签名密钥独立。因此为配置弱化而非可直接利用漏洞。
- **修复建议**：删除 `audience` 配置项（结构上钉死 `DELEGATION_AUDIENCE`），或 `validate_all` 强制 `audience == DELEGATION_AUDIENCE` 否则启动失败。
- **验收标准**：配置非 `artifact-service` 受众 → 进程启动失败（负面配置测试）。

### R2-F-07 [潜在风险] JWKS 公钥无最小密钥强度校验

- **代码位置**：`crates/artifact-infrastructure/src/delegation/jwks.rs:225-240`（仅过滤 RSA + kid，无模长下限）
- **问题**：`DecodingKey::from_jwk` 接受任意 RSA 模长；若 AI-PM 侧误生成弱密钥（如 1024/512 位），AS 无纵深防御。信任模型内（AI-PM 是受信方）风险低。
- **修复建议**：`try_fetch` 过滤时要求模长 ≥ 2048 位；JWKS 单测覆盖弱密钥拒绝。
- **验收标准**：JWKS 返回 1024 位 RSA key → 该 kid 不可用（UnknownKid），2048 位正常。

### R2-F-08 [潜在风险] JwksCache 持锁跨 HTTP await：JWKS 端点缓慢时验证路径串行阻塞

- **代码位置**：`jwks.rs:88-182`（`lookup` 全程持有 `entries.lock().await`，内含 `try_fetch`（5s 超时））
- **问题**：AI-PM JWKS 端点响应缓慢（不超时但慢）时，**所有** token 验证串行排队（每请求最长 ~5s），叠加 10s TimeoutLayer 可能造成验证面整体超时——自造成可用性事件（fail-closed 方向正确，但影响读+写全平面）。
- **修复建议**：fetch 在锁外进行（double-check 模式）或单飞（single-flight）刷新；保留"新键先查缓存"语义。
- **验收标准**：模拟 3s 延迟 JWKS，10 并发验证请求总时延无串行放大。

### R2-F-09 [已确认缺陷] 审计事件缺失 jti/主体/原因（契约 V12 不满足），审计写失败 fail-open

- **代码位置**：`apps/artifact-api/src/error.rs:76-93`（401/403 审计仅 method/path，无 subject/jti/delegation_id/reason）；`crates/artifact-application/src/service/artifact.rs:680-703`（允许审计含 delegation_id 但无 jti）；两处 `tracing::warn!` 后继续（审计失败不阻断）
- **问题**：V12 要求"每次验证结果（含拒绝）入 audit_event：jti/delegation_id/subject/actions/判定原因"。当前：拒绝事件无 jti/subject/原因；允许事件无 jti。按 jti 的撤销取证链在 AS 侧断裂（契约 §5.1 依赖"审计可按 jti 追溯到签发事件"）。审计库写失败仅告警（fail-open audit——可辩护的可用性取舍，但与 V12"每次验证结果"承诺不符，必须显式声明）。
- **修复建议**：拒绝路径在验证器返回中携带 jti（可解析时）入审计；允许路径补 jti；审计失败语义写入契约（fail-open + 计数告警，或关键写路径 fail-closed）。
- **验收标准**：审计事件（允许+拒绝）均含 jti 与判定原因；集成测试断言审计行内容。

### R2-F-10 [潜在风险] SSRF 残余：JWKS 域名 DNS 解析无 pinning

- **代码位置**：`jwks.rs:210-241`（固定 URL、禁重定向、证书校验 ✓；但每次 fetch 按当前 DNS 解析出站）
- **问题**：DNS 重绑定/劫持可将 JWKS 域名解析到攻击者 IP（需同时持有效 CA 证书才能完成 TLS——风险进一步收敛）。URL 为运维受信配置，非请求派生（无请求级 SSRF）。
- **结论**：接受为残余风险并记录；若需强化，允许配置 pin（SPKI hash）作为后续增强（非阻断）。
- **验收标准**（如实施）：配置 SPKI pin 后不匹配即拉取失败。

### R2-F-11 [已确认缺陷] 并发"同 jti 不同指纹"真并发场景未测（s14 为同指纹、s13 为顺序异指纹）

- **代码位置**：`tests/tests/delegation_security.rs:577-604`（s14）、`:540-574`（s13）
- **问题**：数据库唯一约束仲裁逻辑覆盖了两个组合的真子集；"两个并发请求同时消费同一 jti、指纹不同 → 恰一个 Fresh、另一个 `DELEGATION_TOKEN_REUSED`"未直接测试（代码路径 `postgres/delegation_binding.rs:91-109` 的 mismatch 分支在 s13 已顺序覆盖，并发竞态窗口未覆盖）。
- **修复建议**：新增并发异指纹用例（`tokio::join!`，断言 outcomes ⊆ {Fresh, Err(REUSED)} 且恰一个 Fresh）。
- **验收标准**：测试加入 delegation_security 且 CI 稳定通过。

### R2-F-12 [已确认缺陷] 契约 §9 证据映射失实：V6/V7/V8 声称的测试证据不存在

- **代码位置**：契约 `WP-LMMS-DELEGATION-01-AUTHORIZATION-CONTRACT.md` §9 表（V7 证据"拒绝矩阵"、V6"ai_pm 单测 + s 族"、V8"s12；ai_pm 单测"）；对照 `tests/tests/delegation_security.rs`（23 个测试函数逐一核对）与 `ai_pm.rs` 单测（5 个：issuer×2、时间、biz 配对、actor_type）
- **问题**：**无 purpose 拒绝测试、无 jti 非 UUID 拒绝测试、无 actions 形状拒绝测试**（通配/未知/重复/空）；s12 是 V9 层（动作不在 token 集合）不是 V8 层（claims 形状）。R1 的 P1-01 要求"每项一个测试用例"未兑现。R2 PoC 独立验证**代码防御存在且有效**（P1–P7 全部正确拒绝），故为证据缺失而非防御缺失——但安全交付的证据表不得声明不存在的证据。
- **修复建议**：补 V6/V7/V8 验证器级负面测试各≥1（可参照 PoC 场景 P1–P7 直接移植进 `tests/tests/delegation_security.rs`）；修正契约 §9 表。
- **验收标准**：PoC 的 P1/P1b/P2/P3/P4/P5/P6/P7 对应用例全部进入交付测试矩阵并通过 CI。

### R2-F-13 [潜在风险] 指纹规范歧义：path 参数实例化形态与 query 处理未冻结（O-6 关联）

- **代码位置**：`crates/artifact-application/src/service/delegation_request.rs:3-15`（规范："path_with_prefix"未定义参数实例化；query 完全不参与）
- **问题**：① `/v1/artifacts/{artifact_id}` 签发方与 AS 必须对"实例化后的字符串"达成一致（当前规范未写明模板 vs 实例化）；② query 不入指纹——现写端点均无 query（已核对）故无现实遗漏，但规范未禁止未来带 query 的写端点；③ 重复键/浮点规范化不确定性仅靠"签发方拒签"自律，AS 侧不可检测（parse 已合并重复键）。
- **修复建议**（并入 O-6 裁决）：规范冻结为"实例化路径 + 无 query + UTF-8 字节"；补算法版本标识；若未来写端点引入 query，指纹规范必须同步修订。
- **验收标准**：契约修订文本含明确 BNF/示例；AI-PM 与 AS 交叉向量测试（同输入同输出）。

### R2-F-14 [潜在风险] create 路径 project 定位与读路径来源不对称（body vs header）

- **代码位置**：`routes/artifacts.rs:217-235`（create 用 body.project_id 构造 scope）；其余端点用 `X-Project-Id` 头
- **问题**：两者都只是"定位"（授权以 claims 为准，Q2），无安全缺口；但客户端联调易错（create 不带头、read 必带头），错误信息路径不同（s18 400 vs s11 403）。契约 §3.2 V9 允许"X-Project-Id/请求体"两种定位。
- **修复建议**：非阻断；建议契约注明各端点定位来源清单，联调前对齐客户端实现。
- **验收标准**：契约补充定位来源表（文档变更）。

### R2-F-15 [潜在风险] 时钟偏差叠加使 token 实际接受窗最大 ≈ iat+300s 后再 +60s（leeway 语义说明）

- **代码位置**：`ai_pm.rs:36`（leeway=60）、`:145-170`（`validate_time_claims`）、jsonwebtoken exp/nbf 校验带 leeway
- **问题**：exp 校验在 leeway 内放行（now < exp+60），叠加 TTL≤300 上限，单 token 最长可接受 ≈360s。与契约 ±60s 声明一致、属知情设计，记录以防未来误判"TTL=300 严格"。
- **验收标准**：无需修复；契约 V5 行可补充一句"exp 判定含 60s leeway"。

---

## 尚缺证据（无法从仓库/运行证据核实，列入 RELEASE-GATE）

### R2-E-01 F-22 凭据轮换完成证据
AI-PM `.test-env.md` 明文凭据的轮换属仓库外动作，本轮无法核实。进入阶段 1 前必须提供轮换完成声明（不含任何凭据内容）。[尚缺证据]

### R2-E-02 AI-PM 阶段 0（§3.0 验证链）实现与测试
F-25（aud 不校验）确认仍在（`verify.rs` `expected_aud=None → validate_aud=false`）。阶段 0 七项验证链的实现与逐项测试不存在。[尚缺证据]

### R2-E-03 端到端联调（签发 → 验证 → 撤销 → 写入 → 幂等重放）证据
两侧均为独立组件级证据，无任何跨服务 E2E 运行记录（AI-PM 侧无实现，客观上不可能有）。[尚缺证据]

---

## 与任务书 P0 专项的对照结论

| 专项 | 结论 | 关联 Finding |
|---|---|---|
| P0-01 AI-PM Audience 隔离 | 隔离机制**尚未存在**（AI-PM 零实现）；AS 侧对 LMMS 形态 token（purpose=user_login）与数组 aud 的拒绝经 PoC 验证有效；aud 固定值可被配置覆盖（P2） | R2-F-05、R2-F-06 |
| P0-02 V10 目标一致性 | `assert_delegation_matches_request` 存在但零调用；绑定字段全写路由均不校验 | R2-F-01 |
| P0-03 V11 撤销 | 端口存在、零接线、AI-PM 端点不存在；fail-closed 语义未生效 | R2-F-02 |
| P0-04 写操作授权闭环 | 9 个写 handler 全部经 `Authed/Scoped` + 服务层 `require`（逐一路由核对，无绕过；写前授权、无部分写入——幂等/乐观锁/状态机守卫）；但闭环缺口=V10/V11 未接线、三个读端点绕过动作检查、`/v1/generators` 无认证 | R2-F-01/02/03 |
