# WP-AS-06 实施报告 — GenerationJob 执行链路（SeeCode Adapter + Worker 执行循环）

> 状态：完成（本地全量 35 测试目标绿；CI 随 PR）
> 分支：`feat/wp-as-06-generation-execution`（基于 main@`ea8cc87`，含 WP-04-R1/05 合并后基线）
> 关联契约：ART-03 seecode-adapter-design-v1、ART-03A §4/§8/§11（A3A-01~08/22）、
> ART-04 §35/§74/§211、WP 契约 v0.3 §12、ART-03B-01 实施报告

## 1. 目标与结果

打通 GenerationJob 执行链路——此前 `request_generation` 是 stub（恒 503）、
Worker 只有协调维护循环、`complete_generation_job_attempt` 不写 Output/Version：

| 环节 | 实现 |
|---|---|
| 创建 | `request_generation` 接线 AS-2 keyed create（单事务、caller_request_id 幂等三态、V10 绑定前置、ECV 前置条件 ART-04 §74）→ 202 created / 200 replay |
| 领取 | Worker 申报 `seecode → svg-as-1` 能力（0006 钉住注册表 adapter_version，claim v2 等值匹配——NULL seed 缺口修复） |
| 渲染 | vendored SeeCode CLI（`tools/seecode` 0.6.3）经沙箱（零凭据 env 白名单 / spec 走 stdin+文件 / 超时强杀 / 输出上限）；两步 render→export svg |
| 把闸 | SVG 静态化检查（A3A-07，ai-pm artifact_safety 原样移植）在**执行器提交边界**——任何 `GeneratorAdapter` 实现都无法绕过 |
| 上传 | WP-AS-05 `FileServiceStorage.upload_object`（恒带 tid，user_id=None） |
| 校验 | `inspect_object` 下载重算 sha256（file-service 无 hash attestation） |
| 提交 | 0006 `commit_generation_output` 单事务：目标 Version 物化/校验 → Output(primary/passed) → pending→ready → Job/Attempt succeeded → audit |
| 收尾 | 取消（heartbeat CancelRequested → 停止渲染（kill_on_drop 强杀）→ confirm_cancelled）；失败按 0003 重试白名单（GENERATION_TIMEOUT/STORAGE_UNAVAILABLE 可重试，其余确定性失败） |

## 2. 交付物

### 2.1 迁移 0006（`0006_generation_output_commit.sql`）
- `artifact.commit_generation_output`：SECURITY DEFINER / PUBLIC REVOKE /
  worker role 授权（db-roles.sql）；lease/cancel 校验沿用 0002 语义；审计写
  `generation.complete`（不含输入正文/租约 token）。
- 注册表翻转：`seecode` → `available` + **`adapter_version='svg-as-1'`**（NULL
  seed 会使 claim v2 等值匹配永假——Job 永不可领取的缺口）+ status_reason。
- `migrations.sha256` 增行；migration_guard/api_meta/worker_roles 断言同步。

### 2.2 端口与服务层
- `WorkerCoordinationPort::commit_output` + `GenerationOutputFacts` +
  `CommitOutputOutcome(Committed(VersionId)/Stale/CancelRequested)`；PG 实现映射
  CANCEL_REQUESTED token / INVALID_OUTPUT_*（400）/ TARGET_VERSION_NOT_PENDING（409）。
- `request_generation` 服务重写：授权 → V10 `enforce_write_binding` → Artifact
  检查 → ECV 前置条件（409）→ `ValidatedCreateGenerationJobRequest::try_new`
  （信封 allowlist + 规范化）→ `KeyedGenerationJobCreatePort`（幂等三态）→
  审计（denial/allowed）。`JobResolution` + `X-Generation-Job-Resolution` 头
  （created / caller_request_replay / idempotency_replay）。
- `GenerationJobResponse.target_version_id` 加法性暴露（ART-04 §211 消费方契约缺口）。

### 2.3 Worker（apps/artifact-worker，lib+bin）
- `render_sandbox.rs`：ai-pm `viz_render_sandbox` 原样移植（零凭据白名单 /
  stdin 传参 / 超时 kill_on_drop / 输出上限 / UTF-8 安全截断；8 单测）。
- `svg_safety.rs`：ai-pm `artifact_safety` 原样移植 + `SANITIZER_POLICY_VERSION`
  （"svg-allowlist-1"，入 manifest）；15 单测（正转义不误报等实测形态全保留）。
- `seecode_adapter.rs`：去 pm-* 依赖移植——`GeneratorAdapter` trait +
  SeeCodeAdapter 两步渲染 + `DIAGRAM_TYPES` 清单（与 vendored schemas 目录
  1:1，单测锁定）。
- `execute.rs`：执行循环——claim（能力表）→ 渲染（心跳 1/3 续租，select 竞争；
  取消/超时 drop 渲染 future → 子进程强杀）→ 提交边界安全闸 → 上传 → inspect
  sha256 → `commit_output`；补偿删除仅用于本 Worker 上传物（D-W05-3）；失败
  语义按白名单（§3 D-W06-4）。
- `config.rs`：执行面五项 all-or-nothing（SEECODE_CLI_PATH + FILE_* 三项 +
  FILE_LMMS_TID）+ LEASE_SECONDS；全缺 = 协调维护-only（原姿态）。32 单测。

### 2.4 vendor 与镜像
- `tools/seecode/`：0.6.3 全套迁入（scripts/schemas/assets/LICENSE/NOTICE/
  THIRD_PARTY_LICENSES/UPSTREAM/VERSION；上游 commit 9967db28，MIT）。
- `Dockerfile.worker`：runtime 层加 `nodejs`（**Node 只进 Worker 镜像**，
  ART-03 边界不变）+ vendor 拷贝至 `/opt/seecode`。
- `deploy/docker-compose.worker.yml`：执行面 env（默认全空 = 协调-only）。

### 2.5 E2E（tests/wp06_execution.rs，5 场景）
真实 `Executor`（真实 PgWorkerCoordination + 真实 FileServiceStorage ↔ loopback
fake LMMS）+ 可编程 FakeRenderer：e1 全链（Job succeeded / Version READY +
manifest / Output passed / audit）、e2 安全违规零残留、e3 暂时性失败 retry_wait、
e4 输入拒绝 failed、e5 对象面断供 STORAGE_UNAVAILABLE。

## 3. 决策记录

| # | 决策 | 依据 |
|---|---|---|
| D-W06-1 | 目标 Version 在提交事务内物化（target_version_id 为空则插入 pending 并链接，非空则校验 pending）；响应暴露 target_version_id。 | ART-04 §211；0005 受控函数不动（迁移不可变） |
| D-W06-2 | 输入 schema 冻结 = 信封 allowlist（既有 job_input）+ `diagram.type` 清单（与 vendored schemas 1:1，单测锁定）；逐字段 JSON Schema 校验（jsonschema 依赖）为后续增强——CLI 自身对非法 spec 返回 ok:false → Job failed；沙箱覆盖注入面。 | A3A-22 过渡形态 + ART-02 开放项 #21 |
| D-W06-3 | Worker 对象面 tid = 配置常量 `ARTIFACT_WORKER_FILE_LMMS_TID`（单租户）；Job 行持久化调用方 lmms_tid 需扩展 0005 受控函数（新迁移），在 AI-PM 侧接入（多租户签发）前不阻塞 dev 联调。 | F-24 / D-W05-2 |
| D-W06-4 | 失败码映射：确定性（INPUT_REJECTED/RENDER_FAILED/SVG_SAFETY_REJECTED）不重试；暂时性映射到白名单码 STORAGE_UNAVAILABLE（沙箱超时同属此类；deadline 由 heartbeat 侧数据库收尾）。 | 0003 `is_retryable_generation_error` 冻结白名单 |
| D-W06-5 | SVG 安全闸放执行器提交边界而非适配器内部——`GeneratorAdapter` 是可注入接缝，闸在任何实现之外才成立。 | A3A-07 §6 |
| D-W06-6 | 旧 `complete`（0002）保留为协调层成功标记（不写 Output/Version），执行循环的生产完成路径是 `commit_output`；端口文档如实区分。 | ART-03A §8 |

## 4. 残余风险与开放项

- **多租户执行**（D-W06-3）：Worker 对象面 tid 为单租户常量；AI-PM 签发侧落地
  （多租户 Job 创建）前需要 0006 后续迁移在 Job 行持久化 lmms_tid 并由 claim 返回。
- **逐字段 JSON Schema 校验**（D-W06-2）：jsonschema 依赖引入后补齐；开放项 #21
  相应收窄。
- **真实 CLI 冒烟**：vendored CLI 全链冒烟需要 Node 环境（CI runner 无 node）；
  本地 `cargo test -p artifact-worker` 覆盖沙箱/安全/适配器单测，E2E 以
  FakeRenderer 断言链路契约；真实 CLI 渲染质量回归属 SeeCode 上游资产维护。
- **ART-03B-03 容器隔离**（设计 draft PR #6）：进程沙箱已满足 §5.2 四项；网络/
  命名空间隔离仍属 ART-03B-03。

## 5. 测试证据（本地，2026-11-28）

- `cargo test --workspace -- --test-threads=6`：35 个测试目标全部 ok，0 failed
  （含 wp06_execution 5 场景、worker crate 32 单测、wp04/generation/sdk 契约更新）。
- `cargo fmt --all` 干净；`cargo clippy --workspace --all-targets -- -D warnings` 零 error。
- Windows dev PG 并发 PoolTimedOut 为既有环境现象（WP-AS-05 报告 §5 同款），
  以 `--test-threads=6` 规避；CI（postgres 16）无此现象。

## 6. 生产门禁确认

- API 创建路径已接线但生产姿态不变：无受信 delegation issuer → 一切请求 401
  （default-deny）；生成器可用性由注册表状态控制（0006 翻转仅对 seecode）。
- Worker 执行循环默认关闭（执行面五项全缺）；启用需显式配置 file-service 凭证
  与 tid（LMMS grant 表 owner→audience=file-service）。
- 端到端真实流量仍受既有门禁约束：AI-PM 签发侧（阶段 0/1）+ V11 撤销闭环 +
  G7 联调未完成；本 WP 不构成生产开放批准。

## 7. 至此两条核心链路均已闭环

AS 主要功能（Artifact/Version 持久化、References、GenerationJob 创建与执行、
Worker 协调与恢复、委托验证 default-deny）全部就绪；剩余为跨服务前置
（AI-PM 签发侧阶段 0/1、V11 撤销接线、G7 联调）与增强批次（ART-03B-03、
jsonschema、事件通知、指标）。
