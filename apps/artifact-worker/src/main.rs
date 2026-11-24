//! Artifact Worker 独立入口（ART-03B-01）。
//!
//! 本切片只建立可启动、可连接数据库、可领取和协调 Job 的骨架：
//!   * 不实现 Generator Adapter（SeeCode 在 ART-03C）；
//!   * 不包含 Node / SeeCode vendor；
//!   * 不依赖 API 路由层或 AI-PM；
//!   * 不继承用户 delegation、不读取 API 用户身份凭证；
//!   * 领取到 Job 时不伪造成功：以稳定错误码上报 adapter 未实现（不可重试）。
//!
//! 子命令：
//!   `run`（默认）：启动协调循环。
//!   `recover-once`：执行一次租约恢复 + 输入清理（供运维/定时任务调用）。
//!   `ping`：仅验证独立 DB 连接与角色可用。
//!
//! 日志只记录 job_id / attempt_id / worker_id / 错误码；lease token 与输入正文
//! 永不进入日志、trace 或 error_message。

mod config;

use artifact_application::WorkerCoordinationPort;
use artifact_infrastructure::postgres::PgWorkerCoordination;
use config::WorkerConfig;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = WorkerConfig::from_env()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(&config.log_filter)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    // 独立连接池：只使用 Worker 专用 DSN，绝不读取 API 连接配置。
    let pool = PgPoolOptions::new()
        .max_connections(config.db_max_connections)
        .connect(&config.database_url)
        .await?;
    let coordination = Arc::new(PgWorkerCoordination { pool: pool.clone() });

    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "run".to_string());
    match command.as_str() {
        "ping" => {
            sqlx::query("SELECT 1").execute(&pool).await?;
            tracing::info!(worker_id = %config.worker_id, "worker database connection ok");
            Ok(())
        }
        "recover-once" => {
            let recovered = coordination.recover_expired().await?;
            let purged = coordination.purge_expired_input().await?;
            tracing::info!(recovered, purged, "recovery and purge completed");
            Ok(())
        }
        "run" => run_loop(config, coordination).await,
        other => Err(anyhow::anyhow!(
            "unknown command '{other}' (expected 'run', 'recover-once', or 'ping')"
        )),
    }
}

async fn run_loop(
    config: WorkerConfig,
    coordination: Arc<PgWorkerCoordination>,
) -> anyhow::Result<()> {
    tracing::info!(
        worker_id = %config.worker_id,
        lease_seconds = config.lease_seconds,
        poll_interval_ms = config.poll_interval.as_millis(),
        "artifact-worker started (coordination skeleton; generator adapter not implemented)"
    );

    loop {
        match coordination
            .claim(&config.worker_id, config.lease_seconds)
            .await
        {
            Ok(Some(claimed)) => {
                // 只记录非敏感标识；lease token 明文与 input_payload 绝不记录。
                tracing::info!(
                    job_id = %claimed.job_id,
                    attempt_id = %claimed.attempt_id,
                    attempt_number = claimed.attempt_number,
                    generator_id = %claimed.generator_id,
                    "job claimed"
                );
                // ART-03B-01 没有 Generator Adapter：绝不伪造成功。
                // 以稳定错误码上报，且不可重试（输入无效/能力缺失不是暂时故障）。
                let outcome = coordination
                    .fail(
                        claimed.job_id,
                        claimed.attempt_id,
                        &claimed.lease_token,
                        "GENERATOR_ADAPTER_NOT_IMPLEMENTED",
                        Some("ART-03B-01 coordination skeleton has no generator adapter"),
                        false,
                    )
                    .await;
                match outcome {
                    Ok(o) => tracing::info!(
                        job_id = %claimed.job_id,
                        attempt_id = %claimed.attempt_id,
                        outcome = ?o,
                        "attempt reported"
                    ),
                    Err(e) => tracing::warn!(
                        job_id = %claimed.job_id,
                        attempt_id = %claimed.attempt_id,
                        error = %e,
                        "failed to report attempt"
                    ),
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "claim failed; will retry"),
        }

        // 协调循环周期性执行恢复与输入清理（幂等；终态 Job 不会被重新执行）。
        if let Err(e) = coordination.recover_expired().await {
            tracing::warn!(error = %e, "recovery scan failed");
        }
        if let Err(e) = coordination.purge_expired_input().await {
            tracing::warn!(error = %e, "input purge failed");
        }
        sleep(config.poll_interval).await;
    }
}
