//! 委托撤销查询端口（WP 契约 v0.2 §5.2/§5.3，Q1 裁决：写路径在线撤销）。
//!
//! 写动作执行前对 jti 做一次在线撤销查询；**查询不可用 = fail-closed 503
//! `REVOCATION_UNAVAILABLE`**（语义：暂不可判定，而非凭证无效），禁止任何
//! "查询失败视为未撤销" 的 fail-open 分支；禁止进程级缓存撤销结果
//! （撤销即时性优先于时延，契约 §5.3 第 3 条）。读动作不查询（残余窗口
//! ≤ TTL，Q1 明示接受）。
//!
//! 生产实现（AI-PM `GET /internal/delegation-revocations/{jti}` 客户端）
//! 属实施计划任务 1.7/AS-4 接线范畴；端点落地前，生产装配使用
//! [`FailClosedRevocationChecker`]：无条件不可用 = 写路径保持关闭。

use async_trait::async_trait;

/// 撤销查询失败（网络/超时/非 200）。**绝不**用 Ok(false) 表达"查不到"。
#[derive(Debug, thiserror::Error)]
#[error("delegation revocation check unavailable: {0}")]
pub struct RevocationCheckUnavailable(pub String);

#[async_trait]
pub trait DelegationRevocationChecker: Send + Sync {
    /// `Ok(true)` = 已撤销（写路径 → 403）；`Ok(false)` = 未撤销；
    /// `Err` = 暂不可判定（写路径 → 503 REVOCATION_UNAVAILABLE）。
    async fn is_revoked(&self, issuer: &str, jti: &str)
    -> Result<bool, RevocationCheckUnavailable>;
}

/// 默认实现：无条件"不可判定"。在 AI-PM 撤销查询端点（§5.2）上线前，
/// 生产装配使用本实现 —— 写路径因此保持 fail-closed（503），不会误放行。
#[derive(Debug, Clone, Copy, Default)]
pub struct FailClosedRevocationChecker;

impl FailClosedRevocationChecker {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl DelegationRevocationChecker for FailClosedRevocationChecker {
    async fn is_revoked(
        &self,
        _issuer: &str,
        _jti: &str,
    ) -> Result<bool, RevocationCheckUnavailable> {
        Err(RevocationCheckUnavailable(
            "revocation endpoint is not configured (contract §5.2 endpoint pending \
             task 1.7 / AS-4 wiring); write paths stay fail-closed"
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fail-closed 默认：绝不返回 Ok —— 不可判定绝不折算成"未撤销"。
    #[tokio::test]
    async fn default_checker_never_allows_writes() {
        let checker = FailClosedRevocationChecker::new();
        let result = checker
            .is_revoked("ai-pm", "0195aaaa-bbbb-7ccc-dddd-eeeeffff0000")
            .await;
        assert!(
            result.is_err(),
            "unavailable must be an error, never Ok(false)"
        );
    }
}
