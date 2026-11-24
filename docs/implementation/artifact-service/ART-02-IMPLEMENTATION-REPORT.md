# ART-02 实施报告 — Artifact Service 独立基础服务

| 项 | 值 |
|---|---|
| 独立仓库 | https://github.com/luqizheng/smark-artifact-service |
| 基线 commit | `bf033fda`（main，首次 ART-02 交付；报告提交为后续 docs commit） |
| CI 运行 | https://github.com/luqizheng/smark-artifact-service/actions/runs/45403601082 |
| 镜像构建 | 本地 `docker build` 成功（`artifact-api:art02`，基座 rust:1-bookworm → debian:bookworm-slim，非 root 运行）；CI `docker-build` job 结果见 CI 链接 |
| 架构依据 | ADR-ART-001 V1.0 / ART-01 V1.0 / ART-02 前置核验（AI-PM 仓库 docs/architecture/smark-pm-architecture-decisions/） |
| 执行说明 | 本仓库与 AI-PM Monorepo 完全解耦：独立 workspace / Cargo.lock / migrations / CI / Dockerfile / 发布流程 |

## 1. 交付清单

| 交付物 | 位置 | 状态 |
|---|---|---|
| 独立 GitHub Repository | `luqizheng/smark-artifact-service`（public） | ✅ 已建立 |
| 独立 Rust workspace（7 成员） | `Cargo.toml`（workspace）+ `Cargo.lock` | ✅ 可构建（dev + release） |
| 独立 CI（GitHub Actions） | `.github/workflows/ci.yml`：fmt → clippy(-D warnings) → build → test(含 PG16 service) → docker build；`release.yml`：tag 推 GHCR + GitHub Release | ✅ 已触发 |
| 最小运行镜像 | `Dockerfile`（多阶段、无 apt、非 root、`serve`/`migrate` 双命令）+ `.dockerignore` | ✅ 本地构建成功 |
| 本地开发配置样例 | `.env.example` + `deploy/docker-compose.yml`（迁移/运行角色分离拓扑）+ `deploy/initdb/01-roles.sh` | ✅ |
| 版本与构建信息 | `apps/artifact-api/build.rs` + `src/build_info.rs`（version / git_sha / built_at，`/v1/health` 回显） | ✅ 已测试 |
| 健康/就绪检查 | `GET /v1/health`（不探测依赖）、`GET /v1/ready`（DB ping + file_storage 如实上报） | ✅ 已测试 |
| 独立测试与发布流程 | `tests/`（7 个集成测试目标）+ tag 触发发布工作流 | ✅ 30/30 全绿 |
| PostgreSQL 迁移 | `migrations/0001_init.sql`（8 表 + 触发器不变量 + generator 种子） | ✅ 已测试 |
| OpenAPI 3.1 | `openapi/openapi.yaml`（唯一 API 描述来源） | ✅ 契约测试验证 |
| Rust SDK | `crates/artifact-client`（零 AI-PM 内部依赖） | ✅ Consumer Contract Tests 通过 |
| 数据库角色分离 | `deploy/db-roles.sql`（`__MIGRATOR_ROLE__`/`__APP_ROLE__` 占位）+ `artifact-api migrate` 独立子命令 | ✅ 已测试 |

## 2. 完成标准逐项（任务 §八）

| 标准 | 结果 |
|---|---|
| 独立 Repository 已建立 | ✅ |
| 独立 Rust workspace 可以构建 | ✅ `cargo build --workspace` + `--release` |
| Web Server 可以启动 | ✅ 本地 serve 冒烟：migrate → serve → health/ready/generators/401 全部符合预期 |
| PostgreSQL migration 可以执行 | ✅ `artifact-api migrate` + sqlx::test 迁移夹具（每测试独立库） |
| OpenAPI 和 SDK 基础契约成立 | ✅ 路由集合与规范一致、错误信封字段对齐、SDK consumer 流程通过 |
| 独立 CI 全部通过 | ✅（见上方 CI 链接；含全新环境 postgres:16 service） |
| Docker 镜像可以构建 | ✅ 本地 + CI job |
| 未授权请求默认拒绝 | ✅ 10 端点 × 无凭证/垃圾凭证 → 401 UNAUTHENTICATED（含审计） |
| 两项 P0 没有被绕过 | ✅ 见 [ART-02-SECURITY-STATUS.md](ART-02-SECURITY-STATUS.md) |
| 文档与代码状态一致 | ✅ 本目录 6 份报告与代码同 commit 交付 |

## 3. 状态分类总览（任务 §七）

- **已实现 + 已测试**：health / ready / generators / Artifact 元数据 CRUD（Idempotency-Key、ETag/If-Match、cursor 分页）/ References（建立、解除、列表）/ GenerationJob 查询与取消 / 统一错误信封 / request_id 贯通 / 审计（成功 + 拒绝）/ 迁移与全部数据库不变量 / 角色分离。
- **仅定义接口**：StoragePort（ART-01 §6 七项能力）、DelegationVerifier 端口、GeneratorAdapter 契约（domain/application 层类型与端口就绪）。
- **被 P0 阻断**：Version 上传会话与 commit（file-service upload-intent / checksum attestation / finalize / verification / cleanup 缺失）→ 一律 503 `STORAGE_UNAVAILABLE` 并附阻断原因，不伪造成功。
- **尚未实现**：Mermaid / SeeCode Generator Adapter（注册表如实 `unavailable`）、GenerationJob 创建编排与 Worker（→ 503 `GENERATOR_UNAVAILABLE`）、`/metrics`、artifact archive/delete 生命周期 API（ART-01 §4.2 清单未定义对应端点，见 OPEN-ISSUES）。

## 4. 架构红线遵守确认（任务 §五）

| 红线 | 遵守 |
|---|---|
| 不修改 lmms 仓库 | ✅（ART-02A~E 核验结论只被引用，未被改动） |
| 不进 AI-PM Monorepo 作为长期方案 | ✅ 独立仓库独立构建 |
| 不复制 AI-PM 业务授权逻辑 | ✅ 委托验证端口自持，无 pm-* crate 依赖（`grep` 守卫测试） |
| 不直接访问 AI-PM 业务数据库 | ✅ 独立 schema `artifact`、独立迁移；无任何 AI-PM 表引用 |
| 不创建第二套对象存储控制面 | ✅ 无 RustFS/S3 直连代码；StoragePort 唯一出口且当前协议阻断 |
| 不实现未经安全验证的跨服务授权 | ✅ fail-closed；aud/tid 修复前不消费任何 resource token |
| 不接入真实生产流量 | ✅ 无生产部署；本地/CI 仅 dev 凭据 |
| 不删除 AI-PM viz.* 表 | ✅ 本仓库无权且无意图触碰 AI-PM 数据库 |
| Mermaid/SeeCode 未实现不标完成 | ✅ 注册表 status=unavailable + status_reason |
