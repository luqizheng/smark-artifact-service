# ART-03B-02 — 测试策略 v2

> 状态：**v2 设计基线（待人工评审）**。验收口径：矩阵 v2 逐行覆盖 + 故障场景 v2 全场景闭环 + 本文全部用例通过。
> 测试环境沿用 03B-01 既有模式（`tests/tests/worker_coordination.rs` / `worker_roles.rs` / `roles_separation.rs`：真实 PostgreSQL + Worker 独立 role）。进程树测试双平台执行（CI Linux + 本机 Windows，Windows 进程树用例允许本地手动/`cargo test --ignored` 分层）。

## 0. 测试金字塔与映射

| 层级 | 目标 | 环境 |
|---|---|---|
| L1 单元 | 状态机转移纯逻辑、时钟抽象、env allowlist、路径模式校验 | 无外部依赖 |
| L2 PG 集成 | 受控函数返回值 × Supervisor 决策（真实 PG + worker role） | docker compose PG |
| L3 真实进程树 | spawn/组杀/Job Object/死亡确认/残留检测 | 本机双平台 |
| L4 故障注入 | S1~S13 场景 | L2/L3 组合 |
| L5 平台差异 | Linux/Windows 行为分叉 | 双平台矩阵 |
| L6 03B-03 契约 | 容器生命周期接口（占位，03B-03 实施时激活） | 容器 |
| L7 安全隔离 | 凭据/FD/权限 | L3 + 静态断言 |

## 1. L1 单元测试

| ID | 用例 | 断言 |
|---|---|---|
| U-01 | 状态机全转移：矩阵 §3.1~§3.4 每行至少一条转移断言（表驱动：状态 × 事件 → 期望后继/DB 调用序列/提交计数） | 转移与矩阵逐行一致；**不存在未定义事件**（非法组合返回错误而非 panic） |
| U-02 | 提交上界不变量：构造最长链（Err→探针→补交→Err→放弃） | 单 Attempt 内 complete/fail/confirm 各 ≤2；总受控调用次数符合 §3.5-3 |
| U-03 | INV-2/INV-4 配置校验：(L, H, T, M, K) 参数化表 | 非法组合启动失败（如 L=30 拒绝；L=45+H=11+T=10+M=12+K=2 通过） |
| U-04 | 时钟抽象：注入跳变墙钟 + 平稳单调钟 | INV-1 触发时刻只随单调钟变化；deadline 备份计时 = claim 时刻 + (deadline−started) 差值 |
| U-05 | env allowlist：env_clear 后注入白名单 + 显式 deny 断言 | Executor 环境变量集合 == 白名单（集合相等断言）；deny 列表（DSN/token/password 形态）零出现 |
| U-06 | 目录路径校验：`jobs/{uuid}/attempts/{uuid}` 模式 | 合法路径通过；非 UUID 段、symlink 成分、`..`、绝对路径逃逸全部拒绝 |
| U-07 | result.json schema 校验 | 合法/缺字段/超长 diagnostic(>2048)/非法 status 枚举分类正确（合法/EXECUTOR_PROTOCOL） |
| U-08 | 排水预算计算 | budget = min(30s, INV-1 余量, deadline 余量)；宽限动态压缩 ≤ 预算 |

## 2. L2 PostgreSQL 集成测试（延续 worker_coordination.rs 模式）

| ID | 用例 | 断言 |
|---|---|---|
| P-01 | claim → 全生命周期 happy path（stub executor 秒回 succeeded result） | complete 调用序列、DB 终态 succeeded、目录清理 |
| P-02 | E2 取消全链：request_cancel(running) → heartbeat `cancel_requested` → kill+confirm | C1：job/attempt cancelled；confirm 仅在死亡确认后发生（调用序断言） |
| P-03 | E3 deadline：短 max_execution_seconds + 慢 stub | heartbeat `deadline_exceeded`；**断言 Supervisor 零 DB 写**（对 fail/complete 的 mock 端口调用计数=0）；DB attempt timed_out + job retry_wait |
| P-04 | E4 stale：篡改内存 token 后 heartbeat | `stale` → 零 DB 写 + KillOnly |
| P-05 | T2：complete 返回 cancel_requested（先 request_cancel 再 complete stub 输出） | 转 CancelSequence；**不**尝试重 complete |
| P-06 | U1/U4 探针：complete 注入 Err → 手动控制 DB 状态再探针 | (a) DB 仍 running ⇒ 探针 renewed ⇒ 补交一次成功；(b) DB 已 succeeded（预置）⇒ 探针 stale ⇒ 放弃零写 |
| P-07 | W1 竞争：并发 request_cancel 与 complete | DB 裁决两种结果各至少一次出现且 Supervisor 对应处理（T1 或 T2） |
| P-08 | W4：租约过期 → 恢复扫描 → 新 claim 同 Job | 旧 token 任何调用 stale；新 Attempt 正常；无重复 active attempt |
| P-09 | E9 零写放弃的收敛：heartbeat 停发（模拟）→ 跑 recover_expired | job retry_wait（LEASE_EXPIRED）→ 重领成功；**证明放弃=等价收敛**（矩阵 §5） |
| P-10 | E10 本地 deadline 备份：DB 不可达 + 到期 | KillOnly + 零写；恢复扫描 GENERATION_TIMEOUT（lease==deadline 夹逼） |
| P-11 | 迟到写防护回归（03B-01 门禁保持）：终态后 complete/fail/heartbeat | 全部 stale/stale_attempt；DB 不被覆盖 |
| P-12 | K=2 容忍：单次 heartbeat 失败（注入一次 Err） | 不终止 Executor；下一拍成功继续 |
| P-13 | Worker role 权限回归 | 沿用 worker_roles/roles_separation：零业务表 DML（新代码不新增授权面） |
| P-14 | 白名单纪律：fail(EXECUTOR_CRASHED, retryable=true) | DB 实际不可重试（job failed）——自报被白名单拦截 |

## 3. L3 真实进程树测试（新测试 crate `tests/process_tree.rs` 或独立 harness）

| ID | 用例 | 断言 |
|---|---|---|
| T-01 | 基本树杀：executor 脚本 spawn 2 层孙进程（各自 sleep）→ SIGTERM 组 | 全树终止；死亡确认（waitpid + 组空 / Job 计数 0）在时限内 |
| T-02 | SIGTERM 不从（孙进程 trap TERM）→ 宽限 → SIGKILL 组 | SIGKILL 后组空；宽限计时精确（≤G+ε） |
| T-03 | Windows 挂起创建绑定：spawn CREATE_SUSPENDED → AssignJob → Resume | 全程无"未入 Job 的已运行"窗口（通过 Job 计数在中途恒 1 断言） |
| T-04 | Windows KILL_ON_JOB_CLOSE：Supervisor 测试 harness 自杀（模拟：关 Job 句柄后结束 harness） | 全树（含孙）死亡 |
| T-05 | Linux PDEATHSIG：kill -9 Supervisor harness | Executor 秒级死亡；孙进程（executor 为其设 PDEATHSIG）同死 |
| T-06 | Linux 父 PID 二次检查：在 fork 后、prctl 前杀死父（竞态注入：executor 启动延迟 prctl） | executor 自检 getppid 变化后退出 |
| T-07 | 僵尸语义：组内仅 Z 状态成员 | 视为死亡；死亡确认通过 |
| T-08 | 死亡确认超时（S10/FS-04）：D 状态进程 | 确认失败路径：不清理目录、不 confirm、告警日志 |
| T-09 | 凭据剥离端到端：executor 打印自身环境快照到 result 附带文件 | 环境集合 == 白名单（U-05 的运行时版） |
| T-10 | FD 隔离：executor 枚举 /proc/self/fd（Linux）/句柄（Windows） | 仅 stdio 三个（+平台固有少量，基线快照对比） |
| T-11 | 目录所有权：executor 尝试写白名单外路径（work_root 上一级） | 03B-02 阶段仅断言日志告警（强隔离属 03B-03，L6 激活硬断言） |

## 4. L4 故障注入测试（对应 failure-scenarios-v2 的 FS 编号）

| FS | 注入手段 | 断言要点 |
|---|---|---|
| FS-01 | `kill -9` Worker（Linux）/ `taskkill /F`（Windows），in-flight 中 | （另一进程）恢复扫描收敛 + 进程树不残留（T-05/T-04 的场景化） |
| FS-02 | executor 脚本 `exit 1` / `abort()` | E8 → fail(EXECUTOR_CRASHED) 一次；孙进程仍被组杀（S2 残留义务） |
| FS-03 | 孙进程 `setsid` 后长 sleep | 死亡确认发现组空但逃逸者存活 ⇒ 日志声明 + 不误删目录 |
| FS-04 | D 状态（不可中断 IO）进程 / Windows 拒绝终止的挂起进程 | S10 hard gate 行为 |
| FS-05 | advisory lock 持 job 行 + statement_timeout 生效验证 | T 超时触发 Err → E12 计数；INV-1 独立触发 |
| FS-06 | pgoxy/tcpkill 断连 | S5 行为链；放弃后零写 |
| FS-07 | deadline 边界 ± ms 级并发 cancel（循环 N 次） | 两种裁决都收敛且无中间态（S6） |
| FS-08 | complete 提交后、响应前 RST（pg 层：事务提交后 drop 连接） | U4 放弃；DB succeeded |
| FS-09 | 同 FS-08 手段作用于 fail | U4/U1 分支各覆盖 |
| FS-10 | 排水预算 < executor 剩余时间 | E11：KillOnly→零写→退出码 0；日志含 abandoned 原因 |
| FS-11 | 小配额 tmpfs 挂 work_root | E8 路径 + 日志限流不爆 |
| FS-12 | Windows：外部进程持有目录内文件句柄 | 3 次退避后放弃清理 + 告警 + 后续 Attempt 不受影响 |
| FS-13 | 时钟抽象注入（U-04 运行态） | 墙钟跳变零影响 |

## 5. L5 平台差异矩阵

| 能力 | Linux | Windows | 差异测试 |
|---|---|---|---|
| 组容器 | process group | Job Object | T-01/T-03 |
| 组杀 | kill(-pgid, SIG) | TerminateJobObject | T-01/T-02 |
| 父死回收 | PDEATHSIG（链式，协作） | KILL_ON_JOB_CLOSE（内核，递归） | T-04/T-05/T-06 |
| 死亡确认 | waitpid + 组探测 + /proc Z 语义 | child handle + Job 计数 | T-07/T-08 |
| 优雅信号 | SIGTERM 组 | Terminate 即杀（无宽限语义） | T-02（win 版：直接 Terminate 计时断言） |
| 嵌套 Job 风险 | — | Worker 自身在外层 Job（Win8+ 嵌套） | T-03 变体：外层 Job 包裹 harness |
| 拒死形态 | D 状态 | 挂起 + 内核不可终止 | FS-04 双实现 |
| 孤儿 re-parent | → PID 1（契约：Supervisor 收尸） | → 无（Job Object 吸收） | T-07 |

CI：GitHub Actions ubuntu runner 全量；Windows 进程树用例（T-03/04/08/FS-12）在 `cfg(windows)` 下默认 `#[ignore]`，本机 `cargo test -- --ignored` 执行并纳入验收记录（与用户开发环境一致）；L2/L4 非 Windows 专属用例双平台跑。

## 6. L6 容器生命周期契约测试（03B-03 接口，占位）

- CT-01（占位）：容器销毁 ⇒ cgroup 内进程全灭 ⇒（03B-03 实施）激活。
- CT-02（占位）：Attempt 目录随容器 tmpfs 消失。
- CT-03（占位）：Executor 网络出口默认拒绝（T-11 的硬断言版）。
- 本阶段仅登记契约，不实施（职责边界，主设计 §2）。

## 7. L7 安全隔离测试

- S-01：Worker role 权限面回归（P-13）+ 新增"executor 环境零凭据"静态断言（U-05 + T-09）。
- S-02：`ARTIFACT_WORKER_DATABASE_URL` 不出现在任何子进程环境、命令行、日志（日志扫描断言：trace subscriber 输出全文检索 DSN 关键词）。
- S-03：lease token 不出现在日志/目录/结果文件（同上扫描法；延续 domain Debug 脱敏）。
- S-04：result.json 的 diagnostic_summary ≤2048 且不含输入正文（长文本截断测试）。
- S-05：migrations.sha256 + migration_guard 回归（文档阶段无代码，实施阶段保持门禁）。

## 8. 验收清单（汇总）

- [ ] 矩阵 v2 §3 每一行至少一个对应测试（映射表在实施 PR 中给出）。
- [ ] 故障场景 v2 S1~S13 全部 FS 编号闭环。
- [ ] U/P/T/FS/S 全 ID 通过；Windows ignored 集合有本机执行记录。
- [ ] 提交上界不变量（U-02）与零 DB 写路径（P-03/P-04/P-06b/P-09）为**硬性门禁**。
- [ ] 03B-01 既有测试（worker_coordination / worker_roles / roles_separation / migration_*）零回归。
