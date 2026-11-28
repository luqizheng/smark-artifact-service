# WP-LMMS-DELEGATION-04 — 设计裁决记录（Decision Records）

状态：D1 为**待批准的停止裁决**（本 WP 未实现该子部分）；D2/D3 为已实现的字面解释，留档供独立复评否决。

---

## D1 · operation_id 语法表（除取消路由外）—— 停止，待设计裁决

> **R1-R1 文档修正（纯文档，零代码改动）**：本节及全仓原文的"其余 7 条/七条"计数有误——9 条写路由中仅 W9（cancel）落地比对，其余保持"存在时不比对也不拒绝"的写路由实为 **W1–W8 共 8 条**（原文按下方提案表 W2–W8 共 7 行计数，遗漏了同属未比对集合的 W1 create_artifact）。提案表保持原样（仍不含 W1；W1 与 W8 的 operation_id 语法同属待契约裁决项，本文档不发明语法）。**D1 当前仍待裁决：W1–W8 共 8 条写路由尚未完成 operation_id 语义绑定；W9 已按当前冻结契约语法执行对应比对。此处仅描述当前实现状态，不构成对 W1–W8 operation_id 语法的冻结或认可。**下文正文的计数已同步更正为 8，修订仅涉文字，实现行为与测试证据无变化。

**触发条件**：任务书停止条件 + WP-04 §5.2"不得发明新 claim；契约无法表达时先出裁决记录"。

**事实**：
1. 契约 §3.1.2 仅规定"operation_id 存在时服务操作必须映射到同一稳定操作"，**未冻结逐路由的稳定操作标识语法**。
2. 契约 §3.1.1 示例 payload 在 `generation:request` token 上给出 `operation_id: "generation_job:9d2c…"`，而 AS 实现（`PgKeyedGenerationJobCreator`）由服务端 `Uuid::new_v4()` 铸造 job id，客户端无法预知——**契约示例与 AS 实现存在真实分歧**。
3. 自行发明其余 8 条写路由（W1–W8）的语法表并强制比对 = 超出 v0.3 冻结文本的解释权。

**本 WP 的处理**：仅 `generation:cancel` 落地精确比对 `generation_job:{job_id}`（该语法在契约 §5.3 原文 + §3.1.1 示例中直接给出）；**其余 8 条写路由（W1–W8）对 operation_id 保持"存在时不比对也不拒绝"的现状**（`enforce_write_binding` 分支有显式注释，分支不得静默扩大）。

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

**残余风险**（现状）：携带 operation_id 的 token 在这 8 条路由（W1–W8）上不获得 operation_id 轴的额外约束；但 caller+hash(v1) 轴已覆盖 method/path/body 全部实例化事实——"同 token 用于不同操作"在指纹层已拒绝，operation_id 轴是纵深防御而非唯一防线。

---

## D2 · 真正无目标路由（W1/W9）上的 artifact_id claim → 字面 fail-closed 拒绝
### （WP-04-R1 修订：W8 从本裁决中移出）

> **R1 修订原因**：WP-04 原文把 W8（`POST /v1/artifacts/{artifact_id}/generation-jobs`）误列为无目标路由。实际 handler 向绑定事实传入 `Some(路径 artifact_id)`，W8 是**目标寻址路由**，其 `artifact_id` claim 与路径 artifact ID **精确比对**（不一致 → 403；一致 → 通过目标绑定检查；`wp04 :: g1/g2`）。行为自 WP-04 起一直如此（handler 即传 `Some`），本修订只纠正文档/注释与事实的不一致，**未放宽任何 fail-closed 行为**。

**事实**：契约 §3.1.2 #1"artifact_id 存在时请求目标 artifact 必须相等"。真正没有目标 artifact 的路由只有两个：W1（artifact 创建，目标尚不存在）与 W9（取消，路径仅含 job_id）——字面应用即"无目标即不可能相等 → 拒绝"。

**实现**：W1/W9 上携带 `artifact_id` 的 token → 403 `SCOPE_DENIED`（稳定消息）。W2–W8：claim 存在时与请求目标 artifact 精确比对。

**后果（签发侧须知）**：绑定到特定 artifact 的 token 不得用于 W1/W9；用于 W2–W8 时必须与路径目标一致。

**测试**：`wp04 :: b5`（W1 无目标拒绝 + W8 匹配 200 / 失配 403）、`wp04 :: g1/g2`（W8 目标绑定专项）。

---

## D4 · W8 上 artifact_id claim 缺省是否应拒绝 —— 决策项（待契约澄清，未实现）

> **WP-04-R1 新增**。复评修复要求"W8 缺少必需目标 claim：按现有安全契约拒绝"；经契约原文核对，该要求与冻结文本存在冲突，按 §12 停止并记录，**未擅自实现**。

**契约证据**：
1. §3.1.2 #1 冻结规则："artifact_id **存在时**请求目标 artifact 必须相等"——只规定存在时的比对，未规定任何路由的必填性。
2. §3.1.1 示例 payload（冻结文档）：`generation:request` token 携带 `"artifact_id": null`——**缺省形态在契约自己的示例中合法**。
3. §3.3："操作绑定字段按意图填入"——必填性属签发侧意图映射，未在 AS 侧冻结。

**实现**：维持现状——W8 上 artifact_id 缺省 → artifact_id 轴不拒绝（无 claim 即无比对对象），caller+hash(v1) 轴仍完整绑定该请求（method/path/目标/body 全部进指纹），到达 generator 门 503（`wp04 :: g3`）。

**若未来契约澄清"目标寻址路由必填 artifact_id"**：接线是 `enforce_write_binding` 内的单分支（W2–W8 claim 缺省 → 403）+ 测试，不触碰其余强制点；同时必须修订契约 §3.1.1 示例。

**残余风险**：未绑定 artifact 的 W8 token 依赖 caller+hash 轴提供目标绑定（指纹覆盖路径 artifact ID，跨目标复用仍被拒）；artifact_id 轴是纵深防御而非唯一防线，与 D1 残余风险同级。

---

## D3 · If-Match 头不在 v1 指纹成分内

**事实**：v1 冻结成分 = METHOD_UPPER/PATH/CALLER/CANONICAL_JSON(body)（§3.1.3），明文禁止原地演进；`If-Match` 是头字段，不在任何已冻结版本内。

**实现**：不改指纹（无停机必要——见残余分析）。语义结果：同 caller+同 body、不同 If-Match 的两次提交共享同一指纹；首次按 CAS 应用，重放按 CAS 收敛（409）或状态收敛。内容层无重复副作用（patch 是字段覆写，值收敛）。

**残余风险**：同一 token 不能表达"对同一 body 的第二次独立 CAS 意图"——该意图需要新签发的 token（新 caller），与契约"新操作 → 新 token"一致。

**测试**：`wp04 :: d2`（第三段：同 token 同 body 重放 → 确定性 409）。

---

## 裁决流程声明

本文件随实现分支提交。D1 的"停止"状态未阻塞其余交付（caller+hash 轴已全覆盖写路由）；D1 若获批/修订，接线属于独立的窄改动（`enforce_write_binding` ③ 分支 + 测试），不触碰其余强制点。
