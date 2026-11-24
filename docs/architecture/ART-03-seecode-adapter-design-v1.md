# ART-03 — SeeCode Generator Adapter 设计基线

> 状态：设计基线（待项目负责人确认后实施）
> 目标仓库：`luqizheng/smark-artifact-service`
> 基线提交：`561ee7981cb6f7a9ea743ae983e126f3d411851f`
> 上游实现来源：AI-PM `tools/seecode/` 与既有 Visualization SeeCode 集成
> 决策：Mermaid 不属于 Artifact Service；不实现 Mermaid Adapter。

## 1. 目标与非目标

### 1.1 目标

在独立 Artifact Service 中实现 SeeCode Generator Adapter，使其能够：

1. 接收经验证、受限的图形生成输入；
2. 通过独立渲染 Worker 调用固定版本的 SeeCode CLI；
3. 只生成 SVG，不向调用方或浏览器交付中间 HTML；
4. 对 SVG 执行独立安全检查，只有通过检查的输出才能进入文件存储和 READY Version；
5. 将 Job、Version、Output 与 Generator 版本关联，支持幂等、失败、超时、取消和恢复；
6. 与 AI-PM 的业务模型、数据库和项目授权实现保持解耦。

### 1.2 非目标

- 不实现 Mermaid，也不保留 Mermaid 专用 Generator、API、测试或部署依赖。
- 不把 SeeCode 的 HTML、JavaScript、PNG、GIF、MP4 作为 Artifact Service 的输出。
- 不迁移 AI-PM 的 `VizSpec`、项目业务语义、审批、Slot Config、Live Graph 或可见性规则。
- 不让 Artifact Service 访问 AI-PM 数据库或依赖任何 `pm-*` crate。
- 不把 SeeCode CLI 放进 API Server 镜像。
- 不在授权委托或 File Service 上传最终确认协议 P0 未关闭时开放生产生成路径。
- 不把 SVG MIME 类型或扩展名当成安全证明。

## 2. 已核实的上游实现资产

AI-PM 当前已有一套可复用的 SeeCode 技术实现，核验来源：

- `tools/seecode/`：vendored SeeCode 0.6.3，MIT；上游 commit `9967db28e86e0e212d4ddf5a60f90fe62f3436c0`。
- `crates/pm-infrastructure/src/viz_seecode_adapter.rs`：两步 CLI（render → export SVG），SVG only。
- `crates/pm-infrastructure/src/viz_render_sandbox.rs`：子进程隔离、环境白名单、超时、输出上限和临时目录。
- `crates/pm-application/src/visualization/artifact_safety.rs`：SVG 静态化安全检查。
- `crates/pm-application/src/visualization/seecode_compiler.rs`：AI-PM 专用 VizSpec → SeeCode spec 编译器。
- `tools/seecode/UPSTREAM.md`、`LICENSE`、`NOTICE` / 第三方许可材料：版本与许可证记录。

这些资产不能整体复制后直接依赖 AI-PM crate。应逐项拆分：

| 上游资产 | Artifact Service 处理方式 |
|---|---|
| `tools/seecode/` vendor、LICENSE、NOTICE、VERSION、UPSTREAM | 迁入本仓库独立维护；固定版本与 commit |
| Node CLI 两步执行逻辑 | 重构为 Worker 内的 SeeCode Adapter |
| 子进程沙箱 | 重构为 Artifact Worker 通用受限进程执行设施 |
| SVG 安全检查 | 提取为 Artifact Worker 的独立安全模块，补充本仓库测试 |
| `seecode_compiler.rs` | 不直接迁移；其中 VizSpec 语义编译留在 AI-PM，由 AI-PM 转换为服务端公开的 SeeCode 输入契约 |

## 3. 服务边界与部署形态

### 3.1 组件职责

- **Artifact API**：身份委托验证、项目 scope 校验、请求校验、Job 持久化、状态查询与取消；不执行 Node/SeeCode。
- **Artifact Worker**：领取 Job、验证 Job 输入、运行 SeeCode 子进程、检查 SVG、调用 File Service 上传协议、提交 Version/Output 状态。
- **SeeCode Adapter**：将版本化的 SeeCode 输入契约转换为 CLI 所需文件，执行 `render` 与 `export --formats svg`，只返回 SVG 字节和运行元数据。
- **File Service**：唯一对象存储控制面；Artifact Service 不持有长期 S3/RustFS 凭据。
- **AI-PM**：维护 VizSpec 和业务语义；将 VizSpec 编译为 SeeCode 输入契约，并通过 Artifact API 发起生成。业务授权由 AI-PM 签发的短时委托表达。

### 3.2 部署隔离

- API 镜像不安装 Node，不包含 SeeCode vendor。
- Worker 使用独立镜像，固定 Node 20 运行时和 SeeCode vendor 版本。
- Worker 默认无外网出口；网络策略必须在部署层强制执行，不能只依赖 SeeCode 配置。
- Worker 不继承 API 凭据、数据库 DSN、用户 Bearer/JWT、File Service 长期凭据或业务授权上下文。
- Worker 只获得领取/更新 Job 所需的最小服务身份；该身份不得成为用户业务授权替代品。
- Worker 使用独立临时工作目录；每个 Job/Attempt 隔离，结束后清理。
- CPU、内存、PID 数、并发数和执行时长由容器/cgroup/调度器实施硬限制。

### 3.3 数据流

1. AI-PM 通过其业务授权流程取得短时、project/action-scoped delegation。
2. AI-PM 将业务 VizSpec 编译为 SeeCode 输入契约，调用 `POST /v1/artifacts/{id}/generation-jobs`。
3. API 验证 delegation、scope、action、Artifact 状态、generator 状态及输入大小/格式，持久化 queued Job 并返回 202。
4. Worker 原子领取 Job，建立 attempt/heartbeat，读取已持久化输入。
5. Worker 在隔离目录中执行 SeeCode render → SVG export。
6. Worker 对 SVG 执行独立安全静态化检查；检查失败立即拒绝，不能上传或登记 Output。
7. 通过 File Service upload-intent / checksum / finalize / object verification / cleanup-compensation 协议上传 SVG。
8. 在单一数据库事务中登记 Output、将目标 Version 从 pending 转为 ready、将 Job 置为 succeeded；所有对象与元数据补偿路径必须明确。
9. 只有 READY Version 才能成为 Artifact current_version；并发冲突不得覆盖其他请求的 current pointer。

## 4. 输入契约与业务语义隔离

### 4.1 禁止跨边界传递 AI-PM 内部模型

Artifact Service 不接受 AI-PM Rust 类型、数据库行或内部 JSON 结构作为隐式契约。尤其不得引入：

- `VizSpec` Rust crate/type；
- `pm-domain`、`pm-application`、`pm-infrastructure`；
- project requirement/task/ADR 等业务查询或权限逻辑。

### 4.2 版本化 SeeCode 输入契约

建议公开输入 envelope：

```json
{
  "schema": "smark.seecode.input",
  "schema_version": "1",
  "diagram": {
    "type": "architecture",
    "title": "Example",
    "nodes": [],
    "edges": []
  },
  "options": {
    "theme": "light",
    "font_mode": "system"
  },
  "provenance": {
    "source_system": "ai-pm",
    "source_schema": "aipm.vizspec/1"
  }
}
```

要求：

- 具体字段、枚举、必填项、节点/边上限、字符串长度、嵌套深度必须在 OpenAPI/JSON Schema 中定义并版本化。
- `provenance` 仅用于追溯，不参与授权判断。
- 不接受命令、脚本、任意路径、可执行模板、外部 URL、任意环境变量或任意 CLI 参数。
- 用户文本按不可信文本处理；不允许拼接进 shell 命令。
- AI-PM 专属的节点类型、状态、关系、焦点裁剪和来源映射由 AI-PM 编译层完成；Artifact Service 只验证通用 SeeCode 输入契约。
- 超限输入明确拒绝；不得静默丢弃节点/边。若业务方需要聚焦裁剪，应由 AI-PM 编译层显式完成并记录 warnings/provenance。

**实施前置决策**：Generation Job 输入的持久化方式需要结合现有 ART-01 表结构和 File Service P0 再定稿。不能只把请求体留在 API 进程内存，也不能在 File Service 协议未完成时假装具备可靠持久化。评估方案时必须明确输入大小上限、敏感数据保留、重试可重放和删除策略。

## 5. Worker 与 Job 生命周期

### 5.1 Worker 执行模型

- Job 由 PostgreSQL 持久化队列驱动；不引入 Redis 作为第二个 Job 权威状态源。
- 领取必须使用数据库原子 claim（例如 `FOR UPDATE SKIP LOCKED` 或等价 CAS），多 Worker 不得重复拥有同一有效 attempt。
- Worker 以短租约/heartbeat 表达活性；租约过期的 running Job 可被恢复扫描器重新排队或终结。
- 每个 attempt 有独立 attempt id、开始/结束时间、错误码和诊断摘要；不得记录原始输入中的敏感内容。
- 取消是尽力而为：queued/retry_wait 可直接终结；running 需向 Worker 发出取消信号并强杀当前子进程，最终状态由 Job 状态机裁决。
- Worker 崩溃后，未完成 Job 必须可恢复；不能遗留永久 running。
- 重试只针对明确的暂时性故障；输入无效、安全拒绝、版本不兼容等不可重试。
- 生成器版本和 Adapter 版本在 Job 创建时固定，不能在重试中无声切换版本。

### 5.2 失败分类

| 类别 | Job 结果 | 重试 |
|---|---|---|
| 输入 schema/语义校验失败 | failed / GENERATOR_VALIDATION_FAILED | 否 |
| SeeCode 退出非零 | failed / GENERATOR_EXECUTION_FAILED | 默认否；按错误分类 |
| 超时/资源限制 | timed_out / GENERATION_TIMEOUT | 有限重试，受策略控制 |
| SVG 安全检查失败 | failed / OUTPUT_REJECTED | 否；记录安全违规码，不保留可执行产物 |
| File Service 暂时不可用 | retry_wait | 是 |
| File Service checksum/finalize 不一致 | failed 或 quarantine；不得 READY | 依补偿结果决定 |
| Worker 崩溃/租约过期 | retry_wait 或 timed_out | 按 attempt 上限 |
| Job 被取消 | cancelled | 否 |

## 6. SVG 安全签收要求

SVG 是静态文件格式，不代表内容安全。必须使用独立解析器/安全检查模块，不能仅靠正则表达式或 MIME 检查。

至少拒绝：

- `<script>`、事件处理属性（`on*`）；
- `foreignObject`、`iframe`、`object`、`embed`；
- 外部 HTTP(S)、协议相对 URL、外部资源加载；
- CSS `url(...)` 外部引用、`@import`；
- DOCTYPE、ENTITY；
- 任何可执行或导航到外部资源的构造；
- 超出尺寸、元素数、属性数、嵌套深度、路径复杂度或文件大小限制的 SVG。

允许项必须使用 allowlist 明确列出（例如必要的片段引用与经验证的内嵌 data URI）；默认拒绝未知主动内容。

安全模块输出结构化结果：`passed/rejected`、规则版本、违规码、输入/输出 hash。原始恶意 SVG 不得作为可访问 Artifact Output 发布。安全检查失败必须留下审计事件，但不能将失败产物登记为 READY。

## 7. 版本、输出与幂等规则

- 一个生成请求对应一个 GenerationJob；重放相同 Idempotency-Key + 相同请求 hash 返回原 Job。
- 同一 Idempotency-Key + 不同请求 hash 返回 409。
- Job 创建时绑定 generator version、adapter version、输入 schema version、options hash。
- 生成结果写入不可变 ArtifactVersion；SVG 作为 primary Output。
- SVG 对象只有经过 File Service finalize、checksum 与 object verification 才能登记。
- READY Version 不可变；current pointer 更新使用 expected_current_version_id 乐观并发。
- 渲染重试不得重复创建多个逻辑 Version；同一 Job 的 attempt 输出必须先进入隔离暂存区，只有成功提交时登记一个最终 Version。
- Job 的成功状态与 Version READY、Output safety_status=passed 必须在一致性协议中协调；不能出现 Job succeeded 但 Version 非 READY，或 Version READY 但 Job failed 的无解释状态。

## 8. 观测、审计与运维

- 指标：队列深度、排队时长、运行时长、成功/失败/超时/取消数、重试数、Worker 心跳过期数、输出字节数、安全拒绝数。
- 日志使用 request_id、job_id、attempt_id、generator_version、adapter_version；不记录 Bearer/JWT、完整输入或敏感业务文本。
- 审计记录 Job 创建、授权拒绝、取消、重试、完成、失败、安全拒绝和 Version READY。
- Worker 镜像固定 Node、SeeCode vendor、操作系统基础镜像 digest；升级需要依赖/许可证清单和安全回归。
- 生产网络策略、容器资源限额、只读根文件系统、临时目录大小上限、镜像扫描与 SBOM 纳入部署验收。

## 9. P0 门禁与阶段依赖

ART-03 可先完成不接生产流量的代码与测试，但不得绕过以下门禁：

1. **授权委托 P0**：lmms 签发通道必须认证挂载；凭证必须包含并验证 Artifact Service audience、tenant、project、actions、jti、过期时间等必要 claims。
2. **File Service P0**：必须具备 upload-intent、服务端 checksum attestation、finalize、对象验证、幂等补偿与孤儿对象清理。
3. **Job 输入持久化决策**：定义可重放、可限额、可清理的持久化方案。
4. **Worker 隔离验证**：Node 子进程无凭据、无外网、资源受限、临时目录隔离。
5. **SVG 安全验证**：真实 SeeCode 产物与恶意样本双向测试。

P0 未关闭时：生产验证器继续 fail-closed；StoragePort 继续 protocol-blocked；生成 API 不得返回成功 Job 或 READY Version。

## 10. 建议实施拆分与验收

### ART-03A — 输入契约与领域边界

- 冻结 SeeCode 输入 schema / OpenAPI；
- 明确 Job 输入持久化方式；
- 明确 generator/version/options 兼容规则；
- 明确 AI-PM 编译层与 Artifact Service 的责任边界。

验收：Schema golden tests、未知字段/超限/恶意字符串负面测试、AI-PM crate 依赖守卫。

### ART-03B — Vendor 与 Worker 基础

- 固定 vendor 0.6.3 / commit / MIT 与第三方许可；
- 建立独立 Worker 二进制和 Node 20 Worker 镜像；
- 实现原子 Job claim、lease/heartbeat、取消、超时、崩溃恢复；
- API 镜像不包含 Node/SeeCode。

验收：多 Worker 竞争、重复 claim、Worker crash、lease expiry、取消强杀、无凭据继承、无外网测试。

### ART-03C — SeeCode Adapter

- 只实现 SVG 输出；
- render → export 两步执行在同一隔离工作目录；
- 解析 CLI 输出必须验证路径在工作目录内，拒绝绝对路径、路径穿越、符号链接逃逸；
- 输出文件必须通过大小限制、SVG parser、安全检查；
- 记录 generator_version / adapter_version / schema_version。

验收：合法图、中文、空输入、非法输入、CLI 缺失、非零退出、超时、输出超限、确定性与真实产物 Golden Tests。

### ART-03D — File Service 与 Artifact 生命周期集成

- 在 File Service P0 关闭后实现正式 Storage Adapter；
- 上传 SVG 并验证 checksum/finalize/object metadata；
- 建立 Output + READY Version + Job succeeded 一致性和补偿；
- 只在 READY 后允许 current pointer 更新。

验收：对象成功/失败/中断/重复提交/数据库失败/补偿失败矩阵；无孤儿对象；无伪 READY。

### ART-03E — 安全签收与发布

- 双测试门禁：SeeCode Golden/Adapter Contract + SVG 安全恶意样本；
- 镜像扫描、SBOM、许可证清单、资源限制与网络策略验收；
- 更新 OpenAPI、SDK、README、运行手册和发布说明；
- 单独 CI 与发布，不触发 AI-PM 全量测试。

验收：完整端到端 Job → SVG → 安全检查 → File Service → READY Version；两个 P0 未关闭时仅允许测试环境验证，不允许生产流量。

## 11. 本轮必须先解决的设计问题

实现前不得跳过以下设计审查：

1. **输入持久化**：GenerationJob 当前表没有持久化 `input/options` 字段；需要决定结构化 JSONB、File Service 对象，或受控混合方案，并说明敏感数据保留与清理。
2. **Job attempt 模型**：当前表只有 attempt_count，没有 attempt 明细；需决定增加 attempt 表还是可证明恢复正确性的最小模型。
3. **Worker 身份**：Worker 如何 claim/heartbeat/complete Job，如何获得 File Service 最小服务身份，且不继承用户 delegation。
4. **安全解析器**：选择成熟 SVG/XML parser 与 sanitizer 策略，不能将上游 AI-PM 安全模块未经审计照搬。
5. **路径安全**：SeeCode CLI 输出路径必须被限制在本次 attempt 工作目录内。
6. **零外网证明**：CI 中的单元测试不能代替容器/运行环境的网络隔离验证。
7. **Generator 注册状态**：移除 Mermaid seed；SeeCode 仅在 Worker、依赖、配置和安全自检均通过后报告 available，否则保持 unavailable。

---

**当前结论**：ART-03 的方向是“独立 Worker + SVG-only SeeCode Adapter + 独立安全签收”，不是在 API 进程内直接调用 Node，也不是把 AI-PM Visualization crate 搬进来。本文是设计基线，不表示 ART-03 已实现或 P0 已关闭。
