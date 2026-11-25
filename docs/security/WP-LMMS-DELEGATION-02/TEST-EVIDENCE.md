# WP-LMMS-DELEGATION-02 — TEST-EVIDENCE（测试证据独立复核）

| | |
|---|---|
| 状态 | R2 独立安全评审交付（2026-11-25） |
| 方法 | 不复用交付方运行结果：独立配置隔离环境（本地 dev PG `artifact-dev-pg@16500`，每测试独立库），独立运行全部套件；另以受控 PoC（独立 crate，非生产实现）对交付矩阵未覆盖的负面路径做变异式验证 |

## 1. 独立运行结果（R2 复核环境）

| 套件 | 结果 | 备注 |
|---|---|---|
| `cargo test --workspace --lib` | **48 passed / 0 failed** | 含 `ai_pm`(5)、`jwks`(3)、`config`(6)、`revocation`(1)、`state`(1)、`delegation_request`(4) 等 |
| `cargo test --test delegation_security`（`--test-threads=8`） | **23 passed / 0 failed**（8.66s） | 交付声明的 23 场景全部独立复现 |
| `cargo test --workspace`（全量） | **TOTAL PASSED: 226** | 与交付声明"226 通过"**一致** ✓ |
| R2 PoC（`poc/r2-verifier-poc`，独立 crate） | **1 passed**（10 项断言全过） | 见 §3 |

CI 佐证：PR #12 head `40848f1` 上 fmt / clippy+build+test(postgres16) / docker 三组全绿（GitHub 独立查询）。PR #11 **无任何 CI check**（ai-pm 仓库对该分支无 workflow 触发）。

## 2. 23 场景逐项有效性判定

判定标准：测试是否真实进入被测代码路径（HTTP 层=经 router 真实请求 + 真实 DB；验证器层=真实验证器 + 真实回环 JWKS 服务 + 运行期 RSA 密钥；DB 层=真实迁移 schema + 真实约束）。**无任何 mock-only 场景**（唯一测试替身 TestDelegationVerifier 仅用于既有业务回归与 s22 字段扩展回归，不用于安全场景）。

| 场景 | 路径层级 | 有效性 | 备注 |
|---|---|---|---|
| s01 正常 token E2E | HTTP+DB | ✓ 有效 | 201 创建 + 200 读取全链路 |
| s02 错误签名 | 验证器 | ✓ 有效 | kid 指向受信键、签名来自他键（真实验签失败路径） |
| s03 错误 issuer | 验证器 | ✓ 有效 | |
| s04 错误 audience | 验证器 | ✓ 有效 | aud=file-service |
| s05 未知 kid + 限频 | 验证器 | ✓ 有效 | **含出站次数断言**（fetches==1 两次）——真实防打爆证据 |
| s05b 轮换双活 | 验证器 | ✓ 有效 | JWKS 追加新 kid 后刷新可验 |
| s06 JWKS 断供 | HTTP | ✓ 有效 | 真实连接拒绝 → 503 `DELEGATION_VERIFIER_UNAVAILABLE` + retryable 断言 |
| s06b 缓存 Unavailable | 缓存层 | ✓ 有效 | |
| s07 过期（含偏差边界） | 验证器 | ✓ 有效 | exp=now-120（>60s 偏差） |
| s08 未来 iat | 验证器 | ✓ 有效 | +3600s |
| s09 超长 TTL | 验证器 | ✓ 有效 | 3600s > 300s |
| s10 跨租户 | HTTP+DB | ✓ 有效 | 404 不泄露存在性 |
| s11 claim/头不一致 | HTTP+DB | ✓ 有效 | 403 `SCOPE_DENIED` |
| s12 动作越权（V9 层） | HTTP+DB | ✓ 有效 | 只读 token 写入 → 403。**注意：是 V9/require 层，不是 V8 形状层** |
| s13 同 jti 异指纹/caller/scope | DB | ✓ 有效 | 顺序三连拒 |
| s14 并发同 jti 同指纹 | DB | ✓ 有效 | `tokio::join!` 恰一 Fresh；**同指纹**（异指纹并发未测，R2-F-11） |
| s15/s19 重试合法 + 跨实例 | DB | ✓ 有效 | 模拟重启（新 store 实例） |
| s16 指纹篡改 | 声明层 | ✓ 有效 | 函数级（AS-4 接线消费点尚不存在） |
| s17 撤销语义（TTL 窗口） | DB | ✓ 有效 | 验证"残余窗口内改绑仍拒"——**不是撤销功能测试**（撤销未实现），测试名易误导，已在本文澄清 |
| s18 缺 X-Project-Id | HTTP | ✓ 有效 | 400 |
| s20 DB 层不变量 | DB | ✓ 有效 | 23505/23514/purge 护栏直接 SQL 断言 |
| s21 空 issuer 全拒 | HTTP+DB | ✓ 有效 | ClosedDelegationVerifier 全拒姿态 |
| s22 测试替身回归 | 单元 | ✓ 有效 | 字段扩展不破坏既有装配 |

## 3. R2 受控 PoC（变异式验证，非生产实现）

位置：`poc/r2-verifier-poc/`（独立 workspace crate，主构建不编译；标记非生产）。运行：`cargo test -- --nocapture`。

**动机**：契约 §9 证据表声称 V6/V7/V8 有测试证据，实际不存在（R2-F-12）。PoC 对以下路径独立铸造 token 并断言验证器行为：

| # | 场景 | 期望 | 实测 |
|---|---|---|---|
| P9 | 合法 delegation（对照） | 接受 | **接受** ✓（PoC 链路自证） |
| P1 | `purpose=user_login`（LMMS 用户 Token 形态混淆） | 拒绝 | **拒绝** ✓ |
| P1b | purpose 缺失 | 拒绝 | **拒绝** ✓ |
| P2 | jti 非 UUID | 拒绝 | **拒绝** ✓ |
| P3 | actions 含 `*` | 拒绝 | **拒绝** ✓ |
| P4 | actions 含未知动作 | 拒绝 | **拒绝** ✓ |
| P5 | actions 重复条目 | 拒绝 | **拒绝** ✓ |
| P6 | actions 空数组 | 拒绝 | **拒绝** ✓ |
| P7 | aud 为数组（含期望值） | 拒绝 | **拒绝** ✓ |
| P8 | **audience 配置为 `coder-micro-services`** + aud 同值 token | — | **被接受**——证实契约固定受众仅为默认值（R2-F-06 证据） |

结论：**V4/V6/V7/V8 的代码防御真实存在且行为正确**；缺失的是交付测试矩阵中的用例（R2-F-12），不是防御本身。P8 是已确认的配置弱化。

## 4. 任务书专项核对清单覆盖度

| 任务书要求的证据 | 状态 |
|---|---|
| 并发重放 / 两请求同 JTI / DB 唯一约束 | ✓ s13/s14/s20（异指纹并发缺口见 R2-F-11） |
| 轮换双活 / 未知 kid / JWKS 不可用 | ✓ s05/s05b/s06/s06b |
| **撤销服务不可用** | **✗ 无测试**（撤销未接线；仅端口单测）— RELEASE-GATE G2 |
| 跨租户 / 跨项目 | ✓ s10/s11 |
| action 不匹配 | ✓ s12（V9 层） |
| **artifact_id / operation_id 不匹配** | **✗ 无测试**（V10 未接线）— RELEASE-GATE G1 |
| Token 类型混淆 | 部分：s04 aud 混淆 + PoC P1 purpose 混淆 ✓；AI-PM 侧混淆防护未建设（R2-F-05） |
| 旧 Token 重放 / 过期 | ✓ s15/s17/s07 |
| 错误 audience / 错误 issuer | ✓ s04/s03 |

## 5. 结论

1. 交付测试套件**真实、可复现、无 mock 依赖**，226 通过数独立核实一致；V1–V5/V9 与绑定表的安全证据充分。
2. 证据缺口按块划分：V10（0 个测试）、V11（0 个运行路径测试）、V6/V7/V8（代码在、测试 0）、异指纹并发（0）、读端点动作检查（0——对应 R2-F-03 无任何测试覆盖）、`/v1/generators` 认证（0）。
3. 以上缺口全部进入 [RELEASE-GATE](RELEASE-GATE.md) 对应阻断条件；在补齐前，"23 场景通过"**不得**被解读为"授权链路验证充分"。
