# WP-AS-FOUNDATION-01 — 阶段性审计报告

> **结论：PARTIAL / NOT READY FOR IMPLEMENTATION APPROVAL**
>
> 本文为阶段性审计结论。审计计划中的源码逐项核验、SQL 函数安全审查、测试执行和独立安全复审尚未全部完成。

## 1. 审计范围与证据限制

- 审计仓库：`luqizheng/smark-artifact-service`。
- 主要事实基线：main；PR #13 创建时 base SHA `b5095cf4a4065d417ae3fed3250dda4cd78d010a`。
- 已读取：Cargo workspace、README、ART-02 API/security/open issues、OpenAPI、领域模型、应用端口、Worker/API 入口、StoragePort、DB roles、migrations 0002/0003、CI/release workflow、Docker/Compose。
- 已查询：开放 PR #13/#12/#11/#10/#6/#5/#4。
- 未执行：本地 cargo build/test、数据库迁移、Docker 构建/运行、生产部署、真实 JWKS 验签、渗透测试、恢复演练。
- 当前所有结论必须按 VERIFIED / DOC-VERIFIED / PARTIAL / BLOCKED / NEEDS-DECISION 区分；不以历史报告代替本轮测试。

## 2. 当前总体判断

Artifact Service 已经不是空壳：它拥有独立 Rust workspace、HTTP API、领域/应用/基础设施/契约/SDK 分层、OpenAPI、PostgreSQL migrations、独立 API 与 Worker binary、角色分离和基础 CI。

但“核心元数据服务已有基础”不等于“跨系统产物生命周期和生成执行已经可以生产使用”。

### 已具备

- Artifact / Version / Reference / GenerationJob / Attempt 的领域模型。
- Artifact 元数据、引用、Job 查询/取消等 API 基础。
- OpenAPI 3.1 作为唯一 API 描述来源。
- Idempotency-Key、ETag/If-Match、cursor pagination、统一错误 envelope。
- PostgreSQL Attempt/Lease/Recovery 协调函数与独立 Worker DB role。
- 生产授权默认拒绝；File Service 协议阻断时返回 503。
- 独立 CI、API/Worker 镜像、开发 Compose 样例。

### 仍然阻断

| ID | 风险/缺口 | 等级 | 当前状态 |
|---|---|---|---|
| FND-P0-01 | Issuer/Delegation claims/JWKS 授权闭环 | P0 | BLOCKED；PR #12 仍是 DRAFT 契约候选 |
| FND-P0-02 | File Service upload-intent/checksum/finalize/object verification/cleanup | P0 | BLOCKED；Version 上传/commit 503 |
| FND-P0-03 | Worker 真实执行隔离、Start admission、死亡确认与回收 | P0（启用 Generator 前） | BLOCKED；PR #4/#5/#6 相关设计仍 Changes Required |
| FND-P1-01 | Archive/Delete HTTP lifecycle API 缺失 | P1 / NEEDS-DECISION | Domain 与 Action 有定义，API 无端点 |
| FND-P1-02 | 完整 SeeCode diagram schema 未冻结 | P1（Adapter 前） | BLOCKED |
| FND-P1-03 | Supply-chain 安全（依赖审计、镜像扫描、SBOM、签名、provenance） | P1 | NOT IMPLEMENTED |
| FND-P1-04 | StoragePort 能力映射与语义不清 | P1 | DOC-DRIFT / NEEDS-DECISION |
| FND-P1-05 | API 文档开放问题列表与当前 OpenAPI 不一致 | P1 | DOC-DRIFT |
| FND-P2-01 | PATCH 显式 null 语义 | P2 | OPEN |
| FND-P2-02 | Generator registry 是否公开 | P2 | NEEDS-DECISION |
| FND-P2-03 | /metrics、部署恢复手册与演练 | P2/P1 | PARTIAL |

## 3. 关键审计发现

### FND-01 — OpenAPI 与旧开放问题文档漂移

`ART-02-OPEN-ISSUES.md` #3 仍记录 artifact_key duplicate 映射 400 INVALID_REQUEST，并建议新增 409；当前 OpenAPI 的 `POST /v1/artifacts` 已定义 409 ARTIFACT_KEY_CONFLICT / ARTIFACT_KEY_LEGACY_UNVERIFIABLE。需要以当前 OpenAPI、实际 handler 和测试三方复核后，更新旧问题清单，不能继续把它作为待裁决问题。

### FND-02 — StoragePort 方法数与能力语义需核对

安全报告称 StoragePort 七项能力；当前已读取的 Rust trait 有六个方法。且 inspect_object 与 head_object 返回同一 ObjectInspection，差异未在接口注释中定义。需对照 ART-01 §6 做映射，确认遗漏还是文档计数错误。

### FND-03 — Domain 生命周期存在，HTTP 生命周期入口缺失

Artifact Domain 定义 active→archived→deleted，Action 集合也有 artifact:archive/delete；API 状态表没有 archive/delete 端点。应裁决 V1 是否刻意只提供逻辑状态底座，还是需要 ART-04 扩展 API。不能因 DB 状态机存在就声称完整生命周期可由调用方操作。

### FND-04 — Worker 协调与执行隔离是两个不同阶段

当前 Worker 仅执行协调维护，不 claim Job；这符合 fail-closed 的现状。但 PR #4/#5/#6 描述了取消/进程死亡/Start admission/迁移竞争的未决问题。数据库 Lease/Attempt 完成不代表 OS 进程树已经终止，更不代表可以安全读取输出或清理工作目录。

### FND-05 — 发布链缺少供应链证明

CI 有 fmt/clippy/build/test/PostgreSQL/Docker/migration hash；release 有 tag 镜像推送与 GitHub Release。但没有可见的 cargo-deny、镜像 CVE scan、SBOM、provenance、签名或部署 digest/回滚证据。列为 P1 发布加固。

## 4. 后续工作包建议

| 顺序 | 工作包 | 前置条件 | 交付 |
|---|---|---|---|
| 1 | WP-AS-FOUNDATION-01-R2 源码/SQL/测试补证 | 本阶段文档审计 | handler/repository/SQL 函数逐项证据、并发测试与当前 SHA CI |
| 2 | WP-AS-CONTRACT-02 Lifecycle & Storage Contract | API owner + File Service owner 确认 | StoragePort 映射、Archive/Delete 决策、upload/finalize 协议 |
| 3 | WP-AS-SEC-02 Independent Security Review | PR #12 契约收敛但不等同通过 | 独立安全挑战、issuer/JWKS、claims、DB function 权限与部署审查 |
| 4 | WP-AS-SUPPLY-01 Release Integrity | 发布责任人确认 | 依赖审计、镜像扫描、SBOM、provenance、签名、digest 部署 |
| 5 | ART-03B-02/03 设计重新裁决 | #4/#5/#6 Changes Required 项关闭 | 真实 runtime feasibility、取消/死亡确认、并发与恢复测试 |
| 6 | ART-04 Lifecycle API | #1/#2 完成裁决 | Archive/Delete、保留/删除、审计与权限契约 |

## 5. 本轮不批准的事项

- 不批准 Generation Create 生产开放。
- 不批准 SeeCode Generator Adapter 接入。
- 不批准 Worker 执行循环或运行时容器隔离实现直接进入主干。
- 不批准 Version READY 路径开放。
- 不批准使用 LMMS 旧 resource-token endpoint。
- 不批准任何绕过 fail-closed 的临时配置。
- 不批准合并本审计 PR 或现有 Draft PR。

## 6. 阶段结论

Artifact Service 的独立服务基础和元数据领域方向成立；生产授权、文件版本提交和生成执行三条关键路径仍有明确门禁。

下一步应先完成 R2 源码/SQL/测试补证和跨服务契约确认，而不是继续增加生成器功能。当前 PR 保持 Draft，等待人工审查。
