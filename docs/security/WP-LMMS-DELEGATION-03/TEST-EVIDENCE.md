# WP-LMMS-DELEGATION-03 — TEST-EVIDENCE（测试证据）

| | |
|---|---|
| 原则 | 只记录实际执行的命令与真实结果；区分单元/集成/独立 PoC/CI；未覆盖边界显式标注，不伪造测试名、数量或结果 |

## 1. 执行环境

| 项 | 值 |
|---|---|
| 机器 | Windows（win32）本地工作树，分支 `feat/wp-lmms-delegation-03-as-remediation` |
| 数据库 | 本地隔离 dev PG：Docker `artifact-dev-pg` @ `127.0.0.1:16500`（`sqlx::test` 每测试独立建库后丢弃，未连接任何共享/生产库） |
| 环境事件（如实记录） | 首轮全量测试遇 `PoolTimedOut`——根因是 **dev PG 容器处于停止状态**（环境故障，非代码问题）。启动容器 + 清理 4 个残留 `_sqlx_test%` 库后全绿。另按既有经验以 `--test-threads=8` 受控并发运行（Windows 本机 sqlx::test 建库饱和缓解；CI Linux 无需） |

## 2. 命令与结果

| 命令 | 结果 |
|---|---|
| `cargo fmt --check` | ✅ 通过（0 diff） |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | ✅ 通过（0 警告；过程中修复一处删除孤儿 helper 后的 dead_code） |
| `cargo test --workspace --lib` | ✅ **59 通过 / 0 失败**（单元） |
| `cargo test --workspace -- --test-threads=8`（全量） | ✅ **241 通过 / 0 失败** |
| `cargo test -p artifact-integration-tests --test delegation_security -- --test-threads=8` | ✅ **26 通过 / 0 失败**（约 10s） |
| 迁移链完整性 | ✅ `migration_guard :: released_migrations_are_unchanged`（sha256 清单 ↔ 目录双向互检，0006 已除名）+ 全部 DB 测试实际执行 0001–0005 链 |

数量对账：基线 `40848f1` 为 226 通过（R2 独立复核一致）；本分支 = 226 − 6（删除的绑定表专属用例 s13/s14/s15+s19/s17/s20）+ 21（新增）= 241。

## 3. 新增测试清单（21 项）

**单元（9）：**
- `config :: non_contract_audience_config_fails_startup` / `empty_audience_config_fails_startup` / `audience_cannot_be_bypassed_via_case_or_whitespace` / `explicit_contract_audience_is_accepted`（WP-03C）
- `ai_pm :: write_actions_require_caller_request_id`（PC-O5-1 写/读动作分类矩阵）
- `ai_pm :: biz_pairing_rules` 扩充（`request_hash_alg`：未知算法拒绝、alg 无 hash 半配对拒绝）
- `delegation_request :: v1_frozen_vector_minimal_body` / `v1_key_order_and_method_case_are_canonicalized` / `v1_frozen_vector_null_body_and_missing_caller` / `v1_frozen_vector_unicode_is_byte_exact` / `v1_number_null_and_string_semantics_are_distinct` / `v1_method_and_path_changes_alter_fingerprint`（WP-03B，共 6 个冻结向量/语义测试）

**集成（12）：**
- delegation_security :: `v4_array_audience_even_containing_expected_is_rejected`（V4/PoC P7）
- delegation_security :: `v6_jti_must_be_uuid`（V6/PoC P2）
- delegation_security :: `v7_purpose_must_be_delegation`（V7/PoC P1+P1b）
- delegation_security :: `v8_actions_shape_rejections`（V8/PoC P3–P6：通配/未知/重复/空）
- delegation_security :: `write_action_token_without_caller_request_id_is_rejected`（PC-O5-1 验证器层）
- delegation_security :: `write_without_binding_is_rejected_over_http`（PC-O5-1 HTTP 层 401）
- delegation_security :: `version_reads_require_version_read_action`（WP-03D：错误动作 403 / 合法 200 / 无 token 401 / 跨项目 403）
- delegation_security :: `reference_list_requires_artifact_read_action`（WP-03D：403 / 200 / 401）
- api_meta :: `generators_require_delegation_and_action`（WP-03E：401 / 403）
- s21 扩充：closed verifier 下 `/v1/generators` 401（带/不带 token 两断言）
- `contract_openapi :: every_business_path_documents_401` 现在覆盖 `/v1/generators`（豁免移除）
- `served_responses_match_openapi_shapes` 改为带 `generation:read` 凭证访问 generators（200 形状校验）

**既有测试的必要适配（非"修改预期绕过问题"）：** `base_claims()` 补 `caller_request_id`（合法 token 本就应符合新红线——s01/s05b/s10/s11/s18 因此恢复绿色，拒绝原因可核查：修复前失败消息正是新红线生效的证明）；SDK/api_meta/contract_openapi 的 generators 用例改带凭证（端点语义变更的正确跟随）。

## 4. 被删除的测试（O-5 随表移除，非覆盖回退）

s13（同 jti 异请求）、s14（并发同 jti 同指纹）、s15+s19（合法重试 + 重启）、s17（撤销窗口改绑守卫）、s20（DB 层 23505/23514/purge 护栏）——全部仅服务于 `PgDelegationBindingStore` 与迁移 0006，端口与表删除后无被测对象。并发仲裁同强度断言由 as2_17（并发同 caller 竞态恰一创建）继续承担。

## 5. 证据分类声明

| 类别 | 内容 |
|---|---|
| 单元测试 | 59（crates 各 lib） |
| 集成测试 | 182（真实 HTTP over axum router + 隔离 PG + 真实 RS256/JWKS 回环；安全场景无 mock-only——`TestDelegationVerifier` 仅用于既有业务回归，不用于安全断言） |
| 独立 PoC | `docs/security/WP-LMMS-DELEGATION-02/poc/r2-verifier-poc`（R2 评审工具，独立 workspace、不进主构建）；其 P1–P7 场景已移植为上述 v4/v6/v7/v8 自动化用例 |
| CI | **本分支未运行 CI**（任务书禁止未授权推送；本仓库 CI 仅 main/docs/** push 或 PR 打开时触发）。基线 `40848f1` CI 全绿（GitHub 独立查询核实）。分支推送/开 PR 后以 CI 复核为准 |

## 6. 仍未覆盖的边界（显式缺口）

| 缺口 | 归属 |
|---|---|
| V10 目标一致性（绑定字段 vs 请求目标，含三字段独立变异） | AS-4 / REMAINING-BLOCKERS G1 |
| V11 撤销后写 403；不可达/超时/非 200/畸形响应四类 503；并发撤销-写入竞态 | AI-PM 阶段 1 + AS-4 / G2 |
| 审计事件 jti/subject/原因内容断言（R2-F-09） | G5 建议项 |
| 异 jti 指纹并发（原 R2-F-11，绑定表场景） | 随绑定表移除而失去被测对象；业务幂等并发覆盖由 as2_17 承担 |
| request_hash 运行期重算比对 | V10 接线后（当前仅形状/向量） |
