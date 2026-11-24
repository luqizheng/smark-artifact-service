# ART-02 安全状态 — 授权委托 P0、存储协议 P0 与默认拒绝姿态

## 1. 两个 P0 的当前状态（均未绕过）

### P0-① 授权委托（lmms token-service）

| 项 | 事实（ART-02A~D 核验结论） | 本仓库处置 |
|---|---|---|
| `POST /tokens/resource` 未挂载认证 middleware | VERIFIED-CODE / P0 SECURITY BLOCKER | **不消费该 endpoint**。仓库内无任何对 lmms resource-token 的调用代码 |
| resource token claims 缺 `aud` / `tid` | VERIFIED-CODE / P0 | `Delegation` 模型要求 aud（固定 Artifact Service）与 tenant/project scope；无 claims 就无法构造合法委托 → 无实现路径 |
| 生产验证器 | — | `ClosedDelegationVerifier`：无条件 401，错误信息明示"blocked by ART-02 P0 findings" |
| 真实实现路径（P0 关闭后） | — | `DelegationVerifier` 端口就绪：JWKS 验签 + aud/有效期/tenant/project/actions 校验 + scope 一致性；接口与测试替身已定型 |

**红线落实（ART-01 §5.5 / 任务 §4.5）：**

- ✅ 仅验签不验 scope 的实现：不存在（fail-closed，无任何"降级模式"）。
- ✅ 信任请求体 tenant_id/project_id：不存在——tenant 恒取自凭证；project 定位值与凭证不一致 → 403（有测试）。
- ✅ 用普通 service JWT 代替 project/action-scoped delegation：不存在——无任何 service JWT 解析代码。
- ✅ 永久绕过开关：不存在。`test-doubles` 是**编译期 cargo feature**，仅 `tests/` 集成测试启用；`cargo build --release -p artifact-api`（CI 有独立 step）不编译测试替身代码；`main` 只装配 `default_verifier()`（有单元测试 `default_verifier_is_fail_closed` 锁定该装配）。
- ✅ 未实现"先验签"的半成品：没有 JWKS 代码骨架伪装完成态。

### P0-② file-service 上传与对象一致性协议

| 项 | 事实 | 本仓库处置 |
|---|---|---|
| upload-intent / checksum attestation / finalize / object verification / cleanup-compensation 均缺失（PARTIAL，hash=None） | ART-02A~D | StoragePort 七项能力**仅定义接口**；生产实现 `ProtocolBlockedStorage` 全量拒绝并注明 P0 原因 |
| 禁止把普通上传/presigned PUT 当最终提交协议 | 任务 §4.6 | 版本会话与 commit 端点在鉴权/校验之后返回 503 STORAGE_UNAVAILABLE；**不存在**把版本标 READY 的代码路径 |
| 未完成协议前不允许 READY | 任务 §4.6 | READY 只能由 DB UPDATE（pending→ready，需 ready_at）达成；当前无任何 API/服务代码执行该转移（仅测试 fixture 直连 SQL 验证不变量） |

## 2. 默认拒绝姿态（已测试）

| 断言 | 证据 |
|---|---|
| 10 个业务端点无凭证 → 401 UNAUTHENTICATED | tests/default_deny.rs::all_business_endpoints_denied_without_credentials |
| 垃圾 Bearer → 401 | default_deny::garbage_bearer_is_unauthenticated |
| 每次拒绝写审计（不含凭证原文） | default_deny（audit_event 断言，outcome=denied） |
| 生产默认验证器无条件拒绝（单元级锁定） | state.rs::default_verifier_is_fail_closed |
| 跨项目读取 → 404（防 ID 探测）；伪造 project → 403；缺 action → 403 | artifact_flow::tenant_project_boundary_negatives |

## 3. 数据库权限分离（已测试）

| 角色 | 权限 | 验证 |
|---|---|---|
| `artifact_migrator`（迁移） | DDL（schema owner） | `artifact-api migrate` 独立子命令；compose 中独立一次性任务 |
| `artifact_app`（运行） | SELECT/INSERT/UPDATE；**无 DDL、无 DELETE**；audit_event 无 UPDATE | tests/roles_separation.rs：执行仓库真实 `deploy/db-roles.sql` 后验证 42501 拒绝（CREATE TABLE / DELETE / audit UPDATE）与 DML 放行 |

附加防线：`audit_event` append-only 触发器（权限外的第二道锁）；复合外键（artifact_id+tenant_id+project_id）杜绝跨租户绑定。

## 4. 其他安全边界

| 项 | 状态 |
|---|---|
| 长期 S3/RustFS 凭据持有 | 无（无对象存储 SDK 依赖；StoragePort 是唯一出口） |
| Mermaid/SeeCode 零外网、SVG 静态化、Worker 隔离 | 未实现（如实 unavailable）；实现门禁见 ART-01 §7（Golden/安全/隔离测试），属 ART-03+ |
| 错误信封泄漏 | details 仅含稳定 reason；无内部路径/堆栈/凭据 |
| 日志 | tracing 记录 request_id/动作；不记录 bearer token 原文（审计与日志结构均无凭证字段） |
| compose 样例凭据 | 仅本地 dev 值且显式标注；`db-roles.sql` 不含口令，注入由 secret 管理完成 |

## 5. 安全结论

**两项 P0 状态 = 未解除、未绕过。** 在授权委托 P0 关闭（token-service 修复 endpoint 鉴权并补 aud/tid，或改用新的委托签发通道）之前，本服务生产姿态为 deny-by-default，不产生任何业务副作用；在存储协议 P0 关闭之前，不存在任何能把 ArtifactVersion 置为 READY 的路径。开放项见 [ART-02-OPEN-ISSUES.md](ART-02-OPEN-ISSUES.md)。
