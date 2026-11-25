# ART-03B-02 — Worker 执行进程生命周期设计（草稿，待评审）

> 状态：**SUPERSEDED / 已被 v2 取代（2026-11-25）**。
> 本稿经架构评审结论为 **Changes Required / 打回重设计**（状态机不闭合、数据库终态语义理解错误、进程树回收保障不足、终态提交不确定性未处理、重复执行副作用边界不清晰）。
> **v2 为新的设计基线**，见：
> - `ART-03B-02-worker-process-lifecycle-design-v2.md`（主设计）
> - `ART-03B-02-state-transition-matrix-v2.md`（状态转换矩阵）
> - `ART-03B-02-failure-scenarios-v2.md`（故障场景分析）
> - `ART-03B-02-test-strategy-v2.md`（测试策略）
> - `ART-03B-02-decision-record-v2.md`（决策记录与反向审查）
>
> 本文件仅保留历史记录，其内容**不再具有设计效力**；其中与 v2 冲突之处（如 §4 D3 "deadline_exceeded → fail(GENERATION_TIMEOUT)" 的二次提交缺陷、§7 待裁 5）一律以 v2 为准。
> 关联：`ART-03A-job-worker-svg-decisions-v1.md`（尤其 §11 A3A-09~22）、`ART-03B-01-IMPLEMENTATION-REPORT.md`、迁移 `0002/0003`、`deploy/db-roles.sql`。
> 基线：main `c11e79a51a145ded026632630bb0b52aa44dc311`（PR #3 已合并，CI success，迁移校验一致）。

## 0. 结论先行

| 目标 | 现状 | 处置 |
|---|---|---|
| PostgreSQL 作为 Job/Attempt 状态唯一权威 | ✅ 已达成（0003 受控函数 + Worker 只读受控端口） | 保持不变，本设计不引入任何新协调组件 |
| 迟到 heartbeat/complete 覆盖数据库终态 | ✅ 数据库侧已防（token+running+租约期三重校验，一律返回 stale） | Worker 侧补充"单次终态提交"纪律（§5 D5） |
| 取消/超时/崩溃的收尾 | ✅ 数据库语义已备（cancel_requested / deadline_exceeded / stale 返回值 + recover_expired） | 🟡 Worker 侧执行循环尚未存在，属本切片新建 |
| 独立执行进程 / 隔离边界 | ❌ 不存在（worker 目前为 coordination-only，全仓无进程创建代码） | 本设计核心：每 Attempt 独立 executor 子进程（§4 D1） |
| Supervisor 监控与 hard-kill | ❌ 不存在 | 本设计核心（§4 D2） |
| 防孤儿进程 | ❌ 不存在 | 三层防护（§6 D4） |
| shutdown 可靠收尾 | 🟡 有 SIGTERM 监听但无 in-flight 处理 | 有界排水 + 放弃提交交由租约恢复（§4 D3） |

## 1. 只读审计结论（2026-11-25，main c11e79a）

### 1.1 已具备的数据库侧保障（不改动，直接依赖）

| 机制 | 位置 | 语义 |
|---|---|---|
| 原子 claim + lease | `artifact.claim_generation_job` | 新 Attempt + 高熵 token（DB 只存 hash），领不到不匹配 adapter 版本的 Job |
| heartbeat 三重校验 | `artifact.heartbeat_generation_job` | attempt_id + token hash + status=running + 租约未过期，任一不满足返回 `stale`；超 deadline 返回 `deadline_exceeded` 并由 DB 就地收尾 timed_out；取消登记返回 `cancel_requested` |
| complete 校验 | `artifact.complete_generation_job_attempt` | 同上校验；终态后调用一律 `stale`；且要求 Version READY + 主 Output passed + sha256 一致才可能 succeeded |
| fail / confirm_cancelled | 同族函数 | 同样校验租约归属；取消是控制流返回值（A3A-13） |
| 恢复扫描 | `artifact.recover_expired_generation_jobs` | advisory lock 单扫描器；租约过期 → lease_expired → 按上限重试（A3A-11） |
| 审计 | 受控函数内部 `audit_event` | Worker 角色无审计表写权限（A3A-19） |

**判定：任务书中"防止迟到 heartbeat / complete 覆盖数据库终态"在数据库层已经闭环。** Worker 侧需要做的是不与之对抗的客户端纪律（§5 D5），而不是新的数据库机制。

### 1.2 缺失部分（本切片范围）

- `apps/artifact-worker` 目前只有协调维护循环（租约恢复 + 输入清理）；无 claim、无执行、无子进程、无 heartbeat。全仓除 `artifact-api/build.rs`（查 git 版本号）外无任何进程创建代码。
- 无 executor 契约、无进程树终止原语、无孤儿防护、无 in-flight shutdown 收尾。
- 配置项 `ARTIFACT_WORKER_LEASE_SECONDS` 已随 03B-01 移除，需由本切片作为 claim 参数恢复。

### 1.3 审计发现的风险点（现行代码）

| # | 风险 | 等级 | 处置 |
|---|---|---|---|
| R1 | worker 收到 SIGTERM 立即 return，未来 in-flight Attempt 将无人 heartbeat，只能等租约过期 | 🟡 | D3 shutdown 收尾 |
| R2 | SeeCode/Node 子进程树可能自带孙进程；单 PID kill 无法覆盖全树 | 🟡 | D1 进程组 / Job Object |
| R3 | supervisor 被 `kill -9` / OOM 后，已 spawn 的 executor 无父看护会残留为孤儿 | 🟡 | D4 PDEATHSIG / watchdog |
| R4 | executor 与 supervisor 共享环境变量时，崩溃/hung 的执行代码可能直接触达数据库 | 🔴（若不隔离） | D1 executor 无 DB 凭据 |
| R5 | 旧 Attempt 工作目录复用会泄漏输入/输出（A3A-04 明令禁止） | 🟡 | D4 attempt 专用临时目录 |

## 2. 架构边界（保留，不放宽）

- PostgreSQL 继续是 Job/Attempt 状态**唯一权威**；不引入 Redis；不让 RabbitMQ/任何 MQ 取代状态协调。
- Worker 只经 `WorkerCoordinationPort` 调用 0003 受控函数，使用独立 DB role（`deploy/db-roles.sql`）；迁移 0001/0002/0003 **不改写**。
- Worker 不访问 AI-PM 业务数据库；不读取用户 delegation；不持有长期对象存储凭据。
- **生产 P0 门禁不绕过**：LMMS delegation 修复、File Service upload-intent/finalize 修复两项门禁保持独立上线条件；本切片不触碰 File Service 上传与 READY 提交（ART-03D 范围）。
- 容器只读根文件系统/无外网/资源配额属 ART-03B-03，本设计不伪造完成。

## 3. 目标拓扑

```
                        ┌────────────────────────────────────────────┐
                        │            supervisor（worker 主进程）        │
                        │  持有 ARTIFACT_WORKER_DATABASE_URL（唯一持有者）│
                        │  claim → spawn → heartbeat → 收尾状态机        │
                        └───────┬───────────────────────┬────────────┘
                                │ stdin: input envelope  │ stdout/exit+结果文件
                                ▼                        ▼
                    ┌───────────────────┐      ┌──────────────────────┐
                    │ executor（每 Attempt │      │ PostgreSQL 受控函数     │
                    │ 一个独立子进程）      │      │ （唯一状态权威，仅       │
                    │ 无 DB 凭据、无外网    │      │  supervisor 可达）      │
                    └───────────────────┘      └──────────────────────┘
```

- **executor**：每 Attempt 一个独立子进程，运行 Adapter（03C 接入）。本切片先以 stub executor 落契约（§7 待裁 4）。
- **supervisor**：worker 主进程内的执行循环 + 收尾状态机；是唯一连接数据库、唯一决定提交的组件。

## 4. 设计裁决

### D1 独立执行进程与隔离边界

| 项 | 裁决 |
|---|---|
| 隔离单位 | 每 Attempt 一个独立 executor 子进程；Attempt 结束进程必亡，不复用 |
| 进程组 | Unix：executor 以独立 process group 启动（`setpgid`），hard-kill = `kill(-pgid, SIGKILL)`，覆盖孙进程 |
| Windows 等价 | Job Object + `KILL_ON_JOB_CLOSE` + `TerminateJobObject`（用户本机 Windows 开发环境与 Linux 部署环境双支持） |
| 父死防护 | Unix：executor 侧 `prctl(PR_SET_PDEATHSIG, SIGKILL)`；Windows：executor 周期性检查父进程存活（watchdog 线程，间隔 ≤ 2s），父亡即自杀退出 |
| 输入通道 | stdin 传 input envelope（JSON，≤256 KiB，一次性写入后关闭）；不经环境变量、不经命令行参数 |
| 结果通道 | attempt 专用临时目录内的结果文件（attempt_id 命名）+ 退出码协议；stdout/stderr 仅日志脱敏采集 |
| 凭据隔离 | executor 进程环境中**剥离** `ARTIFACT_WORKER_DATABASE_URL` 及一切 DB/对象存储凭据；执行代码即使失控也无 DB 访问面（消解 R4） |

### D2 Supervisor 职责与执行循环

| 项 | 裁决 |
|---|---|
| claim | 每轮 claim 一个 Job（capabilities = 编译进本构建的 adapter 版本表；无 adapter 即领不到，与 03B-01 行为兼容） |
| lease | `ARTIFACT_WORKER_LEASE_SECONDS` 恢复为配置项（1..3600，默认 60）；heartbeat 间隔 = lease/3，tick 对齐 `MissedTickBehavior::Delay` |
| heartbeat 纪律 | 仅当 executor 存活且本 Attempt 未终态化时发送；收到 `cancel_requested`/`deadline_exceeded`/`stale` 后停止 heartbeat |
| 执行监视 | 等待 executor 退出 + 结果文件；deadline 计时用 DB 下发的 `deadline_at`（本地钟不可信） |
| hard-kill | 收尾需要终止时：先 SIGTERM 进程组 → 宽限期（默认 10s，配置项）→ SIGKILL 进程组；Windows 对应 TaskClose/Terminate |

### D3 收尾状态机（每 Attempt 恰好一次终态提交）

```
spawn ──► running ──┬─ executor 退出 + 结果有效 ──────► complete(output_sha256)
                    │                                    └─ cancel_requested → 转 C 路径
                    ├─ heartbeat: cancel_requested ────► SIGTERM→宽限→SIGKILL → confirm_cancelled
                    ├─ heartbeat: deadline_exceeded ───► hard-kill → fail(GENERATION_TIMEOUT, retryable)
                    ├─ executor 崩溃/无结果文件 ────────► fail(<稳定错误码>, 按白名单)
                    └─ shutdown 触发 ─────────────────► 有界排水（§D3.1）→ 排完走对应正常收尾；
                                                          排不完 → 终止 executor → 放弃提交
```

| 路径 | 提交 | 关键纪律 |
|---|---|---|
| 成功 | `complete`（输出登记由 ART-03D 先行；本切片 stub 无输出，路径由测试模拟） | 收到 `cancel_requested` 立即放弃成功路径转取消 |
| 取消 | `confirm_cancelled` | 仅在 executor 已确认死亡后调用 |
| 超时 | `fail(GENERATION_TIMEOUT, retryable=true)` | 白名单内，按 A3A-10 退避重试 |
| 崩溃 | `fail(非白名单码)` | 不可重试 → Job 终态失败；错误码走 `is_valid_error_code` 白名单 |
| **放弃（shutdown/任何 stale 后）** | **不提交** | 让租约自然过期，由 `recover_expired` 按 attempt 上限重试；避免非重试 fail 把本可重试的 Job 打成永久失败 |

**D3.1 shutdown 收尾**：收到 SIGTERM/Ctrl-C → 停止 claim → in-flight 排水，上限 = min(剩余租约，`ARTIFACT_WORKER_SHUTDOWN_GRACE_MS`，默认 30s) → 排完按正常路径收尾后退出码 0；排不完 → hard-kill executor → **放弃提交**（依赖租约恢复）→ 退出码 0。 drained 与 abandoned 都必须留结构化日志。

### D4 防孤儿（三层，消解 R2/R3/R5）

1. **进程组/Job Object**：任何终止动作作用于整棵进程树（D1）。
2. **父死看护**：PDEATHSIG / watchdog，supervisor 无论何种方式死亡，executor 秒级自杀（不依赖容器重建）。
3. **attempt 专用临时目录**：路径含 attempt_id，Attempt 结束时 executor 侧自清理；supervisor 在收尾后兜底删除；容器重建由 03B-03 层面兜底。永不跨 Attempt 复用工作目录（A3A-04）。

### D5 迟到写入防护（Worker 侧纪律，与 DB 权威互补）

- **单次终态提交**：每 Attempt 只发一次 `complete`/`fail`/`confirm_cancelled`；发出后无论返回何种结果（含 `stale`、`cancel_requested`），本 Attempt 即封账，绝不二次提交、绝不重试提交。
- **stale 即弃**：任何协调调用返回 `stale`（含 claim 后、heartbeat 前、提交时），立即停止对该 Attempt 的一切 DB 写入与 executor 管理，仅记录日志；若 executor 仍在，按 D2 hard-kill（此时 attempt 已不属于本 worker，必须杀掉以防它继续产生副作用）。
- **heartbeat 与生命周期绑定**：executor 死亡或 Attempt 封账后不再发 heartbeat，杜绝"死人续租"。
- **executor 无 DB**：D1 凭据剥离，迟到写入在拓扑上不可能由执行代码发起。

### D6 PG 唯一权威（重申）

不变更：无 Redis、无 MQ 协调、无 Worker 本地权威状态文件；所有状态判断以受控函数返回值为准，本地仅存 `deadline_at` 等只读快照用于计时。

## 5. 错误码与日志

- 新增稳定错误码（符合 `^[A-Z][A-Z0-9_]{0,63}$`）：`EXECUTOR_CRASHED`（executor 非零退出且无有效结果）、`EXECUTOR_PROTOCOL`（结果文件不符合契约）、`EXECUTOR_KILLED`（被我方 hard-kill 且非超时/取消场景）。
- 日志不记录 lease token、input 正文、DSN（延续 03B-01 纪律）；Attempt 生命周期事件（claim/heartbeat 结果/收尾路径/放弃原因）结构化输出。

## 6. 实施计划（评审通过后执行）

| 阶段 | 内容 | 验收 |
|---|---|---|
| P1 | executor 契约 + spawn/进程组/Job Object/hard-kill 原语（`crates/artifact-infrastructure` 或 worker 内新模块） | 单元测试：进程树全灭、PDEATHSIG/watchdog 生效、凭据剥离断言 |
| P2 | Supervisor 执行循环：claim/heartbeat/监视/超时（恢复 `ARTIFACT_WORKER_LEASE_SECONDS` 等配置） | 空 capabilities 下行为与 03B-01 等价（不领 Job），回归通过 |
| P3 | 收尾状态机 + 单次终态提交纪律 + shutdown 排水 | 状态机表格逐路径测试 |
| P4 | 防孤儿三层落地 + attempt 临时目录 | 孤儿注入测试 |
| P5 | 集成测试（`tests/`，参照 `worker_coordination.rs` 模式，真实 PG） | 见 §7 |
| P6 | 文档收尾（实施报告 + OPEN-ISSUES 更新）+ CI 全绿 | 交付评审 |

**测试要求（保留原门禁）**：迟到 heartbeat/complete/fail 在 Attempt 终态或租约过期后返回 stale 且 DB 状态不被覆盖；取消竞争（complete 与 cancel 同时）由 DB 裁决且 Worker 正确处理两种结果；executor 崩溃 → fail 不可重试路径；supervisor `kill -9` → 租约过期恢复 + executor 不残留；SIGTERM → 停止领取 → in-flight 收尾/放弃 → 退出码 0；多 Worker 并发 claim 不重复；Worker DB role 最小权限回归（沿用 `roles_separation.rs`/`worker_roles.rs`）。

## 7. 待裁项（评审时决定）

| # | 问题 | 推荐 | 备选 |
|---|---|---|---|
| 1 | executor 形态：独立子进程 | ✅ 推荐（可 hard-kill、可凭据剥离） | 进程内 panic 隔离（不可强杀，不满足任务书"hard-kill"） |
| 2 | shutdown 排水上限默认值 | min(剩余租约, 30s) | 仅 30s 固定 |
| 3 | 临时目录兜底清理归属 | executor 自清 + supervisor 兜底（本切片内闭环） | 完全交给 03B-03 容器层 |
| 4 | stub executor 形态 | worker 子命令 `artifact-worker exec`（同一二进制，测试构建注入契约脚本），生产无 adapter 时领不到 Job、路径仍被集成测试覆盖 | 独立 test-only bin |
| 5 | `EXECUTOR_KILLED` 是否可重试 | 不可重试（保守，防止 kill 循环烧 attempt 上限） | 可重试 |

## 8. 红线自检

- [x] 不重新合并 PR #3；不改写 Migration 0001/0002/0003
- [x] 不引入 Redis；不让 RabbitMQ 取代 PG 状态协调
- [x] Worker 不访问 AI-PM 业务数据库
- [x] 不绕过 LMMS delegation 与 File Service finalize 两个生产 P0 门禁
- [x] 设计未经评审前不做大规模代码实施（本文件即评审输入）
