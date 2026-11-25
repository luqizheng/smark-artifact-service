# ART-02 架构映射 — 基线条款 → 实现位置

权威基线：ADR-ART-001（架构决策）、ART-01（API/数据模型契约）、ART-02（前置核验，含两个 P0）。
本文逐项映射"基线要求 → 本仓库实现 → 证据"。

## 1. ADR-ART-001 验收标准映射

| ADR 验收项 | 级别 | 实现 | 证据 |
|---|---|---|---|
| ART-001 独立启动/部署/健康检查 | P0 | `apps/artifact-api`（axum），`/v1/health`、`/v1/ready`、build info | tests/api_meta.rs；serve 冒烟 |
| ART-002 元数据独立存 PostgreSQL | P0 | `migrations/0001_init.sql` schema `artifact` 8 表 | tests/migration_invariants.rs（10 例） |
| ART-003 正文入对象存储 | P0 | **仅定义接口**：StoragePort（artifact-application），生产实现协议阻断 | storage.rs `ProtocolBlockedStorage`；详见 SECURITY-STATUS |
| ART-004 不可变 Version | P0 | `trg_artifact_version_immutable` 触发器（READY 终态 + 内容不可变） | migration_invariants::ready_version_is_immutable_and_terminal |
| ART-005 多外部引用 | P0 | `artifact_reference` 表 + 部分唯一 + API | artifact_flow::reference_create_remove |
| ART-006 失败不影响当前版本 | P0 | current 指针仅指向 READY（触发器）；Job 状态机独立 | migration_invariants::current_pointer_requires_ready_version_same_artifact |
| ART-007 API/SDK 版本化 + 契约测试 | P0 | openapi.yaml + contract_openapi.rs + artifact-client + sdk_contract.rs | CI test job |
| ART-008 CI 独立于 pm-api | P0 | 本仓库 `.github/workflows/*`，与 AI-PM 无共享 workflow | CI run |
| ART-009 业务变更不触发全量 CI | P0 | 独立仓库天然隔离（AI-PM 变更不进入本仓库触发器） | 仓库边界 |
| ART-010 越权拒绝 | P0 | 委托作用域（tenant 取自凭证、project 一致性、action 白名单）+ 404 防 ID 探测 | artifact_flow::tenant_project_boundary_negatives；default_deny.rs |
| ART-011 Mermaid Adapter | — | **取消**：Mermaid 由前端 Markdown 编辑器负责；Artifact Service 不提供 Mermaid 专用能力 | ART-03 SeeCode 设计决策 |
| ART-012 SeeCode SVG-only | P0 | **未实现**（种子行 security_profile=server_svg_only，html_output=false） | 同上；实施基线见 ART-03 |
| ART-013 上传/登记/恢复一致性 | P0 | **仅定义接口**：StoragePort + 版本提交协议设计（ART-01 §4.4 六步）；实现被存储 P0 阻断 | SECURITY-STATUS §存储 |
| ART-014 Viewer / ART-015 迁移 | P1 | 不属于本阶段（前端/迁移属 ART-05+） | — |

## 2. ART-01 不变量 → 代码位置

| ART-01 §1.2 不变量 | 实现位置 |
|---|---|
| 1 Version READY 不可变 | `migrations/0001` `guard_version_immutability`；service 仅 pending→ready 转移 |
| 2 current_version_id 只是可变指针 | `guard_current_version` 触发器；`POST .../current-version` 只改指针 |
| 3 业务审批不进入本服务 | 领域模型无审批概念；approval 留在 AI-PM viz 域 |
| 4 不读 AI-PM 业务库 / 不依赖 pm-* crate | workspace 依赖图 + sdk_contract::sdk_has_no_internal_aipm_dependencies 静态守卫 |
| 5 请求体 tenant/project 不是授权证明 | `TenantScope` tenant 恒取自凭证；body project ≠ 凭证 project → SCOPE_DENIED |
| 6 文件正文不进 PG / 不建 S3 控制面 | 表结构无正文列（仅 object_id/hash/size/mime）；无对象存储 SDK 依赖 |
| 7 Worker 零凭据 | 独立 `artifact-worker` 二进制只读取自身的 Worker DSN（独立 DB 角色，仅有受控函数的 EXECUTE）；不读取用户 delegation，不持有对象存储凭据 | tests/worker_roles.rs；`apps/artifact-worker/src/config.rs` 单元测试 |
| 8 不双写 viz.* | 本仓库无 viz.* 引用 |
| 9 业务变更不触发全量 CI | 独立仓库 |
| 10 兼容性由契约测试证明 | contract_openapi.rs + sdk_contract.rs |

## 3. ART-01 数据模型 → 表

| ART-01 §2/§3 实体 | 表 | 关键约束（数据库强制） |
|---|---|---|
| Artifact | `artifact.artifact` | UNIQUE(id,tenant,project)；partial UNIQUE(tenant,project,artifact_key)；两个 §3.2 索引；row_revision 乐观并发锚；lifecycle CHECK |
| ArtifactVersion | `artifact.artifact_version` | UNIQUE(artifact_id,version_number)；UNIQUE(id,artifact_id,tenant,project)；复合 FK→artifact；version_number 触发器按父行锁分配；READY 不可变 |
| ArtifactOutput | `artifact.artifact_output` | UNIQUE(version_id,output_role,format)；复合 FK→version |
| ArtifactReference | `artifact.artifact_reference` | 6 元组部分唯一（removed_at IS NULL）；复合 FK→artifact；owner/relation 受控枚举 |
| GenerationJob | `artifact.generation_job` | 状态机白名单触发器；复合 FK→artifact 与 target version。0002 / 0003 增加：输入快照与清理期限、`next_attempt_at` / `expires_at` / `max_execution_seconds`、Idempotency-Key 部分唯一索引、claim 部分索引、错误码形式 CHECK |
| GenerationJobAttempt（ART-03A） | `artifact.generation_job_attempt` | UNIQUE(job_id, attempt_number)；同一 Job 至多一个活跃 Attempt（部分唯一）；活跃 Attempt 的租约必须非空；状态机触发器 |
| Generator | `artifact.generator_registry` | PK generator_id；status CHECK；0001 中的 Mermaid seed 已由 0002 删除，仅保留 SeeCode |
| AuditEvent | `artifact.audit_event` | append-only 触发器 + 运行角色 REVOKE UPDATE/DELETE；scope+created 索引 |
| （幂等） | `artifact.idempotency_record` | UNIQUE(tenant,project,endpoint,key)；request_hash 64 位；24h 过期 |
| outbox_event | 未建 | ART-01"若启用事件通知"，V1 轮询必选，事件通知属后续（OPEN-ISSUES #5） |

## 4. ART-01 API 清单 → 路由状态

见 [ART-02-API-STATUS.md](ART-02-API-STATUS.md)（17 端点逐个状态）。

## 5. ART-01 IAM → 委托验证

| ART-01 §5 条款 | 实现 |
|---|---|
| §5.2 委托 claims（aud/sub/tenant/project/actions/jti/…） | `artifact_domain::Delegation` 全字段建模；`TestDelegationVerifier` 按 claims 语义构造 |
| §5.3 动作集合 15 项 | `artifact_domain::Action` 全集（`action:xxx` 稳定字符串） |
| §5.5 明确拒绝的模式 | 全部落实：不验签不放行（fail-closed）、不信任请求体、无"来自 pm-api 即可信"、无永久绕过开关 |
| §5.1 真实 issuer/JWKS | **被 P0 阻断**（lmms resource token 无鉴权挂载 + 缺 aud/tid）；`ClosedDelegationVerifier` 在 P0 关闭前拒绝一切 |

## 6. ART-01 §6 StoragePort → 协议边界

| 能力 | 端口方法 | 当前实现 |
|---|---|---|
| create_upload_intent / inspect_object / create_download_intent / copy_or_finalize_object / delete_object / head_object / service_auth | `artifact_application::StoragePort`（service_auth 并入未来 file-service adapter） | `ProtocolBlockedStorage`：全部返回 `ProtocolBlocked(原因)`，版本会话与 commit 端点如实 503 |
