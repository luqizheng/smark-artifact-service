# DB-BLOCKER-01 — Migration 0004 独立设计复审报告

> 复审日期：2026-11-25  
> 复审对象：PR #5，分支 docs/db-blocker-01-migration-0004-design  
> 复审基线：main / c11e79a51a145ded026632630bb0b52aa44dc311  
> 当前裁决：**Changes Required / Not approved for implementation**  
> 本报告是设计审查，不是 SQL 实施、数据库执行或 PostgreSQL 测试报告。

## 1. 复审范围与事实来源

重新读取并交叉核对：

- main: migrations/0001_init.sql
- main: migrations/0002_job_coordination.sql
- main: migrations/0003_job_coordination_fixes.sql
- PR #5: DB-BLOCKER-01-migration-0004-design-review.md
- PR #4: ART-03B-02-v2.2-supervisor-fsm-lease-heartbeat.md
- PR #4: ART-03B-02-v2.2-independent-review-report.md

Migration 0001/0002/0003 的已知 blob SHA 分别为：

| 文件 | main blob SHA |
|---|---|
| migrations/0001_init.sql | 73ff9302b509d96f3d89cb1f4601fdba6ecb851c |
| migrations/0002_job_coordination.sql | 63851dc200e5a0cd08445597555afab942e072e1 |
| migrations/0003_job_coordination_fixes.sql | c0a829725d07fb71c3351c9f786a42d30a73352d |

复审没有修改以上文件，也没有运行数据库。

## 2. 核心设计裁决

### 2.1 取消优先于 deadline retry settlement

**结论：方向成立。**

当前 0003 heartbeat 的关键事实：
- 先锁 Attempt 并验证 running/token；
- 再锁 Job 并验证 running；
- deadline 到达时先调用 settle_failed_attempt；
- 再判断 Lease；
- 更新 heartbeat/lease；
- 最后才返回 cancel_requested。

因此 cancel 已提交且 deadline 到达时，旧逻辑可能先把 Job 推进 retry_wait，而 cancel_requested_at 保留；claim 排除 cancel_requested_at 非空的行，形成不可领取组合。

新设计把已持久化 cancel 检查放到 deadline settlement 之前，且取消分支不续租、不更新 heartbeat、不调用 settlement，能阻断该路径。deadline settlement 先获得 Job 锁并提交时，后续 request_cancel 对 retry_wait 直接转 cancelled，也与现有状态守卫兼容。

### 2.2 Lease 过期后的取消收敛

**结论：设计方向可接受，但必须保持严格单向语义。**

允许正确 token 在 Lease 已过期、Recovery 尚未锁定 Attempt 时得到 cancel_requested，前提是：
- Attempt 仍为 running；
- Job 仍为 running；
- cancel_requested_at 已持久化；
- 不续租、不刷新 heartbeat、不 settlement；
- Worker 只允许 KillOnly/confirm，不能恢复 Executor、spawn、resume、读取或提交输出。

若 Recovery 先把 Attempt 更新为 lease_expired，后续 heartbeat/confirm 必须 stale。该行为是取消收敛权限，不是租约恢复。

### 2.3 confirm 与 recovery 的竞争

**结论：锁顺序正确，待真实 PostgreSQL 验证。**

现有 confirm 与 recovery 都采用 Attempt → Job 锁序：
- confirm 先完成：Attempt/Job 进入取消终态，recovery 后续跳过；
- recovery 先完成：Attempt 进入 lease_expired，Job 按 cancel 标记收敛 cancelled；confirm 后续 stale；
- heartbeat 与 recovery 同样先竞争 Attempt 锁。

cancel request 只锁 Job，不再反向获取 Attempt，因此该路径本身不形成 Job→Attempt 锁环。仍须验证 READ COMMITTED 锁等待后的新行版本可见性，以及 REPEATABLE READ/SERIALIZABLE 的 40001 处理。

### 2.4 DB cancelled 与 OS death

**结论：必须分离，PR #5 新增定义正确。**

DB cancelled 只能代表调度/结果提交资格撤销，不能证明 OS 进程树死亡。Recovery 可以先撤销 DB 权限，但仅在 03B-03 提供独立于 Worker/Supervisor 的 runtime/controller、可恢复定位 Attempt cgroup/container、具备有界回收 SLA 时，才可允许生产采用该语义。

DeathUnconfirmed 时必须不 confirm、不提交、不清理目录；只有外部控制面提供 death-confirm 证据后才可清理。当前仓库没有 03B-03 实现证据，因此该前置条件仍未关闭。

## 3. 新发现：Preflight 与 CHECK 启用之间的竞态

**严重设计缺口，已在 PR #5 设计稿 §13 补充。**

若迁移先执行只读 preflight，稍后才 ADD CHECK，则两者之间可能有并发事务提交非法状态。单独写 preflight 查询不能证明约束启用时没有漏网行。

推荐候选顺序：
1. 在受控迁移事务中先 ADD CHECK NOT VALID；
2. 该约束开始约束后续 INSERT/UPDATE；
3. 执行历史异常行 preflight，并检查相关 Attempt；
4. 命中非零则抛错并回滚，不静默修复；
5. 零异常才 VALIDATE CONSTRAINT；
6. 验证约束 validated、定义正确、非法行计数为零。

该顺序仍需按目标 PostgreSQL 版本和实际迁移框架验证 DDL 锁模式、锁持续时间、事务原子性与并发 DML 行为。新增 T27 覆盖迁移期间并发写入竞态。**当前没有实际 PostgreSQL 证据，不得把这一设计要求标为已验证。**

## 4. 逐项复审结果

| 项目 | 结论 | 状态 |
|---|---|---|
| heartbeat cancel 优先于 deadline settlement | 逻辑成立 | 待 SQL 实施与 PG 测试 |
| cancel 分支不续租、不写 heartbeat | 逻辑成立 | 待 SQL/Worker 集成测试 |
| Lease 已过期但 Recovery 未锁 Attempt | 只允许 KillOnly/confirm | 待并发测试 |
| Recovery 先把 Attempt 置 lease_expired | 后续 heartbeat/confirm stale | 待并发测试 |
| confirm/recovery 锁序 | Attempt → Job 一致 | 待死锁/并发测试 |
| settle_failed_attempt cancel guard | 需要基于持锁后的最新 Job 快照 | 待 SQL 设计复核 |
| queued/retry_wait CHECK | 不变量表达正确 | 待约束覆盖与迁移测试 |
| 历史数据 preflight | 必须 race-free | §13 已补充；待 PostgreSQL 验证 |
| Worker 最小权限 | 不应扩大权限 | 待迁移后 grant 回归 |
| DB cancelled / OS death | 必须解耦 | 设计已补充；03B-03 未验证 |
| DeathUnconfirmed 目录 | 禁止清理，交外部 controller | 设计已补充；平台未验证 |
| DB01-0004-T01–T27 | 测试场景已定义 | 尚未执行 |

## 5. 必须补齐的实施前证据

1. 完整的 Migration 0004 SQL 草案（仅在设计批准后创建），逐行证明函数替换、异常处理、约束顺序和函数权限。
2. settle_failed_attempt 的所有调用点与锁持有证明；guard 必须在任何 Attempt/Job 更新前生效，且不得信任陈旧 composite 参数。
3. 目标 PostgreSQL 主版本下 ADD CHECK NOT VALID / VALIDATE 的锁模式和事务行为实测。
4. 真实 PostgreSQL 多连接测试覆盖 T01–T27；heartbeat/cancel/recovery/confirm 并发矩阵每组至少 1,000 轮。
5. READ COMMITTED、REPEATABLE READ、SERIALIZABLE、lock timeout、statement timeout、连接断开、事务回滚和死锁测试。
6. Worker role 仍仅有既有受控函数 EXECUTE；业务表 DML 为零；PUBLIC revoke 精确保持。
7. 03B-03 证明 Worker/Supervisor 崩溃后外部 controller 仍可定位、终止和确认 Attempt 隔离单元；完成目录清理故障注入。
8. DB cancelled 后迟到的 complete/fail/heartbeat/confirm 不得恢复执行权或提交资格。

## 6. 最终裁决

**Changes Required / Not approved for implementation。**

本轮设计已明确取消仲裁、过期 Lease 的 KillOnly-only 语义、confirm/recovery 锁竞争和 DB/OS 双重生命周期，并补上 preflight race。核心方向可继续推进，但以下证据仍缺失：

- Migration 0004 尚无可执行 SQL；
- 没有真实 PostgreSQL 并发/锁/迁移测试；
- 03B-03 外部 runtime/controller 与 cgroup/container 回收能力尚未实现验证；
- Worker 最终协议与数据库新语义尚未通过集成测试。

PR #5 必须保持 Draft；不得合并，不得创建或执行 Migration 0004。Migration 0001/0002/0003、migrations.sha256、deploy/db-roles.sql 和业务代码均保持不变。
