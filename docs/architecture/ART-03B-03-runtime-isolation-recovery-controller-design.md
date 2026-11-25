# ART-03B-03 — Attempt Runtime Isolation & Recovery Controller 架构设计

> 状态：**Architecture proposal / 独立评审前；不可实施**  
> 基线：main `c11e79a51a145ded026632630bb0b52aa44dc311`  
> 关联：ART-03B-01 Worker 协调基础设施、ART-03B-02 v2.2 Supervisor FSM、DB-BLOCKER-01 Migration 0004 设计、ART-03 SeeCode Adapter。  
> 本文只定义平台运行时隔离与回收架构，不实现 Controller、Worker、容器配置、数据库函数或 Migration。

## 1. 目标与范围

ART-03B-03 解决一个明确的生产安全缺口：**Worker/Supervisor 进程崩溃、重启、失联或租约过期时，系统仍能可靠定位、终止、确认和清理某个 GenerationJob Attempt 的整个执行隔离单元。**

目标：

1. 一个 Attempt 对应一个不可逃逸、资源有界的 runtime isolation unit。
2. Executor 不能访问宿主机控制面、Artifact API、LMMS、AI-PM、数据库、对象存储或外部网络。
3. Worker 不持有 Docker/containerd socket、cgroup 控制权限或宿主机特权。
4. 独立 Runtime Controller 在 Worker/Supervisor 崩溃后仍能终止 Attempt。
5. Controller 重启后可发现并收敛孤儿隔离单元；操作幂等、可审计。
6. 只有平台确认整个隔离单元死亡后，Worker 才能读取冻结输出、提交结果或清理 Attempt 目录。
7. DB Job/Attempt 状态仍由 PostgreSQL 唯一裁决；Runtime Controller 不成为第二套 Job 状态数据库。
8. 生产部署不满足硬隔离条件时 fail closed，不退化为普通子进程/进程组 kill。

非目标：

- 不重新设计 GenerationJob/Attempt DB 状态机；以 ART-03B-02 与已批准的 DB 契约为输入。
- 不修改 Migration 0001/0002/0003，也不在本文创建 Migration 0004。
- 不实现 SeeCode Adapter、SVG sanitizer、File Service upload/finalize 或 READY Version 协议。
- 不把容器管理 API 暴露给 Worker 或 Executor。
- 不把 Docker socket bind mount 到 Worker 容器。
- 不把本地目录 TTL 当作进程死亡证明。

## 2. 当前仓库事实与问题边界

当前 main 的工程结构已经有独立 `artifact-worker`，但它目前只做 Job 协调维护（lease recovery / input purge），不申报 Generator Adapter，也不领取执行 Job。README 与 `deploy/docker-compose.worker.yml` 明确指出：

- Worker 是独立进程，使用独立 DB role；
- Worker 镜像没有 Node / SeeCode vendor；
- 生产隔离（只读根文件系统、无外网、资源配额）属于 ART-03B-03，当前 Compose 不宣称已经完成；
- 当前 Worker 没有运行时容器管理或整树进程回收能力。

ART-03B-02 v2.2 已定义 Supervisor 的 RPC/Lease/Heartbeat/FSM 行为，但明确把 cgroup/container 硬隔离实现留给 03B-03。因此本设计是 03B-02 的平台依赖，不是对现有能力的描述。

## 3. 核心架构裁决（推荐基线）

### 3.1 独立 Runtime Controller，不在 Worker 内直接管理容器

采用两个独立进程边界：

- **Artifact Worker / Supervisor**：领取 Attempt、运行 Supervisor FSM、调用 DB 受控函数、请求启动/终止隔离单元、在 death-confirm 后读取输出并继续 Artifact 提交流程。
- **Artifact Runtime Controller（ARC）**：宿主机级或节点级服务，独立于 Worker 生命周期；唯一持有 container runtime 控制 socket 与 cgroup 管理权限；执行受限的 Create / Start / Kill / Inspect / FreezeOutput / Cleanup 操作。

ARC 不处理业务 Job，不执行 SeeCode，不访问 Artifact API 或 File Service，不决定 Job succeeded/failed/cancelled。它只管理“某个 Attempt 对应的隔离单元是否存在、运行、已终止、可安全读取、可清理”。

**禁止**：
- Worker 直接连接 Docker/containerd socket；
- Executor 获得 ARC socket；
- Worker 获得 `/sys/fs/cgroup` 写权限、host PID namespace、host network 或 privileged；
- 将 ARC 作为 Worker 的子进程启动；
- 依赖 Worker 的 Drop/析构函数完成唯一的 kill 操作。

### 3.2 Linux 生产基线：每 Attempt 一个 OCI 容器 + cgroup v2

生产首选 Linux 节点上的 OCI runtime，由 ARC 调用 containerd（或经独立、受限的节点 runtime shim）创建每 Attempt 一个容器。

每个 Attempt 必须拥有：
- 独立 PID namespace；
- 独立 mount namespace；
- 独立 network namespace，默认无外部接口/无外网出口；
- 独立 cgroup v2 subtree；
- 独立、限额的 scratch filesystem；
- 固定且按 digest pin 的只读 Executor 镜像；
- 不可继承宿主机文件描述符、socket、凭据或宿主目录。

cgroup v2 是进程树资源和生命周期边界；OCI container 提供 namespace、mount、seccomp 等隔离组合。**容器名称、PID、进程组、PID 1、PDEATHSIG 都不能单独作为进程树死亡证明。**

若目标节点不支持统一 cgroup v2、runtime 无法将容器放入 ARC 管理的专属 cgroup，或无法证明进程不能迁移/逃逸，则节点健康检查失败，ARC 不得启动生产 Attempt。

### 3.3 Windows 支持策略

设计保留平台接口抽象，但不宣称 Windows 与 Linux 已具备等价生产安全性。

Windows 实现候选为每 Attempt 一个受控 Job Object：
- CREATE_SUSPENDED 后先 AssignProcessToJobObject，再 Resume；
- 禁止 BREAKAWAY / SILENT_BREAKAWAY；
- Job 句柄不可由 Executor 继承或复制；
- 配置 KILL_ON_JOB_CLOSE；
- 必须验证嵌套 Job、服务重启和句柄生命周期语义。

在完成独立实现、故障注入与安全复核之前，**Windows 只能作为开发/测试目标，不属于生产执行支持矩阵**。不得使用普通 Process.Kill 或进程枚举模拟 Linux cgroup 的保证。

## 4. 部署拓扑与信任边界

```text
                 PostgreSQL (Job/Attempt authority)
                         ^             ^
             Worker DB role       ARC read-only role
             controlled RPC       validate runtime authority
                         |             |
                         |             |
                 +-------+-------------+-------+
                 |                             |
          Artifact Worker                Runtime Controller
          / Supervisor                   (node service)
                 |                             |
           Unix Domain Socket --------------> |
                 |                       containerd socket
                 |                             |
           no runtime privilege          OCI runtime / cgroup v2
                                               |
                                       Attempt container
                                       - no network
                                       - no credentials
                                       - bounded scratch
                                       - SeeCode Executor
```

信任层次：

1. PostgreSQL 是 Job/Attempt 状态、lease、deadline、cancel request 的权威。
2. ARC 是宿主运行时资源的权威观察者和控制者，不是业务状态权威。
3. Worker 是受控协调者；其本地状态不能覆盖 DB 或 ARC 的事实。
4. Executor 是不可信代码执行环境；即使 SeeCode 当前为固定 vendor，也按可被利用的输入解析器/执行器处理。

ARC 与 Worker 位于同一节点但不同进程身份。ARC 由 systemd/节点服务管理，不能随 Worker 容器停止而退出。ARC 只向本机 Worker 暴露 UDS；UDS 通过 peer credentials（Linux SO_PEERCRED）识别调用方，并校验 allowlist RPC。不得开放公网 HTTP 管理端点。

## 5. ARC 接口与权限设计

### 5.1 最小 RPC 面

建议版本化本地协议，初始仅允许：

| RPC | 用途 | 安全约束 |
|---|---|---|
| CreateAttemptSandbox | 为一个已 claim Attempt 创建隔离单元 | 必须验证 Attempt runtime capability；配置由服务端策略生成 |
| StartAttemptSandbox | 启动已创建的隔离单元 | 只能启动当前 Worker session 拥有的 Attempt |
| GetAttemptStatus | 查询容器/cgroup 运行事实 | 不返回宿主敏感路径、其他租户信息 |
| RequestAttemptKill | 单向进入 KillOnly 并终止整棵隔离单元 | 幂等；不允许取消 kill |
| WaitForDeath | 等待并确认 cgroup/container 中不存在进程 | 有 deadline；超时返回 DeathUnconfirmed |
| ReadFrozenOutput | 读取 death-confirm 后冻结的 allowlisted 输出 | 只读、文件数/单文件/总字节限额、拒绝 symlink/path traversal |
| AcknowledgeOutputDisposition | Worker 声明输出已消费或明确放弃 | 必须携带 ARC 签发的 output receipt；幂等 |
| CleanupAttemptSandbox | 清理已 death-confirm 且输出 disposition 已确认的资源 | 需 ARC 签发的 cleanup capability 和幂等 tombstone |

协议禁止：
- 任意命令、shell、可执行路径、环境变量、mount、host path、container runtime 参数；
- 任意 container ID/PID/cgroup path 作为调用方可指定的控制目标；
- 以 Worker 传来的任意路径删除文件；
- 为 Executor 暴露 ARC UDS；
- 通用 Docker/containerd proxy API。

所有请求有 request_id、worker_session_id、job_id、attempt_id、attempt_number、protocol_version。ARC 从服务端配置推导镜像 digest、资源配额、seccomp profile、mount allowlist、网络策略，不接受 Worker 覆盖安全配置。

ARC 必须对同一 runtime identity 的 Create/Start/Kill/Inspect/Freeze/Cleanup 操作提供线性化裁决：使用 per-attempt durable operation journal + 单一状态转换锁/CAS。KillRequested 一旦持久化即成为不可逆 fencing point；所有尚未完成的 Start/Create completion 在继续前必须重新检查 generation/state，不能在 KillRequested 之后启动 Executor。多个 ARC 实例若部署在同一节点，必须由节点级单实例锁或共享一致性协调保证只有一个控制者；首版推荐每节点单 ARC active instance。

### 5.2 Worker 身份与 Attempt capability

不能只依赖 Worker 自报 `job_id/attempt_id`。ARC 必须验证调用者身份和 Attempt 的 DB 执行权：

- Worker 通过 UDS peer credential 认证本机进程身份；ARC 将该身份映射到受配置允许的 Worker service identity。
- Create/Start/Inspect/Kill 请求必须带当前 Attempt 的短时 runtime capability；ARC 不接受仅有 ID 的控制请求。
- capability 至少绑定 service audience、worker identity/session、job_id、attempt_id、attempt_number、lease token 或其不可逆证明、到期时间、nonce。
- ARC 使用独立 DB role 调用**只读、窄权限的 Attempt runtime authority 验证函数**，验证 capability 对应的 token hash、Job/Attempt 仍 running、Lease/deadline 与 cancel 状态；ARC role 仅有该函数 EXECUTE，不获得业务表 SELECT/DML。
- DB 验证函数的具体 schema、参数、返回枚举与权限在 DB-BLOCKER-02/后续 DB 设计中单独评审；本文不创建函数或 Migration。
- capability 必须绑定 ARC 建立的 worker session nonce。Worker 进程重启后不得继承旧 session；新 session 不能凭相同 worker_id 接管旧 Attempt。
- capability 不得成为 Artifact/File Service 的业务授权，不得用于上传或 READY 提交。

ARC 的独立 DB 验证是安全边界，不是让 ARC 变成第二个 Job worker。ARC 只验证是否允许创建/继续该隔离单元，不领取 Job、不推进 Job 状态、不执行结果结算。

若数据库不可达、验证函数不兼容、capability 过期或状态不明确，ARC fail closed：不创建新 sandbox；已有 sandbox 进入 KillOnly/回收路径，不继续执行。

### 5.3 Worker session 与资源归属

ARC 为每个 Worker 进程建立 session，并为每个 Attempt 记录不可复用的 runtime identity：
- worker service identity + worker process/session nonce；
- job_id / attempt_id / attempt_number；
- runtime resource ID（由 ARC 生成，不接受 Worker 指定）；
- image digest、resource profile、creation time；
- 最后一次 DB authority 验证时点与结果；
- 当前 Runtime lifecycle state；
- death-confirm 与 cleanup 证据摘要。

Worker session nonce 是进程级随机值，只在该 Worker 进程内存中持有；ARC 同时维护 session 连接状态。ARC 重启后，仍存活的同一 Worker 进程必须通过原 session secret 完成 reattach；ARC 通过 UDS peer credential + session proof 双重确认。Worker 进程已重启、secret 丢失或 session 身份不匹配时，不得 re-adopt 旧 sandbox，即使 DB lease 暂时仍显示 active；ARC 必须 KillOnly，让 DB recovery 按既定协议处理。这样避免“新 Worker 进程凭同一 worker_id 接管旧 Executor”的身份混淆。

一个 Worker session 不能操作另一个 session 的 sandbox；相同 Worker ID 不代表相同进程所有权。

## 6. Sandbox 安全配置基线

### 6.1 进程、身份与文件系统

- 使用专用非 root UID/GID；rootless container 优先，但不能以 rootless 名义替代完整隔离测试。
- drop all Linux capabilities；启用 no_new_privs；禁止 setuid/setgid 提权。
- seccomp 默认拒绝高风险 syscall，使用经过审查的最小 allowlist/profile；AppArmor/SELinux 至少启用一种强制访问控制。
- root filesystem read-only；只挂载只读输入和一个 Attempt 专属 scratch 输出卷。
- 不挂载宿主根目录、Docker/containerd socket、cgroupfs、/proc 宿主视图、设备节点或 Worker 配置目录。
- 输入在创建时由 ARC 接收并写入受控只读位置；Executor 不能修改输入。
- scratch 具有 inode、文件数、单文件大小和总字节数上限；禁止设备文件、FIFO、socket、硬链接逃逸与 symlink traversal。
- 输出读取采用文件 allowlist 和打开后 fd-relative 校验，不接受绝对路径/相对路径穿越。

### 6.2 网络与凭据

- 默认 network namespace 无外部接口；不只依赖 DNS/代理环境变量。
- 不注入 DB DSN、Worker DB role、API token、LMMS delegation、File Service token、S3/RustFS credential、云实例 metadata credential。
- 不挂载 Docker credential、SSH agent、Git config、用户 home、云 provider socket。
- Executor 不得连接宿主机 loopback 服务或通过 IPv6/link-local/metadata 地址绕出网络隔离；必须做实际负向测试。
- ARC 与 Worker 的 UDS 不进入 Attempt container；宿主 runtime API 不可达。

### 6.3 资源与拒绝服务

每个 Attempt 的 CPU quota、memory.max、pids.max、io.max、scratch quota、wall-clock deadline 均由 ARC/runtime 强制执行，而不是只由 Worker 自觉设置 timeout。

资源 profile 由服务端 allowlist 决定。任何 profile 缺省、未知或无法落实时拒绝启动。容器启动前校验节点可用配额，避免只配置不生效。

## 7. Runtime 生命周期与双重状态机

### 7.1 ARC Runtime lifecycle

ARC 自己维护的资源状态（不是 DB Job 状态）：

| 状态 | 含义 |
|---|---|
| Absent | 没有 runtime resource |
| Creating | runtime create 正在进行或结果不确定 |
| Created | 已创建但尚未启动 |
| Running | 容器任务已启动 |
| KillRequested | 已发出不可逆 kill |
| Stopping | runtime 正在回收 |
| DeathConfirmed | cgroup 为空、runtime task 已退出且隔离单元不可再执行 |
| DeathUnconfirmed | 在有界等待内无法确认完整死亡 |
| FrozenOutput | death-confirm 后输出已冻结并生成清单/hash |
| CleanupPending | 等待 Worker 消费/放弃输出或保留策略许可 |
| Removed | runtime resource 与 scratch 均已移除，保留 tombstone |

DB Job/Attempt 状态与 ARC 状态是两个不同维度。允许：
- DB Job cancelled + ARC DeathUnconfirmed；
- DB Job cancelled + ARC Stopping；
- DB Job retry_wait + 前一 Attempt ARC CleanupPending；
- DB Job running + ARC Creating/Running（仅当前有效 Attempt）。

禁止：
- DB cancelled 被映射成 ARC DeathConfirmed；
- ARC DeathUnconfirmed 时允许 ReadFrozenOutput/complete/fail/confirm；
- ARC 未 DeathConfirmed 就删除 scratch；
- 已被 DB 判 stale/expired 的 Attempt 重新 Start；
- Removed 后用相同 attempt_id 重新创建资源。

### 7.2 启动与运行

1. Worker claim 得到 Job/Attempt 和 DB lease token。
2. Worker 请求 ARC 创建 sandbox；ARC 校验 capability 和 DB authority。
3. ARC 生成不可复用 runtime ID，创建 cgroup/container、受控目录和资源策略；先 Created，不运行 Executor。
4. ARC 再次验证 DB authority；成功后 Start，失败则销毁 Created resource。
5. Worker/Supervisor 通过 ARC 控制接口执行有限操作；Executor 本身只运行 SeeCode。
6. ARC 周期性独立验证 DB authority/lease/deadline；cancel、lease expired、deadline reached 或 DB 不可验证时，转 KillRequested。
7. Worker 也可主动 RequestAttemptKill；kill 只能单向触发，不能撤销。

DB-BLOCKER-02 的 remaining-duration + monotonic anchor 契约是 ARC 计算本地 fail-safe cutoff 的前置条件。ARC 不得用本机 wall clock 与 PostgreSQL wall clock 直接比较。

### 7.3 Kill 与死亡确认

Kill 流程必须由 ARC 执行，不依赖 Worker 仍在线：

1. 原子记录 KillRequested；禁止 Start/Resume/新 IPC。
2. 调用 runtime kill 整个 container，并使用 cgroup v2 的整组终止能力（如 cgroup.kill）；不只 kill 主 PID/进程组。
3. 在有界 deadline 内等待 runtime task exit、cgroup `cgroup.events populated=0`，并确认 cgroup 不再允许新进程加入。
4. 关闭/撤销容器控制通道，移除可执行 runtime task；记录 runtime inspect 证据。
5. 满足所有 death predicate 后转 DeathConfirmed；否则 DeathUnconfirmed，保留隔离资源并告警。

Death predicate 至少包含：
- OCI runtime 报告该 Attempt task/container 已停止；
- Attempt 专属 cgroup populated=0；
- 无可写路径允许 Executor 将进程迁移到其他 cgroup；
- 不存在仍能重启该 container/task 的未撤销启动请求；
- ARC 已撤销 Start/Exec 控制能力。

不得把 kill RPC 返回 0、主 PID 不存在、进程组为空、Worker 退出或 lease 过期作为充分死亡证明。

### 7.4 Worker / Supervisor 崩溃与 ARC 崩溃

**Worker/Supervisor 崩溃：**
- ARC 的 runtime watchdog 不依赖 Worker heartbeat 线程；
- ARC DB authority 验证失败/lease 到期/Worker session 断开达到策略阈值时，KillOnly；
- ARC 保留资源记录和目录，直到 death-confirm；
- DB recovery 只撤销 Job/Attempt DB 权限；不负责 OS kill 的事实证明。

**ARC 崩溃或节点重启：**
- containerd/cgroup 资源可能仍存在，不能假设 ARC 进程退出会自动杀死所有容器；
- ARC 重启后先进入 Reconciliation，不接受新 Create/Start；
- 按 runtime labels 与持久化 journal 枚举所有本服务 Attempt resource；
- 对每个资源先验证 runtime identity、原 worker session 与 durable journal；只有原 Worker 进程仍存活并使用原 session secret 成功 reattach，且 DB authority 仍 active，才允许 re-adopt 并继续监控；
- 若 Worker 进程已重启、session secret 丢失/不匹配、session 已断开超过策略窗口，或 ARC 无法证明原所有者仍存活，则即使 DB lease 尚未过期也 KillOnly；不允许新 Worker 进程接管旧 Executor；
- cancelled/stale/expired/unknown 的资源立即 KillOnly；
- DB 不可达时，不重新启动任何容器；已有容器进入 KillOnly，直至可确认死亡或人工隔离；
- 完成全量 reconciliation、确认不存在未知/重复 runtime identity 后才进入 Ready；
- Create 在 runtime create 成功但 journal 写入前崩溃时，通过 runtime label 中的 operation_id/attempt_id 发现孤儿；同一 Attempt 出现多个 runtime resource 时全部 KillOnly 并隔离节点，不任选一个继续；
- 重复 create/kill/cleanup 通过 idempotency key + runtime identity 防止双资源。

节点掉电时 OS 进程随节点停止，但重启后必须扫描残留 runtime metadata/cgroup；没有被 ARC 识别的 Attempt 容器必须隔离并删除，不得自动恢复执行。


### 7.5 ARC 自身故障的独立 fail-safe 回收（新增强制门禁）

ARC 是 runtime 控制者，但不能同时成为自己故障时唯一的恢复机制。containerd/cgroup 不会因为 ARC 进程退出或卡死而自动终止现有 Executor。因此，“ARC 重启后 reconciliation”只能描述恢复步骤，不能单独证明 observation window 有界。

生产节点必须增加一个**独立于 Worker 与 ARC 生命周期的 Node Runtime Guardian（NRG，节点级 fail-safe guardian）**，或提供经独立评审证明等价的内核/runtime 强制期限机制：

- NRG 只拥有 Attempt 专属 cgroup 根范围内的 KillOnly 能力；不得创建 sandbox、启动 Executor、读取输出、访问 DB、操作 Artifact/File Service 或推进 Job 状态。
- ARC 必须向 NRG 续租节点级控制租约/心跳。NRG 超过固定失联阈值未收到有效续租，必须禁止该节点新 Start，并对该节点所有由 Artifact Service 管理的 Attempt cgroup 执行 fail-safe kill。
- NRG 必须由独立节点服务管理器监督（例如 systemd watchdog/restart）；其检测、重启、扫描和 kill 的最坏时延必须纳入 observation window。ARC 与 NRG 不得共享同一个进程、线程池、事件循环或可共同阻塞的 RPC 依赖。
- NRG 不依赖 PostgreSQL 可用性；DB 不可达时仍能执行本地 KillOnly。NRG 无法确认自身健康、权限或 cgroup 根范围时，节点不得 Ready。
- NRG 重启后先枚举其授权范围内的 Artifact Attempt cgroup；未知、journal 不一致、ARC lease 过期或无法识别的资源一律 KillOnly，不得恢复执行。
- NRG 不能确认 kill 时，节点进入 Quarantined；禁止新 Attempt、禁止输出读取和目录清理，保留证据并人工处置。
- 必须明确节点管理器、NRG、ARC、kernel/runtime 各自的故障假设。若无法给出 ARC 失联至 NRG kill 的实测上界，不能声称 bounded fail-closed。

NRG 是宿主机 fail-safe kill-only 组件，不是第二个 ARC，不成为 Job/Attempt 状态权威，也不向 Worker 暴露通用控制 API。

新增验收：ARC 在 Create/Start/Running/Kill 任意阶段被 SIGKILL 或永久挂起，NRG 必须在冻结的上界内阻止新 Start 并终止全部受管 Attempt；同时注入 NRG 进程崩溃、watchdog 超时、权限丢失、cgroupfs 异常，验证节点 fail closed。该测试必须覆盖 ARC 与 Worker 同时故障。


### 7.6 Durable journal 的崩溃一致性协议（新增强制门禁）

“durable journal”必须落实为可验证的 write-ahead 协议，而不能只表示 ARC 将状态写入某个本地文件/数据库。

每个操作必须遵循以下顺序：

1. 分配全局唯一 operation_id，并在 Attempt 锁/CAS 下校验当前 lifecycle generation。
2. 将操作意图、Attempt/runtime identity、operation generation、session binding、期望前态/后态写入 journal；执行 fsync（必要时同步父目录），成功前不得调用 runtime。
3. 调用 containerd/cgroup 操作时，将 service identity、job_id、attempt_id、attempt_number、operation_id、generation 写入不可由 Executor 修改的 runtime labels/metadata。
4. 收到 runtime 结果后，先 Inspect 并核验 runtime identity，再写入 observed resource ID/结果并 fsync；只有持久化完成后才向 Worker 返回成功。
5. 响应丢失或 ARC 崩溃时，以 operation_id + runtime labels + journal 进行 reconciliation；不能唯一确定资源归属时，所有候选资源均 KillOnly，不允许任选一个继续。
6. KillRequested 必须先写入并 fsync，再执行 kill；若 fsync 失败，不得返回“已 fencing”，必须通过 NRG 触发 fail-safe kill，并将节点标记为 Quarantined。
7. Start 同样必须有持久化 intent 和 generation fencing；任何迟到的 runtime success response 都必须重新检查 KillRequested/generation，不能覆盖较新的 Kill 状态。
8. journal 损坏、磁盘满、I/O error、文件系统只读、fsync 失败、label 缺失/重复或版本不兼容时：禁止 Create/Start/Read/Cleanup；已存在 sandbox 由 NRG KillOnly，保留证据，节点不得 Ready。
9. tombstone/operation 去重记录的保留期必须覆盖最长 RPC 重试、ARC/NRG 重启与 reconciliation 窗口；不得因过早压缩而允许旧 request_id 复活资源。

该协议仍不能使 PostgreSQL 与 containerd 原子化；它只解决 ARC 自身操作日志与 runtime 副作用之间的崩溃一致性。必须通过 ARC-T43/T44 故障注入验证。

## 8. 输出冻结、目录保留与清理

ARC 不把宿主 scratch path 暴露给 Worker。所有输出通过 ReadFrozenOutput RPC 读取。

流程：
1. DeathConfirmed 后，ARC 对输出目录做只读冻结（mount/permission/immutable snapshot 以平台能力实现），禁止任何进程继续写入。
2. ARC 构造输出 manifest：规范化相对路径、文件类型、长度、SHA-256、总字节数；拒绝 symlink、hardlink、特殊文件、路径穿越和超限。
3. Worker 只通过 ReadFrozenOutput 读取 manifest 中允许的普通文件；读取过程使用 fd-relative/no-follow 语义，防止 TOCTOU。
4. Worker 完成安全检查与后续 File Service 协议后，调用 AcknowledgeOutputDisposition(consumed)；若结果被放弃，也必须明确调用 AcknowledgeOutputDisposition(discarded)。ARC 校验 output receipt 后签发一次性 cleanup capability。
5. Worker 使用 cleanup capability 调用 CleanupAttemptSandbox。ARC 删除容器、cgroup、scratch，保留最小 tombstone（Attempt ID、runtime ID hash、death-confirm 时间、cleanup 时间、结果码、manifest hash），不保留输入/输出正文。
6. 若 DeathUnconfirmed、ARC 状态未知或 cleanup 操作不确定，目录继续隔离，不允许 TTL 自动删除。

输出读取和清理必须幂等。Worker 重试读取同一冻结 manifest，必须得到相同字节/hash；不允许 Executor 再次运行后覆盖同一 Attempt 的输出。

## 9. DB 契约、授权和状态协调

ARC 不直接 SELECT/UPDATE 业务表。它使用专属只读 DB role，仅执行后续独立设计的 `validate_generation_attempt_runtime_authority`（暂定接口名）函数。

函数至少需要验证/返回：
- job_id、attempt_id、attempt_number；
- token hash 匹配（不返回 token hash）；
- Job/Attempt status；
- cancel_requested_at 是否存在；
- lease 与 deadline 剩余 duration；
- 当前结果：active / cancel_requested / stale / deadline_exceeded / unknown；
- DB schema/protocol version。

权限：
- ARC role 只有该只读函数 EXECUTE；
- 无表 SELECT、INSERT、UPDATE、DELETE；
- 无迁移权限、无业务表 DML；
- 函数默认 PUBLIC revoke，固定 search_path，参数与返回字段最小化。

该函数的事务语义、时间契约、锁顺序与权限必须由 DB-BLOCKER-02/独立 SQL 设计审查确定。若 ARC 的只读验证接口尚未落地，ARC 不得自行猜测 DB 状态，也不得把 Worker 心跳当成数据库权威；生产 Executor 启动保持关闭。

ARC 不改变 DB Job 状态。Worker 仍调用 DB 受控函数完成 heartbeat/complete/fail/confirm；ARC 只根据验证结果控制隔离单元。DB cancelled 与 ARC DeathUnconfirmed 可同时成立，且 API/运维必须分别展示“DB 执行资格已撤销”和“进程回收尚未确认”。

### 9.1 PostgreSQL 与 Runtime 的跨系统竞态

PostgreSQL 事务与 containerd/cgroup 操作之间没有分布式原子提交。本设计不声称 cancel commit 与 OS kill 能形成同一瞬时线性化点。

因此必须区分：
- **DB authority fencing**：cancel/stale/deadline 在数据库提交后，complete/fail/confirm 等 DB 受控函数立即拒绝旧 Attempt 的结果提交或状态复活；
- **Runtime revocation**：ARC 在下一次 authority poll、Worker Kill 请求或 session 失联检测后，执行 KillOnly 并回收 sandbox。

在两者之间可能存在一个有界的 runtime observation window。该窗口内 Executor 可能仍消耗受限 CPU/内存，但由于其无网络、无业务凭据、无 DB/File Service/Artifact API 访问能力，不能产生业务副作用或提交结果。输出在 DeathConfirmed 前不可读取。此窗口是安全模型的一部分，必须有明确上界和监控，不得描述成“cancel commit 后进程瞬间停止”。

必须满足：
1. ARC 在 Start 前执行 authority validation；若非 active，拒绝 Start。
2. ARC 在运行期间以固定、可测的最大间隔重新验证 authority；cancel/stale/deadline/unknown/DB timeout 均进入 KillOnly。
3. Worker 收到 cancel_requested、stale 或 deadline_exceeded 时立即请求 ARC Kill；该快速路径不能替代 ARC 自主 watchdog。
4. 定义最大 observation window：authority poll interval + validation RPC timeout + scheduler jitter + kill-to-death-confirm upper bound。参数需在目标节点实测后冻结；不能只给默认值。
5. 若无法证明该窗口有界，节点不得进入生产 Ready。
6. DB 状态撤销后，ARC 不得接受任何新的 Start/Resume/Exec；已有 sandbox 只允许 Kill/Inspect/WaitForDeath。
7. DB cancelled 但 ARC 尚未 DeathConfirmed 的期间，运维界面必须分别展示两种状态，不能用单一“已取消”掩盖回收延迟。

这避免对 PostgreSQL 与 OCI runtime 作出不可能的原子性承诺，同时依靠 sandbox 无副作用与 ARC 有界回收保持安全。


### 9.2 DB 授权检查与 Runtime Start 的不可原子竞态（新增强制语义）

PostgreSQL validation 与 containerd Start 是两个独立系统操作，不能通过“Start 前再查一次 DB”获得跨系统原子性。以下时序始终可能发生：

1. ARC 的 authority validation 读取到 active；
2. PostgreSQL 随后提交 cancel/stale/deadline；
3. ARC 的 Start 请求随后才到达 containerd，Executor 因而短暂启动或继续运行。

因此，原文中“DB 状态撤销后，ARC 不得接受任何新的 Start/Resume/Exec”必须按可实现语义收敛，不能暗示 DB commit 能瞬时 fencing 一个尚未观察到撤销的 runtime。

规范语义：

- DB commit 立即撤销 DB 侧 complete/fail/confirm/heartbeat 的旧 Attempt 权限；这是数据库内的提交 fencing。
- ARC 的 Start 只能使用 DB authority validation 签发/确认的短时 StartPermit。Permit 必须绑定 job/attempt/attempt_number、worker session、runtime identity、单调递增 operation generation、到期 duration 与唯一 nonce；不可跨 Attempt、跨 session 或跨 runtime 重用。
- StartPermit 的最大有效期必须明确，且由 ARC 使用暂停感知 monotonic clock 保守执行；不得只依赖 ARC 本机 wall clock。Permit 到期或 ARC 失去 DB authority 验证能力后，禁止新的 Start/Resume/Exec。
- 已经取得有效 Permit、但尚未完成的 Start 与 DB cancel commit 仍可能竞争。该竞态不得被描述为“绝不启动”；必须将 Permit 最大寿命、在途 Start RPC 上界、NRG 失联回收上界、kill-to-death-confirm 上界统一纳入 observation window。
- ARC 一旦观察到 cancel/stale/deadline/unknown，或 Permit 到期，必须先持久化 KillRequested fencing，再拒绝所有新 Start/Resume/Exec；对已经 in-flight 的 Start 执行同一 operation generation 的状态复核，确保它不能在 KillRequested 后转为可运行状态。若 runtime API 无法提供可证明的取消/检查顺序，则必须由 NRG 的 kill-only fencing 收敛，不得宣称 ARC 内部锁可以原子化 PostgreSQL 与 containerd。
- DB cancel commit 后，在观察窗口内可能发生的仅是无副作用 sandbox 内的受限计算；不得有网络、业务凭据、DB/File Service/Artifact API 或宿主控制面访问。窗口到期仍未 DeathConfirmed 时节点 Quarantined，不能继续运行或清理。
- 指标必须记录最后一次 active validation、Permit expiry、Start operation generation、cancel 首次观察、KillRequested、NRG kill、DeathConfirmed 时间戳；使用单调时钟并记录时钟源/误差预算。

更新后的最大 observation window 至少覆盖：
max(StartPermit lifetime + in-flight Start upper bound, ARC authority poll interval + validation timeout + scheduler jitter, ARC failure detection + NRG watchdog/restart + NRG kill latency) + kill-to-death-confirm upper bound + safety margin。

上述每项必须在目标 kernel、containerd、节点服务管理器和实际部署拓扑上实测。任一项无上界、NRG 不可用或时钟不满足暂停感知要求，节点不得 Ready。

## 10. 故障处理矩阵

| 故障 | ARC 行为 | DB/Worker 行为 | 资源处理 |
|---|---|---|---|
| Worker 在 Create 前崩溃 | 无资源 | DB lease 最终 recovery | 无 |
| Create RPC 响应丢失 | 按 idempotency key inspect，不重复创建 | Worker 不盲目创建第二个 | 保留唯一资源 |
| Create 完成、Start 前 Worker 崩溃 | authority 复核失败则不 Start，kill Created | DB recovery | death-confirm 后清理 |
| Start RPC 响应丢失 | inspect runtime task；不重复 Start | Supervisor 进入 spawn uncertain | 根据实际 runtime 状态收敛 |
| Worker 运行中崩溃 | ARC watchdog 独立验证 DB authority；失效即 kill | DB recovery 撤销资格 | 死亡确认后清理 |
| DB 网络分区 | 不允许新 Start；已运行 sandbox 按 bounded fail-closed policy kill | Worker 不得继续执行/提交 | 隔离直至死亡确认 |
| cancel 已提交 | ARC 读到 cancel_requested 即 kill | Worker KillOnly/confirm | DeathConfirmed 后可清理 |
| Lease/deadline 到期 | kill，禁止续租执行 | DB 按权威状态 recovery/settlement | death-confirm 后清理 |
| Kill RPC 不确定 | Inspect + retry 同一 kill operation | Supervisor DeathUnconfirmed | 不清理 |
| ARC 崩溃重启 | 全量 reconciliation；未知资源 kill | Worker 不能自动 re-adopt 未验证 Attempt | 资源保留至确认 |
| cgroup populated 长时间非零 | 告警、隔离节点、人工处置 | DB 权限可已撤销 | 不清理、不复用 |
| scratch manifest 生成失败 | 不提供输出 | Worker 不提交 Artifact | 隔离目录 |
| Cleanup RPC 响应丢失 | inspect resource + tombstone 后幂等重试 | Worker 不重复执行 Attempt | 不误删其他 Attempt |

## 11. 可观测性、审计与运维

必须暴露但不得泄露输入/凭据的指标：
- active sandbox count、sandbox create/start/kill latency；
- cgroup populated 状态、kill-to-death-confirm latency；
- DeathUnconfirmed 数量与持续时间；
- ARC reconciliation 发现的 orphan/unknown resource 数；
- DB authority validation error/latency；
- scratch bytes/inodes/file count；
- cleanup pending age、cleanup failure count；
- 每个节点的 runtime/cgroup/seccomp/LSM readiness。

日志和审计关联 job_id、attempt_id、worker_session_id、runtime_id（可 hash）、operation_id、状态变化、错误码；禁止记录 lease token、完整 capability、输入/输出正文、DB DSN。

告警：
- DeathUnconfirmed 超过回收 SLA；
- cgroup populated 非零但 DB execution authority 已撤销；
- ARC Ready 但发现未知容器；
- ARC 与 runtime metadata 不一致；
- scratch quota 接近上限；
- DB authority validation 连续失败；
- 任意 Executor 尝试网络访问、特权 syscall、cgroup 迁移或访问宿主控制面。

## 12. 测试与验收矩阵

所有安全断言必须在真实 Linux + cgroup v2 + 目标 OCI runtime 环境执行。仅有 mock/unit test 不足以关闭 PROCESS-BLOCKER-04。

### 12.1 创建与隔离

| ID | 场景 | 预期 |
|---|---|---|
| ARC-T01 | 每个 Attempt 创建独立 container/cgroup/scratch | runtime identity 唯一，不共享可写目录 |
| ARC-T02 | Executor 尝试读取 Worker 环境变量/凭据 | 不存在可读凭据 |
| ARC-T03 | Executor 尝试访问外网、宿主 loopback、metadata、IPv4/IPv6/link-local | 全部失败并有网络策略证据 |
| ARC-T04 | Executor 尝试访问 Docker/containerd socket、ARC UDS、cgroupfs、宿主目录 | 全部不可达 |
| ARC-T05 | Executor 尝试 setuid、capability、mount、namespace、ptrace、bpf 等提权/逃逸路径 | seccomp/LSM 拒绝；sandbox 仍受控 |
| ARC-T06 | Executor 尝试 fork bomb / 超 PID、CPU、内存、IO、scratch 限额 | runtime 强制限额，宿主与其他 Attempt 稳定 |
| ARC-T07 | 未知/未批准 image digest 或 resource profile | Create fail closed |
| ARC-T08 | Worker 尝试指定 host path、命令、runtime 参数、任意 cgroup path | ARC 拒绝；无任意 runtime API |

### 12.2 终止与死亡确认

| ID | 场景 | 预期 |
|---|---|---|
| ARC-T09 | 正常 Kill | 整个 cgroup/container 终止，populated=0 后才 DeathConfirmed |
| ARC-T10 | Executor fork/setsid/double-fork/daemonize | 后代不能逃出隔离单元；全部被终止 |
| ARC-T11 | Executor 尝试 cgroup migration | 权限拒绝，无法逃逸 |
| ARC-T12 | 主 PID 退出但孙进程仍存活 | 仍是 Stopping/DeathUnconfirmed，不得 DeathConfirmed |
| ARC-T13 | kill RPC 响应丢失 | inspect + 同 operation 幂等重试，不产生双重副作用 |
| ARC-T14 | kill 超过回收 deadline | DeathUnconfirmed，目录不清理，节点告警/隔离 |
| ARC-T15 | cancel/lease expiry/deadline 与 Worker 断连同时发生 | ARC 独立回收，不依赖 Worker |
| ARC-T16 | DB cancelled 但 cgroup 仍 populated | DB/OS 状态分别呈现；禁止输出读取和清理 |

### 12.3 崩溃恢复与幂等

| ID | 场景 | 预期 |
|---|---|---|
| ARC-T17 | Worker 在 Create/Start/Kill/ReadOutput/Cleanup 每个 RPC 前后崩溃 | 每种不确定性可 inspect/reconcile，不创建第二隔离单元 |
| ARC-T18 | ARC 在 Create/Start/Kill/DeathConfirm/Cleanup 每个阶段崩溃 | 重启全量 reconcile，状态单调收敛 |
| ARC-T19 | ARC 重启时发现 DB active 的 sandbox，原 Worker 进程仍存活并提供原 session proof | 仅经 DB authority + runtime identity 校验后 re-adopt |
| ARC-T19a | ARC 重启时原 Worker 已重启，session secret 不存在 | 不允许新进程接管；旧 sandbox KillOnly，DB recovery 收敛 |
| ARC-T20 | ARC 重启时发现 cancelled/stale/expired/unknown sandbox | 不 re-adopt，立即 KillOnly |
| ARC-T21 | DB 不可达时 ARC 重启 | 不 Start/不恢复；已有 sandbox fail closed 并回收 |
| ARC-T22 | 同一 Create/Kill/Cleanup 请求重复发送 | 幂等，同一 Attempt 最多一个 runtime resource |
| ARC-T22a | Start 与 Kill 并发/响应乱序 | KillRequested fencing point 后绝不允许 Start 生效 |
| ARC-T23 | 旧 Worker session 重放 capability | 拒绝；新 session 不能继承旧 session 控制权 |
| ARC-T24 | 伪造 job_id/attempt_id/runtime_id 控制其他 Worker sandbox | 拒绝，不影响其他 Attempt |

### 12.4 输出与清理

| ID | 场景 | 预期 |
|---|---|---|
| ARC-T25 | DeathConfirmed 后读取合法输出 | manifest hash 稳定、内容一致 |
| ARC-T26 | 进程死亡前调用 ReadFrozenOutput | 拒绝 |
| ARC-T27 | 输出含 symlink/hardlink/FIFO/socket/device/path traversal | 不读取、不导出；安全拒绝 |
| ARC-T28 | 输出文件数/单文件/总大小超限 | fail closed，不上传 |
| ARC-T29 | DeathUnconfirmed 时 TTL 到期 | 不清理、不复用 scratch |
| ARC-T30 | cleanup 响应丢失后重复 cleanup | 幂等；保留 tombstone，不误删其他资源 |
| ARC-T30a | Worker 未提交 output disposition 就请求 cleanup | ARC 拒绝；资源继续保留 |
| ARC-T31 | 清理完成后旧 capability 再次访问 | 拒绝，Removed 不可复活 |

### 12.5 数据库授权与时间

| ID | 场景 | 预期 |
|---|---|---|
| ARC-T32 | ARC DB role 尝试 SELECT/UPDATE/DELETE 业务表 | 全部权限拒绝 |
| ARC-T33 | ARC DB role 调用未授权函数/任意 DDL | 权限拒绝 |
| ARC-T34 | ARC authority validation 返回 cancel/stale/deadline/unknown | 立即进入相应 KillOnly/拒绝 Start 路径 |
| ARC-T35 | DB authority RPC timeout/transport uncertain | 不视为 active；按 fail-closed 策略停止/回收 |
| ARC-T36 | ARC monotonic cutoff 与 suspend/resume | 目标 Linux clock 行为经过实测，cutoff 保守且不会延长 DB lease |
| ARC-T37 | PostgreSQL 40001/锁超时/连接池故障 | 不将错误映射为 active；无权限复活 Attempt |

### 12.6 平台矩阵与随机故障注入

- Linux 生产支持矩阵必须固定 kernel 最低版本、cgroup v2 mount/controller availability、containerd/OCI runtime 版本、seccomp/LSM profile 版本。
- 对 create/start/kill/inspect/read/cleanup 各边界注入进程 kill、SIGKILL、服务重启、节点重启、DB 网络分区、runtime socket 故障。
- 每个关键 RPC 边界至少 1,000 轮随机崩溃/重放/响应丢失测试；记录唯一 runtime identity 与最终资源计数。
- 并发运行多个 Attempt，验证 CPU/memory/PID/scratch 隔离；一个 Attempt 被 kill 不得影响其他 Attempt。
- 测试必须保留可审计证据：内核版本、runtime 版本、cgroup 配置、测试镜像 digest、日志、指标和失败样本。


新增验收矩阵（必须纳入 ARC-T 系列并在实施前冻结编号）：
- ARC-T38：ARC 在 Create/Start/Running/Kill 阶段 SIGKILL/永久挂起；NRG 在上界内 kill 全部 Attempt，禁止新 Start。
- ARC-T39：ARC 与 Worker 同时崩溃，NRG 独立回收；DB 不可达不影响本地 KillOnly。
- ARC-T40：NRG 崩溃/卡死/watchdog 超时/重启；节点不得 Ready，恢复后先全量扫描并 KillOnly 未知资源。
- ARC-T41：DB cancel 与 StartPermit 签发、Start RPC 交错；验证 Permit 最大寿命和完整 observation window，不出现窗口外运行。
- ARC-T42：Start RPC 响应丢失且 KillRequested 并发提交；迟到 Start completion 不得使 Executor 运行或复活。
- ARC-T43：journal 在 intent fsync、runtime create/start/kill、completion fsync 各边界崩溃；重启后无重复资源，未知资源全部 KillOnly。
- ARC-T44：journal 损坏、磁盘满、fsync 错误、runtime label 缺失/重复；ARC/NRG fail closed，禁止 Start 与 cleanup。

## 13. 分阶段实施顺序

### Phase 0 — 节点能力探测与威胁模型确认
- 固定 Linux 生产支持矩阵；
- 验证 cgroup v2、containerd、LSM、seccomp、network isolation；
- 明确 ARC service identity、Worker UDS identity 和 DB authority validation 接口。

### Phase 1 — ARC 最小控制面
- 只实现 Create / Start / Inspect / Kill / WaitForDeath；
- Worker 无 runtime socket；
- 每 Attempt 单容器、单 cgroup、单 scratch；
- 无网络、无凭据、只读 root、资源限额。

### Phase 2 — 崩溃恢复与 DB authority
- 独立 ARC service lifecycle；
- runtime labels + durable journal reconciliation；
- DB 只读 authority validation function 与专属 role；
- ARC restart / Worker restart / DB partition 故障注入。

### Phase 3 — 输出冻结与清理
- death-confirm 后 manifest；
- ReadFrozenOutput allowlist；
- Cleanup idempotency/tombstone；
- DeathUnconfirmed 隔离与人工处置。

### Phase 4 — 安全复核与生产门禁
- ARC-T01–T37 通过；
- 资源限制与网络逃逸负向测试通过；
- 1,000+ 故障注入矩阵通过；
- 安全复核证明 Worker 不持 runtime 特权；
- 与 ART-03B-02 FSM、DB-BLOCKER-01/02 真实集成通过。

## 14. 依赖、阻断与最终裁决

| 依赖/阻断 | 对 ART-03B-03 的影响 | 状态 |
|---|---|---|
| ART-03B-01 独立 Worker / DB role | 提供 Worker 进程与最小 DB 身份 | 已有基础实现，需保持边界 |
| ART-03B-02 Supervisor FSM v2.2 | 定义 Worker 与 ARC 的调用时序/不确定状态 | 设计候选，未批准实施 |
| DB-BLOCKER-01 | 定义取消优先、Attempt/Job 锁序与终态 | PR #5 设计中 |
| DB-BLOCKER-02 | 为 ARC 提供可验证的 lease/deadline 剩余时间 | 设计候选，未实现 |

| ARC 自身故障的独立回收 | 原设计未给出 ARC 失联时的独立 kill watchdog | 新增 NRG/等价机制与故障上界实测，未完成前生产阻断 |
| DB validation → runtime Start 竞态 | 两系统无原子提交，原“撤销后绝不 Start”过强 | 短时 StartPermit + 在途操作 fencing + NRG kill 收敛，完整窗口实测 |
| journal 持久化故障语义 | 原文提出 durable journal，但未定义 fsync 顺序/损坏/磁盘满处理 | 明确 write-ahead intent、fsync、runtime label 对账；不确定一律 KillOnly |

| Runtime authority validation DB function | ARC 独立验证 Attempt 权限 | 未设计/未实现，需独立 SQL 审查 |
| Linux cgroup v2 + OCI runtime | 硬隔离与进程树终止 | 目标平台待选型/实测 |
| LMMS delegation P0 / File Service finalize P0 | Artifact 正式提交与 READY | 独立生产 P0，不能由本设计绕过 |

**推荐架构裁决：独立节点级 Artifact Runtime Controller + 每 Attempt 一个 OCI sandbox + cgroup v2；Worker 只通过受限 UDS RPC 控制；ARC 使用专属只读 DB authority 验证；DeathConfirmed 之前禁止读取输出、提交结果和清理目录。**

**当前状态：Architecture proposal / Not approved for implementation。** 本文没有证明当前环境已部署 cgroup v2/OCI runtime，也没有真实平台故障注入证据。PR 必须保持 Draft，等待独立架构评审；不得把设计条目标记为已实现或已关闭。
