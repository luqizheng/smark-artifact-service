//! 审计事件接收端口（ART-01 §8.3）。

use artifact_domain::AuditEvent;
use async_trait::async_trait;

#[async_trait]
pub trait AuditSink: Send + Sync {
    /// 记录审计事件；实现应当尽力而为（审计失败不得改变业务结果，
    /// 但必须留下日志）。数据库层另有触发器保证 append-only。
    async fn record(&self, event: AuditEvent) -> Result<(), String>;
}
