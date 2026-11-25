# WP-AS-FOUNDATION-01 — Version / Storage / Lifecycle 契约审计

> 状态：BLOCKED / PARTIAL；只读核验，不构成 File Service 协议批准。
> 证据：domain version model、application StoragePort、ART-02 security/status reports、OpenAPI、migrations 0001-0003 与 ART-03B-01 记录。

## 1. 领域不变量

- ArtifactVersion 是不可变快照。
- Version 状态：pending → ready / failed / quarantined；READY 是终态。
- 只有 READY Version 可以成为 Artifact.current_version_id。
- version_number 在 Artifact 内单调递增，设计依赖父 Artifact 行锁，避免无锁 MAX(version_number)+1 竞争。
- Version 保存 file-service object ID、SHA-256、source format、generator/adapter 版本与 manifest；不保存对象存储 key 全文。
- Artifact Service 不应直连 RustFS/S3，也不持有长期对象存储凭据。

这些不变量在 domain 与设计/实施文档中有明确表达；数据库触发器和迁移约束需以 migration invariant tests 作为最终证据。

## 2. 当前 StoragePort

`crates/artifact-application/src/port/storage.rs` 定义了以下 6 个方法：

1. create_upload_intent(tenant_id, project_id, suggested_name)
2. inspect_object(object_id)
3. create_download_intent(object_id)
4. copy_or_finalize_object(object_id)
5. delete_object(object_id)
6. head_object(object_id)

`UploadIntent` 当前包含 upload_url、method、object_id、expires_at、max_bytes；`ObjectInspection` 包含 object_id、size_bytes、sha256、detected_mime。

**DOC-STORE-01：能力数量文档差异。** ART-02 security report 与部分文档称 StoragePort“七项能力”，当前已核验 trait 是六个方法。需要核对 ART-01 §6 的七项原始能力与 trait 映射，确认是否有遗漏，或只是统计口径错误。不得直接把“七项”改写成“六项已完整实现”。

**DOC-STORE-02：inspect_object 与 head_object 语义重叠。** 两者都返回 ObjectInspection，接口注释未解释差别。需明确 inspect 是否包含内容扫描/检测、head 是否仅元数据查询，以及调用顺序与信任等级。

## 3. 两个 P0 是互相独立的门禁

### P0-S：File Service upload/finalize

当前缺少完整的 upload-intent、checksum attestation、finalize、object verification、cleanup/compensation 协议。生产适配器为 ProtocolBlockedStorage；Version 上传与 commit 返回 503；不得生成 READY Version。

### P0-A：Delegation 授权

即使 File Service 协议完成，也不能绕过委托认证 P0。授权和存储必须分别通过，不能用一项成功抵消另一项。

## 4. 推荐的端到端一致性协议（待 File Service owner 确认）

以下仅为审计需要冻结的协议问题，不是已经批准的 File Service API：

1. Artifact Service 在 tenant/project/action scope 已验证后请求 upload intent。
2. File Service 产生不可猜测、单对象、短时、限方法/大小的 intent，并绑定 tenant/project 与期望 object identity。
3. 客户端上传临时对象；上传完成后由 File Service 或可信服务端计算/证明 checksum、size、detected MIME。
4. Artifact Service 提交 finalize 请求，携带 intent/object identity 与预期 hash/size；File Service 验证对象实际存在且内容与 attestation 一致。
5. File Service 将临时对象原子 finalize/登记为不可变对象，并返回可验证的 finalized object receipt。
6. Artifact Service 在同一数据库事务内核对 receipt、tenant/project、hash/size/content type 与 pending Version，完成 Version READY 转换并记录审计。
7. 只有 READY Version 可以被设为 current 或作为 GenerationJob 主输出。
8. 超时/重试必须幂等；finalize 重放返回同一 receipt；不能因客户端响应丢失产生重复对象或多次版本提交。

协议至少要回答：intent 是否一次性、过期与撤销、重复 finalize、对象已上传但 DB 事务失败、DB 提交成功但 HTTP 响应丢失、对象校验延迟、孤儿对象清理、下载授权、删除授权、保留期与审计。

## 5. 事务与补偿矩阵

| 故障窗口 | 必须保证 | 目前状态 |
|---|---|---|
| intent 创建成功，客户端未上传 | intent 过期并可清理 | BLOCKED / 未有协议证据 |
| 上传中断 | 不产生 READY Version | 已有设计红线；真实协议待定 |
| 上传完成，checksum 不匹配 | 拒绝 finalize，Version 不 READY | BLOCKED |
| finalize 成功，DB 写入失败 | 有可重试 receipt / 可补偿孤儿对象 | BLOCKED |
| DB 写入成功，响应丢失 | 幂等重放返回相同 Version | NEEDS-TEST |
| 同 Artifact 并发创建版本 | version_number 唯一且 current pointer 不丢更新 | 领域/DB 设计存在；需要并发测试证据 |
| 当前版本并发切换 | expected_current_version_id CAS，失败返回 conflict | API 状态表声称已实现；需多连接竞争测试证据 |
| 删除/归档 | 不误删被引用对象；物理对象清理与逻辑状态分离 | NEEDS-DECISION |

## 6. 建议后续交付

- File Service owner 提供并冻结通用协议/API 契约；
- Artifact Service 单独实现 FileServiceStorage adapter；
- 增加故障注入测试与孤儿对象补偿；
- 通过安全审查后才开放 Version READY 路径；
- 增加下载 intent 的授权、短时性、对象归属和审计测试。

## 7. 结论

当前 Version 数据模型方向合理，但完整存储协议尚未存在，故 Artifact 正文上传/提交能力仍不可用。StoragePort 是端口定义，不是可运行的存储集成。P0-S 关闭前必须保持 503 与 READY 不可达。
