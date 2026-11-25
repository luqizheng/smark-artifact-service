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

const DEFAULT_MAINTENANCE_INTERVAL_MS: u64 = 10_000;
const MIN_MAINTENANCE_INTERVAL_MS: u64 = 1_000;
const DEFAULT_DB_MAX_CONNECTIONS: u32 = 4;

#[derive(Clone)]
pub struct WorkerConfig {
    pub database_url: String,
    pub worker_id: String,
    /// 协调维护（租约恢复 + 输入清理）的执行间隔。
    pub maintenance_interval: Duration,
    pub log_filter: String,
    pub db_max_connections: u32,
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

        Ok(Self {
            database_url,
            worker_id,
            maintenance_interval: Duration::from_millis(maintenance_ms),
            log_filter,
            db_max_connections,
        })
    }
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
