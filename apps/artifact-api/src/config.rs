//! 运行配置（全部来自环境变量，样例见仓库根 `.env.example`）。

use std::net::SocketAddr;
use std::time::Duration;

/// 受信委托签发方（WP 契约 v0.2 §4：候选 B）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct DelegationIssuerConfig {
    /// `iss` claim 精确值（如 `ai-pm`）。
    pub issuer: String,
    /// 固定 JWKS URL。https 强制；仅 127.0.0.1/localhost/[::1] 的 http 回环
    /// 豁免（本机 dev/compose 联调）。不存在任何其他明文传输豁免。
    pub jwks_url: String,
    /// 兼容保留字段：必须**精确等于**契约固定受众 `artifact-service`
    /// （缺省即该值）。任何其他值（含空白、大小写变体）一律启动失败
    /// （WP-03 裁决：受众是契约常量，不是可配置信任根——R2-F-06 修复）。
    #[serde(default = "default_delegation_audience")]
    pub audience: String,
}

fn default_delegation_audience() -> String {
    artifact_infrastructure::delegation::ai_pm::DELEGATION_AUDIENCE.to_string()
}

impl DelegationIssuerConfig {
    /// 校验信任根配置（P0-03 + Q6 裁决 + WP-03 audience 固定）：清单为空 =
    /// 维持全拒（合法姿态）；非空时**恰一项 active issuer**（契约 Q6：V1 单
    /// issuer；LMMS 未来上线候选 A 时按清单机制平滑过渡，届时需复审）；
    /// issuer/jwks_url 非空、jwks_url 为 https（或本机回环 http）、audience
    /// 精确等于契约常量（不做 trim/大小写归一——绕过即失败）。
    pub fn validate_all(list: &[DelegationIssuerConfig]) -> anyhow::Result<()> {
        if !list.is_empty() && list.len() != 1 {
            anyhow::bail!(
                "exactly one active trusted issuer is required (contract v0.2 Q6:                  single active issuer); got {}",
                list.len()
            );
        }
        let mut seen = std::collections::HashSet::new();
        for issuer in list {
            if issuer.issuer.trim().is_empty() {
                anyhow::bail!("delegation issuer id must not be empty");
            }
            if !seen.insert(issuer.issuer.clone()) {
                anyhow::bail!("duplicate delegation issuer '{}'", issuer.issuer);
            }
            if issuer.audience != artifact_infrastructure::delegation::ai_pm::DELEGATION_AUDIENCE {
                anyhow::bail!(
                    "delegation issuer '{}' audience must be exactly '{}' \
                     (the contract audience is a fixed constant and must not be \
                     overridden); got {:?}",
                    issuer.issuer,
                    artifact_infrastructure::delegation::ai_pm::DELEGATION_AUDIENCE,
                    issuer.audience
                );
            }
            validate_jwks_url(&issuer.jwks_url)?;
        }
        Ok(())
    }
}

fn validate_jwks_url(raw: &str) -> anyhow::Result<()> {
    let url = url::Url::parse(raw)
        .map_err(|e| anyhow::anyhow!("delegation jwks_url '{raw}' is not a valid URL: {e}"))?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1");
    match url.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        _ => anyhow::bail!(
            "delegation jwks_url must be https (loopback http is allowed only for \
             127.0.0.1/localhost/[::1]); got '{raw}'"
        ),
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub database_url: String,
    pub log_filter: String,
    pub db_max_connections: u32,
    pub request_timeout: Duration,
    /// 受信委托签发方清单。**空 = 维持全拒（fail-closed 默认）**：
    /// `ClosedDelegationVerifier` 装配，一切业务请求 401。
    pub delegation_issuers: Vec<DelegationIssuerConfig>,
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
        let delegation_issuers: Vec<DelegationIssuerConfig> =
            match std::env::var("ARTIFACT_API_DELEGATION_ISSUERS") {
                Ok(raw) if !raw.trim().is_empty() => {
                    let parsed: Vec<DelegationIssuerConfig> =
                        serde_json::from_str(&raw).map_err(|e| {
                            anyhow::anyhow!(
                                "invalid ARTIFACT_API_DELEGATION_ISSUERS (expected JSON array of \
                         {{issuer, jwks_url, audience?}}): {e}"
                            )
                        })?;
                    DelegationIssuerConfig::validate_all(&parsed)?;
                    parsed
                }
                _ => Vec::new(),
            };
        Ok(Self {
            bind,
            database_url,
            log_filter,
            db_max_connections,
            request_timeout: Duration::from_secs(timeout_secs),
            delegation_issuers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_issuer_list_is_accepted_and_means_fail_closed() {
        let issuers: Vec<DelegationIssuerConfig> = serde_json::from_str("[]").unwrap();
        DelegationIssuerConfig::validate_all(&issuers).unwrap();
        assert_eq!(issuers.len(), 0);
    }

    #[test]
    fn https_issuer_config_roundtrips() {
        let issuers: Vec<DelegationIssuerConfig> = serde_json::from_str(
            r#"[{"issuer":"ai-pm","jwks_url":"https://ai-pm.internal/.well-known/jwks.json"}]"#,
        )
        .unwrap();
        assert_eq!(issuers[0].audience, "artifact-service");
        DelegationIssuerConfig::validate_all(&issuers).unwrap();
    }

    #[test]
    fn plaintext_remote_jwks_url_is_rejected() {
        let issuers: Vec<DelegationIssuerConfig> = serde_json::from_str(
            r#"[{"issuer":"ai-pm","jwks_url":"http://ai-pm.internal/.well-known/jwks.json"}]"#,
        )
        .unwrap();
        assert!(DelegationIssuerConfig::validate_all(&issuers).is_err());
    }

    #[test]
    fn loopback_http_jwks_url_is_allowed() {
        let issuers: Vec<DelegationIssuerConfig> = serde_json::from_str(
            r#"[{"issuer":"ai-pm","jwks_url":"http://127.0.0.1:9090/jwks.json"}]"#,
        )
        .unwrap();
        DelegationIssuerConfig::validate_all(&issuers).unwrap();
    }

    #[test]
    fn multiple_active_issuers_are_rejected_single_issuer_q6() {
        let issuers: Vec<DelegationIssuerConfig> = serde_json::from_str(
            r#"[{"issuer":"ai-pm","jwks_url":"https://a/jwks"},
                {"issuer":"other","jwks_url":"https://b/jwks"}]"#,
        )
        .unwrap();
        let err = DelegationIssuerConfig::validate_all(&issuers).unwrap_err();
        assert!(
            err.to_string()
                .contains("exactly one active trusted issuer")
        );
    }

    #[test]
    fn duplicate_issuers_are_rejected() {
        let issuers: Vec<DelegationIssuerConfig> = serde_json::from_str(
            r#"[{"issuer":"ai-pm","jwks_url":"https://a/jwks"},
                {"issuer":"ai-pm","jwks_url":"https://b/jwks"}]"#,
        )
        .unwrap();
        assert!(DelegationIssuerConfig::validate_all(&issuers).is_err());
    }

    // ── WP-03（R2-F-06 修复）：契约受众是常量，配置不可覆盖 ────────────────

    #[test]
    fn non_contract_audience_config_fails_startup() {
        // PoC P8（R2-F-06）的配置形态：audience=coder-micro-services 必须启动失败。
        let issuers: Vec<DelegationIssuerConfig> = serde_json::from_str(
            r#"[{"issuer":"ai-pm","jwks_url":"https://a/jwks","audience":"coder-micro-services"}]"#,
        )
        .unwrap();
        let err = DelegationIssuerConfig::validate_all(&issuers).unwrap_err();
        assert!(
            err.to_string()
                .contains("must be exactly 'artifact-service'")
        );
    }

    #[test]
    fn empty_audience_config_fails_startup() {
        let issuers: Vec<DelegationIssuerConfig> = serde_json::from_str(
            r#"[{"issuer":"ai-pm","jwks_url":"https://a/jwks","audience":""}]"#,
        )
        .unwrap();
        assert!(DelegationIssuerConfig::validate_all(&issuers).is_err());
    }

    #[test]
    fn audience_cannot_be_bypassed_via_case_or_whitespace() {
        // 大小写 / 空白变体一律拒绝：不做归一化，精确相等是唯一通过路径。
        for bad in [
            "Artifact-Service",
            "artifact-service ",
            " artifact-service",
            "ARTIFACT-SERVICE",
        ] {
            let raw =
                format!(r#"[{{"issuer":"ai-pm","jwks_url":"https://a/jwks","audience":"{bad}"}}]"#);
            let issuers: Vec<DelegationIssuerConfig> = serde_json::from_str(&raw).unwrap();
            assert!(
                DelegationIssuerConfig::validate_all(&issuers).is_err(),
                "audience {bad:?} must not pass validation"
            );
        }
    }

    #[test]
    fn explicit_contract_audience_is_accepted() {
        let issuers: Vec<DelegationIssuerConfig> = serde_json::from_str(
            r#"[{"issuer":"ai-pm","jwks_url":"https://a/jwks","audience":"artifact-service"}]"#,
        )
        .unwrap();
        DelegationIssuerConfig::validate_all(&issuers).unwrap();
        assert_eq!(
            issuers[0].audience,
            artifact_infrastructure::delegation::ai_pm::DELEGATION_AUDIENCE
        );
    }
}
