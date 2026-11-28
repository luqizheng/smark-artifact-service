//! Artifact Service 独立 Web Server 入口。
//!
//! 子命令：
//! - `serve`（默认）：启动 HTTP 服务（运行角色，无 DDL 权限）。
//! - `migrate`：执行数据库迁移（部署流水线以 migrator 角色运行，与运行角色分离）。
//!
//! 生产装配：fail-closed 委托验证 + 协议阻断存储端口（无任何运行时绕过开关）。

use artifact_api::{Config, router, verifier_from_config};
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new(&config.log_filter)?)
        .json()
        .init();

    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "serve".to_string());
    match command.as_str() {
        "migrate" => {
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .connect(&config.database_url)
                .await?;
            sqlx::migrate!("../../migrations").run(&pool).await?;
            tracing::info!("database migrations applied");
            Ok(())
        }
        "serve" => serve(config).await,
        other => Err(anyhow::anyhow!(
            "unknown command '{other}' (expected 'serve' or 'migrate')"
        )),
    }
}

async fn serve(config: Config) -> anyhow::Result<()> {
    let pool = PgPoolOptions::new()
        .max_connections(config.db_max_connections)
        .connect(&config.database_url)
        .await?;
    let verifier = verifier_from_config(&config.delegation_issuers);
    // WP-AS-05：file-service 配置齐备 → 真实适配器；否则 Version 链路
    // 保持 503 协议阻断（诚实模式，/v1/ready 如实上报）。
    let (storage, storage_mode) = artifact_api::storage_from_config(config.file_service.as_ref());
    let state = artifact_api::app_state_with_storage(pool, verifier, storage);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let delegation_mode = if config.delegation_issuers.is_empty() {
        "fail-closed (no trusted issuer configured)".to_string()
    } else {
        format!(
            "ai-pm JWKS ({} trusted issuer[s])",
            config.delegation_issuers.len()
        )
    };
    tracing::info!(
        addr = %config.bind,
        version = state.build_info.version,
        git_sha = state.build_info.git_sha,
        delegation = %delegation_mode,
        "artifact-api listening (storage: {storage_mode})"
    );
    axum::serve(listener, router(state)).await?;
    Ok(())
}
