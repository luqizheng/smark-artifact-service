# ART-03B-02 — Worker 执行进程生命周期架构设计 v2

> 状态：**v2 设计基线（待人工评审）**。取代 `ART-03B-02-worker-process-lifecycle-design-v1-draft.md`（该稿已标记 superseded，仅保留历史记录）。
> 前置评审结论：v1 草稿为 **Changes Required / 打回重设计**，主要缺陷：状态机不闭合、数据库终态语义理解错误（deadline_exceeded 后二次提交）、进程树回收保障不足、终态提交不确定性未处理、重复执行副作用边界不清晰。本 v2 逐一重建。
> 数据库事实基线：`origin/main` `c11e79a51a145ded026632630bb0b52aa44dc311`（PR #3 已合并；本文档在 PR #4 分支 `docs/art-03b-02-lifecycle-design-draft` 上演进）。
> 关联文档：
> - `ART-03B-02-state-transition-matrix-v2.md`（状态转换矩阵，本文 §5 的完整展开）
> - `ART-03B-02-failure-scenarios-v2.md`（故障场景分析）
> - `ART-03B-02-test-strategy-v2.md`（测试设计）
> - `ART-03B-02-decision-record-v2.md`（决策记录 + 反向架构审查 + OPEN ISSUES）
> - 契约来源：`ART-03A-job-worker-svg-decisions-v1.md` §11（A3A-09~22）、迁移 `0002/0003`、`deploy/db-roles.sql`

## 0. 结论先行

| 目标 | v2 设计 | 状态 |
|---|---|---|
| 状态机闭合 | Supervisor 侧完整 FSM（§5），每个 DB 返回值都有确定的转移与终止动作，无悬空状态 | 本文档 |
| 数据库终态语义 | 逐函数核验 Migration 0003 实际 SQL（§2）；`deadline_exceeded` = DB 已收尾，只杀不提交 | 本文档 |
| 进程树回收 | Linux：pgid 组杀 + PDEATHSIG（含父 PID 二次检查）+ PID 1 收尸；Windows：Job Object（KILL_ON_JOB_CLOSE + TerminateJobObject + 挂起创建绑定） | §6 |
| 终态提交不确定性 | 提交后不确定状态 + heartbeat 探针协议（§5.4）：`renewed` 证明未落地→单次补交；`stale`→放弃；探针失败→放弃等租约恢复 | §5.4 |
| 重复执行副作用 | Executor 为**纯计算单元**（§4.4）：无网络、无 DB、无任何外部副作用；副作用全部后移到 ART-03D 受控提交 | §4.4 |
| Supervisor 在失去租约前停止 Executor | 租约安全计时器：`last_renew + L − M` 强制终止（§7），不依赖租约自然过期 | §7 |
| 生产 P0 门禁 | LMMS delegation 修复、File Service upload-intent/finalize/checksum/verification 修复仍是生产启用前置门禁；**ART-03B-02 完成 ≠ 生产生成路径开放** | §3.5 |

## 1. 只读审计结论（2026-11-25，基线 c11e79a）

### 1.1 代码现状

- `apps/artifact-worker`：仅协调维护循环（租约恢复 + 输入清理）+ `ping`/`recover-once` 子命令。**无 claim 执行循环、无子进程创建、无 heartbeat 调用方**（全仓除 `artifact-api/build.rs` 外无进程创建代码）。
- `WorkerCoordinationPort`（`crates/artifact-application/src/port/worker.rs`）已定义 claim/heartbeat/complete/fail/confirm_cancelled/recover/purge 七个方法及全部返回值枚举（`HeartbeatOutcome` 四态、`FailOutcome` 六态、`CompleteOutcome`/`ConfirmCancelOutcome` 三态），与 Migration 0003 函数返回值一一对应——**端口层已就绪，本切片是它的第一个真实消费者**。
- `ClaimedJob`（domain）已含 `deadline_at`、一次性 `lease_token`（Debug 已脱敏）。
- Worker SIGTERM 处理：立即退出，无 in-flight 概念（因无执行循环，当前无害）。
- 配置：`ARTIFACT_WORKER_DATABASE_URL`（独立 role）、`worker_id`、维护间隔等；**无 lease/heartbeat 配置**（需本切片新增，见 §7）。

### 1.2 数据库契约核验（逐项，以 Migration 0003 实际 SQL 为准）

以下每一项均直接从 `migrations/0003_job_coordination_fixes.sql` 函数体核验，非引用旧草稿描述：

| # | 事实 | SQL 依据 |
|---|---|---|
| D1 | claim 直接创建 `running` Attempt（无 claimed 状态）；`lease_expires_at = LEAST(now+lease, deadline_at)`，**租约永不超过 deadline** | claim 函数 v_expires 计算 |
| D2 | heartbeat 返回四态；**判定顺序：token/attempt 归属 → job=running → deadline 检查 → 租约过期检查 → 续租 → cancel 检查** | heartbeat 函数体顺序 |
| D3 | **`deadline_exceeded` 不是通知：DB 在返回前已调用 `settle_failed_attempt(timed_out, GENERATION_TIMEOUT, retryable=true)` 完成收尾**——Attempt 已 timed_out，Job 已 retry_wait 或 timed_out | heartbeat deadline 分支 |
| D4 | heartbeat 的 deadline 检查**先于**租约过期检查：持有正确 token 的迟到 heartbeat（租约已过期但 deadline 已到）也会触发 timed_out 收尾而非 stale；**错误 token 一律 stale，绝不触发收尾** | heartbeat 分支顺序 |
| D5 | `cancel_requested` 返回时**租约已续**（续租 UPDATE 先于 cancel 检查执行）——Supervisor 有完整租约窗口执行 kill+confirm | heartbeat 函数体 |
| D6 | complete 判定顺序：token → job=running **且租约未过期** → cancel_requested → 输出登记校验（Version READY + 主 Output passed + sha256 一致，否则 EXCEPTION `OUTPUT_NOT_REGISTERED`）→ succeeded。**租约过期时 complete 返回 stale，即使取消已登记** | complete 函数体 |
| D7 | fail 判定顺序：token → not_found → job=running 且租约未过期（否则 `stale_attempt`）→ cancel_requested → 错误码校验 → `settle_failed_attempt`。**租约过期后 fail 不可用** | fail 函数体 |
| D8 | `confirm_cancelled` **不检查租约过期**（注释明示"取消已登记，Job 的终态不受租约影响"），但检查 attempt=running + token + job=running；返回 cancelled / not_requested / stale | confirm 函数体 |
| D9 | 可重试白名单仅 `GENERATION_TIMEOUT`、`STORAGE_UNAVAILABLE`（`is_retryable_generation_error`）；Worker 自报 `p_retryable` 只能落在白名单内才生效 | 白名单函数 |
| D10 | 恢复扫描对租约过期 Attempt 的归类：`lease_expires_at >= deadline_at` → timed_out/GENERATION_TIMEOUT（Worker 存活到时限）；否则 lease_expired/LEASE_EXPIRED（Worker 失联）。两者经 `settle_failed_attempt` 以 retryable=true 推进，Job 进 retry_wait 或按 attempt 上限终结 | recover 函数体 |
| D11 | `cancel_requested_at` 一经登记**永不清除**（coalesce 语义）；取消 + 租约过期时恢复扫描直接将 Job 置 cancelled | request_cancel / recover |
| D12 | Worker role 仅有 7 个函数的 EXECUTE（claim/heartbeat/complete/fail/confirm_cancelled/recover/purge），业务表零 DML（`db-roles.sql` 显式 REVOKE ALL） | db-roles.sql |
| D13 | 每次迁移后必须重新执行 `db-roles.sql`（函数 DROP/CREATE 丢授权）；迁移校验 `migrations.sha256` 与三个迁移实测一致（基线验证报告已核验） | db-roles.sql 头注 / 基线报告 |

### 1.3 v1 草稿缺陷与本 v2 的对应修正

| v1 缺陷 | v2 修正 |
|---|---|
| "单次终态提交"含糊，网络错误下不知道该重试还是放弃 | §5.4 提交不确定协议：heartbeat 探针区分"未落地可补交"与"放弃" |
| heartbeat `deadline_exceeded` 后仍调 `fail(GENERATION_TIMEOUT)`——**二次终态提交，DB 已收尾，fail 必返回 stale** | §5.3 明确：deadline_exceeded → 只杀进程树，**零 DB 写** |
| cancel 路径未处理 confirm 的 `not_requested`/`stale` 真实返回值 | §5.5 全枚举处理 |
| lease/3 未经计算论证，无 RPC timeout 概念，DB 慢查询可挂过租约期 | §7 lease/4 + 完整时序不等式 + 启动期配置校验 |
| 无人处理"Supervisor 失去提交权但 Executor 还在写输出"的清理责任 | §6.4 目录所有权 + §8 清理责任矩阵 |
| `EXECUTOR_KILLED` 提交路径试图人为制造重试语义 | §9：该码从 DB 提交路径中**移除**，仅存日志 |

## 2. 阶段职责边界（不混淆）

| 阶段 | 职责 | 本设计是否触碰 |
|---|---|---|
| **ART-03B-02（本设计）** | Supervisor / Executor 进程模型、进程创建与进程树终止、IPC 协议、环境变量 allowlist、FD 隔离、Attempt 私有目录、状态机与终态提交纪律、shutdown 排水 | ✅ 全部 |
| ART-03B-03 | Linux 容器/cgroup、网络拒绝、只读根文件系统、资源限额、挂载白名单、secret 限制、**容器生命周期回收（本设计的兜底依赖）** | ❌ 仅定义接口契约（§6.5） |
| ART-03C | SeeCode 0.6.3 adapter、Node vendor runtime、依赖预打包、离线渲染验证、禁止运行时 npm install；**Executor 内部对孙进程的 spawn 纪律由本设计定义为 Executor 契约**（§6.2），03C 实现遵守 | ❌ 仅契约 |
| ART-03D | File Service 上传、upload-intent/finalize/checksum/object verification、Artifact Output/Version READY、PG 事务提交、幂等与补偿；**所有外部副作用的唯一发生地** | ❌ 禁止本切片产生 |

## 3. 架构红线（重申，本设计全部遵守）

### 3.1 PostgreSQL 唯一状态权威
Job/Attempt 状态唯一权威是 PostgreSQL。不引入 Redis、不引入新状态协调库、不以 RabbitMQ/MQ 取代 PG 状态协调、不以 SQL polling 构建跨服务消息总线。Worker 本地文件只有临时执行数据，不是状态权威；本地仅保存 `deadline_at`、`lease_expires_at` 等只读快照用于**计时**，一切状态判定以受控函数返回值为准。

### 3.2 Migration 不可修改
0001/0002/0003 不可变（`migrations.sha256` 锁定）。若未来确认契约缺能力（如只读状态探针，见决策记录 DR-7），以**新的受控 Migration 提案**单独说明必要性/兼容性/权限/审计，本阶段不实施。

### 3.3 Worker DB 权限
Worker 仅经独立 role + 0003 受控 SECURITY DEFINER 函数操作；不 SELECT 业务表、不 UPDATE Job/Attempt、不触碰 Artifact/Version/Output、不绕过受控函数写审计（D12 已核验：现状即满足，设计不放宽）。

### 3.4 服务边界
Worker 不访问 AI-PM 业务库、不持有用户 delegation。**Executor 不持有任何凭据**：DB DSN、File Service 凭据、AI-PM 凭据、长期对象存储凭据一概不进入 Executor 环境（§4.3）。

### 3.5 生产 P0 门禁
LMMS Resource Token delegation 修复 + File Service upload-intent/finalize/checksum/object verification 修复仍是生产启用生成路径的**前置门禁**。本设计不绕过。ART-03B-02 完成**不代表**生产生成路径开放——事实上 0003 契约下 `complete` 在没有 03D 输出登记时必然失败（`OUTPUT_NOT_REGISTERED`），fail-closed 由数据库保证。

## 4. Supervisor 与 Executor 架构

### 4.1 组件职责与生命周期

**Supervisor**（`apps/artifact-worker` 主进程，`run` 子命令）：
- 进程生命周期 = Worker 进程生命周期；常驻。
- 职责：claim 循环、spawn Executor、heartbeat、本地计时（租约安全计时器 + deadline 备份计时）、进程树终止与死亡确认、终态提交（complete/fail/confirm_cancelled 或放弃）、Attempt 私有目录的创建与清理、shutdown 排水、协调维护（延续 03B-01：recover/purge 定时调用）。
- **唯一 DB 连接持有者**；`ARTIFACT_WORKER_DATABASE_URL` 只存在于 Supervisor 进程。
- 同一 Supervisor 串行执行 Attempt（并发度 = 1，v2 范围）；多 Worker 水平扩展，claim 原子性由 DB 保证。

**Executor**（每 Attempt 一个独立子进程）：
- 生命周期 = 单个 Attempt 的计算：启动 → 读 stdin envelope → 纯计算 → 写结果到 Attempt 私有目录 → 写 `result.json` → 退出。
- **每 Attempt 必须独立 Executor，禁止进程内复用多个 Attempt**。论证（决策记录 DR-1 详述）：(a) 凭据剥离只在进程边界可强制（env_clear + allowlist），复用进程意味着执行代码与上一 Attempt 状态共存；(b) hard-kill 粒度 = Attempt，进程复用会把终止语义耦合到无辜 Attempt；(c) 崩溃/泄漏污染不跨 Attempt 传播；(d) 代价（每 Attempt 一次进程创建，~10ms 级）相对生成任务时长（秒~分钟级）可忽略。
- 允许 Executor 创建孙进程（03C 的 Node 子进程树），但必须遵守 §6.2 的 Executor 孙进程纪律。
- Executor 对宿主机的访问限制：本阶段靠环境隔离（env allowlist、无凭据、工作目录限定）；**运行环境强隔离（无网络、只读根、限额）属 03B-03**，本设计只定义接口契约不伪造完成。

### 4.2 进程拓扑

```
                    ┌──────────────────────────────────────────────────┐
                    │   supervisor（artifact-worker run，常驻）           │
                    │   持有 ARTIFACT_WORKER_DATABASE_URL（唯一持有者）     │
                    │   claim → spawn → heartbeat → 状态机 → 终态提交      │
                    └──────┬───────────────────────────────┬────────────┘
                           │ stdin: envelope（一次性）        │ SQL: 受控函数
                           │ stdout/stderr: 日志管道          ▼
                           ▼                        ┌──────────────────┐
        ┌─────────────────────────────┐            │ PostgreSQL        │
        │ executor（Attempt 私有子进程）  │            │ 唯一状态权威        │
        │ pgid=自身 / Win Job Object   │            └──────────────────┘
        │ 无任何凭据、无 DB、无网络        │
        │ 孙进程须同 pgid + PDEATHSIG    │
        └──────────┬──────────────────┘
                   │ 仅写 Attempt 私有目录
                   ▼
        {work_root}/jobs/{job_id}/attempts/{attempt_id}/
                   ├─ result.json（结构化结果）
                   └─ output/*（产物文件，由 03D 读取上传）
```

### 4.3 IPC 协议与凭据隔离

| 通道 | 契约 |
|---|---|
| 输入（Supervisor→Executor） | **stdin 一次性写入** claim 返回的 input envelope JSON（≤256 KiB，DB 已限），写毕关闭 stdin。不经环境变量、不经命令行参数（避免 ps 泄露与长度限制） |
| 结果（Executor→Supervisor） | Attempt 私有目录内 `result.json` + 退出码。`result.json` schema：`{ "status": "succeeded" \| "failed", "error_code"?: string, "diagnostic_summary"?: string(≤2048), "output": [{ "path": string, "sha256": string, "bytes": int }] }` |
| 退出码协议 | `0` = 协议合规结束（须存在合法 result.json，否则按 `EXECUTOR_PROTOCOL` 处理）；非零 = executor 自身异常（`EXECUTOR_CRASHED`）；被信号杀死 = Worker 主动终止（不提交 DB，见 §9） |
| 日志 | stdout/stderr 由 Supervisor 以管道捕获，**脱敏后**按行采集进结构化日志（不透传原始字节；限流防爆盘） |
| 环境变量 | `Command::env_clear()` 后仅注入 allowlist：`ARTIFACT_ATTEMPT_ID`、`ARTIFACT_JOB_ID`、`ARTIFACT_WORK_DIR`（绝对路径）、`ARTIFACT_EXECUTOR_PROTOCOL=1`、以及 03C 声明所需的进程级常量（如 `PATH` 的受控最小值，需在决策记录 DR-3 确认具体清单）。**显式排除**：`ARTIFACT_WORKER_DATABASE_URL`、一切 DSN/token/key/password 形态变量（实现为 deny-by-default + 显式 deny 列表双重断言，进测试） |
| FD 隔离 | 子进程只继承三个 stdio 管道；Rust std 创建的 fd 默认 CLOEXEC；不传 socket、不传 DB 连接（Executor 无 DB 连接可传） |
| 命令形态 | 同一二进制子命令 `artifact-worker exec`（stub 契约实现，测试可注入契约脚本）；无 adapter 能力申报时 Supervisor 不 claim，路径仍被集成测试覆盖 |

### 4.4 Executor 是纯计算执行单元（副作用边界）

**允许**：读取输入 envelope；本地计算；写 Attempt 私有目录；写 result.json；退出。

**禁止**（Executor 内不可达，而非仅约定）：调用 Artifact API；上传 File Service；创建/发布 READY Version；调用 AI-PM；调用任何外部业务 API；执行任何无法幂等或无法补偿的业务副作用。

拓扑性保证：Executor 无凭据（§4.3）→ 网络调用无认证面；**强网络拒绝由 03B-03 容器保证**（本阶段：无凭据 + 代码审查 + 测试断言环境内容）。

**重复执行为何安全**（正面回答任务 §4.5）：
1. Executor 唯一产出是**本地私有目录中的纯数据**，不改变任何外部系统状态；同一输入重算 N 次结果相同（幂等），最多浪费 CPU。
2. **两个 Attempt 不可能同时产生外部副作用**：外部副作用的唯一入口是 ART-03D 受控提交路径，而该路径的前置条件是持有有效租约（complete 的三重校验，D6）。DB 侧 `uq_generation_job_active_attempt` 部分唯一索引保证同一 Job 同时最多一个活跃 Attempt；旧僵尸 Executor 即使算完也无法提交（token 已 stale，D4）。
3. Attempt 私有输出隔离：目录以 attempt_id 命名、UUID 级唯一、永不复用；新 Attempt 使用新目录；旧目录在进程树死亡确认后由 Supervisor 清理（§8）。

## 5. Supervisor 状态机（核心）

完整矩阵（含每个返回值/错误码的逐行展开）见 `ART-03B-02-state-transition-matrix-v2.md`。此处给出机器本体。

### 5.1 状态集

```
Idle ──claim──► Spawning ──spawn ok──► Executing ──┐
                   │                                │
                   └─spawn 失败──► SubmitFail(EXECUTOR_CRASHED)  │ 事件驱动转移见 §5.2
                                                   │
Terminal 提交态（每 Attempt 至多进入一个）:
  SubmitComplete │ SubmitFail │ CancelSequence │ Abandon(零 DB 写)
Terminal 解析态:
  Settled(收到明确返回值并处理完毕) │ Uncertain→探针→(Settled|Abandon)
```

### 5.2 Executing 态的事件转移

| 事件 | 转移 |
|---|---|
| Executor 退出(0) + 合法 result.json(succeeded) | → SubmitComplete(output_sha256) |
| Executor 退出(0) + result.json(failed) | → SubmitFail(error_code, 白名单内才 retryable) |
| Executor 退出(0) + 无/坏 result.json | → SubmitFail(EXECUTOR_PROTOCOL, 不可重试) |
| Executor 退出(非零) / 信号异常 | → SubmitFail(EXECUTOR_CRASHED, 不可重试) |
| heartbeat → `renewed` | 留在 Executing（刷新租约安全计时器） |
| heartbeat → `cancel_requested` | → CancelSequence（租约已续，D5，有完整窗口） |
| heartbeat → `deadline_exceeded` | → **KillOnly + Abandon**（DB 已收尾，D3；零 DB 写） |
| heartbeat → `stale` | → **KillOnly + Abandon**（失去提交权；零 DB 写） |
| 租约安全计时器到点（§7） | → KillOnly + Abandon（保租约恢复的重试语义） |
| 本地 deadline 备份计时到点 | → KillOnly + Abandon（DB deadline 是权威；本地计时只是提前止损，避免 DB 已 timed_out 而 Executor 还在烧 CPU 的窗口扩大） |
| shutdown 排水预算耗尽 | → KillOnly + Abandon |
| heartbeat 调用 Err（网络/DB 故障） | 不立即转移：按 §7 连续失败预算与租约安全计时器裁决（容忍单次抖动） |

**KillOnly = 进程树终止 + 死亡确认（§6），不含任何 DB 写。**

### 5.3 SubmitComplete / SubmitFail 的返回值处理

| 返回值 | 动作 |
|---|---|
| `succeeded` / `retry_wait` / `failed` / `timed_out` | Settled；清理私有目录（§8） |
| `cancel_requested`（complete 或 fail 均可能返回，D6/D7） | → CancelSequence（Executor 已退出，kill 步骤退化为死亡确认） |
| `stale` / `stale_attempt` / `not_found` | Settled（无提交权/已被收尾）；清理私有目录；零额外 DB 写 |
| Err（网络错误/超时，**结果不确定**） | → Uncertain（§5.4），**绝不盲目重试、绝不盲目当 stale 处理** |

### 5.4 Uncertain：终态提交不确定协议

complete/fail 的 Err 可能意味着：事务未提交；已提交但响应丢失；连接中断无法判断。处理协议：

```
Uncertain:
  1. Executor 已在提交前退出（SubmitComplete/SubmitFail 的前置），
     无需 kill；唯一任务是弄清"提交是否落地"。
  2. 探针 = heartbeat（现有受控函数，无需新 migration）：
     ├─ renewed            → Attempt 仍 running 且本方持 token
     │                       ⇒ 先前 complete/fail 必未落地（若落地，attempt 已终态、
     │                         heartbeat 必返回 stale，D2）
     │                       ⇒ 单次补交同一终态请求（幂等语义：第一次从未生效）
     │                       ⇒ 补交再有 Err → Abandon（零 DB 写，等租约恢复）
     ├─ cancel_requested   → 提交未落地且取消已登记 ⇒ CancelSequence
     ├─ deadline_exceeded  → DB 已按超时收尾 ⇒ Abandon（日志记"先前提交未落地"）
     ├─ stale              → 二义：提交已落地 ∨ 租约已失效（两者动作相同）
     │                       ⇒ Abandon（零 DB 写）
     │                       ⇒ 可观测性差异记 OPEN ISSUE（决策记录 DR-7：
     │                         未来受控只读探针 migration 提案）
     └─ Err（探针也不可达）  ⇒ Abandon（零 DB 写，停止探针，等待租约恢复/恢复扫描）
```

**关键认知（写入设计的硬约束）**：heartbeat 只能确认"当前租约与执行权"，**不能证明**先前 complete/fail 是否已提交。本协议的正确性不依赖区分 `stale` 的两种成因——两种成因下 Supervisor 的合规动作完全相同（零 DB 写 + 等待权威收敛）：
- 若提交已落地：DB 终态即所提交的终态，正确。
- 若租约失效：恢复扫描按 LEASE_EXPIRED/GENERATION_TIMEOUT 收尾（D10，均可重试推进），正确。

补交的安全性论证：`renewed` 证明 Attempt 仍 running 且无人收尾 ⇒ 原提交事务必然未生效（同一 attempt 的终态提交在同一事务内改变 status，若已生效则 status ≠ running）⇒ 补交不是二次提交。补交与 cancel 的竞争由 DB 裁决（补交可能返回 `cancel_requested` → 正常入 CancelSequence）。

### 5.5 CancelSequence（取消收尾）

```
CancelSequence:
  1. 终止 Executor 进程树（若仍在运行）：SIGTERM(组) → 宽限 → SIGKILL(组)；
     Windows: TerminateJobObject
  2. 死亡确认（§6.3）：waitpid 直接子进程 + 组内无存活成员
  3. confirm_generation_job_cancelled:
     ├─ cancelled      → Settled；清理私有目录
     ├─ not_requested  → 理论不可达（cancel_requested_at 永不清除，D11）；
     │                    处理：日志告警 + Abandon（不重试 confirm，防循环）→ OPEN ISSUE 复核
     ├─ stale          → 他人（恢复扫描）已收尾 → Settled；清理私有目录
     └─ Err            → Uncertain 的 cancel 版：探针逻辑同 §5.4（stale→Settled；
                          renewed→补交 confirm 一次；Err→Abandon 等恢复扫描按
                          cancel_requested_at 置 cancelled，D11/recover 分支）
```

取消不可能永久挂起的保证：即使 Supervisor 在 CancelSequence 任意一步死亡，恢复扫描会在租约过期后因 `cancel_requested_at IS NOT NULL` 直接将 Job 置 cancelled（recover 分支，D11）——**取消终局有 DB 侧兜底，不依赖 Worker 存活**。

### 5.6 每 Attempt 提交次数上界

不变量：`complete ≤ 2 次（1 原始 + 1 探针确认后的补交）`、`fail ≤ 2 次`、`confirm_cancelled ≤ 2 次`，且第 2 次仅在"探针证明第 1 次未落地"后才允许。任何路径不存在第三次提交。状态机闭合（每个状态都有出边且出边全集枚举于 §5.2–§5.5，无自环死锁）。

## 6. 进程树创建、终止与死亡确认

### 6.1 创建

**Linux**：
- `Command::process_group(0)`：Executor 成为新 process group leader（等价 `setpgid` 由 std 在 spawn 时原子完成，规避 fork/setpgid 竞态）。
- 孙进程自动继承同一 pgid → 组杀覆盖全树。
- Supervisor 若为容器 PID 1：承担孤儿收尸（reap）义务（`waitpid(-1, WNOHANG)` 周期 + 退出前排空），这同时解决僵尸残留的组员检测干扰（§6.3）。

**Windows**：
- `CreateProcess` 以 `CREATE_SUSPENDED` 创建（经 `Command::pre_exec` 不可用，实现于 `std::os::windows::io` + `windows-sys`，或使用 `CommandExt` raw attribute）→ `AssignProcessToJobObject(job, child)` → `ResumeThread`。挂起创建+绑定后恢复，杜绝"已运行但未入 Job"窗口。
- Job Object 配置：`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` + `JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION`；**不设** `SILENT_BREAKAWAY_OK`（孙进程默认留在 Job 内；breakaway 需特权，Executor 无此特权）。
- Supervisor 持有 Job 句柄至 Attempt 收尾。

### 6.2 Executor 孙进程纪律（Executor 契约，03C 实现遵守）

- Executor 创建的每个孙进程：默认同 pgid（Linux，自动）/ 自动入 Job（Windows，自动）。
- **Linux 补充**：Executor 对其直接子进程必须设置 `PR_SET_PDEATHSIG(SIGKILL)`，并执行父 PID 二次检查（`prctl` 后比对 `getppid()`，防 fork–prctl 间隙父亡）。PDEATHSIG 只保护直接子进程**不递归**——孙进程对曾孙进程同理。链式 PDEATHSIG 的履行者是 Executor 自身代码（03C 契约），Supervisor 通过测试验证（测试策略 §进程树测试）。
- 逃逸形态（孙进程 `setsid`/daemonize 脱离 pgid）：Linux 非容器环境下为**已声明的残余风险**；生产以 03B-03 容器/cgroup 生命周期回收兜底（§6.5）。Windows 无此逃逸（Job Object 递归覆盖，除非特权 breakaway）。

### 6.3 终止与死亡确认

**终止序列（统一）**：
```
1. Linux: kill(-pgid, SIGTERM)        Windows: TerminateJobObject(job)
   ├─ Linux 宽限（默认 10s，受租约安全边际动态压缩，§7）后 kill(-pgid, SIGKILL)
   └─ Windows TerminateJobObject 即 SIGKILL 语义，无需宽限（宽限仅对"优雅取消"场景
      可选地先发 CTRL_BREAK/信号；v2 默认直接 Terminate，简化语义）
2. 死亡确认（bounded，默认上限 5s）：
   Linux:  waitpid(child) 成功  AND  组内无存活成员：
           循环 kill(-pgid, 0)：ESRCH ⇒ 组空；EEXIST/0 ⇒ 扫描 /proc/<pgid 成员>，
           全部处于 Z（僵尸）状态 ⇒ 视为死亡（僵尸无执行/写文件能力），
           由 PID 1 语义收尸（§6.1）
   Windows: WaitForSingleObject(child) 成功  AND
           QueryInformationJobObject(BasicAccounting).NumberOfActiveProcesses == 0
3. 超过死亡确认上限仍无法确认 ⇒ 不声称"已安全终止"：
   结构化日志记录（attempt_id, pgid/job handle, 残留成员清单），
   不清理私有目录（防误删他人占用），升级为 OPEN 事项交由 03B-03 容器回收与运维。
   ——不把"大概死了"当"确认死亡"，这是 hard gate。
```

### 6.4 Supervisor 崩溃（SIGKILL / OOM）下的回收

- Linux：Executor 经 PDEATHSIG 秒级自杀（含父 PID 二次检查）；孙进程经 Executor 链式 PDEATHSIG 自杀；若链断裂（孙进程未设）→ 残留至容器/cgroup 回收（03B-03 契约：**per-Worker 容器销毁 = cgroup 内全部进程被内核回收**）。租约由恢复扫描按 D10 处理。
- Windows：Job 句柄随 Supervisor 进程死亡被内核关闭 → `KILL_ON_JOB_CLOSE` 递归杀全树（含孙进程，无需配合）。**这是 Windows 路径的核心优势：回收保证由内核而非协作纪律提供。**
- 两种平台都**不以 watchdog 轮询作为唯一清理保证**；watchdog（Supervisor 内周期性扫描自身 pgid/Job 状态）仅作为可观测性辅助。

### 6.5 03B-03 接口契约（本设计对 03B-03 的要求）

1. 每个 Worker 独立容器；容器终止（任意原因）必须保证 cgroup/namespace 内全部进程死亡（内核语义，非协作）。
2. Attempt 私有目录在容器文件系统生命周期内（tmpfs / 卷挂载点）；容器销毁 ⇒ 目录消失。开发环境持久目录的兜底清理见 §8。
3. 网络默认拒绝（仅允许的出口白名单）；只读根；Secret 仅注入 Supervisor 环境，不透传 Executor。

## 7. Lease 与 Heartbeat 时序设计

### 7.1 参数与符号

| 符号 | 含义 | 默认（候选） |
|---|---|---|
| L | claim 的 lease_seconds | 60（范围 1..3600，DB 约束） |
| H | heartbeat 间隔 | **L/4**（=15s） |
| T | 单次协调调用 RPC 超时（statement_timeout + connect 超时的总预算） | 10s（且恒 < H） |
| K | 连续 heartbeat 失败容忍次数 | 2（第 K 次失败即触发 KillOnly+Abandon） |
| M | 终止安全边际（从发起 SIGTERM 到进程树确认死亡的最坏耗时预算） | 12s（= 宽限 10s + SIGKILL/收尾 2s） |
| G | SIGTERM 宽限 | 10s（动态压缩：≤ 剩余安全时间） |

### 7.2 核心不变式（必须全部成立）

**INV-1（先停后失约）**：Supervisor 发起进程树终止的时刻 ≤ `last_renew_local + L − M`。`last_renew_local` 取最近一次 heartbeat 成功响应的**本地接收时刻**（响应到达晚于 DB 写租约时刻，故以本地接收时刻为基准是保守方向——实际到期不早于该基准 + L − 误差，误差为单向网络延迟，并入 M）。

**INV-2（失败检测预算）**：K 次连续失败的检测必须在 INV-1 的余量内完成：`K·H + T ≤ L − M`。代入默认：2×15 + 10 = 40 ≤ 60 − 12 = 48 ✅（余量 8s 吸收调度延迟/时钟频率级漂移；本地用单调钟测 elapsed，不受墙钟跳变影响）。

**INV-3（lease/4 论证，非既定结论）**：
- lease/3（H=20）代入 INV-2：2×20+10 = 50 > 48 ❌——两连败检测贴着租约边界，无抖动余量；DB 慢查询一次（T 用满）即可能先失约。
- lease/4（H=15）：余量 8s；且允许 T=10s 的真实 RPC 超时（lease/3 要满足不等式需 T≤8，超时过短会在 DB 正常慢时误杀）。
- lease/2（H=30）：一次失败即耗尽预算，K 只能为 1，单次网络抖动就杀 Executor，不可取。
- **结论：默认 L=60、H=L/4、T=10、K=2、M=12。** lease/4 是从 INV-2 反推的结果，不是教条。

**INV-4（配置合法性，启动期 fail-fast 校验）**：`K·(L/4) + T + M ≤ L` 必须在启动时断言，否则拒绝启动（写错的配置必须被看见，延续 config.rs 纪律）。由此得 **L 的下界**：解 `2·L/4 + 10 + 12 ≤ L` ⇒ `L ≥ 44` ⇒ 配置校验 `L ≥ 45`（默认 60）。小租约场景应等比缩小 T/M（另行配置），不由系统静默接受。

**INV-5（deadline 与租约的耦合）**：DB 保证 `lease_expires ≤ deadline`（D1），且 heartbeat 到期检查先于租约检查（D4）。因此：
- 本地 deadline 备份计时 = `claim 本地时刻 + (deadline_at − started_at)`（用 DB 下发的两个时刻之差，消除墙钟绝对偏差）；
- 安全触发时刻 = min(INV-1 时刻, 本地 deadline 备份时刻)——**取更早者**；
- 权威裁决始终是 DB 的 `deadline_exceeded`；本地计时仅提前止损（缩短"DB 已 timed_out 而 Executor 仍在跑"的窗口，该窗口本身无害——Executor 纯计算——只是资源浪费）。

**INV-6（排水期间租约不流失）**：shutdown 排水期间 heartbeat 照常执行（§10），且排水预算 ≤ INV-1 余量。

### 7.3 时序图（关键窗口）

```
claim(t0): lease→t0+L, deadline≥lease
heartbeat(t0+H): renewed → lease→t0+H+L(clamped≤deadline)，刷新 last_renew_local
  ├ DB 慢查询（最坏 T=10s）：响应最迟 t0+H+T 到达，仍在余量内
  ├ 第1次失败(t0+H+T)：不动作，继续
  └ 第2次失败(t0+2H+T ≤ t0+40+10=50)：≤ INV-1 界(t0+48) —— 若贴界则由 INV-1
      计时器更早触发，两者取先
INV-1 界(t0+L−M = t0+48)：无条件 KillOnly+Abandon（无论 heartbeat 是否成功）
  ——即使所有 heartbeat 都失败，Executor 也死于租约失效之前（INV-1 不依赖任何
    heartbeat 成功，仅依赖本地单调钟）
```

**"不能依赖租约自然过期来保证 Executor 已停止"的落实**：INV-1 是本地主动计时器，最坏情况（heartbeat 全失败、DB 完全不可达）下依然在 t0+48 强杀，此时租约到 t0+60 才失效。DB 侧即使在这 12s 内发生任何收尾（不可能——租约未失效且无人触发），Supervisor 的 Abandon 路径也是零 DB 写，不与权威冲突。

## 8. Attempt 私有目录与清理责任

| 项 | 契约 |
|---|---|
| 布局 | `{work_root}/jobs/{job_id}/attempts/{attempt_id}/`，三层 UUID；`work_root` 来自配置（容器内 tmpfs；开发环境持久目录） |
| 唯一性 | attempt_id 为 UUID，目录**永不跨 Attempt 复用**；创建即独占（`create_dir` 非复用语义 + 0700 权限） |
| 所有权 | Supervisor 创建/清理；Executor 只写不删（Executor 无清理责任——崩溃/被杀场景下清理仍可预测） |
| 清理时机 | **进程树死亡确认（§6.3）之后**才允许清理；Settled 后立即清理；Abandon 后（树死确认后）清理；死亡确认失败 → 不清理（§6.3 hard gate） |
| 误删防护 | Supervisor 仅操作"严格匹配三层 UUID 路径模式"的目录；路径任一段非 UUID 或存在 symlink 成分 → 拒绝操作并告警。**不存在**删除其他 Attempt 目录的路径（每 Attempt 只持有自己的路径字面量） |
| Supervisor 崩溃后 | 生产：容器 tmpfs 随容器销毁回收（03B-03 契约）。开发持久目录：启动期扫描 `jobs/{uuid}/attempts/{uuid}` 模式目录，仅删除**修改时间早于 7 天**（与输入保留期对齐）者；不做任何 DB 交叉验证（Worker 无业务表 SELECT 权限，D12），因此时间阈值为唯一防线——7 天远大于 lease+retry 生命期，活跃 Attempt 目录不可能被误删 |
| 磁盘不足 | Executor 写入失败自陷（result.json failed / 非零退出）→ 正常 SubmitFail；Supervisor 创建目录失败 → 直接 SubmitFail(EXECUTOR_CRASHED)（不 spawn）；磁盘不足码不在 DB 白名单，不可重试——**这是数据面故障，重试大概率同样失败，符合白名单保守语义**；运维指标上报（详见故障场景文档） |

## 9. 错误码与提交策略

### 9.1 码表（均符合 `^[A-Z][A-Z0-9_]{0,63}$`）

| 码 | 提交场景 | DB 可重试（白名单事实，非本设计决定） | 备注 |
|---|---|---|---|
| EXECUTOR_CRASHED | Executor 非零退出 / spawn 失败 / 磁盘不足等数据面故障 | ❌（不在白名单） | |
| EXECUTOR_PROTOCOL | 退出码 0 但 result.json 缺失/非法 | ❌ | |
| GENERATION_TIMEOUT | **仅一种提交场景**：主动提前终止路径中（本地 deadline 备份计时触发、Executor 尚在运行、**租约仍有效**）提交 fail(GENERATION_TIMEOUT, retryable=true) | ✅ 白名单 | 若此时租约已过期，fail 返回 stale_attempt（D7）→ 转入 Abandon，由恢复扫描处理——不视为错误 |
| EXECUTOR_KILLED | **不提交 DB**。v2 移除该码的一切 DB 提交路径：所有 kill 场景的归宿是 cancel(confirm) / deadline(零写) / 租约安全(零写) / 排水超时(零写)，无一处需要 fail(EXECUTOR_KILLED)。保留为**日志级**码标识 Worker 主动终止事件 | — | 决策记录 DR-6，需人工确认 |

### 9.2 六类终止来源的区分（正面回答任务 §七）

| 来源 | 表达 | DB 终态语义 |
|---|---|---|
| Executor 自身异常 | fail(EXECUTOR_CRASHED / EXECUTOR_PROTOCOL, retryable=false) | failed |
| Worker 主动终止（租约安全/排水超时/不确定性放弃） | **零提交**，日志 EXECUTOR_KILLED | 恢复扫描按 LEASE_EXPIRED（retryable=true 经 D10）推进 → retry_wait（保住重试语义） |
| Job 取消 | confirm_cancelled | cancelled |
| deadline 超时（DB 裁决） | heartbeat deadline_exceeded → 零提交 | DB 已收尾（timed_out + retry_wait/timed_out，D3） |
| deadline 超时（Worker 主动提前） | fail(GENERATION_TIMEOUT, retryable=true)（§9.1） | retry_wait / timed_out |
| 租约失效 | 零提交（一切调用自然返回 stale/stale_attempt） | 恢复扫描收尾 |

**明确禁止**：以 `p_retryable=true` 绕过白名单（DB 侧已免疫，D9，但客户端也不构造此类调用）；以 fail(EXECUTOR_KILLED) 制造重试（§9.1 已移除该路径）。

## 10. Shutdown 与排水

```
SIGTERM / Ctrl-C:
1. 停止 claim（立即）
2. 维护循环停止；recover/purge 不再发起
3. in-flight Attempt 排水:
   预算 = min(排水上限默认 30s, INV-1 剩余安全时间, 本地 deadline 备份剩余)
   期间: heartbeat 照常（INV-6）; 不再等待新的 executor 结果?
   └─ 不——继续等待当前 executor 退出:
      ├─ 预算内退出+合法结果 → 正常提交路径（§5.3，含 Uncertain 协议）
      └─ 预算耗尽 → KillOnly（SIGTERM 宽限同样受预算压缩）
                    → 死亡确认 → Abandon（零 DB 写，依赖租约恢复）
   排水预算 < 宽限 G 时: 宽限压缩为预算内（G 动态化）
4. 退出码恒 0（drained 与 abandoned 均为合规收尾）；drained/abandoned
   均留结构化日志（attempt_id, 路径, 原因, 耗时）
```

不"只等租约过期"：排水是**主动**的（预算内完整提交优先，预算外主动终止），租约恢复只是 Abandon 的兜底，不是排水策略本身。

## 11. 风险与约束（设计内已识别）

| # | 风险 | 处置 |
|---|---|---|
| V1 | Linux 孙进程 setsid 逃逸 pgid（非容器环境） | 已声明残余风险；生产 03B-03 cgroup 兜底；测试覆盖可检测形态 |
| V2 | 开发环境 Windows 下 Worker 自身处于外层 Job Object | Win8+ 嵌套 Job 支持；测试验证（测试策略 §平台差异） |
| V3 | 容器 PID 1 语义依赖部署形态（Supervisor 是否 PID 1） | 03B-03 契约显式声明；非 PID 1 时由容器 init 收尸，组员检测以 /proc 状态为准（不受影响） |
| V4 | heartbeat 探针无法区分 stale 的两种成因 | 正确性不受影响（§5.4）；可观测性差异 → OPEN ISSUE / DR-7 |
| V5 | 7 天目录清理阈值 vs 超长 retry 链 | 阈值 ≥ 最大退避链生命期（max_attempts≤10 × 300s 上限 + 24h 排队 + 7d 输入保留），留足余量 |
| V6 | 单 Supervisor 串行 Attempt（并发 1） | v2 范围决策；吞吐不足时后续切片按同状态机扩展多 slot（决策记录 DR-2） |

## 12. 实施前置与验收口径

- 本设计经人工评审通过后，才进入实施（PR #4 仅含文档）。
- 实施验收 = 测试策略 v2 全部用例通过 + 状态转换矩阵逐行覆盖 + 故障场景文档中的全部注入场景闭环。
- **不把"文档写完"等同于"设计通过"**；评审待决项见决策记录（DR-3 环境白名单、DR-6 EXECUTOR_KILLED 移除、DR-7 只读探针提案等）。
