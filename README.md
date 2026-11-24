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
crates/artifact-domain       领域模型与不变量（纯规则，无 IO）
crates/artifact-application  端口（ports）与应用服务
crates/artifact-infrastructure  PostgreSQL 仓储、fail-closed 验证器、协议阻断存储端口
crates/artifact-contract     API DTO 与统一错误信封（与 openapi/openapi.yaml 契约测试对齐）
crates/artifact-client       独立 Rust SDK（AI-PM 消费者使用，零 AI-PM 内部依赖）
openapi/openapi.yaml         OpenAPI 3.1 —— 唯一 API 描述来源
migrations/                  服务自有迁移（独立于 AI-PM）
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

## 当前状态（ART-02）

- **已实现**：health / ready / generators / Artifact 元数据（创建、查询、分页、
  If-Match 更新）/ References / GenerationJob 查询与取消 / 幂等键 / 统一错误信封。
- **被 P0 阻断（诚实 503，不伪装成功）**：Version 上传会话与 commit
  （file-service upload-intent / checksum / finalize 协议缺失）。
- **未实现（如实 unavailable）**：Mermaid / SeeCode Generator Adapter，
  GenerationJob 创建 → 503 GENERATOR_UNAVAILABLE。
- **授权**：生产验证器 fail-closed——委托签发方存在两个 P0
  （resource token 端点未鉴权挂载、claims 缺 aud/tid）关闭前，
  所有业务端点默认 401/403。**没有运行时绕过开关**；
  测试替身仅存在于 `test-doubles` cargo feature，发布二进制不包含。

详细状态见 `docs/implementation/artifact-service/ART-02-*.md`。
