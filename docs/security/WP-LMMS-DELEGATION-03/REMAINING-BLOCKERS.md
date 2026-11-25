# WP-LMMS-DELEGATION-03 — REMAINING-BLOCKERS（未完成的后续阻断项）

| | |
|---|---|
| 性质 | 任务书 §11/§14 要求显式保留的缺口清单。**本 WP 未将其伪装成已完成，也未引入任何默认允许的 mock/空实现。** |
| 门槛 | 在下列条目关闭并经复评前：不合并 PR #12/#11、不开放生产写路由、不改变生产默认拒绝策略、不在共享环境配置 `ARTIFACT_API_DELEGATION_ISSUERS` |

## 1. AS 侧（V10 / V11 —— 均为零接线状态）

### V10：操作绑定目标一致性（R2-F-01）⛔ 未闭环
- 现状：`assert_delegation_matches_request` 存在且接口已复核（caller + hash 两轴，本 WP 修正文档并冻结 v1 指纹规范），但**零生产调用点**；`artifact_id`/`operation_id` 与请求目标的比对未实现。
- 影响：契约 §3.1.2 场景②（同一 token 用于不同操作 → 403）当前不成立；写路径残余防护 = scope + actions + TTL + 各端点幂等键。
- 关闭条件（RELEASE-GATE G1）：全部写服务入口调用一致性判定；每写路由 ≥1 条"绑定不一致 → 403"真实 HTTP 测试（三绑定字段独立变异）；request_hash 按 v1 规范校验。
- 本 WP 已铺垫：v1 规范 + 冻结向量 + 形状校验 + caller_request_id 写路径红线（签发侧前置）。

### V11：撤销闭环（R2-F-02）⛔ 未闭环
- 现状：`DelegationRevocationChecker` 端口 + `FailClosedRevocationChecker`（默认拒绝）存在，未进入 AppState、无任何请求路径消费；503 `REVOCATION_UNAVAILABLE` 映射存在但运行路径不存在。**未引入任何"查询失败视为未撤销"或默认允许的替身。**
- 影响：撤销能力整体缺失；写路径残余窗口 = TTL 全窗（≤300s），违反 Q1"残余 ≈ 0"。
- 关闭条件（G2）：AI-PM 撤销存储 + `GET /internal/delegation-revocations/{jti}`；AS checker 客户端接线（V1–V10 后、写入前，超时 2s，一切异常 → 503）；撤销后写 403 等四类真实网络测试。
- 契约 §5.3 的"AI-PM 停机 = AS 写平面 503"fail-closed 语义**尚未生效**（既无查询点）。

### 其他 AS 侧建议项（R2 G5，非阻断）
- JWKS 最小模长 ≥2048（R2-F-07）；JwksCache lookup 刷新移出锁外（R2-F-08）；审计事件补 jti/subject/原因并明确审计失败语义（R2-F-09，当前 fail-open 审计 + 401/403 事件缺 jti）。
- SPKI pin（R2-F-10，可选增强）。

## 2. AI-PM 侧（R2-F-05：授权闭环零实现）

| 项 | 状态 |
|---|---|
| §3.0 七项验证链（含 F-25 aud 不校验修复） | ⛔ 零实现（阶段 0） |
| 专用 delegation 身份验证 + 签发链（意图接口、专用密钥、`/.well-known/jwks.json`） | ⛔ 零实现（阶段 1） |
| `pm_delegation` / `pm_delegation_revocation` 存储 + 撤销端点 | ⛔ 不存在（V11 前置） |
| request_hash 同算法实现 + 交叉向量（PC-O6-3） | ⛔ 零实现（本 WP 已在 AS 侧备好向量基准） |
| 内部服务身份认证（撤销查询端点的认证） | ⛔ 待设计落地（O-2） |
| F-22 凭据轮换完成证据 | ⛔ 仓库外动作，尚无声明（R2-E-01） |
| ai-pm PR #11 CI workflow | ⛔ 该 PR 无任何 check |

## 3. 端到端（R2-E-03）

- 签发 → 验证 → 撤销 → 写入 → 幂等重放的跨服务 E2E：⛔ 不可能存在（AI-PM 零实现）；G7 联调 + 人工验收 + 人工批准仍未启动。

## 4. 阶段推进判据（沿 R2 RELEASE-GATE，本 WP 关闭项已标注）

| 从 → 到 | 条件 |
|---|---|
| 现在 → 阶段 0（AI-PM 验证链） | ✅ 已具备（R2 CONDITIONAL PASS + WP-03 契约修订定稿） |
| 阶段 0 → 阶段 1 | G4 契约修订（✅ WP-03 完成，待复评确认）+ G6 阶段 0 项 + R2-E-01 |
| 阶段 1 → 联调 | G1–G5 全部关闭（G3/G4 的 AS 侧已由本 WP 关闭；G1/G2/G6/G5-建议项未关闭）+ 复评 |
| 联调 → 受控写窗口 | G7 + 人工验收 |
| 受控写窗口 → 生产 | 凭据轮换 + 生产配置核查 + 人工批准 |

## 5. 生产姿态确认

- 生产默认拒绝策略**未改变**：空 `ARTIFACT_API_DELEGATION_ISSUERS` → `ClosedDelegationVerifier` 全拒（s21 持续覆盖）。
- 未执行任何共享环境迁移；未配置任何受信 issuer；未开放任何生产写路由。
- **WP-03 是 AS 契约与实现修复，不是授权闭环完成，也不是生产开放批准。**
