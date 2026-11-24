//! 运行配置（全部来自环境变量，样例见仓库根 `.env.example`）。

use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub database_url: String,
    pub log_filter: String,
    pub db_max_connections: u32,
    pub request_timeout: Duration,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let bind = std::env::var("ARTIFACT_API_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8110".to_string())
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid ARTIFACT_API_BIND: {e}"))?;
        let database_url = std::env::var("ARTIFACT_API_DATABASE_URL").map_err(|_| {
            anyhow::anyhow!("ARTIFACT_API_DATABASE_URL is required (see .env.example)")
        })?;
        let log_filter = std::env::var("ARTIFACT_API_LOG").unwrap_or_else(|_| "info".to_string());
        let db_max_connections = std::env::var("ARTIFACT_API_DB_MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8);
        let timeout_secs = std::env::var("ARTIFACT_API_REQUEST_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10u64);
        Ok(Self {
            bind,
            database_url,
            log_filter,
            db_max_connections,
            request_timeout: Duration::from_secs(timeout_secs),
        })
    }
}
