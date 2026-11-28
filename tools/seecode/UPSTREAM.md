# SeeCode（vendored）

**上游**：https://github.com/Aryanutkarsh/SeeCode
**版本**：0.6.3
**Git commit**：`9967db28e86e0e212d4ddf5a60f90fe62f3436c0`（2026-11-02）
**许可**：MIT（见 `LICENSE`）；捆绑编码器与字体见 `THIRD_PARTY_LICENSES.md`
**引入日期**：2026-11-12
**取用路径**：上游 `skills/seecode/` 目录整份复制

## 定位（ADR-VIZ-002 M-2 修订后）

**服务端 SVG 出图器，非核心运行依赖。**

- 只以**子进程**方式从渲染 Worker 调用（`node tools/seecode/scripts/seecode.mjs`）
- 只使用 `render`（产出 HTML）与 `export --formats svg`
- **业务领域不得直接依赖本目录**——唯一调用方是 Adapter 实现
- 渲染失败降级为表格视图，**Live 图与 Fallback 不受影响**
- **移除路径**：删除本目录 + 注销 Adapter 注册 → 回退 Fallback，无需其他改动

## 执行隔离（M-2 §3）

调用方必须经 `pm-infrastructure/src/viz_render_sandbox.rs`：
零凭据继承、输入经 stdin（不拼命令行）、超时强杀、输出上限、独立临时目录。

产物必须经 `pm-application/src/visualization/artifact_safety.rs` 的 `inspect_svg`
静态化检查后才允许入库（**SVG 格式 ≠ 内容安全**）。

## 本地修改

**当前无修改**（原样 vendor）。任何本地 patch 必须记录在此处，并在升级时重新评估。

| 日期 | 文件 | 改动 | 理由 |
|---|---|---|---|
| — | — | 无 | — |

## 升级流程

1. 取上游 tag / commit，对比 `skills/seecode/` 差异
2. 更新本文件的版本与 commit
3. 记录新增依赖与许可证变化
4. **必须**重跑双测试门禁：
   - `cargo test -p pm-application --test viz_artifact_safety`（真实产物 + 恶意 SVG）
   - `cargo test -p pm-application --lib visualization::adapter_contract`（八项契约断言）
5. 在渲染 Worker 镜像内验证 `node tools/seecode/scripts/seecode.mjs doctor`

**禁止**自动追随上游——每次升级都是一次经评估的依赖变更（ADR-VIZ-002 M-2 §2.3）。

## 已知约束

- 需要 **Node 20+**（仅渲染 Worker 镜像；API 服务与普通任务 Worker 不引入）
- 默认节点预算 ≤9 节点 / ≤12 边（`budget: faithful` 上限 24）——超预算需在 Adapter 编译层做 top-K 聚焦裁剪，并在产物页脚注明"显示 N/M 节点"
- SVG 导出纯 Node（**不需要 Chrome**）；PNG/GIF/MP4 需浏览器，本通道不产出
- 产物含内嵌 base64 字体，体积偏大（实测 4 节点 292KB）——受 `viz.render_output` 容量规则约束
