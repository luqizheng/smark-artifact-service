# ART-03B-01 — Job 持久化与 Worker 协调基础：实施报告

> 状态：**已实现 + 已测试（本地真实 PostgreSQL 16）**；未合并，Draft PR 待审。
> 分支：`impl/art-03b-01-job-coordination`（基于 `docs/art-03-seecode-adapter-design`）。
> 设计依据：`docs/architecture/ART-03-seecode-adapter-design-v1.md`、
> `docs/architecture/ART-03A-job-worker-svg-decisions-v1.md`。
> 范围声明：本切片只做 Migration、Attempt/Lease、受控函数、Worker 骨架、权限与部署基础；
> **不实现** SeeCode / Mermaid / Node / File Service / SVG Sanitizer，不开放生产生成路径。

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

- PR Draft 创建后在本节补 CI run 链接（`.github/workflows/ci.yml`：fmt / build-test / docker-build）。
- 本地等价命令与结果见 §7；所有 CI 步骤均在本地以相同命令真实跑过。

## 9. 与 ART-03A 设计裁决的偏差

| ID | 裁决 | 本切片实现 | 说明 |
|---|---|---|---|
| A3A-01/02 | 有界 JSONB 快照、同事务、256 KiB、7 天清理 | ✅ 全实现 | 应用层 + DB CHECK 双防线 |
| A3A-03 | 独立 Attempt | ✅ | Job 保留 `attempt_count` 汇总 |
| A3A-04 | 原子 claim + lease/heartbeat | ✅ | `FOR UPDATE SKIP LOCKED`；token 明文一次性返回 |
| A3A-05 | Worker 独立最小权限 role | ✅ | 仅函数 EXECUTE，全业务表拒绝 |
| A3A-08 | Mermaid seed 后续 migration 删除 | ✅ | 幂等 DELETE，不改 0001 |
| D1 | 文档应已合并 main | ⚠️ 偏差 | ART-03A 文档仍在远端设计分支，未合并 main；实现分支基于该设计分支，Draft PR 不指向 main |
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

## 11. 已知问题与未关闭风险

- `deploy/docker-compose.yml` 基础文件在本次会话中读取被审批拦截，未修改；Worker 以 `docker-compose.worker.yml` override 形式提供独立服务，使用时需 `-f` 同时指定两份文件。**未验证基础 compose 中 postgres 服务名/网络是否与 override 完全一致**（本地 dev PG 为独立容器，非 compose 启动）。CI 未跑 docker compose 集成。
- 输入校验为"通用结构上限 + 禁字段"规则集；SeeCode 专用 JSON Schema（字段/枚举/必填）冻结属 ART-03A 验收项，本切片以结构规则近似，未做 schema golden tests。
- `VALID_JSON` 通用提取器仍允许 8 MiB，只有生成端点走 256 KiB 专用提取器；符合"生成 envelope 256 KiB"要求，其他端点不受影响。
- Worker 领取后因无 Adapter 立即以 `GENERATOR_ADAPTER_NOT_IMPLEMENTED`（不可重试）fail；这是诚实的骨架行为，避免伪造成功，但也意味着生产一旦放开 generator=available 会快速耗尽 attempt——**必须在 Adapter 就绪后再放开 generator 状态**。
