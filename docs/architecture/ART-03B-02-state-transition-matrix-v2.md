# ART-03B-02 — 状态转换矩阵 v2

> 状态：**v2 设计基线（待人工评审）**。是 `ART-03B-02-worker-process-lifecycle-design-v2.md` §5 的完整展开。
> 事实来源：`migrations/0003_job_coordination_fixes.sql` 逐函数核验（主设计文档 §1.2 的 D1~D13 为索引）。所有返回值均为 0003 实际 SQL 返回值，无一处引自旧草稿或记忆。
> 约定：**「零 DB 写」= 该行之后对本 Attempt 不再调用任何受控函数。**「提交上界」指该函数在整个 Attempt 生命周期内的最大合法调用次数（含 §5.4 探针协议的补交）。

## 1. 数据库侧状态空间（权威，只读引用）

**Job.status**（0001 白名单 + 0003 迁移）：`queued → running → {retry_wait → queued …} | succeeded | failed | cancelled | timed_out`。终态不可逆（无函数离开终态）。

**Attempt.status**（0002 trigger 守卫）：`claimed → running → {succeeded | failed | cancelled | timed_out | lease_expired}`；`running` 是唯一可提交态。0003 的 claim 直接创建 running（claimed 无函数产生）。

**Job ↔ Attempt 收尾耦合**（settle_failed_attempt）：attempt `timed_out/lease_expired` ⇒ Job `timed_out`；attempt `failed` ⇒ Job `failed`；attempt `cancelled` ⇒ Job `cancelled`（confirm/recover 路径）；attempt `succeeded` ⇒ Job `succeeded`。

## 2. 协调函数返回值总表（核验基准）

| 函数 | 返回值全集 | 判定顺序（SQL 事实） | 调用方提交上界 |
|---|---|---|---|
| claim | 行集 或 空 | 入参校验 → 窗口 → 加锁复核 → 建 running Attempt | 1/轮询 |
| heartbeat | `renewed` / `cancel_requested` / `deadline_exceeded` / `stale` | token 归属 → job=running → **deadline（触发 DB 收尾）** → 租约未过期 → 续租 → cancel | 生命周期内多次（间隔 L/4） |
| complete | `succeeded` / `cancel_requested` / `stale`；EXCEPTION `OUTPUT_NOT_REGISTERED` / `INVALID_OUTPUT_SHA256` | token → job=running **且租约未过期** → cancel → 输出登记校验 → 成功 | **2**（原始 + 探针确认未落地后的补交） |
| fail | `retry_wait` / `failed` / `timed_out` / `cancel_requested` / `stale_attempt` / `not_found`；EXCEPTION `INVALID_ERROR_CODE` | token → 存在 → job=running **且租约未过期** → cancel → 码校验 → settle | **2**（同上） |
| confirm_cancelled | `cancelled` / `not_requested` / `stale` | token → attempt=running → job=running → cancel_requested（**不查租约**，D8） | **2**（同上） |
| recover_expired | integer | advisory lock；队列过期 → cancelled/QUEUE_EXPIRED；租约过期 Attempt → 归类收尾 | 幂等，任意 |
| purge_expired_input | integer | 终态 + input_expires_at 到期 | 幂等，任意 |

## 3. Supervisor 状态机转换矩阵（核心交付）

列说明：**Executor 动作**＝对进程树的处置；**后续 DB 操作**＝对该 Attempt 的下一次受控函数调用；**重试**＝Job 层面是否获得重试（白名单事实）；**清理**＝私有目录责任。行内「—」表示无动作。

### 3.1 Executing 态事件行

| # | 触发 | DB 状态（触发时刻） | Executor 动作 | 后续 DB 操作 | 提交上界影响 |
|---|---|---|---|---|---|
| E1 | heartbeat `renewed` | running/running | — | 下一次 heartbeat（L/4 后） | — |
| E2 | heartbeat `cancel_requested` | running/running，**租约已续**（D5） | 终止序列（SIGTERM 组 → 动态宽限 → SIGKILL 组）→ 死亡确认 | confirm_cancelled（→ C 行） | confirm ≤2 |
| E3 | heartbeat `deadline_exceeded` | **已终态**：attempt timed_out，job retry_wait 或 timed_out（D3） | KillOnly（直接 SIGKILL 组，**无宽限**——DB 已裁决，越快越好）→ 死亡确认 | **零 DB 写**（fail 必返回 stale/stale_attempt，D7；调用即违规） | 消耗 0 |
| E4 | heartbeat `stale` | 失去提交权（attempt 可能已被他人终态或租约过期未收尾） | KillOnly（SIGKILL 组）→ 死亡确认 | **零 DB 写**；杀进程是防止无权 Executor 继续烧资源与写盘 | 消耗 0 |
| E5 | Executor 退出(0) + result(succeeded) | running/running | —（已死） | complete(output_sha256)（→ T 行） | complete ≤2 |
| E6 | Executor 退出(0) + result(failed) | running/running | — | fail(result 的 error_code, result 的 retryable 落白名单)（→ T 行） | fail ≤2 |
| E7 | Executor 退出(0) + 无/坏 result.json | running/running | — | fail(EXECUTOR_PROTOCOL, false) | fail ≤2 |
| E8 | Executor 退出(非零)/spawn 失败 | running/running | — | fail(EXECUTOR_CRASHED, false) | fail ≤2 |
| E9 | 租约安全计时器（INV-1） | 租约**尚未过期**（提前 M 发起） | 终止序列 → 死亡确认 | **零 DB 写** → 租约过期后由恢复扫描以 LEASE_EXPIRED（可重试，D10）推进 | 消耗 0；Job 保重试 |
| E10 | 本地 deadline 备份计时（INV-5） | 租约可能仍有效（lease ≤ deadline，临界点） | 终止序列 → 死亡确认 | 二选一（实现固定为后者，见下）：a) 若确认租约有效且 heartbeat 可达 → fail(GENERATION_TIMEOUT, true)；**b) 零 DB 写交恢复扫描**（lease≈deadline ⇒ 恢复扫描归类 timed_out/GENERATION_TIMEOUT，同语义，D10） | b) 消耗 0。选 b 的理由：临界点租约余量为零，fail 有 stale_attempt 风险（D7），而恢复扫描的归类与主动提交**完全等价**（同码同重试语义），无需冒不确定提交 |
| E11 | shutdown 排水预算耗尽 | running/running | KillOnly → 死亡确认 | **零 DB 写**（同 E9 语义） | 消耗 0 |
| E12 | heartbeat 调用 Err（网络/DB） | 未知 | — | 不转移；计入连续失败计数（K=2）与 INV-1 计时器共同裁决 | — |

### 3.2 终态提交返回值行（T 行）

| # | 调用 | 返回值 | DB 终局 | Executor 动作 | 后续 DB 操作 | Job 重试 |
|---|---|---|---|---|---|---|
| T1 | complete | `succeeded` | succeeded/succeeded | — | 零（Settled）；清理目录 | — |
| T2 | complete | `cancel_requested` | running + cancel 登记（提交未生效） | 已死；死亡确认（幂等重查） | confirm_cancelled（→ C 行） | — |
| T3 | complete/fail | `stale` / `stale_attempt` | 他人已收尾或租约失效 | — | 零（Settled）；清理目录 | 视 DB 实际终局 |
| T4 | fail | `not_found` | attempt 不存在（异常态） | — | 零（Settled + 告警日志） | 视 DB 实际终局 |
| T5 | fail | `retry_wait` | attempt failed/timed_out + job retry_wait | — | 零（Settled）；清理目录 | ✅（退避后重领，新 Attempt） |
| T6 | fail | `failed` | failed/failed | — | 零（Settled）；清理目录 | ❌ |
| T7 | fail | `timed_out` | attempt timed_out + job timed_out 或 retry_wait | — | 零（Settled）；清理目录 | 按返回值 |
| T8 | complete EXCEPTION `OUTPUT_NOT_REGISTERED` | 事务回滚，attempt 仍 running | — | **映射为 Err(uncertain)**？否——EXCEPTION 是**确定性失败**（响应已收到）：此为调用方协议错误（03D 未先行登记）。处置：fail(EXECUTOR_CRASHED? 否——用 EXECUTOR_PROTOCOL, false) 一次；若再 Err → Uncertain 协议 | ❌ |
| T9 | complete/fail Err（网络） | **不确定** | —（若 Err 前 Executor 已死） | **Uncertain 协议（→ U 行）** | 见 U 行 | — |

### 3.3 Uncertain 探针行（U 行）

前置：终态提交（complete/fail/confirm）返回 Err；Executor 已退出。探针 = heartbeat（唯一现有合规探针）。

| # | 探针返回 | 语义推断（严格依据 D2 顺序） | 动作 |
|---|---|---|---|
| U1 | `renewed` | attempt 仍 running + 本方 token ⇒ **原提交必未落地** | 单次补交原终态请求（complete 或 fail，同参数）；补交返回值走 T 行重处理；补交再 Err → U6 |
| U2 | `cancel_requested` | 同 U1（提交未落地）+ 取消登记 | CancelSequence（→ C 行） |
| U3 | `deadline_exceeded` | DB 已按超时收尾（原提交未落地——若落地则 stale） | 零 DB 写；Settled；日志记"提交未落地，DB 以超时收尾" |
| U4 | `stale` | 二义：提交已落地 ∨ 租约失效。**两者动作相同** | 零 DB 写；Settled；清理目录；成因区分 → OPEN ISSUE / DR-7 |
| U5 | 探针 Err | DB 不可达 | 放弃探针 → U6 |
| U6 | —（放弃/耗尽） | 无法确认 | 零 DB 写；Settled(abandoned)；**等待租约过期 + 恢复扫描**（若原提交落地则 DB 已终态=目标态；否则 LEASE_EXPIRED 可重试推进。两个世界都正确收敛） |

**协议正确性公理**：heartbeat 返回 `renewed/cancel_requested/deadline_exceeded` 任一，都蕴含"attempt 仍 running"，而终态提交成功必然使 attempt 离开 running（同事务）⇒ 这三个返回值都是"原提交未落地"的**充分证据**。`stale` 无法区分（attempt 已终态 ⇒ 可能提交落地；或 token/job 不匹配 ⇒ 租约失效）⇒ 只能放弃，且放弃在两个成因下都是正确动作。

### 3.4 取消确认行（C 行）

前置：CancelSequence 第 1、2 步完成（进程树终止 + **死亡确认**——confirm 只允许在确认死亡后调用）。

| # | confirm 返回 | DB 终局 | 后续 | 备注 |
|---|---|---|---|---|
| C1 | `cancelled` | cancelled/cancelled | 零；Settled；清理目录 | 正常路径 |
| C2 | `stale` | 他人（恢复扫描按 D11 cancel 分支）已收尾 | 零；Settled；清理目录 | 幂等收敛 |
| C3 | `not_requested` | 理论不可达（cancel_requested_at 永不清除，D11） | 零 DB 写 + **告警日志**（不可达分支被触达=契约假设被打破，需要人工看） | 不重试 confirm（防循环）；OPEN ISSUE-2 复核 |
| C4 | Err | 不确定 | Uncertain 的 cancel 版：探针 heartbeat；`renewed/cancel_requested` ⇒ 补交 confirm 一次；`stale` ⇒ C2 语义；`deadline_exceeded` ⇒ U3 语义；探针 Err ⇒ 零写放弃（恢复扫描 cancel 分支兜底，D11——**取消终局不依赖 Worker 存活**） | confirm ≤2 |

### 3.5 状态机闭合性证明（要点）

1. **状态出边完备**：Supervisor 侧状态 = {Idle, Spawning, Executing, SubmitComplete, SubmitFail, CancelSequence, Uncertain, Settled, Abandoned}。每个状态的转移全集 = §3.1~§3.4 的行集；无状态缺少出边；Settled/Abandoned 为吸收态。
2. **无死循环**：所有"重试"类动作（补交）有硬上界 2；探针失败链最长 2（原提交 + 探针 + 补交 + 放弃）；confirm 无重试循环（C3 直接放弃）。
3. **DB 写封闭**：每 Attempt 受控函数调用总次数 ≤ heartbeat(正常节拍) + 终态提交 2 + 探针 1；一切"零 DB 写"行的存在使最坏路径（E4/E9/E11/U6）的写次数为 0。
4. **与 DB 权威不冲突**：所有行的前置判断只依赖返回值（DB 裁决结果），不存在"本地认为 X 而 DB 认为 Y 仍写入 Y"的路径。

## 4. 资源清理责任矩阵

| 资源 | 创建者 | Executor 死亡前 | 死亡确认后 | Supervisor 崩溃后 |
|---|---|---|---|---|
| Executor 进程树 | Supervisor（spawn） | — | 终止 + 确认（§6.3） | Linux PDEATHSIG 链 / Windows KILL_ON_JOB_CLOSE / 03B-03 cgroup |
| Attempt 私有目录 | Supervisor | **不清理**（Executor 可能还在写；Executor 也不清理） | Settled/Abandoned ⇒ Supervisor 删除（严格路径模式校验） | 生产容器 tmpfs 回收；开发 7 天阈值启动清理 |
| lease token 明文 | DB（claim 返回一次） | 仅 Supervisor 内存；日志/落盘零出现（domain Debug 已脱敏） | Attempt 结束即丢弃 | 随进程消失 |
| 环境凭据 | 部署 Secret → Supervisor | Executor 环境零凭据（env_clear + allowlist，测试断言） | — | 随进程消失 |
| Job/Attempt 状态 | PostgreSQL | — | — | 恢复扫描（唯一兜底，D10/D11） |

## 5. 「是否允许重试」判定表（DB 白名单事实 + Supervisor 行为）

| 场景 | Job 获得重试？ | 机制 | 依据 |
|---|---|---|---|
| fail(GENERATION_TIMEOUT, true) 且 attempts 未耗尽 | ✅ | settle retry_wait | D9 白名单 |
| fail(其它任何码, 任意 retryable 自报) | ❌ | 白名单外自报无效 | D9 |
| E3 deadline_exceeded | ✅（attempts 未耗尽时） | DB settle 已带 retryable=true | D3 |
| E4/E9/E11/U6 零写放弃 | ✅（attempts 未耗尽时） | 恢复扫描 LEASE_EXPIRED/GENERATION_TIMEOUT，retryable=true | D10 |
| E7/E8 EXECUTOR_CRASHED/PROTOCOL | ❌ | 不在白名单 | D9 |
| C* 取消终局 | ❌（cancelled 是终态） | — | 0001 状态机 |
| heartbeat 崩溃前正常完成但 Executor 死亡后未及提交，租约过期 | ✅ | 恢复扫描 | D10 |

**该表证明**：Supervisor 的"零 DB 写放弃"纪律不是丢失重试机会，而是**把重试语义完整转移给恢复扫描**（其归类码 LEASE_EXPIRED/GENERATION_TIMEOUT 恰好全部在可重试路径上）。这是 v2 相对 v1 的关键语义修正：v1 把"放弃"理解为损失，v2 论证其为等价收敛。

## 6. 竞争窗口登记表（谁赢、怎么收敛）

| 窗口 | 参与方 | DB 裁决 | Supervisor 合规动作 |
|---|---|---|---|
| W1 complete vs 取消登记 | API cancel 与 Worker complete 并发 | complete 内 cancel 检查（D6 顺序）——**租约有效时** cancel_requested 先判 | T2 → CancelSequence |
| W2 complete vs 租约过期 | lease 到期瞬间 complete | D6：lease 过期 ⇒ stale | T3（零写） |
| W3 deadline 收尾 vs Executor 运行 | heartbeat 到点 vs Executor | D3：DB 已终态 | E3（KillOnly，零写） |
| W4 retry_wait 重领 vs 旧 Executor 残留 | 新 Worker claim vs 旧 Executor | 新 Attempt（active 唯一索引） | 旧侧：E4 stale → KillOnly；新 Attempt 与旧 Executor 无共享状态（目录/token/进程均隔离） |
| W5 恢复扫描 vs 迟到 confirm | recover 与 Worker confirm 并发 | 行锁串行化；先到者收尾，后到者 stale/`v_job.status<>'running'` ⇒ stale | C2 |
| W6 恢复扫描 vs 迟到 heartbeat（deadline 已过） | D4：持正确 token 的迟到 heartbeat 触发 timed_out 收尾（而非 stale）；错误 token 一律 stale | 不影响正确性（与恢复扫描同归类），矩阵 E3 已覆盖 | E3 |
| W7 补交 vs 取消登记 | U1 补交与新 cancel | complete 的 cancel 检查 | T2 |
