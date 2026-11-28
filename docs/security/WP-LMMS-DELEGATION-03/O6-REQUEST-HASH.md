# WP-LMMS-DELEGATION-03 — O6-REQUEST-HASH（O-6 request_hash 修订后保留：规范与状态）

| | |
|---|---|
| 裁决 | R2 O-6 = **修订后保留（CONDITIONAL KEEP）**，条件 PC-O6-1..4 全部满足才可接线；接线前 request_hash 校验不得作为任何唯一防线 |
| 本 WP 范围 | 仅规范与基础实现对齐（PC-O6-1/2）；**未接线**（PC-O6-4 属 AS-4/V10，PC-O6-3 属 AI-PM 签发侧） |

## 1. PC-O6 状态

| 条件 | 内容 | 状态 |
|---|---|---|
| PC-O6-1 | `biz.request_hash` + 顶层 `caller_request_id` 并入契约 §3.1.1（写路径必填红线），明确读路径不要求 | ✅ 契约 v0.3（caller_request_id 红线 + biz 行修订 + §3.1.3 新增） |
| PC-O6-2 | 算法/版本标识 + path/query/body 规范冻结 | ✅ `biz.request_hash_alg`（缺省=v1，未知值 fail-closed 拒绝）+ v1 规范全文冻结（契约 §3.1.3 + `delegation_request.rs` 模块文档，二者同文） |
| PC-O6-3 | AI-PM 签发侧实现同算法 + 双方交叉向量 | ⛔ AI-PM 侧零实现（阻断项；AS 侧已提供冻结向量基准） |
| PC-O6-4 | `assert_delegation_matches_request` 进入全部写路径 | ⛔ 未接线（零生产调用点，V10 同族接线任务） |

## 2. v1 规范要点（全文见契约 §3.1.3；此处为摘要）

```text
request_hash(v1) = lower_hex(sha256( METHOD_UPPER "\n" PATH "\n" CALLER "\n" CANONICAL_JSON(body) ))
```

覆盖任务书 §6 全部要求项的落点：

| 任务书要求 | v1 落点 |
|---|---|
| 版本标识 | `biz.request_hash_alg`；缺省 v1；未知算法 AS 拒绝（fail-closed）；禁止原地改 v1 语义（升版 v2） |
| 哈希算法 | SHA-256，64 位小写十六进制（大写/长度错在 claims 形状校验拒绝） |
| method | ASCII 大写参与指纹（跨 method 混淆不可能） |
| path 规范化 | 实例化形态（UUID 小写连字符代入）、UTF-8 字节原样、无百分号重编码、无 query/fragment；编码差异 = 不同指纹（fail-closed） |
| query 规范化 | **不参与，且 v1 写端点禁带 query**；未来带 query 写端点必须升版 v2（契约红线） |
| 特定请求头 | 不含（授权不依赖头，Q2；定位不一致由 require/Scoped 拦截——R2 O-6 依据第 2 点维持） |
| body 规范化 | serde_json Value 紧凑序列化：键按 UTF-8 字节序、无空白、整数十进制、浮点 ryu（签发方必须拒签浮点 body）、字符串 UTF-8 原样 |
| JSON 键序/空字段/数字/Unicode | 键序无关（BTreeMap）；`null`/缺失/空串三值可区分；1 / 1.0 / "1" 三值可区分；无 NFC/NFD 归一化（字节层 fail-closed） |
| 不可忽略字段 | path/query 红线如上；重复键 = 签发方必须拒签（AS 解析后 last-wins 不可检测，签发侧义务） |
| 跨版本兼容 | 新算法必须显式 `request_hash_alg` 声明且 AS 验证器先行扩展；旧 token 的缺省=v1 语义永不改变 |
| body 换而 hash 未更新 / 目标不一致 | 属 V10 接线的运行期比对（重算 vs claim），未接线前不宣称防护存在 |
| 与 AI-PM 签发端一致性 | PC-O6-3：同算法 + 冻结向量交叉验证（同输入 → 同 64hex） |

## 3. 冻结向量（AS 仓库权威基准）

位置：`crates/artifact-application/src/service/delegation_request.rs` `#[cfg(test)]`（AI-PM 侧按同一输入实现并断言同值）：

| 向量 | 输入要点 | 期望（64 hex） |
|---|---|---|
| V1 | `POST /v1/artifacts` + caller `0b2e5f1a-9c4d-4e8a-b7f6-2a1d3c5e7f90` + body `{"artifact_type":"visualization","name":"wp03-vector"}`（键序与方法大小写变体同值） | `64cc551d71fd091999486c83377cc492b2e38aa5946d23c93d771f49de5f4d8f` |
| V2 | `POST /v1/generation-jobs` + 无 caller + 空 body（→ `null`） | `645127e060bbdab9760aafb82c012f212a1da5d582f6f8a9912e8cdd539ec266` |
| V3 | `PATCH /v1/artifacts/5c1e0000-0000-4000-8000-000000000001` + caller + body `{"标题":"中文✓"}`（Unicode 字节原样） | `51dae3324e1caaef6466b5f782c2d8a079e871917e7237197bb893f991975019` |
| 语义组 | 1 / 1.0 / "1" 两两不同；`{"n":null}` ≠ `{}`；method/path 变化必变指纹 | `assert_ne` 断言组 |

## 4. AS 侧基础实现（本 WP 允许范围）

- `canonical_request_fingerprint`：v1 唯一权威实现（注释声明"语义变更必须升版"）。
- `assert_delegation_matches_request`：接口复核——caller + hash 两轴已覆盖"本次请求绑定"判定；`artifact_id`/`operation_id` 目标比对属 AS-4 同族接线（文档已注明）。**仍零生产调用点**。
- 验证器形状校验：hash 格式（64 小写 hex）、配对（hash↔caller、alg↔hash 同空同非空）、算法白名单（仅 v1）。单测：`ai_pm :: biz_pairing_rules`（含未知算法/半配对拒绝）。
- **未做**：任何路由/服务层的重算比对（V10）；未宣称目标绑定生效。

## 5. 残余风险（如实声明）

1. PC-O6-3 未完成前，签发端一致性无验证手段（AI-PM 不存在）。
2. v1 规范中"签发方拒签浮点/重复键 body"不可被 AS 检测（解析后信息丢失）——签发侧义务，契约明示。
3. 接线前，request_hash 对写路径无任何运行期防护效力（此为裁决设计的中间态，非缺陷；当前生产空 issuer 全拒）。
