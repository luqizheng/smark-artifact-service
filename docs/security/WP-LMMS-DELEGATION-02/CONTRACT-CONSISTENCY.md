# WP-LMMS-DELEGATION-02 — CONTRACT-CONSISTENCY（契约与实现一致性核对）

| | |
|---|---|
| 状态 | R2 独立安全评审交付（2026-11-25） |
| 核对对象 | 契约 v0.2（冻结提交 AS `26a37f3`，实现提交 `40848f1` 追加 O-5/O-6/§9）↔ AS PR #12 实际代码 ↔ AI-PM PR #11 |
| 核对方法 | 逐条契约条款 → 源码定位 → 测试证据独立运行复核（隔离 PG @16500 + R2 PoC） |

## 1. 冻结完整性核对

| 项 | 结果 |
|---|---|
| Q1–Q6 冻结裁决原文 | `26a37f3` → `40848f1` diff 核对：**未被改动** ✓ |
| §5.1 "AS 不加表" 冻结裁决 | 原文未动 ✓；实现（迁移 0006 + PgDelegationBindingStore）**违反该裁决**，以 O-5 提请裁决（程序上诚实，引用上失实——见 R2-F-04） |
| 契约文档在冻结后被修改 | `40848f1` 追加 §8 O-5/O-6 两行 + 新增 §9 证据映射（31 行）。追加内容自我声明"待独立评审/待裁决"，未改动冻结语义——**程序可接受，但必须记录**：契约文档在冻结提交后并非零改动 |
| §9 证据映射表 | **多处失实**：V6/V7/V8 声称的测试证据不存在（见 R2-F-12）；V10/V11 两行如实声明未接线 ✓ |

## 2. 逐条一致性矩阵

| 契约条款 | 实现状态 | 一致性判定 |
|---|---|---|
| §2 候选 B（AI-PM 签发，AS JWKS 验签） | `AiPmDelegationVerifier` + `JwksCache` + issuer 配置 | **一致**（AS 侧）；AI-PM 签发侧不存在 |
| §3.0 七项验证链 | AI-PM 侧零实现；F-25 确认仍在（`verify.rs` `validate_aud=false`） | **一致于"未实施"声明**；属 RELEASE-GATE 前置 |
| §3.1.1 claims 表 | `DelegationClaims` 字段与契约逐字段比对：必填集一致（iss/sub/aud/tenant_id/project_id/actions/iat/exp/jti/delegation_id/purpose）；可选集多出 **`actor_type`**（契约无此 claim） | **基本一致 + 一处未申报扩展**：`actor_type`（缺省 User，白名单 user/agent/service）未列入契约 §3.1.1 也未入 O 清单——需并入契约或移除（P2 级，随契约修订处理） |
| §3.1.1 TTL 硬上限 300s / ±60s | `MAX_TOKEN_TTL_SECS=300`、`CLOCK_SKEW_LEEWAY_SECS=60`、`validate_time_claims` 边界测试（300 过/301 拒） | **一致** ✓ |
| §3.1.1 jti UUID | `claims.jti.parse::<Uuid>()` | 实现一致；测试缺失（R2-F-12） |
| §3.1.1 "aud 精确单值禁止数组" | serde `aud: String`（数组反序列化即败）+ `set_audience` + 显式复核；PoC P7 数组 aud 拒绝 | **实现一致**；但受众值可被配置覆盖（R2-F-06） |
| §3.1.2 五场景判定 | ①③⑤ 经绑定表/幂等（绑定表未接线）；② 绑定字段不比对——**不满足**；④ 依赖撤销（未实现） | **部分不一致**（R2-F-01/02）；契约 §9 已如实声明 V10/V11 属 AS-4 |
| §3.2 V1 | header.alg==RS256 严格 | 一致 ✓（s02 族） |
| §3.2 V2 | kid 必填（缺失拒绝）+ 未知 kid 单次限频刷新 | 一致 ✓（s05 出站次数断言、s05b 轮换） |
| §3.2 V3 | iss 精确 + 配置层恰一项（Q6） | 一致 ✓（s03、config 单测） |
| §3.2 V4 | 精确单值 aud | 一致 ✓（s04 + PoC P7） |
| §3.2 V5 | exp/iat/nbf/TTL/偏差 | 一致 ✓（s07/s08/s09 + 单测边界） |
| §3.2 V6 | jti UUID | 实现 ✓，测试缺（R2-F-12） |
| §3.2 V7 | purpose==delegation | 实现 ✓（PoC P1/P1b 验证），测试缺（R2-F-12） |
| §3.2 V8 | 非空/无重复/∈15 动作 | 实现 ✓（PoC P3–P6 验证），测试缺（R2-F-12） |
| §3.2 V9 | tenant 恒凭证、header 仅定位 | 一致 ✓（s10/s11/s18）；`create_artifact` 用 body.project 定位（契约允许，R2-F-14 备注） |
| §3.2 V10 | 绑定形状解析仅 | **未实现目标一致性**（已声明）— R2-F-01 |
| §3.2 V11 | 端口 + fail-closed 默认 | **未接线**（已声明）— R2-F-02；503 错误码映射存在 |
| §3.2 V12 | 拒绝审计 method/path；允许审计 delegation_id | **不满足**（无 jti/subject/原因）— R2-F-09 |
| §3.3 签发红线 | AI-PM 侧零实现 | 与"未实施"一致；红线文本与计划一致 |
| §4.1 AI-PM 密钥/JWKS 端点 | 不存在 | 与"未实施"一致 |
| §4.2 JWKS 缓存策略（Q5） | 5min/15min/60s 限频/重启空缓存 fail-closed/超陈旧 fail-closed/禁重定向/https 强制 | **一致** ✓（常量契约测试 + s06/s06b）；补强项见 R2-F-06/07/08/10 |
| §5.3 写路径撤销时序 | 未接线 | 不一致（已声明）— R2-F-02 |
| §5.1 "AS 不加表" | 违反（迁移 0006） | **冲突，待 O-5 裁决**；引用失实（R2-F-04） |
| Q3 "所有受保护 API 含 GET 均要求 delegation" | `get_version`/`list_versions`/`list_references` 缺动作检查；`/v1/generators` 无认证 | **违反** — R2-F-03 |
| 错误码族 | UNAUTHENTICATED 401 / SCOPE_DENIED 403 / DELEGATION_VERIFIER_UNAVAILABLE 503 / REVOCATION_UNAVAILABLE 503 / DELEGATION_TOKEN_REUSED 409 | 一致 ✓（error.rs 映射核对；后两者运行路径未接线） |
| 私钥/Token 不入日志 | 错误消息无 token 断言（`rejects_with`）；测试密钥运行期生成；无私钥日志路径 | 一致 ✓ |

## 3. AI-PM PR #11 与契约的一致性

- 文档内容与契约 v0.2 语义一致（Q1–Q6 转述准确、P0-01 租户 UUID 方案一致、F-22/F-25 如实声明）✓。
- PR 仅 1 文档文件；集成说明第 2 节所述全部组件均未实现（与"评审通过前不得实现"的自我约束一致）。

## 4. LMMS 只读核对（契约事实基线复核）

| 契约声明 | R2 独立核实 |
|---|---|
| 用户 JWT aud 硬编码 `coder-micro-services`（issuer.rs:194） | ✓ `services/token-service/src/issuer.rs`（`Audiences::AsString("coder-micro-services")`，保留 claims 过滤 + purpose 签发侧重打） |
| `verify_token` 第三参为 expected_aud，None 即不校验 | ✓ `libs/common-jwt/src/verify.rs`（`None → validate_aud=false`） |
| 缺失 kid 回退 default key（verify.rs:80-82） | ✓ 仍存在（`None => cache.default_key()`）；AS 验证器独立强制 kid 必填，不依赖该库——契约修复要求成立 |
| `digua_common_jwt` JWKSCache 缺省刷新 3600s | ✓ `cache.rs:36`（契约据此要求 AS 自建 5min/15min 缓存——已实现） |

## 5. 结论

1. **AS 验证段实现与契约一致性良好**（V1–V5、V9、Q5、Q6、错误码族全部一致且有真实测试）。
2. **三处契约-实现冲突**：Q3/V8（读端点动作绕过）、§5.1（AS 加表）、§9（证据失实）——分别记 R2-F-03、O-5、R2-F-12。
3. **一处未申报扩展**：`actor_type` claim（低风险，随契约修订处理）。
4. **契约修订需求清单**（O-5/O-6 裁决的落地形态见 [O5-O6-DECISIONS](O5-O6-DECISIONS.md)）：删表或新增 §3.4；request_hash 并入 §3.1.1 + 版本标识 + path/query 规范冻结；`actor_type` 入表或移除；修正 §9 证据表；V5 补 leeway 语义；读端点豁免清单（若豁免 /v1/generators 需明示理由）。
