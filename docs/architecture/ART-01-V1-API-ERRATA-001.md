# ART-01 V1 API Errata 001 — GenerationJob 增量契约

> **状态：** 已确认，纳入 Artifact Service ART-01 V1 正式 API 契约  
> **确认日期：** 2026-11-25  
> **适用基线：** ART-01 V1.0（本仓库）  
> **契约权威：** `smark-artifact-service` 仓库；OpenAPI 3.1 为 API 描述的唯一来源

## 1. 决策

本 Errata 将已实现并经人工确认的 GenerationJob API 增量正式纳入 Artifact Service ART-01 V1。该变更属于向后兼容扩展：仅增加响应字段和明确请求错误，不删除或重命名 ART-01 V1.0 已定义字段，不改变既有资源权威、授权边界或状态机的基本语义。

本文件由 Artifact Service 仓库维护。AI-PM 是 API 消费方，应通过 SDK/API 遵循本仓库发布的契约；AI-PM 不拥有或维护 Artifact Service 的 API 契约。

## 2. GenerationJob 响应字段

GenerationJob 响应增加以下字段：

| 字段 | 含义 |
|---|---|
| `error_code` | 稳定机器可读错误码；无错误时为空 |
| `attempt_count` | 已执行/登记的执行尝试次数 |
| `max_attempts` | 当前 Job 允许的最大尝试次数 |
| `cancel_requested` | 是否已登记取消请求 |

这些字段用于让客户端可靠展示执行失败、重试进度和取消状态。字段的实际可用性与值约束以本仓库的 OpenAPI 3.1 契约为准。

## 3. 创建 GenerationJob 的请求错误

`POST /v1/artifacts/{artifact_id}/generation-jobs` 明确增加以下错误响应：

| HTTP | Error code | 条件 |
|---|---|---|
| 409 Conflict | `IDEMPOTENCY_KEY_REUSED` | 同一作用域内的 Idempotency-Key 被不同请求内容复用 |
| 413 Payload Too Large | `PAYLOAD_TOO_LARGE` | 请求体超过 256 KiB 上限 |

请求 envelope 固定要求：

- `schema = "smark.seecode.input"`
- `schema_version = 1`

不符合 envelope、包含未知顶层字段、超过结构上限或包含被禁止的危险字段/URL 形态值时，返回 400。diagram 内部完整 JSON Schema 仍由 Generator 输入契约单独冻结；本 Errata 不宣称其已全部冻结。

## 4. 取消语义

`POST /v1/generation-jobs/{job_id}/cancel` 的行为明确如下：

- Job 为 `queued` 或 `retry_wait`：直接进入取消终态。
- Job 为 `running`：登记取消请求；响应仍可显示 `status = running`，同时 `cancel_requested = true`。Worker 观察到取消请求后完成协作式收尾。
- Job 已处于终态：不改变既有终态。

因此，`cancel_requested` 不等价于 `status = cancelled`；客户端必须分别展示“已请求取消”和“已取消”。

## 5. 兼容性与实施约束

1. 新增的 GenerationJob 响应字段属于向后兼容扩展；客户端应忽略其不认识的可选扩展字段。
2. 409 与 413 纳入统一错误 envelope，错误码保持稳定。
3. 本仓库 OpenAPI 3.1 是 API 描述的唯一来源；Rust DTO、SDK 和契约测试必须与其保持一致。
4. 本 Errata 只确认 API 契约，不代表 SeeCode Generator Adapter 已实现或生成 API 已启用。
5. 当前 Artifact Service 在 Adapter 不可用时，GenerationJob 创建仍必须 fail-closed，返回 `503 GENERATOR_UNAVAILABLE`；不得创建虚假成功 Job。
6. 生产生成路径仍受 ART-02 两个 P0 门禁约束：
   - LMMS resource-token 授权委托端点的认证挂载与 claims（至少 audience / tenant scope）问题关闭前，不得启用生产 delegation。
   - File Service upload-intent / checksum / finalize / object verification 协议完成前，不得提交 READY Version。
7. 本 Errata 不授权绕过上述门禁，不改变 Artifact Service 不读取 AI-PM 业务数据库、不复制 AI-PM 项目授权逻辑的边界。

## 6. 验收依据

本 Errata 的实现核对点：

- OpenAPI 声明四个 GenerationJob 响应字段。
- Rust DTO 与 OpenAPI 字段一致。
- 409 `IDEMPOTENCY_KEY_REUSED` 和 413 `PAYLOAD_TOO_LARGE` 有契约定义及测试。
- GenerationJob 创建在 Adapter 不可用时返回 503 `GENERATOR_UNAVAILABLE`。
- Cancel API 区分取消请求与取消终态。
- 请求 envelope 的 schema/version 与 400 校验规则有契约测试。

实现状态以本仓库对应 commit、CI 和实施报告为证；本文件本身是架构契约，不代替代码验收。

## 7. 生效关系

本 Errata 是 Artifact Service ART-01 V1.0 的补充文件，与 ART-01 V1.0 一并构成当前有效的 V1 API 契约。若本文与 ART-01 V1.0 对 GenerationJob 响应字段或上述错误语义存在不一致，以本 Errata 对应条款为准；其他主题仍以 ART-01 V1.0 为准。
