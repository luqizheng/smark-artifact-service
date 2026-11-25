# DB-BLOCKER-01 — Migration 0004 设计审查稿：取消优先与可领取状态不变量

> 状态：**Design Review / Proposed — 不可实施**。
> 基线：`main`，Migration 0001/0002/0003 当前已发布内容。
> 本文只定义 Migration 0004 的目标、函数替换边界、锁语义、约束和验收；不包含可直接执行的 Migration SQL。
> 明确禁止：修改 0001/0002/0003、更新 migrations.sha256、创建可执行 0004、部署或运行数据库写操作。
> 与 ART-03B-02 PR #4 分离：本专题只关闭 DB-BLOCKER-01，不混入 DB-BLOCKER-02 的时间契约。

## 1. 裁决摘要

**推荐采用：已提交的取消请求优先于 deadline retry settlement。**

Migration 0004 目标只包含四项：

1. 替换 `heartbeat_generation_job`：在 deadline settlement 之前识别已持久化的 `cancel_requested_at`；返回 `cancel_requested`，不续租、不调用 timeout settlement。
2. 对内部辅助函数 `settle_failed_attempt` 增加防御性拒绝：带有已登记取消请求的 Job 不允许进入失败/超时重试 settlement。
3. 为 `generation_job` 增加数据库 CHECK 不变量：`queued/retry_wait` 状态必须满足 `cancel_requested_at IS NULL`。
4. 保留现有 cancel request、complete、fail、claim、recovery 的业务行为；逐项验证它们与新不变量兼容，不做无必要的函数重写。

不新增表、不新增状态列、不新增索引、不改变 retry backoff、不修改错误码白名单、不改变 Worker 的最小权限模型。

**不在本轮解决：** DB-BLOCKER-02 remaining-duration RPC、heartbeat 时钟映射、Supervisor FSM 实施、Worker 代码、cgroup/container。

## 2. 代码与 SQL 事实基线（已对 main 核验）

复核文件及 Git blob SHA：

| 文件 | main blob SHA |
|---|---|
| `migrations/0001_init.sql` | `73ff9302b509d96f3d89cb1f4601fdba6ecb851c` |
| `migrations/0002_job_coordination.sql` | `63851dc200e5a0cd08445597555afab942e072e1` |
| `migrations/0003_job_coordination_fixes.sql` | `c0a829725d07fb71c3351c9f786a42d30a73352d` |

### 2.1 0003 heartbeat 当前顺序

当前 `heartbeat_generation_job`：
1. 锁 Attempt，验证 running、token hash；
2. 锁 Job，验证 running；
3. 先检查 deadline；若已到，调用 `settle_failed_attempt(... GENERATION_TIMEOUT, retryable=true ...)`；
4. 再检查 lease 是否过期；
5. 更新 Attempt/Job heartbeat 与 lease；
6. 最后才检查 `cancel_requested_at` 并返回 `cancel_requested`。

因此，取消已提交且 deadline 已到时，heartbeat 会先把 Attempt 结算并可能将 Job 推入 `retry_wait`，根本到不了 cancel 分支。

### 2.2 相关函数当前行为

- `request_generation_job_cancel`：锁 Job；queued/retry_wait 直接 cancelled；running 只设置 `cancel_requested_at`。
- `claim_generation_job`：候选锁定复核已要求 `cancel_requested_at IS NULL`。
- `complete_generation_job_attempt`：验证有效 lease 后检查取消标记，返回 `cancel_requested`，不完成成功状态转换。
- `fail_generation_job_attempt`：验证有效 lease 后检查取消标记，返回 `cancel_requested`，不进入失败/重试 settlement。
- `recover_expired_generation_jobs`：锁 Attempt 后锁 Job；若 running 且取消标记非空，先将 Attempt 记为 `lease_expired`，再将 Job 置为 `cancelled`；只有无取消标记时才调用 `settle_failed_attempt`。
- `settle_failed_attempt`：当前按 retryable 与 attempt 次数推进 retry_wait；自身没有 cancel guard。
- 0001 的 Job 状态守卫允许 running→retry_wait、retry_wait→cancelled 等状态转换；它不约束 `cancel_requested_at` 与状态的组合。

### 2.3 已发现的持久化缺陷

当前 schema 没有数据库约束禁止：

`status IN ('queued','retry_wait') AND cancel_requested_at IS NOT NULL`

claim 排除这类行，但不会自动收敛它们。0003 heartbeat 的 deadline 分支可产生该组合，造成 Job 永久不可领取，直到 queue expiry。

## 3. 规范行为

### 3.1 heartbeat 新顺序

在参数校验、Attempt token 验证、Attempt 行锁、Job 行锁和 Job running 检查之后：

1. 若 `cancel_requested_at IS NOT NULL`：
   - 返回 `cancel_requested`；
   - 不调用 `settle_failed_attempt`；
   - 不更新 Attempt/Job heartbeat；
   - 不延长 lease；
   - 不更改 Attempt/Job status；
   - 不清除取消标记。
2. 否则，执行现有 deadline 检查：
   - deadline 已到：按既有规则调用超时 settlement，返回 `deadline_exceeded`；
   - deadline 未到：继续现有 lease 有效性检查。
3. lease 有效时按既有规则续租并返回 `renewed`；lease 无效返回 `stale`。

注意：取消分支位于当前 deadline 检查之前，但仍位于 Attempt/token 和 Job 状态校验之后。这样旧 token、非 running Attempt、非 running Job 仍然得到 stale，不泄露取消状态；而有效 Attempt 一旦观察到取消请求，即使 deadline 同时已到，也先得到 cancel_requested。

**取消分支不续租。** Worker 收到 cancel_requested 后必须立即 KillOnly；死亡确认成功后调用一次 confirm。若 Worker 失联，既有 recovery 在 lease 到期后收尾 cancelled。不得通过持续 heartbeat 延长一个已请求取消的 Attempt。

### 3.2 deadline settlement 先于 cancel request 的竞争

若 heartbeat 已先取得 Job 行锁并提交 deadline settlement：
- 若可重试，Job 进入 retry_wait，取消标记仍为空；
- 随后 cancel request 取得 Job 行锁，看到 retry_wait，按既有函数逻辑直接将 Job 改为 cancelled；
- 不会留下 retry_wait + 非空 cancel_requested_at。

若 cancel request 先取得 Job 行锁并提交：
- heartbeat 随后取得 Job 行锁时读取到非空取消标记；
- 返回 cancel_requested，不执行 deadline retry settlement。

竞争按 PostgreSQL Job 行锁事务顺序决定，不按客户端发起时间、网络到达时间或 Worker wall clock 决定。

### 3.3 内部 settlement 防御

`settle_failed_attempt` 是 SECURITY DEFINER 内部辅助函数，当前调用者均应已锁定 Attempt 和 Job。Migration 0004 应增加 fail-closed guard：

- 若传入的 Job 快照 `cancel_requested_at IS NOT NULL`，拒绝执行失败/超时 settlement（使用稳定、可测试的 SQLSTATE/错误标识）；
- 不允许该辅助函数在取消已登记时写入 retry_wait；
- 该 guard 是防御未来新增调用点，不替代 heartbeat、fail、recovery 各自正确的业务分支；
- 现有三个调用路径必须在调用前处理取消：heartbeat 新分支、fail 现有分支、recovery 现有分支。

在 SQL 设计稿中必须证明传入的 `p_job` 是持有 Job 行锁后读取的当前行版本。若不能证明，辅助函数必须自行按 ID 重新读取并锁定当前 Job，再基于当前行裁决，不能信任陈旧 composite 参数。

## 4. 数据库不变量约束

建议新增命名约束：

`generation_job_cancel_not_claimable_check`

逻辑：

```sql
CHECK (
    status NOT IN ('queued', 'retry_wait')
    OR cancel_requested_at IS NULL
)
```

这条约束：
- 允许 running + cancel_requested_at 非空；
- 允许 terminal + cancel_requested_at 非空（保留取消审计事实）；
- 禁止 queued/retry_wait + cancel_requested_at 非空；
- 防止未来任何新写路径绕过应用约定重新制造不可领取状态。

约束不能替代函数逻辑：若未来函数试图把 running + cancel_requested_at 非空的 Job 更新为 retry_wait，事务会失败；正确行为仍应是 cancelled/停止 settlement，而不是依赖约束异常作为正常业务控制流。

### 4.1 迁移前异常数据巡检

Migration 0004 必须先执行只读 preflight：

```sql
SELECT id, status, cancel_requested_at, next_attempt_at, attempt_count
FROM artifact.generation_job
WHERE status IN ('queued', 'retry_wait')
  AND cancel_requested_at IS NOT NULL
ORDER BY id;
```

还应对命中 Job 检查是否存在 status=claimed/running 的 Attempt，防止只看 Job 行而遗漏孤儿 Attempt。

**推荐 fail closed：**
- 若巡检结果非零，Migration 0004 中止，不自动清理；
- 先输出受控、可审计的历史数据修复方案，再由独立操作审批执行；
- 若结果为零，继续添加并验证 CHECK 约束。

不得把历史异常行静默改为 cancelled，也不得在 Migration 内无审计地清空 cancel_requested_at。

### 4.2 约束部署方式

SQL 设计稿应比较：
- 直接 ADD CHECK（默认立即验证）；
- ADD CHECK NOT VALID，再 VALIDATE CONSTRAINT。

最终选择必须结合目标 PostgreSQL 版本、表规模、锁影响和部署停机预算。无论采用哪种方式：
- 迁移提交后约束必须 VALIDATED；
- 不允许长期保留 NOT VALID；
- Migration 应在同一受控部署窗口内完成函数替换与约束启用，避免出现函数已改变但数据库不变量未生效的中间运行版本；
- 若分步提交，必须证明每个中间状态仍然安全。

## 5. 函数替换范围与不替换范围

| 对象 | 0004 动作 | 原因 |
|---|---|---|
| `heartbeat_generation_job` | **必须替换** | cancel 检查移动到 deadline settlement 前，并取消分支不续租 |
| `settle_failed_attempt` | **建议替换** | 增加内部 cancel guard，防止未来调用点误入 retry |
| `request_generation_job_cancel` | 不替换 | 现有 Job 行锁和 queued/retry_wait 直接 cancelled 语义已符合目标 |
| `claim_generation_job` | 不替换 | 已过滤 cancel_requested_at 非空，且新 CHECK 提供 DB 防线 |
| `complete_generation_job_attempt` | 不替换 | 已在成功状态转换前检查取消标记 |
| `fail_generation_job_attempt` | 不替换 | 已在调用 settlement 前检查取消标记 |
| `confirm_generation_job_cancelled` | 不替换 | 允许取消请求后 lease 到期仍确认；与 Supervisor 死亡确认流程相容 |
| `recover_expired_generation_jobs` | 不替换 | 已在调用 settlement 前优先处理 cancel_requested_at |
| `guard_job_transition` | 不替换 | 新 CHECK 负责状态与取消标记组合；避免重复实现同一不变量 |

若 SQL 实际复核发现上述函数在 main 已变化，必须以新基线重新评估，不得照抄此表。

## 6. 锁顺序与事务隔离要求

现有多行协调函数主要采用：
- heartbeat / complete / fail / confirm：Attempt 行锁 → Job 行锁；
- recovery：Attempt 行锁 → Job 行锁；
- cancel request：只锁 Job 行；
- claim：只锁候选 Job 行，之后在同一事务内创建 Attempt。

0004 不得引入 Job→Attempt 的反向双行锁顺序。cancel request 只锁 Job、不再获取 Attempt 锁，因此不会仅因其先持有 Job 锁而形成 Job↔Attempt 锁环；heartbeat/recovery 仍按 Attempt→Job 顺序等待。

必须明确并测试：
- 受控函数在 PostgreSQL READ COMMITTED 下的行锁等待后可见性：等待 Job 锁后读取到 cancel request 已提交的新行版本；
- REPEATABLE READ / SERIALIZABLE 下若出现 serialization failure，调用方如何识别 SQLSTATE 40001；不能把它误分类为业务 stale；
- statement timeout、lock timeout、连接断开和事务回滚不会留下半更新状态；
- advisory recovery lock 与 Attempt/Job 行锁的顺序不形成死锁；
- 不允许在无锁状态下读取 cancel 标记，再决定是否调用 settlement。

## 7. Migration 0004 文件级设计要求

未来真正实施时：
1. 新增 `migrations/0004_cancel_deadline_arbitration.sql`（最终命名按仓库迁移命名规范确认）。
2. 只在新文件中新增/替换对象；不改写 0001/0002/0003。
3. 如果使用 `CREATE OR REPLACE FUNCTION` 且签名/返回类型不变，确认 PostgreSQL 对参数名、返回类型和 OUT 参数限制；若签名必须变化，按 DROP/CREATE 处理并重授最小权限。
4. 若替换函数后 EXECUTE grant 未被保留或被 DROP 丢失，必须在同一迁移部署链路重新执行精确 GRANT，并核对 PUBLIC revoke。
5. 更新 `migrations.sha256` 时只追加 0004 的 hash；前三项 hash 必须逐字保持不变。
6. 重新运行 migration hash CI、DB role CI、SQL lint、函数签名/权限检查。
7. 不给 Worker 任何业务表 SELECT/INSERT/UPDATE/DELETE 权限；Worker 继续只调用既有受控函数。
8. 不将 DB-BLOCKER-02 的 remaining duration 返回字段并入本迁移。

## 8. 必须通过的 PostgreSQL 验收

所有并发场景必须使用真实 PostgreSQL 多连接/多事务测试，不接受只用 mock repository 或单连接顺序测试。

| ID | 场景 | 预期 |
|---|---|---|
| DB01-0004-T01 | cancel request 提交后，deadline 已到的 heartbeat | 返回 cancel_requested；Attempt/Job 不被 settlement；lease 不延长 |
| DB01-0004-T02 | heartbeat 先持 Job 锁并提交 deadline retry_wait，cancel request 随后执行 | Job 被直接改为 cancelled；不残留 retry_wait |
| DB01-0004-T03 | cancel request 先持 Job 锁，heartbeat 已持 Attempt 锁后等待 Job 锁 | cancel 提交后 heartbeat 观察新版本并返回 cancel_requested |
| DB01-0004-T04 | heartbeat deadline 与 cancel request 多轮随机并发 | 只出现合法结果；绝不出现 queued/retry_wait + cancel_requested_at |
| DB01-0004-T05 | deadline 到达、无取消、attempt 可重试 | 保持既有 retry_wait/backoff |
| DB01-0004-T06 | deadline 到达、无取消、达到 max_attempts | 保持既有 timed_out 终态 |
| DB01-0004-T07 | 取消已登记，Worker 不确认死亡，lease 到期后 recovery | Job cancelled；Attempt 审计明确 recovery 收尾；不 retry |
| DB01-0004-T08 | 取消已登记时调用 fail | 返回 cancel_requested；不调用 settlement |
| DB01-0004-T09 | 取消已登记时调用 complete | 返回 cancel_requested；不完成 succeeded |
| DB01-0004-T10 | 内部 settlement 在 cancel 标记非空时被直接调用 | 防御 guard 阻止 settlement；无 retry_wait |
| DB01-0004-T11 | claim 与 cancel queued/retry_wait 并发 | Job 只能被 claim 或被取消之一；锁顺序下无双重领取 |
| DB01-0004-T12 | 历史异常行 preflight 非零 | Migration fail closed；无数据被静默改写 |
| DB01-0004-T13 | 约束验证后尝试写入 retry_wait + cancel_requested_at | CHECK 拒绝事务 |
| DB01-0004-T14 | PostgreSQL READ COMMITTED 锁等待可见性 | 等待结束后读取到先提交取消事务的行版本 |
| DB01-0004-T15 | REPEATABLE READ / SERIALIZABLE 并发冲突 | SQLSTATE 分类明确；无半提交状态 |
| DB01-0004-T16 | Worker role 权限回归 | 仅受控函数 EXECUTE；无业务表 DML |
| DB01-0004-T17 | Migration hash 回归 | 0001/0002/0003 hash 不变；0004 新增且 manifest 一致 |
| DB01-0004-T18 | Migration 重复/升级失败恢复 | 迁移框架阻止重复执行；失败事务不留半迁移对象 |

### 8.1 必须执行的并发矩阵

至少分别执行 1,000 轮随机交错（或更高，按 CI 可接受预算）：
- heartbeat 获得 Job 锁在先；
- cancel request 获得 Job 锁在先；
- heartbeat 已锁 Attempt、等待 Job；
- recovery 已锁 Attempt、等待 Job；
- claim 与 queued/retry_wait cancel 竞争。

记录每轮事务顺序、SQLSTATE、函数返回值、最终 Job/Attempt 状态；断言无死锁、无非法状态、无重复终态审计。

## 9. 上线/回滚与运维边界

- 这是纯数据库状态机修复，不需要 Worker 代码同步发布；旧 Worker 已能识别 `cancel_requested` 并停止，但其旧行为可能仍在收到该值后按旧协议处理，实施前必须核对当前 Worker 对返回枚举的处理。
- 迁移前先做只读 preflight，并确认 recovery 扫描器与 Worker 版本兼容。
- 迁移后立即运行不变量巡检：非法组合数量必须为零。
- 若 0004 已提交，不允许通过删除 CHECK 或恢复旧 heartbeat 函数来“回滚”到已知缺陷；修复应采用前向补丁。
- 若迁移在事务内失败，确认 PostgreSQL 对象与 migration ledger 均回滚；若迁移框架非事务式，必须提供逐步恢复手册。
- 不把迁移通过等同于 ART-03B-02 完成；其他 timing/process/FSM 阻断仍存在。

## 10. 设计复核问题与关闭门槛

独立 SQL 复审必须逐项回答：
1. heartbeat 的 cancel 分支是否确实发生在 deadline settlement 之前，并且仍在 token/Job 状态验证之后？
2. 取消分支是否不续租、不写 Attempt/Job heartbeat？
3. `settle_failed_attempt` 的 guard 是否基于持锁的最新 Job 行版本？
4. 新 CHECK 是否覆盖所有 queued/retry_wait 写入路径？
5. 现有 complete/fail/recovery/cancel/claim 函数是否与新 CHECK 完全兼容？
6. 迁移前异常数据是否 fail closed，且没有静默修复？
7. 锁顺序是否无环，READ COMMITTED 下并发结果是否可证明？
8. 函数 grant/PUBLIC revoke 是否保持 Worker 最小权限？
9. 0001/0002/0003 与原 hash 是否完全不变？
10. 所有 18 项测试与并发矩阵是否有真实 PostgreSQL 结果？

**只有上述问题全部有证据，且 SQL 复审结论为 Approved，才允许把设计推进到 Migration 0004 实施任务。**

当前状态仍为：DB-BLOCKER-01 **Design proposal / Not approved for implementation**。
