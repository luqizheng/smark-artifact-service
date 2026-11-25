

---

## 11. 取消终态与 OS 进程死亡：权威边界补充（v2 复审后，规范性）

本节是对本文 §3.1、§3.3、§5、§8、§9 中涉及取消、Lease 过期、Worker recovery 和终态语义的规范性补充；如有冲突，以本节为准。

### 11.1 generation_job.status = cancelled 的精确定义

**裁决：数据库 cancelled 只表示该 Job 的调度资格及 DB 提交资格已撤销，不表示 OS 进程树已死亡。**

必须区分两个独立事实：

1. **DB execution authority revoked**：Job 不可再次 claim；当前 Attempt 不得 complete/fail、不得发布 Artifact；数据库已不再承认该 Attempt 有提交结果的资格。
2. **OS process tree dead**：该 Attempt 对应的 Executor 进程树已由平台硬边界终止，且由可信控制面确认不存在存活后代。

两者不能互相推导。尤其：
- Job/Attempt 进入 cancelled，不构成进程死亡证据；
- confirm_generation_job_cancelled 成功只能由已完成平台级死亡确认的 Supervisor 调用；数据库函数本身无法验证 OS 进程；
- recovery 可以撤销 DB execution authority，但不能把“Lease 已过期”伪装成“进程树已死亡”；
- 运维、审计、监控和 API 不得把 Job cancelled 展示/解释为“Executor 已停止”，除非另有独立的 ProcessCleanup 状态证据。

不增加 generation_job 的新状态列。OS cleanup 是独立的平台资源生命周期，不属于 Job 状态机的事实来源。

### 11.2 允许 recovery 先撤销 DB 权限，但必须满足的硬边界

**推荐：允许 recovery 在 OS 死亡尚未确认时，将 Job/Attempt 的 DB 状态收敛为 cancelled；但这只在 03B-03 平台硬边界满足以下强制条件时成立。**

1. Executor 只能进行无外部副作用的受限计算：无网络、无业务凭据、无 Artifact/LMMS/AI-PM 访问能力；不能自行上传、finalize、登记 Output/Version 或调用 complete/fail/confirm。
2. 只有 Supervisor 能提交冻结后的输出；Supervisor 必须在完整进程树死亡确认后才读取/冻结输出并发起任何结果提交。
3. Executor 与 Supervisor 的控制 IPC 必须在 KillOnly 后关闭或拒绝新的执行/结果请求；Executor 无法绕过 Supervisor 获得提交能力。
4. 每个 Attempt 必须绑定到平台管理的硬隔离单元（生产 Linux 为不可逃逸的 cgroup v2 / container 边界；Windows 为受控 Job Object），Executor 无权迁移、销毁或改变该边界。
5. **硬隔离单元的最终销毁责任不能只依赖当前 Worker/Supervisor 进程仍存活。** Worker 崩溃、Supervisor 崩溃或 DB recovery 完成后，外部 runtime/controller 必须仍能定位并终止该 Attempt 的隔离单元；平台必须提供有界回收 SLA、失败告警和人工隔离手段。
6. DB cancelled 后，任何迟到的 Worker/Supervisor RPC 都不得恢复执行权或发布结果；DB 受控函数的 Attempt/Job 状态与 token 校验继续作为提交拒绝边界。

因此，DB recovery 可先写 cancelled，是因为它撤销的是调度/提交资格；进程仍存活期间只能继续做无副作用计算，最终由独立 runtime/controller 强制回收。**如果 03B-03 无法证明上述条件，禁止采用“DB cancelled 先于 OS death”的方案；必须改为有界 cleanup-pending 设计，并在确认死亡后才写 cancelled。**不能以无限续租或无限等待替代硬边界。

当前仓库尚未证明这些 03B-03 平台能力已经实现。因此这是一项实施前置条件，不是已关闭事实；在证据补齐前，PR #5 不得批准实施。

### 11.3 Lease 已过期、Recovery 尚未取得 Attempt 锁时的 heartbeat

取消请求优先于 deadline settlement，但不得恢复执行权。规范顺序为：

1. 锁定 Attempt 行；
2. 验证 Attempt 仍为 running、token hash 匹配；
3. 锁定 Job 行；
4. 验证 Job 仍为 running；
5. 若 cancel_requested_at IS NOT NULL，返回 cancel_requested，不检查/延长 Lease、不更新 heartbeat、不调用 settlement；
6. 否则先按既有语义检查 deadline；已到 deadline 则执行 timeout settlement 并返回 deadline_exceeded；
7. deadline 未到时再检查 Lease；Lease 已过期返回 stale，否则按 DB-BLOCKER-02 时间契约执行正常续租。

第 5 步意味着：即使 lease_expires_at <= database_now，只要 recovery 尚未取得 Attempt 锁并改变 Attempt 状态，持有正确 token 的旧 Worker 仍可能收到 cancel_requested。这**不是租约续期，也不是重新获得执行权**，只授予一个狭窄的、单向的 KillOnly/confirm 收敛机会：
- 不得 spawn、resume、继续驱动 Executor；
- 不得读取/冻结/上传输出；
- 不得调用 complete/fail；
- 只能终止其拥有的 Attempt 硬隔离单元，并在死亡确认成功后调用一次 confirm；
- 不能完成死亡确认则进入 DeathUnconfirmed，停止所有 DB 写并交给外部 runtime/controller 回收。

若 Attempt 已被 recovery 锁定并更新为 lease_expired，heartbeat 等待后必须返回 stale。不得因为 cancel 标记存在而覆盖 recovery 已完成的 Attempt 状态。

这项语义是“允许过期租约持有者做取消收敛”，不是“过期租约仍然有效”。安全性依赖 DB 状态/token 校验、Supervisor 的单向 KillOnly 状态和 03B-03 硬隔离边界共同成立。

### 11.4 confirm 与 recovery 的锁竞争裁决

当前两条路径都采用相同锁顺序：**Attempt 行锁 → Job 行锁**。Migration 0004 不得改变该顺序。

- **confirm 先取得 Attempt 锁**：验证 Attempt running/token、Job running/cancel_requested；只有调用方已在 OS 层确认死亡才允许调用。confirm 将 Attempt 与 Job 原子置为 cancelled 并提交。Recovery 随后扫描时必须因 Attempt 已非 claimed/running 而跳过。
- **recovery 先取得 Attempt 锁**：将 Attempt 置为 lease_expired，再锁 Job；若 Job 有 cancel 标记，则将 Job 置为 cancelled。confirm 随后等待结束，读取到 Attempt 非 running，返回 stale，不得把 Attempt 从 lease_expired 改回 cancelled。
- **heartbeat 与 recovery 竞争**：二者都先锁 Attempt。heartbeat 先取得锁且发现 cancel 时可返回 cancel_requested（KillOnly only）；recovery 先取得锁并完成 lease_expired 后，heartbeat 返回 stale。
- **confirm/recovery 不得形成 Job→Attempt 反向锁序**；不得引入仅锁 Job 后再等待 Attempt 的新路径。

READ COMMITTED 下，锁等待方必须基于等待结束后可见的当前行版本进行状态判断。REPEATABLE READ/SERIALIZABLE 的 serialization failure（SQLSTATE 40001）按事务失败处理，由调用层安全放弃/重试整个只读决策流程；不能降级为 stale 或 cancel 成功。

### 11.5 DeathUnconfirmed 与目录清理

DeathUnconfirmed 是 Supervisor 本地状态，不是 DB Job 状态。其行为固定为：
- 不调用 confirm、complete、fail；
- 不提交输出，不创建 READY Version；
- 不删除、不复用 Attempt 工作目录；目录保留为隔离态；
- 停止 heartbeat，不通过延长 Lease 等待；
- 把 Attempt 隔离单元标记为待外部回收，并发出可观测告警；
- 外部 runtime/controller 终止并确认整个隔离单元死亡后，才允许清理目录及其临时资源；
- 若外部控制面无法确认死亡，资源继续隔离并升级人工处置，不得把“超时”当作死亡证明。

DB Job 已 cancelled 与 Attempt 目录仍保留可以同时成立。目录清理必须依据独立的 ProcessCleanup 证据，不得只看 Job.status。

### 11.6 新增必须通过的 PostgreSQL + Supervisor 验收

在原 T01–T18 之外，新增：

| ID | 场景 | 预期 |
|---|---|---|
| DB01-0004-T19 | Lease 已过期、cancel 已提交、Recovery 尚未取得 Attempt 锁，旧 token 调 heartbeat | 返回 cancel_requested；不续租、不写 heartbeat、不 settlement |
| DB01-0004-T20 | 同 T19，但 Recovery 先锁 Attempt 并将其置 lease_expired | heartbeat 等待后返回 stale |
| DB01-0004-T21 | confirm 先锁 Attempt 并提交 cancelled，Recovery 随后运行 | Recovery 跳过该 Attempt；不重复终态转换 |
| DB01-0004-T22 | Recovery 先锁 Attempt 并提交 lease_expired/cancelled，confirm 随后执行 | confirm 返回 stale；不把 lease_expired 改写为 cancelled |
| DB01-0004-T23 | cancel heartbeat 返回后 kill 超过原 Lease 剩余时间 | 不续租；confirm/recovery 竞争结果符合 T21/T22；不得恢复 Executor |
| DB01-0004-T24 | DeathUnconfirmed 且 DB recovery 已将 Job cancelled | DB 终态不被解释为 OS death；目录不清理；runtime/controller 接管 |
| DB01-0004-T25 | 外部 controller 确认 cgroup/container 已销毁 | 记录独立 cleanup 证据后才清理 Attempt 目录 |
| DB01-0004-T26 | DB cancelled 后迟到的 complete/fail/heartbeat/confirm | 全部不能恢复 Job/Attempt 执行或提交资格 |

Supervisor/平台测试另需证明：
- KillOnly 后 Executor 不再收到新命令，IPC/结果通道关闭；
- DB cancelled 不触发目录清理；
- Worker/Supervisor 崩溃后，外部 controller 仍可定位并销毁隔离单元；
- controller 重启、重复回收、隔离单元已不存在、控制面超时均幂等且可审计；
- 只有 death-confirmed 的 Attempt 才允许冻结输出或清理目录；
- 没有 cgroup/container 硬边界的运行环境必须拒绝生产执行，而不是降级为进程组 kill。

原 1,000 轮随机并发矩阵必须增加 heartbeat-expired/cancel、confirm/recovery、heartbeat/recovery 三组交错；每组至少 1,000 轮真实 PostgreSQL 多连接测试，并与 Supervisor 平台集成测试关联同一 Attempt trace。

## 12. 本轮设计裁决与未关闭项

| 项目 | 裁决 | 状态 |
|---|---|---|
| DB cancelled 的含义 | 撤销 DB 调度/提交资格，不代表 OS 进程死亡 | 设计裁决 |
| recovery 是否可先写 cancelled | 仅当 03B-03 外部硬隔离回收契约成立时允许 | 前置条件未验证 |
| Lease 已过期但 Recovery 未锁 Attempt | cancel 请求存在时允许返回 cancel_requested，仅 KillOnly/confirm | 设计裁决，待 SQL/并发测试 |
| Recovery 已将 Attempt 置 lease_expired | 后续 heartbeat/confirm 均 stale，不允许复活 Attempt | 设计裁决，待 SQL/并发测试 |
| confirm 与 recovery | 相同 Attempt→Job 锁序，先提交者决定唯一终态 | 设计裁决，待真实 PG 测试 |
| DeathUnconfirmed 目录 | 保留隔离，禁止清理；外部 controller death-confirm 后清理 | 设计裁决，平台能力未验证 |
| cgroup/container controller | 必须在 Worker/Supervisor 崩溃后仍可回收 | 未关闭，属于 03B-03 实施证据 |

**总裁决仍为 Design Changes Required / Not approved for implementation。** 本补充消除了 DB 终态与 OS 死亡的语义混淆，但没有证明当前部署具备外部隔离单元的独立回收能力，也没有真实 PostgreSQL 并发测试证据。不得据此创建 Migration 0004 SQL。
