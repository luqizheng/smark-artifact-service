//! Worker 执行循环（WP-AS-06，ART-03B-02 核心切片）。
//!
//! 流程（ART-03A §4/§8）：
//! ```text
//! claim（能力表匹配） → 输入校验（diagram.type 清单） → 渲染（沙箱，
//! heartbeat 周期续租） → SVG 静态化把闸 → file-service 上传（WP-AS-05
//! upload_object，恒带 tid）→ inspect 重算 sha256（"校验"半程）→
//! commit_output（0006 单事务：Output + pending→ready + Job/Attempt
//! succeeded + audit）。
//! ```
//!
//! 失败语义：
//! * `Rejected` / `Failed` / `UnsafeOutput`（确定性）→ `fail(retryable=false)`；
//! * `Unavailable` / 超时 / 上传 / 提交失败（暂时性）→ `fail(retryable=true)`
//!   （最终是否重试由数据库白名单裁定）；
//! * `CancelRequested`（heartbeat 或 commit 阶段）→ 停止执行 +
//!   `confirm_cancelled`；渲染 future 被 drop 时沙箱子进程经 `kill_on_drop`
//!   被强杀（ART-03B-02 强杀语义的进程层）；
//! * `StaleAttempt`（租约失效/Job 非 running）→ 静默放弃（DB 已收尾）；
//! * commit 失败时补偿删除本 Worker 上传的对象（D-W05-3：补偿仅用于
//!   AS 自身上传的输出对象）。
//!
//! 心跳信号经 `mpsc` 通知主执行流；`DeadlineExceeded` 表示 Attempt 已由
//! 数据库按超时收尾（无需 fail 上报）。

use std::sync::Arc;
use std::time::Duration;

use artifact_application::{
    AdapterCapabilities, CommitOutputOutcome, FailOutcome, GenerationOutputFacts, StoragePort,
    WorkerCoordinationPort,
};
use artifact_domain::{ClaimedJob, DomainError};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};

use crate::config::ExecutionSettings;
use crate::seecode_adapter::{
    GeneratorAdapter, SEECODE_ADAPTER_VERSION, SEECODE_GENERATOR_ID, SeeCodeAdapter,
};

/// 执行循环轮询间隔（与 claim 窗口解耦；claim 自身有 SKIP LOCKED 去重）。
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 心跳信号。
#[derive(Debug)]
enum HeartbeatSignal {
    /// 取消请求已登记：停止执行并 confirm_cancelled。
    Cancel,
    /// deadline 已超：数据库已按超时收尾，无需上报。
    DeadlineExceeded,
    /// 租约已不属于本调用方：静默放弃。
    Stale,
    /// 心跳通道自身故障（DB 故障）：放弃本次执行（租约会过期收尾）。
    Failed(String),
}

/// 执行器（一个 Worker 实例一份；全部依赖已装配）。
pub struct Executor {
    coordination: Arc<dyn WorkerCoordinationPort>,
    storage: Arc<dyn StoragePort>,
    adapter: Arc<dyn GeneratorAdapter>,
    worker_id: String,
    lease_seconds: i32,
    /// 对象面出站路由 tid（D-W06-3：配置常量，见 config 文档）。
    lmms_tid: i64,
}

impl Executor {
    /// 直接装配（测试注入假渲染器/假存储用；生产走 [`build_executor`]）。
    pub fn new(
        coordination: Arc<dyn WorkerCoordinationPort>,
        storage: Arc<dyn StoragePort>,
        adapter: Arc<dyn GeneratorAdapter>,
        worker_id: &str,
        lease_seconds: i32,
        lmms_tid: i64,
    ) -> Self {
        Self {
            coordination,
            storage,
            adapter,
            worker_id: worker_id.to_owned(),
            lease_seconds,
            lmms_tid,
        }
    }
}

/// 从执行面配置装配执行器；装配失败（配置非法）返回错误（启动硬错）。
pub fn build_executor(
    coordination: Arc<dyn WorkerCoordinationPort>,
    settings: &ExecutionSettings,
    worker_id: &str,
) -> anyhow::Result<Executor> {
    let tokens = artifact_infrastructure::file_service::service_jwt::ServiceJwtMinter::new(
        artifact_infrastructure::file_service::service_jwt::ServiceJwtConfig {
            token_service_url: settings.file_token_service_url.clone(),
            bootstrap_token: settings.file_bootstrap_token.clone(),
            request_timeout: Duration::from_secs(10),
            refresh_skew: artifact_infrastructure::file_service::service_jwt::DEFAULT_REFRESH_SKEW,
        },
    )
    .map_err(|e| anyhow::anyhow!("service-jwt minter config invalid: {e}"))?;
    let storage: Arc<dyn StoragePort> = Arc::new(
        artifact_infrastructure::file_service::storage::FileServiceStorage::new(
            artifact_infrastructure::file_service::storage::FileServiceConfig {
                file_service_url: settings.file_service_url.clone(),
                upload_max_bytes: 16 * 1024 * 1024,
                request_timeout: Duration::from_secs(30),
            },
            tokens,
        )
        .map_err(|e| anyhow::anyhow!("file-service storage config invalid: {e}"))?,
    );
    let adapter: Arc<dyn GeneratorAdapter> =
        Arc::new(SeeCodeAdapter::new(settings.seecode_cli_path.clone()));
    tracing::debug!(
        generator = %adapter.generator_id(),
        adapter_version = %adapter.adapter_version(),
        "generator adapter assembled"
    );
    Ok(Executor::new(
        coordination,
        storage,
        adapter,
        worker_id,
        settings.lease_seconds,
        settings.file_lmms_tid,
    ))
}

impl Executor {
    /// 本 Worker 申报的能力（与 migrations 0006 钉住的注册表
    /// adapter_version 一致；claim v2 以此等值匹配）。
    pub fn capabilities() -> AdapterCapabilities {
        AdapterCapabilities::from([(
            SEECODE_GENERATOR_ID.to_string(),
            SEECODE_ADAPTER_VERSION.to_string(),
        )])
    }

    /// 执行循环：轮询 claim → 执行；shutdown future 完成即退出。
    pub async fn run_loop(&self, shutdown: impl std::future::Future<Output = ()> + Send) {
        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    tracing::info!(worker_id = %self.worker_id, "execution loop stopped (shutdown)");
                    return;
                }
                _ = tick.tick() => {
                    self.claim_and_execute().await;
                }
            }
        }
    }

    pub async fn claim_and_execute(&self) {
        let capabilities = Self::capabilities();
        match self
            .coordination
            .claim(&self.worker_id, self.lease_seconds, &capabilities)
            .await
        {
            Ok(Some(job)) => {
                if let Err(e) = self.execute_job(&job).await {
                    tracing::warn!(job_id = %job.job_id, error = %e, "job execution failed to report");
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "claim failed; will retry"),
        }
    }

    /// 执行单个 Job：渲染（并发心跳）→ 上传 → 校验 → 单事务提交。
    async fn execute_job(&self, job: &ClaimedJob) -> Result<(), DomainError> {
        tracing::info!(
            job_id = %job.job_id,
            attempt = job.attempt_number,
            generator = %job.generator_id,
            "job claimed"
        );

        // 心跳协程：租约 1/3 周期续租，取消/超时/失租以信号通知主流程。
        let (signal_tx, mut signal_rx) = mpsc::unbounded_channel::<HeartbeatSignal>();
        let hb_coordination = self.coordination.clone();
        let hb_job = job.clone();
        let hb_seconds = self.lease_seconds;
        let heartbeat_handle = tokio::spawn(async move {
            let period = Duration::from_secs((hb_seconds / 3).max(1) as u64);
            let mut tick = interval(period);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            tick.tick().await; // interval 首拍立即完成——跳过
            loop {
                tick.tick().await;
                match hb_coordination
                    .heartbeat(
                        hb_job.job_id,
                        hb_job.attempt_id,
                        &hb_job.lease_token,
                        hb_seconds,
                    )
                    .await
                {
                    Ok(artifact_application::HeartbeatOutcome::Renewed) => {}
                    Ok(artifact_application::HeartbeatOutcome::CancelRequested) => {
                        let _ = signal_tx.send(HeartbeatSignal::Cancel);
                    }
                    Ok(artifact_application::HeartbeatOutcome::DeadlineExceeded) => {
                        let _ = signal_tx.send(HeartbeatSignal::DeadlineExceeded);
                        return;
                    }
                    Ok(artifact_application::HeartbeatOutcome::Stale) => {
                        let _ = signal_tx.send(HeartbeatSignal::Stale);
                        return;
                    }
                    Err(e) => {
                        let _ = signal_tx.send(HeartbeatSignal::Failed(e.to_string()));
                        return;
                    }
                }
            }
        });

        // 渲染与心跳并发：取消/超时路径 drop 渲染 future → 沙箱子进程
        // kill_on_drop 强杀（强杀语义的进程层，ART-03B-02）。
        let adapter = self.adapter.clone();
        let input = job.input_payload.clone();
        let render = async move { adapter.render(&input).await };
        tokio::pin!(render);
        let render_result = tokio::select! {
            result = &mut render => Some(result),
            signal = signal_rx.recv() => {
                heartbeat_handle.abort();
                match signal {
                    Some(HeartbeatSignal::Cancel) => {
                        tracing::info!(job_id = %job.job_id, "cancel requested; aborting render");
                        self.handle_cancel(job).await;
                        return Ok(());
                    }
                    Some(HeartbeatSignal::DeadlineExceeded) => {
                        tracing::warn!(job_id = %job.job_id, "attempt deadline exceeded; database finalized the attempt");
                        return Ok(());
                    }
                    Some(HeartbeatSignal::Stale) => {
                        tracing::warn!(job_id = %job.job_id, "lease no longer belongs to this worker; abandoning");
                        return Ok(());
                    }
                    Some(HeartbeatSignal::Failed(message)) => {
                        tracing::warn!(job_id = %job.job_id, error = %message, "heartbeat channel failed; abandoning (lease expiry will finalize)");
                        return Ok(());
                    }
                    None => {
                        tracing::warn!(job_id = %job.job_id, "heartbeat channel closed; abandoning");
                        return Ok(());
                    }
                }
            }
        };
        if !heartbeat_handle.is_finished() {
            heartbeat_handle.abort();
        }

        let svg = match render_result {
            Some(Ok(svg)) => svg,
            Some(Err(e)) => return self.report_render_failure(job, e).await,
            None => unreachable!("select resolved via signal branch already"),
        };

        // 静态化把闸（A3A-07）在执行器边界：任何 GeneratorAdapter 实现的产物
        // 都必须过闸；失败 → 确定性 fail（SVG_SAFETY_REJECTED，不重试）。
        let svg_text = match String::from_utf8(svg) {
            Ok(text) => text,
            Err(_) => {
                let message = "render output is not valid UTF-8".to_string();
                return self
                    .report_render_failure(
                        job,
                        crate::seecode_adapter::AdapterError::Failed(message),
                    )
                    .await;
            }
        };
        let report = crate::svg_safety::inspect_svg(&svg_text);
        if !report.is_clean() {
            return self
                .report_render_failure(
                    job,
                    crate::seecode_adapter::AdapterError::UnsafeOutput(report.summary()),
                )
                .await;
        }

        self.submit_output(job, svg_text.into_bytes()).await
    }

    /// 渲染失败上报（确定性错误不重试；暂时性错误请求重试）。
    async fn report_render_failure(
        &self,
        job: &ClaimedJob,
        error: crate::seecode_adapter::AdapterError,
    ) -> Result<(), DomainError> {
        use crate::seecode_adapter::AdapterError;
        // 错误码与重试语义（0003 `is_retryable_generation_error` 白名单只认
        // GENERATION_TIMEOUT / STORAGE_UNAVAILABLE）：确定性错误用稳定业务码
        // 不重试；暂时性错误映射到白名单码（细节在诊断摘要里保留）。
        let (code, retryable) = match &error {
            AdapterError::Rejected(_) => ("INPUT_REJECTED", false),
            AdapterError::Failed(_) => ("RENDER_FAILED", false),
            AdapterError::UnsafeOutput(_) => ("SVG_SAFETY_REJECTED", false),
            AdapterError::Unavailable(_) => ("STORAGE_UNAVAILABLE", true),
        };
        let summary = sanitize_message(&error.to_string());
        tracing::warn!(job_id = %job.job_id, code, retryable, "render failed");
        match self
            .coordination
            .fail(
                job.job_id,
                job.attempt_id,
                &job.lease_token,
                code,
                Some(&summary),
                retryable,
            )
            .await
        {
            Ok(outcome) => {
                if outcome == FailOutcome::CancelRequested {
                    self.handle_cancel(job).await;
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// 取消收尾：Worker 确认停止后数据库才落 cancelled。
    async fn handle_cancel(&self, job: &ClaimedJob) {
        match self
            .coordination
            .confirm_cancelled(job.job_id, job.attempt_id, &job.lease_token)
            .await
        {
            Ok(artifact_application::ConfirmCancelOutcome::Cancelled) => {
                tracing::info!(job_id = %job.job_id, "cancellation confirmed");
            }
            Ok(outcome) => {
                tracing::warn!(job_id = %job.job_id, outcome = ?outcome, "cancellation confirm returned non-cancelled outcome");
            }
            Err(e) => {
                tracing::warn!(job_id = %job.job_id, error = %e, "confirm_cancelled failed (recovery will finalize)");
            }
        }
    }

    /// 上传 → 校验 → 单事务提交（ART-03A §8）。
    async fn submit_output(&self, job: &ClaimedJob, svg: Vec<u8>) -> Result<(), DomainError> {
        // 上传（服务身份代理；恒带 tid —— F-24 防线）。
        let object_id = match self
            .storage
            .upload_object(
                self.lmms_tid,
                bytes::Bytes::from(svg.clone()),
                "diagram.svg",
                Some("image/svg+xml"),
            )
            .await
        {
            Ok(object_id) => object_id,
            Err(e) => {
                return self
                    .report_output_failure(job, "STORAGE_UNAVAILABLE", true, &e.to_string())
                    .await;
            }
        };

        // 校验半程：下载重算 sha256（file-service 无 hash attestation）。
        let inspection = match self.storage.inspect_object(self.lmms_tid, &object_id).await {
            Ok(inspection) => inspection,
            Err(e) => {
                self.compensate_delete(&object_id).await;
                return self
                    .report_output_failure(job, "STORAGE_UNAVAILABLE", true, &e.to_string())
                    .await;
            }
        };
        let Some(sha256) = inspection.sha256.clone() else {
            self.compensate_delete(&object_id).await;
            let message = "storage inspection did not produce sha256".to_string();
            return self
                .report_output_failure(job, "STORAGE_UNAVAILABLE", true, &message)
                .await;
        };

        // finalize 近似（LMMS 无 finalize 协议——no-op，P1 差异记录在案）。
        let finalized = match self
            .storage
            .copy_or_finalize_object(self.lmms_tid, &object_id)
            .await
        {
            Ok(id) => id,
            Err(e) => {
                self.compensate_delete(&object_id).await;
                return self
                    .report_output_failure(job, "STORAGE_UNAVAILABLE", true, &e.to_string())
                    .await;
            }
        };

        let manifest = serde_json::json!({
            "object_id": finalized,
            "size_bytes": inspection.size_bytes,
            "sha256": sha256,
            "content_type": inspection.detected_mime,
            "sanitizer_policy_version": crate::svg_safety::SANITIZER_POLICY_VERSION,
            "input_sha256": job.input_sha256,
        });
        let facts = GenerationOutputFacts {
            object_id: finalized.clone(),
            sha256,
            size_bytes: inspection.size_bytes,
            mime_type: inspection.detected_mime.clone(),
            format: "svg".to_string(),
            manifest,
        };

        match self
            .coordination
            .commit_output(job.job_id, job.attempt_id, &job.lease_token, &facts)
            .await
        {
            Ok(CommitOutputOutcome::Committed(version_id)) => {
                tracing::info!(
                    job_id = %job.job_id,
                    version = %version_id,
                    object = %finalized,
                    "generation output committed"
                );
                Ok(())
            }
            Ok(CommitOutputOutcome::CancelRequested) => {
                self.compensate_delete(&object_id).await;
                tracing::info!(job_id = %job.job_id, "cancel raced the commit; object compensated");
                self.handle_cancel(job).await;
                Ok(())
            }
            Ok(CommitOutputOutcome::Stale) => {
                tracing::warn!(job_id = %job.job_id, "commit rejected as stale; object compensated");
                self.compensate_delete(&object_id).await;
                Ok(())
            }
            Err(e) => {
                self.compensate_delete(&object_id).await;
                self.report_output_failure(job, "STORAGE_UNAVAILABLE", true, &e.to_string())
                    .await
            }
        }
    }

    /// 补偿删除（幂等：404 视为已删除；失败仅告警——孤儿由后续清理兜底）。
    async fn compensate_delete(&self, object_id: &str) {
        if let Err(e) = self.storage.delete_object(self.lmms_tid, object_id).await {
            tracing::warn!(object = %object_id, error = %e, "compensation delete failed; orphan cleanup will handle");
        }
    }

    async fn report_output_failure(
        &self,
        job: &ClaimedJob,
        code: &str,
        retryable: bool,
        reason: &str,
    ) -> Result<(), DomainError> {
        let summary = sanitize_message(&format!("{code}: {reason}"));
        tracing::warn!(job_id = %job.job_id, code, retryable, "output submission failed");
        match self
            .coordination
            .fail(
                job.job_id,
                job.attempt_id,
                &job.lease_token,
                code,
                Some(&summary),
                retryable,
            )
            .await
        {
            Ok(outcome) => {
                if outcome == FailOutcome::CancelRequested {
                    self.handle_cancel(job).await;
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// 诊断摘要入库前清洗（复用应用层 sanitize；不携带输入正文）。
fn sanitize_message(message: &str) -> String {
    artifact_application::sanitize_diagnostic(Some(message)).unwrap_or_default()
}
