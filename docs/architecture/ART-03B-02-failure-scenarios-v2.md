# ART-03B-02 — 故障场景分析 v2

> 状态：**v2 设计基线（待人工评审）**。每场景统一给出：触发 / 检测 / Supervisor 行为 / DB 收敛 / 残留风险 / 覆盖测试。行为引用主设计 §5 状态机与 §6~§10，DB 收敛引用矩阵 D1~D13。
> 阅读前提：Executor 为纯计算单元（主设计 §4.4），一切"输出被写坏/写了一半"的场景**均不产生外部副作用**，只影响本地私有目录。

## S1 Supervisor 崩溃（SIGKILL / OOM / panic）

- **触发**：Worker 进程非优雅死亡；in-flight Attempt 无人照管。
- **检测**：无（本地视角）——由 DB 恢复扫描与 03B-03 容器层收敛。
- **Supervisor 行为**：无（已死）。回收链条：
  - Linux：Executor 经 PDEATHSIG SIGKILL 秒级死亡（孙进程依赖 Executor 的链式 PDEATHSIG，§6.2）；链断裂的孙进程 → 容器 cgroup 回收（生产）/ 残余风险（开发，V1）。
  - Windows：Job 句柄内核关闭 → KILL_ON_JOB_CLOSE 递归杀全树（孙进程无需配合，§6.4）。
- **DB 收敛**：租约到期内无人 heartbeat → 恢复扫描 D10：lease < deadline ⇒ LEASE_EXPIRED（可重试）⇒ retry_wait；lease == deadline（claim 即如此夹逼的极端）⇒ GENERATION_TIMEOUT。cancel 已登记 ⇒ cancelled（D11）。attempts 耗尽 ⇒ timed_out。
- **残留风险**：Supervisor panic 但进程未死（hang 而非崩溃）——由租约安全计时器（INV-1，本地单调钟）覆盖：即使 heartbeat 任务全部卡死，独立计时器仍触发 KillOnly+Abandon。**INV-1 的计时器与 heartbeat 执行路径解耦**（实现要求：select! 多分支，任一 Ready 即行动）。
- **孙进程残留判定**：本场景是 S3 的父集，见 S3。
- **测试**：FS-01。

## S2 Executor 崩溃（非零退出 / 信号异常 / spawn 失败）

- **检测**：waitpid 直接子进程（退出码/信号）。
- **Supervisor 行为**：E7/E8 → fail(EXECUTOR_CRASHED 或 EXECUTOR_PROTOCOL, retryable=false) 一次。fail 返回值走矩阵 T5~T7/C2；Err 走 Uncertain。
- **DB 收敛**：attempt failed / job failed（不可重试——白名单事实 D9；崩溃原因未知，盲目重试会烧 attempt 上限且大概率同因复发）。
- **残留风险**：崩溃瞬间的孙进程成为孤儿（Linux 下 re-parent 到 init/PID 1）：Supervisor 的终止序列**不因 Executor 已死而跳过**——E7/E8 后仍执行组杀（kill(-pgid, SIGKILL)，组以 pgid 为准而非父关系）+ 死亡确认。这是矩阵 §3.1 E7/E8 行 "—" 只表示"无需等待"，终止义务仍在。
- **测试**：FS-02。

## S3 Executor 孙进程残留（树杀不全 / PDEATHSIG 链断裂）

- **触发形态**：(a) 孙进程 setsid 逃逸 pgid（Linux）；(b) Executor 未给某孙进程设 PDEATHSIG 且 Supervisor 死亡；(c) 组杀后死亡确认超时。
- **检测**：死亡确认协议（§6.3）：waitpid + kill(-pgid,0) 迭代 + /proc 成员状态扫描（Z 视为死）/ Windows Job 活跃计数。
- **Supervisor 行为**：
  - (a) 形态下组杀无法命中逃逸者：死亡确认发现组空但 Attempt 曾有孙进程记账（Supervisor 记录 spawn 期 Executor 报告的子进程概况?——否，Supervisor 无法枚举孙进程的孙进程）⇒ **诚实声明**：逃逸检测在非容器环境不可靠，属 V1 残余风险；生产由 03B-03 容器销毁兜底（cgroup 全收）。
  - (c) 死亡确认超时：**不清理目录**（目录可能仍被残留进程写入）、**不声称安全终止**、结构化日志 + OPEN 事项升级（§6.3 hard gate）。
- **DB 收敛**：不受影响（孙进程无凭据无 DB 面）；若因此放弃提交（如 cancel 路径 C 行前置死亡确认失败）→ 恢复扫描兜底（cancel 分支 / LEASE_EXPIRED）。
- **残留风险**：开发环境 CPU/内存被残留进程消耗——watchdog 辅助告警（非唯一保证，合规于任务红线）。
- **测试**：FS-03（故意造 setsid 孙进程，验证日志/不误清理）、FS-04（死亡确认超时注入）。

## S4 DB 慢查询（heartbeat/提交挂起）

- **触发**：锁竞争（长事务持 job 行）、IO 抖动、连接池耗尽。
- **检测**：单次调用 RPC 超时 T=10s（statement_timeout + 总预算，INV-2）。
- **Supervisor 行为**：
  - heartbeat 慢：E12 计入连续失败（K=2 容忍一次；第二次失败时若未触 INV-1 计时器则 KillOnly+Abandon）。**INV-1 计时器不受慢查询影响**（本地单调钟），最坏在 t0+L−M 强杀。
  - complete/fail 慢：Err(超时) → Uncertain 协议（U 行）——探针也慢 ⇒ U5/U6 放弃。
- **DB 收敛**：heartbeat 挂起期间租约仍按 DB 时钟逼近过期；恢复扫描最终以 LEASE_EXPIRED（可重试）收尾。**慢查询不会被"当作 stale 处理后被覆盖写"**：一切放弃路径零 DB 写（矩阵 §3.5-3）。
- **关键参数论证**：T=10 < H=15（慢查询超时必须早于下一拍，否则失败计数失效）；2H+T=40 < L−M=48（INV-2）。
- **测试**：FS-05（pg_advisory 锁注入 + `statement_timeout` 验证）。

## S5 网络中断（Worker ↔ DB 断连）

- **检测**：连接错误 / 心跳 Err。
- **Supervisor 行为**：与 S4 同机制（Err 分类），区别在持续时间更长：连续 K=2 失败 → KillOnly+Abandon；或 INV-1 计时器先行。**中断期间绝不停发 heartbeat 直到触发 K 或 INV-1**（放弃前尽量续租，保住"能提交就提交"的机会窗口）。
- **DB 收敛**：恢复扫描 LEASE_EXPIRED → retry_wait；网络恢复后本 Worker 下一轮 claim 可能重领同一 Job（新 Attempt），正确。
- **反面论证（为何不无条件重试终态提交）**：断连时 complete 可能已落地；盲目重试若 lease 已失效则 stale（无害），但若 lease 仍有效且第一次实际落地（两阶段不一致窗口——DB 侧函数单事务原子，落地即终态 ⇒ 重试必 stale，**数据库自身保证**）；真正的风险在"把网络错误当 stale"从而错过仍可提交的结果——探针协议 U1 恰好覆盖：`renewed` ⇒ 补交。两端都闭合。
- **测试**：FS-06（代理/防火墙注入断连）。

## S6 deadline 与 cancel 竞争

- **场景**：heartbeat 恰在 deadline 边界，同时 API 已登记取消。
- **DB 裁决**（heartbeat 判定顺序，D2）：deadline 检查先于 cancel 检查 ⇒ 返回 `deadline_exceeded`，DB 按 timed_out/GENERATION_TIMEOUT（可重试）收尾；**cancel_requested_at 仍留在行上但 Job 已离开 running** ⇒ 取消不再生效，Job 走重试——这是数据库的既定裁决（用户请求取消但执行到时限，系统按超时+重试处理），Supervisor 不再对 cancel 做任何事（E3 零写）。反之 heartbeat 先到且未过 deadline ⇒ `cancel_requested`（E2）⇒ CancelSequence ⇒ C1 cancelled。
- **两路径互斥且有 DB 串行化保证**（行锁 FOR UPDATE），Supervisor 侧无竞争代码。
- **测试**：FS-07。

## S7 complete 响应丢失（提交已落地 / 未落地）

- **场景**：complete 事务成功但响应丢失（连接 RST）；或请求未达。
- **Supervisor 行为**：Uncertain（U 行）：
  - 落地 ⇒ 探针 heartbeat = `stale`（attempt 已 succeeded）⇒ U4 放弃。**DB 终态 = succeeded = 目标态** ✔
  - 未落地 ⇒ 探针 `renewed` ⇒ U1 补交 ⇒ succeeded ✔
  - deadline 已过且未落地 ⇒ U3（DB 超时收尾，放弃）✔
- **错误重试风险（任务 §九-5）**：不存在"响应丢失 → Job 被错误重试"路径：重试只可能来自恢复扫描/白名单码，而 succeeded 落地后 attempt 已终态、active 唯一索引清位，恢复扫描不再触碰（只扫 claimed/running）。**唯一损失是可观测性**（U4 二义），OPEN ISSUE / DR-7。
- **测试**：FS-08（pg 层注入：提交后杀连接）。

## S8 fail 响应丢失

- 同 S7 结构：落地 ⇒ 探针 stale ⇒ 放弃（DB 已 failed/retry_wait = 目标态）；未落地 ⇒ renewed ⇒ 补交。
- 与 S7 的差异：fail 未落地且探针也失败 ⇒ 恢复扫描 LEASE_EXPIRED（可重试）——比"未提交的 failed（不可重试）"**更宽容**，不会造成错误终态，只会多重试一次，安全方向正确。
- **测试**：FS-09。

## S9 shutdown 排水超时

- **场景**：SIGTERM 后 in-flight Attempt 排不完（Executor 还在算）。
- **Supervisor 行为**（主设计 §10）：预算 = min(30s, INV-1 余量, deadline 余量)；预算耗尽 → KillOnly（宽限动态压缩进预算）→ 死亡确认 → **Abandon（零 DB 写）** → 退出码 0 + 结构化日志。
- **DB 收敛**：租约过期 → LEASE_EXPIRED 可重试 → retry_wait（语义损失为零，最多退避 5s 后重跑）。
- **不允许的形态**（对照任务红线）：排水期间停止 heartbeat（INV-6 违反 ⇒ 租约在排水中流失 ⇒ 本可完整提交的结果被强制放弃）；只等租约过期不主动终止（违反"先停后失约"INV-1）。
- **测试**：FS-10。

## S10 进程树无法确认死亡

- **场景**：SIGKILL 组杀后 5s 内 kill(-pgid,0) 仍非 ESRCH 且存在非 Z 成员 / Windows Job 活跃计数 > 0。
- **Supervisor 行为**（§6.3 hard gate）：不声称安全终止；不清理目录；不因该状态改变 DB 路径（cancel 路径的 confirm **前置死亡确认**——确认失败则**不调用 confirm**，转零写放弃，取消终局由恢复扫描 cancel 分支（D11）兜底）；结构化日志 + 升级。
- **设计立场**：宁可 Job 多退避一次（可重试收敛），不冒"对活树宣称已死/删掉在写目录"的风险。**"不能确认整树已停止 ⇒ 不能声称 Attempt 已安全终止"为硬规则**。
- **测试**：FS-04（注入拒死进程：SIGSTOP 状态成员对 SIGKILL 免疫？——SIGKILL 不可阻塞，改用 D 状态（不可中断 IO）进程模拟，接受平台差异）。

## S11 磁盘空间不足

- **场景**：Executor 写 result/output 中途 ENOSPC；或 Supervisor 建目录失败。
- **Supervisor 行为**：Executor 自陷（非零退出或 result(failed)）→ E8/E6 正常 fail 路径；ERROR 码不在白名单 ⇒ 不可重试（数据面故障保守语义，§8）。Supervisor 建目录失败 → 不 spawn，直接 fail(EXECUTOR_CRASHED)。
- **DB 收敛**：failed（终态）。
- **残留风险**：失败结果本身要写日志/诊断——日志管道限流（§4.3）防日志二次爆盘。
- **测试**：FS-11（小配额 tmpfs / 磁盘满注入）。

## S12 临时目录清理失败

- **场景**：Settled 后删除目录 Err（Windows 文件占用 / 权限 / 残留进程持句柄）。
- **Supervisor 行为**：重试 N（默认 3，指数退避）次失败后：**放弃清理并告警**（不影响主流程——目录名 UUID 隔离，不会被复用，不会影响后续 Attempt 正确性，只占空间）；记录到待清理清单；开发环境由 7 天阈值启动清理兜底（§8）；生产由容器 tmpfs 生命周期兜底（03B-03 契约）。
- **不因清理失败而重提交或改变状态机路径**（清理是 Settled 之后的动作，不可逆 precede）。
- **测试**：FS-12（Windows 锁定目录句柄注入）。

## S13（补充）时钟相关

- **本地墙钟跳变**（NTP 修正/手动改时）：所有安全计时用单调钟（INV-1/INV-5）；deadline 差值用 DB 下发两时刻之差（D1 deadline_at − started_at），绝对墙钟只做显示。跳变不影响任何裁决。
- **DB 时钟与本地时钟偏差**：所有"到期"判定以 DB 时钟为准（函数内 clock_timestamp）；本地仅做保守方向的提前量（§7.2 INV-1 论证：以响应本地接收时刻为基准偏保守）。偏差被 M 吸收。
- **测试**：FS-13（libfaketime 不可用于单调钟；用注入时钟抽象验证）。

## 覆盖矩阵（场景 × 测试）

| 场景 | 测试 ID | 层级 |
|---|---|---|
| S1 Supervisor 崩溃 | FS-01 | 真实进程树（Linux+Windows） |
| S2 Executor 崩溃 | FS-02 | 真实进程树 |
| S3 孙进程残留 | FS-03 / FS-04 | 真实进程树 |
| S4 DB 慢查询 | FS-05 | PG 集成 |
| S5 网络中断 | FS-06 | PG 集成 |
| S6 deadline/cancel 竞争 | FS-07 | PG 集成 |
| S7 complete 响应丢失 | FS-08 | PG 集成 |
| S8 fail 响应丢失 | FS-09 | PG 集成 |
| S9 排水超时 | FS-10 | 集成 + 进程树 |
| S10 死亡确认失败 | FS-04 | 真实进程树 |
| S11 磁盘不足 | FS-11 | 真实进程树 |
| S12 清理失败 | FS-12 | 真实进程树（Windows 优先） |
| S13 时钟 | FS-13 | 单元（时钟抽象） |

（测试 ID 定义见 `ART-03B-02-test-strategy-v2.md`。）
