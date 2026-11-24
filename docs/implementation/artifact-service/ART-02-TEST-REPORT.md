# ART-02 测试报告

## 1. 门禁结果（本地 + CI）

| 门禁 | 本地（Windows，PG16 容器 artifact-dev-pg@16500） | CI（ubuntu-latest + postgres:16 service） |
|---|---|---|
| `cargo fmt --all -- --check` | ✅ | ✅（独立 job） |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ 0 告警 | ✅ |
| `cargo build --workspace` | ✅ | ✅ |
| `cargo test --workspace` | ✅ **30 passed / 0 failed** | ✅（同一套件） |
| `cargo build --release -p artifact-api`（无 test-doubles） | ✅ | ✅（独立 step，验证发布二进制不含测试替身） |
| `docker build` | ✅ 本地 128MB 镜像（rust:1-bookworm→bookworm-slim，非 root） | ✅ docker-build job + 入口冒烟 |
| serve 冒烟 | ✅ migrate→serve→health/ready/generators/401 逐端点核对 | — |

CI run：https://github.com/luqizheng/smark-artifact-service/actions/runs/45403601082
CI 在全新环境可重复：postgres:16 service 容器 + DATABASE_URL 注入，无本机状态依赖。

## 2. 测试清单（30 用例）

### 单元（artifact-api）
| 用例 | 覆盖 |
|---|---|
| default_verifier_is_fail_closed | 生产验证器装配无条件拒绝 |

### api_meta（健康/就绪/Generator，3）
| 用例 | 覆盖 |
|---|---|
| health_reports_build_info | health 200 + version/git_sha |
| ready_reports_database_and_blocked_storage | DB=ok；file_storage=protocol_blocked_p0（诚实上报） |
| generators_report_unavailable_adapters | mermaid/seecode 均 unavailable（无伪 available） |

### default_deny（默认拒绝，2）
| 用例 | 覆盖 |
|---|---|
| all_business_endpoints_denied_without_credentials | 10 业务端点 × 无凭证 → 401 + 信封 + request_id；拒绝入审计 |
| garbage_bearer_is_unauthenticated | 垃圾 Bearer → 401 |

### artifact_flow（API 全流程，6）
| 用例 | 覆盖 |
|---|---|
| create_get_patch_etag_roundtrip | 201+ETag；If-Match 缺失→400、错配→409、命中→200+新 ETag |
| idempotency_replay_and_reuse_conflict | 同 key 同 body 重放；同 key 异 body → 409 IDEMPOTENCY_KEY_REUSED |
| list_pagination_with_cursor | cursor 分页 3 条 2 页、无重复 |
| tenant_project_boundary_negatives | 跨项目 404 / 伪造 project 403 / 缺 action 403 |
| current_version_pointer_invariant_via_api | 不存在版本 404；pending → 409；READY 后切换成功；expected 不匹配 409 |
| reference_create_remove | 创建、非法枚举 400、解除 204、重复解除 404 |

### migration_invariants（数据库不变量，10）
| 用例 | 覆盖 |
|---|---|
| version_numbers_are_serialized_and_dense | 6 并发插入 → 版本号 1..6 无重复无跳号（父行锁分配） |
| current_pointer_requires_ready_version_same_artifact | pending 不可为 current；跨 artifact 版本不可为 current；READY 可 |
| ready_version_is_immutable_and_terminal | READY 内容/状态不可变；pending→failed 合法；failed→ready 非法 |
| deleted_artifact_rejects_children | deleted 拒绝新 version/reference/job |
| active_reference_unique_and_soft_remove | 活跃引用 6 元组唯一；软删后可重建 |
| artifact_key_unique_within_project_only | key 项目内唯一；跨项目同 key 合法；NULL key 不受限 |
| audit_is_append_only | audit UPDATE/DELETE 触发器拒绝 |
| generation_job_state_machine_enforced | running→queued 非法；running→failed 合法；终态不可复活 |
| idempotency_record_unique_per_scope | (tenant,project,endpoint,key) 唯一；异 scope/endpoint 不冲突 |
| output_unique_per_version_role_format | (version,role,format) 唯一 |

### roles_separation（角色分离，1）
| 用例 | 覆盖 |
|---|---|
| runtime_role_is_dml_only | 执行仓库真实 db-roles.sql：运行角色 DML 放行；CREATE TABLE / DELETE / audit UPDATE → 42501 |

### contract_openapi（OpenAPI 契约，4）
| 用例 | 覆盖 |
|---|---|
| openapi_paths_match_server_routes | 规范路径×方法集合 == 服务端路由集合（防漂移） |
| error_envelope_schema_matches_contract | 错误信封字段 + Artifact 必备字段对齐 |
| every_business_path_documents_401 | 每个业务路径规范声明 401 |
| served_responses_match_openapi_shapes | 实服响应形状与规范一致（health/ready/generators/401 信封） |

### sdk_contract（SDK 消费者契约，3）
| 用例 | 覆盖 |
|---|---|
| sdk_full_flow_matches_contract | SDK 创建→读取→更新→列表→引用→current-version→生成（GENERATOR_UNAVAILABLE）全链 |
| sdk_surfaces_error_envelope_fields | 401 信封字段（status/code/request_id/retryable）经 SDK 透出 |
| sdk_has_no_internal_aipm_dependencies | SDK 依赖面静态守卫（无 pm-* / 无 infrastructure / 无 api） |

## 3. 测试边界声明

- 集成测试使用**测试替身委托**（`test-doubles` feature，编译期隔离），用于驱动业务路径；生产二进制的默认拒绝由 default_deny.rs（无 feature）+ state 单元测试双保险锁定。
- 未覆盖（属后续阶段）：Generator Golden/安全负例（ART-03+ Mermaid/SeeCode 实现）、对象存储集成（存储 P0 关闭后）、备份恢复演练、负载/限流。
