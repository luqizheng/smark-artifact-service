//! 渲染子进程沙箱（M-2 §3 服务端执行隔离的实现）。
//!
//! 依据 ART-03 seecode-adapter-design-v1 §3（上游实现：ai-pm ADR-VIZ-002 M-2 §3）：
//! SeeCode 从「浏览器内渲染器」改为「服务端出图器」后，**原浏览器安全假设不能
//! 继承**——必须在服务端另建执行隔离。
//!
//! 本模块落实六项中的四项（其余两项由调用方保证）：
//!
//! | M-2 条款 | 本模块 |
//! |---|---|
//! | §3.1 输入不可信 + **传参不经命令行** | ✅ spec 一律经 **stdin** 传入，绝不拼进 argv |
//! | §3.2 零凭据继承 | ✅ `env_clear()` + 白名单（不含任何 token / DSN） |
//! | §3.4 资源限额 | ✅ 执行超时（kill_on_drop 强杀）+ 输出字节上限 + 独立临时目录 |
//! | §3.3 零外网 | 由部署侧网络策略保证（本模块不做网络命名空间隔离） |
//! | §3.5 独立静态化检查 | 由 `crate::svg_safety::inspect_svg` 保证（渲染后、入库前） |
//! | §3.6 检查失败拒绝入库 | 由执行循环保证（safety 检查失败 → Job failed，不提交 Output） |
//!
//! **安全责任分工**：本模块防「渲染进程越权」，不防「渲染输出恶意」——
//! 后者是静态化检查的职责（SVG 格式 ≠ 内容安全）。

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

/// 沙箱配置。
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// 可执行文件（Node）。
    pub program: String,
    /// 参数（**不得包含业务数据**——数据一律走 stdin）。
    pub args: Vec<String>,
    /// 执行超时（超时即强杀子进程）。
    pub timeout: Duration,
    /// stdout 字节上限（防渲染器刷屏撑爆内存/磁盘）。
    pub max_output_bytes: usize,
    /// 额外允许的环境变量（如渲染器必需的 `NODE_PATH`）。
    pub extra_env: Vec<(String, String)>,
    /// 复用调用方指定的工作目录（多步渲染需要在同目录内传递中间产物）。
    ///
    /// `Some` 时本模块**不创建也不清理**该目录——生命周期由调用方负责。
    pub workdir: Option<PathBuf>,
    /// 把 `stdin_payload` 写入工作目录下的该文件名（CLI 需要文件路径入参时用）。
    ///
    /// 设此项时不再通过 stdin 管道传输（数据仍不进命令行，只是换载体为文件）。
    pub payload_file: Option<String>,
}

/// 沙箱错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SandboxError {
    /// 执行超时（子进程已被强制终止）。
    #[error("render sandbox timeout after {0:?}")]
    Timeout(Duration),
    /// 输出超出上限。
    #[error("render output exceeded {limit} bytes")]
    OutputTooLarge { limit: usize },
    /// 子进程以非零码退出（附截断后的 stderr）。
    #[error("render exited with {code}: {stderr}")]
    NonZeroExit { code: i32, stderr: String },
    /// 沙箱自身失败（创建临时目录 / spawn 失败等）。
    #[error("render sandbox failure: {0}")]
    Internal(String),
}

/// 执行一次渲染子进程。
///
/// `stdin_payload` 是待渲染的输入（VizSpec JSON 等业务数据）——
/// **必须走 stdin，绝不拼进命令行**（防命令注入，M-2 §3.1）。
///
/// 退出后临时目录被清理（成功与失败都清理）。
pub async fn run_sandboxed(
    config: &SandboxConfig,
    stdin_payload: &[u8],
) -> Result<Vec<u8>, SandboxError> {
    // 工作目录：调用方指定则复用（多步渲染），否则自建并在退出时清理
    let owned_temp;
    let workdir: &std::path::Path = match &config.workdir {
        Some(path) => path.as_path(),
        None => {
            owned_temp = TempWorkdir::create()?;
            owned_temp.path()
        }
    };

    // payload 落文件（CLI 以文件路径入参时使用；数据仍不进命令行）
    if let Some(filename) = &config.payload_file {
        let target = workdir.join(filename);
        std::fs::write(&target, stdin_payload).map_err(|e| {
            SandboxError::Internal(format!("write payload {} failed: {e}", target.display()))
        })?;
    }

    let mut cmd = tokio::process::Command::new(&config.program);
    cmd.args(&config.args)
        .current_dir(workdir)
        // 子进程随 future 一起被 drop 时强杀（超时路径依赖此设置）
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if config.payload_file.is_some() {
        cmd.stdin(std::process::Stdio::null());
    }

    apply_env_whitelist(&mut cmd, workdir, &config.extra_env);

    let mut child = cmd
        .spawn()
        .map_err(|e| SandboxError::Internal(format!("spawn {} failed: {e}", config.program)))?;

    // §3.1：输入经 stdin；写完即关闭（子进程读到 EOF 才继续）
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_payload)
            .await
            .map_err(|e| SandboxError::Internal(format!("write stdin failed: {e}")))?;
        drop(stdin);
    }

    let timeout = config.timeout;
    let output = match tokio::time::timeout(timeout, async move { child.wait_with_output().await })
        .await
    {
        Ok(result) => result.map_err(|e| SandboxError::Internal(format!("wait failed: {e}")))?,
        // 超时：async 块被 drop → child 被 drop → kill_on_drop 强杀
        Err(_) => return Err(SandboxError::Timeout(timeout)),
    };

    if output.stdout.len() > config.max_output_bytes {
        return Err(SandboxError::OutputTooLarge {
            limit: config.max_output_bytes,
        });
    }
    if !output.status.success() {
        let stderr = truncate_utf8(&output.stderr, 2_000);
        return Err(SandboxError::NonZeroExit {
            code: output.status.code().unwrap_or(-1),
            stderr,
        });
    }

    Ok(output.stdout)
}

/// §3.2 环境变量白名单：`env_clear()` 后只注入渲染必需项。
///
/// 明确**不注入**：API 凭据（Bearer/JWT）、数据库 DSN、文件服务 token、
/// 任何业务权限上下文——渲染器不需要知道调用者是谁。
fn apply_env_whitelist(
    cmd: &mut tokio::process::Command,
    workdir: &std::path::Path,
    extra: &[(String, String)],
) {
    cmd.env_clear();

    // 基础运行时（Windows 缺 SystemRoot 时连 cmd 都起不来）
    const BASE: &[&str] = &["PATH", "SystemRoot", "SYSTEMROOT", "WINDIR"];
    for key in BASE {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }
    // 临时目录与 HOME 指向沙箱工作目录（隔离写盘）
    let workdir_string = workdir.to_string_lossy().to_string();
    cmd.env("TMPDIR", &workdir_string)
        .env("TMP", &workdir_string)
        .env("TEMP", &workdir_string)
        .env("HOME", &workdir_string);

    for (key, value) in extra {
        // 调用方追加项同样受控：拒绝看起来像凭据的键名（防御性）
        let lower = key.to_ascii_lowercase();
        if lower.contains("token")
            || lower.contains("secret")
            || lower.contains("password")
            || lower.contains("database_url")
            || lower.contains("api_key")
        {
            continue;
        }
        cmd.env(key, value);
    }
}

/// 沙箱临时工作目录（Drop 即清理）。
struct TempWorkdir {
    path: PathBuf,
}

impl TempWorkdir {
    fn create() -> Result<Self, SandboxError> {
        let path = std::env::temp_dir()
            .join("artifact-worker-render")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&path).map_err(|e| {
            SandboxError::Internal(format!("create workdir {} failed: {e}", path.display()))
        })?;
        Ok(Self { path })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempWorkdir {
    fn drop(&mut self) {
        // 尽力删除；失败不 panic（临时目录残留由上层定期清理兜底）
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 按字节截断并保证 UTF-8 边界安全。
fn truncate_utf8(bytes: &[u8], limit: usize) -> String {
    if bytes.len() <= limit {
        return String::from_utf8_lossy(bytes).to_string();
    }
    let mut end = limit;
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    format!("{}…(truncated)", String::from_utf8_lossy(&bytes[..end]))
}

/// 平台相关的测试/工具命令（返回 program + args）。
///
/// 让单测在 Windows 与 Unix 上都能构造「读 stdin」「睡一会」「打印环境」。
#[cfg(test)]
pub fn platform_command(script: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        (
            "cmd".to_string(),
            vec!["/C".to_string(), script.to_string()],
        )
    }
    #[cfg(not(windows))]
    {
        ("sh".to_string(), vec!["-c".to_string(), script.to_string()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(script: &str) -> SandboxConfig {
        let (program, args) = platform_command(script);
        SandboxConfig {
            program,
            args,
            timeout: Duration::from_secs(10),
            max_output_bytes: 1024 * 1024,
            extra_env: vec![],
            workdir: None,
            payload_file: None,
        }
    }

    /// Windows 的 `more` 会把 stdin 原样转到 stdout；Unix 用 `cat`。
    #[cfg(windows)]
    const ECHO_STDIN: &str = "more";
    #[cfg(not(windows))]
    const ECHO_STDIN: &str = "cat";

    /// 睡 5 秒（超时用例把 timeout 设短）。
    #[cfg(windows)]
    const SLEEP: &str = "ping -n 6 127.0.0.1 > nul";
    #[cfg(not(windows))]
    const SLEEP: &str = "sleep 5";

    /// 打印全部环境变量。
    #[cfg(windows)]
    const PRINT_ENV: &str = "set";
    #[cfg(not(windows))]
    const PRINT_ENV: &str = "env";

    #[tokio::test]
    async fn stdin_payload_is_passed_to_child() {
        let output = run_sandboxed(&config(ECHO_STDIN), b"{\"spec\":\"hello\"}")
            .await
            .expect("应成功");
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("hello"), "stdin 内容应到达子进程：{text}");
    }

    #[tokio::test]
    async fn timeout_kills_child() {
        let mut cfg = config(SLEEP);
        cfg.timeout = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let result = run_sandboxed(&cfg, b"").await;
        assert_eq!(result, Err(SandboxError::Timeout(cfg.timeout)));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "超时应立即返回而不是等子进程睡完"
        );
    }

    #[tokio::test]
    async fn credentials_are_not_inherited() {
        // 父进程里的凭据类变量不得出现在子进程环境中（§3.2）。
        // edition 2024 起 set_var/remove_var 为 unsafe：测试进程内串行执行、
        // 无并发读者，注入与清理均在此块内完成。
        // SAFETY: 见上。
        unsafe {
            std::env::set_var("AI_PM_TEST_SECRET_TOKEN", "must-not-leak");
            std::env::set_var("DATABASE_URL", "postgres://leak");
        }
        let output = run_sandboxed(&config(PRINT_ENV), b"")
            .await
            .expect("应成功");
        let env_text = String::from_utf8_lossy(&output).to_ascii_lowercase();
        assert!(
            !env_text.contains("must-not-leak"),
            "凭据不得继承到渲染子进程"
        );
        assert!(!env_text.contains("postgres://leak"), "数据库 DSN 不得继承");
        unsafe {
            std::env::remove_var("AI_PM_TEST_SECRET_TOKEN");
            std::env::remove_var("DATABASE_URL");
        }
    }

    #[tokio::test]
    async fn secret_looking_extra_env_is_rejected() {
        let mut cfg = config(PRINT_ENV);
        cfg.extra_env = vec![
            ("MY_API_TOKEN".to_string(), "leaked-value".to_string()),
            ("NODE_PATH_SAFE".to_string(), "ok-value".to_string()),
        ];
        let output = run_sandboxed(&cfg, b"").await.expect("应成功");
        let env_text = String::from_utf8_lossy(&output);
        assert!(!env_text.contains("leaked-value"), "凭据类键名应被过滤");
        assert!(env_text.contains("ok-value"), "非凭据键名应放行");
    }

    #[tokio::test]
    async fn oversized_output_is_rejected() {
        let mut cfg = config(ECHO_STDIN);
        cfg.max_output_bytes = 16;
        let payload = vec![b'x'; 4096];
        let result = run_sandboxed(&cfg, &payload).await;
        assert!(
            matches!(result, Err(SandboxError::OutputTooLarge { limit: 16 })),
            "超大输出应被拒绝：{result:?}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported() {
        let result = run_sandboxed(&config("exit 3"), b"").await;
        match result {
            Err(SandboxError::NonZeroExit { code, .. }) => assert_eq!(code, 3),
            other => panic!("应报非零退出：{other:?}"),
        }
    }

    #[test]
    fn workdir_is_cleaned_up_on_drop() {
        // 直接测 Drop 语义（不经 run_sandboxed）——避免并行用例的计数竞态
        let path = {
            let temp = TempWorkdir::create().expect("创建临时目录");
            let path = temp.path().to_path_buf();
            assert!(path.exists(), "临时目录应存在");
            path
        };
        assert!(!path.exists(), "丢出作用域后临时目录应被清理");
    }

    #[test]
    fn truncate_respects_utf8_boundary() {
        let text = "中文内容".as_bytes();
        let truncated = truncate_utf8(text, 4);
        // 4 字节落在第二个汉字中间 → 截到 3 字节（一个完整汉字）
        assert!(truncated.starts_with('中'));
        assert!(truncated.ends_with("…(truncated)"));
    }
}
