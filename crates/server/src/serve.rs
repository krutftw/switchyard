//! The listening socket: accept loop, optional TLS, HTTP/1.1 and HTTP/2,
//! graceful shutdown with a deadline.

use crate::lifecycle::LifecycleOwner;
use crate::stall::StallGuard;
use crate::{ServeOptions, TlsFiles};
use axum::Router;
use axum::body::Body;
use axum::extract::Extension;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::server::graceful::{GracefulShutdown, Watcher};
use hyper_util::service::TowerToHyperService;
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tower::{Service, ServiceExt};

/// How long a client may take over the TLS handshake.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a connection may sit without sending a (complete) request
/// head — or, when new, anything at all. Bounds what an idle or
/// deliberately slow client can hold on to; longer than the idle timeouts
/// of common HTTP clients' connection pools, so those give a connection up
/// before the server does.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// How often an HTTP/2 connection is pinged, and how long the answer may
/// take before the connection is considered dead.
const H2_PING_INTERVAL: Duration = Duration::from_secs(60);
const H2_PING_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a write may wait for a peer that takes no data at all before
/// the connection is given up (responses, streams and WebSockets alike).
/// Any progress starts the clock afresh.
const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// How long WebSocket sessions get to say goodbye once they have been told
/// that the grace period is over.
const FAREWELL: Duration = Duration::from_secs(1);

/// A bound, not yet serving, listener.
pub struct BoundServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    tls: Option<TlsAcceptor>,
    shutdown_grace: Duration,
}

impl std::fmt::Debug for BoundServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundServer")
            .field("local_addr", &self.local_addr)
            .field("tls", &self.tls.is_some())
            .field("shutdown_grace", &self.shutdown_grace)
            .finish()
    }
}

/// An `io::Error` that says which file it is about.
fn file_error(
    kind: io::ErrorKind,
    what: &str,
    path: &Path,
    problem: impl std::fmt::Display,
) -> io::Error {
    io::Error::new(kind, format!("{what} {}: {problem}", path.display()))
}

/// Builds the TLS configuration from the PEM files: ring as the crypto
/// provider (passed explicitly — nothing depends on a process-wide
/// default), TLS 1.2 and 1.3, ALPN `h2` and `http/1.1`.
fn load_tls(files: &TlsFiles) -> io::Result<Arc<ServerConfig>> {
    const CERT: &str = "TLS certificate file";
    const KEY: &str = "TLS private key file";

    let cert_pem = std::fs::read(&files.cert)
        .map_err(|error| file_error(error.kind(), CERT, &files.cert, &error))?;
    let certs = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| file_error(io::ErrorKind::InvalidData, CERT, &files.cert, error))?;
    if certs.is_empty() {
        return Err(file_error(
            io::ErrorKind::InvalidData,
            CERT,
            &files.cert,
            "contains no PEM certificate",
        ));
    }

    let key_pem = std::fs::read(&files.key)
        .map_err(|error| file_error(error.kind(), KEY, &files.key, &error))?;
    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|error| file_error(io::ErrorKind::InvalidData, KEY, &files.key, error))?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "TLS certificate {} and private key {} cannot be used together: {error}",
                    files.cert.display(),
                    files.key.display()
                ),
            )
        })?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Binds the listening socket (and loads TLS material, if configured).
pub async fn bind(options: ServeOptions) -> io::Result<BoundServer> {
    let tls = match &options.tls {
        Some(files) => {
            let files = files.clone();
            // Reading and parsing key files is blocking work.
            let config = tokio::task::spawn_blocking(move || load_tls(&files))
                .await
                .map_err(io::Error::other)??;
            Some(TlsAcceptor::from(config))
        }
        None => None,
    };

    // `[::1]` is how an IPv6 address is written in a URL, not for a socket.
    let host = options.host.trim();
    let host = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    let listener = TcpListener::bind((host, options.port))
        .await
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot listen on {}:{}: {error}",
                    options.host, options.port
                ),
            )
        })?;
    let local_addr = listener.local_addr()?;
    Ok(BoundServer {
        listener,
        local_addr,
        tls,
        shutdown_grace: options.shutdown_grace,
    })
}

/// Whether a failed `accept` only concerns the connection that was being
/// accepted (the client hung up first) rather than the listener.
fn is_connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

impl BoundServer {
    /// The address actually bound (useful with port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serves `app` until `shutdown` resolves, then drains in-flight requests
    /// for at most the configured grace period.
    ///
    /// Connections are served with `ConnectInfo<SocketAddr>`, over HTTP/1.1
    /// (with upgrades) and HTTP/2; with TLS the protocol is negotiated by
    /// ALPN. When `shutdown` resolves the listener is closed at once; idle
    /// connections are closed, requests in progress — streams included —
    /// may finish, and WebSocket sessions are told to end (close code 1001,
    /// after the turn in progress). Whatever is still running when the grace
    /// period is over is dropped.
    pub async fn serve(
        self,
        app: Router,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> io::Result<()> {
        let BoundServer {
            listener,
            tls,
            shutdown_grace,
            ..
        } = self;

        let mut lifecycle = LifecycleOwner::new();
        let app = match lifecycle.handle() {
            Some(handle) => app.layer(Extension(handle)),
            None => app,
        };
        let mut make_service = app.into_make_service_with_connect_info::<SocketAddr>();

        let mut builder = Builder::new(TokioExecutor::new());
        builder
            .http1()
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_READ_TIMEOUT);
        // HTTP/2 connections are long-lived and say nothing when idle: ping
        // them, so that one whose peer has vanished is noticed and closed.
        builder
            .http2()
            .timer(TokioTimer::new())
            .keep_alive_interval(Some(H2_PING_INTERVAL))
            .keep_alive_timeout(H2_PING_TIMEOUT);
        let builder = Arc::new(builder);

        let graceful = GracefulShutdown::new();
        // Tells handshakes in progress to give up when shutdown begins.
        let closing = CancellationToken::new();
        let mut connections: JoinSet<()> = JoinSet::new();

        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(accepted) => accepted,
                        Err(error) if is_connection_error(&error) => continue,
                        Err(error) => {
                            // Out of file descriptors, typically: give the
                            // process a moment instead of spinning.
                            tracing::warn!(%error, "cannot accept connections");
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            continue;
                        }
                    };
                    // Events of a stream should not wait for a full segment.
                    let _ = stream.set_nodelay(true);
                    // Infallible: the service is the router plus the peer.
                    let Ok(service) = make_service.call(peer).await;
                    let service = TowerToHyperService::new(
                        service.map_request(|request: http::Request<Incoming>| request.map(Body::new)),
                    );
                    connections.spawn(serve_connection(
                        stream,
                        tls.clone(),
                        Arc::clone(&builder),
                        service,
                        graceful.watcher(),
                        closing.clone(),
                    ));
                }
                // Reap finished connections so the set does not grow.
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }

        // Stop accepting, then let what is in flight finish.
        drop(listener);
        closing.cancel();
        lifecycle.begin_shutdown();
        // The router holds lifecycle handles of its own; only requests and
        // sessions should keep the server waiting.
        drop(make_service);
        lifecycle.release();

        let drained = async {
            graceful.shutdown().await;
            lifecycle.idle().await;
        };
        if tokio::time::timeout(shutdown_grace, drained).await.is_err() {
            tracing::warn!(
                grace_secs = shutdown_grace.as_secs(),
                "requests were still in progress when the shutdown grace period ended"
            );
            lifecycle.kill();
            connections.shutdown().await;
            let _ = tokio::time::timeout(FAREWELL, lifecycle.idle()).await;
        } else {
            // Every connection has finished; collect the tasks.
            while connections.join_next().await.is_some() {}
        }
        Ok(())
    }
}

/// Serves one connection to its end.
async fn serve_connection<S>(
    stream: TcpStream,
    tls: Option<TlsAcceptor>,
    builder: Arc<Builder<TokioExecutor>>,
    service: S,
    watcher: Watcher,
    closing: CancellationToken,
) where
    S: hyper::service::Service<
            http::Request<Incoming>,
            Response = http::Response<Body>,
            Error = std::convert::Infallible,
        > + Send
        + 'static,
    S::Future: Send + 'static,
{
    match tls {
        None => serve_io(stream, builder, service, watcher).await,
        Some(acceptor) => {
            let handshake = tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream));
            let stream = tokio::select! {
                _ = closing.cancelled() => return,
                handshake = handshake => match handshake {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(error)) => {
                        tracing::debug!(%error, "a TLS handshake failed");
                        return;
                    }
                    Err(_) => {
                        tracing::debug!("a TLS handshake timed out");
                        return;
                    }
                },
            };
            serve_io(stream, builder, service, watcher).await;
        }
    }
}

/// Runs HTTP on an established byte stream.
async fn serve_io<I, S>(io: I, builder: Arc<Builder<TokioExecutor>>, service: S, watcher: Watcher)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: hyper::service::Service<
            http::Request<Incoming>,
            Response = http::Response<Body>,
            Error = std::convert::Infallible,
        > + Send
        + 'static,
    S::Future: Send + 'static,
{
    // A peer that connects and says nothing, or stops taking what it is
    // sent, ends the connection instead of holding it — and what is behind
    // it — forever.
    let io = StallGuard::new(io, HEADER_READ_TIMEOUT, WRITE_STALL_TIMEOUT);
    let connection = builder.serve_connection_with_upgrades(TokioIo::new(io), service);
    if let Err(error) = watcher.watch(connection).await {
        // Clients hanging up mid-request end up here; it is not news.
        tracing::trace!(%error, "a connection ended with an error");
    }
}
