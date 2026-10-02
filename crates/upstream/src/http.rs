//! HTTP clients, one per effective proxy setting.
//!
//! A [`reqwest::Client`] owns a connection pool, so building one per request
//! would throw away every keep-alive connection. [`HttpClients`] builds a
//! client the first time a proxy setting is seen and reuses it afterwards.
//!
//! Every client:
//!
//! * uses the shared rustls configuration (ALPN `h2`, `http/1.1`);
//! * has a connect timeout but **no** overall request timeout — time limits
//!   are set per call, because a streamed generation may legitimately run
//!   for a very long time;
//! * decodes `gzip`, `br`, `zstd` and `deflate` response bodies;
//! * never follows redirects: a redirected `POST` would silently become a
//!   `GET`, and vendor credential headers (`x-api-key`, `x-goog-api-key`)
//!   would be replayed to whatever host the redirect names.

use crate::proxy::{proxy_cache_key, redact_proxy_url};
use crate::request::local_error;
use parking_lot::Mutex;
use rustls::ClientConfig;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use switchyard_core::UpstreamError;
use switchyard_core::config::{ProxySetting, parse_proxy};

/// How long an idle pooled connection is kept.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Idle connections kept per upstream host.
const POOL_MAX_IDLE_PER_HOST: usize = 64;

/// TCP keep-alive probe interval.
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);

/// HTTP/2 keep-alive: a silent connection is pinged this often while a
/// request is in flight, and dropped when the ping goes unanswered. Without
/// it a stream on a connection that died without a FIN or RST would hang
/// until the caller's idle timeout.
const H2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const H2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Distinct clients kept before the cache is rebuilt from scratch. Proxy
/// settings come from the configuration file, so in practice there are only
/// a handful; the bound protects against a config that is edited endlessly.
const MAX_CACHED_CLIENTS: usize = 64;

/// A cache of [`reqwest::Client`]s keyed by proxy setting and connect
/// timeout.
pub struct HttpClients {
    tls: Arc<ClientConfig>,
    cache: Mutex<HashMap<String, reqwest::Client>>,
}

impl std::fmt::Debug for HttpClients {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClients")
            .field("cached", &self.cache.lock().len())
            .finish()
    }
}

impl HttpClients {
    /// A cache using the process-wide TLS configuration ([`crate::tls::shared`]).
    pub fn new() -> Result<HttpClients, UpstreamError> {
        let tls = crate::tls::shared()
            .map_err(|e| local_error(format!("tls: cannot build the TLS configuration: {e}")))?;
        Ok(HttpClients::with_tls(tls.http.clone()))
    }

    /// A cache using `tls` for every `https` connection. The configuration
    /// should advertise `h2` and `http/1.1` through ALPN.
    pub fn with_tls(tls: Arc<ClientConfig>) -> HttpClients {
        HttpClients {
            tls,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Number of clients currently cached.
    pub fn cached(&self) -> usize {
        self.cache.lock().len()
    }

    /// The client for `proxy`, built on first use.
    ///
    /// * [`ProxySetting::Inherit`] — proxies from the environment
    ///   (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`) and, where
    ///   the platform has them, the system proxy settings;
    /// * [`ProxySetting::Direct`] — no proxy at all, environment ignored;
    /// * [`ProxySetting::Url`] — that proxy for every destination (`http`,
    ///   `https`, `socks5` with local DNS, `socks5h` with DNS at the proxy;
    ///   credentials in the URL are used).
    pub fn client(
        &self,
        proxy: &ProxySetting,
        connect_timeout: Duration,
    ) -> Result<reqwest::Client, UpstreamError> {
        // Zero would mean "fail immediately"; treat it as "no preference".
        let connect_timeout = if connect_timeout.is_zero() {
            Duration::from_secs(30)
        } else {
            connect_timeout
        };
        let key = format!("{}|{}", proxy_cache_key(proxy), connect_timeout.as_millis());
        if let Some(client) = self.cache.lock().get(&key) {
            return Ok(client.clone());
        }
        // Built outside the lock: loading system proxy settings can be slow.
        let client = self.build(proxy, connect_timeout)?;
        let mut cache = self.cache.lock();
        if cache.len() >= MAX_CACHED_CLIENTS {
            cache.clear();
        }
        Ok(cache.entry(key).or_insert(client).clone())
    }

    fn build(
        &self,
        proxy: &ProxySetting,
        connect_timeout: Duration,
    ) -> Result<reqwest::Client, UpstreamError> {
        let mut builder = reqwest::Client::builder()
            // reqwest takes the configuration by value.
            .tls_backend_preconfigured((*self.tls).clone())
            .connect_timeout(connect_timeout)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            .tcp_keepalive(TCP_KEEPALIVE)
            .tcp_nodelay(true)
            .http2_adaptive_window(true)
            .http2_keep_alive_interval(H2_KEEPALIVE_INTERVAL)
            .http2_keep_alive_timeout(H2_KEEPALIVE_TIMEOUT)
            .gzip(true)
            .brotli(true)
            .zstd(true)
            .deflate(true)
            .redirect(reqwest::redirect::Policy::none())
            .referer(false);
        match proxy {
            ProxySetting::Inherit => {}
            ProxySetting::Direct => builder = builder.no_proxy(),
            ProxySetting::Url(url) => {
                let unusable = || {
                    local_error(format!(
                        "connect: the proxy `{}` cannot be used",
                        redact_proxy_url(url)
                    ))
                };
                // reqwest accepts schemes it cannot actually proxy through;
                // hold the URL to the configuration grammar first.
                if !matches!(parse_proxy(url), Ok(ProxySetting::Url(_))) {
                    return Err(unusable());
                }
                let proxy = reqwest::Proxy::all(url.trim()).map_err(|_| unusable())?;
                builder = builder.proxy(proxy);
            }
        }
        builder.build().map_err(|e| {
            // The builder error can quote the proxy URL; keep it out.
            tracing::error!("building an HTTP client failed: {}", e.without_url());
            local_error("connect: the HTTP client could not be initialised")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clients() -> HttpClients {
        HttpClients::new().unwrap()
    }

    #[tokio::test]
    async fn one_client_per_proxy_setting() {
        let clients = clients();
        let t = Duration::from_secs(30);
        clients.client(&ProxySetting::Inherit, t).unwrap();
        clients.client(&ProxySetting::Inherit, t).unwrap();
        assert_eq!(clients.cached(), 1);
        clients.client(&ProxySetting::Direct, t).unwrap();
        clients
            .client(&ProxySetting::Url("http://127.0.0.1:3128".into()), t)
            .unwrap();
        clients
            .client(&ProxySetting::Url("http://127.0.0.1:3128".into()), t)
            .unwrap();
        clients
            .client(
                &ProxySetting::Url("socks5h://user:pw@127.0.0.1:1080".into()),
                t,
            )
            .unwrap();
        clients
            .client(&ProxySetting::Url("socks5://127.0.0.1:1080".into()), t)
            .unwrap();
        clients
            .client(&ProxySetting::Url("https://proxy.example:443".into()), t)
            .unwrap();
        assert_eq!(clients.cached(), 6);
        // A different connect timeout is a different client.
        clients
            .client(&ProxySetting::Direct, Duration::from_secs(5))
            .unwrap();
        assert_eq!(clients.cached(), 7);
        // Zero is normalised to the default.
        clients
            .client(&ProxySetting::Direct, Duration::ZERO)
            .unwrap();
        assert_eq!(clients.cached(), 7);
    }

    #[tokio::test]
    async fn unusable_proxies_are_reported_without_credentials() {
        let clients = clients();
        let err = clients
            .client(
                &ProxySetting::Url("ftp://user:hunter2@proxy.example:21".into()),
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(err.status, 0);
        assert!(err.info.message.contains("proxy"), "{}", err.info.message);
        assert!(
            !err.info.message.contains("hunter2"),
            "{}",
            err.info.message
        );
    }

    #[tokio::test]
    async fn the_cache_is_bounded() {
        let clients = clients();
        for port in 0..(MAX_CACHED_CLIENTS as u16 + 5) {
            let proxy = ProxySetting::Url(format!("http://127.0.0.1:{}", 20000 + port));
            clients.client(&proxy, Duration::from_secs(1)).unwrap();
        }
        assert!(clients.cached() <= MAX_CACHED_CLIENTS);
        assert!(clients.cached() >= 1);
    }
}
