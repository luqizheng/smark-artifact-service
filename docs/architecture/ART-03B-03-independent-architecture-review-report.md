# ART-03B-03 — Runtime Isolation & Recovery Controller 独立架构复审报告

> 复审日期：2026-11-25  
> 复审对象：PR #6 / `docs/art-03b-03-runtime-isolation-design`  
> 复审基线：PR head 在复审开始时为 `e166289aa4b2989fdaf273581b7d8ed6fbb96059`；复审修订随后追加到同一 Draft 分支。  
> 范围：设计内部一致性、与 ART-03B-02 v2.2 / DB-BLOCKER-01 的跨层契约、当前 main 的 SQL/部署事实。  
> 裁决：**Changes Required / Not approved for implementation。**

## 1. 结论摘要

推荐方向仍然合理：独立节点级 ARC + 每 Attempt 一个 OCI sandbox + cgroup v2；Worker 不持有 runtime socket；Executor 无网络、无业务凭据；DeathConfirmed 前不读取输出、不提交、不清理。

但原稿存在三个实质性缺口，不能批准实施：

1. **P0 — ARC 自身故障没有独立的有界回收者。** ARC 是唯一 runtime 控制者。ARC crash/hang 时，containerd/cgroup 不会自动终止 Executor；“ARC 重启后 reconciliation”没有给出 ARC 不重启、重启卡住或服务管理器失效时的上界。原稿据此声称 bounded observation window 不成立。
2. **P0 — DB authority validation 与 container Start 不可能原子化。** “DB 状态撤销后不得接受任何新 Start”与“DB commit 和 runtime kill 之间存在 observation window”存在语义冲突。验证 active 后、Start 前 DB cancel 仍可提交，Start 仍可能短暂成功。必须定义短时 StartPermit、in-flight Start 语义及完整窗口；不能宣称跨系统瞬时 fencing。
3. **P1 — durable journal 的崩溃一致性协议不够具体。** 原稿只提出 journal + lock/CAS，没有规定 runtime 副作用前后的 write-ahead intent、fsync/父目录持久化、runtime labels 对账、磁盘满/损坏/只读文件系统处理及 tombstone 保留期。仅写“durable”不足以证明 Create/Start/Kill 的 crash consistency。

因此本轮在设计文档中增加：
- §7.5 Node Runtime Guardian（NRG）独立 KillOnly fail-safe 及其故障假设；
- §7.6 journal write-ahead/fsync/reconciliation 协议；
- §9.2 StartPermit 与 DB→runtime 不可原子竞态语义；
- ARC-T38–T45 故障注入验收项；
- §14 新增阻断记录。

NRG 是新增架构依赖，尚未选型、实现或实测，不得把它视为现有能力。

## 2. 代码与仓库事实核验

复审重新读取了 PR #6 文档、PR #4 v2.2 Supervisor 设计与独立报告、PR #5 DB-BLOCKER-01 设计与独立报告，以及 main 的 README、Worker Compose、Migration 0002/0003。

确认事实：
- main 的 Worker 是独立协调维护进程；当前只做 lease recovery / input purge，不安装 Node/SeeCode、不领取 Generator Job。
- `deploy/docker-compose.worker.yml` 明确声明 ART-03B-03 的只读根文件系统、无外网、资源配额尚未完成。
- Migration 0003 的 heartbeat 在原逻辑中先处理 deadline，再处理 cancel；claim 要求 cancel_requested_at IS NULL。
- Migration 0003 的 `recover_expired_generation_jobs` 会在 lease 过期后推进 Attempt/Job 状态；它不具备 OS/container death proof。
- PR #4 与 PR #5 都仍是 Draft 设计提案；它们没有给 ARC/NRG 提供已实现的 DB authority API 或真实 PostgreSQL 并发证据。
- LMMS delegation 与 File Service finalize 仍是 Artifact 正式提交的独立生产 P0，ARC 设计不能替代或绕过。

## 3. 发现详情

### AR-03B-03-01 — P0：ARC 单点故障使 bounded recovery 无法证明

**问题**

ARC 独立于 Worker 是必要条件，但不充分。ARC 自身崩溃、永久挂起、事件循环阻塞、runtime socket 卡死或节点服务管理器未及时重启时，已有容器仍可能继续运行。容器不会因 ARC 进程退出而自动停止。

原稿只描述 ARC 重启后的 reconciliation，没有提供在 ARC 不恢复时仍独立运行的 kill-only 机制，也没有给出 watchdog/restart/kill 的最坏时间上界。因此最大 observation window 的公式漏掉了一个可能无界的故障分支。

**风险**
- DB 已撤销执行资格，但 Executor 仍在 sandbox 内消耗资源；
- 同时 Worker crash 时，没有组件可以保证在规定时间内发起 kill；
- “有界回收”与“节点不得 Ready”缺少可验证闭环。

**必须修正**
- 增加独立于 ARC/Worker 的 Node Runtime Guardian（NRG），只拥有 Attempt cgroup 范围内的 KillOnly 能力；
- ARC 向 NRG 续租控制心跳；失联超过阈值后 NRG 禁止新 Start 并 kill 全部受管 Attempt；
- NRG 由独立节点服务管理器/watchdog 监督；把 ARC failure detection、NRG watchdog/restart、NRG kill latency 纳入 observation window；
- NRG/权限/cgroup 根范围不健康时节点不得 Ready；kill 无法确认时 Quarantined；
- 注入 ARC 永久挂起、ARC+Worker 同时 crash、NRG crash/watchdog failure。

**状态：已写入设计修订；未实现、未实测，P0 仍开放。**

### AR-03B-03-02 — P0：DB authority check 与 container Start 存在不可消除的跨系统竞态

**问题**

PostgreSQL validation 与 containerd Start 不共享事务。以下时序不可由“Start 前再验证一次”排除：

1. ARC 验证返回 active；
2. DB cancel transaction commit；
3. ARC 的 Start 请求抵达 runtime 并启动 Executor。

因此原稿“DB 状态撤销后不得接受任何新 Start”不能被解释为 DB commit 后瞬间生效。它与原稿承认的 observation window 不一致。

**风险**
- 设计声称的 Start fencing 强于系统可提供的保证；
- cancel 与 Start 交错时，可能出现 DB 已取消但 Executor 短暂启动；
- 如果该短暂执行可访问外部副作用，DB 状态与真实执行将分裂。

**必须修正**
- 明确 DB commit 只立即撤销 DB 侧结果提交/续租权限；
- Start 使用绑定 Attempt、session、runtime identity、generation、expiry、nonce 的短时 StartPermit；
- Permit 过期、DB authority unknown/timeout 或观察到 revoke 后禁止新 Start/Resume/Exec；
- 允许已签发 Permit 与 cancel commit 的在途竞争，但必须纳入最大 observation window；
- ARC 先持久化 KillRequested fencing；迟到 Start completion 不得覆盖新 generation；
- 无法证明 runtime API 顺序时由 NRG kill-only fencing 收敛；
- Executor 在窗口内必须保持无网络、无凭据、无业务副作用能力。

**状态：已写入设计修订；需要独立 DB 契约与 runtime 并发测试，P0 仍开放。**

### AR-03B-03-03 — P1：ARC journal 的 crash consistency 缺少可执行持久化协议

**问题**

per-Attempt journal + CAS 只是抽象。原稿没有定义在 runtime create/start/kill 前后何时持久化 intent、fsync 是否成功后才调用 runtime、runtime label 如何与 journal 对账，以及磁盘满、fsync error、journal corruption 时如何处理。

**风险**
- runtime 已创建但 journal 无记录；
- KillRequested 只存在于页缓存，ARC crash 后 fencing 状态丢失；
- journal 损坏时错误恢复/重复创建/误清理；
- tombstone 过早清理后旧 RPC 重放可再次创建资源。

**必须修正**
- runtime side effect 前 write-ahead intent + fsync（必要时父目录 fsync）；
- runtime labels 绑定 operation_id / Attempt identity / generation；
- runtime response 后 inspect，再持久化 observed resource ID 并 fsync 后才回复成功；
- KillRequested 先持久化并 fsync；fsync 失败时 NRG kill-only + node quarantine；
- journal 损坏、磁盘满、label 缺失/重复、版本不兼容一律 fail closed；
- tombstone 保留覆盖最大重试与 reconciliation 窗口。

**状态：已写入设计修订；需真实故障注入验证，P1 仍开放。**


### AR-03B-03-04 — P0：NRG 必须能撤销 Start admission，单纯 KillOnly 不足以 fencing 在途 Start

**问题**

NRG 被定义为 KillOnly 组件，但设计同时要求它“禁止节点新 Start”。如果 NRG 只有 cgroup.kill 权限，却不能撤销 ARC 对 containerd 的 Start 控制通道，那么 ARC 可能存在已发送但尚未完成的 Start RPC；NRG kill 后，迟到的 Start 仍可能启动容器或复活 task。

**必须修正**
- NRG 需要最小化的 Start-admission revoke 权限，但不得获得创建/启动 sandbox 的权限；
- 每次 Start 绑定当前 NRG admission generation，不能跨 generation 重放；
- NRG 失联时先撤销 ARC runtime control channel/Start admission，再处理所有 in-flight Start 和 cgroup kill；
- 对无法确认是否存在迟到 Start 的 Attempt 保持 Quarantined；不能只看一次 cgroup populated=0；
- 增加 NRG revoke 与 in-flight Start 响应乱序的真实 runtime 故障注入测试。

**状态：已写入设计修订；NRG 与 runtime control channel 的具体实现尚未设计/实测，P0 仍开放。**

## 4. 跨 PR 依赖一致性

### 与 PR #4（ART-03B-02 v2.2）

PR #4 已把 DeathUnconfirmed 定义为不 confirm、不提交、不清理，并规定 Worker/Supervisor crash 后由外部 controller 回收。PR #6 的 ARC 方向满足这一职责拆分，但在增加 NRG 前，ARC 自身 crash 路径仍未满足“外部、独立、有界回收”。

### 与 PR #5（DB-BLOCKER-01）

PR #5 允许 DB recovery 先撤销 Job/Attempt 的执行/提交资格，前提是 03B-03 已提供独立于 Worker 的硬隔离和有界回收。当前 Migration 0003 recovery 只改变 DB 状态，不证明 OS 进程死亡；PR #6 必须把 NRG 纳入该前置条件。不能把 DB cancelled 解释成 DeathConfirmed。

## 5. 修订后仍未解决的实施前置条件

| 项目 | 当前状态 | 关闭证据 |
|---|---|---|
| NRG 方案与权限边界 | 新增设计候选 | 独立安全设计审查、KillOnly + Start admission revoke 最小权限验证 |
| ARC/NRG/节点管理器故障上界 | 未知 | 目标节点实测与最坏时序证明 |
| StartPermit DB 契约 | 未设计 | DB-BLOCKER-02/独立 SQL 审查、nonce/expiry/generation 并发测试 |
| ARC journal 实现语义 | 已补设计要求 | fsync/磁盘故障/响应丢失/重启故障注入 |
| Linux kernel/cgroup/containerd 版本矩阵 | 未冻结 | 目标生产节点选型与兼容性测试 |
| cgroup death predicate | 仅候选要求 | fork/setsid/double-fork/cgroup migration 真实测试 |
| ART-03B-02 / DB-BLOCKER-01 | 仍为 Draft | 各自独立评审、实施和真实 PostgreSQL/平台测试 |
| LMMS delegation / File Service finalize | 生产 P0 | 上游服务契约和端到端验证 |

## 6. 最终裁决

**Changes Required / Not approved for implementation。**

本轮确认 ARC + OCI + cgroup v2 是合理的目标方向，但当前不能批准实施。新增 NRG、StartPermit 语义和 journal write-ahead 协议只是设计修订，不是实现证据。NRG 如何可靠撤销 ARC 的 in-flight Start admission 仍是 P0 设计与实测门禁。

PR #6 必须保持 Open / Draft。不得创建 Worker/ARC/NRG 业务代码、Migration、Compose 或生产 Executor 启用开关；先对修订后的设计做第二轮独立复审。


## 7. 第二轮独立复审（2026-11-25）

第二轮裁决：**Changes Required / Not approved for implementation。** 首轮修订改善了问题描述，但仍未把 NRG admission revoke 落到可验证的权限/内核执行机制；同时发现时间锚定与死亡证明契约不完整。

### AR-03B-03-05 — P0：NRG Start-admission revoke 没有可执行的强制执行点

**问题**

当前拓扑仍让 ARC 直接持有 containerd socket 与 cgroup 管理权限，同时要求 NRG 在 ARC 失联时撤销 ARC 的 Start admission。关闭 UDS、关闭客户端 socket、更新 generation 或发送 gRPC cancel，都不能证明已经进入 containerd 的 Start 请求不会迟到生效。

NRG 若只有 KillOnly 权限，就不能撤销 ARC 的 runtime socket 能力；若 NRG 直接拥有 runtime socket，则它已不再是 KillOnly 组件。首轮设计没有裁决这两者的权限冲突。

**必须修正**
- 在 §7.7 明确两种可验证候选：A) NRG 独占 cgroup freeze/kill execution fence，ARC 保留受限 runtime API；B) NRG 成为唯一最小权限 runtime broker，ARC 不持 runtime socket。
- 方案 A 必须证明 Executor 在第一条用户态指令前已进入 NRG 可 freeze 的 Attempt cgroup；NRG freeze 确认后，即使迟到 Start 创建 task，也不能执行；保持 frozen 直到 runtime task 删除与所有在途 Start 收敛。
- 方案 B 必须证明 Start/revoke 在同一持久化 generation 状态机线性化，并处理已发给 containerd 的在途 RPC；不能只依赖 gRPC cancel。
- 未选定并在目标 containerd/OCI runtime 上完成并发原型前，NRG revoke 不得被视为已设计完成。

**状态：已写入 §7.7 候选方案；P0 仍开放。**

### AR-03B-03-06 — P0：cgroup populated=0 不能单独证明未来不会出现迟到 task

**问题**

原 DeathConfirmed predicate 包含 runtime task exited、cgroup populated=0 和 ARC 已撤销 Start，但未把“所有可创建 task 的在途操作已收敛”变成强制证据。一次 populated=0 只是瞬时观察；如果 Start RPC 仍在 runtime server/shim 内处理，后续可能创建 task。

**必须修正**
- NRG 先 freeze cgroup，并等待内核 frozen 事件；
- 撤销 StartPermit/admission generation；
- 证明所有 runtime Start 操作已完成、明确失败或不可再提交；
- 删除 OCI task/container，禁用 restart policy 与外部自动重建；
- 再执行 cgroup.kill，确认 populated=0，确保旧操作不能重新填充 cgroup；
- 任一条件未知即 DeathUnconfirmed/Quarantined；保留 frozen cgroup，不读输出、不提交、不清理。

**状态：已写入 §7.8 与 ARC-T46/T48；仍需真实 runtime 证据。**

### AR-03B-03-07 — P1：Linux suspend 会使泛称 CLOCK_MONOTONIC 的 Permit 截止时间失真

**问题**

设计要求使用“暂停感知 monotonic clock”，但没有指定时钟源。Linux CLOCK_MONOTONIC 不计入系统 suspend 时间；若 ARC/NRG 节点 suspend，Permit/admission cutoff 可能在挂起期间不前进，恢复后旧执行权可能比预期多存活。

**必须修正**
- Linux 使用 CLOCK_BOOTTIME 或经证明计入 suspend 的等价时钟；
- resume 后先检查 Permit/admission lease 并完成 reconciliation，不得先解除 frozen cgroup 或接收 Start；
- 加入 ARC-T47 suspend/resume 测试。

**状态：已写入 §9.3；目标平台实测未完成，P1 仍开放。**

### AR-03B-03-08 — P1：StartPermit 剩余时长缺少 DB→本机的保守锚定规则

**问题**

StartPermit 绑定 expiry，但未规定 DB 验证响应的网络延迟如何扣除。如果本机按 receive time + DB 返回剩余时长计算，传输延迟会被错误地加回 Permit，扩大执行窗口。DB validation 函数是否锁 Attempt/Job、与 cancel/recovery 的锁序也未定义。

**必须修正**
- DB-BLOCKER-02 返回向下取整、被 lease/deadline clamp 的 start_permit_remaining_ms；
- ARC 在 RPC send 前记录 CLOCK_BOOTTIME t_send，以 t_send + remaining - transport/scheduler guard 计算 cutoff；
- response 到达时已过期或不足以覆盖 Start/kill 预算则拒绝；
- DB-BLOCKER-02 明确隔离级别、行锁与 cancel/recovery 锁序；若不锁行，证明短 TTL 足以覆盖竞态；若锁行，不得引入反向锁序；
- 同一 Permit 只允许一个 operation_id；Start uncertain 后不得盲目重放。

**状态：已写入 §9.3；DB-BLOCKER-02 尚未设计，P1 仍开放。**

### AR-03B-03-09 — P1：NRG 自身故障的“独立监督”仍是抽象承诺

**问题**

设计写有 systemd watchdog/restart，但没有固定服务依赖、watchdog 超时、RestartSec、停止超时、NRG 卡在不可中断内核等待时的处理，以及节点 Ready 的恢复门槛。不能把“由 systemd 监督”直接等同于有界 fail-safe。

**必须修正**
- 具体冻结 NRG systemd unit/watchdog/RestartSec/TimeoutStopSec 与 NRG service cgroup；
- 明确 PID 1、kernel、containerd 的故障假设；若 systemd/PID 1 无法执行回收，节点必须被视为不可用并由节点外部 fencing/重启策略处理；
- NRG crash/restart 前后，所有 Attempt cgroup 先 freeze/reconcile，未知资源 KillOnly；
- 实测 NRG crash、hang、D-state、watchdog、systemd restart、权限/磁盘故障的最坏时延；
- 未证明时节点 Ready 必须失败。

**状态：未充分设计，P1 仍开放。**

## 8. 第二轮最终裁决

PR #6 仍为 **Changes Required / Not approved for implementation**。第二轮新增的 §7.7/§7.8/§9.3 是候选约束，不是已证明的实现方案。

当前不批准创建 ARC/NRG 代码、DB migration、Compose 或生产启用配置。下一步应先做一个最小 Linux runtime feasibility spike：固定目标 kernel/containerd/runc/systemd 版本，验证方案 A/B 中一个完整的 Start-vs-freeze/revoke-vs-late-Start 时序，并保留内核、containerd、shim、cgroup 证据。若无法证明迟到 Start 不会执行，则必须改变执行架构，不能靠加长 timeout 或增加轮询掩盖。
