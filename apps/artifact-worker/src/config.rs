//! Worker 运行配置：全部来自独立环境变量（样例见 .env.example）。
//!
//! 安全边界：本模块不读取 ARTIFACT_API_DATABASE_URL 或任何用户
//! delegation/token 环境变量；Worker 只使用独立服务身份连接数据库。
//!
//! 取值不合法时直接报错，不静默回落默认值：写错的配置必须被看见。

use std::env::VarError;
use std::fmt;
use std::time::Duration;

pub const DATABASE_URL_ENV: &str = "ARTIFACT_WORKER_DATABASE_URL";
pub const WORKER_ID_ENV: &str = "ARTIFACT_WORKER_ID";
pub const MAINTENANCE_INTERVAL_MS_ENV: &str = "ARTIFACT_WORKER_MAINTENANCE_INTERVAL_MS";
pub const LOG_ENV: &str = "ARTIFACT_WORKER_LOG";
pub const DB_MAX_CONNECTIONS_ENV: &str = "ARTIFACT_WORKER_DB_MAX_CONNECTIONS";

pub const SEECODE_CLI_PATH_ENV: &str = "ARTIFACT_WORKER_SEECODE_CLI_PATH";
pub const FILE_SERVICE_URL_ENV: &str = "ARTIFACT_WORKER_FILE_SERVICE_URL";
pub const FILE_TOKEN_SERVICE_URL_ENV: &str = "ARTIFACT_WORKER_FILE_TOKEN_SERVICE_URL";
pub const FILE_BOOTSTRAP_TOKEN_ENV: &str = "ARTIFACT_WORKER_FILE_BOOTSTRAP_TOKEN";
pub const FILE_LMMS_TID_ENV: &str = "ARTIFACT_WORKER_FILE_LMMS_TID";
pub const LEASE_SECONDS_ENV: &str = "ARTIFACT_WORKER_LEASE_SECONDS";

const DEFAULT_MAINTENANCE_INTERVAL_MS: u64 = 10_000;
const MIN_MAINTENANCE_INTERVAL_MS: u64 = 1_000;
const DEFAULT_DB_MAX_CONNECTIONS: u32 = 4;
const DEFAULT_LEASE_SECONDS: i32 = 300;
const LEASE_SECONDS_RANGE: std::ops::RangeInclusive<i32> = 1..=3600;

/// 执行面配置（WP-AS-06：SeeCode Adapter + file-service 对象面）。
///
/// **五项 all-or-nothing**：任一出现而其余缺失 → 启动失败（硬错）；全缺 →
/// `None` → Worker 保持协调维护-only 模式（不申报能力、领不到 Job）。
#[derive(Clone)]
pub struct ExecutionSettings {
    /// `tools/seecode/scripts/seecode.mjs` 路径（node 由 PATH 解析）。
    pub seecode_cli_path: String,
    pub file_service_url: String,
    pub file_token_service_url: String,
    /// bootstrap 服务凭证（Debug 恒定脱敏）。
    pub file_bootstrap_token: String,
    /// 对象面出站路由的 LMMS 数字租户 tid（D-W06-3：单租户配置常量；
    /// Job 行持久化调用方 lmms_tid 属后续迁移开放项）。
    pub file_lmms_tid: i64,
    /// claim 租约秒数（heartbeat 按 1/3 周期续租）。
    pub lease_seconds: i32,
}

impl fmt::Debug for ExecutionSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionSettings")
            .field("seecode_cli_path", &self.seecode_cli_path)
            .field("file_service_url", &self.file_service_url)
            .field("file_token_service_url", &self.file_token_service_url)
            .field("file_bootstrap_token", &"<redacted>")
            .field("file_lmms_tid", &self.file_lmms_tid)
            .field("lease_seconds", &self.lease_seconds)
            .finish()
    }
}

#[derive(Clone)]
pub struct WorkerConfig {
    pub database_url: String,
    pub worker_id: String,
    /// 协调维护（租约恢复 + 输入清理）的执行间隔。
    pub maintenance_interval: Duration,
    pub log_filter: String,
    pub db_max_connections: u32,
    /// 执行面（WP-AS-06）；None = 协调维护-only。
    pub execution: Option<ExecutionSettings>,
}

/// DSN 含口令：Debug 输出脱敏，这样整个配置被打进日志也不会泄露凭据。
impl fmt::Debug for WorkerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerConfig")
            .field("database_url", &"[redacted]")
            .field("worker_id", &self.worker_id)
            .field("maintenance_interval", &self.maintenance_interval)
            .field("log_filter", &self.log_filter)
            .field("db_max_connections", &self.db_max_connections)
            .field("execution", &self.execution)
            .finish()
    }
}

/// 键值来源：环境变量，或测试提供的表。
pub type Lookup<'a> = &'a dyn Fn(&str) -> Result<String, VarError>;

impl WorkerConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(&|key| std::env::var(key))
    }

    pub fn from_lookup(lookup: Lookup<'_>) -> anyhow::Result<Self> {
        let database_url = required(lookup, DATABASE_URL_ENV)?;
        if database_url.trim().is_empty() {
            return Err(anyhow::anyhow!("{DATABASE_URL_ENV} must not be blank"));
        }
        let worker_id =
            optional(lookup, WORKER_ID_ENV)?.unwrap_or_else(|| "artifact-worker".to_string());
        if worker_id.trim().is_empty() || worker_id.len() > 128 {
            return Err(anyhow::anyhow!(
                "{WORKER_ID_ENV} must be 1..=128 characters"
            ));
        }
        let maintenance_ms: u64 = parse(
            lookup,
            MAINTENANCE_INTERVAL_MS_ENV,
            DEFAULT_MAINTENANCE_INTERVAL_MS,
        )?;
        if maintenance_ms < MIN_MAINTENANCE_INTERVAL_MS {
            return Err(anyhow::anyhow!(
                "{MAINTENANCE_INTERVAL_MS_ENV} must be >= {MIN_MAINTENANCE_INTERVAL_MS}"
            ));
        }
        let log_filter = optional(lookup, LOG_ENV)?.unwrap_or_else(|| "info".to_string());
        let db_max_connections: u32 =
            parse(lookup, DB_MAX_CONNECTIONS_ENV, DEFAULT_DB_MAX_CONNECTIONS)?;
        if db_max_connections == 0 {
            return Err(anyhow::anyhow!("{DB_MAX_CONNECTIONS_ENV} must be >= 1"));
        }

        let execution = execution_settings_from_lookup(lookup)?;
        Ok(Self {
            database_url,
            worker_id,
            maintenance_interval: Duration::from_millis(maintenance_ms),
            log_filter,
            db_max_connections,
            execution,
        })
    }
}

/// 五项 all-or-nothing（见 [`ExecutionSettings`]）。
fn execution_settings_from_lookup(lookup: Lookup<'_>) -> anyhow::Result<Option<ExecutionSettings>> {
    let cli = optional(lookup, SEECODE_CLI_PATH_ENV)?;
    let file_url = optional(lookup, FILE_SERVICE_URL_ENV)?;
    let token_url = optional(lookup, FILE_TOKEN_SERVICE_URL_ENV)?;
    let bootstrap = optional(lookup, FILE_BOOTSTRAP_TOKEN_ENV)?
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let tid_raw = optional(lookup, FILE_LMMS_TID_ENV)?;
    let provided = [
        cli.as_ref(),
        file_url.as_ref(),
        token_url.as_ref(),
        bootstrap.as_ref(),
        tid_raw.as_ref(),
    ]
    .iter()
    .filter(|v| v.is_some_and(|s| !s.trim().is_empty()))
    .count();
    if provided == 0 {
        return Ok(None);
    }
    if provided < 5 {
        return Err(anyhow::anyhow!(
            "{SEECODE_CLI_PATH_ENV}, {FILE_SERVICE_URL_ENV}, {FILE_TOKEN_SERVICE_URL_ENV},              {FILE_BOOTSTRAP_TOKEN_ENV} and {FILE_LMMS_TID_ENV} are all-or-nothing:              {provided}/5 set — execution stays disabled until all five are configured"
        ));
    }
    let seecode_cli_path = cli.unwrap().trim().to_owned();
    let file_service_url = file_url.unwrap().trim().to_owned();
    let file_token_service_url = token_url.unwrap().trim().to_owned();
    let file_bootstrap_token = bootstrap.unwrap();
    let file_lmms_tid: i64 = tid_raw
        .unwrap()
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("{FILE_LMMS_TID_ENV} must be a positive integer"))?;
    if file_lmms_tid <= 0 {
        return Err(anyhow::anyhow!("{FILE_LMMS_TID_ENV} must be positive"));
    }
    let lease_seconds: i32 = parse(lookup, LEASE_SECONDS_ENV, DEFAULT_LEASE_SECONDS)?;
    if !LEASE_SECONDS_RANGE.contains(&lease_seconds) {
        return Err(anyhow::anyhow!(
            "{LEASE_SECONDS_ENV} must be within {LEASE_SECONDS_RANGE:?}"
        ));
    }
    Ok(Some(ExecutionSettings {
        seecode_cli_path,
        file_service_url,
        file_token_service_url,
        file_bootstrap_token,
        file_lmms_tid,
        lease_seconds,
    }))
}

fn optional(lookup: Lookup<'_>, key: &str) -> anyhow::Result<Option<String>> {
    match lookup(key) {
        Ok(value) => Ok(Some(value)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(raw)) => {
            Err(anyhow::anyhow!("{key} is not valid unicode ({raw:?})"))
        }
    }
}

/// 错误信息只说明缺哪个变量，不回显配置值。
fn required(lookup: Lookup<'_>, key: &str) -> anyhow::Result<String> {
    optional(lookup, key)?.ok_or_else(|| {
        anyhow::anyhow!("{key} is required (the worker must use its own database role, never the API credentials)")
    })
}

fn parse<T: std::str::FromStr>(lookup: Lookup<'_>, key: &str, default: T) -> anyhow::Result<T> {
    match optional(lookup, key)? {
        Some(raw) => raw
            .trim()
            .parse::<T>()
            .map_err(|_| anyhow::anyhow!("{key} must be a valid number, got {raw:?}")),
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::{DATABASE_URL_ENV, MAINTENANCE_INTERVAL_MS_ENV, WORKER_ID_ENV, WorkerConfig};
    use std::collections::HashMap;
    use std::env::VarError;
    use std::time::Duration;

    const WORKER_DSN: (&str, &str) = (
        "ARTIFACT_WORKER_DATABASE_URL",
        "postgres://worker:secret-password@db:5432/artifact_service",
    );

    fn from(pairs: &[(&'static str, &'static str)]) -> anyhow::Result<WorkerConfig> {
        let map: HashMap<&'static str, &'static str> = pairs.iter().copied().collect();
        WorkerConfig::from_lookup(&|key| {
            map.get(key)
                .map(|value| value.to_string())
                .ok_or(VarError::NotPresent)
        })
    }

    #[test]
    fn requires_its_own_database_url() {
        // 只配置了 API 的 DSN：Worker 不会退而使用它。
        let err = from(&[(
            "ARTIFACT_API_DATABASE_URL",
            "postgres://api:api-password@db:5432/artifact_service",
        )])
        .unwrap_err();
        assert!(err.to_string().contains("ARTIFACT_WORKER_DATABASE_URL"));
        assert!(!err.to_string().contains("api-password"));

        let config = from(&[WORKER_DSN]).unwrap();
        assert_eq!(config.worker_id, "artifact-worker");
        assert_eq!(config.maintenance_interval, Duration::from_millis(10_000));
        assert_eq!(config.log_filter, "info");
        assert_eq!(config.db_max_connections, 4);
    }

    #[test]
    fn invalid_values_are_errors_not_defaults() {
        for (key, value) in [
            (MAINTENANCE_INTERVAL_MS_ENV, "1OO0"),
            (MAINTENANCE_INTERVAL_MS_ENV, "500"),
            ("ARTIFACT_WORKER_DB_MAX_CONNECTIONS", "0"),
            ("ARTIFACT_WORKER_DB_MAX_CONNECTIONS", "four"),
            (WORKER_ID_ENV, "   "),
        ] {
            let err = from(&[WORKER_DSN, (key, value)]).unwrap_err();
            assert!(err.to_string().contains(key), "{key}={value:?}: {err}");
        }
        let err = from(&[("ARTIFACT_WORKER_DATABASE_URL", "  ")]).unwrap_err();
        assert!(err.to_string().contains(DATABASE_URL_ENV));
    }

    #[test]
    fn debug_output_does_not_expose_the_database_url() {
        let rendered = format!("{:?}", from(&[WORKER_DSN]).unwrap());
        assert!(rendered.contains("database_url: \"[redacted]\""));
        assert!(!rendered.contains("secret-password"));
    }
}
