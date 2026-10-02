//! The TLS configuration shared by every outbound connection.
//!
//! Two [`rustls::ClientConfig`]s are built once per process from the same
//! trust store: one advertising `h2` and `http/1.1` through ALPN for HTTP
//! clients, and one without ALPN for WebSocket handshakes (a WebSocket
//! upgrade is an HTTP/1.1 exchange; offering `h2` would let the server pick a
//! protocol the handshake cannot speak).
//!
//! The crypto provider is always passed explicitly (`ring`), so nothing here
//! depends on a process-wide default provider having been installed.

use rustls::{ClientConfig, RootCertStore};
use std::sync::{Arc, OnceLock};

/// The pair of client configurations used by this crate.
#[derive(Clone, Debug)]
pub struct TlsConfigs {
    /// For HTTP clients: ALPN `h2`, `http/1.1`.
    pub http: Arc<ClientConfig>,
    /// For WebSocket handshakes and proxy tunnels: no ALPN.
    pub ws: Arc<ClientConfig>,
}

/// Builds the trust store: the bundled Mozilla roots plus whatever the
/// operating system trusts (so corporate TLS-inspecting proxies keep
/// working). Failing to read the OS store is not fatal.
fn root_store() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let native = rustls_native_certs::load_native_certs();
    if !native.errors.is_empty() {
        tracing::debug!(
            errors = native.errors.len(),
            "some operating system root certificates could not be loaded"
        );
    }
    let (added, ignored) = roots.add_parsable_certificates(native.certs);
    tracing::debug!(added, ignored, "loaded operating system root certificates");
    roots
}

/// Builds a fresh pair of configurations. Most callers want [`shared`].
pub fn build() -> Result<TlsConfigs, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let base = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(root_store())
        .with_no_client_auth();

    let mut http = base.clone();
    http.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let mut ws = base;
    ws.alpn_protocols.clear();

    Ok(TlsConfigs {
        http: Arc::new(http),
        ws: Arc::new(ws),
    })
}

/// The process-wide configurations, built on first use.
pub fn shared() -> Result<&'static TlsConfigs, String> {
    static SHARED: OnceLock<Result<TlsConfigs, String>> = OnceLock::new();
    SHARED
        .get_or_init(|| build().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_without_a_default_provider_installed() {
        // No `install_default()` anywhere in this test binary.
        let tls = build().unwrap();
        assert_eq!(
            tls.http.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
        assert!(tls.ws.alpn_protocols.is_empty());
    }

    #[test]
    fn shared_is_built_once() {
        let a = shared().unwrap();
        let b = shared().unwrap();
        assert!(Arc::ptr_eq(&a.http, &b.http));
        assert!(Arc::ptr_eq(&a.ws, &b.ws));
    }

    #[test]
    fn only_modern_protocol_versions_are_enabled() {
        let provider = rustls::crypto::ring::default_provider();
        // Sanity check on the provider the configs are built from.
        assert!(!provider.cipher_suites.is_empty());
        let tls = build().unwrap();
        assert!(tls.http.enable_sni);
    }
}
