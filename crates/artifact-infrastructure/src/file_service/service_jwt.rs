//! 短时 file-service 服务令牌（WP-AS-05，最小移植 ai-pm
//! `digua_common_jwt::ServiceJwtCache` 的 mint 流程；**不引入 LMMS 私有仓库
//! 依赖**，AS 保持纯 crates.io 构建面）。
//!
//! # 契约（LMMS token-service `POST /tokens/service-jwt`，已核验）
//!
//! * 请求：`Authorization: Bearer <bootstrap>`，JSON `{"aud": target, "tid": tid}`；
//! * 响应：`{"access_token": ..., "expires_in": ...}`（RS256，`aud=target`、
//!   `tid=<tid>`）；`expires_in <= 0` 视为不可用（fail-closed）；
//! * 授权：签发侧 DB grant 表 `owner→audience`（`is_allowed`），缺 grant /
//!   disabled → 403；bootstrap 被拒 → 401。**两者都绝不重试、绝不折算为
//!   "无令牌继续"**（fail-closed，调用方得到 [`ServiceJwtError::AuthFailed`]）。
//!
//! # F-24 陷阱（LMMS `services/file.rs`）
//!
//! file-service 以 `scope.tenant_id().unwrap_or(1)` 解析租户：无 `tid` 的令牌
//! 会被静默当作租户 1。因此本客户端**恒定携带 tid mint**（per-(aud, tid)
//! 缓存），绝不签发或转发无 tid 令牌。
//!
//! # 与 JWKS 缓存的一致性
//!
//! 与 [`crate::delegation::jwks`] 相同的姿态：固定配置 URL、https 强制
//! （127.0.0.1/localhost/[::1] 回环 http 豁免）、显式超时、禁重定向、
//! 失败关闭。令牌到期（含 [`ServiceJwtConfig::refresh_skew`] 提前量）即重
//! mint；重 mint 失败 = 错误，**绝不返回过期令牌**。

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

/// AS 侧访问 LMMS file-service 的固定 audience（A3A-06 §5.2）。
pub const FILE_SERVICE_AUDIENCE: &str = "file-service";

/// mint 端点路径（LMMS token-service 固定路由）。
const MINT_PATH: &str = "/tokens/service-jwt";

/// 默认提前刷新量：到期前 60 秒即重 mint（对齐 ai-pm `refresh_skew`）。
pub const DEFAULT_REFRESH_SKEW: Duration = Duration::from_secs(60);

/// mint 客户端配置。`bootstrap_token` 在 [`Debug`] 中恒定脱敏。
#[derive(Clone)]
pub struct ServiceJwtConfig {
    /// LMMS token-service 基址（如 `http://127.0.0.1:8082`，回环 http 豁免见模块注释）。
    pub token_service_url: String,
    /// bootstrap 服务凭证（opaque；绝不写入日志/错误）。
    pub bootstrap_token: String,
    /// 单次 mint 请求超时。
    pub request_timeout: Duration,
    /// 到期前提前重 mint 的提前量。
    pub refresh_skew: Duration,
}

impl fmt::Debug for ServiceJwtConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceJwtConfig")
            .field("token_service_url", &self.token_service_url)
            .field("bootstrap_token", &"<redacted>")
            .field("request_timeout", &self.request_timeout)
            .field("refresh_skew", &self.refresh_skew)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceJwtError {
    /// bootstrap 被拒（401）或 audience grant 缺失（403）：fail-closed，
    /// 这是配置/授权面问题，重试无意义。
    #[error("service-jwt auth failed: {0}")]
    AuthFailed(String),
    /// 网络 / 5xx / 解析失败：可重试，但调用方必须停止当前操作
    /// （写路径上游映射为 503，绝不放行）。
    #[error("service-jwt unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Clone)]
struct CachedToken {
    token: Arc<str>,
    expires_at: std::time::Instant,
}

#[derive(Clone)]
pub struct ServiceJwtMinter {
    http: reqwest::Client,
    config: Arc<ServiceJwtConfig>,
    cache: Arc<RwLock<HashMap<(String, i64), CachedToken>>>,
}

impl fmt::Debug for ServiceJwtMinter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceJwtMinter")
            .field("config", &self.config)
            .field("cached_keys", &"<opaque>")
            .finish()
    }
}

impl ServiceJwtMinter {
    /// 构造并校验配置。URL 必须 https（或回环 http）；bootstrap 非空。
    pub fn new(config: ServiceJwtConfig) -> Result<Self, ServiceJwtError> {
        if config.bootstrap_token.trim().is_empty() {
            return Err(ServiceJwtError::AuthFailed(
                "bootstrap token must not be empty".into(),
            ));
        }
        validate_service_url(&config.token_service_url).map_err(ServiceJwtError::Unavailable)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.request_timeout)
            .build()
            .map_err(|e| ServiceJwtError::Unavailable(format!("http client build failed: {e}")))?;
        Ok(Self {
            http,
            config: Arc::new(config),
            cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// 取得（并按需 mint）`aud=file-service`、`tid=tid` 的短时服务令牌。
    /// 缓存命中（未进入提前刷新窗口）不发网络请求。
    pub async fn file_service_token(&self, tid: i64) -> Result<String, ServiceJwtError> {
        self.get(FILE_SERVICE_AUDIENCE, tid).await
    }

    /// 取得（并按需 mint）`(aud, tid)` 的短时服务令牌。
    async fn get(&self, aud: &str, tid: i64) -> Result<String, ServiceJwtError> {
        let key = (aud.to_string(), tid);
        {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.get(&key) {
                let now = std::time::Instant::now();
                let refresh_at = cached
                    .expires_at
                    .checked_sub(self.config.refresh_skew)
                    .unwrap_or(now);
                if now < refresh_at {
                    return Ok(cached.token.to_string());
                }
            }
        }
        // 快路径未命中：mint 在锁外执行（同键并发 mint 产生重复有效令牌，
        // 无害）；随后写锁内插入最新值。
        self.mint(aud, tid).await
    }

    async fn mint(&self, aud: &str, tid: i64) -> Result<String, ServiceJwtError> {
        let url = format!(
            "{}{MINT_PATH}",
            self.config.token_service_url.trim_end_matches('/')
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.config.bootstrap_token)
            .json(&serde_json::json!({ "aud": aud, "tid": tid }))
            .send()
            .await
            .map_err(|e| {
                ServiceJwtError::Unavailable(format!("token-service request failed: {e}"))
            })?;
        let status = resp.status();
        match status {
            reqwest::StatusCode::UNAUTHORIZED => {
                return Err(ServiceJwtError::AuthFailed("bootstrap rejected".into()));
            }
            reqwest::StatusCode::FORBIDDEN => {
                return Err(ServiceJwtError::AuthFailed(
                    "audience not granted for this bootstrap identity".into(),
                ));
            }
            s if !s.is_success() => {
                return Err(ServiceJwtError::Unavailable(format!(
                    "token-service returned {s}"
                )));
            }
            _ => {}
        }
        let parsed: MintResponse = resp.json().await.map_err(|e| {
            ServiceJwtError::Unavailable(format!("token-service response parse failed: {e}"))
        })?;
        if parsed.access_token.trim().is_empty() || parsed.expires_in <= 0 {
            return Err(ServiceJwtError::Unavailable(
                "token-service returned an empty token or non-positive expires_in".into(),
            ));
        }
        let cached = CachedToken {
            token: Arc::from(parsed.access_token.as_str()),
            expires_at: std::time::Instant::now() + Duration::from_secs(parsed.expires_in as u64),
        };
        self.cache
            .write()
            .await
            .insert((aud.to_string(), tid), cached);
        Ok(parsed.access_token)
    }

    /// 测试专用：预填缓存（绕过 HTTP）。生产路径绝不使用。
    #[doc(hidden)]
    pub async fn prime_cache_for_test(&self, aud: &str, tid: i64, ttl: Duration, token: &str) {
        self.cache.write().await.insert(
            (aud.to_string(), tid),
            CachedToken {
                token: Arc::from(token),
                expires_at: std::time::Instant::now() + ttl,
            },
        );
    }
}

#[derive(serde::Deserialize)]
struct MintResponse {
    access_token: String,
    expires_in: i64,
}

/// URL 校验：与 JWKS 信任根同一规则 —— https 强制，
/// 127.0.0.1/localhost/[::1] 的 http 回环豁免（本机 compose/联调用）。
fn validate_service_url(raw: &str) -> Result<(), String> {
    let parsed =
        url::Url::parse(raw).map_err(|e| format!("service url '{raw}' is not a valid URL: {e}"))?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1");
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        _ => Err(format!(
            "service url must be https (loopback http is allowed only for \
             127.0.0.1/localhost/[::1]); got '{raw}'"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn loopback_config(port: u16) -> ServiceJwtConfig {
        ServiceJwtConfig {
            token_service_url: format!("http://127.0.0.1:{port}"),
            bootstrap_token: "bootstrap-dev-token".into(),
            request_timeout: Duration::from_secs(5),
            refresh_skew: DEFAULT_REFRESH_SKEW,
        }
    }

    /// 极简 HTTP 服务器：记录请求数与最后请求体，按脚本返回响应。
    struct TokenServer {
        addr: std::net::SocketAddr,
        hits: StdArc<AtomicUsize>,
        last_body: StdArc<std::sync::Mutex<String>>,
    }

    impl TokenServer {
        /// `responses`：每次请求依次取用的原始响应（状态行 + headers + body）。
        async fn spawn(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let hits = StdArc::new(AtomicUsize::new(0));
            let last_body = StdArc::new(std::sync::Mutex::new(String::new()));
            let scripted = tokio::sync::Mutex::new(responses.into_iter());
            let hits_bg = hits.clone();
            let last_body_bg = last_body.clone();
            tokio::spawn(async move {
                loop {
                    let (mut sock, _) = match listener.accept().await {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let Some(response) = scripted.lock().await.next() else {
                        break;
                    };
                    let mut buf = Vec::new();
                    // 读完头部与 body（Content-Length 已知场景足够）。
                    let mut tmp = [0u8; 4096];
                    loop {
                        let n = match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                        let text = String::from_utf8_lossy(&buf);
                        if let Some(idx) = text.find("\r\n\r\n") {
                            let headers = &text[..idx];
                            let len = headers
                                .lines()
                                .find_map(|l| {
                                    let (k, v) = l.split_once(':')?;
                                    k.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse::<usize>().ok())?
                                })
                                .unwrap_or(0);
                            if buf.len() >= idx + 4 + len {
                                break;
                            }
                        }
                    }
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(idx) = text.find("\r\n\r\n") {
                        *last_body_bg.lock().unwrap() = text[idx + 4..].to_string();
                    }
                    hits_bg.fetch_add(1, Ordering::SeqCst);
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                }
            });
            Self {
                addr,
                hits,
                last_body,
            }
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        fn last_body(&self) -> String {
            self.last_body.lock().unwrap().clone()
        }
    }

    fn ok_response(access_token: &str, expires_in: i64) -> String {
        let body = serde_json::json!({ "access_token": access_token, "expires_in": expires_in });
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.to_string().len(),
            body
        )
    }

    fn err_response(status: &str) -> String {
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }

    #[tokio::test]
    async fn mints_with_expected_request_shape() {
        let server = TokenServer::spawn(vec![ok_response("tok-1", 3600)]).await;
        let minter = ServiceJwtMinter::new(loopback_config(server.addr.port())).expect("config");
        let token = minter.file_service_token(7).await.expect("mint");
        assert_eq!(token, "tok-1");
        assert_eq!(server.hits(), 1);
        let body: serde_json::Value = serde_json::from_str(&server.last_body()).expect("json body");
        assert_eq!(body["aud"], FILE_SERVICE_AUDIENCE);
        assert_eq!(body["tid"], 7);
    }

    #[tokio::test]
    async fn caches_within_refresh_window() {
        let server = TokenServer::spawn(vec![ok_response("tok-1", 3600)]).await;
        let minter = ServiceJwtMinter::new(loopback_config(server.addr.port())).expect("config");
        let t1 = minter.file_service_token(3).await.expect("mint 1");
        let t2 = minter.file_service_token(3).await.expect("mint 2");
        assert_eq!(t1, "tok-1");
        assert_eq!(t2, t1);
        assert_eq!(server.hits(), 1, "cache hit must not hit token-service");
    }

    #[tokio::test]
    async fn re_mints_for_different_tenant() {
        let server =
            TokenServer::spawn(vec![ok_response("tok-a", 3600), ok_response("tok-b", 3600)]).await;
        let minter = ServiceJwtMinter::new(loopback_config(server.addr.port())).expect("config");
        let a = minter.file_service_token(1).await.expect("mint a");
        let b = minter.file_service_token(2).await.expect("mint b");
        assert_eq!(a, "tok-a");
        assert_eq!(b, "tok-b");
        assert_eq!(server.hits(), 2);
    }

    #[tokio::test]
    async fn re_mints_once_inside_refresh_window() {
        // expires_in=1s + 60s 提前量 ⇒ 立即进入刷新窗口：每次 get 都重 mint。
        let server =
            TokenServer::spawn(vec![ok_response("tok-1", 1), ok_response("tok-2", 3600)]).await;
        let minter = ServiceJwtMinter::new(loopback_config(server.addr.port())).expect("config");
        let _ = minter.file_service_token(9).await.expect("mint 1");
        let second = minter.file_service_token(9).await.expect("mint 2");
        assert_eq!(second, "tok-2");
        assert_eq!(server.hits(), 2);
    }

    #[tokio::test]
    async fn bootstrap_rejected_is_auth_failed() {
        let server = TokenServer::spawn(vec![err_response("401 Unauthorized")]).await;
        let minter = ServiceJwtMinter::new(loopback_config(server.addr.port())).expect("config");
        let err = minter.file_service_token(1).await.expect_err("401");
        assert!(matches!(err, ServiceJwtError::AuthFailed(_)));
    }

    #[tokio::test]
    async fn missing_grant_is_auth_failed() {
        let server = TokenServer::spawn(vec![err_response("403 Forbidden")]).await;
        let minter = ServiceJwtMinter::new(loopback_config(server.addr.port())).expect("config");
        let err = minter.file_service_token(1).await.expect_err("403");
        assert!(matches!(err, ServiceJwtError::AuthFailed(_)));
    }

    #[tokio::test]
    async fn server_down_is_unavailable() {
        // 未监听端口 → 连接失败。
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener); // 立即释放端口：无任何服务在听。
        let minter = ServiceJwtMinter::new(loopback_config(addr.port())).expect("config");
        let err = minter.file_service_token(1).await.expect_err("down");
        assert!(matches!(err, ServiceJwtError::Unavailable(_)));
    }

    #[tokio::test]
    async fn non_positive_expires_in_is_unavailable() {
        let server = TokenServer::spawn(vec![ok_response("tok-1", 0)]).await;
        let minter = ServiceJwtMinter::new(loopback_config(server.addr.port())).expect("config");
        let err = minter.file_service_token(1).await.expect_err("ttl");
        assert!(matches!(err, ServiceJwtError::Unavailable(_)));
    }

    #[test]
    fn config_validation_rejects_empty_bootstrap() {
        let cfg = ServiceJwtConfig {
            token_service_url: "http://127.0.0.1:8082".into(),
            bootstrap_token: "  ".into(),
            request_timeout: Duration::from_secs(5),
            refresh_skew: DEFAULT_REFRESH_SKEW,
        };
        assert!(ServiceJwtMinter::new(cfg).is_err());
    }

    #[test]
    fn config_validation_rejects_remote_http() {
        let cfg = ServiceJwtConfig {
            token_service_url: "http://token-service.internal:8082".into(),
            bootstrap_token: "bootstrap".into(),
            request_timeout: Duration::from_secs(5),
            refresh_skew: DEFAULT_REFRESH_SKEW,
        };
        assert!(ServiceJwtMinter::new(cfg).is_err());
    }

    #[test]
    fn config_validation_accepts_https_and_loopback_http() {
        for raw in [
            "https://token-service.internal",
            "http://127.0.0.1:8082",
            "http://localhost:8082",
        ] {
            let cfg = ServiceJwtConfig {
                token_service_url: raw.into(),
                bootstrap_token: "bootstrap".into(),
                request_timeout: Duration::from_secs(5),
                refresh_skew: DEFAULT_REFRESH_SKEW,
            };
            assert!(ServiceJwtMinter::new(cfg).is_ok(), "{raw} must be accepted");
        }
    }
}
