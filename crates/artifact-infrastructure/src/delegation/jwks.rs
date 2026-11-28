//! JWKS 信任根：固定 URL 拉取 + kid-strict 缓存 + 失败关闭（WP 契约 v0.2 §4.2，Q5 裁决固化）。
//!
//! 安全规则（实现与契约一一对应）：
//! * **固定配置 URL**：JWKS 地址只来自受信 issuer 配置；绝不依据 token 的
//!   `iss` / header / 任何请求内容做动态发现（P0-03）。
//! * **HTTPS**：由配置解析层强制（`Config::from_env` 拒绝非 https 的
//!   JWKS URL，仅 127.0.0.1/localhost/::1 的 http 回环豁免）；本模块的
//!   HTTP 客户端启用标准证书校验、禁用重定向（3xx = 拉取失败）。
//! * **kid 严格匹配**：只在已拉取的 key 集合内按 kid 查找；找不到即拒绝。
//! * **缺失 kid**：直接拒绝，不回退任何 default key。
//! * **未知 kid**：触发一次强制刷新并复检（轮换窗口内新 kid 立即可用）；
//!   复检仍未知 → 拒绝。全部刷新尝试按 [`UNKNOWN_KID_REFRESH_MIN_INTERVAL`]
//!   限频（契约要求 ≥60s，防攻击者用随机 kid 打爆签发方）。
//! * **陈旧策略（Q5）**：缓存软刷新间隔 [`JWKS_REFRESH_INTERVAL`]（5 分钟，
//!   到期触发刷新）；**最大陈旧时间 [`JWKS_MAX_STALENESS`]（15 分钟）**——
//!   超过后缓存不得再作为可信密钥来源：刷新失败即 fail-closed（401/503），
//!   不得无限期 "keeping stale"。15 分钟窗口内刷新失败仍可用 last-known-good
//!   验签（轮换窗口容忍）。
//! * **服务重启**：缓存从空开始，首次验证触发拉取；拉取失败 = fail-closed，
//!   绝不以空缓存放行任何请求。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{AlgorithmParameters, Jwk};
use tokio::sync::Mutex;

/// 周期刷新间隔（契约 §4.2：5 分钟，到期后下一次验证触发刷新）。
pub const JWKS_REFRESH_INTERVAL: Duration = Duration::from_secs(300);
/// 最大缓存陈旧时间（契约 §4.2：15 分钟；超过即不得作为可信密钥来源）。
pub const JWKS_MAX_STALENESS: Duration = Duration::from_secs(900);
/// 全部刷新尝试的最小间隔（同 URL 维度限频；契约要求 ≥60s）。
pub const UNKNOWN_KID_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(60);
/// JWKS 拉取超时。
pub const JWKS_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub enum JwksLookup {
    /// kid 命中（缓存新鲜、或陈旧窗口内的 last-known-good）。
    Known(Arc<Jwk>),
    /// 拉取成功（或缓存有效）但 kid 不存在（含原因短句，无敏感信息）。
    UnknownKid(String),
    /// 无法获得有效 key 集合（无缓存 / 超过最大陈旧时间且刷新失败）：失败关闭。
    Unavailable(String),
}

struct CacheEntry {
    keys: Arc<HashMap<String, Arc<Jwk>>>,
    fetched_at: Instant,
    last_refresh_attempt: Instant,
}

pub struct JwksCache {
    http: reqwest::Client,
    entries: Mutex<HashMap<String, CacheEntry>>,
    refresh_interval: Duration,
    max_staleness: Duration,
}

impl JwksCache {
    /// 构建缓存（默认 5 分钟刷新 / 15 分钟最大陈旧）。HTTP 客户端：
    /// rustls 证书校验开启、禁用重定向、5s 超时。
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(JWKS_FETCH_TIMEOUT)
            .build()
            .expect("static reqwest client configuration");
        Self::with_policy(http, JWKS_REFRESH_INTERVAL, JWKS_MAX_STALENESS)
    }

    /// 测试装配：显式给出软刷新间隔与最大陈旧时间。
    pub fn with_policy(
        http: reqwest::Client,
        refresh_interval: Duration,
        max_staleness: Duration,
    ) -> Self {
        Self {
            http,
            entries: Mutex::new(HashMap::new()),
            refresh_interval,
            max_staleness,
        }
    }

    /// 取得 `jwks_url` 下 `kid` 对应的公钥。只会以配置给的 URL 出站。
    pub async fn lookup(&self, jwks_url: &str, kid: &str) -> JwksLookup {
        let mut entries = self.entries.lock().await;
        let now = Instant::now();
        let entry = entries.get(jwks_url);

        let (age, fresh_keys, since_attempt) = match entry {
            Some(e) => (
                Some(now.duration_since(e.fetched_at)),
                Some(Arc::clone(&e.keys)),
                Some(now.duration_since(e.last_refresh_attempt)),
            ),
            None => (None, None, None),
        };

        // ① 无缓存：必须拉取成功；失败 = fail-closed（重启空缓存不放过任何请求）。
        let Some(age) = age else {
            return self.fetch_and_store(jwks_url, kid, &mut entries).await;
        };
        let fresh_keys = fresh_keys.expect("entry exists");
        let since_attempt = since_attempt.expect("entry exists");

        // ② 缓存超过最大陈旧时间：先尝试刷新；失败 = fail-closed（Q5 硬上限）。
        if age >= self.max_staleness {
            return match self.try_fetch(jwks_url).await {
                Ok(keys) => {
                    let known = keys.get(kid).map(Arc::clone);
                    entries.insert(
                        jwks_url.to_string(),
                        CacheEntry {
                            keys: Arc::new(keys),
                            fetched_at: Instant::now(),
                            last_refresh_attempt: Instant::now(),
                        },
                    );
                    known.map_or(
                        JwksLookup::UnknownKid("kid not in refreshed jwks set".into()),
                        JwksLookup::Known,
                    )
                }
                Err(err) => JwksLookup::Unavailable(format!(
                    "jwks cache exceeded max staleness and refresh failed: {err}"
                )),
            };
        }

        // ③ 缓存陈旧窗口内（< 15min）：
        //    - kid 命中 → last-known-good 直接可用（轮换窗口容忍）；
        //    - 距上次刷新 ≥ 软刷新间隔 → 尝试刷新（成功则更新，失败保持 last-known-good）；
        //    - 未知 kid 且距上次刷新尝试 ≥ 限频间隔 → 强制刷新一次并复检。
        if let Some(key) = fresh_keys.get(kid) {
            if age >= self.refresh_interval && since_attempt >= UNKNOWN_KID_REFRESH_MIN_INTERVAL {
                // 周期刷新：成功更新缓存（仍返回命中键，键集合只增不减判定不变）。
                if let Ok(keys) = self.try_fetch(jwks_url).await {
                    entries.insert(
                        jwks_url.to_string(),
                        CacheEntry {
                            keys: Arc::new(keys),
                            fetched_at: Instant::now(),
                            last_refresh_attempt: Instant::now(),
                        },
                    );
                }
            }
            return JwksLookup::Known(Arc::clone(key));
        }

        // kid 未命中：软刷新到期或限频窗口已过 → 强制刷新一次复检。
        if age >= self.refresh_interval || since_attempt >= UNKNOWN_KID_REFRESH_MIN_INTERVAL {
            return match self.try_fetch(jwks_url).await {
                Ok(keys) => {
                    let known = keys.get(kid).map(Arc::clone);
                    entries.insert(
                        jwks_url.to_string(),
                        CacheEntry {
                            keys: Arc::new(keys),
                            fetched_at: Instant::now(),
                            last_refresh_attempt: Instant::now(),
                        },
                    );
                    known.map_or(
                        JwksLookup::UnknownKid("kid not in refreshed jwks set".into()),
                        JwksLookup::Known,
                    )
                }
                Err(err) => {
                    // 刷新失败：记录尝试时间（限频），kid 仍未知 → 拒绝。
                    if let Some(e) = entries.get_mut(jwks_url) {
                        e.last_refresh_attempt = Instant::now();
                    }
                    JwksLookup::UnknownKid(format_reason(&err))
                }
            };
        }
        JwksLookup::UnknownKid("kid not in cached jwks set (refresh rate limited)".into())
    }

    async fn fetch_and_store(
        &self,
        jwks_url: &str,
        kid: &str,
        entries: &mut HashMap<String, CacheEntry>,
    ) -> JwksLookup {
        match self.try_fetch(jwks_url).await {
            Ok(keys) => {
                let known = keys.get(kid).map(Arc::clone);
                entries.insert(
                    jwks_url.to_string(),
                    CacheEntry {
                        keys: Arc::new(keys),
                        fetched_at: Instant::now(),
                        last_refresh_attempt: Instant::now(),
                    },
                );
                known.map_or(
                    JwksLookup::UnknownKid("kid not in freshly fetched jwks set".into()),
                    JwksLookup::Known,
                )
            }
            Err(err) => JwksLookup::Unavailable(format!("jwks fetch failed and no cache: {err}")),
        }
    }

    async fn try_fetch(&self, jwks_url: &str) -> Result<HashMap<String, Arc<Jwk>>, String> {
        let response = self
            .http
            .get(jwks_url)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("jwks request failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("jwks endpoint returned {}", response.status()));
        }
        let jwks: jsonwebtoken::jwk::JwkSet = response
            .json()
            .await
            .map_err(|e| format!("jwks body is not a valid JWK set: {e}"))?;
        let mut keys = HashMap::new();
        for jwk in jwks.keys {
            // 只收 RSA 公钥（RS256 验证在 Validation 层强制）；kid 必填（kid-strict）。
            // 非 RSA / 缺 kid 的条目被忽略；kid 冲突或家族不符会在验签时失败关闭。
            let AlgorithmParameters::RSA(_) = jwk.algorithm else {
                continue;
            };
            let Some(kid) = jwk.common.key_id.clone() else {
                continue;
            };
            keys.insert(kid, Arc::new(jwk));
        }
        if keys.is_empty() {
            return Err("jwks set contains no usable RSA signing keys".into());
        }
        Ok(keys)
    }
}

/// 把刷新失败原因折叠为 UnknownKid 携带的稳定短句（无敏感信息）。
fn format_reason(err: &str) -> String {
    format!("kid not in cached jwks set; refresh failed ({err})")
}

impl Default for JwksCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback_http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(JWKS_FETCH_TIMEOUT)
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn unreachable_jwks_is_unavailable() {
        // 端口 1 上几乎必然无监听 → 连接失败 → 不可用（重启空缓存 fail-closed）。
        let cache = JwksCache::with_policy(
            loopback_http_client(),
            JWKS_REFRESH_INTERVAL,
            JWKS_MAX_STALENESS,
        );
        let lookup = cache.lookup("http://127.0.0.1:1/jwks.json", "kid-1").await;
        assert!(matches!(lookup, JwksLookup::Unavailable(_)));
    }

    #[tokio::test]
    async fn redirect_response_is_treated_as_failure() {
        let cache = JwksCache::with_policy(
            loopback_http_client(),
            JWKS_REFRESH_INTERVAL,
            JWKS_MAX_STALENESS,
        );
        let lookup = cache.lookup("http://[::1]:1/jwks.json", "kid-1").await;
        assert!(matches!(lookup, JwksLookup::Unavailable(_)));
    }

    #[test]
    fn stale_policy_constants_match_contract() {
        // 契约 §4.2（Q5）：5 分钟刷新、15 分钟最大陈旧、≥60s 刷新限频。
        assert_eq!(JWKS_REFRESH_INTERVAL, Duration::from_secs(300));
        assert_eq!(JWKS_MAX_STALENESS, Duration::from_secs(900));
        assert!(UNKNOWN_KID_REFRESH_MIN_INTERVAL >= Duration::from_secs(60));
        assert!(
            JWKS_MAX_STALENESS >= 2 * Duration::from_secs(300),
            "≥ 2×max TTL"
        );
    }
}
