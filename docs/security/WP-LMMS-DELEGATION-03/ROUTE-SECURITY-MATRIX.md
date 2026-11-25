# WP-LMMS-DELEGATION-03 — ROUTE-SECURITY-MATRIX（全量路由安全审计矩阵）

| | |
|---|---|
| 审计时点 | WP-03 修复完成后（分支 `feat/wp-lmms-delegation-03-as-remediation`） |
| 方法 | 以 `apps/artifact-api/src/routes/mod.rs::router` 注册清单为起点，**沿调用链核查到服务层**（handler → 提取器 → 服务方法 → `Delegation::require` → repo 谓词），并与 `openapi.yaml`、`contract_openapi::routes_match_openapi` 互检；未修改任何生产默认拒绝策略 |
| 授权语义 | tenant 恒取凭证（Q2/P0-01）；`X-Project-Id`/body 仅定位；所有动作检查经服务层 `require(action, scope)`，失败 403 `SCOPE_DENIED`，且**前置于任何数据读取/副作用** |

## 1. 路由矩阵（19 条路由 = 全部 HTTP 面）

| # | Method | Path | Handler | 认证 | 所需动作 | 资源归属校验 | 写副作用 | 关键测试 |
|---|---|---|---|---|---|---|---|---|
| 1 | GET | `/v1/health` | `routes::health` | 豁免（探针） | — | — | 无 | api_meta :: health_reports_build_info |
| 2 | GET | `/v1/ready` | `routes::ready` | 豁免（探针） | — | — | 无（只读 SELECT 1） | api_meta :: ready_reports_database_and_blocked_storage |
| 3 | GET | `/v1/generators` | `routes::list_generators` | **Authed（WP-03 修复，原无认证）** | `generation:read` | 全局注册表（以委托自身 scope 判定动作；无 project 头要求） | 无 | api_meta :: generators_report_unavailable_adapters / generators_require_delegation_and_action；delegation_security :: s21；contract_openapi :: served_responses_match_openapi_shapes |
| 4 | POST | `/v1/artifacts` | `artifacts::create_artifact` | Authed | `artifact:create` | `TenantScope(claims.tenant, body.project_id)`；keyed/unkeyed 幂等（AS-1）；project 定位取 body（契约允许，R2-F-14 备注） | **写** | delegation_security :: s01/s11/s12/write_without_binding_is_rejected_over_http；as1_creation_identity 全族 |
| 5 | GET | `/v1/artifacts` | `artifacts::list_artifacts` | Scoped | `artifact:read` | (tenant, project) 谓词 | 无 | sdk_contract；artifact_flow |
| 6 | GET | `/v1/artifacts/{id}` | `artifacts::get_artifact` | Scoped | `artifact:read` | repo (tenant, project, id) 谓词；跨租户 404 不泄露 | 无 | delegation_security :: s10 |
| 7 | PATCH | `/v1/artifacts/{id}` | `artifacts::patch_artifact` | Scoped | `artifact:update_metadata` | 同上 + If-Match 乐观锁 | **写** | artifact_flow |
| 8 | POST | `/v1/artifacts/{id}/versions` | `artifacts::create_version_session` | Scoped | `version:create` | repo 谓词；Storage 协议 P0 阻断 → 503 fail-closed | 意图写（当前 503 拒绝） | artifact_flow |
| 9 | GET | `/v1/artifacts/{id}/versions` | `artifacts::list_versions` | Scoped | **`version:read`（WP-03 修复，原绕过动作检查）** | repo 谓词 | 无 | delegation_security :: version_reads_require_version_read_action |
| 10 | GET | `/v1/artifacts/{id}/versions/{vid}` | `artifacts::get_version` | Scoped | **`version:read`（WP-03 修复，同上）** | repo 谓词 | 无 | 同上 |
| 11 | POST | `/v1/artifacts/{id}/versions/{vid}/commit` | `artifacts::commit_version` | Scoped | `version:commit` | repo 谓词；503 阻断 fail-closed | 意图写（当前 503） | artifact_flow |
| 12 | POST | `/v1/artifacts/{id}/current-version` | `artifacts::set_current_version` | Scoped | `version:set_current` | repo 谓词 + READY 状态机守卫 + 期望指针比对 | **写** | artifact_flow |
| 13 | POST | `/v1/artifacts/{id}/references` | `artifacts::create_reference` | Scoped | `reference:write` | repo 谓词 + artifact accepts_writes | **写** | artifact_flow |
| 14 | GET | `/v1/artifacts/{id}/references` | `artifacts::list_references` | Scoped | **`artifact:read`（WP-03 修复，原绕过动作检查；Q4 无 reference:read，精确动作映射，契约 §3.2 表登记）** | repo 谓词 | 无 | delegation_security :: reference_list_requires_artifact_read_action |
| 15 | DELETE | `/v1/artifacts/{id}/references/{rid}` | `artifacts::remove_reference` | Scoped | `reference:remove` | repo 谓词软删 | **写** | artifact_flow |
| 16 | POST | `/v1/artifacts/{id}/generation-jobs` | `artifacts::request_generation` | Scoped | `generation:request` | repo 谓词 + generator 状态；Adapter 未实现 → 503 fail-closed（无 Job 产生） | 意图写（当前 503） | generation_api |
| 17 | GET | `/v1/generation-jobs/by-caller-request/{crid}` | `artifacts::get_job_by_caller_request` | Scoped | `generation:read` | (tenant, project, caller) 谓词；跨 scope 与不存在统一 404 | 无 | as2_caller_identity 族 |
| 18 | GET | `/v1/generation-jobs/{id}` | `artifacts::get_job` | Scoped | `generation:read` | repo 谓词 | 无 | generation_api |
| 19 | POST | `/v1/generation-jobs/{id}/cancel` | `artifacts::cancel_job` | Scoped | `generation:cancel` | repo 谓词 + 终态空操作守卫 | **写**（状态迁移） | worker_coordination / generation_api |

中间件（全部路由）：`request_id_layer`（X-Request-Id）→ `audit_and_enrich_layer`（401/403 审计拒绝事件；错误信封注入 request_id）→ TraceLayer → TimeoutLayer（10s → 503）。

## 2. 重点核查结论

| 检查项 | 结论 |
|---|---|
| 路由集合与 OpenAPI 一致 | ✅ `contract_openapi :: routes_match_openapi`（router 注册 ↔ spec 双向） |
| 旧路由 / 别名 / 嵌套绕过 | 无：全部业务路径枚举于矩阵；静态段 `/by-caller-request/` 先于动态 `{job_id}` 注册（axum 精确匹配语义 + 注册顺序注释）；无任何第二 router 或 nest |
| `/v1` 之外的业务面 | 无：非 `/v1` 路径仅 health/ready（探针） |
| 内部接口 | 仓库内无 internal HTTP 端点（撤销查询端点属 AI-PM 侧，不存在——见 REMAINING-BLOCKERS） |
| 批量操作 | 无批量端点（AS-1 keyed create 为单资源语义） |
| 间接写（经其他 handler 触发） | 无：全部写副作用收敛在 #4/7/12/13/15/19 六个服务入口；授权（require）均在其服务方法首行 |
| Worker 面 | `artifact-worker` **无 HTTP server**（无 axum 依赖路由）；执行层经 DB 受控函数 + lease，永不接触 delegation token（契约红线 5，架构保证） |
| 未知/缺失身份 fail-closed | 全部业务路由经 `Authed`/`Scoped` 提取器：无 token/坏 token → 401；空 issuer 清单 → `ClosedDelegationVerifier` 全拒（s21 + default_verifier_is_fail_closed） |
| 读操作豁免 | 仅 health/ready；generators 与全部 GET 业务端点均要求 delegation + 精确动作（Q3） |

## 3. 遗留（非本矩阵范围内的绕过）

- V10 目标一致性未接线：写路由的动作/scope 授权完整，但 token 绑定字段（artifact_id/operation_id/request_hash）与请求目标的比对属 AS-4（见 REMAINING-BLOCKERS G1）。
- V11 撤销检查未接线：写路径无撤销查询（G2）。
- 审计事件缺 jti/subject/原因（R2-F-09，G5 建议项）。
