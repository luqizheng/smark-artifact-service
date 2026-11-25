# WP-LMMS-DELEGATION-02 — RELEASE-GATE（阻断条件清单）

| | |
|---|---|
| 状态 | R2 独立安全评审交付（2026-11-25） |
| 最终裁决 | **CONDITIONAL PASS**——具备进入下一阶段实施与修复的条件；**不具备**联调启用/受控写路径开放的条件，直至下列阻断条件全部关闭 |
| 效力说明 | 即使全部条件关闭并复评通过，**也不自动授权生产部署**：仍需凭据轮换证据、AI-PM 阶段 0、V10/V11 闭环、受控联调、真实窗口人工验收、人工批准生产开放（任务书 §11） |

## 阻断条件（逐项，全部为必过）

### G1 — V10 目标一致性接线（R2-F-01）
- [ ] 全部 9 个写 handler/服务入口调用 `assert_delegation_matches_request`（或等价判定），绑定 claim 与请求目标逐一比对（tenant/project 已由 require 覆盖，此处为 artifact_id/operation_id/caller_request_id/request_hash）
- [ ] 每个写路由各≥1 条"绑定与目标不一致 → 403"真实 HTTP 集成测试（含三个绑定字段独立变异）
- [ ] request_hash 按 PC-O6-2 版本化规范校验

### G2 — V11 撤销闭环（R2-F-02）
- [ ] AI-PM：`pm_delegation`/`pm_delegation_revocation` 迁移 + `GET /internal/delegation-revocations/{jti}`（service 身份认证）+ 移出项目/禁用/取消事务内批量撤销
- [ ] AS：checker 客户端 + 接线（写路径、V1–V10 后、写入前；超时 2s；一切异常 → 503 `REVOCATION_UNAVAILABLE`）
- [ ] 测试：撤销后写 403；不可达/超时/非 200/畸形响应四类真实网络场景全部 503；并发撤销-写入竞态无部分写入；无进程级撤销缓存

### G3 — 授权面补全（R2-F-03）
- [ ] `get_version`/`list_versions`/`list_references` 补 `require(VersionRead/ArtifactRead)`
- [ ] `/v1/generators` 挂认证（推荐 `Authed`+`generation:read`）或契约明示豁免理由
- [ ] 全路由审计测试（枚举 router 断言业务路由均经动作检查）防回归

### G4 — 契约修订 + 引用修正（R2-F-04、O-5、O-6、R2-F-12）
- [ ] O-5 按 [O5-O6-DECISIONS](O5-O6-DECISIONS.md) 处置：移除迁移 0006 与绑定存储（或完成完整 §3.4 修订——R2 推荐前者）
- [ ] O-6 按 PC-O6-1..4 并入 §3.1.1 并版本化
- [ ] `actor_type` claim 入表或移除；修正 §9 证据表（删除不存在的证据声明、补 V6/V7/V8 真实用例）；修正 O-5/O-6 交叉笔误；全文清理幻影"§3.4"引用

### G5 — 配置与加固（R2-F-06/07/08/09）
- [ ] audience 钉死（删除配置项或校验恒等于 `artifact-service`，负面配置启动失败测试）
- [ ] V6/V7/V8 验证器负面测试进交付矩阵（PoC P1–P7 移植）
- [ ] （建议非阻断）JWKS 最小模长 2048；lookup 刷新移出锁外；审计事件补 jti/subject/原因并明确审计失败语义

### G6 — AI-PM 侧建设（R2-F-05、R2-E-01/02）
- [ ] **阶段 0**：§3.0 七项验证链实现 + 逐项测试（aud 强制 `coder-micro-services`、purpose、ct、roles、principal、仅人类、aud 类型语义）；F-25 修复；F-22 凭据轮换完成证据（仓库外声明）
- [ ] **阶段 1**：意图接口（动作映射服务端写死）、签发器（专用密钥独立注入）、`/.well-known/jwks.json`、撤销存储+端点、artifact-service 客户端（caller_request_id 注入、401/403/409/503 透传）
- [ ] AI-PM PR #11 补充 CI workflow（当前该 PR 无任何 check）

### G7 — 端到端联调与人工验收（R2-E-03）
- [ ] dev compose 受控联调：登录 → 意图 → 签发 → AS 全链验证 → 写入 → 幂等重放 → 撤销 → 再写被拒
- [ ] 跨租户/跨项目/action/绑定/过期/撤销不可用六类 E2E 负面用例
- [ ] 真实窗口人工验收 + 人工批准（生产写路由开放的前置）

## 阶段推进判据（R2 建议）

| 从 → 到 | 条件 |
|---|---|
| 现在 → 阶段 0 启动 | 本评审交付即可（CONDITIONAL PASS 允许阶段 0 准备） |
| 阶段 0 → 阶段 1 | G4（契约修订定稿）+ G6 阶段 0 项 + R2-E-01 |
| 阶段 1 → 联调 | G1–G5 全部关闭 + R2 复评（或双方联合评审）确认 |
| 联调 → 受控写窗口 | G7 联调项 + 人工验收 |
| 受控写窗口 → 生产 | 凭据轮换、生产配置核查（audience 钉死、https、单 issuer）、人工批准 |

## 明确不为（R2 后仍禁止，直到对应条件关闭）

1. 不合并 PR #12/#11（G4/G6 关闭并复评前）。
2. 不执行迁移 0006 于任何共享环境（O-5 裁决为移除；若裁决被推翻须先修订契约）。
3. 不配置 `ARTIFACT_API_DELEGATION_ISSUERS` 于任何共享/生产环境（G1–G6 关闭前，空清单全拒是唯一安全姿态）。
4. 不以"测试通过"推断系统安全（本评审已证：226 通过 ≠ 闭环成立）。
