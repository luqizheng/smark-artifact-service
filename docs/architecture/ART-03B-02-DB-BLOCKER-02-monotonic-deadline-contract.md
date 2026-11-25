# ART-03B-02 — DB-BLOCKER-02：短 Deadline 单调时钟契约设计

> 状态：**Design proposal / 待独立评审；未批准实施**。
> 范围：定义 DB deadline/lease 时间如何安全映射到 Worker 本机 monotonic clock。
> 本文不修改 Migration，不提供可直接执行 SQL；不代表批准创建新 Migration。
> 归属：ART-03B-02 架构收敛工作流第 2 步。

## 1. 问题

Migration 0003 的 claim 返回 `lease_expires_at` 与 `deadline_at`（timestamptz），但没有返回可供 Worker 可靠换算的 DB 时间锚点或剩余时长。Worker 不能用 deadline_at 减 started_at，因为 started_at 不在 claim 返回结构中；也不能直接把 DB timestamptz 与本机 wall clock 比较，除非额外定义并强制最大时钟偏差。

当 `max_execution_seconds=1` 时，DB 会把 lease 截短至 deadline。若 Supervisor heartbeat 周期为 15 秒，则“下次 heartbeat 才发现 deadline”显然太迟。短 deadline 必须在 Executor spawn 前就映射为本机单调时钟截止点，并且每次续租都重新映射 lease/deadline。

## 2. 推荐方案：DB 返回剩余时长，Worker 用 RPC-send monotonic 锚点

**不传递 DB 绝对时间给 Worker 参与时钟比较。由 DB 在受控函数内计算剩余时长，Worker 以发出该 RPC 前记录的 monotonic instant 为保守锚点。**

对任意返回的 DB 截止点 D，数据库在函数接近返回时刻 t_db 计算：

`remaining_ms = max(0, floor((D - clock_timestamp()) * 1000))`

Worker 在发起 RPC 前记录本机单调时钟 `t_send`。响应收到时记录 `t_recv`。本机保守截止点定义为：

`local_cutoff = t_send + remaining_ms - safety_margin_ms`

关键证明：

- DB 的剩余时长从 DB 计算时刻 t_db 起算；
- t_db 不早于 Worker 记录的 t_send；
- `t_recv - t_send` 包含请求传输、DB 执行、响应传输和本地排队；
- 因而用 `t_send + remaining_ms` 作为本机截止点，会把 RPC 请求/响应耗时全部扣除，结果偏早而非偏晚；
- 不依赖 DB 与 Worker 的 wall clock 同步，也不依赖时区或 NTP 偏差。

**禁止**使用响应接收时刻加 remaining_ms；那会把 RPC 往返时间加回可用执行窗口。

## 3. DB 受控函数返回契约

### 3.1 claim

新的 claim 结果至少应包含：

| 字段 | 含义 |
|---|---|
| lease_remaining_ms | claim 函数完成关键 DB 写入后，距当前 lease_expires_at 的剩余毫秒数 |
| deadline_remaining_ms | 同一计算时点距 deadline_at 的剩余毫秒数 |
| lease_expires_at | 可保留用于日志/审计展示，不用于 Worker wall-clock 比较 |
| deadline_at | 可保留用于日志/审计展示，不用于 Worker wall-clock 比较 |
| attempt_id / token / job 等 | 保持原 claim 语义 |

要求：
- 使用 `clock_timestamp()`，不能用事务开始时固定的 `now()` / `transaction_timestamp()` 代替实际剩余时长。
- 剩余时长应在 Attempt/Job 写入完成、函数返回前计算，避免锁等待和 SQL 处理时间被漏算。
- 如果函数执行期间 lease/deadline 已到，返回 0，不允许返回正数。
- duration 使用整数毫秒或更细的整数微秒；单位必须在字段名/协议中明确，禁止浮点秒造成舍入歧义。
- DB 结果不允许出现负数；Worker 将 0 视为立即停止/不可安全启动。
- claim 的时间预算从 claim RPC 的 send monotonic instant 起算，覆盖网络、DB、响应、Supervisor 排队和后续 spawn 准备。

### 3.2 heartbeat

heartbeat 不能只返回 text。新的受控返回结构至少应表达：
- `outcome`: renewed / cancel_requested / deadline_exceeded / stale；
- `lease_remaining_ms`: renewed 时距新 lease expiry 的剩余时长；
- `deadline_remaining_ms`: renewed 时距 Attempt deadline 的剩余时长。

语义：
- cancel_requested：不再延长 lease（与 DB-BLOCKER-01 推荐契约一致），Supervisor KillOnly；不把该结果解释为新的执行窗口。
- deadline_exceeded：DB 已完成 timeout settlement；Worker KillOnly + 零 DB 写。
- stale：不再写 DB；停止 Executor 并等待权威恢复。
- renewed：两个剩余时长都必须由同一次受控 DB 函数计算并返回；Worker 分别映射 lease 与 deadline 的本机截止点。
- 任何非 renewed outcome 的剩余时长字段为 NULL 或 0，具体形式由协议 schema 固定，不得被误用。

如果为了兼容现有 SDK 需要保留旧 heartbeat 函数，可以新增版本化函数名/返回类型；不得在不审查调用方的情况下改变已有函数的返回类型或签名。

### 3.3 complete / fail / confirm

这几个提交函数仍由 DB 自己判断 lease/deadline/cancel 的权威状态。Worker 的本机单调截止点只是“何时必须停止执行/不再尝试”的安全边界，不取代 DB token、状态与 lease 检查。

任何客户端认为自己尚有本地剩余时间，都不代表数据库一定接受提交；DB 返回 stale 仍是最终裁决。

## 4. Worker 本机算法

对 claim/heartbeat 每次 RPC：

1. 在 RPC 调用前，立即读取 `Stopwatch.GetTimestamp()` 或等价 monotonic clock，保存 `t_send`。
2. 发起 RPC；禁止并行发送多个 heartbeat。
3. 收到响应后立即读取 monotonic clock `t_recv`。
4. 验证返回结构、单位、范围、outcome 与 attempt_id 对应关系。
5. 计算：
   - `lease_cutoff = t_send + lease_remaining - safety_margin`
   - `deadline_cutoff = t_send + deadline_remaining - deadline_guard`
   - `effective_cutoff = min(lease_cutoff, deadline_cutoff)`
6. 若 `t_recv >= effective_cutoff`，立即 KillOnly，不启动或继续 Executor。
7. 否则只允许执行到 `effective_cutoff` 前；Supervisor 的本地 stop timer 必须基于 monotonic clock。
8. 所有 spawn、输出冻结、终止、死亡确认和提交准备时间都消耗同一 Attempt 时间预算；不得只限制 Executor 计算时间而遗漏启动/回收时间。
9. 任何系统暂停/调度延迟导致恢复执行时已越过 cutoff，立即 KillOnly。
10. 不允许因 heartbeat 响应较晚而重置截止点为 `t_recv + remaining`。

### 4.1 时长计算的舍入

数据库应向下取整剩余时间（floor），Worker 再扣除明确的 safety margin。禁止四舍五入增加可用时间。若协议用微秒，跨语言转换也必须向下取整。

### 4.2 进程启动门槛

claim 成功不等于一定可以 spawn：
- 若剩余 deadline 不足以覆盖 spawn 上界 + 最小执行窗口 + 终止/死亡确认预算，Supervisor 必须不启动 Executor；
- 该情况应使用稳定的“未启动/本地预算不足”处理路径，但不能擅自把它伪装为 GENERATION_TIMEOUT 并触发 retry，需在后续 FSM 设计中明确 DB settlement 语义；
- 不得在剩余时间为 0 或已越过 cutoff 时创建进程。

## 5. 时序参数与安全边际

必须把不同预算分开，不能把所有风险混入一个模糊的 M：

- `R_rpc`: RPC 请求/响应最大允许耗时；
- `H`: heartbeat 调度间隔；
- `K`: 连续 RPC timeout 次数阈值；
- `J`: 调度抖动/线程饥饿上界；
- `S_spawn`: 创建进程、配置 Job/cgroup、Resume 前置准备的上界；
- `S_kill`: 发出终止到平台确认进程树死亡的实测上界；
- `S_freeze`: 冻结输出快照与提交前本地准备的上界；
- `G_deadline`: DB 时间换算、毫秒精度、调度延迟等额外保护量。

对于执行中租约失联检测，固定 Delay 调度若每次 RPC timeout 后再 Delay H，至少满足：

`K × (H + R_rpc) + J + S_kill + G_deadline < lease_duration`

如果请求调度改为固定频率 tick，必须单独证明不会因 tick 漏失而推迟下一次发送，不能直接复用上述公式。

对 deadline 本身，Supervisor 应直接依据 `deadline_cutoff` 本地停止，不依赖 heartbeat 周期；heartbeat 是 DB 状态协调与租约续期，不是短 deadline 的唯一停止机制。

所有参数必须基于实际运行时与目标 OS 测量，不能只给理论值。旧值 L=60/H=15/T=10/K=2/M=12 已由 v2.1 复审证明不成立，不得沿用。

## 6. 时钟与运行时前提

- Worker 使用**暂停感知的单调时钟**，不得默认 .NET Stopwatch/QueryPerformanceCounter/CLOCK_MONOTONIC 在所有平台都包含系统睡眠时间；不得用 DateTime.UtcNow/系统 wall clock 计算 cutoff。
- 平台适配层必须证明时钟在 VM suspend、宿主睡眠期间仍推进，或具备可靠的 resume 检测并在恢复时强制 KillOnly。Linux 优先评估 CLOCK_BOOTTIME；Windows 选择包含低功耗睡眠时间的单调计时源（例如 GetTickCount64），并验证其分辨率/回绕语义。未通过验证的平台不得运行生成 Executor。
- Supervisor 进程内所有 deadline、lease、RPC timeout、shutdown budget 使用同一暂停感知 monotonic 时间域。
- monotonic clock 进程重启后不可恢复；Supervisor 崩溃后不得恢复旧 Attempt 的本地执行资格，只能由 DB lease/recovery 处理。
- VM suspend/resume、宿主机睡眠、容器暂停必须有平台级测试；不能证明计时持续推进时，恢复后无条件 KillOnly，不得继续 Executor。
- 不假设 NTP 无偏差；DB wall clock 只用于 DB 自身生成 deadline 与剩余时长。
- DB 主机时钟必须保持单调合理性（系统时钟大幅回拨/跳跃仍可能影响 timestamptz deadline）；这属于 DB 平台时钟运维约束，应记录为基础设施风险。duration 协议解决 DB 与 Worker 之间的时钟映射，不解决 DB 自身时钟回拨。

## 7. RPC 不确定与剩余时长

- claim RPC 传输不确定：Worker 可能不知道 DB 是否已经创建 Attempt；不得重试 claim 并假设未领取。该路径由 FSM-BLOCKER-05 处理。
- heartbeat RPC 传输不确定：不能使用上一次剩余时长无限续命；只允许按本地既有 cutoff 运行，直到保守截止点；达到 cutoff 立即 KillOnly。是否发一次探针由 FSM/Lease 协议约束。
- 响应字段缺失、单位不符、负值、溢出、attempt_id 不匹配或无法解析：视为协议错误，KillOnly + 零 DB 写。
- heartbeat 超时不能把旧 lease cutoff 向后延长。
- 若响应在网络中延迟至本地 cutoff 之后才到达，即使 outcome=renewed，也不得恢复 Executor；新租约在 DB 端存在，但当前 Supervisor 已经无法证明其安全时间窗口。应停止本 Attempt，后续由恢复机制处理。

## 8. Migration 与版本边界

DB-BLOCKER-01 与 DB-BLOCKER-02 必须分开评审：
- 0004 候选只解决 deadline × cancel 仲裁；
- 时间契约候选可作为后续独立 Migration（例如 0005）设计，或在迁移编排上证明两者可安全组合，但不得把两个语义变更混为一个不可独立回滚的改动。
- 历史 0001/0002/0003 不得重写。
- 新函数需显式设置 SECURITY DEFINER search_path、函数 owner、PUBLIC revoke、Worker role EXECUTE grants。
- 旧函数签名兼容与调用方升级顺序必须明确；不允许旧 Worker 把新结构误读为原 text outcome。
- `migrations.sha256` 只在新 Migration 经过审查并进入正式迁移序列时更新。

## 9. 必须具备的验收测试

| ID | 场景 | 预期 |
|---|---|---|
| DB02-T01 | DB 返回 1000ms，RPC 总耗时 100ms | 本机 cutoff 不晚于 send+900ms-safety |
| DB02-T02 | RPC 响应耗时接近剩余时长 | 收到响应时 cutoff 已过，立即 KillOnly，不 spawn/恢复 |
| DB02-T03 | DB 与 Worker wall clock 故意偏移 ±数小时 | monotonic 映射结果不受偏移影响 |
| DB02-T04 | DB 函数内部锁等待/执行耗时增加 | 返回剩余时长正确扣除函数执行时间 |
| DB02-T05 | 剩余时长为 0 | 不创建 Executor |
| DB02-T06 | 1 秒 deadline，claim RPC 慢于预算 | 不 spawn，按确定的本地预算不足路径收尾 |
| DB02-T07 | heartbeat 返回新的 lease/deadline duration | 以该请求 send monotonic instant 建立 cutoff，不以 receive instant |
| DB02-T08 | heartbeat 超时后旧 cutoff 到达 | KillOnly；不得继续等待下一个 heartbeat |
| DB02-T09 | heartbeat 响应字段缺失/单位错/溢出/Attempt 不匹配 | 协议错误，KillOnly + 零 DB 写 |
| DB02-T10 | VM suspend/resume 或进程恢复 | 按平台策略保守停止，不延长截止点 |
| DB02-T11 | 计算期间线程饥饿/调度抖动 | 证明 stop timer 延迟有界；超界视为平台不满足支持条件 |
| DB02-T12 | Worker 崩溃后重启 | 不恢复旧 monotonic cutoff/旧 Attempt 执行资格 |

## 10. 未决但必须在第 3 步解决的问题

1. `deadline_remaining_ms` 的计算时点必须在 DB 函数所有关键更新之后；还需确认事务提交/网络发送延迟由 Worker send-anchor 保守扣除。
2. 取消分支是否完全停止续租，必须与 DB-BLOCKER-01 的 Migration 设计一致。
3. claim 成功但剩余时间不足以 spawn 时，如何安全收尾而不制造错误重试，需要在 Supervisor FSM 里明确。
4. RPC timeout、heartbeat interval、lease duration、kill budget、scheduler jitter 的数值需要统一推导和实测。
5. monotonic clock 在 Windows/Linux、VM suspend、容器暂停下的语义需要目标运行环境测试。
6. DB wall clock 自身发生大幅回拨/跳跃时的风险仍存在；应定义 PostgreSQL 主机时钟运维告警与故障策略。

## 11. 关闭标准

DB-BLOCKER-02 只有在以下条件全部满足后才能标记 Closed：

1. claim/heartbeat 的版本化响应 schema 明确返回 lease/deadline remaining duration；
2. DB 计算时点、精度、舍入、0 值和 NULL 语义完整；
3. Worker 统一使用 monotonic send-anchor 算法，不使用本机 wall clock；
4. RPC 往返耗时、锁等待、调度抖动、spawn/kill 预算均纳入证明；
5. 1 秒 deadline 不足以安全启动时有明确拒绝/收尾路径；
6. 跨 OS、VM suspend/resume 和 Worker crash 测试通过；
7. 版本升级与函数权限迁移方案通过独立评审。

**当前结论：已给出可实现的时间契约候选，但 DB-BLOCKER-02 尚未关闭，未批准 Migration 或代码实施。**
