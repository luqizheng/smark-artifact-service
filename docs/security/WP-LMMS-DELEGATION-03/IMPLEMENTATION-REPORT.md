# WP-LMMS-DELEGATION-03 — IMPLEMENTATION-REPORT（契约对齐与 AS 侧修复实施报告）

| | |
|---|---|
| 状态 | WP-03 实施完成（2026-11-26）。**本工作包是 AS 契约与实现修复，不是授权闭环完成，也不是生产开放批准。** |
| 依据 | R2 独立安全评审（`docs/security/WP-LMMS-DELEGATION-02/`，提交 `94d22a1`）+ WP-03 任务书 O-5/O-6 裁决 |
| 基线 | 分支基线 = PR #12 实现分支 HEAD `40848f1`（`docs/wp-lmms-delegation-01`，实现前经 GitHub + 本地双重核实，无并行改动） |
| 分支 | `feat/wp-lmms-delegation-03-as-remediation`（基于 `40848f1`，未推送、未开 PR、未合并任何 PR） |
| 最终 HEAD | 见 git log（本提交）；工作树干净 |

## 1. 执行摘要

按 R2 的 CONDITIONAL PASS 与 O-5/O-6 裁决完成 AS 侧全部可执行修复：

| 项 | 结果 |
|---|---|
| O-5 绑定表移除 | ✅ 迁移 0006、端口、Pg 实现、错误码、专属测试全部删除；`caller_request_id` 写路径红线落地（PC-O5-1）。详见 [O5-REMOVAL](O5-REMOVAL.md) |
| O-6 request_hash | ✅ 版本化（`biz.request_hash_alg`）+ v1 规范冻结（契约 §3.1.3）+ 冻结向量入库；**未接线**（PC-O6-3/4 阻断）。详见 [O6-REQUEST-HASH](O6-REQUEST-HASH.md) |
| audience 固定 | ✅ 验证器受众改为编译期常量（`TrustedIssuer` 移除 audience 字段）；配置非 `artifact-service` 启动失败。详见 [AUDIENCE-SECURITY](AUDIENCE-SECURITY.md) |
| 三个读 handler | ✅ `get_version`/`list_versions`/`list_references` 下沉服务层，动作检查（`version:read`/`artifact:read`）前置于任何数据读取 |
| `/v1/generators` | ✅ `Authed` + `generation:read`；OpenAPI 401/403；SDK 同步为需凭证 |
| §3.4 幻影引用 | ✅ 全仓清除（仅裁决记录中对问题本身的描述保留） |
| §9 证据表 | ✅ 以真实测试名重建（v0.2 的 V6/V7/V8 失实声明修正；V6/V7/V8 负面测试补齐） |
| 路由安全审计 | ✅ 19 路由全量矩阵，见 [ROUTE-SECURITY-MATRIX](ROUTE-SECURITY-MATRIX.md) |
| V10/V11 | ⛔ 未闭环（按任务书 §11 明确保持为阻断项），见 [REMAINING-BLOCKERS](REMAINING-BLOCKERS.md) |
| 生产姿态 | ✅ 未改变：空 issuer 清单 = 全拒（`ClosedDelegationVerifier`）；未开放任何生产写路由；未执行任何共享环境迁移 |

## 2. 文件变更清单

**删除（O-5）：**

| 文件 | 说明 |
|---|---|
| `migrations/0006_delegation_token_use.sql` | 绑定表迁移（从未在任何共享环境执行） |
| `crates/artifact-application/src/port/delegation_binding.rs` | 绑定端口 |
| `crates/artifact-infrastructure/src/postgres/delegation_binding.rs` | Pg 绑定存储 |

**修改（实现）：**

| 文件 | 变更 |
|---|---|
| `migrations.sha256` | 移除 0006 行（0001–0005 一字未动） |
| `crates/artifact-domain/src/action.rs` | `Action::is_write()`（契约 §5.3 的 11 写动作分类）；O-5→O-6 笔误修正 |
| `crates/artifact-domain/src/error.rs` | 移除 `DelegationTokenReused` 变体与码映射（唯一生产者已删） |
| `crates/artifact-contract/src/error.rs` | 移除 `DELEGATION_TOKEN_REUSED` 409 映射与 §3.4 注释 |
| `crates/artifact-application/src/lib.rs` | 移除绑定端口导出；新增 `REQUEST_HASH_ALGORITHM_V1` 导出 |
| `crates/artifact-application/src/port/mod.rs`、`crates/artifact-infrastructure/src/postgres/mod.rs` | 移除绑定模块声明/导出；清理孤儿 helper `persistence_unavailable` |
| `crates/artifact-application/src/service/delegation_request.rs` | v1 规范冻结文档、算法常量、6 组冻结向量测试 |
| `crates/artifact-application/src/service/artifact.rs` | 新增 `get_version`/`list_versions`/`list_references`（服务层 `require` 前置） |
| `crates/artifact-application/src/service/generator.rs` | `list_authorized`（`generation:read` 授权） |
| `crates/artifact-infrastructure/src/delegation/ai_pm.rs` | 受众常量钉死（移除 `TrustedIssuer.audience`）；`biz.request_hash_alg` 校验；写动作 token 必带 `caller_request_id`；新增单测 |
| `apps/artifact-api/src/config.rs` | `validate_all` 受众恒等校验（非常量值启动失败）+ 4 个负面单测 |
| `apps/artifact-api/src/state.rs` | 装配不再透传 audience |
| `apps/artifact-api/src/routes/mod.rs` | `list_generators` 挂 `Authed` + 服务层授权 |
| `apps/artifact-api/src/routes/artifacts.rs` | 三个读 handler 改走服务层 |
| `openapi/openapi.yaml` | `/v1/generators` 移除 `security: []`、补 401/403 与说明 |
| `crates/artifact-client/src/lib.rs` | `generators(ctx)` 需凭证 |

**修改（测试）：**

| 文件 | 变更 |
|---|---|
| `tests/tests/delegation_security.rs` | 删除 s13/s14/s15+s19/s17/s20（绑定表专属）；新增 v4/v6/v7/v8（PoC P1–P7 移植）、PC-O5-1 两用例、读 handler 两用例；`base_claims` 补 `caller_request_id`；s21 覆盖 generators 全拒 |
| `tests/tests/api_meta.rs` | generators 用例改带凭证；新增 `generators_require_delegation_and_action` |
| `tests/tests/sdk_contract.rs` | "SDK 无凭证可用面"收敛为 health/ready；generators 无凭证 401 断言 |
| `tests/tests/contract_openapi.rs` | 取消 `/v1/generators` 的 401 文档豁免；形状测试带凭证 |

**修改（契约文档）：** `docs/architecture/WP-LMMS-DELEGATION-01-AUTHORIZATION-CONTRACT.md`（v0.2 → v0.3，见 [CONTRACT-CHANGES](CONTRACT-CHANGES.md)）。

## 3. 关键设计决策（含边界）

1. **caller_request_id 红线的执行层 = 验证器（claims 形状），不是服务层目标比对。** 写动作 token 缺 `caller_request_id` 在验证层 401 拒绝（`ai_pm.rs`）；这与 V10 的"目标一致性比对"（caller/hash/artifact/operation 四比对，AS-4 接线）不同层、不越界。测试替身 `TestDelegationVerifier` 仅存在于 `test-doubles` feature、不进发布二进制，业务回归不受红线影响（安全场景全部走真实 `AiPmDelegationVerifier`）。
2. **request_hash 保持"形状校验已启用、目标校验未接线"的分层**（PC-O6 前置条件结构）：claim 存在时的配对/格式/算法校验是形状拒绝；`assert_delegation_matches_request` 仍零调用点。
3. **audience 三层钉死**：常量（验证器内部比对）→ 无字段（`TrustedIssuer` 结构上不可配置）→ 恒等校验（配置层防御性拒绝，含大小写/空白变体）。
4. **读端点动作映射**：versions → `version:read`；references 列表 → `artifact:read`（Q4 冻结 15 动作无 `reference:read`，引用属 Artifact 内容元数据；精确动作、非通配）；generators → `generation:read`（R2 建议）。已登记契约 §3.2 读端点动作表。
5. **未顺手放宽任何校验**：issuer 清单机制（Q6 设计的平滑过渡机制）保持配置驱动 + 恰一项强制；algorithm 恒 RS256 硬编码；kid-strict 不变（信任边界复核见 [AUDIENCE-SECURITY](AUDIENCE-SECURITY.md) §4）。

## 4. 测试与 CI

- 本地全量：`cargo test --workspace -- --test-threads=8` → **241 通过 / 0 失败**（基线 226：−6 绑定表专属用例，+21 新用例）。
- `cargo fmt --check` ✅；`cargo clippy --workspace --all-targets --all-features -- -D warnings` ✅（0 警告）。
- 迁移链：`migration_guard`（sha256 清单互检）+ 全套 sqlx::test 每测试建库走 0001–0005 迁移 ✅。
- 环境说明：本地 Windows dev PG（`artifact-dev-pg@16500`，隔离）。首轮跑测试时该容器处于停止状态导致 `PoolTimedOut`（环境故障，非代码问题），启动容器并清理 4 个残留 `_sqlx_test%` 库后全绿；已按任务书要求如实记录，未把环境失败伪装成通过。
- CI：**本分支未推送（任务书 §17 禁止），无 CI 运行**。基线 `40848f1`（PR #12 head）CI 全绿（GitHub 独立查询，2026-11-26 核实）。本仓库 CI 仅在 `main`/`docs/**` push 或 PR 打开时触发——推送与开 PR 均待明确授权。
- 分类：59 库单元测试 / 182 集成测试（真实 HTTP + 隔离 PG）/ R2 PoC 保持为独立评审工具（未混入本工作包提交）。

## 5. 完成判据对照（任务书 §16）

| 判据 | 状态 |
|---|---|
| O-5 绑定表和迁移 0006 已按裁决移除 | ✅ |
| delegation 写操作强制要求 caller_request_id | ✅（验证器强制 + 契约红线 + 负面测试；目标比对属 V10） |
| O-6 request_hash 版本化 + 契约定义，未冒进接线 | ✅ |
| audience 固定 `artifact-service`，不可配置覆盖 | ✅ |
| 三个读 handler 修复授权检查 | ✅ |
| `/v1/generators` 完整认证授权 | ✅ |
| 不存在已发现的 HTTP 路由认证授权绕过 | ✅（全量矩阵复核，见 ROUTE-SECURITY-MATRIX） |
| 错误的契约 §3.4 引用已修正 | ✅ |
| V6/V7/V8 测试证据声明真实准确 | ✅（§9 重建 + 测试补齐） |
| 全量测试和 CI 通过 | ✅ 测试 / ⚠️ CI 未跑（分支未推送，待授权） |
| 文档完整 | ✅（本目录 8 份） |
| V10/V11 与 AI-PM 缺口仍被明确标记为阻断项 | ✅（REMAINING-BLOCKERS） |
| 生产默认拒绝策略没有改变 | ✅ |

## 6. 最终声明

**WP-03 是 AS 契约与实现修复，不是授权闭环完成，也不是生产开放批准。** 授权闭环的签发侧（AI-PM 全部）与执行侧两闸（V10/V11 接线）仍为阻断项；生产默认拒绝策略未改变；未合并任何 PR、未推送任何分支、未触碰 R2 评审分支、未修改 LMMS、未连接共享/生产数据库。
