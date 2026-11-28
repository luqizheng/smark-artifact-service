//! SeeCode Generator Adapter（WP-AS-06；上游实现：ai-pm `viz_seecode_adapter`
//! 去 pm-* 依赖移植，ART-03 §2 资产拆分表执行）。
//!
//! - 只以子进程方式调用 `node tools/seecode/scripts/seecode.mjs`
//! - 只产出 **SVG**（HTML 中间产物不对外投递）
//! - 全程经 [`crate::render_sandbox`]（零凭据 / 输入不进命令行 / 超时 / 输出上限）
//! - 产物由执行循环在提交边界经 [`crate::svg_safety::inspect_svg`] 静态化把闸
//!   （闸在执行器边界而非适配器内部：任何 `GeneratorAdapter` 实现都无法绕过）
//!
//! ## 两步渲染
//!
//! SeeCode CLI 的形态是 `render`（spec → HTML）再 `export`（HTML → SVG）。
//! 两步必须在**同一个沙箱工作目录**内完成（中间 HTML 要传递），
//! 因此本适配器自管临时目录并复用给两次沙箱调用。
//!
//! ## 输入契约（A3A-22 / ART-02 开放项 #21）
//!
//! Adapter 接收的 `input` 是已通过 AS 信封层 allowlist（`job_input`）的
//! JSON 对象；本模块再加一层 `diagram.type` 合法性检查——类型清单由
//! vendored `tools/seecode/schemas/*.schema.json` 生成并由单测锁定（清单与
//! schemas 目录严格一致）。逐字段 JSON Schema 校验（jsonschema 依赖）为
//! 后续增强；CLI 自身对非法 spec 返回 `ok:false`（渲染失败 → Job failed），
//! 沙箱保证注入面。

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::render_sandbox::{SandboxConfig, SandboxError, run_sandboxed};

/// 注册表/能力表两侧共享的 Adapter 版本契约常量（migrations 0006 钉住
/// `generator_registry.adapter_version`；claim v2 以此等值匹配）。
pub const SEECODE_ADAPTER_VERSION: &str = "svg-as-1";

/// generator_id（注册表 seed）。
pub const SEECODE_GENERATOR_ID: &str = "seecode";

/// diagram.type 合法清单（与 vendored schemas 目录 1:1，单测锁定）。
pub const DIAGRAM_TYPES: &[&str] = &[
    "bar",
    "common",
    "exploded",
    "fishbone",
    "gantt",
    "graph",
    "heatmap",
    "isometric-plan",
    "isometric",
    "journey",
    "kanban",
    "layers",
    "line",
    "loop",
    "matrix",
    "nested",
    "polar",
    "process",
    "pyramid",
    "quadrant",
    "radar",
    "sankey",
    "scatter",
    "sequence",
    "stages",
    "story-map",
    "timeline",
    "tree",
    "treemap",
    "venn",
    "wardley",
    "waterfall",
];

/// Adapter 执行错误（调用方映射为受控 fail 语义）。
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// 输入被拒绝（确定性错误：不重试）。
    #[error("input rejected: {0}")]
    Rejected(String),
    /// 执行失败（确定性：CLI/渲染层报错，不重试）。
    #[error("render failed: {0}")]
    Failed(String),
    /// 暂时性失败（超时/资源：可重试）。
    #[error("render unavailable: {0}")]
    Unavailable(String),
    /// 产物未通过静态化检查（确定性：不重试）。
    #[error("svg safety check failed: {0}")]
    UnsafeOutput(String),
}

/// Generator Adapter 抽象（执行循环只面向本 trait，便于测试注入假渲染器）。
#[async_trait::async_trait]
pub trait GeneratorAdapter: Send + Sync {
    fn generator_id(&self) -> &'static str;
    fn adapter_version(&self) -> &'static str;
    /// 渲染并静态化检查；成功返回 SVG 字节（已通过 inspect_svg）。
    async fn render(&self, input: &serde_json::Value) -> Result<Vec<u8>, AdapterError>;
}

/// SeeCode Adapter。
pub struct SeeCodeAdapter {
    /// `tools/seecode/scripts/seecode.mjs` 的路径。
    cli: PathBuf,
    /// 单次子进程执行超时。
    timeout: Duration,
    /// stdout 上限。
    max_output_bytes: usize,
}

impl SeeCodeAdapter {
    /// 以默认路径（`tools/seecode/scripts/seecode.mjs`）构造。
    pub fn new(cli: impl Into<PathBuf>) -> Self {
        Self {
            cli: cli.into(),
            timeout: Duration::from_secs(60),
            max_output_bytes: 8 * 1024 * 1024,
        }
    }

    fn base_config(&self, args: Vec<String>, workdir: PathBuf) -> SandboxConfig {
        SandboxConfig {
            program: "node".to_string(),
            args,
            timeout: self.timeout,
            max_output_bytes: self.max_output_bytes,
            extra_env: vec![(
                "SEECODE_FONT_CACHE".to_string(),
                font_cache_dir().to_string_lossy().into_owned(),
            )],
            workdir: Some(workdir),
            payload_file: None,
        }
    }

    /// 跑一步 CLI，返回 stdout（JSON 单行）。
    async fn run_cli(
        &self,
        args: Vec<String>,
        workdir: &Path,
        payload_file: Option<(&str, &[u8])>,
    ) -> Result<Vec<u8>, SandboxError> {
        let mut config = self.base_config(args, workdir.to_path_buf());
        let payload: Vec<u8> = match payload_file {
            Some((filename, bytes)) => {
                config.payload_file = Some(filename.to_string());
                bytes.to_vec()
            }
            None => Vec::new(),
        };
        run_sandboxed(&config, &payload).await
    }
}

/// SeeCode CLI 的字体字节缓存目录（宿主视角的稳定路径）。
///
/// Google Fonts 对同一 css2 URL 每次请求返回字节不同的子集，会把「同输入
/// 必须同输出」的渲染契约打破（回归基线的前提）。CLI 端按 URL 缓存字体
/// 字节；而沙箱把子进程的 TMPDIR/TMP/TEMP/HOME 全部映射进每次渲染即焚的
/// workdir，所以必须用 extra_env 把缓存目录显式钉在宿主临时目录上。
fn font_cache_dir() -> PathBuf {
    std::env::temp_dir().join("seecode-font-cache")
}

/// 从 CLI 的 JSON 单行输出里取 `out` 字段（相对/绝对路径都接受）。
fn parse_out_path(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let line = text.lines().find(|l| l.trim_start().starts_with('{'))?;
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    value
        .get("out")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn map_sandbox_error(error: SandboxError) -> AdapterError {
    match error {
        SandboxError::Timeout(duration) => {
            AdapterError::Unavailable(format!("render timeout after {}s", duration.as_secs()))
        }
        SandboxError::OutputTooLarge { limit } => {
            AdapterError::Failed(format!("render output exceeded {limit} bytes"))
        }
        SandboxError::NonZeroExit { code, stderr } => {
            AdapterError::Failed(format!("renderer exited {code}: {stderr}"))
        }
        SandboxError::Internal(message) => AdapterError::Unavailable(message),
    }
}

#[async_trait::async_trait]
impl GeneratorAdapter for SeeCodeAdapter {
    fn generator_id(&self) -> &'static str {
        SEECODE_GENERATOR_ID
    }

    fn adapter_version(&self) -> &'static str {
        SEECODE_ADAPTER_VERSION
    }

    async fn render(&self, input: &serde_json::Value) -> Result<Vec<u8>, AdapterError> {
        // 输入契约：diagram.type 必须在 vendored schema 清单内（A3A-22）。
        let diagram_type = input
            .get("diagram")
            .and_then(|d| d.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or_default();
        if !DIAGRAM_TYPES.contains(&diagram_type) {
            return Err(AdapterError::Rejected(format!(
                "diagram.type '{diagram_type}' is not a supported SeeCode type"
            )));
        }

        let workdir = std::env::temp_dir()
            .join("artifact-worker-seecode")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&workdir).map_err(|e| {
            AdapterError::Unavailable(format!("create workdir {} failed: {e}", workdir.display()))
        })?;

        // 结果在 return 前读走；workdir 由本函数负责清理
        let result = self.render_two_steps(&workdir, input).await;
        let _ = std::fs::remove_dir_all(&workdir);
        result
    }
}

impl SeeCodeAdapter {
    /// 实际的两步渲染：render（spec.json → HTML）→ export（HTML → SVG）。
    async fn render_two_steps(
        &self,
        workdir: &Path,
        input: &serde_json::Value,
    ) -> Result<Vec<u8>, AdapterError> {
        let cli = self.cli.to_string_lossy().to_string();
        let spec_json = serde_json::to_vec(input)
            .map_err(|e| AdapterError::Failed(format!("serialize spec failed: {e}")))?;

        // ① render：spec.json → diagram.html（spec 经文件传入，不进命令行）
        let render_stdout = self
            .run_cli(
                vec![cli.clone(), "render".to_string(), "spec.json".to_string()],
                workdir,
                Some(("spec.json", &spec_json)),
            )
            .await
            .map_err(map_sandbox_error)?;

        let html_path = parse_out_path(&render_stdout).ok_or_else(|| {
            AdapterError::Failed(format!(
                "render returned no out path: {}",
                String::from_utf8_lossy(&render_stdout)
            ))
        })?;
        let html_name = Path::new(&html_path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .ok_or_else(|| {
                AdapterError::Failed("render returned a path with no file name".into())
            })?;

        // ② export：diagram.html → diagram.svg（纯 Node，无需浏览器）
        let _ = self
            .run_cli(
                vec![
                    cli,
                    "export".to_string(),
                    html_name,
                    "--formats".to_string(),
                    "svg".to_string(),
                ],
                workdir,
                None,
            )
            .await
            .map_err(map_sandbox_error)?;

        // ③ 读回 SVG（export 产物与 HTML 同名同目录）
        let svg_path = workdir.join(
            Path::new(&html_path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "diagram".to_string())
                + ".svg",
        );
        std::fs::read(&svg_path)
            .map_err(|e| AdapterError::Failed(format!("read {} failed: {e}", svg_path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_path_is_parsed_from_cli_json_line() {
        let stdout = br#"{"ok":true,"out":"C:\\tmp\\x\\attack.html","type":"architecture"}"#;
        assert_eq!(
            parse_out_path(stdout).as_deref(),
            Some("C:\\tmp\\x\\attack.html")
        );
    }

    #[test]
    fn failed_cli_output_yields_none() {
        let stdout = br#"{"ok":false,"problems":[{"code":"E_SPEC","at":"nodes[0]"}]}"#;
        assert_eq!(parse_out_path(stdout), None);
    }

    #[test]
    fn unsupported_diagram_type_is_rejected() {
        let adapter = SeeCodeAdapter::new("tools/seecode/scripts/seecode.mjs");
        let input = serde_json::json!({
            "schema": "smark.seecode.input",
            "schema_version": "1",
            "diagram": {"type": "vizspec-unknown", "title": "x"}
        });
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(adapter.render(&input));
        assert!(matches!(result, Err(AdapterError::Rejected(_))));
    }

    #[test]
    fn diagram_type_list_matches_vendored_schemas() {
        // 类型清单与 vendored schemas 目录 1:1（清单即契约；新增 schema 必须同步）。
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let schemas = std::path::Path::new(manifest_dir).join("../../tools/seecode/schemas");
        let mut expected: Vec<String> = std::fs::read_dir(&schemas)
            .unwrap_or_else(|e| {
                panic!("vendored schemas dir {} must exist: {e}", schemas.display())
            })
            .filter_map(|entry| {
                let name = entry.ok()?.file_name().to_string_lossy().to_string();
                name.strip_suffix(".schema.json").map(str::to_string)
            })
            .collect();
        expected.sort();
        let mut actual: Vec<&str> = DIAGRAM_TYPES.to_vec();
        actual.sort();
        assert_eq!(
            actual, expected,
            "DIAGRAM_TYPES must mirror tools/seecode/schemas"
        );
    }

    #[test]
    fn adapter_identity_is_stable() {
        let adapter = SeeCodeAdapter::new("x");
        assert_eq!(adapter.generator_id(), "seecode");
        assert_eq!(adapter.adapter_version(), SEECODE_ADAPTER_VERSION);
    }
}
