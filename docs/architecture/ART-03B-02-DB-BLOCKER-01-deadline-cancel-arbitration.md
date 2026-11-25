# ART-03B-02 — DB-BLOCKER-01：Deadline × Cancel 数据库竞争裁决设计

> 状态：**Design proposal / 待独立评审；未批准实施**。
> 范围：只解决 Migration 0003 中 deadline 与 cancel 请求竞争导致的 retry_wait 不可领取问题。
> 本文不修改 Migration 0001/0002/0003，不提供可直接执行的 SQL，不代表批准创建 0004。
> 归属：ART-03B-02 架构收敛工作流第 1 步。

## 1. 问题与事实基线

Migration 0003 的 `heartbeat_generation_job` 在锁定 Attempt 和 Job 后，先检查 deadline，再检查 lease，再更新 heartbeat/lease，最后检查 `cancel_requested_at`。deadline 到达时调用：

```sql
settle_failed_attempt(
  v_job, p_attempt_id, 'timed_out', 'GENERATION_TIMEOUT',
  ..., true, ...
)
```

`settle_failed_attempt` 在 retryable、attempt 未耗尽且未越过 expires_at 时可将 Job 改为 `retry_wait`，但不清除 `cancel_requested_at`。

Migration 0003 的 `claim_generation_job` 对候选 Job 和最终锁定复核均要求：

```sql
cancel_requested_at IS NULL
```

因此，deadline 分支可能产生 `status='retry_wait' AND cancel_requested_at IS NOT NULL`。该 Job 不再可被 claim，只能等待 queue expiry 恢复路径将其收尾为 QUEUE_EXPIRED。

这不是 Worker 调整轮询周期、增加本地状态或补写 DB 能安全修复的问题；它是 DB 状态转换契约缺陷。

## 2. 推荐裁决语义：已登记取消优先

**推荐：若 heartbeat/恢复事务观察到已持久化的 `cancel_requested_at`，取消请求优先于 deadline 超时重试。**

理由：

1. cancel 是明确的用户/业务终止意图；一旦持久化，不应再安排新的执行 Attempt。
2. 取消优先可直接消除 `retry_wait + cancel_requested_at` 的不可领取组合。
3. 不需要清除取消标记，也不需要把取消请求重新解释为可重试失败。
4. 竞争由同一 Job 行锁与 PostgreSQL 事务提交顺序裁决，不依赖 Worker/DB 主机时钟比较或 RPC 到达时间猜测。
5. 与既有语义一致：`request_generation_job_cancel` 对 queued/retry_wait 直接取消；running 仅登记请求，等待 Worker 确认停止或恢复流程收尾。

该规则的精确定义是“谁先成功提交相应的 Job 行锁事务”，而不是比较客户端发起时间或网络到达时间。

## 3. 竞争裁决矩阵

| 事务提交顺序 | DB 观察到的状态 | 期望结果 | 是否允许重试 |
|---|---|---|---|
| cancel request 先提交；heartbeat 随后执行 | running + cancel_requested_at 非 NULL | heartbeat 返回 cancel_requested，不执行 deadline retry settlement；Worker KillOnly，确认死亡后调用 confirm | 否 |
| heartbeat deadline settlement 先提交；cancel request 随后执行 | retry_wait + cancel_requested_at NULL | cancel request 将 retry_wait 直接改为 cancelled | 否 |
| heartbeat deadline settlement 先提交；恢复扫描随后执行 | retry_wait + cancel_requested_at NULL | 正常 retry_wait，满足 next_attempt_at 后可 claim | 是 |
| cancel request 与 deadline heartbeat 并发 | 两者争用 Job 行锁 | 按实际锁获得/事务提交顺序归入以上两行之一 | 由先提交的事务决定 |
| cancel request 已提交；Worker 在死亡确认前失联 | running + cancel_requested_at 非 NULL | lease 到期后 recovery 将 Job cancelled；Attempt 记录 lease_expired（按审计语义记录 Worker 未确认） | 否 |
| cancel request 发生在 deadline settlement 已将 Job 置为 timed_out/failed 后 | terminal Job | cancel 返回 terminal，不改变终态 | 否 |

**不允许出现的持久状态：**

`status IN ('queued','retry_wait') AND cancel_requested_at IS NOT NULL`

在新契约中，该组合必须由数据库约束/原子转换避免，而不能仅靠应用层保证。

## 4. 目标数据库行为

### 4.1 heartbeat 的规范顺序

在校验 Attempt/token、锁定 Attempt 与 Job 并确认 Job 仍 running 后：

1. 首先检查 `cancel_requested_at IS NOT NULL`。
2. 若取消已登记：
   - 不调用 `settle_failed_attempt`；
   - 不将 Job 推入 retry_wait；
   - 不清除取消标记；
   - 不延长 lease（推荐不再续租，避免 Worker 收到取消后仍获得额外执行窗口）；
   - 返回 `cancel_requested`。
3. 仅当没有取消请求时，才检查 deadline。
4. deadline 已到：按既有超时策略调用受控 settlement，并返回 `deadline_exceeded`。
5. deadline 未到：再验证 lease 有效并执行正常续租，返回 `renewed`。

注意：取消分支不续租是建议的契约变化，需要在 0004 设计中确认。Supervisor 收到 cancel_requested 后立即 KillOnly；若死亡确认失败，停止 heartbeat，等待 lease/recovery 收敛。若现有终止预算证明必须额外保留 lease，应明确其上界，不能默认为续租。

### 4.2 queued/retry_wait 取消

保留既有直接取消语义，并确保同一事务内：
- status = cancelled；
- completed_at 写入；
- next_attempt_at 清空；
- input_expires_at 按现有保留策略设置；
- cancel_requested_at 不应成为后续可领取条件的残留因素。

### 4.3 recovery

恢复扫描继续以 Job 行锁为最终裁决：
- running + cancel_requested_at 非 NULL + lease expired：Job cancelled；Attempt 记录 lease_expired 或专门的 recovery-cancelled 结果，审计必须明确是 recovery 代为收尾，不能伪装成 Worker 已确认。
- running + 无 cancel + lease expired：按 deadline/lease-expired 规则调用统一 settlement。
- queued/retry_wait：正常 queue expiry 规则。
- recovery 不得把已有取消请求的 Job 重新置入 retry_wait。

### 4.4 统一状态不变量

数据库层应保证：

- 取消请求一旦在 running Job 上持久化，该 Job 不再进入任何可领取状态。
- 任一 queued/retry_wait Job 的 `cancel_requested_at` 必须为 NULL。
- Worker 只有在本地确认 Executor 已死亡后才调用 confirm。
- deadline_exceeded 表示 DB 已完成超时 settlement；Worker 必须 KillOnly + 零 DB 写。
- 取消请求先提交时，heartbeat 返回 cancel_requested，不得返回 deadline_exceeded，也不得触发超时重试。
- deadline settlement 先提交时，后续 cancel request 必须能直接取消 retry_wait，不允许留下不可领取的 retry_wait。

## 5. 事务锁与并发要求

现有受控函数主要采用 Attempt 行锁后 Job 行锁；cancel request 只锁 Job。新设计应：

1. 记录并统一所有多行协调函数的锁顺序，避免引入 Job→Attempt 与 Attempt→Job 的反向锁序死锁。
2. heartbeat、complete、fail、confirm、recovery 的竞争必须以 Job 行锁下的当前状态为最终判定。
3. 若要让 cancel 优先于 deadline，heartbeat 必须在调用 settlement 前读取持锁状态下的 cancel 标记。
4. cancel request 与 heartbeat 的先后关系以 PostgreSQL 行锁串行化结果为准。
5. 不允许在事务外先读 cancel 标记，再在无锁状态下决定是否重试。
6. 明确处理 recovery 与 heartbeat 同时锁定 Attempt/Job 的顺序，验证不存在可重现死锁；若保留现有锁顺序，必须证明 recovery 的查询/锁定顺序不会形成环。

## 6. Migration 0004 的候选修改范围（仅供后续设计）

不在本文给出可执行 SQL。候选新 Migration 需要独立考虑：

- 替换 heartbeat 受控函数，使 cancel 检查优先于 deadline settlement；
- 检查是否需要替换/修订 recovery 函数，确保 cancel 优先且审计语义准确；
- 通过受控函数状态转换或数据库约束，阻止 queued/retry_wait 与非空 cancel_requested_at 共存；
- 必须保留 Worker 最小权限：Worker 仍只 EXECUTE 受控函数，不授予业务表 DML；
- 更新函数签名/权限时，显式处理旧函数 DROP/CREATE、PUBLIC revoke、Worker role grants；
- 不修改已发布的 0001/0002/0003 文件及其 hash；新增 0004 并更新迁移清单；
- Migration 0004 不能顺手引入 DB-BLOCKER-02 的时间契约，两个问题分别设计、分别评审。

是否需要新增状态列、取消原因列或新返回枚举，须由后续 SQL 设计证明必要性；不应为了“看起来完整”而扩大 schema。

## 7. 必须具备的验收测试

| ID | 场景 | 预期 |
|---|---|---|
| DB01-T01 | cancel request 已提交，heartbeat 同时/随后发现 deadline 已到 | 返回 cancel_requested；不调用 timeout settlement；Job 不进入 retry_wait |
| DB01-T02 | deadline settlement 已提交，随后 cancel request | Job 从 retry_wait 直接进入 cancelled |
| DB01-T03 | 两事务并发重复执行，多轮压力测试 | 只能得到矩阵中的合法终态；不得残留 retry_wait + cancel_requested_at |
| DB01-T04 | cancel 已登记，Worker 死亡确认失败，lease 到期 | recovery 收尾 cancelled；审计标记 recovery；不重试 |
| DB01-T05 | 无 cancel 的 deadline 超时且可重试 | 维持既有 retry_wait/backoff 行为 |
| DB01-T06 | 无 cancel 的 deadline 超时且达到 max_attempts/queue expiry | 按既有终态策略收尾 |
| DB01-T07 | retry_wait Job 在取消请求后直接取消 | 不可被 claim；终态字段完整 |
| DB01-T08 | 约束/函数替换后以 Worker role 调用 | Worker 仍无表 DML；只可调用授权的受控函数 |
| DB01-T09 | 0001/0002/0003 文件 hash 与既有基线比对 | 历史文件不变；仅新增 0004 与 hash 清单更新 |
| DB01-T10 | heartbeat 与 recovery 并发 | 无死锁；状态/Attempt/audit 一致且幂等 |

还必须增加一个数据库不变量巡检查询，用于在迁移前后发现历史残留：
`status IN ('queued','retry_wait') AND cancel_requested_at IS NOT NULL`。
巡检结果必须为零，或对历史异常行提供明确、可审计的修复方案；不得静默清理。

## 8. 对 Worker 生命周期设计的影响

- 收到 heartbeat `cancel_requested`：KillOnly → 死亡确认 → 最多一次 confirm。
- 收到 `deadline_exceeded`：KillOnly + 零 DB 写；数据库已完成 settlement。
- Worker 不清除 cancel_requested_at，不自行把 Job 改为 cancelled/retry_wait。
- 若 confirm 返回 not_requested/stale，按 FSM-BLOCKER-05 的保守路径处理；不声称取消成功。
- 本修订不改变 claim RPC 不确定、短 deadline 本地计时、heartbeat 连续超时预算、进程树回收等其他阻断。

## 9. 关闭标准

DB-BLOCKER-01 只有在以下条件全部满足后才能标记 Closed：

1. 产品/业务接受“已提交取消优先于 deadline 重试”的语义；
2. 0004 SQL 设计完整，锁顺序、事务竞争和约束均经独立评审；
3. 上述测试全部具备，并有真实 PostgreSQL 并发测试结果；
4. 迁移升级/回滚或前向修复策略明确；
5. Worker 最小权限和迁移 hash 机制经核验；
6. PR 文档明确 0001/0002/0003 未被改写；
7. 独立复审确认不再可能产生 queued/retry_wait + cancel_requested_at 非空的状态。

**当前结论：DB-BLOCKER-01 已有推荐语义和目标契约，但尚未关闭；Migration 0004 未设计完成、未实施。**
