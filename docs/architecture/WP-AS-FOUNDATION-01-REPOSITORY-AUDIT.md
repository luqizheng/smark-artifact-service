# WP-AS-FOUNDATION-01 — Repository / CI / Release 审计

> 状态：PARTIAL（基于 main 文件只读核验；未运行本地构建或 CI）
> 审计对象：`main`，PR #13 创建时 base SHA `b5095cf4a4065d417ae3fed3250dda4cd78d010a`
> 证据范围：Cargo workspace、README、CI/release workflow、Dockerfile、Compose、DB roles 与已开放 PR 元数据。

## 1. 仓库结构

| 项目 | 结论 | 证据 |
|---|---|---|
| 独立 Rust workspace | VERIFIED | 根 `Cargo.toml` |
| 分层 crate | VERIFIED | domain / application / infrastructure / contract / client |
| 独立 API / Worker binary | VERIFIED | `apps/artifact-api`、`apps/artifact-worker` |
| 独立 OpenAPI 3.1 | VERIFIED | `openapi/openapi.yaml` |
| 独立 migrations 与 hash guard | VERIFIED | `migrations/`、`migrations.sha256`、CI |
| 独立 Rust SDK | VERIFIED（结构存在） | `crates/artifact-client`；具体 consumer 覆盖见测试矩阵 |
| 无 AI-PM crate 依赖 | DOC CLAIM / PARTIAL | README 声明；本轮未完成 Cargo metadata 依赖图实测 |
| 无 LMMS / AI-PM 数据库依赖 | DOC CLAIM / PARTIAL | 安全报告声明；本轮未运行全仓 SQL 引用扫描 |

注意：根 Cargo workspace 实际列出 8 个成员（含 tests）；早期 ART-02 实施报告仍写“7 成员”，属于历史报告口径差异，不影响当前 workspace 可见结构，但应避免在新文档继续复用旧数字。

## 2. CI 覆盖

`.github/workflows/ci.yml` 当前包含：

- fmt：`cargo fmt --all -- --check`；
- build-test：clippy `-D warnings`、workspace build、workspace tests、release API/Worker binary build；
- PostgreSQL 16 service；
- `sha256sum -c migrations.sha256` 历史迁移不可变性门禁；
- Docker：API / Worker 镜像构建、未知命令退出码 smoke、Compose 配置校验、grant 脚本 bash 语法检查。

结论：**基础质量 CI = VERIFIED（workflow 配置存在）；CI 最近一次成功状态 = NOT VERIFIED**。本轮针对 PR #13 base SHA 查询 workflow runs 未返回记录，不能把既往 CI 绿灯推断为本次审计分支已通过。

## 3. Release / Supply Chain

`.github/workflows/release.yml` 当前在 `v*` tag 推送时构建并推送 API 与 Worker 镜像，同时发布 GitHub Release。

已确认的差距：

| ID | 发现 | 等级 | 说明 |
|---|---|---|---|
| REP-01 | 无 cargo-deny / 依赖许可证与漏洞策略门禁 | P1 | OPEN-ISSUES #15 已列出，CI 当前无对应 job |
| REP-02 | 无容器镜像 CVE 扫描 | P1 | CI 仅 build，没有 scan 阶段 |
| REP-03 | 无 SBOM / provenance / 镜像签名 | P1 | release workflow 未生成/验证相关证明 |
| REP-04 | Actions 使用版本 tag（如 checkout@v4），未按 commit SHA 固定 | P1 | 供应链可变引用风险；是否统一升级需独立评估 |
| REP-05 | Rust / Debian builder/runtime 使用可变 tag | P1 | 未在 Dockerfile 固定 digest；应由发布策略决定可复现基线 |
| REP-06 | 同时发布版本 tag 与 `latest` | P2 | latest 可变，部署必须使用 digest 或不可变 tag 证据 |
| REP-07 | 无部署、迁移、授权、回滚流水线 | P1 | 当前 release 仅构建/推送镜像和 GitHub Release；生产部署仍需外部 runbook/审批 |

以上是 workflow 文件可见缺口，不等于已经发生供应链攻击。

## 4. Runtime / Compose

- API 与 Worker 分离镜像，均以非 root 用户运行：VERIFIED。
- API Compose 绑定本机 127.0.0.1；开发 PostgreSQL 也绑定本机：VERIFIED。
- Worker override 使用独立 DB DSN/角色：VERIFIED。
- Compose 文件明确为本地开发样例，不应当作生产加固配置：VERIFIED。
- Worker 镜像注释明确只实现协调维护；只读根文件系统、无外网、资源配额属于尚未通过审查的 ART-03B-03：VERIFIED。
- 本轮未执行 Compose 启动或镜像运行：NOT TESTED。

## 5. 当前并行 PR 与实施门禁

本轮查询到的开放 PR（状态以查询时刻为准）：

| PR | 主题 | 当前意义 |
|---|---|---|
| #13 | WP-AS-FOUNDATION-01 | 本审计，Draft |
| #12 | LMMS delegation 审计/契约草案 | 授权契约仍为 DRAFT，不能当作安全放行 |
| #11 | AS-1/AS-2 文档修订 | Draft；含 grants 文档修订 |
| #10 | AS-1/AS-2 主干集成审计 | 历史集成审计 |
| #6 | ART-03B-03 Runtime isolation | 独立复审 Changes Required，不批准实施 |
| #5 | Migration 0004 DB-BLOCKER-01 | 独立复审 Changes Required，不批准实施 |
| #4 | ART-03B-02 Worker process lifecycle | 独立复审 Changes Required，不批准实施 |

#4/#5/#6 的 PR 描述明确：进程取消/死亡确认/迁移竞态/runtime Start admission 仍有阻断，不得以 ART-03B-01 数据库协调能力已实现推断为生成执行安全已完成。

## 6. 结论

- 独立工程骨架和基础 CI：VERIFIED。
- 当前 CI 实际状态、可复现构建、发布资产 digest：NOT VERIFIED。
- Supply-chain 与生产部署链：PARTIAL / P1。
- Runtime 执行隔离：BLOCKED（依赖 #4/#5/#6 的独立裁决和后续实测）。
- 本审计未执行构建、迁移、部署、生产操作。
