# WP-AS-FOUNDATION-01 — Artifact Service 独立基础能力审计

> 状态：**进行中 / 文档先行 / 只读审计基线**
>
> 分支：`docs/wp-as-foundation-01-audit`（从 `main` 创建）
>
> 本文件记录本工作包的范围、当前已核验事实、差距假设和后续审计任务。它不是实施批准，也不替代现有 ART-01 / ART-02 / ART-03 决策。

## 1. 目标

验证 smark-artifact-service 是否已经具备独立、可复用的 Artifact 基础设施能力，并明确剩余契约、存储、生命周期、运维和安全工作。

本工作包以仓库当前代码、OpenAPI、迁移、测试、CI 和既有审计报告为证据；区分：

- 已实现且有测试证据；
- 已定义但未实现；
- 已实现但受 P0 门禁阻断；
- 文档过期或相互矛盾；
- 需要产品/架构裁决；
- 需要后续实施工作包。

## 2. 严格范围与非目标

### 本工作包允许

- 只读检查 main、现有 Draft PR 和相关文档；
- 核对 OpenAPI、Rust DTO/SDK、领域模型、迁移、数据库权限、测试和 CI；
- 新增审计/契约文档；
- 在独立分支提交文档并创建 Draft PR；
- 提出后续实施工作包及依赖关系。

### 本工作包禁止

- 修改 LMMS 或 AI-PM 仓库；
- 修改现有业务代码、已发布 migration、Compose、Dockerfile、CI 或运行配置；
- 执行数据库迁移、生产部署、凭据轮换或真实服务调用；
- 开放 Generation Create、启用 Generator Adapter 或 Worker 执行循环；
- 绕过授权委托或 File Service 存储协议 P0；
- 合并本工作包 PR 或其他现有 Draft PR；
- 覆盖、清理或重写其他并行工作产生的分支/代码。

## 3. 当前已核验基线

审计起点为仓库 `main`。以下结论只引用已读取的仓库文件；尚未完成的源码逐项检查不得视为通过。

### 3.1 服务定位与独立性

README 明确将本服务定位为独立 Artifact 基础设施：管理 Artifact、不可变 Version、外部 Reference 和 Generation Job 元数据；正文由平台 File Service / RustFS 管理，本服务不持有长期对象存储凭据。

仓库具备独立 Rust workspace、API Server、独立 Worker 二进制、domain/application/infrastructure/contract/client crate、OpenAPI 3.1、独立 migrations、独立 CI 和 Docker 部署样例。

### 3.2 已实现能力（需进一步对照源码复核）

README 与 ART-02 / ART-03B-01 报告声明已实现：

- Health / Ready / Generator registry；
- Artifact 元数据创建、查询、分页、If-Match 更新；
- References 创建、解除和查询；
- GenerationJob 查询与取消；
- 幂等键、统一错误信封、审计；
- PostgreSQL Attempt / Lease / Recovery 协调基础；
- 独立 Worker DB role 与受控数据库函数；
- Rust SDK 与 OpenAPI 契约测试。

本工作包后续必须逐项核对实现、测试、OpenAPI 和报告的一致性，不能仅凭 README 将其判定为最终验收通过。

### 3.3 明确阻断项

当前仓库文档明确保持两项独立 P0：

1. **授权委托 P0**：LMMS 既有 resource-token 路径存在认证挂载及 claims scope 缺口。Artifact Service 生产验证器保持 fail-closed；不得消费不满足正式契约的 token。
2. **File Service 存储协议 P0**：upload-intent、checksum attestation、finalize、object verification、cleanup/compensation 协议尚未闭环。Version 上传/提交保持 503；不得产生 READY Version。

以上两个门禁互相独立。授权修复不能替代存储协议，存储协议完成也不能解除授权门禁。

### 3.4 生成链路现状

- SeeCode Generator Adapter 未实现，注册状态如实为 unavailable。
- GenerationJob 创建在 Adapter 不可用时必须 fail-closed，返回 503 `GENERATOR_UNAVAILABLE`，不得创建虚假成功 Job。
- Worker 当前只承担协调维护；没有 Adapter 的 Worker 不领取 Job。
- Mermaid 不属于 Artifact Service 职责；由前端 Markdown 编辑器预览，源码仍由 Markdown + Git 管理。

### 3.5 已知文档一致性风险

初步发现 `docs/implementation/artifact-service/ART-02-OPEN-ISSUES.md` 中部分条目可能落后于后续正式决策或 OpenAPI：

- GenerationJob 响应字段及 409/413 已由 `ART-01-V1-API-ERRATA-001.md` 确认，但 OPEN-ISSUES 仍以“待确认 errata”口吻记录。
- Artifact key 冲突的错误语义需要以当前 OpenAPI、实际实现和契约测试重新核对；OPEN-ISSUES 的旧描述可能已过期。
- Archive/Delete 生命周期 API 仍列为未定义/未实现，需要核对 DB 状态机、权限动作和 API 现状。

这些是待核验的文档差异，不在本轮直接修改既有开放问题清单。

## 4. 审计工作分解

### A. Repository / CI / Release 基线

- [ ] 记录 main HEAD、当前所有相关分支及开放 PR 的 base/head/status。
- [ ] 核对 CI workflow 覆盖 fmt、clippy、build、test、Docker、迁移哈希门禁和发布流程。
- [ ] 核对依赖审计、镜像 CVE、SBOM、构建 provenance、签名与发布资产；缺失项分级，不擅自引入工具。
- [ ] 区分仓库 CI 通过、镜像构建、部署、迁移和生产启用证据。

### B. Domain / API Contract

- [ ] 核对 Artifact、ArtifactVersion、ArtifactOutput、Reference、GenerationJob、Attempt 的身份、归属、唯一性和状态机。
- [ ] 核对 tenant/project scope 是否由凭证提供并在所有读写路径验证；请求头仅可作为定位输入，不能成为授权来源。
- [ ] 核对 API、OpenAPI、Rust DTO、SDK、错误码和契约测试一致性。
- [ ] 核对 Idempotency-Key 的作用域、请求指纹、重放响应、冲突语义和并发竞争。
- [ ] 核对 ETag / If-Match、分页 cursor、PATCH 缺省/null 语义。
- [ ] 核对 Reference 的重复创建、解除、目标引用和跨项目边界。
- [ ] 核对 Archive/Delete 是否缺少 API、保留策略、级联策略和审计语义。
- [ ] 明确 API 版本兼容策略及 OpenAPI 唯一权威规则。

### C. Version / Storage Contract

- [ ] 核对 Version 是否不可变、版本号分配是否并发安全、current pointer 是否 CAS。
- [ ] 定义并核对 upload-intent → upload → checksum attestation → finalize → object verification → READY 的完整协议。
- [ ] 核对对象存储键、File Service object ID、tenant/project scope、hash、size、content type 和元数据绑定。
- [ ] 核对上传中断、重复 finalize、客户端超时、对象已上传但 DB 提交失败、DB 成功但响应丢失等补偿路径。
- [ ] 核对孤儿对象清理、保留/删除策略、重试和审计。
- [ ] 确认 Artifact Service 不直接连接 RustFS/S3、不持有长期对象存储凭据。

### D. Authorization / Security Boundary

- [ ] 独立复核 DelegationVerifier 的预期 claims、JWKS issuer、audience、tenant/project、actions、exp/nbf、kid/key rotation、拒绝语义。
- [ ] 核对 issuer 与 verifier 的职责归属，避免 Artifact Service 复制 AI-PM 项目授权决策。
- [ ] 核对默认拒绝、跨租户/跨项目负向测试、对象 ID 探测防护和审计脱敏。
- [ ] 核对 API、Worker、Migrator、Maintenance DB role 的最小权限与真实 grants。
- [ ] 核对密钥/凭据注入、轮换、撤销、日志脱敏、备份与静态加密策略。
- [ ] 安全审查必须独立于功能实现；本工作包不签发安全放行结论。

### E. Job / Worker / Artifact Runtime 边界

- [ ] 核对 Job / Attempt 状态机、lease、deadline、cancel、retry、recovery 的实现与 PostgreSQL 并发测试。
- [ ] 核对 Worker DB role 是否只能调用受控函数，不能直接访问业务表。
- [ ] 核对 Worker 是否具备真实进程执行循环；区分协调 Worker 与 Generator Executor。
- [ ] 复核 ART-03B-02 / 03 的 Changes Required 结论，不将 Draft 设计当作已批准实现。
- [ ] 核对运行时隔离、Start admission、进程死亡证明、输出读取与目录清理门禁。
- [ ] 在执行隔离和输出安全签收未通过前，不启动真实 Generator。

### F. Operations / Observability / Recovery

- [ ] 核对 health/readiness 是否能区分进程存活、DB 可用、File Service 协议阻断和 Generator 可用。
- [ ] 核对日志、trace、request/correlation ID、审计事件和敏感数据边界。
- [ ] 核对 metrics、队列深度、Attempt 延迟、租约恢复、存储失败和安全拒绝指标。
- [ ] 核对备份/恢复、迁移后 grants 重应用、版本升级、回滚和数据兼容策略。
- [ ] 核对部署镜像 digest、配置版本、迁移版本和运行版本是否可追溯。

## 5. 后续交付物

1. `WP-AS-FOUNDATION-01-REPOSITORY-AUDIT.md`：仓库、CI、依赖和发布基线。
2. `WP-AS-FOUNDATION-01-DOMAIN-API-CONTRACT.md`：领域实体、状态机、API/OpenAPI/SDK 一致性。
3. `WP-AS-FOUNDATION-01-STORAGE-LIFECYCLE-CONTRACT.md`：Version 与对象存储一致性、补偿和生命周期。
4. `WP-AS-FOUNDATION-01-SECURITY-BOUNDARY-REVIEW.md`：授权、凭据、数据库角色和威胁模型差距（不替代独立安全审查）。
5. `WP-AS-FOUNDATION-01-TEST-MATRIX.md`：现有测试证据、缺口和建议新增测试。
6. `WP-AS-FOUNDATION-01-FINAL-REPORT.md`：结论、风险等级、阻塞依赖、后续工作包和是否具备进入实施阶段的条件。

## 6. 验收标准

- 每项结论标注证据文件/代码位置/测试名称/CI 运行（可用时）。
- 明确区分 VERIFIED、PARTIAL、NOT IMPLEMENTED、BLOCKED、DOC-DRIFT、NEEDS-DECISION。
- 对已存在的能力不重复造轮子；对未实现能力不以接口存在冒充实现完成。
- 不修改已发布 migrations，不执行迁移，不更改生产开关。
- 明确列出所有 P0/P1；任何 P0 不得通过文档措辞降级。
- 交付文档可独立指导后续工作，不依赖本次聊天上下文。
- PR 保持 Draft，等待人工审查；不自动合并。

## 7. 当前阶段结论

**阶段 0：范围与初始基线已建立。审计尚未完成。**

目前最重要的方向不是增加新的 Artifact 功能，而是完成以下证据闭环：

1. Domain/API 与 OpenAPI/SDK/测试的一致性；
2. Version 与 File Service 的完整提交/补偿协议；
3. 授权委托与数据库角色的独立安全边界；
4. Worker 协调能力与真实执行隔离能力的明确分界；
5. 运维、恢复和供应链证据。

在这些审计完成前，不批准 Generation Create、Worker 执行循环、SeeCode Adapter 或生产部署启用。
