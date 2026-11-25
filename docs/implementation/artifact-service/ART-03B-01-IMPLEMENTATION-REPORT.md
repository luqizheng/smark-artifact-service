# ART-03B-01 — Job 持久化与 Worker 协调基础：实施报告

> 状态：**已合并**。PR #2 于 2026-11-24 合入 `docs/art-03-seecode-adapter-design`。
> 分支：`impl/art-03b-01-job-coordination`（基于 `docs/art-03-seecode-adapter-design`）。
> 设计依据：`docs/architecture/ART-03-seecode-adapter-design-v1.md`、
> `docs/architecture/ART-03A-job-worker-svg-decisions-v1.md`。
> 范围声明：本切片只做 Migration、Attempt/Lease、受控函数、Worker 骨架、权限与部署基础；
> **不实现** SeeCode / Mermaid / Node / File Service / SVG Sanitizer，不开放生产生成路径。
>
> **阅读提示**：§1 至 §11 记录迁移 0002 切片的原始实现（历史状态，非当前事实）。合并后的
> 评审发现其中若干缺陷，由迁移 0003 修复——第一轮见 §12.1，第二轮（PR #3 review）见 §12.2；
> 与历史描述不一致时以 §12 为准。

## 0. 实施前检查结论

| 检查项 | 结果 |
|---|---|
| Git 工作区 | 干净，无未提交修改（在 `main` @ `561ee79` 开始） |
| 远端引用 | 已 `git fetch --prune`；ART-03A 文档位于远端分支 `docs/art-03-seecode-adapter-design` @ `9de062b`，**未合并 main** |
| 分支策略 | 从设计分支新建实施分支；**不提交到 main**（符合"文档未合并则不得入 main"约束） |
| 0001 基线哈希 | `ea8acd4490516a31700005838c18a41cb7ff41e81b62f549cb407f874343a1c8`（实施后复验一致） |
| 既有状态机 | `queued→running/cancelled`；`running→succeeded/failed/cancelled/timed_out/retry_wait`；`retry_wait→queued/cancelled`（0001 触发器） |
| 既有 Job 流程 | API `request_generation` 仍 fail-closed（Generator unavailable → 503），无 Job 创建编排；`JobRepository` 只有 `get`/`cancel` |
| 实施前差异 | 见 §9「与 ART-03A 的偏差」 |

## 1. 实际修改文件

### 新增
- `migrations/0002_job_coordination.sql` — 输入快照字段、`generation_job_attempt`、协调字段、Mermaid seed 删除、10 个受控函数
- `crates/artifact-domain/src/attempt.rs` — `AttemptState` / `GenerationJobAttempt` / `ClaimedJob`
- `crates/artifact-application/src/port/worker.rs` — `WorkerCoordinationPort`、`FailOutcome`
- `crates/artifact-application/src/service/job_input.rs` — 输入 envelope 服务端校验
- `crates/artifact-infrastructure/src/postgres/worker.rs` — `PgWorkerCoordination`（受控函数 SQL 调用）
- `apps/artifact-worker/`（`Cargo.toml` + `src/main.rs` + `src/config.rs`）— 独立 Worker 二进制
- `Dockerfile.worker`、`deploy/docker-compose.worker.yml` — 独立镜像与独立服务配置
- `tests/tests/worker_coordination.rs`（11 测试）、`tests/tests/worker_roles.rs`（2 测试）、`tests/tests/migration_guard.rs`（3 测试）
- 本报告

### 修改
- `Cargo.toml`（workspace 增加 `apps/artifact-worker` 成员）、`Cargo.lock`
- `deploy/db-roles.sql`、`deploy/initdb/01-roles.sh` — 增加 `__WORKER_ROLE__` 最小权限
- `.github/workflows/ci.yml` — Worker release 编译 + 0001 哈希不可变门禁
- `.env.example` — Worker 独立 DSN/ID/租约/轮询配置
- `apps/artifact-api/src/routes/artifacts.rs` — 生成请求专用 256 KiB 提取器 + 输入校验（仍 fail-closed）
- `crates/artifact-domain/src/lib.rs`、`crates/artifact-application/src/lib.rs`、`src/port/mod.rs`、`src/service/mod.rs`、`crates/artifact-infrastructure/src/postgres/mod.rs` — 模块/端口导出；Job `cancel` 改走受控函数
- `openapi/openapi.yaml` — 503 描述不再声称支持 Mermaid
- 测试修正（未改机制）：`tests/tests/{api_meta,default_deny,migration_invariants,sdk_contract}.rs` 中 `mermaid` fixture → `seecode`

## 2. 数据库结构与权限变化

### 表/字段
- `artifact.generation_job` 新增：`input_payload jsonb`、`options_payload jsonb`、`input_schema_version text`、`input_sha256 char(64)`、`input_expires_at timestamptz`、`cancel_requested_at timestamptz`。
  - CHECK：`pg_column_size` ≤ 262144（256 KiB 第二道防线）、sha256 长度 64、input/options 成对约束。
  - 索引：输入过期时间（部分索引）、取消请求时间（部分索引）。
- 新表 `artifact.generation_job_attempt`：`id / job_id / attempt_number / worker_id / status / lease_token_hash / lease_expires_at / started_at / heartbeat_at / completed_at / error_code / diagnostic_summary / output_sha256 / created_at`。
  - `UNIQUE(job_id, attempt_number)`；**部分唯一索引 `uq_generation_job_active_attempt(job_id) WHERE status IN ('claimed','running')`** 保证同一 Job 同时最多一个有效 Attempt；租约过期索引；归属不可变 + 状态机触发器。
- **0001 未改一字**（CI + 测试双重哈希校验）。

### 受控函数（10 个，均 `SECURITY DEFINER` + `SET search_path = pg_catalog, artifact`）
| 函数 | 职责 |
|---|---|
| `create_generation_job_with_input` | Job 与输入快照**同一 INSERT/事务**落地（API role 调用） |
| `claim_generation_job` | `FOR UPDATE SKIP LOCKED` 原子领取；新 Attempt + 新随机 lease token（72 字符，两个 UUID 拼接）；DB 只存 sha256(token)；返回一次性明文 token 与输入 |
| `heartbeat_generation_job` | job_id + attempt_id + token 三要素匹配且租约有效才续租 |
| `complete_generation_job_attempt` | 验证未取消 + 租约有效 + attempt 有效；写 succeeded 与 `input_expires_at=now()+7d` |
| `fail_generation_job_attempt` | 可重试→`retry_wait`，不可重试/超限→`failed` |
| `request_generation_job_cancel` | queued/retry_wait→直接 cancelled；running→**只登记 `cancel_requested_at`** |
| `confirm_generation_job_cancelled` | Worker 确认停止后才置 Job/Attempt cancelled |
| `recover_expired_generation_jobs` | 过期 Attempt→`lease_expired`；Job 按上限转 retry_wait 或 timed_out；幂等 |
| `purge_expired_generation_job_input` | 终态过期后清空 input/options，保留 sha256/schema/执行元数据 |
| `guard_attempt_transition`（触发器函数） | Attempt 归属不可变 + 状态机白名单 |

**权限**：migration 中 `REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA artifact FROM PUBLIC`；`deploy/db-roles.sql` 按函数逐个授权 Worker（7 个）与 API（2 个）。Worker role `LOGIN NOINHERIT`，仅 `USAGE ON SCHEMA` + 函数 EXECUTE；**对全部业务表无任何 SELECT/INSERT/UPDATE/DELETE、无 DDL**。

## 3. Job 状态转换

- `queued --claim--> running --complete--> succeeded`
- `running --fail(retryable)--> retry_wait --claim(经 queued 中转)--> running`
- `running --fail(terminal/recover超限)--> failed / timed_out`
- `queued|retry_wait --cancel--> cancelled`
- `running --cancel--> running + cancel_requested_at --confirm/recover--> cancelled`
- 终态不再自动重跑；重新生成必须创建新 Job。

## 4. Claim / Lease / Heartbeat 机制

- Claim 在单事务内 `SELECT ... FOR UPDATE OF j SKIP LOCKED LIMIT 1` 锁定 Job，检查 generator=`available`、未取消、`attempt_count < max_attempts`、输入仍在；创建**新 Attempt**（不复用）；`attempt_count+1`。
- token 由 `gen_random_uuid()` 拼接生成（不依赖 pgcrypto），只在 claim 返回中出现一次；DB 存 `encode(sha256(convert_to(token,'UTF8')),'hex')`。
- Heartbeat/Complete/Fail/Confirm 都以 `lease_token_hash = sha256(token)` 且 `attempt.status='running'` 且 `lease_expires_at > now()` 为前置条件；NULL/空 token 显式拒绝（避免 SQL 三值逻辑绕过）。
- 过期租约的迟到 Worker 因 `lease_expired` 状态 + 过期时间双重校验无法覆盖新 Attempt。

## 5. 取消与恢复机制

- 取消统一走 `request_generation_job_cancel`（API `JobRepository::cancel` 已改调用），避免"直接 UPDATE"与函数两个权威来源。
- running 取消**只登记请求**，Job/Attempt 仍显示 running，不伪造 Worker 已停止；Worker 在执行前/后/提交前检查取消并调用 `confirm_generation_job_cancelled`。
- Cancel 与 Complete 都 `SELECT ... FOR UPDATE` 锁 Job 行，由数据库事务裁决唯一结果（顺序场景 + 真并发 `tokio::join!` 均测）。
- 恢复扫描只处理 `claimed/running` 且租约过期的 Attempt，标记 `lease_expired`，按 `attempt_count < max_attempts` 转 `retry_wait` 或 `timed_out`；天然幂等，不会无限重试，终态 Job 不受影响。
- 输入在终态时设置 7 天 `input_expires_at`，清理只清正文保留 hash/版本/非敏感元数据。

## 6. 输入校验与隐私

- HTTP 生成请求专用提取器限制 envelope ≤ 256 KiB（超限 413 `PAYLOAD_TOO_LARGE`）。
- 应用层 `validate_job_input_envelope`：深度 ≤ 16、节点 ≤ 500、边 ≤ 2000、单文本 ≤ 4 KiB、集合/属性 ≤ 4096、禁 URL/路径/脚本/命令/环境变量/凭据类字段名；返回稳定码 + 字段路径，**不回显输入正文**。
- 普通 Job 查询 DTO（`GenerationJobResponse`）不含任何输入字段；`PgJobRepository::get` 不 SELECT 输入列；输入不进入日志/trace/error_message/audit_event（Worker 日志只记录 job_id/attempt_id/错误码）。

## 7. 测试命令与真实执行结果

环境：本地 Docker `artifact-dev-pg`（PostgreSQL 16.16），`DATABASE_URL=postgres://postgres:artifact_dev@127.0.0.1:16500/artifact_service`，`sqlx::test` 为每测试创建独立真实库。

| 命令 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | 通过（已格式化） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过，0 warning |
| `cargo check --workspace --all-targets` | 通过 |
| `cargo test --workspace -- --test-threads=1` | **全部通过**；关键目标：`worker_coordination` 11/11、`worker_roles` 2/2、`migration_guard` 3/3、`migration_invariants` 10/10、`api_meta` 3/3、`sdk_contract` 3/3、`contract_openapi` 4/4、`default_deny` 2/2、`artifact_flow` 6/6 |
| `cargo build --release -p artifact-worker` | 通过（CI 同款命令） |
| Worker `ping`（独立 DB 角色 `art03b_worker`） | `worker database connection ok` |
| Worker `recover-once`（独立 DB 角色） | `recovery and purge completed` |

**13 项要求逐项对应**：
1. 并发 claim 不重复 → `concurrent_claim_yields_one_active_attempt` ✅
2. 有效 Attempt 唯一 → 同上（部分唯一索引 + active 计数断言）✅
3. 错误 token 拒绝 heartbeat → `wrong_lease_token_rejects_heartbeat` ✅
4. 过期 lease 拒绝迟到 complete → `expired_lease_rejects_late_complete` ✅
5. 旧 Attempt 无法覆盖新 Attempt → `stale_attempt_cannot_override_new_attempt` ✅
6. Worker 崩溃可恢复 → `crashed_worker_job_is_recoverable` ✅
7. 超最大 attempt 不无限重试 → `max_attempts_stops_retry` ✅
8. Cancel/Complete 竞争唯一终态 → `cancel_complete_race_has_single_winner` + `concurrent_cancel_and_complete_yield_consistent_terminal`（真并发）✅
9. running 取消不伪造停止 → `running_cancel_does_not_fabricate_stop` ✅
10. 终态 7 天清理保留元数据 → `terminal_input_purged_after_retention` ✅
11. Worker role 无法改业务表 → `worker_role_cannot_touch_business_tables`（**动态遍历全部 artifact 表 × SELECT/INSERT/UPDATE/DELETE，均 42501**）+ `worker_role_can_claim_and_heartbeat_via_controlled_function` ✅
12. 0001 无修改 → `migration_0001_is_unchanged`（哈希）+ CI 门禁 ✅
13. Mermaid 清理幂等 → `mermaid_seed_removed_idempotently_and_seecode_kept` ✅

## 8. CI 运行链接

- Draft PR：https://github.com/luqizheng/smark-artifact-service/pull/2
- CI Run：https://github.com/luqizheng/smark-artifact-service/actions/runs/45429394570
- 各 job：
  - fmt：https://github.com/luqizheng/smark-artifact-service/actions/runs/45429394570/job/138570160730
  - clippy + build + test (postgres 16)：https://github.com/luqizheng/smark-artifact-service/actions/runs/45429394570/job/138570161142
  - docker image：https://github.com/luqizheng/smark-artifact-service/actions/runs/45429394570/job/138570160349
- **CI 最终结果：3/3 job 全部 success**（fmt ✅ / clippy + build + test (postgres 16) ✅ / docker image ✅；run conclusion=`success`，2026-11-24 验证）。
- 本地等价命令与结果见 §7；所有 CI 步骤均在本地以相同命令真实跑过。

## 9. 与 ART-03A 设计裁决的偏差

| ID | 裁决 | 本切片实现 | 说明 |
|---|---|---|---|
| A3A-01/02 | 有界 JSONB 快照、同事务、256 KiB、7 天清理 | ✅ 全实现 | 应用层 + DB CHECK 双防线 |
| A3A-03 | 独立 Attempt | ✅ | Job 保留 `attempt_count` 汇总 |
| A3A-04 | 原子 claim + lease/heartbeat | ✅ | `FOR UPDATE SKIP LOCKED`；token 明文一次性返回 |
| A3A-05 | Worker 独立最小权限 role | ✅ | 仅函数 EXECUTE，全业务表拒绝 |
| A3A-08 | Mermaid seed 后续 migration 删除 | ✅ | 幂等 DELETE，不改 0001 |
| D1 | 文档应已合并 main | 已解决 | ART-03A 文档已由 PR #1 合入 main |
| D2 | Job+输入同事务创建 | ⚠️ 部分 | 受控创建函数已实现并测试；**API 生产创建路径仍 fail-closed**（generator unavailable + 无 adapter），未返回成功 Job |
| D3 | claim 检查 generator=`available` | ⚠️ 生产不可领取 | 守约检查 available；因无 adapter、seed=unavailable，生产环境不领取任何 Job（这正是"不开放生产生成路径"）；测试库内临时置 available 验证协调机制，**非生产绕过开关** |
| D4 | 256 KiB envelope | ✅ | 专用提取器 + 应用层校验 |

## 10. 已实现 / 已测试 / 未实现 / 生产阻断项

- **已实现**：0002 migration（字段/Attempt/协调/受控函数/Mermaid 清理）；Worker DB role 最小权限；Rust 四层协调实现；独立 Worker 二进制；输入校验；CI/部署基础；测试矩阵。
- **已测试**：上表全部 13 项；真实 PG 锁、事务、权限；真并发 claim 与 cancel/complete；Worker 二进制独立凭据启动。
- **未实现（按切片边界）**：SeeCode Adapter / Node vendor（ART-03C）；File Service 上传与 READY Version 提交（ART-03D）；SVG Sanitizer；多 Worker 生命周期与强杀（03B-02）；容器只读根文件系统/无外网/资源配额（03B-03）。
- **生产阻断项（未关闭，不得宣称可上线）**：
  1. **LMMS 授权委托 P0** 未关闭；
  2. **File Service upload-intent/finalize P0** 未关闭；
  3. 生产 generator registry 为 `unavailable`，无 Worker 领取，生成 API 仍 503；
  4. Worker 生产隔离（无外网/只读根 FS/资源限额）属 ART-03B-03，未完成；
  5. SVG 安全签收未实现。
- 本切片**不引入** Redis 或第二套 Job 权威状态源；**不增加**任何生产绕过开关；**未解除**任何 P0 门禁。

## 11. 已知问题与未关闭风险（0002 切片当时的状态；历史记录，非当前事实）

- **历史状态，已解决**：0002 切片当时 compose 未做任何验证（基础文件在当时的会话中读取被审批拦截）。0003 轮已在全新卷上完成 compose 全栈启动验证（postgres → migrate → db-grants → api / worker，角色授权、Worker 维护循环与 SIGTERM 退出均核对，见 §12）；CI 现在校验 compose 配置并构建/冒烟 Worker 镜像。
- （部分解决，见 §12）输入校验为"通用结构上限 + 禁字段"规则集；SeeCode 专用 JSON Schema（字段/枚举/必填）冻结属 ART-03A 验收项，本切片以结构规则近似，未做 schema golden tests。
- `VALID_JSON` 通用提取器仍允许 8 MiB，只有生成端点走 256 KiB 专用提取器；符合"生成 envelope 256 KiB"要求，其他端点不受影响。
- **历史状态，风险已消除**：0002 切片的 Worker 领取 Job 后因无 Adapter 立即以 `GENERATOR_ADAPTER_NOT_IMPLEMENTED`（不可重试）fail——当时若放开 generator=available 会快速耗尽 attempt。0003 之后数据库按 adapter 版本匹配领取，本切片的 Worker 不申报任何能力，领不到任何 Job，该风险不再存在；**仍须在 Adapter 就绪后再放开 generator 状态**（当前生产 seed 保持 unavailable）。

## 12. 评审修复（迁移 0003，两轮）

> 状态：**已实现 + 已本地测试**。CI 结果以 PR #3 的检查为准。
> 裁决依据：`ART-03A-job-worker-svg-decisions-v1.md` §11。
> 0001、0002 未改写，内容哈希由 `migrations.sha256` 锁定；0003 在合并前（PR #3）按评审意见就地修订，清单在同一次提交中更新。

### 12.1 第一轮修复

评审指出的问题逐项处置如下。验证一列是覆盖它的自动化测试；标为"实测"的项另在一次性 PostgreSQL 16 容器或 compose 栈上验证过。

| # | 评审问题 | 处置 | 验证 |
|---|---|---|---|
| 1 | `lease_seconds = NULL` 产生永久 running 的 Job | NULL 被拒绝；活跃 Attempt 的租约必须非空（CHECK）；迁移会修复 0002 留下的卡死数据 | `invalid_lease_seconds_never_leaves_a_stuck_job`、`migration_0003_repairs_damage_left_by_0002` |
| 2 | 重试没有退避 | `next_attempt_at` 与指数退避（A3A-10） | `crashed_worker_waits_for_backoff_before_reclaim` |
| 3 | 没有最大执行时长 | Attempt deadline；heartbeat 不再越过 deadline（A3A-12） | `deadline_caps_the_lease_and_ends_the_attempt`、`recovery_classifies_a_reached_deadline_as_timeout` |
| 4 | Worker 无法感知取消 | heartbeat 返回四态；fail 与 complete 返回 `cancel_requested`（A3A-13） | `cancel_is_visible_to_the_running_worker` |
| 5 | `complete` 可以在没有 Version / Output 时成功 | 只有目标 Version 已 READY 且有通过检查的主 Output 才成功（A3A-14） | `completion_requires_a_ready_version_with_passed_output` |
| 6 | 输入限额互相冲突，禁字段名单可绕过 | 文档规定的 500 节点 + 2000 边可以提交；信封层 allowlist；危险字段名按词元拦截；URL 形态的值被拒绝 | `job_input` 单元测试 12 例、`generation_request_enforces_the_input_envelope` |
| 7 | 本地 compose 全新启动失败 | 角色与授权移到迁移之后（`db-grants` 服务与 `deploy/dev-grants.sh`） | 实测：compose 栈在全新卷上启动，migrate 与 db-grants 均以 0 退出 |
| 8 | 版本没有在 Job 创建时固定 | 创建时从注册表固定；claim 按 adapter 版本匹配（A3A-15） | `job_creation_pins_versions_and_is_idempotent`、`claim_only_matches_advertised_adapter_version` |
| 9 | 创建幂等未实现，`input_sha256` 未校验 | 部分唯一索引；同 key 同请求返回原 Job，不同请求报冲突；`input_sha256` 由数据库计算（A3A-16） | 同上 |
| 10 | 接受空输入，排队没有时限 | 入参严格校验；生成器不可用时拒绝入队；排队时限（A3A-17） | `queue_ttl_cancels_unclaimed_job` |
| 11 | API 契约表达不了结果 | `GenerationJob` 新增 4 个字段；OpenAPI 补 409 与 413 | `get_job_serves_the_documented_shape`、`cancel_running_job_reports_cancel_requested`、`generation_job_documents_outcome_fields` |
| 12 | Worker 侧状态演进没有审计 | 受控函数内写审计（A3A-19） | `controlled_functions_write_audit_events` |
| 13 | claim 全量排序，恢复扫描重复执行 | 窗口加主键复核；advisory lock（A3A-21） | 实测：排队约 10 万条时单次 claim 约 4 至 5 毫秒，修复前约 98 毫秒 |
| 14 | 失败上报不健壮 | 诊断摘要截断；错误码形式约束；超时记为 `timed_out`；取消用返回值（A3A-18） | `long_diagnostic_is_truncated_not_rejected`、`worker_reported_timeout_is_recorded_as_timed_out` |
| — | `db-roles.sql` 的默认权限缺 `FOR ROLE` | 已补；"迁移之后必须重跑授权脚本"写进脚本头与 README | `runtime_role_is_dml_only` |
| — | 取消函数的 scope 校验在函数外 | 移入函数（A3A-20） | `job_endpoints_enforce_scope_and_action` |
| — | `cancel_job` 的成功路径没有测试 | 新增 API 层测试 | `tests/tests/generation_api.rs`（5 例） |
| — | Worker 配置写错时静默回落默认值；配置测试是空断言 | 取值不合法直接启动失败；测试改为真实断言 | `config.rs` 单元测试 3 例；compose 栈实测 |
| — | `ClaimedJob` 的 Debug 输出明文 | 手写 Debug 脱敏 | `claimed_job_debug_redacts_secrets` |
| — | 迁移哈希门禁只覆盖 0001 | `migrations.sha256` 加 `sha256sum -c`；未列入清单的迁移会使测试失败 | `released_migrations_are_unchanged` |
| — | Worker 镜像没有 CI 构建，发布流程不发布 | CI 构建并冒烟；发布流程推送 Worker 镜像 | 实测镜像构建、入口冒烟与 SIGTERM 以 0 退出 |

行为变化（调用方或运维可见）：

- Worker 不再领取任务。没有申报 adapter 能力的 Worker 领不到 Job，`run` 模式只做租约恢复与输入清理。
- 取消 running 的 Job 仍返回 200 与 `running`，新增 `cancel_requested=true`。终态 Job 上的取消是空操作，不再记审计。
- `GenerationJob` 响应新增 `error_code`、`attempt_count`、`max_attempts`、`cancel_requested`。
- 生成请求的校验失败改为 `INVALID_REQUEST` 加 `details.reason` 与 `details.field`，不再把原因拼进 message。
- 环境变量 `ARTIFACT_WORKER_LEASE_SECONDS` 与 `ARTIFACT_WORKER_POLL_INTERVAL_MS` 已移除，新增 `ARTIFACT_WORKER_MAINTENANCE_INTERVAL_MS`。租约秒数由执行循环（ART-03B-02）作为 claim 参数传入。
- `deploy/initdb/01-roles.sh` 已删除。部署顺序固定为迁移、执行 `deploy/db-roles.sql`、启动服务；compose 环境由 `db-grants` 服务完成第二步。

第一轮验证（0003 初版）：

| 检查 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过，0 告警 |
| `cargo test --workspace`（并行，PG 16 容器 `artifact-dev-pg`） | 83 通过，0 失败 |
| compose 栈（postgres、migrate、db-grants、api、worker） | 全新卷一次启动成功；角色权限矩阵与预期一致；可领取的 Job 未被 Worker 领走；排队超时的 Job 被维护循环收尾；`docker stop` 以 0 退出 |

### 12.2 第二轮评审修复（PR #3 review）

> 0003 尚未合并，本轮按评审意见就地修订 0003 并补充迁移回归测试；0001、0002 未改写。

**P1 — 历史活跃 Attempt 的 deadline 回填。** 0002 的活跃 Attempt 没有 deadline（0002 无该列），而 heartbeat 只在 deadline 非 NULL 时检查执行时限：旧 Attempt 可以无限续租，绕过执行上限。0003 现在为迁移时仍处于 `claimed`/`running` 的 Attempt 回填：

- `deadline_at = started_at + max_execution_seconds`（经 Job 关联取得；0002 时代的 Job 迁移后默认 300 秒）；
- `lease_expires_at = LEAST(lease_expires_at, deadline_at)`；
- 回填在 NULL 租约修复之后执行：那批 Attempt 已被置为 `lease_expired`，不会被回填触碰，0002 数据修复逻辑不受影响；
- 回填后 deadline 已过去的 Attempt，租约同被夹到过去，recovery 立即处理——Worker 存活到时限的按 `GENERATION_TIMEOUT` 收尾（Job 为 `timed_out`），租约先于 deadline 失效的按 `LEASE_EXPIRED`（Worker 失联）归类，与 A3A-12 的分类规则一致。

回归测试（真实 0002 → 0003 迁移路径：先只应用 0001+0002 并用 0002 的受控函数构造数据，再应用 0003 后断言；见 `tests/tests/migration_guard.rs`）：

| 场景 | 断言 |
|---|---|
| A `migration_0003_backfills_deadline_for_active_attempts` | 迁移前无 deadline 列；迁移后活跃 Attempt 的 deadline 非 NULL 且等于 started_at + 300s；3600 秒的长租约被夹到 deadline，30 秒的短租约不变；Job/Attempt 仍 running |
| B `migration_0003_expired_deadline_attempts_time_out_via_recovery` | 迁移前 started_at 已超过 300 秒而租约仍有效；迁移把租约夹到已过期的 deadline；recovery 立即收尾：Job `timed_out` / `GENERATION_TIMEOUT`，Attempt 变为 `timed_out`（不再 active） |
| C `migration_0003_heartbeat_respects_backfilled_deadline` | 迁移后 heartbeat 正常续租且不超过 deadline；申请 3600 秒续租被夹到 deadline；deadline 过后 heartbeat 返回 `deadline_exceeded`，Attempt `timed_out`，Job 进入退避（`GENERATION_TIMEOUT`） |

**退避策略对齐。** 移除实现中未经确认的 ±20% jitter，按已确认公式执行：第 n 次失败后等待 `min(5s × 2^(n−1), 300s)`。`ART-03A` A3A-10 已同步修订。确认的默认值：

- Queue TTL：24 小时；
- 单次执行时限：300 秒；
- Retry backoff：5 秒指数增长，上限 300 秒，无 jitter；
- Retryable errors：仅 `GENERATION_TIMEOUT`、`STORAGE_UNAVAILABLE`。

**当前事实与历史状态的区分。** 为避免把实施过程误读为当前能力：

- 当前 Worker 不领取任何 Job（不申报 adapter 能力）；0002 切片"领取后立即失败"是历史行为（见 §11）。
- API 层仍未接通实际 Job 创建流程：生成请求在输入校验后仍 fail-closed（503 `GENERATOR_UNAVAILABLE`），不产生 Job。数据库受控创建函数 `create_generation_job_with_input` 已实现并有测试，**不代表 API 创建路径已启用**。
- `GenerationJob` 响应新增字段与 409 / 413 已由用户于 2026-11-25 确认，并正式记录于 AI-PM 的 [ART-01 V1 API Errata 001](https://github.com/luqizheng/ai-pm/blob/master/docs/architecture/smark-pm-architecture-decisions/ART-01-V1-API-ERRATA-001.md)，同时由 ART-01 V1.0 第 15 节引用。
- compose 全栈已在全新卷上验证（见 12.1 的验证表）；CI 校验 compose 配置、构建并冒烟 Worker 镜像，发布流程推送 Worker 镜像。

**Migration 清单与 CI。** `migrations.sha256` 锁定 0001/0002/0003；CI 以 `sha256sum -c` 校验，测试 `released_migrations_are_unchanged` 用同一份清单再校验一次，并拒绝未列入清单的迁移。本轮修订后三个哈希均与清单一致，0001、0002 相对已推送基线无改动。

第二轮验证：

| 检查 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过，0 告警 |
| `cargo test --workspace`（并行，PG 16 容器 `artifact-dev-pg`） | 86 通过，0 失败（含场景 A/B/C 迁移回归） |
| `sha256sum -c migrations.sha256` | 0001/0002/0003 全部一致；0001、0002 与已合并基线相同 |
| compose `config` 与全新卷启动 | 通过：基础与 override 组合的 config 校验通过；全新卷栈 postgres → migrate（0001–0003 全部成功，含本轮修订后的 0003）→ db-grants（exit 0）→ api（约 2 秒就绪）→ worker（coordination-only）；函数授权矩阵 worker 7 / app 2 / public 0；`docker stop` 以 0 退出 |

仍未解决（不在本次修复范围）：

- SeeCode 输入 schema 冻结之前，`diagram` 内部没有逐字段的 allowlist（A3A-22）。在那之前 Adapter 不得把原始 JSON 直接交给 CLI。
- 注册表里 seecode 的 `supported_input_formats` 仍是 `vizspec`，版本仍是 `0.0.0`，等输入 schema 冻结与 vendor 引入时一并更新。
- 排队时限、退避参数与最大执行时长的默认值已确认（见上），是否需要按生成器配置尚未裁决。
- Worker 的执行循环、强杀与容器隔离（ART-03B-02、ART-03B-03）。
