# WP-AS-FOUNDATION-01 — 测试证据矩阵

> 状态：PARTIAL。测试名称来自 ART-02 状态/安全报告与 CI 配置；本轮未执行 cargo test 或 CI。

## 1. 已有测试证据（文档声明，需 CI run / 源码复核）

| 领域 | 已知测试/门禁 | 覆盖点 | 状态 |
|---|---|---|---|
| API Meta | `api_meta::health_reports_build_info` | health build info | DOC-VERIFIED |
| API Meta | `api_meta::ready_reports_database_and_blocked_storage` | DB ready + storage P0 明示 | DOC-VERIFIED |
| API Meta | `api_meta::generators_report_unavailable_adapters` | Adapter unavailable | DOC-VERIFIED |
| Artifact | `artifact_flow::create_get_patch_etag_roundtrip` | create/get/patch/ETag | DOC-VERIFIED |
| Idempotency | `artifact_flow::idempotency_replay_and_reuse_conflict` | 重放/同 key 不同 body 冲突 | DOC-VERIFIED |
| Pagination | `artifact_flow::list_pagination_with_cursor` | cursor 分页 | DOC-VERIFIED |
| Version | `artifact_flow::current_version_pointer_invariant_via_api` | READY/current pointer | DOC-VERIFIED |
| Reference | `artifact_flow::reference_create_remove` | 建立/解除/幂等行为 | DOC-VERIFIED |
| Default deny | `default_deny::all_business_endpoints_denied_without_credentials` | 无凭证 401 | DOC-VERIFIED |
| Default deny | `default_deny::garbage_bearer_is_unauthenticated` | 垃圾 token 401 | DOC-VERIFIED |
| DB roles | `roles_separation.rs` | App 无 DDL/DELETE，审计 append-only | DOC-VERIFIED |
| Worker roles | `worker_roles.rs` | Worker 无业务表 DML/DDL，函数权限精确 | DOC-VERIFIED |
| Migration | `migration_invariants.rs` | schema / constraints / idempotency | DOC-VERIFIED |
| Job API | `generation_api::get_job_serves_the_documented_shape` | Job 响应形状 | DOC-VERIFIED |
| Job cancel | `generation_api::cancel_running_job_reports_cancel_requested` | running cancel request semantics | DOC-VERIFIED |
| Input envelope | `generation_api::generation_request_enforces_the_input_envelope` | 256 KiB/schema rejection | DOC-VERIFIED |
| CI | fmt/clippy/build/test/postgres16/docker/migration hash | 基础工程质量 | WORKFLOW-VERIFIED |

## 2. 必须补齐或提供独立证据的测试

| ID | 测试要求 | 优先级 | 目的 |
|---|---|---|---|
| T-AS-01 | 所有 API 路由 × 所需 action 矩阵 | P0 | 防止新增路由漏鉴权/错 action |
| T-AS-02 | 真正 issuer/JWKS 签名验证集成测试 | P0 | 当前生产 verifier fail-closed，尚无真实授权闭环 |
| T-AS-03 | aud/iss/tid/project/actions/exp/nbf/jti/alg/kid 负向测试 | P0 | 覆盖 claims 与 key lifecycle |
| T-AS-04 | 并发 create 同 artifact_key / 同 idempotency key | P1 | 验证唯一约束与原子重放 |
| T-AS-05 | 并发 PATCH stale ETag | P1 | 验证无 lost update |
| T-AS-06 | 并发创建 Version + version_number 分配 | P1 | 验证父行锁与唯一性 |
| T-AS-07 | 并发 current pointer CAS | P1 | 证明只发生一个成功更新 |
| T-AS-08 | Upload / finalize / DB commit 故障注入矩阵 | P0 | 证明不会 READY 伪成功、孤儿可补偿 |
| T-AS-09 | finalize 重放与客户端响应丢失 | P0 | 幂等、对象身份稳定 |
| T-AS-10 | Object tenant/project/hash/size/MIME 不匹配 | P0 | 防止对象替换与跨租户绑定 |
| T-AS-11 | SECURITY DEFINER 每函数权限与 search_path 测试 | P0 | 防止函数提权/权限漂移 |
| T-AS-12 | 迁移后 grants 自动验证 | P1 | 防止 DROP/CREATE 函数丢 EXECUTE 授权 |
| T-AS-13 | Worker cancel/timeout/lease/recovery 真实并发测试 | P0（启用执行前） | DB 取消不等于 OS 进程死亡 |
| T-AS-14 | Runtime Start-vs-revoke / death-confirmation 测试 | P0（启用执行前） | 对应 ART-03B-03 #6 复审阻断 |
| T-AS-15 | SVG allowlist 重建与恶意 XML corpus | P0（Adapter 前） | 防止脚本/外部引用/命名空间绕过 |
| T-AS-16 | 依赖 CVE / license / image scan / SBOM / signature 验证 | P1 | 供应链发布证明 |
| T-AS-17 | backup restore / migration rollback / grants reapply 演练 | P1 | 恢复能力 |
| T-AS-18 | readiness 与 metrics 告警验证 | P2 | 运维可观测性 |
| T-AS-19 | API contract diff / SDK consumer compatibility | P1 | 防止 OpenAPI、DTO、SDK 漂移 |
| T-AS-20 | 跨项目 Reference owner_id 不可用于访问业务实体 | P1 | 保持跨服务职责边界 |

## 3. 证据分级

- `DOC-VERIFIED`：仅表示测试名称/结论出现在已审阅仓库文档。
- `WORKFLOW-VERIFIED`：仅表示 CI workflow 配置包含该命令/job。
- `RUN-VERIFIED`：必须有本次 SHA 对应的成功 workflow run 与 job 结果。
- `EXECUTED-HERE`：必须由本轮实际执行并保存输出。
- `NOT-TESTED`：没有可核验运行证据。

本轮没有将任何历史报告中的“30/30”或其他历史绿灯标为 RUN-VERIFIED。
