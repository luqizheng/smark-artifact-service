# WP-LMMS-DELEGATION-04 — 测试证据报告（TEST-EVIDENCE）

> **文档版本**：§0–§6 为 WP-04 原始证据（历史记录，保留不改）；**§7 为 WP-04-R1 复评修复的新增证据**。两者不可混用：WP-04 的 276 passed 对应其终态 HEAD `d6258e9`；R1 的 282 passed 对应 R1 终态 HEAD（见 §7）。

## 0. 基线与隔离（WP-04 原始）

| 项 | 值 |
|---|---|
| 实现基线 | `8fd5173061f935513246b952195e88cea39e8515`（WP-03 HEAD；契约 v0.3、PC-O5-1/O-6 机制未回退） |
| 文档分支父提交 | `e3bc3ba`（任务书） |
| 实现分支 | `feat/wp-lmms-delegation-04-operation-binding` @ `8f59c97`（本 WP 终态 HEAD，见 §6） |
| worktree | `C:\projects\smark-as-wp04`（独立；主工作区 / PR #14 工作区 / 其他并行任务零触碰） |
| 提交 | `7e140af` feat（V10 接线 + unkeyed 单事务）、`7fd9360` test（对抗套件 + 既有套件迁移）、`8f59c97` docs |
| PR | **Draft** #15 → base `feat/wp-lmms-delegation-03-as-remediation`（未合并；不合并任何 PR） |

## 1. Discovery 缺口清单（改动前，基线 `8fd5173` 实测）

- **V10 零生产调用点**：`assert_delegation_matches_request` 仅测试引用；`artifact_id`/`operation_id` 零目标比对；`biz.request_hash` 仅形状校验。契约 §3.1.2 场景②不成立。
- **caller_request_id 与请求无任何关联**：服务层 `request_id` 参数 = `X-Request-Id` HTTP 追踪 ID（易混淆点，非 token caller）。
- **AS-1 request_hash（原始 body sha256）与 v1 指纹是两条不同的轴**，同存于 `idempotency_record.request_hash char(64)`，靠 `endpoint` 列区分。
- **G2-1**：unkeyed+`Idempotency-Key` 创建 reserve→insert→complete 跨 3 事务；崩溃窗口 = 已提交 Artifact + 永久 pending 行（重试 409 直至 24h TTL 清理）。
- **G2-2**：unkeyed 业务失败写错误快照 → 同键重试 `from_value::<ArtifactResponse>` 失败 → **500**。
- **AS-2 端口未接入 AppState**：`request_generation` 生产路径恒 503（零写入）；migration 0005 机制完好。
- **路由↔OpenAPI 双向对账**：19/19 一致，无未登记路由。

## 2. 本地验证（Windows dev PG：Docker `artifact-dev-pg` @ 127.0.0.1:16500，`postgres:16`，sqlx::test 每用例独立库 + migrations）

| 命令 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | ✓ |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✓（0 warning） |
| `DATABASE_URL=… cargo test --workspace -- --test-threads=8` | **276 passed / 0 failed**（22 个测试二进制；较 WP-03 基线 241 增加 35 = 新增 29 例 + 单测 6 例） |
| `cargo build --release`（api，无 test-doubles） | ✓ |
| `cargo build --release -p artifact-worker` | ✓ |
| migration immutability guard（`migration_guard` 8 例） | ✓（无迁移文件改动；新机制复用既有表） |

### 2.1 新增套件 `wp04_operation_binding`（29 例，真实 PostgreSQL）

| 组 | 测试 | 证明 |
|---|---|---|
| A 正向 | `a1`–`a9` | 9 条写路由逐一：精确绑定放行；W3/W4 绑定通过后到达 P0 存储门（503 ≠ 403）；W8 绑定先于 generator 门；W9 契约语法 `generation_job:{job_id}` 放行 |
| B 变异 | `b1`–`b8` | 换目标/换 method/body 变异/同动作异目标/artifact_id claim（失配 403 + 匹配 200 + 无目标路由 403）/cancel operation_id 失配 403 匹配 200/无 caller·半配对·伪造 hash 全 403（服务层独立防线）/跨项目·跨租户绑定完整 token 仍被 scope 拒绝 |
| C 冻结向量 | `c1` | 测试侧 v1 实现 == WP-03 冻结常量（逐字节）+ 完整 body 的 HTTP 全链路服务端重算比对一致放行 |
| D 重放/并发/失败 | `d1`–`d9` | 同 token 同请求重放安全（keyed）；同 caller 异指纹 403 + 同 body 重放 CAS 确定性 409；unkeyed 重放（单事务，resolution/replay 头）；unkeyed 并发 8 → 恰 1 created；keyed 并发 8 → 恰 1 created + 恰 1 身份行；**提交前失败零幂等痕迹**（触发器注入）+ 解除后重试恰一次；**legacy 错误快照重放确定性 409**（G2-2）；cancel 重试终态空操作（审计恰 1）；cancel 并发 6 全 200 且审计恰 1 |
| E 副作用顺序 | `e1`/`e2` | 绑定拒绝 → 零 DB 痕迹且 storage/generator 门未被触达；job 创建 body caller ≠ claim → 403，相等 → 通过绑定到达 503 门 |

**变异敏感性**：B 组全部 403 由 `enforce_write_binding` 产生——移除/绕过该调用后动作+作用域检查仍放行、写入成功，上述测试随即失败（满足任务书 §7"测试必须在 V10 被移除时失败"）。

### 2.2 既有套件迁移（语义不变）

`artifact_flow`(6)、`as1_creation_identity`(38)、`as2_caller_identity`(34)、`generation_api`(11)、`delegation_security`(26，真实 RS256 签发链路 + `biz.request_hash` 跨层)、`sdk_contract`(5)、`api_meta`(4)、`contract_openapi`(8)、`default_deny`(2)、`migration_*`(18)、`roles_separation`(1)、`worker_*`(26)。AS-1/AS-2 全部既有幂等/并发/崩溃恢复测试零语义改动通过；跨 scope 负面语义（`as1 :: replay_requires_fresh_authorization…`、`cross_project_token_is_denied_without_disclosure`）保持。

## 3. CI（PR #15）

见 §6（CI 完成后回填：run URL、逐 job 结论）。

## 4. 显式非声明（红线重申）

1. `caller_request_id` 是幂等/绑定键，**不是防重放保证**（契约 §3.1.2 #6）；token 在 TTL 窗口内对同一绑定请求可重复提交，由端点幂等机制收敛。
2. 本 WP 的全部证据是**隔离 PostgreSQL 上的单元/集成套件**，**不是端到端授权闭环**：V11 撤销闭环（零接线）与 AI-PM 签发链（零实现）仍为阻断项（WP-03 `REMAINING-BLOCKERS.md` 未变）；request_hash 的签发侧同算法实现（PC-O6-3）仍未落地。
3. 生产默认拒绝未变（空 issuer 清单 → `ClosedDelegationVerifier` 全拒）；未配置任何受信 issuer、未执行任何共享环境迁移、未开放任何生产写路径、未合并任何 PR。

## 5. 遗留裁决与阻断项（WP-04 时点；R1 更新见 §7.4）

- **D1（待批准）**：operation_id 语法表——除 cancel 外 7 条写路由保持"存在时不比对不拒绝"；提案见 `DECISION-RECORDS.md`。
- D2/D3（已实现，留档复评）：无目标路由 artifact_id fail-closed；If-Match 不在 v1 指纹。（R1 修订：D2 的无目标集合修正为 W1/W9，W8 为目标寻址路由——见 §7.3 与 DECISION-RECORDS D2 修订版。）
- 未解决阻断项（本 WP 范围外）：V11、AI-PM 全部阶段项、PC-O6-3、F-22 轮换证据、PR #12/#14 的 review 与 CI。

## 6. CI 结果与最终 HEAD（WP-04 原始证据）

- WP-04 终态 HEAD：`8f59c97`（分支 `feat/wp-lmms-delegation-04-operation-binding`；文档回填提交 `d6258e9` 已推送 origin）
- PR：**Draft** #15 → base `feat/wp-lmms-delegation-03-as-remediation`（未合并；本 WP 不合并任何 PR）
- CI run：https://github.com/luqizheng/smark-artifact-service/actions/runs/45748468520（run status: **success**）

| Job | 结论 | 时长 |
|---|---|---|
| fmt | **pass** | 8s |
| clippy + build + test (postgres 16) | **pass** | 3m52s（clippy `-D warnings` + build + 全量测试 + release 双二进制 + 迁移不可变守卫；测试统计：30 个 test-result 行，**276 passed / 0 failed**，与本地一致） |
| docker images + compose | **pass** | 1m55s（镜像构建 + 入口 smoke + compose 校验 + grant 脚本解析） |

> WP-04 完成后状态：V10 接线 ✅（AS 侧）；仍为阻断项：V11 撤销闭环、AI-PM 签发链/撤销存储/PC-O6-3 交叉向量、独立安全复评、生产开放裁决。**本 PR 不是授权闭环完成，也不是生产开放批准。**

---

## 7. WP-04-R1 复评修复证据（新增；与 §0–§6 原始证据分立）

### 7.1 基线与 Git

| 项 | 值 |
|---|---|
| R1 基线 | `d6258e933239966878efff0e4c5ac207c40956a8`（PR #15 head，WP-04 终态） |
| R1 分支 | `fix/wp-lmms-delegation-04-r1`（独立 worktree `C:\projects\smark-as-wp04-r1`；主工作区/PR #14/#15 工作区零触碰） |
| R1 提交 | `d3241c0` fix（P1 query 拒绝）→ `bd07770` docs（P2 W8/D2/D4）→ `eeccc1d` test（F/G 组）→ fmt 提交 |
| **R1 最终 HEAD** | 分支 `fix/wp-lmms-delegation-04-r1` 的 HEAD（完整 SHA 在 R1 交付报告中给出；本提交链不含 CI run——push 未获授权，见 §7.5） |
| push / PR 操作 | **未执行**（任务书 §11 未授权；PR #15 保持 Draft @ `d6258e9`，未修改 head/base） |

### 7.2 R1 修复内容与验证（13 项全过）

| # | 任务书 §8 要求 | 命令/证据 | 结果 |
|---|---|---|---|
| 1 | fmt | `cargo fmt --all -- --check` | ✓ |
| 2 | clippy | `cargo clippy --workspace --all-targets -- -D warnings` | ✓ 0 warning |
| 3 | 全量测试（隔离 PG） | `cargo test --workspace -- --test-threads=8`（dev PG `artifact-dev-pg@16500`，sqlx::test 每用例独立库+迁移） | **282 passed / 0 failed**（= WP-04 的 276 + R1 新增 6） |
| 4 | release API | `cargo build --release` | ✓ |
| 5 | release Worker | `cargo build --release -p artifact-worker` | ✓ |
| 6 | 迁移不可变守卫 | `sha256sum -c migrations.sha256` + `migration_guard` 8 例（全量套件内）；**未新增任何迁移文件** | ✓ |
| 7 | Docker 镜像 | `docker build`（api + worker，`ARTIFACT_GIT_SHA=local-r1`） | ✓ |
| 8 | 入口 smoke | `docker run … __no_such_command__` 双镜像 | ✓ 均 exit 1（二进制层拒绝） |
| 9 | Compose 校验 | `docker compose config -q`（base + worker overlay）+ `bash -n deploy/dev-grants.sh` | ✓ |
| 10 | query 对抗测试 | `wp04_r1_remediation :: f1/f2/f3` | ✓（§7.3） |
| 11 | W8 artifact_id 正反向 | `wp04_r1_remediation :: g1/g2/g3` | ✓（§7.3） |
| 12 | V10 mutation sensitivity | 禁用 query 检查 → `f1` **FAILED**（实测）；恢复 → 全绿。WP-04 既有绑定变异矩阵（`wp04 :: b1–b8`）保持 | ✓ |
| 13 | 既有幂等/并发/重试/跨项目 | `wp04_operation_binding` 29 例 + `as1` 38 + `as2` 34 + 全量回归 | ✓ 零语义改动 |

### 7.3 R1 新增测试清单（`tests/tests/wp04_r1_remediation.rs`，6 例）

| 测试 | 证明 |
|---|---|
| `f1_query_with_params_is_rejected_on_every_write_route` | **全部 9 条写路由**：正确 token+body+`?x=1` → 400 `INVALID_REQUEST`；写表面四表行数零变化；W3/W8 返回 400 而非门 503 → storage/generator 门未触达 |
| `f2_bare_empty_query_is_rejected_on_every_write_route` | 裸 `?` 同样 400（`Uri::query()` 保留存在性 `Some("")`——解析后 query map 会丢失该形态） |
| `f3_positive_control_without_query_still_passes_query_gate` | 同 token 同 body 无 query → 按各路由语义到达既定结果（201/200/204/503）→ 400 确由 query 触发；同时构成变异敏感性的失败前提 |
| `g1_w8_artifact_id_claim_matching_path_passes_target_binding` | W8 claim==路径 → 目标绑定通过 → 503 `GENERATOR_UNAVAILABLE`（**区分 403 绑定拒绝与 503 功能门**）；jobs=0 |
| `g2_w8_artifact_id_claim_mismatch_is_403_with_no_side_effects` | W8 claim≠路径 → 403 `SCOPE_DENIED`；jobs=0；非 503 → 门未触达 |
| `g3_w8_artifact_id_claim_absent_follows_frozen_contract_shape` | claim 缺省 → artifact_id 轴不拒绝（契约 §3.1.2 #1"存在时校验"+ §3.1.1 示例 `artifact_id:null` 合法；决策项 D4）→ 503 门；caller+hash 轴仍完整绑定 |

**Mutation sensitivity 实测**：将 `if uri.query().is_some()` 禁用后 `f1` 即 FAILED（写入绕过 query 门成功）；恢复后全绿。绑定变异矩阵（`wp04 :: b1–b8`）继续覆盖 V10 主检查的敏感性。

### 7.4 R1 文档/裁决更新

- **D2 修订**：无目标路由集合修正为 **W1/W9**；**W8 为目标寻址路由**（handler 传 `Some(路径 artifact_id)`，claim 与路径 ID 精确比对——行为自 WP-04 起一直正确，本轮只修文档/注释不一致，未放宽任何 fail-closed 行为）。
- **D4 新增（决策项，未实现）**：复评要求"W8 缺少必需目标 claim → 拒绝"与契约冻结文本冲突（§3.1.2 #1 只规定存在时校验；§3.1.1 示例 `generation:request`+`artifact_id:null` 合法；§3.3"绑定字段按意图填入"属签发侧）→ 按 §12 停止并记录，未擅自新增必填拒绝；若契约未来澄清，接线为单分支+测试。
- **D1 状态不变（待裁决）**：七条未完成 operation_id 语义绑定的路由 = W1 create_artifact、W2 patch_artifact、W3 create_version_session、W4 commit_version、W5 set_current_version、W6 create_reference、W7 remove_reference（W8 request_generation 同属 D1 未接线集合；W9 cancel 已按契约语法比对）。
- 矩阵/设计说明已同步：query 禁带轴（新）、artifact_id 轴 W8 归类修正、operation_id 轴未完成清单引用。

### 7.5 R1 CI 状态

**未运行**：任务书 §11 未授权 push/PR 更新，CI 无法在本地分支上触发。§7.2 的 13 项本地验证与 CI job 内容一一对应（fmt / clippy+build+test+release+迁移守卫 / docker 三项均已本地执行并记录）。**获得 push 授权后**，PR #15 分支更新将触发同一 workflow；R1 CI run URL 待授权执行后回填。

### 7.6 R1 后的安全边界（不变项核查）

- 九个写服务入口 V10 接线不变；`Delegation::require` 仍先于 `enforce_write_binding`，后者仍先于任何副作用（新增的 query 检查在更前的 HTTP 统一入口）。
- request_hash v1 算法与冻结向量**零改动**（`delegation_request.rs` 未触碰；`c1` 冻结向量测试保持绿）。
- caller_request_id 写路径必填红线、audience 固定、tenant/project isolation、AS-1 keyed、unkeyed 单事务幂等、legacy 错误快照确定性 409、W8 fail-closed/503、W3/W4 storage 阻断、W9 原子状态机与审计语义——全部由全量回归（282/0）覆盖确认。
- 生产默认拒绝未变（`ClosedDelegationVerifier` 全拒）；未配置真实 issuer、未操作真实凭据、未执行共享环境迁移、未开放生产写路径、未合并任何 PR。
- **本 R1 证据仍是隔离 PostgreSQL 上的单元/集成证据，不是端到端授权闭环**：V11、AI-PM 签发链、PC-O6-3 仍为阻断项（WP-03 `REMAINING-BLOCKERS.md` 未变）。
