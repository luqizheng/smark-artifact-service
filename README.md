# smark-artifact-service

SMARK **Artifact Service** —— AI-PM 的独立产物基础设施：Artifact / 不可变 Version /
外部引用 / Generation Job 的元数据管理与版本化 API。
文件正文由平台 file-service / RustFS 管理，本服务不持有长期对象存储凭据。

架构基线（AI-PM 仓库 `docs/architecture/smark-pm-architecture-decisions/`）：

- `ADR-ART-001`：独立服务、PostgreSQL 元数据 + 对象存储正文、不可变版本
- `ART-01`：V1 API / 数据模型 / 授权委托 / 错误码契约
- `ART-02`：实施前置核验（两个 P0：授权委托、file-service 上传协议）
- 本仓库实施报告：`docs/implementation/artifact-service/`

## 工程结构

```text
apps/artifact-api            HTTP Server（axum；serve / migrate 子命令）
apps/artifact-worker         独立 Worker（Job 协调维护；无 Generator Adapter 时不领取 Job）
crates/artifact-domain       领域模型与不变量（纯规则，无 IO）
crates/artifact-application  端口（ports）与应用服务
crates/artifact-infrastructure  PostgreSQL 仓储、fail-closed 验证器、协议阻断存储端口
crates/artifact-contract     API DTO 与统一错误信封（与 openapi/openapi.yaml 契约测试对齐）
crates/artifact-client       独立 Rust SDK（AI-PM 消费者使用，零 AI-PM 内部依赖）
openapi/openapi.yaml         OpenAPI 3.1 —— 唯一 API 描述来源
migrations/                  服务自有迁移（独立于 AI-PM）
migrations.sha256            已发布迁移的内容哈希；CI 与测试据此拒绝修改历史迁移
tests/                       集成 / 契约 / 边界 / 不变量 / 角色分离测试
deploy/                      Compose 样例 + 数据库角色分离脚本
.github/workflows/           独立 CI（fmt / clippy / build / test / docker）
```

## 本地开发

```bash
cp .env.example .env
# 本地专用 dev 库（或用 deploy/docker-compose.yml 起整套）：
docker run -d --name artifact-dev-pg -e POSTGRES_PASSWORD=artifact_dev \
  -e POSTGRES_DB=artifact_service -p 127.0.0.1:16500:5432 postgres:16

cargo build --workspace
cargo test --workspace          # 需要 DATABASE_URL 指向可建库的 postgres
cargo run -p artifact-api -- migrate
cargo run -p artifact-api       # serve，默认 127.0.0.1:8110
curl -s 127.0.0.1:8110/v1/health
```

### 数据库角色与授权

部署顺序固定为：**迁移，然后执行 `deploy/db-roles.sql`，再启动服务**。
迁移如果重建函数，函数上的 EXECUTE 授权会随旧函数丢失，所以每次迁移之后都必须重新执行一次授权脚本。

用 compose 起整套时，这一步由 `db-grants` 服务自动完成：

```bash
export ARTIFACT_APP_DB_PASSWORD=dev-only
export ARTIFACT_WORKER_DB_PASSWORD=dev-worker      # 两个 dev 口令都必须显式提供
docker compose -f deploy/docker-compose.yml up -d --build
# 需要 Worker 时叠加 override：
docker compose -f deploy/docker-compose.yml -f deploy/docker-compose.worker.yml up -d --build
# 每次迁移之后重新应用授权：
docker compose -f deploy/docker-compose.yml run --rm db-grants
```

用独立 dev 容器时，迁移之后手动应用（把占位符替换成实际角色名）：

```bash
sed -e 's/__MIGRATOR_ROLE__/postgres/' -e 's/__APP_ROLE__/artifact_app/' \
    -e 's/__WORKER_ROLE__/artifact_worker/' \
    -e 's/__MAINTENANCE_ROLE__/artifact_maintenance/' deploy/db-roles.sql \
  | docker exec -i -e PGPASSWORD=artifact_dev artifact-dev-pg \
      psql -U postgres -d artifact_service -v ON_ERROR_STOP=1
```

### Worker

```bash
cargo run -p artifact-worker -- ping           # 验证独立 DB 角色可用
cargo run -p artifact-worker -- recover-once   # 执行一次租约恢复与输入清理
cargo run -p artifact-worker -- run            # 协调维护循环；SIGTERM / Ctrl-C 退出
```

Worker 只读取 `ARTIFACT_WORKER_*` 配置，不读取 API 的 DSN，也不读取用户委托。
配置取值不合法时直接启动失败，不会静默回落默认值。

## 当前状态（ART-02）

SeeCode 架构基线：[`ART-03-seecode-adapter-design-v1.md`](docs/architecture/ART-03-seecode-adapter-design-v1.md)；Job/Worker/SVG 裁决：[`ART-03A-job-worker-svg-decisions-v1.md`](docs/architecture/ART-03A-job-worker-svg-decisions-v1.md)。Mermaid 不属于本服务职责。

- **已实现**：health / ready / generators / Artifact 元数据（创建、查询、分页、
  If-Match 更新）/ References / GenerationJob 查询与取消 / 幂等键 / 统一错误信封。
- **已实现（数据库与 Worker 层）**：Job 输入快照、Attempt 与租约、原子领取、重试退避、
  执行时限、取消协调、排队时限、审计，以及只拥有受控函数权限的独立 Worker 角色。
  调用方可见的结果是 `GenerationJob` 响应里的 `error_code`、`attempt_count`、`max_attempts`
  与 `cancel_requested`。详见 [ART-03B-01 实施报告 §12](docs/implementation/artifact-service/ART-03B-01-IMPLEMENTATION-REPORT.md)。
- **被 P0 阻断（诚实 503，不伪装成功）**：Version 上传会话与 commit
  （file-service upload-intent / checksum / finalize 协议缺失）。
- **未实现（如实 unavailable）**：SeeCode Generator Adapter，
  GenerationJob 创建 → 503 GENERATOR_UNAVAILABLE。Mermaid 由前端 Markdown 编辑器解析与预览，Artifact Service 不提供 Mermaid 专用能力。
  没有 Adapter 的 Worker 不领取任何 Job。
- **授权**：生产验证器 fail-closed——委托签发方存在两个 P0
  （resource token 端点未鉴权挂载、claims 缺 aud/tid）关闭前，
  所有业务端点默认 401/403。**没有运行时绕过开关**；
  测试替身仅存在于 `test-doubles` cargo feature，发布二进制不包含。

详细状态见 `docs/implementation/artifact-service/ART-02-*.md`。
