//! Client-facing API routes. See `docs/DESIGN.md` sections 9 and 10.
//!
//! The public surface is fixed (the `switchyard` binary and the admin crate
//! are written against it):
//!
//! * [`router`] — every client route (HTTP, SSE, WebSocket) as an
//!   [`axum::Router`];
//! * [`ServeOptions`], [`bind`], [`BoundServer`] — listening socket, optional
//!   TLS, graceful shutdown. Connections are served with
//!   `ConnectInfo<SocketAddr>` so handlers (including the admin crate's) can
//!   see the peer address.

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use switchyard_core::Config;
use switchyard_gateway::Gateway;

/// Builds the client API router.
pub fn router(_gateway: Gateway) -> axum::Router {
    axum::Router::new()
}

/// Certificate chain and private key files for HTTPS.
#[derive(Clone, Debug)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Where and how to listen.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    pub host: String,
    pub port: u16,
    pub tls: Option<TlsFiles>,
    /// How long in-flight requests may run after shutdown was requested.
    pub shutdown_grace: Duration,
}

impl ServeOptions {
    /// Listening options from `[server]`; relative TLS paths resolve against
    /// `config_dir`.
    pub fn from_config(config: &Config, config_dir: &Path) -> Self {
        ServeOptions {
            host: config.server.host.clone(),
            port: config.server.port,
            tls: config.server.tls.as_ref().map(|t| TlsFiles {
                cert: config_dir.join(&t.cert),
                key: config_dir.join(&t.key),
            }),
            shutdown_grace: Duration::from_secs(30),
        }
    }
}

/// A bound, not yet serving, listener.
pub struct BoundServer {
    local_addr: SocketAddr,
}

impl BoundServer {
    /// The address actually bound (useful with port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serves `app` until `shutdown` resolves, then drains in-flight requests
    /// for at most the configured grace period.
    pub async fn serve(
        self,
        _app: axum::Router,
        _shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        Err(std::io::Error::other("not implemented"))
    }
}

/// Binds the listening socket (and loads TLS material, if configured).
pub async fn bind(_options: ServeOptions) -> std::io::Result<BoundServer> {
    Err(std::io::Error::other("not implemented"))
}
