# WP-LMMS-DELEGATION-04 — 强制点设计说明（enforcement placement & transaction guarantees）

## 1. 强制点（任务书 §5.1）

```
HTTP adapter（apps/artifact-api）
  ├─ 采集事实：Method + 实例化 Uri.path() + 原始 body Value（ValidJson.raw / ValidGenerationJson.raw）
  ├─ 组装 WriteOperationBinding（operation_binding.rs）——只采集，不做策略判定
  ▼
ArtifactService 写方法（crates/artifact-application/src/service/artifact.rs，9 个写入口）
  ├─ ① Delegation::require(action, scope)          —— 403（既有）
  ├─ ② enforce_write_binding(delegation, binding)  —— 403（WP-04 新增）
  │     caller/request_hash/artifact_id/operation_id 四轴 + v1 指纹重算比对
  ├─ ③ 输入校验 / 读库校验
  └─ ④ 副作用（DB 变更 / 端口调用）
```

**为什么在应用服务层**：`ArtifactService` 是全部写路径的唯一编排入口（HTTP adapter 与任何未来 adapter 共用）；在服务入口接线保证无旁路。HTTP 层仅采集事实数据（指纹从不取自任何调用方可控的头字段，`X-Request-Hash` 之类不存在）。

**原始 body Value 而非 typed DTO 重序列化**：v1 指纹对签发方所见的请求 JSON 计算；`ValidJson` 先按 `serde_json::Value` 保形解析再收窄 DTO，未知字段参与指纹（否则未知字段会被 DTO 静默丢弃造成假拒绝/假放行）。

**实例化 path**：`Uri::path()` 按字节原样（不做百分号解码/大小写归一）——契约 §3.1.3 冻结语义；UUID 路径参数先经 `parse_path_uuid`（非法即 400）。

## 2. 绑定轴语义（契约 v0.3 冻结可表达部分）

| 轴 | 规则 | 位置 |
|---|---|---|
| caller | 请求侧 caller 优先（W8 body 字段），其余路由取 claim；写 token 无 claim → 403（服务层防线独立于验证器 401 红线，防验证器替身/降级） | `enforce_write_binding` ① |
| request_hash | `canonical_request_fingerprint`（v1 唯一权威实现）重算 vs `biz.request_hash` claim；配对/形状/算法校验仍在验证器（WP-03 PC-O6-1/2 未动） | `enforce_write_binding` ④ + `assert_delegation_matches_request` |
| artifact_id | claim 存在 → 请求目标相等；无目标路由出现 → 403（D2 字面 fail-closed） | `enforce_write_binding` ② |
| operation_id | 仅 W9：`generation_job:{job_id}`（§5.3 语法）；其余路由不比对不拒绝（D1 停止条件） | `enforce_write_binding` ③ |

失败一律 `DomainError::ScopeDenied` → 403 `SCOPE_DENIED` 稳定信封；消息只含判定原因，不含 token 本体/指纹值/请求内容。401/403 拒绝审计由既有中间件记录。

## 3. 事务保证（任务书 §6.2 #6/#7）

| 操作 | 幂等决定与副作用的原子性 |
|---|---|
| W1 keyed | 单 PostgreSQL 事务（AS-1，`PgKeyedArtifactCreator`，未改动） |
| W1 unkeyed+key | **单事务**（WP-04 新增 `PgUnkeyedArtifactCreator`，复用 `idempotency_record` 表，无新表/迁移）：预留→插入→完成→一次 COMMIT；业务失败整体回滚 → 无幂等痕迹；重放前验证 resource_id→Artifact 存在且 scope 一致（失败关闭） |
| W2/W5/W6/W7/W9 | 非操作键幂等分类（CAS/自然键/状态机），各自机制原子性：W6 部分唯一索引约束、W9 单受控函数 |
| 外部副作用 | 无队列/对象存储/外部调用（StoragePort 恒 ProtocolBlocked；无 outbox 发布者）→ §6.2 #7 当前无适用面，如实声明而非宣称覆盖 |

**G2-1/G2-2 修复**（旧行为的两个缺口）：旧 unkeyed+key 路径 reserve→insert→complete 跨 3 事务（崩溃窗口：已提交 Artifact + 永久 pending 行）且错误快照重放 500。新端口消除两者：崩溃窗口不再产生（同事务）、legacy 错误快照重放确定性 409（`d6/d7` 测试）。

## 4. 测试替身说明

`TestDelegationVerifier`（`test-doubles` feature 专属）扩展 `allow_with_binding` + 内部互斥（支持 spawn 后按具体请求登记绑定 token）；发布二进制不含该模块，生产默认装配 `ClosedDelegationVerifier`（未改动）。`wp04 :: b7` 证明：即使验证器替身放行未绑定 token，服务层仍然 403——服务层不信任验证器之外的任何保证。

## 5. 未做事项（显式）

- V11 撤销闭环、AI-PM 签发/撤销/JWKS、生产写路径开放：**不在本 WP**，保持阻断（WP-03 REMAINING-BLOCKERS 未变）。
- AS-2 keyed job 端口的 HTTP 接线：维持 fail-closed（`as2_41`），属 ART-02 P0 之后的独立工作。
- 非 W9 路由的 operation_id 语法：停止条件触发，见 `DECISION-RECORDS.md` D1。
