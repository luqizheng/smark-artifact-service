# ART-03B-02 — 决策记录 v2

> 状态：**v2 设计基线（待人工评审）**。记录 ART-03B-02 重新设计的关键架构决策：每项给出推荐、备选、理由、代价、未决问题。**标 🔴 的决策必须人工确认后方可实施**。文末为反向架构审查（任务 §九 13 问）与 OPEN ISSUES 汇总。

## 决策索引

| ID | 决策 | 状态 |
|---|---|---|
| DR-1 | 每 Attempt 独立 Executor 进程，禁复用 | 推荐，默认采纳 |
| DR-2 | Supervisor 并发度 = 1（串行 Attempt） | 推荐，须确认 |
| DR-3 | Executor 环境变量 allowlist 具体清单 | 🔴 须人工确认 |
| DR-4 | lease/4 + 时序参数族（L=60,H=15,T=10,K=2,M=12,G=10） | 推荐，须确认 |
| DR-5 | 终态提交不确定 → heartbeat 探针协议 | 推荐，默认采纳 |
| DR-6 | 移除 EXECUTOR_KILLED 的 DB 提交路径 | 🔴 须人工确认 |
| DR-7 | 只读状态探针函数（未来受控 Migration 提案） | OPEN，不实施 |
| DR-8 | E10 本地 deadline 备份计时触发 → 零 DB 写（交恢复扫描） | 推荐，须确认 |
| DR-9 | 死亡确认失败 = hard gate（不清理、不 confirm、不声称安全） | 推荐，默认采纳 |
| DR-10 | 开发环境持久目录 7 天阈值启动清理 | 推荐，须确认 |
| DR-11 | Windows 优雅终止无宽限（Terminate 直接执行） | 推荐，须确认 |
| DR-12 | stub executor = `artifact-worker exec` 子命令 | 推荐，默认采纳 |

---

## DR-1 每 Attempt 独立 Executor 进程

- **推荐**：每 Attempt spawn 独立 Executor 进程，Attempt 结束进程必亡，禁止在同进程复用多个 Attempt。
- **备选**：(a) 进程内线程/task 隔离（v1 备选）；(b) Executor 池化常驻 + 按 Attempt 分派。
- **理由**：
  1. 凭据剥离只能以进程为边界强制（`env_clear`+allowlist 只在 exec 时生效；池化/线程形态下执行代码与凭据同处一址空间，崩溃/hung 代码可直接读环境）。
  2. hard-kill 粒度 = Attempt：进程复用会把终止语义耦合到无辜 Attempt（kill 池进程 = 杀掉正在跑的另一个 Attempt）。
  3. 崩溃、内存泄漏、fd 泄漏不跨 Attempt 传播——重复执行安全性（主设计 §4.4）需要"每次执行从零开始"的强假设。
  4. 状态机闭合的简化：Executor 生命周期 = Attempt 生命周期一一映射，无池调度这一额外状态机。
- **代价**：每 Attempt 一次进程创建（实测 ~10ms 级）+ 内存峰值；相对生成任务时长可忽略。
- **边界**：孙进程纪律（主设计 §6.2）是 Executor 自身契约，03C 实现遵守，03B-02 用测试验证。
- **未决**：无。

## DR-2 Supervisor 并发度 = 1

- **推荐**：v2 范围内每 Worker 同时只跑 1 个 in-flight Attempt。
- **备选**：(a) N 并发 slot（每 slot 独立状态机实例）；(b) 多进程 Worker 拓扑。
- **理由**：状态机、排水、死亡确认、磁盘/内存核算在串行模型下可完全闭合论证；并发扩展不改变任何 DB 契约（claim 原子性由 DB 保证），可平滑后移。吞吐不足时（SeeCode 渲染 CPU 密集但秒级）先横向加 Worker 容器。
- **代价**：单 Worker 吞吐上限 = 1/max(任务时长, L)；长任务期空转 claim 轮询（维护循环仍工作）。
- **未决**：实施前确认目标吞吐量级（当前无生产流量，串行足够）。

## DR-3 🔴 Executor 环境变量 allowlist 清单

- **推荐初始清单**（deny-by-default 之下的唯一白名单）：
  `ARTIFACT_ATTEMPT_ID`、`ARTIFACT_JOB_ID`、`ARTIFACT_WORK_DIR`、`ARTIFACT_EXECUTOR_PROTOCOL=1`、`PATH`（受控最小值：仅系统必要目录 + 03C vendor runtime 声明路径）。
- **备选**：完全零环境（信息全部走 stdin envelope / result 协议字段）。
- **理由**：标识类变量便于日志关联与目录解析；PATH 由 03C Node runtime 实际需求决定。
- **代价**：清单与 03C 需求耦合，须在其落地时复核。
- **未决/须确认**：① PATH 是否可完全省略（03C 用绝对路径调用 node？——倾向是，则清单更小）；② 是否需要 locale/timezone 类变量以稳定渲染输出（SeeCode 文本渲染可能敏感）。**此项必须人工确认后冻结**。

## DR-4 lease/4 时序参数族

- **推荐**：L=60（默认）、H=L/4=15、T=10、K=2、M=12、G=10；启动期强制校验 `K·H+T+M ≤ L`（⇒ L≥45）。
- **备选**：lease/3（v1 草案，未论证）；lease/2。
- **理由**（完整不等式推导见主设计 §7）：
  - INV-1（先停后失约）：终止发起 ≤ last_renew+L−M，本地单调钟，不依赖任何 DB 可达性。
  - INV-2（两连败检测）：2·H+T=40 ≤ L−M=48，余量 8s。
  - lease/3（H=20）：2·20+10=50 > 48 ❌ 贴界无余量；要满足须 T≤8（DB 正常慢即误杀）。
  - lease/2（H=30）：K 只能=1，单次网络抖动即杀 Executor。
  - lease/4 是 INV-2 反推结果：**既有两连败容错，又容得下 10s 真实 RPC 超时**。
- **代价**：小租约（<45s）不可配（fail-fast 校验拒绝）；如未来需要短租约须等比缩 T/M。
- **未决**：deadline 很短（max_execution_seconds 下界 1s）时 M=12 > 执行期——已由 INV-5 的 min() 取早者处理（租约与 deadline 双夹逼，安全方向仍正确：提前终止，DB 权威裁决照常）。

## DR-5 终态提交不确定 → heartbeat 探针协议

- **推荐**：complete/fail/confirm 的 Err 进入 Uncertain：以 heartbeat 为探针——`renewed/cancel_requested/deadline_exceeded`（蕴含 attempt 仍 running）⇒ 原提交未落地 ⇒ 单次补交；`stale` ⇒ 放弃（二义但动作同构）；探针 Err ⇒ 放弃等租约恢复。
- **备选**：(a) 无条件重试 N 次；(b) 一律当 stale 放弃；(c) 新增只读状态查询函数（须 migration）。
- **理由**：
  - (a) 违反任务红线（不能无条件重试）；且成功落地后的重试虽被 DB stale 免疫，但属无谓写放大。
  - (b) 丢弃本可挽救的提交（探针 renewed 证明可安全补交——见主设计 §5.4 正确性公理）。
  - (c) 当前**正确性不需要**：stale 二义的两个成因对应同一合规动作（零写+等待权威收敛）；新增函数是可观测性优化 → 移入 DR-7 作为未来提案。
- **代价**：U4 场景的终局成因不可区分（日志只能记二义）；补交窗口内若恰逢 cancel 登记，补交返回 cancel_requested 正常转取消（无额外风险）。
- **未决**：无（正确性论证完整；观测性缺口归 DR-7）。

## DR-6 🔴 移除 EXECUTOR_KILLED 的 DB 提交路径

- **推荐**：`EXECUTOR_KILLED` 不再作为任何 fail() 调用的错误码（v1 草稿曾用它表达"被我方杀死"）。Worker 主动终止仅出现在四类场景，归宿分别为：cancel→confirm_cancelled；deadline→零写（DB 已收尾或恢复扫描同语义收尾）；租约安全/排水超时/不确定性→零写（恢复扫描 LEASE_EXPIRED 可重试收敛）。**无一处需要 fail(EXECUTOR_KILLED)**。该码保留为日志级标识。
- **备选**：保留提交但 retryable=false（v1 方案）。
- **理由**：
  1. v1 方案的 kill→fail(EXECUTOR_KILLED) 路径会把 Job 打成**不可重试终态 failed**——而租约安全/排水场景下放弃提交（零写）经恢复扫描以 LEASE_EXPIRED **可重试**收敛（矩阵 §5）。v1 实际损失了重试语义。
  2. 任务红线明令：不得把主动 kill 记为可重试、不得用该码制造重试。移除提交路径是对两条红线的同时满足。
  3. 若保留提交码但永不调用，则它是死契约——不如显式移除并记录。
- **代价**：DB 侧无法区分"worker 失联（LEASE_EXPIRED，恢复扫描归类）"与"worker 主动放弃"——都收敛为 LEASE_EXPIRED。可接受：两者对 Job 的正确处理相同（重试）。
- **未决**：**须人工确认**：是否需要 DB 侧可观测的"主动终止"标记？若需要，属 DR-7 migration 的 scope（如审计 detail 扩展），而非恢复提交路径。

## DR-7 只读状态探针函数（未来受控 Migration 提案）

- **状态**：OPEN，本阶段不实施（红线 3.2：不自行加 migration）。
- **提案草案**（供未来 0004 评审）：`artifact.get_generation_job_attempt_status(job_id, attempt_id, lease_token) RETURNS text`——SECURITY DEFINER + token 归属校验 + 返回 attempt/job 状态（不含输入正文）；仅授权 Worker role。用途：U4 二义消解、DR-6 观测标记、运维诊断。
- **必要性论证**：非正确性必需（DR-5 已论证），纯可观测性/诊断增益。**是否值得一个 migration 的审计与权限成本，须人工裁决**。
- **兼容性预研**：纯新增函数 + GRANT，零表结构变更，`migrations.sha256` 追加行；db-roles.sql 增一行 GRANT；不触碰 0001~0003。

## DR-8 本地 deadline 备份计时触发后的处置

- **推荐**：本地 deadline 计时到点（Executor 仍在跑、DB 不可达或未及 heartbeat）→ KillOnly + **零 DB 写**，交恢复扫描收尾（其归类 GENERATION_TIMEOUT 与主动 fail(GENERATION_TIMEOUT, retryable=true) 完全同语义，D10）。
- **备选**：kill 后若 heartbeat 可达则主动 fail(GENERATION_TIMEOUT, true)。
- **理由**：临界点租约余量 ≈ 0（lease ≤ deadline 恒成立，D1），fail 有 stale_attempt 风险（D7）；恢复扫描归类与主动提交**等价**（同码同重试语义同终态）⇒ 无收益的提交不值得冒不确定提交（Err → Uncertain 链）的复杂度。DB 的 deadline_exceeded 才是权威裁决，本地计时仅提前止损。
- **代价**：该场景 Job 重试等待多一个恢复扫描周期（≤维护间隔 10s）。
- **未决**：无。

## DR-9 死亡确认失败 = hard gate

- **推荐**：死亡确认（主设计 §6.3）超时未通过 ⇒ 不清理目录、不 confirm 取消、不声称"安全终止"；结构化日志升级。宁可让 Job 走零写放弃（恢复扫描重试收敛），不冒"对活树宣称已死 / 删掉在被写的目录"的风险。
- **备选**：放行（日志降级为 warn，继续清理/confirm）。
- **理由**：任务红线原文"如果不能确认整个进程树已停止，就不能声称 Attempt 已安全终止"；放行违反红线且引入真实数据竞争（活进程写目录 + 并发删除）。
- **代价**：极端场景（拒死进程）下目录残留至 7 天/容器回收；cancel 终局延迟一个恢复周期（D11 兜底保证不悬挂）。
- **未决**：无。

## DR-10 开发环境持久目录 7 天阈值启动清理

- **推荐**：开发环境（非容器 tmpfs）下，Supervisor 启动时扫描严格匹配 `jobs/{uuid}/attempts/{uuid}` 的目录，删除修改时间 >7 天者。**不删除任何不匹配模式的路径**；无法与 DB 交叉验证（Worker 无业务表 SELECT 权限，D12——这是权限模型的刻意结果，不是缺陷）。
- **备选**：(a) 不清理（开发目录无限增长）；(b) 启动即全清（危险）。
- **理由**：7 天 = 输入保留期，≥ 任何活跃链生命期（max_attempts≤10 × 退避≤300s + 排队≤24h + 输入保留 7d）——活跃 Attempt 目录不可能被触碰；严格模式校验防误删未知目录（任务 §六 原文要求）。
- **代价**：崩溃残留目录最长滞留 7 天（可观测、可接受）。
- **未决**：阈值是否改 3 天（更激进）——**建议维持 7 天与输入保留期对齐，须确认**。

## DR-11 Windows 无优雅宽限

- **推荐**：Windows 终止 = 直接 `TerminateJobObject`（SIGKILL 语义），不实现 SIGTERM 宽限等价物（CTRL_BREAK 信号需 console 耦合，进程树场景不可靠）。
- **备选**：GenerateConsoleCtrlEvent + 宽限。
- **理由**：优雅终止的价值在"executor 完成收尾写入"——但 Executor 契约下收尾写 result.json 由自身管理，被硬杀只损失该次结果（走 EXECUTOR_CRASHED 不可重试——取消/租约场景走零写不受影响）；CTRL_BREAK 只达直接子进程的 console 组，树覆盖不可靠，引入"半优雅"复杂度不值得。
- **代价**：Windows 上被终止的 executor 无法写"临终遗言"——诊断信息依赖日志管道（已脱敏限流）。
- **未决**：无。

## DR-12 stub executor = `artifact-worker exec` 子命令

- **推荐**：同一二进制的 `exec` 子命令作为 stub Executor（契约实现），集成测试可注入外部契约脚本（`Command` 可替换可执行路径）。无 adapter 能力申报时 Supervisor 不 claim（与 03B-01 行为兼容、空转等价）。
- **备选**：独立 test-only bin。
- **理由**：v1 草稿同一裁决，评审未反对；契约测试需要可注入的替身，子命令形态两者兼得。
- **未决**：无。

---

## 反向架构审查（任务 §九 13 问，独立执行）

**Q1 是否存在 DB 已终态、Executor 仍在运行的窗口？**
存在且**无害**，已显式管理：(a) deadline_exceeded 窗口（E3）——DB 已 timed_out，Executor 仍跑到 KillOnly 完成，期间它只做纯计算与私有目录写入，无外部副作用，无 DB 写；(b) 恢复扫描收尾 vs 僵尸 Executor（W4）——同性质。窗口被压缩：本地 deadline 备份计时提前触发 KillOnly（INV-5）；窗口的终止由状态机保证（E3/E4 均以 KillOnly 收束）。**判定：受控，非风险**。

**Q2 是否可能两个 Attempt 同时产生外部副作用？**
不可能产生**外部**副作用（Executor 纯计算，无凭据无网络面——拓扑保证 + 03B-03 网络拒绝）；两个 Attempt 同时计算是允许的（幂等，私有目录隔离）；唯一外部副作用入口是 03D 受控提交，其前置是有效租约 + 活跃 Attempt 唯一索引 + complete 三重校验（D6）——同一 Job 不可能两个持有权的提交者。**判定：安全**。

**Q3 是否存在取消请求永远无法完成的路径？**
三条取消终局路径全部有兜底：Worker 存活 → C1/C2；Worker 在 CancelSequence 中途死亡 → 恢复扫描 cancel 分支（D11：租约过期 + cancel_requested_at ⇒ cancelled）；死亡确认失败（C 行前置不满足）→ 零写放弃 → 同恢复扫描兜底。`not_requested`（C3）不可达且转告警不循环。**判定：无悬挂路径**。

**Q4 deadline_exceeded 是否可能导致二次终态提交？**
不会。矩阵 E3 行为唯一：KillOnly + 零 DB 写（fail/complete/confirm 一律禁止，fail 必 stale_attempt 已由 D7 核验）。v1 草稿的缺陷（deadline_exceeded → fail 二次提交）在 v2 中被显式移除并有 P-03 测试断言零调用。**判定：闭合**。

**Q5 complete/fail 响应丢失是否可能造成错误重试？**
不会错误重试：落地 ⇒ attempt 终态 ⇒ active 索引清位 ⇒ 恢复扫描不触碰（只扫 claimed/running）⇒ 不重试；未落地+放弃 ⇒ LEASE_EXPIRED 重试（这是**正确**的重试：执行结果确实丢失）。错误重试指"已 succeeded 却重跑"——active 唯一索引 + 终态不可逆（trigger guard）双重阻断。**判定：安全，仅观测性缺口（DR-7）**。

**Q6 Supervisor 崩溃后是否可能残留孙进程？**
Linux：链式 PDEATHSIG 覆盖 Executor 及其 disciplined 孙进程；**逃逸者（setsid/未设 PDEATHSIG 的孙进程）可能残留**——已声明为 V1 残余风险，生产由 03B-03 容器 cgroup 兜底（契约 §6.5-1），非容器开发环境接受风险（FS-03 检测）。Windows：KILL_ON_JOB_CLOSE 内核递归，无此风险。**判定：Linux 有界残余（声明+兜底），Windows 无**。

**Q7 Linux 和 Windows 是否都有可信的进程树回收机制？**
Linux：组杀（主动）+ PDEATHSIG 链（父死）+ PID 1 收尸（僵尸）+ cgroup（最终，03B-03）——四层，前两层是本切片义务，均非 watchdog 轮询。Windows：Job Object（主动 Terminate + 父死 KILL_ON_JOB_CLOSE 内核保证）——单机制双场景覆盖，可信度更高。**判定：双平台可信，Linux 的完整性依赖 03B-03 上线（生产）**。

**Q8 是否存在 Executor 继承数据库凭据或敏感 FD 的路径？**
Env：`env_clear` + allowlist（DR-3）+ 测试断言（U-05/T-09/S-02）。FD：std 进程默认 CLOEXEC，仅 stdio 三管道继承（T-10 断言）。命令行：无凭据（envelope 走 stdin）。日志：DSN/token 零出现（S-02/S-03 扫描）。**判定：无泄漏路径（以测试为证）**。

**Q9 是否可能删除其他 Attempt 的临时目录？**
Supervisor 每 Attempt 只持有自己路径字面量（UUID 组成，创建于 claim 时）；清理动作严格模式校验（U-06：非 UUID 段/symlink/`..` 拒绝）；启动清理只删 7 天+ 严格模式目录（DR-10）。**判定：不存在跨 Attempt 误删路径**。

**Q10 是否有路径把 Executor 主动 kill 错误记为可重试？**
主动 kill 的所有路径：cancel（confirm_cancelled，非 fail）、deadline（零写）、租约安全（零写）、排水（零写）、stale（零写）——**没有任何路径把主动 kill 传给 fail()**；EXECUTOR_KILLED 已从 DB 提交路径移除（DR-6）。DB 侧白名单双重免疫（D9，P-14 回归）。**判定：无**。

**Q11 是否有任何路径绕过 ART-03D 提交协议？**
Executor 无 File Service 凭据/网络面（拓扑）；Supervisor 的 complete 需要 03D 先行登记（D6 OUTPUT_NOT_REGISTERED 否则异常，P-01/T8 处理）；本切片不实现任何上传/READY 逻辑（阶段边界 §2）。**判定：无绕过路径**。

**Q12 是否错误地依赖 RabbitMQ、Redis 或 Worker 本地状态？**
设计零引入：状态判定全部来自受控函数返回值；本地仅只读计时快照（单调钟）；目录非权威。**判定：合规**。

**Q13 是否将 ART-03B-02 的完成误认为生产生成路径已开放？**
主设计 §3.5 + 各文档状态栏显式声明：LMMS delegation 与 File Service 两个 P0 门禁独立；数据库 fail-closed（无 03D 登记 complete 必失败）提供结构性保证。**判定：声明充分**。

**审查结论**：13 问中 12 项闭合；Q6 的 Linux 孙进程逃逸为**已声明的有界残余风险**（依赖 03B-03 生产兜底），登记为 OPEN-1。

## OPEN ISSUES（汇总，含来源）

| ID | 议题 | 来源 | 状态 |
|---|---|---|---|
| OPEN-1 | Linux 孙进程逃逸 pgid（setsid 等）在非容器环境不可可靠回收 | Q6 / V1 | 残余风险，接受；生产依赖 03B-03 契约 §6.5-1 |
| OPEN-2 | confirm_cancelled 返回 not_requested 的可达性（理论不可达） | C3 / D11 | 实施时以 P-02 变体验证不可达性；若触达即契约假设破坏，人工介入 |
| OPEN-3 | U4 stale 二义（提交落地 vs 租约失效）的终局成因不可观测 | DR-5 / DR-7 | 正确性无影响；观测性留待 0004 提案人工裁决 |
| OPEN-4 | Executor env allowlist 终版清单（PATH、locale/tz） | DR-3 | 🔴 实施前须人工确认 |
| OPEN-5 | EXECUTOR_KILLED 从 DB 提交路径移除 | DR-6 | 🔴 须人工确认（含是否需要 DR-7 式观测替代） |
| OPEN-6 | 开发目录清理阈值 7 天 vs 3 天 | DR-10 | 建议 7 天，须确认 |
| OPEN-7 | Windows ignored 测试集的 CI 纳入方式（本机记录 vs 双平台 runner） | 测试 §5 | 实施阶段定，暂定本机记录 + 验收清单门禁 |
| OPEN-8 | 并发度 1 的吞吐上限 | DR-2 | 无生产流量前不裁；扩展路径已论证平滑 |

## 须人工确认决策汇总（评审输入）

🔴 **DR-3**（env 白名单终版）、🔴 **DR-6**（EXECUTOR_KILLED 移除）、DR-2/4/8/10/11（推荐已论证，评审可否决）。确认完成后本文档与设计 v2 即为实施基线。
