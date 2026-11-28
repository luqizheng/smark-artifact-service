# WP-LMMS-DELEGATION-04 — 设计裁决记录（Decision Records）

状态：D1 为**待批准的停止裁决**（本 WP 未实现该子部分）；D2/D3 为已实现的字面解释，留档供独立复评否决。

---

## D1 · operation_id 语法表（除取消路由外）—— 停止，待设计裁决

**触发条件**：任务书停止条件 + WP-04 §5.2"不得发明新 claim；契约无法表达时先出裁决记录"。

**事实**：
1. 契约 §3.1.2 仅规定"operation_id 存在时服务操作必须映射到同一稳定操作"，**未冻结逐路由的稳定操作标识语法**。
2. 契约 §3.1.1 示例 payload 在 `generation:request` token 上给出 `operation_id: "generation_job:9d2c…"`，而 AS 实现（`PgKeyedGenerationJobCreator`）由服务端 `Uuid::new_v4()` 铸造 job id，客户端无法预知——**契约示例与 AS 实现存在真实分歧**。
3. 自行发明 7 条路由的语法表并强制比对 = 超出 v0.3 冻结文本的解释权。

**本 WP 的处理**：仅 `generation:cancel` 落地精确比对 `generation_job:{job_id}`（该语法在契约 §5.3 原文 + §3.1.1 示例中直接给出）；**其余 7 条写路由对 operation_id 保持"存在时不比对也不拒绝"的现状**（`enforce_write_binding` 分支有显式注释，分支不得静默扩大）。

**待批准的提案**（若批准则在后续 WP 接线，均为既有 claim 的比对规则、无新 claim）：

| 路由 | 提议的稳定操作标识 |
|---|---|
| PATCH /v1/artifacts/{id} | `artifact_metadata:{artifact_id}` |
| POST .../current-version | `artifact_current_version:{artifact_id}` |
| POST .../versions | `version_session:{artifact_id}` |
| POST .../versions/{vid}/commit | `version_commit:{artifact_id}:{version_id}` |
| POST .../references | `reference_create:{artifact_id}` |
| DELETE .../references/{rid} | `reference_remove:{artifact_id}:{reference_id}` |
| POST .../generation-jobs | ⚠ 需契约修订：选项 a) 签发方预铸 job id 并作为请求参数（改 AS-2 输入面）；选项 b) 修订契约示例，明确创建型操作不使用 operation_id 绑定 |

**残余风险**（现状）：携带 operation_id 的 token 在这 7 条路由上不获得 operation_id 轴的额外约束；但 caller+hash(v1) 轴已覆盖 method/path/body 全部实例化事实——"同 token 用于不同操作"在指纹层已拒绝，operation_id 轴是纵深防御而非唯一防线。

---

## D2 · 无目标路由上的 artifact_id claim → 字面 fail-closed 拒绝

**事实**：契约 §3.1.2 #1"artifact_id 存在时请求目标 artifact 必须相等"。W1/W8/W9 的请求**没有**目标 artifact（创建型/仅 job_id）——字面应用即"不可能相等 → 拒绝"。

**实现**：这三类路由上携带 `artifact_id` 的 token → 403 `SCOPE_DENIED`（稳定消息）。

**后果（签发侧须知）**：绑定到特定 artifact 的 token 不得授予创建动作（artifact:create / generation:request）或 generation:cancel——语义上也自洽：单 artifact 窄化委托不应用于创建新资源。

**测试**：`wp04 :: b5`（第三段）。

---

## D3 · If-Match 头不在 v1 指纹成分内

**事实**：v1 冻结成分 = METHOD_UPPER/PATH/CALLER/CANONICAL_JSON(body)（§3.1.3），明文禁止原地演进；`If-Match` 是头字段，不在任何已冻结版本内。

**实现**：不改指纹（无停机必要——见残余分析）。语义结果：同 caller+同 body、不同 If-Match 的两次提交共享同一指纹；首次按 CAS 应用，重放按 CAS 收敛（409）或状态收敛。内容层无重复副作用（patch 是字段覆写，值收敛）。

**残余风险**：同一 token 不能表达"对同一 body 的第二次独立 CAS 意图"——该意图需要新签发的 token（新 caller），与契约"新操作 → 新 token"一致。

**测试**：`wp04 :: d2`（第三段：同 token 同 body 重放 → 确定性 409）。

---

## 裁决流程声明

本文件随实现分支提交。D1 的"停止"状态未阻塞其余交付（caller+hash 轴已全覆盖写路由）；D1 若获批/修订，接线属于独立的窄改动（`enforce_write_binding` ③ 分支 + 测试），不触碰其余强制点。
