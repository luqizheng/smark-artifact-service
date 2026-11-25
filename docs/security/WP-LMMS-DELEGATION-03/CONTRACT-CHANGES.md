# WP-LMMS-DELEGATION-03 — CONTRACT-CHANGES（契约 v0.2 → v0.3 修订记录）

| | |
|---|---|
| 对象 | `docs/architecture/WP-LMMS-DELEGATION-01-AUTHORIZATION-CONTRACT.md` |
| 性质 | 落实 R2 裁决（O-5 移除 / O-6 修订后保留）与 WP-03 任务书要求的契约对齐；**Q1–Q6 冻结裁决与 §5.1"AS 不加表"原文未改动**（O-5 修复方式是让实现回到裁决，而不是改裁决） |
| 修订后状态 | 仍为 DRAFT（评审通过前不得标 FINAL） |

## 修订清单

| # | 位置 | 修订内容 | 依据 |
|---|---|---|---|
| 1 | 文档头 | 版本 v0.3；修订记录补 WP-03 条目；输入约束补"AS 不加表（O-5 裁决重申）" | WP-03 §15 |
| 2 | §3.1.1 `caller_request_id` 行 | 条件必填 → **写路径必填红线**：actions 含任一写动作（§5.3 清单 11 动作）时必填；格式 = 合法 UUID；缺失 → AS 验证层 401 fail-closed；纯读 token 不要求 | PC-O5-1 |
| 3 | §3.1.1 新增 `actor_type` 行 | 可选，`user\|agent\|service`，缺省 `user`，白名单外拒绝；不改变授权归属（此前为未申报扩展，R2-F-12 关联项） | R2 CONTRACT-CONSISTENCY §5 |
| 4 | §3.1.1 `biz` 行 | 并入 `request_hash`（64 位小写 hex）+ `request_hash_alg`（v1）；与顶层 `caller_request_id` 配对；未知算法 fail-closed | PC-O6-1/2 |
| 5 | §3.1.1 示例 payload | 补 `caller_request_id` 必带标注与 `biz.request_hash`/`request_hash_alg` 配对示例 | PC-O6-1 |
| 6 | §3.1.2 caller_request_id 边界声明 | 扩充为 8 条：红线、格式约束、缺失拒绝、重复请求处理（幂等语义）、并发仲裁（DB 唯一约束，O-5 后由 AS-1/AS-2 承接）、与 Idempotency-Key 双轴关系、跨租户/项目/主体隔离、**不是授权凭证**声明 | WP-03 §5 强制契约红线 |
| 7 | §3.1.3（新增） | request_hash **v1 规范全文**：算法标识与演进规则、四成分（METHOD/PATH/CALLER/CANONICAL_JSON）逐项冻结、query 不参与且 v1 写端点禁 query、重复键签发侧义务、Unicode 不规范化、数值语义、签发-执行一致性、PC-O6-3 交叉向量要求；显式声明"未接线，不得宣称已生效防线" | PC-O6-2、WP-03 §6 |
| 8 | §3.2 V5 行 | 补 leeway 语义：exp/nbf 判定含 60s leeway，单 token 最长实际接受窗 ≈360s，知情设计 | R2-F-15 |
| 9 | §3.2 新增读端点动作映射表 | 全部 GET 端点 → 精确动作（versions→`version:read`；references→`artifact:read`；generators→`generation:read`）；豁免清单仅 health/ready | Q3、R2-F-03 |
| 10 | §8 O-5 行 | "待裁决" → **已裁决并执行**：B（移除），WP-03 删除清单 + PC-O5-1 落地 + 再评估通道保留 | R2 O-5 裁决 |
| 11 | §8 O-6 行 | "待裁决" → **已裁决**：CONDITIONAL KEEP，PC-O6-1/2 已执行、PC-O6-3/4 未完成（阻断项） | R2 O-6 裁决 |
| 12 | §9 | **证据表整体重建**：V6/V7/V8 失实声明修正并以真实测试名补齐（`v6_jti_must_be_uuid`/`v7_purpose_must_be_delegation`/`v8_actions_shape_rejections`）；删除绑定表（s13/s14/s15/s17/s19/s20）行，改为 AS-1/AS-2 幂等证据；新增 caller_request_id 红线、request_hash v1 向量、读端点动作授权、audience 固定、引用一致性五行；V10/V11/V12 保持诚实缺口标注；PoC 明确标记为评审工具而非自动化回归 | R2-F-12、WP-03 §10 |
| 13 | 全文 | V10/O-6 关联笔误修正（原"request_hash …见 O-5"→ 指向 §3.1.3/O-6）；`action.rs` 注释同步 | R2 CONTRACT-CONSISTENCY §5 |

## 未做修订（显式声明）

- **Q1–Q6 冻结裁决**：未改动（diff 可核对）。
- **§5.1 / §5.2 / §5.3**：撤销模型未改动（V11 未接线，语义不变）。
- **未新增"契约 §3.4"**：按任务书"不得为了让引用成立而擅自增加契约条款"——request_hash 规范落在新增的 §3.1.3（§3.1.1 的子节），不是幻影 §3.4 的复活。
- **issuer 值未钉死为常量**：保持 Q6"受信 issuer 清单 + 恰一项 active"的既定机制（LMMS 候选 A 平滑过渡的设计面）；与 audience（契约常量、无演进机制）性质不同。已记录于 [AUDIENCE-SECURITY](AUDIENCE-SECURITY.md) §4。
