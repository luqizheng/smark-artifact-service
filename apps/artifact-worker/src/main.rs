//! Artifact Worker 独立入口（ART-03B-01 协调 + WP-AS-06 执行循环）。
//!
//! `run` 始终执行协调维护（租约恢复 + 输入清理）；当执行面配置齐备
//! （`ARTIFACT_WORKER_SEECODE_CLI_PATH` + `ARTIFACT_WORKER_FILE_*` 五项
//! all-or-nothing，见 config）时并行运行 SeeCode 执行循环：
//!   * Worker 只申报与 vendored Adapter 版本一致的能力（claim v2 等值匹配）；
//!   * 渲染经沙箱（零凭据 / stdin 传参 / 超时强杀），SVG 产物经静态化把闸；
//!   * 输出经 file-service 上传 + sha256 重算校验 + 0006 单事务提交；
//!   * 未配置执行面时不申报任何能力，领不到 Job（ART-03B-01 原姿态）。
//!
//! Worker 不继承用户 delegation，不读取 API 用户身份凭证；Node/SeeCode
//! vendor 只进入 Worker 镜像（ART-03 部署边界）。
//!
//! 子命令：
//!   `run`（默认）：协调维护 + （可选）执行循环，SIGTERM / Ctrl-C 后退出。
//!   `recover-once`：执行一次租约恢复 + 输入清理（供运维/定时任务调用）。
//!   `ping`：仅验证独立 DB 连接与角色可用。
//!
//! 日志不记录 lease token、输入正文或数据库连接串。

use artifact_application::run_coordination_maintenance;
use artifact_infrastructure::postgres::PgWorkerCoordination;
use artifact_worker::config::WorkerConfig;
use artifact_worker::{execute, seecode_adapter};

use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use tokio::time::{MissedTickBehavior, interval};

enum Command {
    Run,
    RecoverOnce,
    Ping,
}

impl Command {
    fn parse(arg: &str) -> anyhow::Result<Self> {
        match arg {
            "run" => Ok(Command::Run),
            "recover-once" => Ok(Command::RecoverOnce),
            "ping" => Ok(Command::Ping),
            other => Err(anyhow::anyhow!(
                "unknown command '{other}' (expected 'run', 'recover-once', or 'ping')"
            )),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 先解析命令：未知命令不读取配置、不连接数据库，直接以非零状态退出。
    let command = Command::parse(&std::env::args().nth(1).unwrap_or_else(|| "run".to_string()))?;
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

    match command {
        Command::Ping => {
            sqlx::query("SELECT 1").execute(&pool).await?;
            tracing::info!(worker_id = %config.worker_id, "worker database connection ok");
        }
        Command::RecoverOnce => {
            let (recovered, purged) = run_coordination_maintenance(coordination.as_ref()).await?;
            tracing::info!(recovered, purged, "recovery and purge completed");
        }
        Command::Run => run_loop(config, coordination).await?,
    }
    pool.close().await;
    Ok(())
}

async fn run_loop(
    config: WorkerConfig,
    coordination: Arc<PgWorkerCoordination>,
) -> anyhow::Result<()> {
    // 执行面（WP-AS-06）：五项配置齐备才装配执行器；缺省保持协调-only。
    let executor = match &config.execution {
        Some(settings) => {
            let executor = execute::build_executor(
                coordination.clone() as Arc<dyn artifact_application::WorkerCoordinationPort>,
                settings,
                &config.worker_id,
            )?;
            tracing::info!(
                worker_id = %config.worker_id,
                seecode_cli = %settings.seecode_cli_path,
                lease_seconds = settings.lease_seconds,
                adapter_version = seecode_adapter::SEECODE_ADAPTER_VERSION,
                "artifact-worker started with seecode execution loop (sandboxed vendored CLI)"
            );
            Some(executor)
        }
        None => {
            tracing::info!(
                worker_id = %config.worker_id,
                maintenance_interval_ms = config.maintenance_interval.as_millis() as u64,
                "artifact-worker started in coordination-only mode: no execution settings, jobs will not be claimed"
            );
            None
        }
    };

    let maintenance = async {
        let mut tick = interval(config.maintenance_interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match run_coordination_maintenance(coordination.as_ref()).await {
                // 恢复扫描的 advisory lock 被别的 Worker 持有时返回 0，属于正常情况。
                Ok((0, 0)) => {}
                Ok((recovered, purged)) => {
                    tracing::info!(recovered, purged, "coordination maintenance applied");
                }
                Err(e) => tracing::warn!(error = %e, "coordination maintenance failed; will retry"),
            }
        }
    };
    tokio::pin!(maintenance);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    tokio::select! {
        _ = &mut shutdown => {
            tracing::info!(worker_id = %config.worker_id, "shutdown signal received; artifact-worker stopped");
        }
        _ = &mut maintenance => {}
        _ = async {
            if let Some(executor) = &executor {
                executor.run_loop(std::future::pending::<()>()).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {}
    }
    Ok(())
}

/// SIGTERM（容器停止）或 Ctrl-C（本地前台运行）。
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signals) => {
                signals.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
