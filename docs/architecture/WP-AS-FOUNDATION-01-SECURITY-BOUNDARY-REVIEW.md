# WP-AS-FOUNDATION-01 — Security Boundary 审计

> 状态：PARTIAL / NOT A SECURITY SIGN-OFF
> 本文基于 main 安全状态文档、Delegation / Action / Scope 类型、StoragePort、Worker 入口、DB role grants、Docker/Compose 与开放 Draft PR 元数据。未执行渗透测试、真实 JWKS 验签、部署验证或独立第三方安全评审。

## 1. 授权边界

### 已核验设计

- Delegation 领域对象包含 delegation_id、jti、TenantScope、Actor、actions、expires_at。
- Action 为细粒度集合，包含 artifact read/create/update/archive/delete、version create/read/commit/set_current、reference write/remove、generation request/read/cancel、output download。
- TenantScope 明确要求 tenant_id/project_id 来自可信委托，而不是请求体。
- DelegationVerifier 是 application port；当前生产装配 fail-closed。
- OpenAPI 对业务端点使用 bearer delegation；health/ready/generators 显式 security:[]。

### P0-A：授权签发/验证闭环未放行

当前 Artifact Service 安全报告称 LMMS 旧 resource-token endpoint 未鉴权挂载且 claims 缺 aud/tid；本仓库拒绝消费该 endpoint，生产 verifier 无条件拒绝。PR #12 的最新描述提出由 AI-PM 签发、Artifact Service 通过 AI-PM JWKS 验签的候选方向，但授权契约仍为 DRAFT。

因此：
- 不得将 PR #12 的候选契约视为已批准；
- 不得接入旧 LMMS token endpoint；
- 不得实现“只验签不验 scope”的降级模式；
- 不得添加运行时绕过开关；
- 正式 issuer、audience、tenant/project/action claims、jti 撤销/重放策略、JWKS key rotation、clock skew 和长任务续期语义需独立安全审查批准。

## 2. 数据库角色边界

`deploy/db-roles.sql` 明确区分 migrator、app、worker、maintenance：

- App：schema usage + SELECT/INSERT/UPDATE，撤销 audit_event UPDATE/DELETE；对 artifact_creation_identity 撤销 UPDATE/DELETE；对 generation_job 撤销直接 INSERT/UPDATE，只允许调用受控创建/取消函数。
- Worker：无业务表 DML/DDL，只能执行逐个列出的 SECURITY DEFINER 协调函数。
- Maintenance：无业务表 DML/DDL，只能执行过期 idempotency 清理函数。
- Migrator：负责 schema/DDL。
- 脚本注释要求每次迁移后重新应用 grants。

安全报告声明已有角色矩阵测试；本轮未连接数据库执行 grants 或复跑测试，因此测试结论引用为文档证据，不是本轮实测。

## 3. SECURITY DEFINER 审查要求

迁移 0002/0003 引入多个 SECURITY DEFINER 函数。后续独立 SQL 审查必须逐个核对：

- 固定安全 search_path；
- 函数 owner 是否为预期 migrator；
- PUBLIC EXECUTE 是否全部撤销；
- 参数验证、租户/项目 scope 校验与锁顺序；
- 是否避免动态 SQL 注入；
- 错误消息是否泄漏内部数据；
- 受控函数之间调用权限是否最小化；
- grants 脚本与实际函数签名逐字一致；
- 新 migration 是否同步 grants 与权限测试。

本轮未逐个审阅所有函数体，状态 PARTIAL。

## 4. Worker 与 Generator 安全边界

已核验 Worker binary 仅执行协调维护（恢复租约、清理 Job 输入），没有 Adapter 时不 claim Job；不读取用户 delegation、不读取 API DSN；使用独立 Worker DSN。Dockerfile.worker 以非 root 用户运行。

但：
- 只读根文件系统、无外网、资源配额、进程树硬终止与死亡确认仍属于 ART-03B-03；
- PR #6 的独立架构复审仍为 Changes Required；
- PR #4 的 Worker 生命周期设计仍为 Changes Required；
- PR #5 的 Migration 0004 DB blocker 设计仍为 Changes Required；
- 不允许从“Worker coordination-only mode”推导出“生成执行隔离已经安全”。

## 5. 存储与敏感数据

安全报告声明：无 S3/RustFS SDK、无长期对象存储凭据；StoragePort 是唯一出口；Job 输入不进入普通查询/日志，终态 7 天后逻辑清空。物理擦除仍依赖数据库卷加密、备份访问控制和保留策略。

需要部署侧补证：
- PostgreSQL 静态加密/备份加密；
- secret manager 注入与轮换；
- DB TLS 与网络隔离；
- 审计/日志保留期与访问控制；
- 备份恢复演练；
- 镜像签名与 digest pinning。

## 6. 威胁模型与必要负向测试

| 威胁 | 必须证明 |
|---|---|
| 跨 tenant/project IDOR | 所有读取、写入、列表、版本、reference、job 路径 scope 一致；资源不存在与越权不泄漏差异 |
| action 混淆/过度授权 | 每个端点绑定唯一所需 action；缺 action fail closed |
| Token 重放/撤销 | jti、exp、issuer/aud、key rotation、撤销与缓存策略明确 |
| JWKS key confusion | 固定 issuer/jwks URI、允许算法白名单、kid 轮换与未知 kid 拒绝 |
| 请求定位头伪造 | X-Project-Id 仅定位；必须与 claims 一致 |
| SECURITY DEFINER 提权 | PUBLIC revoke、固定 search_path、最小 grants、异常参数与跨 scope SQL 测试 |
| Worker 凭据越权 | Worker 不持用户 token/API DSN/对象存储凭据；不能直接读写业务表 |
| 输入/输出恶意内容 | schema allowlist、SVG 重建 allowlist、外部引用拒绝、隔离执行与输出安全签收 |
| 日志泄密 | token、lease token、连接串、输入正文、内部路径不进入日志/trace/error |
| 供应链污染 | 依赖审计、镜像扫描、SBOM、provenance、签名、digest 部署 |

## 7. 结论

默认拒绝与 DB role 分离是当前架构的强项；但授权 issuer/verifier 闭环仍 P0，运行时隔离设计仍未获批准，部署供应链证明不足。本报告不构成安全放行，所有 P0 保持冻结。
