# WP-AS-05 实施报告 — Version 生命周期链路（file-service 存储适配器）

> 状态：完成（本地全量测试绿；CI 随 PR）
> 分支：`feat/wp-as-05-version-lifecycle`（基于 WP-04 PR #15 head `d6258e9`）
> 关联契约：WP-LMMS-DELEGATION-01 v0.3 §12（"上传+校验+补偿"裁决）、ART-01 §6、
> ART-02 P0 存储协议、ART-03A §5.2/§8、WP-04（V10 基线）

## 1. 目标与结果

解除 Version 链路的有意阻断（`create_version_session`/`commit_version_session`
恒 503），落地契约 v0.3 §12 裁决的近似协议：

| 步骤 | 实现 |
|---|---|
| 调用方上传 | 调用方以自身身份上传对象至 file-service（AS 不代理字节；`create_upload_intent` 因 LMMS 无 upload-intent 协议（P1）保持明确不可用） |
| 会话创建 | `source_object_id`+`source_sha256` 必填 → AS head 对象（fail-early 404/大小校验）→ 插入 pending Version（触发器分配 version_number）→ 201 + Location |
| 提交校验 | AS 以带 tid 短时令牌下载对象、重算 sha256（file-service 无 hash attestation）→ 与声明比对（不符 400） |
| 单事务提交 | `artifact_output`（primary，safety pending）+ Version `pending→ready`（manifest 写校验事实；DB 触发器背书终态） |
| set_current | AS-1 既有 READY-only CAS 路径打通 |

## 2. 交付物

### 2.1 新增
- `crates/artifact-infrastructure/src/file_service/service_jwt.rs`：
  `ServiceJwtMinter`（最小移植 ai-pm `digua_common_jwt::ServiceJwtCache` 的
  mint 流程，**不引入 LMMS 私有仓库依赖**）——`POST /tokens/service-jwt`
  （Bearer bootstrap，body `{aud:"file-service", tid}`），per-(aud,tid) 缓存、
  到期前 60s 重 mint、401/403 → `AuthFailed`（不重试）、5xx/网络 → `Unavailable`
  （fail-closed）。**F-24 防线**：恒定携带 tid mint，绝不发出无 tid 令牌
  （file-service `unwrap_or(1)` 回落陷阱，`services/file.rs`）。
- `crates/artifact-infrastructure/src/file_service/storage.rs`：
  `FileServiceStorage` 实现 `StoragePort` —— multipart `POST /file/upload`
  （`is_temporary=false`，"上传即正式"；`user_id`/`meta.tenant_id` 恒 None）；
  `head_object`（info 元数据，sha256=None）；`inspect_object`（下载+sha256）；
  `create_download_intent`（`POST /file/presign` get/600s）；`delete_object`
  （404 幂等视为已删除）；`copy_or_finalize_object`（no-op，P1 差异记录）。
- `tests/tests/version_lifecycle.rs`（10 场景）：真实适配器 ↔ loopback fake
  LMMS（token mint + multipart/info/download/delete/presign）—— 全链
  pending→commit→READY→set_current；sha 不符 400（Version 保持 pending、
  调用方对象不被删）；对象缺失 404；缺字段/非法 sha 400；重复 commit 409；
  file-service 断供 503（retryable）；委托缺 lmms_tid 503（对象面路由
  fail-closed）；ready 面如实上报；V10 绑定在版本写路径仍强制（body 变异 403）；
  补偿删除幂等。

### 2.2 端口与服务层修订
- `StoragePort`（`port/storage.rs`）：对象面操作统一携带 `lmms_tid`（仅出站
  令牌 mint 路由 + file-service 自身租户 scope，**绝不参与 AS 授权判定/scope
  装配**——契约 §3.1 lmms_tid 红线）；新增 `upload_object`（Worker 路径，
  WP-AS-06 使用）；`ObjectInspection.sha256` → `Option<String>`（head 无 hash）。
- `VersionRepository`：新增 `insert`（pending，触发器分配号）与 `commit_ready`
  （单事务 output 插入 + `pending→ready`；0 行更新 → 409）。
- `ArtifactService::create_version_session`/`commit_version_session` 重写：
  移除双层"拒绝伪成功"守卫；V10 `enforce_write_binding` 保持前置于一切副作用。
- `Delegation::lmms_tid_i64()`（domain）：追溯 claim → 数字 tid（缺失/非法 →
  None → 调用方 fail-closed）。

### 2.3 装配与诚实面
- 配置（api）：`ARTIFACT_FILE_SERVICE_URL` / `ARTIFACT_FILE_TOKEN_SERVICE_URL` /
  `ARTIFACT_FILE_BOOTSTRAP_TOKEN`（三项 all-or-nothing，部分配置 = 启动硬错）+
  `ARTIFACT_FILE_UPLOAD_MAX_BYTES`（默认 64 MiB）/ `ARTIFACT_FILE_REQUEST_TIMEOUT_SECS`。
  三者全缺 → `ProtocolBlockedStorage`（503 诚实阻断，姿态不变）。
- `/v1/ready` `components.file_storage` 如实上报装配模式
  （`file_service_adapter` / `protocol_blocked_p0` / `protocol_blocked_config_invalid`）；
  `main.rs` 启动日志同步；`AppState.storage_mode` 承载。
- OpenAPI：create session 201（Location 头）+ 必填字段 + commit 200/409 语义；
  `wp04_operation_binding a9` 对齐新契约（绑定通过 → 输入齐备 → 存储门 503）。

## 3. 决策记录

| # | 决策 | 依据 |
|---|---|---|
| D-W05-1 | API 用户路径不代理上传字节：调用方先上传对象，再以 `source_object_id` 建会话。LMMS 无上传 presign（`/file/presign` 仅对已存在 key 的 get/put），代理上传需新增内容路由并扩展 V10 绑定矩阵（W10）——收益不抵复杂度；AI-PM 等服务调用方本就持有 file-service 身份（docproc 同款）。 | 契约 §12 + ART-01 §6 |
| D-W05-2 | 对象面路由使用 `lmms_tid` claim：这是"出站对象面路由"，不是授权判定/scope 装配（红线针对的是后者）。缺失即 fail-closed 503，绝不回落租户 1。 | 契约 §3.1 lmms_tid 红线 + F-24 |
| D-W05-3 | commit 失败不删除调用方对象：Version 保持 pending（可重试）、对象归属调用方；补偿删除仅适用于 AS 自身上传的输出对象（Worker，WP-AS-06）。 | 幂等重放语义 |
| D-W05-4 | 凭证机制：AS 内 ~150 行最小 mint 客户端（用户裁决：不引入 kellnr 私有仓库依赖，保持纯 crates.io 构建面）。 | 用户裁决 2026-11-28 |
| D-W05-5 | file-service/token-service URL 采用与 JWKS 信任根同规则（https 强制、回环 http 豁免）。token-service 承载 bootstrap 凭证，适配器构造期即校验。 | repo 现行姿态 |
| D-W05-6 | LMMS 侧运维前置：AS 的 bootstrap 身份需在 token-service grant 表配置 `owner→audience=file-service` 授权（`is_allowed` fail-closed）；无 grant → mint 403 → 503。 | LMMS `routes/service_jwt.rs` |

## 4. 残余风险（记录在案）

- **P1-05（信任边界）**：file-service `user_id` 自声明、租户信任基于 tid
  claim。AS 上传恒 `user_id=None`；不传 `meta.tenant_id`；不放大该边界。
- **"上传即正式"**：LMMS 无 finalize/is_temporary 区分用途协议（P1）——对象
  上传即对租户可见；无两段提交。
- **inspect 下载开销**：commit 时下载对象计算 sha256（file-service 无
  checksum attestation，P1 落地后可消除）。
- **孤儿对象**：调用方上传后永不 commit → 调用方侧孤儿（file-service
  temporary TTL 或调用方清理）；AS 不扫描不代删。

## 5. 测试证据（本地，2026-11-28）

- `cargo test --workspace -- --test-threads=6`：31 个测试目标全部
  `test result: ok`，0 failed（含新增 version_lifecycle 10 场景；wp04 a9 按新
  契约更新后 29/29）。
- `cargo fmt --all` 干净；`cargo clippy --workspace --all-targets -- -D warnings`
  干净。
- 本机注意事项：Windows dev PG 并发 `PoolTimedOut` 为既有环境现象（sqlx
  测试库并行迁移压力），与本次改动无关——以 `--test-threads=6` 稳定复现
  全绿；CI（postgres 16）无此现象。

## 6. 生产门禁确认

- 默认姿态未变：不配置 `ARTIFACT_FILE_*` → Version 链路 503；不配置
  `ARTIFACT_API_DELEGATION_ISSUERS` → 全拒。
- 未改迁移（无新 migration）；未改 LMMS 仓；无运行时绕过开关。
- 真实流量开放仍受既有门禁约束（AI-PM 签发侧零实现 + V11 撤销未接线）。

## 7. 下一步（WP-AS-06）

GenerationJob 执行链路：迁移 0006（`commit_generation_output` 单事务输出
提交）+ `request_generation` 接线 AS-2 keyed create + Worker 执行循环
（SeeCode adapter 迁入、SVG 安全重建、输出上传复用本 WP 的
`upload_object`/`FileServiceStorage`）。
