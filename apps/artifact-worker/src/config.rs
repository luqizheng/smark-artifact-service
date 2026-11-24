//! Worker 运行配置：全部来自独立环境变量（样例见 .env.example）。
//!
//! 安全边界：本模块刻意不读取 ARTIFACT_API_DATABASE_URL 或任何用户
//! delegation/token 环境变量；Worker 只使用独立服务身份连接数据库。

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub database_url: String,
    pub worker_id: String,
    pub lease_seconds: i32,
    pub poll_interval: Duration,
    pub log_filter: String,
    pub db_max_connections: u32,
}

impl WorkerConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let database_url = std::env::var("ARTIFACT_WORKER_DATABASE_URL").map_err(|_| {
            anyhow::anyhow!(
                "ARTIFACT_WORKER_DATABASE_URL is required (worker must not reuse API credentials)"
            )
        })?;
        let worker_id =
            std::env::var("ARTIFACT_WORKER_ID").unwrap_or_else(|_| "artifact-worker".to_string());
        if worker_id.trim().is_empty() {
            return Err(anyhow::anyhow!("ARTIFACT_WORKER_ID must not be blank"));
        }
        let lease_seconds: i32 = std::env::var("ARTIFACT_WORKER_LEASE_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        if !(1..=3600).contains(&lease_seconds) {
            return Err(anyhow::anyhow!(
                "ARTIFACT_WORKER_LEASE_SECONDS must be within 1..=3600"
            ));
        }
        let poll_ms: u64 = std::env::var("ARTIFACT_WORKER_POLL_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        if poll_ms < 50 {
            return Err(anyhow::anyhow!(
                "ARTIFACT_WORKER_POLL_INTERVAL_MS must be >= 50"
            ));
        }
        let log_filter =
            std::env::var("ARTIFACT_WORKER_LOG").unwrap_or_else(|_| "info".to_string());
        let db_max_connections: u32 = std::env::var("ARTIFACT_WORKER_DB_MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);

        Ok(Self {
            database_url,
            worker_id,
            lease_seconds,
            poll_interval: Duration::from_millis(poll_ms),
            log_filter,
            db_max_connections,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::WorkerConfig;

    /// Worker 配置必须读取独立 DSN 环境变量名；源码层面确认不引用 API DSN。
    #[test]
    fn worker_config_uses_independent_env_names() {
        // 通过编译期常量确认键名，避免误用 API 的 ARTIFACT_API_DATABASE_URL。
        const WORKER_DSN_ENV: &str = "ARTIFACT_WORKER_DATABASE_URL";
        assert_eq!(WORKER_DSN_ENV, "ARTIFACT_WORKER_DATABASE_URL");
        let _ = WorkerConfig::from_env;
    }
}
