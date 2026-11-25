# AS-1 + AS-2 主干集成审计报告

- 审计日期：2026-11-25
- 审计基线：`main` = `b5095cf4a4065d417ae3fed3250dda4cd78d010a`
- PR #8（AS-1）merge commit：`9d50f520bd0413889294d0d1d30163f6865328f3`
- PR #9（AS-2）merge commit：`b5095cf4a4065d417ae3fed3250dda4cd78d010a`
- 依据：GitHub PR 元数据、合并后 main 文件、Migration SQL、CI workflow/job 结果、实施报告及路由/权限代码只读核查。
- 范围：仅审计 AS-1 + AS-2 合并后的代码集成与部署前门禁；未执行数据库迁移、未连接生产数据库、未部署服务、未开放生成入口。

## 1. 审计结论

**代码集成：通过。部署启用：不通过（门禁未关闭）。**

PR #8、PR #9 均已合并。当前 `main` 与 AS-2 merge commit 完全一致（compare status = `identical`，ahead/behind 均为 0），未发现合并后代码漂移。

AS-2 最新 PR CI run [45614873858](https://github.com/luqizheng/smark-artifact-service/actions/runs/45614873858) 的 3 个 Job 全部成功：
- `fmt`
- `clippy + build + test (postgres 16)`，包括 unit/integration/contract/invariant tests、API release build、Worker release build、released migration hash 检查
- `docker images + compose`，包括 API/Worker 镜像构建、entrypoint smoke、Compose 配置校验和 grants 脚本语法检查

注意：这是 PR head 的 CI 证据；合并提交与该 head 的树已核验完全相同。CI 不是生产迁移或部署证据。

## 2. 集成检查结果

| 检查面 | 结果 | 证据 / 说明 |
|---|---|---|
| AS-1 / AS-2 合并状态 | PASS | PR #8、#9 均 merged；main 与 #9 merge commit identical |
| Migration 顺序与历史不可变 | PASS（代码层） | 0004、0005 已加入；`migrations.sha256` 登记 0001–0005；CI 执行 `sha256sum -c migrations.sha256` |
| AS-1 持久化 Artifact identity | PASS（代码层） | 0004 新增 append-only identity、双向 deferred pairing、Artifact key immutable guard、清理函数 |
| AS-2 Caller identity | PASS（代码层） | 0005 三列配对 CHECK、部分唯一索引、身份不可变 trigger、caller/key 双门禁及异常块新快照恢复 |
| APP 数据库权限 | PASS（代码层/测试） | APP 对 generation_job 保留 SELECT，撤销直接 INSERT/UPDATE；创建经 SECURITY DEFINER 函数；精确 EXECUTE grant |
| WORKER / MAINTENANCE 权限 | PASS（代码层/测试） | WORKER 仅协调函数；MAINTENANCE 仅幂等清理函数；无业务表直接 DML |
| Caller Request Lookup | PASS（代码层） | 使用 Scoped，与 Job 查询同一授权链路；静态路径在动态 `{job_id}` 路由前注册 |
| Generation Create fail-closed | PASS（当前边界） | HTTP route 仍通过现有不可用 Generator 路径拒绝；AS-2 没有接线真实创建；0005 只有测试环境可将 Generator 临时置为 available |
| API / Worker / SeeCode 生产启用 | BLOCKED（正确保持关闭） | 未开放生成、未启用 Generator/Worker、SeeCode Adapter 未实现 |
| 生产 Migration 0004/0005 执行证据 | BLOCKED | 本次没有生产 DB 访问或执行；不能把 CI/sqlx 测试视为生产迁移证据 |

## 3. 需要保留的部署前门禁

### 3.1 AS-1：0004 切换人工门禁

实施报告中的 4 项仍未由本次审计关闭：
- #35：旧 writer 与新 handler 不得重叠。
- #52：切换窗口内 ingress 必须一致暂停整个 `POST /v1/artifacts`，包括 keyed 与 unkeyed。
- #53：在途请求归零、旧实例完全退出的证据。
- #55：新版本 schema 兼容与 smoke 通过前保持 gated。

执行顺序必须是：暂停整个创建入口 → 排空 in-flight → 确认旧实例退出 → 执行 0004 → 重新执行 `deploy/db-roles.sql` → 部署新版本但保持路由关闭 → schema/health/smoke → 记录证据后才按单独审批恢复入口。

### 3.2 AS-2：0005 / AS2-46

AS2-46 仍为部署人工门禁。维护窗口中必须：
1. 停止会写入 GenerationJob 的所有 API/Worker 实例，并确认无 in-flight 创建事务。
2. 迁移前记录 `count(*) FROM artifact.generation_job`。
3. 执行 0005 后记录总行数，并验证 `caller_request_id IS NOT NULL` 为 0。
4. 迁移前后总行数一致。
5. 重新执行 `deploy/db-roles.sql`，验证 APP/WORKER/MAINTENANCE 最终权限。

用户此前说明当前没有生产部署、生产调用方或在途创建；这属于重要背景，但不能替代维护窗口内的数据库实测证据。

### 3.3 每次迁移后的授权重放

0005 会 DROP 旧 13 参数 `create_generation_job_with_input` 并创建 16 参数版本。函数重建会丢失旧 EXECUTE ACL，因此 **0004、0005 全部执行完后必须重新执行 `deploy/db-roles.sql`，再启动 API/Worker**。不可只执行迁移而跳过 grants。

## 4. 当前生产阻断项（不属于 AS-1/AS-2 已解决范围）

以下事项仍然阻止真实 Artifact 生成链路上线：
1. **LMMS 授权委托 P0**：委托签发端点鉴权与 claims（aud/tid）等问题必须在 LMMS 侧关闭并完成跨服务验证。
2. **File Service P0**：upload-intent / checksum / finalize 协议必须冻结并完成端到端验证；Artifact Service 不得绕过 File Service 或持有长期对象存储凭据。
3. **SeeCode Generator Adapter**：尚未实现；Generator registry 必须保持 unavailable。
4. **Worker 生产隔离**：无外网、只读根文件系统、资源配额与进程生命周期等门禁未全部完成。
5. **SVG 安全签收**：SVG parser/allowlist rebuild/sanitization 尚未完成。
6. **AS-3 / AS-4**：target_version_id 原子绑定及 HTTP Generation Create 接线不属于本次合并范围，不得因为 AS-2 已合并而提前实施或启用。

因此，当前允许的状态是：Artifact/Job 元数据与 durable identity 代码进入主干；真实生成链路继续关闭。

## 5. 文档与操作说明不一致（需单独修正）

### DOC-01：AS-2 实施报告仍记录“未合并”

`docs/implementation/artifact-service/AS-2-IMPLEMENTATION-REPORT.md` 的开头与边界声明仍写有“未合并 PR #8”，并在边界表中记录 AS-1 / AS-2 未合并。该内容已被 PR #8、#9 的实际合并状态取代。PR #9 描述也仍保留“Draft — 不合并”等历史文本。

建议后续文档修订只补充“合并状态 / merge SHA / 代码合并不等于部署”状态说明，不改写历史实施事实与原验收记录。

### DOC-02：README 手工 grants 示例遗漏 Maintenance 占位符

`deploy/dev-grants.sh` 会正确替换 `__MAINTENANCE_ROLE__`，Compose 也设置了 `ARTIFACT_MAINTENANCE_DB_USER`。但 README 的独立容器手工授权示例只替换 MIGRATOR / APP / WORKER，没有替换 MAINTENANCE；直接复制该命令会把字面角色名 `__MAINTENANCE_ROLE__` 留在脚本中，实际 maintenance 用户可能无法执行 purge。

建议补齐：
`-e 's/__MAINTENANCE_ROLE__/artifact_maintenance/g'`
并同步修正 `deploy/db-roles.sql` 文件头的示例命令。此项是部署文档问题，不影响当前 CI 与自动化角色测试；但生产执行前必须修正操作命令。

## 6. 本次未执行的事项

- 未执行 0004 / 0005 生产迁移。
- 未启动、重启或更改任何生产服务。
- 未更改 Generator registry 状态。
- 未开放 `POST /v1/artifacts` 或 GenerationJob Create。
- 未启动 Worker / SeeCode。
- 未实施 AS-3、AS-4 或 AI-PM 侧改动。

## 7. 最终建议

1. 先单独修正文档状态与 Maintenance grants 命令（DOC-01 / DOC-02），不改业务代码。
2. 继续保持当前 fail-closed，不安排生产迁移，直到 LMMS 与 File Service 两个 P0 有可验证的跨服务关闭证据。
3. 当进入部署窗口时，按 §3.1、§3.2 顺序收集 AS-1 / AS-2 数据库证据；不得把测试库证据代替生产证据。
4. AS-3 仍保持 Deferred；只有在其独立契约、挑战评审与实施计划完成后再启动。
