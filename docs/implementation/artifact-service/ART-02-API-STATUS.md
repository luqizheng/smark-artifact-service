# ART-02 API 状态 — 17 端点逐项（对照 ART-01 §4.2）

状态口径：**已实现+已测试** / **被 P0 阻断（诚实 503）** / **尚未实现（路由不存在）**。
机器可读性约束（任务 §4.2）：无伪成功——所有未落地能力返回统一错误信封（code/message/request_id/retryable/details）。

| # | Method Path | ART-01 用途 | 状态 | 行为 | 测试 |
|---|---|---|---|---|---|
| 1 | GET /v1/health | 进程存活 | ✅ 已实现+已测试 | 200，version/git_sha/built_at，不探测依赖 | api_meta::health_reports_build_info |
| 2 | GET /v1/ready | 依赖就绪 | ✅ 已实现+已测试 | 200/503；components.database + file_storage=protocol_blocked_p0 如实上报 | api_meta::ready_reports_database_and_blocked_storage |
| 3 | GET /v1/generators | Generator 能力 | ✅ 已实现+已测试 | 注册表实况；mermaid/seecode 均 unavailable + 原因 | api_meta::generators_report_unavailable_adapters |
| 4 | POST /v1/artifacts | 创建 Artifact | ✅ 已实现+已测试 | 201+ETag；Idempotency-Key 重放 200（X-Idempotent-Replay）、同 key 异 body 409 | artifact_flow::create_get_patch_etag_roundtrip / idempotency_replay_and_reuse_conflict；sdk_contract |
| 5 | GET /v1/artifacts | 分页查询 | ✅ 已实现+已测试 | cursor 分页（base64url(created_at\|id)）、artifact_type/lifecycle_state 过滤、limit≤200 | artifact_flow::list_pagination_with_cursor |
| 6 | GET /v1/artifacts/{id} | 元数据+当前版本 | ✅ 已实现+已测试 | 200+ETag；跨 scope → 404（防 ID 探测） | artifact_flow（boundary） |
| 7 | PATCH /v1/artifacts/{id} | 非版本元数据 | ✅ 已实现+已测试 | If-Match 必填（缺→400、不匹配→409 VERSION_CONFLICT）；成功→新 ETag（row_revision+1） | artifact_flow::create_get_patch_etag_roundtrip |
| 8 | POST /v1/artifacts/{id}/versions | 创建上传会话 | ⛔ 被 P0 阻断 | 鉴权+动作校验+artifact 存在校验后 → 503 STORAGE_UNAVAILABLE（file-service upload-intent 协议缺失） | 路由在位；503 语义由 default_deny/协议阻断实现共同保证 |
| 9 | GET /v1/artifacts/{id}/versions | 列出版本 | ✅ 已实现+已测试（空集语义） | version_number DESC | artifact_flow（current 指针流程间接覆盖读取） |
| 10 | GET /v1/artifacts/{id}/versions/{vid} | 版本+输出清单 | ✅ 已实现+已测试 | 200 / 404 | artifact_flow::current_version_pointer_invariant_via_api（间接） |
| 11 | POST .../versions/{vid}/commit | 校验并提交 | ⛔ 被 P0 阻断 | 503 STORAGE_UNAVAILABLE（无原子提交协议，禁止把普通上传当最终提交） | 同 #8 |
| 12 | POST /v1/artifacts/{id}/current-version | 切换当前版本 | ✅ 已实现+已测试 | 只接受 READY；expected_current_version_id 乐观并发（不匹配→409）；DB 触发器兜底 | artifact_flow::current_version_pointer_invariant_via_api |
| 13 | POST /v1/artifacts/{id}/references | 建立引用 | ✅ 已实现+已测试 | owner_type/relation_type 受控枚举；重复活跃引用返回既有记录（幂等语义） | artifact_flow::reference_create_remove |
| 14 | DELETE /v1/artifacts/{id}/references/{rid} | 解除引用 | ✅ 已实现+已测试 | 软删除（removed_at）；重复解除 → 404 | 同上 |
| 15 | POST /v1/artifacts/{id}/generation-jobs | 请求生成 | 🚧 尚未实现（Adapter 层） | 鉴权+校验后 → 503 GENERATOR_UNAVAILABLE（adapter not implemented）；已知 generator → 400；未注册 → 400 | sdk_contract::sdk_full_flow_matches_contract |
| 16 | GET /v1/generation-jobs/{job_id} | Job 状态 | ✅ 已实现+已测试（读取路径） | 200 / 404；创建路径落地后即为完整读面 | sdk_contract（404 分支） |
| 17 | POST /v1/generation-jobs/{job_id}/cancel | 取消 Job | ✅ 已实现+已测试（终态语义） | 终态 Job 原样返回（ART-01 §8.2 尽力而为取消） | 同上（状态机 DB 触发器已测） |
| — | GET /metrics | 内部监控 | 🚧 尚未实现 | ART-01 要求网络受限暴露；属可观测性后续批次 | — |

## 认证语义（对全部业务端点）

| 场景 | 响应 |
|---|---|
| 无 Authorization / 非 Bearer / 空令牌 | 401 UNAUTHENTICATED |
| 令牌无法通过验证器（当前生产=任何令牌） | 401 UNAUTHENTICATED |
| 委托 project 范围 ≠ 请求定位 project | 403 SCOPE_DENIED |
| 委托缺少所需 action | 403 SCOPE_DENIED |
| 资源不在委托 scope 内 | 404 RESOURCE_NOT_FOUND（避免 ID 探测） |

以上每一行均有自动化测试（default_deny.rs、artifact_flow.rs、sdk_contract.rs）。

## 通用机制

| 机制 | 实现 | 测试 |
|---|---|---|
| 统一错误信封 | `{error:{code,message,request_id,retryable,details}}`，ART-01 §8.1 全部 13 码 + INTERNAL_ERROR | contract_openapi::error_envelope_schema_matches_contract；default_deny |
| request_id | X-Request-Id 透传/生成，注入信封与响应头 | default_deny（request_id 断言）；sdk_contract（request_id 透出） |
| Idempotency-Key | idempotency_record 原子预留 + 响应快照重放，保留 ≥24h | artifact_flow::idempotency_replay_and_reuse_conflict；migration_invariants::idempotency_record_unique_per_scope |
| ETag / If-Match | ETag=`"r<row_revision>"`；更新类操作强制乐观并发 | artifact_flow（400/409/200 三分支） |
| 审计 | 成功动作 allowed 事件 + 全部 401/403 拒绝事件（不含任何凭证原文） | default_deny（审计断言） |
