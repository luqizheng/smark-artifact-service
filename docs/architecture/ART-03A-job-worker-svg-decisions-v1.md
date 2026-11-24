# ART-03A — SeeCode 输入、Job 与 Worker 设计裁决

> 状态：推荐设计裁决，供 PR review；本文件不执行数据库或运行时代码变更。
> 关联：`ART-03-seecode-adapter-design-v1.md`
> 数据库基线：`migrations/0001_init.sql`
> 目标：把 ART-03 实施前必须决定的持久化、Attempt、Worker 身份和 SVG 安全边界收敛为可实现契约。
> 补充：ART-03B-01 评审后的裁决 A3A-09 至 A3A-22 见 §11，它们是迁移 `0003_job_coordination_fixes.sql` 的契约来源。

## 1. 裁决摘要

| ID | 主题 | 推荐裁决 |
|---|---|---|
| A3A-01 | Job 输入持久化 | PostgreSQL JSONB 保存受限、结构化 SeeCode input/options；不把输入放进 Worker 内存作为唯一副本，也不提前依赖 File Service 上传协议 |
| A3A-02 | 输入限制与保留 | 输入 envelope 最大 256 KiB；结构/深度/节点/边/字符串均有独立上限；Job 终态后保留 7 天供有限诊断/重放，再清空 input/options；保留 hash 与非敏感摘要 |
| A3A-03 | Attempt | 新增 `generation_job_attempt` 表，一次实际执行一个不可复用 Attempt；Job 的 attempt_count 作为汇总字段，不作为执行历史 |
| A3A-04 | Lease | 数据库原子 claim + lease；每次 claim 产生新的 attempt_id 和随机 lease token；数据库只存 token hash；heartbeat/complete 必须匹配 attempt_id + token |
| A3A-05 | Worker 数据库权限 | Worker 使用独立 DB role，只能执行受控 claim/read-input/heartbeat/complete/fail/retry/cancel-check 函数；不授予业务表任意读写权限 |
| A3A-06 | File Service 身份 | Worker 使用独立服务身份获取 audience=file-service 的短时服务令牌；只具备必要 tenant/object 操作；绝不转发或持久化用户 delegation |
| A3A-07 | SVG 安全 | XML 解析后重建 allowlist SVG；拒绝未知元素/属性/命名空间/外部引用；不做原样透传，不以 MIME/正则作为安全判断 |
| A3A-08 | Mermaid | 从 generator_registry 种子和后续能力清单中移除；历史 migration 不改写，使用新 migration 删除 seed 行 |

这些裁决不会关闭 LMMS 授权委托 P0 或 File Service upload-intent/finalize P0。

A3A-03 至 A3A-05 在实现时暴露出未落到可执行契约的地方，补充裁决见 §11。

## 2. A3A-01 — GenerationJob 输入持久化

### 2.1 决策

在 `artifact.generation_job` 增加：

- `input_payload jsonb`：已通过 schema validation 的规范化 SeeCode input envelope；
- `options_payload jsonb`：已通过 allowlist validation 的选项；
- `input_schema_version text`：例如 `1`；
- `input_sha256 char(64)`：对规范化 input + options + schema version 的稳定编码计算 SHA-256；
- `input_expires_at timestamptz`：终态后的清理期限。

选择 PostgreSQL JSONB 的理由：

1. SeeCode 输入是有严格大小上限的结构化数据，不是大文件或二进制媒体；
2. Job 与输入同事务提交，避免“Job 已排队但输入对象上传失败/找不到”的双写窗口；
3. Worker 重试可以读取同一不可变输入，不依赖调用方在线或重复提交；
4. 不依赖尚未完成的 File Service upload-intent/finalize P0；
5. JSONB 可通过数据库权限与应用层字段屏蔽，不必为小型输入引入对象生命周期。

### 2.2 限制与隐私

- HTTP 请求体上限：256 KiB；超过返回 413。
- JSON Schema 限制：最大深度 16；节点最多 500；边最多 2,000；单个文本字段最多 4 KiB；集合/属性数量有明确上限。
- 不接受任意 URL、路径、脚本、命令、环境变量、文件引用或外部资源。
- `provenance` 只保存必要的 source_system/source_schema/correlation id，不允许把完整需求、任务、客户文本或访问令牌塞入其中。
- 输入字段不出现在普通 API Job 查询响应中；不进入日志、trace attributes、error_message 或 audit_event。
- 数据库磁盘/卷必须按部署基线启用静态加密和受限备份访问；应用层不自行发明加密密钥管理。
- Job 成功、失败、取消或超时后，设置 `input_expires_at = completed_at + 7 days`；清理任务到期后将 `input_payload`、`options_payload` 置 NULL，保留 input_sha256、schema version、generator/adapter version 和非敏感执行摘要。
- 对 queued/running/retry_wait Job 不清除输入；清理程序只处理终态记录。
- 7 天是诊断/有限重放保留期，不代表允许自动重跑已终态 Job。重新生成必须创建新 Job。

### 2.3 不选择 File Service 对象作为 Job 输入的原因

输入不是输出 Artifact，不需要用户下载/分享/长期版本管理。把输入对象化会让 Job 入队依赖 File Service 上传最终确认协议，并增加对象孤儿、删除与重放协调复杂度。未来只有输入超过 256 KiB 或确实需要独立二进制附件时，才引入受控 input attachment 机制；不能默默扩大 JSONB 限额。

## 3. A3A-03 — GenerationJobAttempt

新增 `artifact.generation_job_attempt`，建议字段：

| 字段 | 含义 |
|---|---|
| `id uuid PK` | Attempt ID |
| `job_id uuid FK` | 所属 Job |
| `attempt_number int` | 从 1 递增；同一 Job 唯一 |
| `worker_id text` | Worker 实例标识，不包含主机敏感信息 |
| `status text` | claimed/running/succeeded/failed/cancelled/timed_out/lease_expired |
| `lease_token_hash char(64)` | 随机 lease token 的 hash；不存明文 |
| `lease_expires_at timestamptz` | 当前 lease 期限 |
| `started_at / heartbeat_at / completed_at` | 生命周期时间 |
| `error_code text` | 稳定机器错误码 |
| `diagnostic_summary text` | 脱敏、限长摘要 |
| `output_sha256 char(64)` | 本次 attempt 的输出 hash（成功生成且安全检查后填写） |

约束：

- UNIQUE(job_id, attempt_number)。
- 同一 Job 最多一个 claimed/running Attempt（部分唯一索引）。
- Attempt 记录只追加状态演进，不允许重用 attempt_id。
- lease token 明文只在 claim 响应中返回一次；DB 仅存 hash，日志不记录。
- Job 的 `attempt_count` 在 claim 时递增；attempt_number 与该计数一致。
- attempt 达到 max_attempts 后不再 claim，Job 转为 failed/timed_out，不能无限循环。

Job 汇总状态由受控数据库函数维护，不能由 Worker 任意更新 Job 行。Attempt 是执行事实，Job 是用户可见的聚合状态。

## 4. A3A-04 — Claim / Lease / Recovery 协议

### 4.1 Claim

Worker 调用数据库受控函数（逻辑名称 `claim_next_generation_job(worker_id, lease_seconds)`）：

1. 在事务中选取 queued Job，或已到期且符合恢复规则的 retry_wait Job；
2. 使用 `FOR UPDATE SKIP LOCKED` 或等价原子 CAS 锁定；
3. 检查 generator 状态、max_attempts、取消标志和输入仍在保留期；
4. 创建新的 Attempt，生成高熵随机 lease token；
5. 仅将 token hash 写入 DB；
6. 更新 Job 为 running、attempt_count + 1、started_at/heartbeat_at；
7. 提交后返回 job_id、attempt_id、lease token、input/options、固定 generator/adapter/schema 版本和 deadline。

同一 Job 不允许并行有效 Attempt。旧 Attempt lease 失效后，才允许创建新 Attempt。

### 4.2 Heartbeat

- heartbeat 需要 job_id + attempt_id + lease token。
- 只有当前有效 Attempt 且 token hash 匹配才续租。
- 续租必须受最大总执行时长限制，不能无限 heartbeat 延长任务。
- 旧 Worker 的迟到 heartbeat/complete 一律拒绝，避免 lease 过期后“僵尸 Worker”覆盖新 Attempt 结果。

### 4.3 Lease 过期与恢复

- lease 过期由恢复扫描器发现；先将 Attempt 标记 lease_expired，再按重试策略将 Job 转 retry_wait 或终态。
- 对外部 File Service 上传等非事务副作用，必须使用幂等 upload intent/对象键和 attempt 隔离；旧 Attempt 不得提交 READY。
- Job 重新 claim 时创建新 Attempt，不能复用旧工作目录或输出。
- Worker 完成和 lease expiry 发生竞争时，数据库函数在同一事务中锁定 Job/Attempt，只有一个结果胜出。

### 4.4 取消

- API 取消 queued/retry_wait Job：直接将 Job 置 cancelled。
- API 取消 running Job：设置取消请求时间/标志；Worker heartbeat/取消检查发现后终止子进程并调用受控 complete-cancel。
- Worker 必须在执行前、执行后、上传前、提交 READY 前检查取消状态。
- 取消与完成竞争由数据库事务裁决；READY 提交成功后不再回滚为 cancelled。

## 5. A3A-05 — Worker 数据库与服务身份

### 5.1 数据库身份

分离 API DB role 与 Worker DB role。Worker 不获得任意业务 SQL 权限，不直接更新 `artifact.artifact`、`artifact.artifact_version`、`artifact.artifact_output`、`artifact.audit_event`。

Worker 仅可执行受控数据库函数：

- claim next job；
- heartbeat；
- fetch claimed input；
- report attempt result；
- request cancellation status；
- submit validated output completion（仅允许符合状态机与安全签收证明的参数）。

这些函数必须校验当前 Attempt、lease、Job 状态、版本绑定和 scope。Version/Output/Job 成功状态由函数在受控事务内一致提交。

Worker 数据库凭据从部署 Secret 提供，不写入镜像、配置仓库、Job payload 或日志。生产环境使用独立凭据轮换策略。

### 5.2 File Service 身份

Worker 不使用用户 delegation。Worker 以独立 service principal 获取短时 `aud=file-service` 服务令牌，最小权限仅覆盖当前租户的上传 intent、上传、finalize、verification 和必要清理操作。

在 LMMS P0 完成前，此身份和接口契约只作为目标设计；不得复用当前未经修复的 `POST /tokens/resource`，也不得把普通 bootstrap-owner token 当作项目授权委托。

Worker 不持有长期 S3/RustFS access key，不直连 RustFS。

## 6. A3A-07 — SVG 安全解析与重建

### 6.1 决策

不将 SeeCode 生成的 SVG 原样发布。使用 XML parser 解析输入，再通过严格 allowlist 将允许的节点/属性重建为新的 SVG 文档。未知内容默认拒绝，而不是保留。

推荐实现分两层：

1. **Parser**：采用禁用 DTD/外部实体/网络解析的 XML parser，限制输入字节、节点数、深度、属性数和文本长度。
2. **Safe serializer**：仅输出已批准 SVG 元素与属性；所有文本/属性通过 XML serializer 正确转义；不复制未知命名空间、处理指令、注释或原始标签片段。

禁止：

- script、事件处理属性、foreignObject、iframe、object、embed；
- DOCTYPE、ENTITY、处理指令；
- 外部 href/src、协议相对 URL、data:text/html；
- CSS url()、@import、未知 CSS property；
- 未批准 namespace、未知元素和未知属性；
- 超限 path、filter、gradient、文本或复杂度。

第一版优先支持 SeeCode 实际输出所需的静态 SVG 子集。不要为了兼容“任意 SVG”放宽 allowlist。若字体、样式或渐变导致无法安全表达，拒绝该输出并通过 fixture 增量审查扩展 allowlist。

### 6.2 测试

- 收集固定 SeeCode 0.6.3 真实 SVG golden fixtures。
- 对每个允许元素/属性建立正向 fixture。
- 对每个禁止类别建立负向 fixture。
- 验证 sanitizer 输出再次解析后仍满足 allowlist。
- 验证外部网络请求为零；浏览器加载输出时不触发外部资源。
- 验证超大输入、深层嵌套、实体膨胀、巨型 path 和大量属性被资源限制拒绝。
- 记录 sanitizer_policy_version、input_sha256、sanitized_output_sha256、violation_codes。

具体 Rust crate 选型在 ART-03B/C 的依赖审查与 PoC 中锁定；不得在未验证 SeeCode fixture 覆盖率前把某个 parser 名称写成已批准生产依赖。

## 7. A3A-08 — Mermaid seed 清理

当前 `migrations/0001_init.sql` 初始化了 `mermaid` 和 `seecode` 两条 generator_registry seed。不得修改已发布/已应用的 0001 migration。

新增后续 migration：

- 删除 `generator_registry.generator_id = 'mermaid'` 的 seed 行；
- 保留 SeeCode 行，初始状态仍为 `unavailable`；
- 对不存在的 Mermaid 行采用幂等删除；
- migration rollback 不应重新启用 Mermaid；如项目 migration 规范要求 down migration，应明确它只恢复历史数据形态，不代表产品重新支持 Mermaid。
- README、OpenAPI、测试 fixtures、seed、监控指标和 generator capability 列表不得再暴露 Mermaid 能力。

Mermaid 由 AI-PM 前端 Markdown 编辑器解析和预览；Markdown + Git 管理源码。Artifact Service 不提供 Mermaid Generator、专用 API、渲染或导出能力。

## 8. 状态一致性与输出提交

Job succeeded、Attempt succeeded、Version READY、Output safety_status=passed 必须通过单一受控提交路径协调：

1. Worker 完成渲染与安全重建，得到本地 SVG 和 hash；
2. File Service 完成 upload-intent、checksum、finalize、object verification；
3. Worker 提交 object_id、size、hash、safety policy version、Attempt lease；
4. 数据库函数验证 lease 仍有效、取消未获胜、hash/metadata 完整；
5. 在一个 PostgreSQL 事务中插入 Output、将 Version pending → ready、更新 Job/Attempt succeeded、写 audit event；
6. Artifact current pointer 通过单独的 expected-current-version CAS 更新；冲突不能使已生成 READY Version 变成失败，也不能覆盖其他当前版本。

若 File Service 上传成功而数据库提交失败，对象进入 orphan cleanup/compensation 队列；不能伪造 READY。若数据库提交成功后客户端响应丢失，幂等查询必须返回已完成 Job。

## 9. 对现有 0001 schema 的影响（后续 migration，不在本 PR 实施）

现有 `generation_job` 当前没有持久化 input/options、lease token/expiry、cancel request，也没有 Attempt 明细。实施阶段需要后续 migration 添加：

- `input_payload`, `options_payload`, `input_schema_version`, `input_sha256`, `input_expires_at`；
- `generation_job_attempt` 表及唯一约束、索引；
- `lease_expires_at` / `cancel_requested_at` 等 Job 协调字段（如实现选择放在 Attempt 表，避免重复权威字段）；
- Mermaid seed 删除。

迁移必须考虑当前 generation_job 表为空的 ART-02 已知基线，但仍需以目标数据库实际 migration 状态执行安全升级；不得改写 0001。

## 10. ART-03A 验收门槛

勾选项均由 `tests/` 中的自动化测试覆盖。

- [ ] OpenAPI/JSON Schema 对 input/options 完整定义并版本化。（过渡形态见 A3A-22：信封层 allowlist 已实现，逐字段 schema 未冻结）
- [x] 请求体、深度、节点、边、文本和总字节上限均有测试。
- [ ] Job 与 input/options 在同一 DB transaction 创建。（受控函数已实现并测试；API 创建路径未开放）
- [x] 输入不出现在普通查询、日志、trace、error_message、audit_event。
- [x] 终态保留 7 天后清理输入，保留 hash/版本元数据。
- [x] 多 Worker 并发 claim 不会重复执行同一有效 Attempt。
- [x] lease 过期、迟到 heartbeat/complete、取消竞争、Worker 崩溃均有集成测试。
- [x] Worker DB role 不能任意读写业务表。
- [x] Worker 不接收用户 delegation，不持有长期对象存储凭据。
- [ ] SVG 采用 parser + allowlist rebuild；正反向 fixture 覆盖完整。
- [x] Mermaid seed 在新 migration 删除；README/OpenAPI/测试不再声称服务支持 Mermaid。
- [ ] LMMS delegation P0 与 File Service upload/finalize P0 仍保持独立上线门禁。

## 11. ART-03B-01 评审后的补充裁决（迁移 0003）

ART-03B-01（迁移 0002）评审发现 §3 至 §5 的部分裁决没有落到可实现的契约上，并暴露出若干缺陷。迁移 `0003_job_coordination_fixes.sql` 取代了 0002 的全部协调函数；0001、0002 不改写，内容哈希由 `migrations.sha256` 锁定。

| ID | 主题 | 裁决 |
|---|---|---|
| A3A-09 | 租约参数 | `lease_seconds` 必填且在 1..3600；活跃 Attempt 的 `lease_expires_at` 必须非空（数据库 CHECK）。0002 对 NULL 的放行会产生永久 running、既不能恢复也不能取消的 Job |
| A3A-10 | 重试退避 | `retry_wait` 的 Job 在 `next_attempt_at` 之前不会被领取。第 n 次失败后的等待为 min(5s × 2^(n−1), 300s) × [0.8, 1.2)；等不到的下一次重试直接终结 |
| A3A-11 | 重试白名单 | 只有 `GENERATION_TIMEOUT` 与 `STORAGE_UNAVAILABLE` 可重试；Worker 自报的 `retryable` 不能扩大范围。租约过期（Worker 失联）按 attempt 上限重试 |
| A3A-12 | 最大执行时长 | 每个 Attempt 的 `deadline_at = started_at + max_execution_seconds`（Job 创建时固定，1..3600 秒，默认 300）。租约不会超过 deadline；超过后 heartbeat 不再续租，Attempt 按 `timed_out` 收尾 |
| A3A-13 | 取消可见性 | heartbeat 返回 `renewed / cancel_requested / deadline_exceeded / stale`；fail 与 complete 返回 `cancel_requested`。取消属于控制流，用返回值表达，不用异常；取消检查在租约校验之后，错误 token 一律 `stale` |
| A3A-14 | 完成一致性 | Job 只有在目标 Version 已是 READY、且存在 `safety_status=passed` 的主 Output、其 sha256 与上报一致时才能 `succeeded`。输出登记（Output 与 Version pending → ready）由 ART-03D 在同一事务内先行完成，随后调用 complete |
| A3A-15 | 版本固定与能力匹配 | Job 创建时从注册表固定 `generator_version` 与 `adapter_version`。`claim_generation_job(worker_id, lease_seconds, adapter_versions)` 只把 Job 交给申报了相同 adapter 版本的 Worker；没有申报能力的 Worker 领不到任何 Job。没有注册 `adapter_version` 的生成器，其 Job 不可领取 |
| A3A-16 | 幂等与输入指纹 | `(tenant_id, project_id, idempotency_key)` 部分唯一索引。同 key 同 request hash 返回原 Job，不同 request hash 报 `IDEMPOTENCY_KEY_REUSED`。`input_sha256` 由数据库计算（schema 版本加 JSONB 规范文本），调用方不再自报 |
| A3A-17 | 排队时限 | `expires_at`（默认 24 小时，1 秒至 7 天）。超过时限仍未被领取的 Job 以 `cancelled / QUEUE_EXPIRED` 收尾。生成器不可用时拒绝入队 |
| A3A-18 | 错误码 | 形式固定为 `^[A-Z][A-Z0-9_]{0,63}$`。系统错误码：`CANCELLED`、`QUEUE_EXPIRED`、`MAX_ATTEMPTS_EXCEEDED`、`LEASE_EXPIRED`、`GENERATION_TIMEOUT`。Attempt 以 `timed_out` 或 `lease_expired` 收尾时，Job 的终态为 `timed_out` |
| A3A-19 | 审计 | 受控函数内部写 `audit_event`（`generation.retry / fail / timeout / complete / cancelled / expired`，`actor_type=service`）。Worker 角色没有审计表写权限，审计只能在 `SECURITY DEFINER` 函数内完成；detail 不含输入正文 |
| A3A-20 | 取消的 scope 校验 | `request_generation_job_cancel(tenant_id, project_id, job_id)` 在函数内校验 scope；不匹配一律 `not_found` |
| A3A-21 | 领取性能与恢复扫描 | claim 分两步：先按 (generator, adapter) 取有序窗口（只走部分索引 `idx_generation_job_claimable`），再按主键加锁复核。恢复扫描用 advisory lock 保证同一时刻只有一个扫描器推进，每轮处理的数量有上限 |
| A3A-22 | 输入校验（过渡） | 信封层是 allowlist：顶层字段、`schema = smark.seecode.input`、`schema_version = 1`、`diagram` 必填。其余只有结构上限、危险字段名与 URL 形态的值拦截；逐字段的 allowlist 等输入 schema 冻结后确定 |

对原裁决的修订：

- A3A-03 列出的 `claimed` 状态仍然没有任何函数产生；claim 直接创建 `running` 的 Attempt。
- A3A-02 的各上限互相独立：500 节点与 2000 边同时取满时，元素字段较多会先触达 256 KiB 的请求体上限。
- A3A-04 §4.1 的"已到期的 retry_wait"由 `next_attempt_at` 落实，§4.2 的续租上限由 `deadline_at` 落实。
- ART-03B-01 的 Worker 构建不含任何 Adapter，因此 `run` 只做协调维护（租约恢复与输入清理），不领取 Job。领取与执行循环属于 ART-03B-02。

API 契约的配套增量待 ART-01 errata 确认，均为新增字段或响应码：`GenerationJob` 响应新增 `error_code`、`attempt_count`、`max_attempts`、`cancel_requested`；`requestGeneration` 文档化 409 与 413。

部署顺序固定为：迁移，然后执行 `deploy/db-roles.sql`，再启动服务。迁移若重建函数，其 EXECUTE 授权随旧函数丢失，所以每次迁移之后都必须重新执行授权脚本。

**裁决结论**：采用 PostgreSQL 有界 JSONB 作为 Job 输入快照，独立 Attempt + lease 作为可恢复执行模型，受控 DB 函数作为 Worker 权限边界，独立服务身份访问 File Service，SVG 使用 allowlist parse-and-rebuild。此设计允许 ART-03 后续并行实现 Worker 和 Adapter，但不允许绕过两个基础设施 P0 进入生产。
