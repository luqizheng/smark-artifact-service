# WP-LMMS-DELEGATION-03 — AUDIENCE-SECURITY（audience 固定与信任边界复核）

| | |
|---|---|
| 缺陷 | R2-F-06（P2，PoC P8 实证）：契约固定受众仅靠默认值保证，配置可覆盖为任意非空值（如 `coder-micro-services`），AS 将接受对应 aud 的 delegation token |
| 修复 | 受众升级为**编译期常量信任根**：验证器内部恒以常量比对；结构上移除可覆盖面；配置层恒等校验（含大小写/空白变体） |

## 1. 固定策略（三层，选择"语义最明确、误配置风险最低"的形态）

| 层 | 修复前 | 修复后 |
|---|---|---|
| 验证器（信任判定点） | `validation.set_audience(&[trusted.audience])` + `claims.aud != trusted.audience`（信任配置字符串） | `validation.set_audience(&[DELEGATION_AUDIENCE])` + `claims.aud != DELEGATION_AUDIENCE`（`"artifact-service"` 常量，`ai_pm.rs` pub const，契约 §3.1.1） |
| 受信签发方结构 | `TrustedIssuer { issuer, jwks_url, audience }`——受众是可注入字段 | `TrustedIssuer { issuer, jwks_url }`——**受众不再是字段**：构造后不存在可被外部输入改变受众的路径（编译期保证"验证器构造后不能被外部配置改变其信任边界"） |
| 配置层（`ARTIFACT_API_DELEGATION_ISSUERS`） | 仅查非空 | `audience` 字段保留兼容但必须**精确等于** `artifact-service`；任何其他值（`coder-micro-services`、空串、`Artifact-Service`、`artifact-service ` 等）→ `validate_all` bail → **进程启动失败**。不做 trim/大小写归一化——绕过即失败，不存在归一化旁路 |

保留配置字段的理由：兼容既有配置结构（任务书允许的两种形态之一），同时以恒等校验实现"非 `artifact-service` 启动失败"；`serde(default)` 使未写字段的旧配置自然等于常量。

## 2. 负面测试矩阵（全部真实运行通过）

| # | 场景 | 期望 | 测试 |
|---|---|---|---|
| 1 | 正确受众（aud=`artifact-service` 单值） | 接受 | delegation_security :: s01（E2E 201）；`explicit_contract_audience_is_accepted`（配置） |
| 2 | 错误受众（aud=`file-service`） | 拒绝 | s04 |
| 3 | 配置 audience=`coder-micro-services`（PoC P8 形态） | **启动失败** | config :: `non_contract_audience_config_fails_startup` |
| 4 | 配置 audience 为空 | 启动失败 | config :: `empty_audience_config_fails_startup` |
| 5 | 大小写/空白绕过（`Artifact-Service`/`artifact-service `/` ARTIFACT-SERVICE` 等） | 启动失败 | config :: `audience_cannot_be_bypassed_via_case_or_whitespace` |
| 6 | 多受众数组（含 `artifact-service`） | 拒绝 | delegation_security :: `v4_array_audience_even_containing_expected_is_rejected`（claims `aud: String` 反序列化失败 + 显式复核双闸） |
| 7 | 构造后改变信任边界 | 不可能 | 结构性保证：`TrustedIssuer` 无 audience 字段（无测试面可构造）；验证器内部仅引用常量 |
| 8 | 空 issuer 清单（缺省） | 全拒姿态不变 | s21 + `state::default_verifier_is_fail_closed` + `empty_issuer_list_is_accepted_and_means_fail_closed` |

## 3. 其他信任根的同类问题核查（任务书 §7 要求）

| 配置项 | 是否存在同类可覆盖问题 | 处置 |
|---|---|---|
| `issuer`（受信签发方标识） | **不钉死为常量，维持配置驱动——设计如此**：Q6 的"受信 issuer 清单"是候选 A（LMMS 未来上线委托签发）平滑过渡的既定机制（契约 §2/§7），issuer 清单是信任根本体（配置即信任决策），与 audience（契约常量、无演进机制）性质不同。既有防线已足够：恰一项 active 强制（`multiple_active_issuers_are_rejected_single_issuer_q6`）+ 空清单全拒 + 签名验证强制 iss 真实性 | 维持；已在契约 v0.3 显式记录该裁决（CONTRACT-CHANGES §未做修订） |
| `jwks_url` | 配置驱动 = 信任根定位（设计如此）；既有防线：https 强制（回环 http 豁免仅 dev）、禁重定向、证书校验（R2 §5 密码学配置轮已审，R2-F-10 残余 DNS 风险维持记录） | 维持 |
| algorithm | RS256 硬编码（V1），无配置面 | 维持（无放宽） |
| key 材料 | 仅来自 JWKS 拉取（kid-strict，缺失 kid 拒绝、未知 kid 单次限频刷新） | 维持 |
| `purpose`/`actor_type`/TTL/leeway 常量 | 均为代码常量，无配置面 | 维持 |

**未顺手放宽任何 issuer/algorithm/kid 校验**；本 WP 对信任边的唯一变更是收紧（audience 从"默认值"升级为"不可变常量"）。

## 4. 遗留关联项（非本 WP 范围）

- LMMS 侧无 AI-PM 专用 audience（P0-01 残余，属 LMMS 后续需求）——AS 侧 `purpose==delegation` + 固定 aud 双闸不受影响（PoC P1 亦已移植为 `v7_purpose_must_be_delegation`）。
- R2-F-07（JWKS 最小模长）/ R2-F-08（锁内 fetch）/ R2-F-09（审计缺 jti）维持 RELEASE-GATE G5 建议项，见 REMAINING-BLOCKERS。
