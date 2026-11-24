# ART-02 开放问题清单

按优先级排列；"待裁决"项需要产品/架构拍板，"待实现"项属已排期的后续阶段。

## P0 相关（关闭前冻结生产路径）

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| 1 | **授权委托 P0 未关闭**：lmms v1.8.1 token-service resource token 端点未鉴权挂载、claims 缺 aud/tid | 业务 API 持续 deny-by-default；AI-PM 无法真实联调 | 三选一：(a) 修复 token-service（挂鉴权 + 扩 claims）；(b) 新建专用 delegation 签发端点（aud=artifact-service、短时、project/action scoped）；(c) 先以 IAM 侧方案立项。修复后按 DelegationVerifier 端口接入 JWKS 验签 |
| 2 | **存储协议 P0 未关闭**：file-service 无 upload-intent / checksum attestation / finalize / verification / cleanup | Version 生命周期全部 503；READY 不可达（按设计） | 在 file-service 通用层补协议（ART-02 报告建议），随后实现 FileServiceStorage adapter + 上传中断/孤儿对象补偿测试（ART-05 批次） |

## 契约层待裁决

| # | 问题 | 现状 | 建议 |
|---|---|---|---|
| 3 | `artifact_key` 冲突错误码 | DB 唯一冲突映射为 400 INVALID_REQUEST（details 注明 duplicate）；ART-01 §8.1 无通用 CONFLICT 码 | 向 ART-01 契约提 errata：新增 409 `DUPLICATE_KEY`（或明确 400 语义） |
| 4 | Reference 重复创建语义 | 返回既有活跃记录（200/201 幂等语义），未返回 409 | 与 #3 一并裁决；当前实现幂等友好，契约测试已锁定 |
| 5 | Artifact lifecycle（archive/delete）无端点 | ART-01 §4.2 清单未定义 archive/delete API；DB 状态机（active→archived→deleted）已建但无 HTTP 入口 | ART-04+ 补端点设计（需 artifact:archive/delete 动作，已在 Action 集合中预留） |
| 6 | `X-Project-Id` 头是过渡态定位机制 | 正式委托签发后 project 应来自 claims；当前头仅作资源定位并与凭证核对（不一致即 403） | P0-① 关闭后评估移除或保留为冗余一致性校验 |
| 7 | GET /v1/generators 无需认证 | 注册表为全局元数据（无租户数据）；ART-01 §5 未明确其授权要求 | 若裁决需鉴权，加 scope 检查即可（单中间件路径） |

## 待实现（已排期，不在 ART-02 范围）

| # | 项 | 归属 |
|---|---|---|
| 8 | SeeCode Adapter（SVG-only、独立 Worker、安全签收、拆除 AI-PM crate 依赖） | ART-03（设计基线：docs/architecture/ART-03-seecode-adapter-design-v1.md） |
| 9 | GenerationJob 编排 + Worker 最小服务身份 + 心跳/超时调度 | ART-03 |

Mermaid Adapter 已取消：由前端 Markdown 编辑器负责解析与预览，不属于 Artifact Service。历史 0001 migration 中的 Mermaid seed 应通过后续 migration 删除。
| 11 | `/metrics`（网络受限）+ Job 队列深度等指标 | 可观测性批次 |
| 12 | outbox_event 表与事件通知 | ART-01 标注"若启用"；轮询为 V1 必选，事件通知后补 |
| 13 | PATCH 更新语义：无法区分"未提供字段"与"显式置 null"（当前 Option 三态缺省） | 契约演进时处理（如 JSON Merge Patch） |
| 14 | 速率限制（429 RATE_LIMITED 码已预留，无实现） | 部署 profile 阶段 |
| 15 | 仓库基建：依赖审计（cargo-deny）、镜像扫描（CVE）、supply-chain 签名 | 发布管线加固批次 |
| 16 | sdk_contract 的 SDK 依赖面守卫目前是 Cargo.toml 文本断言 | 建议升级为 CI 层 cargo-metadata/依赖图断言 |

## 运维备忘

| # | 项 | 说明 |
|---|---|---|
| 17 | PAT scope | fine-grained PAT 未包含新仓库 → 本仓库 push 暂走 SSH（fetch 保持 https）；如需统一 HTTPS，请把 `smark-artifact-service` 加入 PAT 的 repository access |
| 18 | compose 样例凭据 | `deploy/` 全部为本地 dev 值（显式标注）；生产部署须走 secret 注入 + `db-roles.sql` |
| 19 | OpenAPI 未用代码生成 | DTO/SDK 走"规范 + 双向契约测试"路线（任务 §4.4 允许）；若未来多语言客户端增多，可引入 spec 生成校验 |
