# WP-LMMS-DELEGATION-03 — O5-REMOVAL（O-5 绑定表移除证据）

| | |
|---|---|
| 裁决 | R2 O-5 = **B（移除）**：删除迁移 0006 与 `PgDelegationBindingStore`，遵守冻结契约 §5.1"AS 不加表"；附强制前置条件 PC-O5-1（写路径必带 `caller_request_id`）与再评估通道 |
| 执行 | WP-03 已全部执行；绑定表从未在任何共享/生产环境执行过迁移（R2 核实 + 本 WP 核实：仅 sqlx::test 每测试独立建库丢弃，不构成"已执行"事实）→ 未合并前移除**无迁移回滚负担** |

## 1. 删除文件清单

| 文件 | 内容 |
|---|---|
| `migrations/0006_delegation_token_use.sql` | `artifact.delegation_token_use` 表 + `uq_delegation_token_use_jti` 唯一约束 + 配对 CHECK + `purge_delegation_token_use` SECURITY DEFINER 函数 |
| `crates/artifact-application/src/port/delegation_binding.rs` | `DelegationBindingStore` 端口、`DelegationTokenUse`、`DelegationBindingOutcome` |
| `crates/artifact-infrastructure/src/postgres/delegation_binding.rs` | `PgDelegationBindingStore::consume`（INSERT ON CONFLICT + FOR UPDATE 重判） |

## 2. 关联清理（隐式依赖检查）

| 位置 | 清理内容 | 方式 |
|---|---|---|
| `crates/artifact-application/src/lib.rs` / `port/mod.rs` | 端口导出与模块声明 | 删除；`cargo check` 证实无残留引用 |
| `crates/artifact-infrastructure/src/postgres/mod.rs` | `pub mod`/`pub use PgDelegationBindingStore` + 孤儿 helper `persistence_unavailable`（唯一调用方是绑定存储；clippy `-D warnings` 验证删除后无 dead_code） | 删除 |
| `crates/artifact-domain/src/error.rs` | `DomainError::DelegationTokenReused`（唯一生产者是绑定存储） | 删除变体 + 码映射 |
| `crates/artifact-contract/src/error.rs` | `DELEGATION_TOKEN_REUSED` 409 映射 + "契约 §3.4"注释 | 删除（OpenAPI 错误枚举从未收录该码，无漂移） |
| `crates/artifact-infrastructure/src/delegation/ai_pm.rs` | 模块文档中对绑定存储的引用 | 改写为"由 AS-1/AS-2 幂等机制承接" |
| `migrations.sha256` | 0006 行 | 删除；0001–0005 一字未动 |
| `tests/tests/delegation_security.rs` | s13/s14/s15+s19/s17/s20 + `binding_use`（仅服务于绑定表的测试） | 删除 |

其余代码全仓 grep `delegation_binding|DelegationBinding|delegation_token_use|DelegationTokenUse`：业务实现零残留（仅本文档与评审文档的历史记录）。

## 3. 迁移链完整性检查

- `migrations/` 现含 0001–0005，编号连续、链式无缺口；`migrations.sha256` 与目录互检（双向：清单内文件不可改、目录内文件必须列清单）由 `migration_guard :: released_migrations_are_unchanged` 强制——本分支通过。
- 全套 sqlx::test（每个 DB 测试独立建库）实际执行 0001→0005 迁移链 ✅（全量 241 测试通过）。
- `deploy/db-roles.sql` 无 delegation_token_use 引用（按 ON ALL TABLES 授权机制，无需修改）。

## 4. 幂等机制复用说明（并发仲裁职责承接核查，任务书 §5.7/5.8）

R2 裁决依据第 3 点要求证明"既有机制承担预期的并发仲裁职责"。核查结论：

| 机制 | 覆盖面 | 并发仲裁强度 | 证据 |
|---|---|---|---|
| AS-2 caller 三元组（migration 0005 `(tenant_id, project_id, caller_request_id)` 部分唯一索引 + `create_generation_job_with_input` v2） | Job 创建（写） | DB 唯一约束原子判定：同键同指纹 → `caller_request_replay` 零写入；同键异指纹 → 409 `CALLER_REQUEST_ID_REUSED` | as2_caller_identity 34 用例（含 as2_17 并发同键竞态恰一创建——与被删 s14 同强度断言） |
| AS-1 artifact_key / Idempotency（单事务端口） | Artifact 创建（写） | 单事务幂等解析 + 插入 + 身份写入原子；同键异指纹 409 | as1_creation_identity 38 用例 |
| `Idempotency-Key` 头幂等 | 带 key 的 create | reserve/complete 两段 + hash 比对 | artifact_flow 族 |

**未删除任何业务需要的幂等机制**（任务书 §5.8）：上述三条全部保留且测试齐全。被删的绑定表与它们的关系：绑定表是"jti 级第二道闸"，其两项硬闸能力（并发重放恰一收敛、跨请求 jti 复用拒绝）在声明层分别由业务幂等（同键收敛）与 V10 目标一致性（跨请求拒绝，AS-4 接线，见 REMAINING-BLOCKERS）承担——与 R2 O-5 裁决分析一致。

## 5. PC-O5-1 红线落地（强制前置条件 1）

- 契约 §3.1.1/§3.1.2：`caller_request_id` 写路径必填 + 8 条边界声明（格式/缺失拒绝/重复处理/并发仲裁/幂等键关系/隔离性/非授权凭证）。
- 实现强制：`ai_pm.rs` 验证器——token actions 含任一写动作（`Action::is_write()`，契约 §5.3 的 11 动作，与 Q1 撤销清单同集）且无 `caller_request_id` → 401（fail-closed，先于一切授权/写入）。
- 负面测试（缺 `caller_request_id` 的写请求不能成功）：
  - 单元：`ai_pm :: write_actions_require_caller_request_id`（11 写动作/4 读动作分类矩阵）；
  - 验证器：delegation_security :: `write_action_token_without_caller_request_id_is_rejected`（纯写 + 混合读写拒绝；纯读放行对照；带 caller 的写 token 放行对照）；
  - HTTP 层：delegation_security :: `write_without_binding_is_rejected_over_http`（真实 RS256 + 回环 JWKS + 真实路由 → 401，消息引用缺失绑定）。
- **边界声明**：`caller_request_id` 是幂等/绑定键，不是授权凭证；身份认证仅由签名 + §3.2 矩阵决定（任务书红线，契约 §3.1.2 第 8 条显式声明）。

## 6. 再评估通道（PC-O5-2）

若 AS-4 联调/威胁评审证明仍需 DB 兜底（例如幂等层无法覆盖的写端点出现），按契约修订流程重新提案；届时以 R2 评审过的绑定表设计为底稿，附完整契约章节、事务边界修订与全部消费路由清单。本 WP 未预设该需求。
