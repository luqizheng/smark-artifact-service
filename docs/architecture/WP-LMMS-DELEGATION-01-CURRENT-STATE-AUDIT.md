# WP-LMMS-DELEGATION-01 — 跨服务授权委托：现状审计（只读，R1 修订版）

| | |
|---|---|
| 工作包 | WP-LMMS-DELEGATION-01 |
| 阶段 | 第一阶段：只读审计（本文档不含任何协议裁决，协议草案见 AUTHORIZATION-CONTRACT） |
| 审计日期 | 2026-11-25（R1 修订：独立安全评审后全量重核，基线修正 + F-22 事实更正 + 新增 F-27/F-28） |
| 审计基线 | lmms tag `v1.8.2` @ `93fc3a2e`（本地检出 `92691f33` = tag 后 2 个 SDK dist 提交，`git diff v1.8.2..HEAD -- services/` 为空，服务代码与 tag 一致）；ai-pm **GitHub 权威** `master` @ `d0e72fa6`（`origin` 为内部同步副本 `origin/master@6f317176`，已落后，不作为基线）；smark-artifact-service `origin/main` @ `b5095cf`（PR #8 AS-1 + PR #9 AS-2 已合并后的真实主干） |
| 证据分级 | **A 类**（用户提供的部署环境事实：LMMS 1.8.2 已在私有网络发布）、**B 类**（Git 源码证据：上述基线的源码与测试）、**C 类**（实际部署证据：运行镜像 digest、启动版本、运行行为）——本文档全部结论为 B 类；**C 类证据缺失**（见 F-27），A 类不用于推导任何代码结论 |
| 审计方法 | 逐仓库检查真实 Handler / Service / Domain / Migration / 测试 / OpenAPI / compose 部署配置；所有结论附文件路径与函数名；"缺失"结论附检索关键词。R1 修订对上一轮全部（复核）条目抽查重验（本轮抽验：`middleware/mod.rs:265` aud=None、`revoke_svc::is_revoked` 无生产调用方、`token.router.toml:19-23` public=true、`auth_svc.rs:209-215` claims、`service_jwt.rs:41` TTL=3600、`issuer.rs:194` aud 常量、`file.rs:90-91`、`issuer.rs:253` as u64——全部属实），并修正 F-22 的事实错误（见 §5） |
| 约束遵守 | 未修改 lmms 仓库；未运行任何 Migration；未连接任何数据库 |

---

## 0. 执行结论

**LMMS 1.8.2 当前是否已经具备满足 AI-PM → Artifact Service 安全委托的全部能力？**

## **结论：B — 部分具备，需要补充 AI-PM / Artifact Service。**

一句话理由：LMMS 1.8.2 已**完整提供其职责范围内**的能力（用户身份认证、RS256 JWT + JWKS、服务间 service-jwt、文件存储与租户隔离），但**不具备可用的资源委托 Token 能力**——`POST /tokens/resource` 端点未挂载任何认证、签出的 token 缺少 `aud`/`tid`/project/action claims、且平台内没有任何服务消费此类 token。按照三服务职责边界（LMMS=身份与文件、AI-PM=业务授权决策、Artifact Service=通用执行），**委托签发能力按职责应落在 AI-PM（业务授权决策方）**，因此缺失项的补充落在 AI-PM 与 Artifact Service，不需要阻塞在 LMMS 改版上；"将委托签发上收 LMMS token-service"列为 LMMS 后续版本需求（P1），不构成本次最小可行方案的阻断项。

判定依据摘要（详见后文证据）：

| 判定要素 | 状态 |
|---|---|
| 用户身份认证 + 可验证 JWT | ✅ LMMS 已具备 |
| 服务到服务认证（aud 绑定 service-jwt） | ✅ LMMS 已具备，AI-PM 已在 file-service 上实战使用 |
| 文件存储 + 租户隔离 | ⚠️ 常规路径具备；`unwrap_or(1)` 回落 + user_id 自声明限制其安全表述（F-24，见 §1.6） |
| 委托 Token 签发（LMMS 侧） | ❌ 不具备可用实现（resource token 端点未鉴权 + claims 不全 + 无消费者） |
| 委托 Token 验证（AS 侧） | ❌ 端口契约已定义，生产实现 fail-closed（等待可信签发方出现） |
| 业务授权决策（AI-PM 侧） | ✅ 授权模型完备；缺"把决策固化为可验证凭证"的签发器 |
| 在线撤销 | ❌ 三方均不具备（LMMS 撤销表无生产读取方；详见 SECURITY-REVIEW §挑战 1） |

> 注意：本结论不等于"设计通过 = 实现完成"。当前 Artifact Service 全部业务 API 处于 **deny-by-default**（`ClosedDelegationVerifier` 拒绝一切请求），在委托链路实现并评审通过之前，任何调用方都无法真实使用 Artifact Service——这是有意设计的安全姿态，不是缺陷。

---

## 1. LMMS 1.8.2 审计

### 1.1 版本与部署形态

- 版本 1.8.2 属实：`lmms/Cargo.toml` `[workspace.package] version = "1.8.2"`（8 个 services 继承）；tag `v1.8.2` @ `93fc3a2e` 存在；`examples/docker-deploy/.image.env` 与 `examples/docker-deploy-full/.image.env` 钉版 `1.8.2`。（复核）
- **C 类证据缺口（F-27）**：上述钉版只是**仓库内的配置文件**，不能证明运行中的镜像与 `v1.8.2` 源码一致。实际部署产物一致性需要：运行镜像 digest、容器启动日志中的版本号、运行服务的实际行为探测——当前三者均无记录。本文档所有"已具备/未具备"结论均指 **tag `v1.8.2` 源码形态**（B 类证据），不声称实际部署产物已与源码一致。
- 服务清单（`services/`）：`api-gateway`、`token-service`、`iam-service`、`iam-outbox-publisher`、`audit-service`、`notification-service`、`file-service`、`prue_backend`；共享库 `libs/`（`common-jwt`、`jwt-ctx`、`authz`、`common-crypto` 等）；SDK `sdk/rust`、`sdk/java`、`sdk/iam-services`（TS）、`sdk/file-services`（npm）。
- 部署：`docs/deploy-topologies.md`（2026-11-06）定义三种内网拓扑（单机 / 合并模板 / 多机分置），全部为纯内网 HTTP——`grep "listen 443|ssl_certificate"` 于 examples/infrastructure 为 0 命中，nginx 配置 `listen 80`。**无 TLS、无 vault/KMS**，密钥经 `.env` / compose env 注入。
- JWT 签名密钥：`services/token-service/config/config.yaml` 指向 `/app/keys/private.pem|public.pem`（compose 命名卷 `token-keys` 持久化）；keystore（`services/token-service/src/keystore.rs`）支持多 kid、损坏 fail-closed；轮换为 API 触发（`POST /admin/keys/rotate`，`routes/keys_admin.rs:133`），kid-strict 验签（`libs/common-jwt/src/verify.rs:71-83`）。**无自动轮换**。仓库根 `keys/private.pem`/`public.pem` 为历史遗留密钥对且**已提交进 git**（复核：`git ls-files keys/` 命中）——见 SECURITY-REVIEW 附加发现 F-21。

### 1.2 用户身份认证

- 登录链：`POST /iam/auth/login`（`services/iam-service/src/routes/auth.rs:24,65`）→ `AuthService::login`（`service/auth_svc.rs:26-194`，租户解析、密码验证、`iam_login_logs`、outbox 事件）→ `issue_session`（:198-255）→ iam 不自签 token，经 `TokenForwardClient::issue_user_jwt`（`service/token_forward.rs:53-85`）以 service-jwt 调 token-service `POST /tokens/user-jwt`。
- 用户访问令牌为 **RS256 JWT**（非 opaque）：`TokenIssuer::issue_user_jwt`（`services/token-service/src/issuer.rs:183-239`）。claims：`sub`（用户 id）、`jti`（uuid v4）、`iss=superpowers-token-service`、**`aud` 硬编码 `"coder-micro-services"`**（issuer.rs:194）、`exp`、`scope="access"`、`purpose="user_login"`（调用方不可覆盖：保留 claims 过滤 `issuer.rs:211` + 签发侧复盖 `issuer.rs:223-235`，测试 `issue_user_jwt_sets_audience_and_filters_reserved`）、业务 claims `tid`（租户 i64）、`ct`（跨租户布尔，仅 platform_admin 为 true）、`username`、`roles[]`、`permissions[]`（`auth_svc.rs:209-215`，复核）。
- **aud 值的可证明性（P0-02 输入）**：`aud="coder-micro-services"` 是签发代码内**硬编码常量**（issuer.rs:194），token-service 配置目录无任何 audience 覆盖项（grep config/ 0 命中）——该值在 v1.8.2 **可证明**；但它同时是**全平台共享值**（所有用户 JWT 唯一 audience），**v1.8.2 不存在 AI-PM 专用 audience**，且 `issue_access_token`（issuer.rs:104-144）、`issue_resource_token`（:241-280）、`issue_refresh_token`（:146-181）均不设置标准 `aud`。用户 JWT 的 audience 隔离只能靠"共享 aud + `purpose=user_login` 强制"组合实现（AI-PM 侧验证链设计，见契约 §3.0）；AI-PM 专用 audience 需 LMMS 改版（本工作包禁止）→ 列为契约 §3.0 的**实施阻断项**。
- 刷新会话：opaque `nanoid!(64)` SHA-256 落库 `iam_refresh_tokens`（`V001__init.sql:129-142`），30 天 TTL，**refresh 不轮换**（`auth_svc.rs:257-328`）；`logout` 撤销该用户全部 refresh 行并把当前 access jti 写入 Redis 黑名单（:330-350）。
- 身份提取：`Principal::from_token`（`libs/jwt-ctx/src/principal.rs:79-92`）→ `digua_common_jwt::verify_token`（`libs/common-jwt/src/verify.rs:65-105`，RS256、kid-strict JWKS、可选 `expected_aud`）；`Principal` 携带 `jti/user_id/user_code/tenant_id(tid, 必填)/cross_tenant/roles/permissions`。另有 `Principal::from_headers`（principal.rs:97-108）为**纯解码不验签**、信任网关已验的路径。**平台内无 `X-User-*` 头身份传播机制**（grep `X-User` 于 services+libs 为 0 命中）。
- 验签库行为细节（AS/AI-PM 验证器与 JWKS 策略设计输入，P1-01/P1-02）：`verify_token` 的第三参即 `expected_aud: Option<&str>`（verify.rs:65-69）——**API 已支持 audience 强制，未启用是调用方选择**；`Some(aud)` → `set_audience` 精确匹配，`None` → `validate_aud=false`（:86-94）；header `kid` 缺失时**回退 `default_key()`**（:80-82，"kid 必须存在"在当前库上不成立，AS 验证器必须显式拒绝缺失 kid）；未知 kid 直接报错**不触发刷新**（:76-79）；`Validation::new(Algorithm::RS256)` 固定算法（算法降级被 jsonwebtoken 拒绝），但 leeway 为 jsonwebtoken 默认 60s 未显式配置、`validate_nbf` 默认 **false**、`iat` 不校验（:84）——AS 验证器必须显式补齐。
- JWKS 缓存行为（Q5 设计输入）：`JWKSCache` 默认 `refresh_interval=3600s`（`libs/common-jwt/src/cache.rs:36`）；刷新失败 **"keeping stale"** 无限期继续用旧键（:146）；`fetched_at` 有记录但**无任何读取方强制陈旧上限**（:44,86-91）；缓存为空/过期均不阻断验签——即**当前库无最大陈旧时间概念**。AI-PM 实际消费的 kellnr 发布版 `digua_common_jwt 1.8.1`（ai-pm 根 `Cargo.toml:85`）行为与上述完全一致（已核对其 registry 缓存源码 `~/.cargo/registry/src/192.168.2.12-*/digua_common_jwt-1.8.1/src/{verify,cache}.rs`）。
- 网关鉴权：`services/api-gateway/src/middleware/mod.rs:229-327`——验签 `expected_aud=None`（:265，**不校验 aud**，复核）、强制 `tid` claim（:275-277）、Redis jti 黑名单检查**失败即放行（fail-open）**（:280-288，复核）、permission-epoch 令权限变更后旧 token 失效（:292-314）。网关**只有认证没有逐路由授权**（`RouteConfig` 仅 public/ws/headers/timeout）。

### 1.3 Token Service 实际接口清单

路由装配：`services/token-service/src/main.rs:170-259`（复核）。

| 方法+路径 | 鉴权 | 说明 | 证据 |
|---|---|---|---|
| `POST /tokens/access` | 无（公开） | `client_credentials` / `authorization_code` | `routes/access.rs:54,117,216` |
| `POST /tokens/refresh` | 无（凭 client secret + 单次刷新标记） | purpose 继承 | `routes/refresh.rs:102-179` |
| `POST /tokens/revoke` | **无（公开）** | 写 `token_revoked_jtis` | `routes/revoke.rs:30`；**`revoke_svc::is_revoked` 无任何生产调用方**（复核：非测试代码 grep 仅命中无关的 `perm_epoch::is_revoked`） |
| `POST /tokens/resource` | **无（公开）** | 签发 resource token | `routes/resource.rs:21-42`（复核：handler 无鉴权提取器；网关路由 `public = true`，`token.router.toml:19-23` 复核） |
| `GET /.well-known/jwks.json` | 公开 | JWKS 分发 | `routes/jwks.rs` |
| `POST /tokens/user-jwt` | service-jwt `aud=token-service` | 为任意 user_id 铸造用户 JWT | `routes/user_jwt.rs:35`；`main.rs:235-248`（复核） |
| `POST /tokens/service-jwt` | bootstrap token（DB 明文比对+状态+有效期） | 签发 aud 绑定 service-jwt | `routes/service_jwt.rs:43-105`（复核） |
| `/api-keys`、`/admin/oauth-clients*`、`/admin/bootstrap-tokens*`、`/admin/keys*`、`/v1/admin/bootstrap-tokens*` | service-jwt / user-jwt+权限码 | 管理面 | `main.rs:211-259` |
| introspection（RFC 7662） | — | **不存在**（grep `introspect` 0 命中） | — |

TTL：access 3600s、refresh 30 天、resource 默认 300s、service-jwt 硬编码 3600s（`service_jwt.rs:41` 复核）。

### 1.4 Resource Token / 委托授权机制 —— 关键结论

**LMMS 1.8.2 存在 resource token 的"铸造形状"，但端到端不可用，且铸造端点未受保护。**（复核）

- 铸造：`TokenIssuer::issue_resource_token`（`issuer.rs:241-280`，复核）签出 claims：`sub`（路由硬编码 `"resource-service"`，`resource.rs:30`）、`jti`、`iss`、`exp`、`scope="resource"`、`resource_type`、`resource_id`、`permission`（单字符串）、`purpose="resource"`。**没有 `aud`、没有 `tid`、没有 project/action 细粒度 scope、没有业务操作绑定。**
- **TTL 输入缺陷（F-28，R1 新增）**：`ResourceRequest.ttl_sec: Option<i64>`（`routes/resource.rs:12`）→ `req.ttl_sec.unwrap_or(state.default_resource_ttl_sec)`（:25，缺省 300，`config.rs:128`）——**全链路无边界校验**：负数、零、超大值均原样传入 `issue_resource_token`，在 `issuer.rs:253` 经 `Duration::from_secs(ttl_sec as u64)` 转换——**负 i64 经 `as u64` 回绕为天文数字**（如 -1 → 18446744073709551615 秒），签出实际永不过期的 token；`expires_in` 原样回显（:40）。当前无消费者故无实害，但该端点 `public=true`（`token.router.toml:19-23`），任何未来接入方（含候选 A 场景）若直接消费即引入"永不过期能力凭证"风险。详见 SECURITY-REVIEW §11 F-28。
- 铸造端点 `POST /tokens/resource` **无任何鉴权**：handler 无提取器（`resource.rs:21-24`），挂在开放路由器（`main.rs:188-191`，无 layer），网关路由 `public = true`（`token.router.toml:19-23`）。任何能到达内网者可为自己铸造任意 resource_type/resource_id/permission 的 token。
- **无消费者**：grep `purpose::RESOURCE|scope=="resource"|resource_type` 于 services+libs 仅命中签发器与其测试；api-gateway / file-service / iam-service 均不读取。且因缺少 `tid`，该 token 无法通过 `Principal::from_claims`（`tid` 必填）与网关 `tid` 强制检查——**铸造出的 token 被平台内所有验证路径拒绝**。
- OpenAPI 缺口：`/tokens/resource` 与 `/tokens/service-jwt` 均未列入 `services/token-service/openapi.yaml` 与 `src/openapi.rs` 路径清单。
- 委托词汇检索（`delegat*`、`on-behalf`、`capability`、`service_token`、OAuth 风格 `scopes`、租户/项目/资源细粒度绑定）：除上述 resource token 外均为注释或无关命中。**结论：LMMS 1.8.2 无可用的资源委托 Token 机制。**

### 1.5 服务到服务认证

- 机制 = **bootstrap token → 短时 aud 绑定 service-jwt**（`docs-site/guides/service-to-service-auth.md` 明示取代已废弃的 Consul Connect mTLS；无服务间 mTLS，出口为纯 `http://host:port`）。
- 签发：`POST /tokens/service-jwt`，`Authorization: Bearer <bootstrap>`，body `{aud, tid, ct?}`；claims `iss/sub=<bootstrap owner>/aud=<目标服务>/tid/iat/exp(+3600s)/roles=["service"]/purpose="service"`（`service_jwt.rs:70-98`，复核）。bootstrap token 持久化于 `token_bootstrap_tokens`（哈希+状态+有效期），来源 env CSV `SERVICE_BOOTSTRAP_TOKENS`（`models/bootstrap_token.rs`，env 不匹配时 fail-closed）。
- 客户端：`ServiceJwtCache`（`libs/common-jwt/src/service_jwt.rs:40-264`）按 `(target,tid,ct)` 缓存并自动刷新，`get_for_tenant()` 专为"业务 API 代理用户操作文件"场景设计（:108-115 注释）。
- 验证：内部端点统一 `verify_aud_principal_middleware("<service>")`（`libs/authz/src/verify_aud.rs:24-57`）——iam `/internal/*`、token 管理面、**file-service 全部路由**（`services/file-service/src/routes/mod.rs:33-45`）。
- 网关→下游的用户请求转发**不附加 service-jwt**；下游直接验转发来的用户 JWT（各服务 JWKS 缓存）。

### 1.6 File Service 与文件权限模型

端点（全部经 `verify_aud_principal_middleware("file-service")`，`routes/mod.rs:33-45`；`/v1/file/*` 用户面路由已按 2026-11-07 裁决从网关移除）：

| 方法+路径 | 行为 | 证据 |
|---|---|---|
| `POST /file/upload`（multipart metadata+file） | 同步上传：对象写入 → 元数据落库，DB 失败补偿删对象 | `routes/file.rs:38`；`services/file.rs:47-113`（复核：`tenant_id = scope.tenant_id().unwrap_or(1)` 在 :90，`user_id` 取自客户端 multipart metadata 在 :91） |
| `GET /file/list` | 租户内列表 | `services/file.rs:207-229` |
| `GET /file/{id}` | 下载（临时文件过期拒绝） | `services/file.rs:116-124` |
| `DELETE /file/{id}` | 租户内删除（先删 presign 行；注释明确已签发 S3 URL 无法撤回） | `services/file.rs:129-143` |
| `GET /file/{id}/info` | 元信息 | `services/file.rs:231-238` |
| `POST /file/presign` | S3 预签 GET/PUT，TTL 1s–7d，落 `file_presign_urls` 审计 | `routes/presign.rs:27`；`services/file.rs:146-205`；本地存储返回 501 fail-closed |

- 存储：`ObjectStore` trait，`LocalStorage`（文件卷）与 `S3Storage`（SigV4，兼容 s3/oss/minio/rustfs）（`main.rs:34-50`、`config.rs:63-91`）。
- **Finalize 协议不存在**：grep `finalize|commit` 于 file-service src 为 0 命中。上传即提交（单段 put + INSERT + 补偿删除）；仅有 `is_temporary/expires_at` 临时文件回收器（`file.rs:243-278`）。**没有 upload-intent / 两段提交 / checksum attestation。**
- 权限模型：**仅租户隔离**——info/list/delete/presign 全部经 `qb.and_tenant(scope)`（:217,231-238,303-308）；测试 `tenant_isolation_test.rs::platform_admin_cross_tenant_file`、`user_jwt_rejected_on_file_routes`（aud 不符 401）。**不做**：per-user 归属校验（`user_id` 落库但从不与 principal 比对）、`FILE_UPLOAD/DOWNLOAD/DELETE/READ` 权限码检查（`libs/authz/src/perms.rs:86-92` 定义但全平台仅 `bootstrap_token_web.rs:32` 使用过 `has_permission`）、文件级 ACL。
- 信任边界事实（代码事实，非推测）：(a) 任何持 file-service service-jwt 者对其 `tid` 租户全部文件拥有读写删权限；(b) `ct` 跨租户调用方租户回落为 **1**（`unwrap_or(1)`）；(c) `user_id` 归属由调用方在 metadata 中任意声明、服务端不校验。影响评估见 SECURITY-REVIEW §挑战 3/6 与附加发现 F-24。

### 1.7 LMMS 能力总表

| 能力 | 状态 | 关键证据 |
|---|---|---|
| 用户登录 + RS256 JWT 访问令牌 | ✅ | `iam-service/auth_svc.rs`；`token-service/issuer.rs:183-239` |
| JWKS 分发 + kid-strict 验签 | ✅ | `routes/jwks.rs`；`libs/common-jwt/src/verify.rs` |
| claims：sub/tid/roles/permissions/jti/exp | ✅ | issuer.rs:190-236 |
| aud 受限（用户令牌） | ⚠️ 部分：aud 硬编码 `coder-micro-services`（全平台共享，**无 AI-PM 专用值**），网关与 Principal 提取器**不校验 aud**，其他 token 类型无标准 aud；aud 仅在内部端点中间件强制 | issuer.rs:194；`middleware/mod.rs:265`；`verify_aud.rs`；§1.2 |
| 刷新/登出 | ⚠️ 部分（refresh 不轮换） | `auth_svc.rs:223-328` |
| 在线撤销 | ❌ 实质不可用：`/tokens/revoke` 公开且撤销表无生产读取方；Redis 黑名单仅 logout 写入且 fail-open | 复核：`revoke_svc::is_revoked` 无生产调用；`middleware/mod.rs:280-288` |
| introspection | ❌ | grep 0 命中 |
| 服务间认证（aud 绑定 service-jwt） | ✅ | `service_jwt.rs`；`libs/common-jwt/src/service_jwt.rs`；`libs/authz/src/verify_aud.rs` |
| **资源委托 Token（可用）** | ❌ 铸造形状存在但端点未鉴权、claims 缺 aud/tid/scope、无消费者 | `resource.rs`；`issuer.rs:241-280`；`token.router.toml:19-23` |
| 文件租户隔离 | ⚠️ **有条件成立（R1 修订，不得表述为"已就绪"）**：常规 `Single(tenant)` 调用路径下 `qb.and_tenant` 隔离有效；但 scope 缺失/`CrossTenant` 时上传回落租户 **1**（`file.rs:90` `unwrap_or(1)`）、`user_id` 归属由调用方 metadata 任意声明（:91）、FILE_* 权限码全平台未消费——"所有调用方式都安全"不成立（§1.6、F-24） | `qb.and_tenant`；`tenant_isolation_test.rs`；`file.rs:90-91` |
| 文件 per-user 归属/权限码/ACL | ❌ | `services/file.rs:90-91`（复核） |
| upload-intent / finalize 两段协议 | ❌ | grep 0 命中；presign PUT 存在（仅 S3） |
| 签名密钥轮换 | ⚠️ API 手动轮换、无自动调度 | `keys_admin.rs:133-226` |
| TLS / 密钥管理 | ❌ 内网纯 HTTP；密钥走 env；遗留密钥对入库 | `deploy-topologies.md`；`git ls-files keys/` |

---

## 2. AI-PM 审计（GitHub 权威 master @ d0e72fa6）

> 基线说明（R1 修正）：上一轮审计使用 `origin/master@6f317176`。GitHub 权威 `master` 已推进至 `d0e72fa6`（3 个 docs 提交：ART-01 errata 增删 + docs 索引），`apps/`、`crates/` 代码在两基线间**逐字节一致**（`git diff 6f317176..d0e72fa6 -- apps/ crates/` 为空），故本轮以权威基线重标注，代码引用行号不受影响。

### 2.1 技术栈与结构

Rust 2021 workspace：`pm-domain / pm-application / pm-infrastructure / pm-ai / pm-mcp / kb-pc-contracts` + 应用 `pm-api`（REST+MCP，:8090）、`pm-worker`、`ai-pm-worker`、`ai-pm-runner`；前端 `ai-frontend/`（`/api`→pm-api、`/gw`→lmms api-gateway）。LMMS 平台 crate 经 kellnr 私有源引入：`digua_common_jwt 1.8.1`、`digua_common_consul 1.8.1`、`digua_file_sdk 1.8.1`（`Cargo.toml` workspace deps）。部署 `infrastructure/full-dev/docker-compose.ai-pm.yml`（pm-api/pm-worker/ai-pm-worker 与 lmms 服务栈共享 `${DATABASE_URL}` 实例）。

### 2.2 LMMS 登录认证集成

- **AI-PM 不做登录**：登录/刷新/登出由前端直连 `/gw`（lmms api-gateway → iam-service）。AI-PM 对每个请求做**本地 JWKS 验签**（`apps/pm-api/src/auth.rs:1-34`；`main.rs:44-67`）：`AI_PM_AUTH_MODE=jwks` 时从 `AI_PM_JWKS_URL` 拉 JWKS，启动期未就绪则全部请求 401 `IAM_AUTH_1004`（fail-closed）。
- 凭证形态：每请求 `Authorization: Bearer <RS256 JWT>`（唯一例外：SSE `/v1/pm/events/stream` 接受 `?access_token=`，`auth.rs:549,685-690`）。**用户 JWT 不落库、不向下游转发**（migration 中仅有 `token_hash` 类列：lease token、cpk 设备凭证、邀请链接）。
- claims 解析（`AuthenticatedIdentity`，`auth.rs:91-107`）：`tid/sub/roles/permissions/aud/client_id/purpose/ct/username/nickname`；purpose 白名单含 token-service v1.7.0 的 `user_login/api_key/auth_code/service/resource`，未知 purpose fail-closed（:136-152）；service 角色令牌拒绝（:329-334 `IAM_PERM_3003`）、跨租户令牌拒绝（:335-337 `IAM_PERM_3004`）。
- **⚠️ R1 关键缺口（P0-02 证据）**：`require_bearer` 调用 `verify_token(token, jwks, None)`（`auth.rs:289`）——**未传 expected_aud，用户 JWT 的 audience 不被校验**；`aud` claim 被解析进 `AuthenticatedIdentity`（:117）但仅用于存量旧令牌的 `auth_mode` 推断（:142-150：有 aud → interactive、有 client_id → api_key）。即当前 AI-PM 仅凭"签名有效 + `tid` 存在"放行 token，api_key/service/resource（无 aud）与 user_login（aud=`coder-micro-services`）在验签层**不可区分**，区分完全依赖 purpose 解析。delegation 签发链路必须建立独立验证链（aud 强制 + purpose=user_login + ct=false + roles 排除），见契约 §3.0 与 §3.3。

### 2.3 身份上下文传递

- 中间件链（`apps/pm-api/src/routes.rs:175-230`）：`require_bearer` → `resolve_principal` → `require_active`（MCP 另有 `require_mcp_permission`）。身份对象注入 request extensions：`AuthenticatedIdentity`、`AuthedPrincipal`、`HumanActor`（MCP 工具层经 rmcp `RequestContext` 读取，`crates/pm-mcp/src/caller.rs::caller_from_ctx`）。
- 租户映射：上游数值 `tid` 1:1 镜像进 `core.tenant (provider='lmms', external_key=tid)`（`ensure_tenant`，`auth.rs:520-546`）；身份自动开通为 **inactive**，首个管理员 env 对激活为 owner。
- **角色以 DB 为准**：每请求读 `tenant_role()`（:496-500）；token `roles` 仅作记录不用于授权（ADR-0001 D5）。
- **出站调用不转发用户 JWT**：对 lmms file-service 每请求铸造 per-tenant service-jwt——`ServiceJwtCache::get_for_tenant("file-service", tid, ct=false)`，凭证为进程级 `SERVICE_BOOTSTRAP_TOKEN`（`apps/pm-api/src/files.rs:96-117`，复核；`ct` 恒 false 因 pm-api 拒收跨租户 token）。同一模式复用于 `pm-infrastructure/src/docproc_store.rs:73-79`（worker 侧上载归属覆写为服务身份 "pm-worker"）。

### 2.4 租户/项目授权模型

- 数据模型（`migrations/0001_baseline.sql`）：`core.tenant`、`core.principal`（kind: human|dev_agent|ai_pm|service）、`core.principal_identity`、`core.tenant_member`（owner|admin|member）、`core.project`、`core.project_member`（owner|member|ai_pm|architect|developer|tester）、后加 `core.product_member`（0040）、agent registry（0041）。
- 检查函数：`project_visible/require_project_visible`（`apps/pm-api/src/support.rs:252-286`，SQL join project+project_member，不可见即 404 不泄露存在性；R1 复核 `require_project_visible` :266-277）；MCP `ensure_member/authorize → require_project_member`（`crates/pm-mcp/src/caller.rs:58-99`，全部写工具挂接）；项目关闭闸门 `project_gate.rs:46-103`；人工确认闸门 `ensure_trusted_context` + `require_human_decision_context`（`support.rs:109-160`，human 身份以 **DB lookup** 验证而非 token）。
- **⚠️ R1 边界澄清（P0-03 证据）**：`require_project_visible` 只回答"该 principal 是否为该租户该项目的（任意）成员"——它**不是** Artifact 动作授权：AS 的 15 个精确 Action（`artifact-domain/src/action.rs:6-22`）与 AI-PM 的 project 角色（owner/member/ai_pm/architect/developer/tester）之间**不存在任何映射**。若把"项目可见"直接等价为"可签发全部 15 个 Action"，等于把 `artifact:delete`、`generation:cancel` 等破坏性动作授予全体成员。R1 裁决：签发必须走 AI-PM 内部**受控业务意图接口**（见契约 §3.3），每意图映射精确 Action 集，禁止调用方提交任意 actions。
- 生成触发与验收：run 由执行体经 claim/lease 领取（ADR-0002 lease token）；MCP 写工具需 JWT 权限码 `PM_MCP_READ/DOCUMENT_WRITE/AGENT_REPORT/COMMAND`（`auth.rs:567-580`）；任务产物正式验收 `POST /v1/pm/tasks/{task_id}/product-acceptances` 要求 DB 验证的人类 + 目标产品成员资格（`crates/pm-application/src/task_products.rs:409-436`，DR-PA-06）；viz 审批 `decide_approval` 挂 `require_project_visible`。

### 2.5 服务端委托能力

- **已有窄形态**：per-tenant、aud 绑定、短时 service-jwt 服务端铸造（`ServiceJwtCache`）——正是委托模式的雏形，但**仅接了 file-service 一个受众**。
- **没有**：面向 artifact-service 的任何委托；client_credentials 流；资源级 scope 签发。检索 `client_credentials|service_account|delegat|resource_token|token_exchange|on_behalf` 于全部 `.rs` 仅 2 处无关命中。
- AI-PM 自身文档已明确该缺口：`docs/architecture/smark-pm-architecture-decisions/ART-02-...v1.0.md:13,62-75` 指出"现有代码没有证明 token-service 已能签发 aud=artifact-service 且带 project/action scope 的短时能力凭证"，并将授权委托列为 P0 阻断、给出两个待裁决选项（token-service 扩展 vs AI-PM 授权代理）。

### 2.6 Artifact Service 客户端

**不存在。** 检索 `smark-artifact|artifact-service_url|ARTIFACT_SERVICE` 于 `.rs/.yml/.toml/.md`：代码命中仅为**内部** viz artifact 模块（`crates/pm-application/src/visualization/artifact_service.rs`）与其测试；任何 compose 文件无 artifact-service 容器。唯一 artifact 相邻的外呼是 **lmms file-service**（`FileServiceStore`）。ART-01 §10 规划的 SDK（base URL/超时/重试/Idempotency-Key/凭证注入）未实现。

### 2.7 密钥存储

- 全部 env 注入：长时服务凭证 `SERVICE_BOOTSTRAP_TOKEN`（token-service bootstrap 白名单 `SERVICE_BOOTSTRAP_TOKENS`）；主加密密钥 `AI_PM_SECRET_KEY` → AES-256-GCM 加密 LLM key/SSH 私钥（AAD 绑定 project id，`crates/pm-application/src/secret.rs`，缺钥 fail-closed）；DB 只存哈希（cpk sha256、`lease_token_hash`、邀请 `token_hash`）。**delegation 签名密钥必须使用独立环境变量/安全注入（如 `AI_PM_DELEGATION_SIGNING_KEY`），不得复用 `AI_PM_SECRET_KEY`**（前者是数据加密密钥，后者用途不同、轮换周期不同——契约 §4）。
- **发现（R1 事实更正，原 F-22 表述有误）**：仓库根存在 `.test-env.md`（1193 字节，2026-11-24）含明文基础设施凭据（类别：SSH/root、LLM API key、GitLab——**本审计不复述其内容**）。R1 重核结论：该文件**未被 git 跟踪**（`git log --all -- .test-env.md` 零命中；`git cat-file -e <ref>:.test-env.md` 在 `github/master@d0e72fa6`、`origin/master@6f317176`、本地分支 HEAD 均不存在），且被 `.gitignore:20` 显式忽略——**上一轮"已提交进 git"的表述是错误的**。评级维持 **P0**（独立评审指定），但理由修正：不再是"远端 git 历史暴露"，而是①活跃明文凭据以文件形式存在于工作树，防护仅依赖单行 `.gitignore`（`git add -f` 或 ignore 规则回归即暴露）；②无法证明凭据未经其他渠道（同步盘/聊天/备份）泄露。处置要求不变：**立即轮换全部凭据 + 移出工作树**；轮换前不得视其为可继续使用的凭据。

### 2.8 Artifact Job / Task / Run 关联

- run 模型：`delivery.task → delivery.task_run`（lease_token_hash、lease_generation、deadline_at、execution_mode；`0001_baseline.sql:1894-1950`）；执行体经 MCP 工具以 lease-token 认证领取（`ai-pm-runner/src/mcp.rs`）；延长执行需 `delivery.task_run_execution_authorization` 审批行。
- 文档处理生成：`docproc.document/processing_job/artifact`（`0016_docproc_domain.sql`）；`pm-worker` 表轮询 + 事务 outbox 回写（`apps/pm-worker/src/main.rs:52-63`；`outbox.rs:144-158`）；文件经 `FileServiceStore` 写 lmms file-service；file-service 未配置时诚实跳过循环。
- 验收：viz `draft→pending→published/stale/rejected`（hash/version 漂移 409）；任务产物人工验收中心 `GET /v1/pm/artifacts`、`GET /v1/pm/acceptance-overview`。**无入站回调**；出站走 outbox + SSE。
- **与 smark-artifact-service 的关联：不存在**（无客户端、无轮询、无回调）。

### 2.9 反模式扫描

| # | 检查项 | 结论 | 证据 |
|---|---|---|---|
| a | 用户长期 Access Token 转交下游并持久化 | **否** | 用户 JWT 仅本地验签（`auth.rs:289`）；出站用 per-tenant service-jwt（复核 `files.rs:96-117`）；无用户 JWT 落库 |
| b | Project 权限判断交给 Artifact Service | **否**（尚无客户端可交） | ART-01 §1.1 明确权限留在 AI-PM |
| c | 外部服务直连 AI-PM 数据库 | **否**（注意：full-dev 中 ai-pm 与 lmms 服务共享同一 PG **实例**，表级分属；artifact-service 为独立仓库/独立库） | `docker-compose.ai-pm.yml:15,53` |
| d | 长期 LMMS 凭证交给 Worker/执行体 | **部分**：pm-api/pm-worker 持长时 `SERVICE_BOOTSTRAP_TOKEN`（服务凭证，非用户凭证）；**run 执行体仅获 run 级 lease token**，ADR-0002 D7 明确真人 API Key 不进入平台 spawn 的进程 | `files.rs:57-66`；`pm-worker/main.rs:52-55`；`codex_executor.rs:719,759-760` |
| e | 信任客户端传入 tenant/project | **否**：tenant 恒由服务端从 token 镜像；project 访问 DB 验证；file 代理显式丢弃客户端 `tenant_id` | `auth.rs:463,507`；`support.rs:252-265`；`files.rs:16-19` |

---

## 3. Artifact Service 审计（origin/main @ b5095cf，AS-1 + AS-2 合并后主干）

### 3.1 结构与入口

Rust 2024 workspace，六边形分层：`artifact-domain`（`Artifact/ArtifactVersion/GenerationJob/JobState/Action/Delegation/TenantScope`）、`artifact-application`（端口 + 服务）、`artifact-infrastructure`（PG 仓储、`delegation.rs`、`storage.rs`）、`artifact-contract`（DTO/错误码）、`artifact-client`（对外 Rust SDK）、`apps/artifact-api`（`serve`/`migrate` 子命令）、`apps/artifact-worker`（**协调型 worker**：租约恢复 + 输入清理，**不领业务 Job**——未声明任何 adapter 能力，`main.rs:1-16,88`）。配置全部走 env（`ARTIFACT_API_*`、`ARTIFACT_WORKER_*`），fail-fast 无默认值。**全 workspace 无 JWT 库依赖**（Cargo.toml 无 jsonwebtoken/jwks）。

### 3.2 认证与调用方身份（AS-1 / AS-2 之后的主干现状）

- 唯一凭证形态：`Authorization: Bearer <委托 token>`（`apps/artifact-api/src/auth.rs:88-96`）；项目定位经 `X-Project-Id` 头（UUID，缺失/非法 400）。无 API key、无 `X-User-*`。
- **生产验证器 fail-closed**：`ClosedDelegationVerifier`（`crates/artifact-infrastructure/src/delegation.rs:15-33`，复核）无条件拒绝一切 token，拒绝理由明确引用 ART-02 P0（resource token 端点未鉴权、缺 aud/tid）；`apps/artifact-api/src/state.rs:28-30` 默认装配并有测试 `default_verifier_is_fail_closed` 锁定。**无任何运行时绕过开关**（`.env.example` 明示；CI 发布构建不含 `test-doubles` feature，`.github/workflows/ci.yml:53-58`）→ **当前全部业务端点 401 deny-by-default**（测试 `default_deny::all_business_endpoints_denied_without_credentials`）。
- 端口契约（`crates/artifact-application/src/port/auth.rs:1-22`，复核）：`DelegationVerifier::verify(bearer) -> Delegation`，实现方**必须**验证受信签发方签名、`aud`（固定 = Artifact Service）、有效期、tenant/project/actions 范围，且"不得信任任何请求体字段"。
- 领域模型（复核引用文件）：`Delegation { delegation_id, jti, scope: TenantScope{tenant_id, project_id}, actor: Actor{subject, actor_type: User|Agent|Service}, actions: HashSet<Action>(15 种细粒度动作), expires_at }`（`crates/artifact-domain/src/action.rs:80-87`）；授权原语 `Delegation::require(action, scope)`（:95-113）——scope 不匹配或动作缺失即 `SCOPE_DENIED`。**租户恒取自凭证**（`auth.rs:79`），body/header 中的 id 仅是"资源定位输入，不是授权证明"（`scope.rs:3-4`）。
- **⚠️ R1 关键缺口（P0-01/P0-04 证据）**：① `TenantScope.tenant_id` 类型为 `Uuid`（`scope.rs:10-13`），而 LMMS `tid` 为数值 i64（`libs/jwt-ctx/src/scope.rs:10-15` `Single(i64)`）——**类型不兼容是刻意边界**：委托 token 若携带 LMMS 数值 tid，AS 无法也不得将其装入 `TenantScope`，tenant 必须来自 AI-PM 内部 tenant UUID（`AuthedPrincipal.tenant_id`，`auth.rs:174`）；② `Delegation` 领域模型**没有任何操作绑定字段**（无 artifact_id/generation_job_id/operation_id/caller_request_id），`require()` 只判 scope+action——同一项目内一枚有效 token 在 TTL 内可对**任意**目标执行**任意**已授权动作，可重复用于不同操作（P0-04）。operation-bound 绑定必须由验证层新增 claim（契约 §3.1）并在服务层比对目标，或扩展领域模型（实施计划阶段 1 任务）。
- 401/403 作为拒绝事件写入审计（`apps/artifact-api/src/error.rs:65-112`）。
- AS-1：`ValidatedCreateArtifactRequest` + 规范化指纹 v1（`service/creation_fingerprint.rs:32-82`，域分隔符 `artifact-create:v1`，冻结黄金向量测试）；持久化 `artifact.artifact_creation_identity`（migration 0004）。
- AS-2：`caller_request_id` 三元组（`caller_request_id/request_fingerprint/fingerprint_version`）不可变持久化（migration 0005）+ 查询端点 `GET /v1/generation-jobs/by-caller-request/{caller_request_id}`（`routes/mod.rs:63-66`；跨 scope 命中不可区分，统一 404）。

### 3.3 委托凭证验证逻辑

**仅有端口与 fail-closed 实现，无真实验签代码。** 无 `jsonwebtoken`/JWKS 拉取/scope claim 解析代码；OpenAPI 已声明 `bearerDelegation` scheme（`bearerFormat: delegation-jwt`）并记录 fail-closed 姿态（`openapi/openapi.yaml:529-537`）。等待可信签发方出现后按端口接入。

### 3.4 LMMS 客户端 / File Service 客户端

**不调用 LMMS。** `reqwest` 仅用于 `artifact-client`（对外 SDK）。文件端口 `StoragePort`（`port/storage.rs`：`create_upload_intent / inspect_object / create_download_intent / copy_or_finalize_object / delete_object / head_object`）的生产实现为 `ProtocolBlockedStorage`（`crates/artifact-infrastructure/src/storage.rs:16-60`）——每个方法返回 `StorageError::ProtocolBlocked`（理由：上游缺 upload-intent/checksum attestation/finalize/对象校验/清理补偿协议，即 ART-02 存储协议 P0）。`/v1/ready` 诚实上报 `file_storage: "protocol_blocked_p0"`（`routes/mod.rs:104`）。未来 worker 文件身份已预留决策 A3A-06（短时、aud=file-service 的服务 token）。

### 3.5 资源归属模型

- Migration 清单（哈希钉于 `migrations.sha256`，测试 `migration_guard::released_migrations_are_unchanged` + CI "released migrations are immutable" 锁定；**本次审计未运行任何迁移**）：
  - `0001_init.sql`：schema `artifact`，8 表；全部业务表带 `tenant_id, project_id NOT NULL`；复合外键 `(id, tenant_id, project_id)` 级联约束 version/output/reference/job；current_version 守卫、version 不可变/编号分配、job 状态迁移守卫、审计 append-only 触发器。
  - `0002_job_coordination.sql`：job 输入快照（256KiB 上限 CHECK）、`generation_job_attempt`（lease_token 仅存 sha256）、首批协调函数（SECURITY DEFINER、回收 PUBLIC EXECUTE）。
  - `0003_job_coordination_fixes.sql`：替换协调函数；`claim_generation_job` 按 `(generator_id, adapter_version)` 分区 + `FOR UPDATE SKIP LOCKED`；`expires_at/max_execution_seconds/deadline_at`。
  - `0004_artifact_creation_identity.sql`（AS-1）：`artifact_creation_identity`（append-only + 与 artifact 双向配对 DEFERRED 触发器）、`artifact_key` 不可变、`purge_expired_idempotency_records` 维护函数。
  - `0005_generation_job_caller_identity.sql`（AS-2）：caller 身份三元组 + CHECK（全空或全非空）+ 部分唯一索引 `(tenant_id, project_id, caller_request_id) WHERE caller_request_id IS NOT NULL`；`guard_job_transition` 加固（caller 列永久不可变，含 NULL→非 NULL）；`create_generation_job_with_input` v2（冻结的重放判定次序 + 唯一冲突异常块重跑同一组闸门）；EXECUTE 回收 PUBLIC。**信任边界声明（0005 L22-27）：DB 函数无法验证 HTTP 授权；`p_tenant_id/p_project_id` 必须来自已验证的授权上下文（委托→tenant，X-Project-Id→project），绝不能来自请求体。**
- 查询强制：仓储全部方法显式 `tenant_id = $ AND project_id = $`（`crates/artifact-infrastructure/src/postgres/mod.rs:371-494,510-554,651-685`）；服务层每个操作先 `delegation.require(Action, scope)`（`service/artifact.rs` 13 处挂接）。**无 RLS**（AS-2 报告明示隔离不依赖 RLS，靠 scope 谓词 + 唯一索引 + 触发器）——跨 scope 测试齐备（`as2_20/21/22`、as1 `identity_cannot_cross_tenant_or_project_scope`）。

### 3.6 Generation Job 生命周期

- HTTP 端点（`routes/mod.rs:57-71`）：`POST /v1/artifacts/{id}/generation-jobs`、`GET /v1/generation-jobs/{id}`、`GET /v1/generation-jobs/by-caller-request/{id}`、`POST /v1/generation-jobs/{id}/cancel`。**无 claim/complete/fail HTTP 端点**（worker 专属 DB 函数）。
- **Generation Create 是双重门禁，当前实际不可开放**：① 鉴权门（`Scoped` + `Action::GenerationRequest` + scope 匹配）；② 可用性门——两个注册 generator（seecode）均 `status='unavailable'`，`request_generation` 恒返回 503 `GENERATOR_UNAVAILABLE`（`service/artifact.rs:602-622`），handler 对 `Ok` 分支硬拒绝"拒绝伪造成功"（`routes/artifacts.rs:529-532`；测试 `as2_41_http_create_stays_fail_closed`）。AS-2 的 DB 创建路径（`create_generation_job_with_input` v2 + `PgKeyedGenerationJobCreator`）**已实现未接线 HTTP**（AS-4 显式排除在 AS-2 范围外）。
- 状态机：`queued→running|cancelled; running→succeeded|failed|cancelled|timed_out|retry_wait; retry_wait→queued|cancelled`（`domain/job.rs:9-54` + 触发器双向强制）。
- caller_request_id 语义（AS-2）：与 `request_hash`（原始体传输防重放）、`input_sha256`（持久化输入快照）三者正交；重放解析 `created / caller_request_replay（零写入）/ idempotency_replay`；冲突 `CALLER_REQUEST_ID_REUSED(409)`、指纹版本不匹配与跨身份幂等键均 fail-closed 5xx。
- Worker：`claim_generation_job(worker_id, lease_seconds, adapter_versions)` SECURITY DEFINER，租约 token 每次随机、明文只返回一次、DB 仅存 sha256；heartbeat/complete/fail/confirm-cancel 需 `(job_id, attempt_id, lease_token)` 全匹配。**worker 不做任何业务授权决策**——DB 角色使其连业务表都读不到（§3.7）。

### 3.7 DB 角色

`deploy/db-roles.sql`（每次迁移后必须重跑——DROP FUNCTION 丢失 EXECUTE 授权；脚本头 L13-17 与 OPEN-ISSUES #23 均有备忘）：

| 角色 | 授权要点 | 使用方 |
|---|---|---|
| migrator | schema owner + DDL | `artifact-api migrate`（migrator DSN） |
| APP | 业务表 SELECT/INSERT/UPDATE，无 DELETE；audit 与 creation_identity 的 UPDATE/DELETE 撤销；**`generation_job` INSERT/UPDATE 撤销**（Job 行只能经 v2 函数创建、只能经 `request_generation_job_cancel` 取消）；无 worker 函数 EXECUTE | artifact-api serve |
| WORKER（NOINHERIT） | **0 表权限**；仅 7 个协调函数 EXECUTE | artifact-worker |
| MAINTENANCE（NOINHERIT） | 仅 `purge_expired_idempotency_records(integer)` | 运维 cron |

角色分离有运行时测试锁定（`roles_separation.rs`、`worker_roles.rs`）。**Worker 无超出任务范围的长期凭证**：除专用 DB 角色外无任何用户/对象存储凭证（`apps/artifact-worker/src/main.rs:9`、`config.rs:2-5`；DSN Debug 脱敏有测试）。

### 3.8 上传与 Version 创建链路

`POST /v1/artifacts`（Idempotency-Key 或 AS-1 artifact_key 单事务路径，崩溃恢复由持久 identity 兜底，测试齐备）→ `POST /v1/artifacts/{id}/versions` → `storage.create_upload_intent` → **`ProtocolBlockedStorage` → 503 `STORAGE_UNAVAILABLE`**。**当前无任何文件字节落盘路径**；version 的 `pending|ready|failed|quarantined` 状态机与两段协议仅存在于 schema 与端口定义。

### 3.9 反模式扫描

| # | 检查项 | 结论 | 证据 |
|---|---|---|---|
| a | 信任客户端 tenant/project | **否**：tenant 恒取自凭证；project 与凭证核对（不一致 403）；body 无 tenant 字段入持久层 | `auth.rs:79`；`action.rs:100-105` |
| b | Worker 持超范围凭证 | **否**：0 表权限 + 7 函数；租约 token 单次明文、哈希落库 | `db-roles.sql:74-98` |
| c | 跨租户读写路径 | **否**（结构注意：无 RLS，靠每查询显式谓词 + DB 触发器；0005 信任边界注释声明 DB 无法验证 HTTP 授权，未来误接线不会被 DB 拦截） | §3.5、§3.6 |
| d | 凭证/密钥入日志 | **否**：grep 无命中；DSN 脱敏有测试；审计只存 delegation_id 与 subject | `postgres/worker.rs`；`config.rs` |

---

## 4. 三服务职责边界（现状快照）

```
            LMMS 1.8.2（已发布内网）
     身份认证(登录/JWT/JWKS) · Token Service · File Service(租户隔离)
            │ 用户JWT(验签)              ▲ service-jwt(aud=file-service)
            ▼                            │
          AI-PM ──×（无任何通路）──── smark-artifact-service
   项目授权决策 · Task/Run · 验收        委托端口(fail-closed) · Artifact/Version/Job
```

- LMMS：身份 + 文件资源能力 ✅ 已就位（含 S2S service-jwt 机制）。
- AI-PM：业务授权决策 ✅ 就位；**把决策固化为可验证委托凭证的签发器缺失**；artifact-service 客户端缺失。
- Artifact Service：通用生命周期 ✅ 就位（AS-1/AS-2 主干已合并）；**真实验证器、文件存储 adapter、AS-4 创建接线**缺失（前两者均被"可信签发方不存在"与"file-service 无 finalize 协议"两个上游事实合理阻塞）。

## 5. 审计附加发现（非挑战项，但必须记录）

评级为 R1 独立安全评审指定；F-27/F-28 为 R1 新增。F-21/F-22 涉及的凭据内容在任何报告、PR 评论、测试输出中**不得复述**。

| # | 等级 | 发现 | 位置 | 影响 |
|---|---|---|---|---|
| F-20 | P2（流程） | migration 0005 与 AS-2 实施报告引用的唯一规范契约 `docs/architecture/ART-04G-independent-review-as2-generation-job-caller-identity.md` **不在仓库中**（全树 find/git ls-files 均无；仅存未跟踪工作笔记 `.trae/documents/as2-durable-caller-identity-plan.md`） | smark-artifact-service | 契约溯源断链；建议补交或以本 WP 文档族替代引用 |
| F-21 | P0（LMMS 运维，非本 WP 代码） | LMMS 仓库根 `keys/private.pem`/`keys/public.pem` 已提交进**远端** git（`git ls-files keys/` 于 v1.8.2 基线命中，复核；legacy kid="default" 导入路径仍在 `main.rs:95-111`） | lmms | **凭据曾进入远端 Git 历史，按已暴露处理**：必须轮换/吊销该密钥对并从历史移除；仅删除当前文件不构成处置完成（LMMS 侧行动项，本 WP 不执行） |
| F-22 | P0（AI-PM 运维） | `.test-env.md` 含明文 SSH/root、LLM API key、GitLab 凭据（内容不复述）。**R1 事实更正：文件未被 git 跟踪、从未进入任何远端历史**（§2.7），原"已提交进 git"表述错误 | ai-pm | 评级维持 P0，理由修正为"活跃明文凭据 + 无法证明未泄露 + ignore 依赖脆弱"：**立即轮换全部凭据 + 移出工作树**（§2.7） |
| F-23 | P1（LMMS） | `/tokens/revoke` 公开无鉴权且撤销表无生产读取方（§1.3）；网关黑名单 fail-open | lmms | 在线撤销实质不可用，影响 §安全评审挑战 1/5 |
| F-24 | P1（LMMS） | file-service：跨租户回落租户 1（`unwrap_or(1)`）、`user_id` 调用方任意声明 | lmms | 未来接入 AS 文件链路时的信任边界决策输入 |
| F-25 | **P0（AI-PM delegation issuer 启用前）** | LMMS 网关与 `Principal` 提取器不校验用户 JWT `aud`（`middleware/mod.rs:265`、`principal.rs`）；同一缺口的 AI-PM 侧形态：`require_bearer` 传 `None`（`auth.rs:289`） | lmms + ai-pm | delegation issuer 启用的**前置条件**：AI-PM 侧验证链必须先行强制 aud（契约 §3.0，P0-02）；LMMS 侧修复列为后续需求 |
| F-26 | P2 | LMMS 签名密钥轮换仅手动 API；无自动调度 | lmms | 运维 runbook 补轮换节奏 |
| F-27 | P1（证据缺口，R1 新增） | **部署产物一致性证据缺失**：无运行镜像 digest、无启动版本记录、无运行行为探测记录；"1.8.2 已在私有网络发布"为 A 类用户陈述，不能替代 C 类证据 | 部署面 | 在提供 C 类证据前，任何"部署环境已与源码一致"的声明都不成立；delegation 上线检查单必须包含 C 类取证步骤 |
| F-28 | P1（LMMS 缺陷，R1 新增） | `POST /tokens/resource` 的 `ttl_sec` 输入无边界校验（`resource.rs:12,25`），`issuer.rs:253` `ttl_sec as u64` 使负值回绕为超大正数 → 可签出**实际永不过期**的 resource token；端点本身 `public=true` 且当前**全平台无消费者**（grep 仅命中签发器与测试） | lmms | 当前无实害（无消费者）；未来任何消费方接入前必须先修复（边界校验 + 拒绝非正值 + 移除 as 转换）。LMMS 后续需求，本 WP 不修改 |

## 6. 与 WP 问题清单的对应（§三 的 8 个核心问题→答案所在）

| WP 问题 | 现状答案 | 详见 |
|---|---|---|
| 1 用户身份如何传递 | LMMS 登录 → RS256 JWT → AI-PM 本地 JWKS 验签（已有） | §2.2 |
| 2 AI-PM 如何判权 | DB 验证的项目成员关系（已有） | §2.4 |
| 3 短期 Resource Token 获取 | LMMS 不可用；按职责应由 AI-PM 签发（草案见契约文档） | §1.4、CONTRACT |
| 4 AS 如何验证委托凭证 | 端口契约已定义，生产 fail-closed 等待可信签发方 | §3.2-3.3 |
| 5 Token 限定维度 | AS 领域模型已备 15 动作 + tenant/project/jti/exp；签发侧待建 | §3.2、CONTRACT |
| 6 AS 如何用 File Service | 未实现（协议 P0）；过渡路线见 PLAN | §3.4 |
| 7 防重放/越权/跨项目/扩权 | AS 已有：request_hash + caller_request_id 唯一索引 + scope 谓词 + require()；AI-PM 侧无信任客户端 id | §3.5-3.6 |
| 8 过期/撤销/不可用/长任务 | 撤销三方皆缺（代码现状；**Q1 已冻结设计**：写路径在线撤销 + fail-closed，见契约 §5）；**长任务与 Token 解耦已被 AS-2 架构消解**（worker 不持用户凭证、输入已快照） | SECURITY-REVIEW、CONTRACT §5 |
