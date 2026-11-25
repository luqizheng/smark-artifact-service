# ART-03B-01 — Main 基线验证报告

- 日期: 2026-11-25
- 仓库: https://github.com/luqizheng/smark-artifact-service.git
- 结论: ✅ **基线确认通过**，可启动 ART-03B-02

## 1. Git 基线

| 项目 | 结果 | 状态 |
|---|---|---|
| `git fetch origin` | 成功 | ✅ |
| 工作区状态 | `git status --porcelain` 为空，工作区干净 | ✅ |
| 当前分支 | `main`（未发生切换，已在 main 上） | ✅ |
| `git pull --ff-only origin main` | 已是最新，无可拉取变更 | ✅ |
| PR #3 合并提交是否进入 main | `c11e79a` = `Merge pull request #3 from luqizheng/impl/art-03b-01-review-fixes`，且 `HEAD -> main, origin/main, origin/HEAD` 三者一致 | ✅ |
| **main HEAD SHA** | `c11e79a51a145ded026632630bb0b52aa44dc311` | 记录 |

## 2. 指定文件存在性

| 文件 / 目录 | 状态 |
|---|---|
| `migrations/0002_job_coordination.sql` | ✅ 存在 |
| `migrations/0003_job_coordination_fixes.sql` | ✅ 存在 |
| `docs/architecture/ART-01-V1-API-ERRATA-001.md` | ✅ 存在 |
| `docs/architecture/ART-03A-job-worker-svg-decisions-v1.md` | ✅ 存在 |
| `apps/artifact-worker/` | ✅ 存在（含 `Cargo.toml` 与 `src/`） |

附带确认：`migrations/0001_init.sql` 亦存在，三个迁移文件齐全。

## 3. CI 状态

| 项 | 值 |
|---|---|
| run | `45521725590` |
| 触发 | push → main（PR #3 合并提交） |
| 结果 | **completed / success** |
| 耗时 | 2m28s |
| 时间 | 2026-11-25T00:54:31Z |

main HEAD 对应 CI 已完成并通过，非"进行中"或"未知"状态。

## 4. 迁移校验一致性

`migrations.sha256` 与实际文件 sha256 实测对比：

| 文件 | migrations.sha256 记录值 | 实测值 | 一致 |
|---|---|---|---|
| `0001_init.sql` | `ea8acd44…434343a1c8`（完整：`ea8acd4490516a31700005838c18a41cb7ff41e81b62f549cb407f874343a1c8`） | 相同 | ✅ |
| `0002_job_coordination.sql` | `ce4e86408b38177c94ca06b7fbbff8a0ec04a527a0269ce35c5b35da4653fc9d` | 相同 | ✅ |
| `0003_job_coordination_fixes.sql` | `5c6429f5b8ad726038a8a6d051ce4ca2355bb606a29afcadb90687f23997669c` | 相同 | ✅ |

三个迁移文件均与校验文件一致，无任何漂移。

另注意到仓库中存在迁移守护测试：`tests/tests/migration_guard.rs` 与 `tests/tests/migration_invariants.rs`（本次未执行，仅记录存在）。

## 5. 红线确认

- 未重新合并 PR #3，未触碰其 Draft/Ready 状态。
- 未修改 Migration 0001 / 0002 / 0003。
- 工作区无未提交修改，不存在需要保护的现场。
- 本报告仅为基线验证产出，不含代码改动。

## 6. 结论与下一步

✅ 基线验证全部通过。下一步按 ART-03B-02 指令进入**只读审计**阶段，产出 Worker 执行进程生命周期管理的架构设计与实施计划，经设计评审确认后才开始代码实现。
