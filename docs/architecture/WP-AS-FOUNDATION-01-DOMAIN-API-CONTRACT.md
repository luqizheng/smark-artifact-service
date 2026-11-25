# WP-AS-FOUNDATION-01 — Domain / API Contract 审计

> 状态：PARTIAL；已核验领域类型、OpenAPI、API 状态表和部分应用端口。未完成所有 handler/repository/test 源码逐行复核。
> 依据：main 的 `crates/artifact-domain/src/*.rs`、`crates/artifact-application/src/lib.rs`、`openapi/openapi.yaml`、`ART-02-API-STATUS.md`、`ART-02-OPEN-ISSUES.md`。

## 1. 核心领域模型

| 实体 | 已核验语义 | 状态 |
|---|---|---|
| Artifact | tenant/project 归属、可选 artifact_key、类型、生命周期、current_version 指针、row_revision | VERIFIED |
| ArtifactVersion | Artifact 内单调递增 version_number；pending/ready/failed/quarantined；READY 不可变；仅 READY 可设 current | VERIFIED（领域定义；DB 强制性以迁移/测试另行核对） |
| ArtifactReference | owner_type + owner_id 外部引用，不建立跨服务外键；解除为软删除 | VERIFIED |
| GenerationJob | queued/running/retry_wait/succeeded/failed/cancelled/timed_out；Job 与 Artifact 生命周期分离 | VERIFIED |
| GenerationJobAttempt | 独立 Attempt、lease、deadline、错误码与诊断摘要 | VERIFIED（实现报告与 0002/0003；执行隔离另见 ART-03） |
| Delegation / Action | tenant/project scope + actor + actions + expiry；细粒度动作，不设 admin | VERIFIED（领域模型） |

## 2. API 已实现、阻断与未实现

以 `docs/implementation/artifact-service/ART-02-API-STATUS.md` 的 17 项逐项表为当前状态记录：

- 已实现并有测试报告：health / ready / generators / Artifact 创建查询分页元数据更新 / Version 读取与 current pointer / Reference 创建解除 / Job 查询取消。
- 被存储 P0 阻断：Version upload session、Version commit；诚实返回 503 STORAGE_UNAVAILABLE。
- 尚未实现：GenerationJob 创建执行路径；输入 envelope 可校验，但 Adapter 不可用时返回 503 GENERATOR_UNAVAILABLE。
- 未实现：`GET /metrics`。
- Mermaid 已取消，属于前端 Markdown 编辑器预览。

不得把“路由存在 + 503”写成业务能力已经可用。

## 3. 已确认的契约机制

- OpenAPI 3.1 是唯一 API 描述来源。
- 统一错误 envelope：code/message/request_id/retryable/details。
- Artifact create 支持 Idempotency-Key；重放返回既有响应；相同 key 不同请求内容冲突。
- Artifact metadata PATCH 使用 If-Match / ETag 乐观并发。
- Artifact list 使用 cursor 分页；limit 上限 200。
- GenerationJob 请求 envelope 固定 schema `smark.seecode.input` / version 1，256 KiB 上限；未知顶层字段与危险输入拒绝。
- GenerationJob 响应含 error_code、attempt_count、max_attempts、cancel_requested；取消请求与取消终态区分。
- Artifact key 冲突：当前 OpenAPI 已列出 409 ARTIFACT_KEY_CONFLICT / ARTIFACT_KEY_LEGACY_UNVERIFIABLE。

## 4. 需要后续裁决/补证的契约缺口

| ID | 发现 | 等级 | 当前证据与影响 | 建议 |
|---|---|---|---|---|
| API-01 | Archive/Delete 无 HTTP endpoint | P1 | Domain 有 active→archived→deleted 状态机，Action 也预留 artifact:archive/delete；API 状态表无对应路由 | 明确 V1 是否只提供生命周期内部能力；若需要 API，另立 ART-04 契约，包含权限、审计、current pointer、引用、版本保留/清理语义 |
| API-02 | PATCH 缺省与显式 null 无法区分 | P2 | OPEN-ISSUES #13；Option 三态不足 | V1 明确 null 的语义，后续考虑 JSON Merge Patch 或显式 FieldUpdate<T> |
| API-03 | GET /v1/generators 无认证 | P2 / NEEDS-DECISION | OpenAPI 明确 security:[]；OPEN-ISSUES #7 | 确认全局能力注册表是否可公开；若部署环境不应公开，补受限网络策略或认证，不要隐式假设 |
| API-04 | Generation 输入 schema 未完全冻结 | P1（启用 Adapter 前） | Errata 明确 diagram 内部 JSON Schema 单独冻结；OPEN-ISSUES #21 | 冻结完整 schema、兼容策略、拒绝用例；Adapter 启用前强制完成 |
| API-05 | Reference 的外部语义由调用方负责 | DESIGN BOUNDARY | Domain 明确不解析 owner_id、不访问业务对象 | 保持该边界；调用方在建立引用前负责校验 owner 存在与权限 |
| API-06 | 错误码文档曾落后 OpenAPI | DOC-DRIFT | OPEN-ISSUES #3 仍描述 400 duplicate，但当前 OpenAPI 已定义 409 artifact key 冲突 | 后续文档同步时注明现行契约优先，避免消费者误读旧表 |
| API-07 | PATCH/ETag 覆盖面需完整证明 | PARTIAL | API 状态表覆盖 Artifact PATCH；版本提交/current pointer/metadata 变更的并发语义需按端点逐项测试 | 增补 stale ETag、并发创建版本、并发 current pointer 的竞争测试 |

## 5. Domain/API 边界判断

目前领域模型保持了正确的基础分离：

- Artifact 是逻辑容器；内容由不可变 Version 表达。
- Reference 只保存外部业务对象引用，不复制 AI-PM 业务实体。
- GenerationJob 是执行请求的聚合状态，不等于 Artifact 当前版本。
- tenant/project 授权来自 Delegation，不应由请求体或 X-Project-Id 自行证明。
- OpenAPI 是契约权威，SDK/DTO/测试必须追随 OpenAPI。

## 6. 结论

Domain 模型基础完整，核心元数据 API 已有明确 V1 契约；但生命周期 API、完整输入 schema、公开 Generator registry 策略和 PATCH null 语义仍需裁决/补证。当前没有理由为“完善领域契约”而重写已有模型；应先解决契约差异和测试缺口。
