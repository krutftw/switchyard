//! WebSocket connections to upstream providers: the Responses WebSocket
//! endpoint (`wss://…/v1/responses`) and the Realtime API
//! (`wss://…/v1/realtime?model=…`).
//!
//! The connection is established by hand — TCP, optional proxy tunnel, TLS
//! with the shared no-ALPN configuration, then the WebSocket handshake by
//! `tokio-tungstenite` — so that proxies are honoured and every phase can
//! be named in errors.
//!
//! # Proxies
//!
//! The target's proxy setting applies as for HTTP calls:
//!
//! * `http://` — `CONNECT` tunnel, `Proxy-Authorization: Basic` from the
//!   URL's credentials;
//! * `https://` — TLS to the proxy, then `CONNECT`;
//! * `socks5://` — SOCKS5 with the destination resolved locally;
//!   `socks5h://` — SOCKS5 with the destination resolved by the proxy;
//!   username/password authentication from the URL's credentials;
//! * `direct` — no proxy;
//! * inherit — `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY` from
//!   the environment. Unlike HTTP calls, operating-system proxy settings
//!   (the Windows registry, macOS system configuration) are **not**
//!   consulted for WebSockets.
//!
//! # Connecting
//!
//! A host name is resolved once and its addresses are tried the way the
//! HTTP client tries them ("happy eyeballs"): address families alternate,
//! and when an attempt has neither succeeded nor failed after a short delay
//! the next address is tried alongside it. A dead first address — an
//! unreachable IPv6 record on a network without IPv6 — therefore costs a
//! fraction of a second instead of the whole connect timeout. The same
//! applies to the proxy's host when a proxy is used.
//!
//! # Failures
//!
//! A handshake the upstream answers with an HTTP error (401, 403, 429, …)
//! is classified exactly like an HTTP response ([`crate::classify()`]); note
//! that only the part of the error body that arrived together with the
//! response head is available. Everything else is a transport error whose
//! message names the phase (`connect`, `tls`, `timeout`, `read`). As for
//! HTTP calls, credentials the handshake presented are removed from
//! whatever the upstream answered.

use crate::client::{UpstreamClient, authority_of, classify_response, deadline_after, scrub_urls};
use crate::proxy::{proxy_from_env, redact_proxy_url};
use crate::request::{USER_AGENT, build_ws_request, local_error};
use crate::secrets::Scrubber;
use crate::target::Target;
use base64::Engine;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use http::HeaderMap;
use percent_encoding::percent_decode_str;
use rustls::pki_types::ServerName;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use switchyard_core::UpstreamError;
use switchyard_core::config::ProxySetting;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Time allowed for establishing a WebSocket (TCP, proxy, TLS, handshake)
/// when the caller states none.
pub const DEFAULT_WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest message / frame accepted from an upstream. Generous on purpose:
/// a Responses event can carry a complete base64 image.
const MAX_WS_MESSAGE_BYTES: usize = 256 << 20;

/// How long one address is given before the next one is tried alongside it
/// (RFC 8305 recommends 250 ms).
const ADDRESS_FALLBACK_DELAY: Duration = Duration::from_millis(250);

/// Largest proxy `CONNECT` response head accepted.
const MAX_PROXY_HEAD_BYTES: usize = 16 * 1024;

/// A byte stream a WebSocket can run over.
pub trait AsyncIo: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> AsyncIo for T {}

// Lets `UpstreamWebSocket` (and `Result`s holding one) be formatted.
impl std::fmt::Debug for dyn AsyncIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AsyncIo")
    }
}

/// A type-erased connection: plain TCP, TLS, or either through a proxy.
pub type BoxedIo = Box<dyn AsyncIo>;

/// An established upstream WebSocket.
pub type UpstreamWebSocket = WebSocketStream<BoxedIo>;

/// An established upstream WebSocket together with the handshake response.
pub struct WsConnection {
    pub stream: UpstreamWebSocket,
    /// Headers of the `101 Switching Protocols` response — in particular
    /// `sec-websocket-protocol`, which a relay has to echo to its client.
    pub headers: HeaderMap,
}

impl std::fmt::Debug for WsConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsConnection")
            .field("headers", &self.headers)
            .finish()
    }
}

/// Phase-tagged failure while establishing a connection.
fn failure(phase: &str, message: impl std::fmt::Display) -> UpstreamError {
    UpstreamError::transport(format!("{phase}: {}", scrub_urls(&message.to_string())))
}

fn timed_out(what: &str, limit: Duration) -> UpstreamError {
    let mut error = UpstreamError::transport(format!(
        "timeout: {what} took longer than {} s",
        limit.as_secs_f32()
    ));
    error.status = 408;
    error
}

/// A proxy to tunnel through.
#[derive(Clone, PartialEq, Eq)]
struct ProxyEndpoint {
    scheme: ProxyScheme,
    host: String,
    port: u16,
    credentials: Option<(String, String)>,
    /// For messages: scheme, host and port only.
    display: String,
}

// Not derived: the proxy password must not reach a log through `{:?}`.
impl std::fmt::Debug for ProxyEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ProxyEndpoint({})", self.display)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProxyScheme {
    Http,
    Https,
    /// SOCKS5, destination resolved locally.
    Socks5,
    /// SOCKS5, destination resolved by the proxy.
    Socks5h,
}

fn parse_proxy_endpoint(raw: &str) -> Result<ProxyEndpoint, UpstreamError> {
    let display = redact_proxy_url(raw);
    let bad = || failure("connect", format!("the proxy `{display}` cannot be used"));
    let url = url::Url::parse(raw.trim()).map_err(|_| bad())?;
    let (scheme, default_port) = match url.scheme() {
        "http" => (ProxyScheme::Http, 80),
        "https" => (ProxyScheme::Https, 443),
        "socks5" => (ProxyScheme::Socks5, 1080),
        "socks5h" => (ProxyScheme::Socks5h, 1080),
        _ => return Err(bad()),
    };
    let host = match url.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(bad()),
    };
    let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
    let credentials = if url.username().is_empty() && url.password().is_none() {
        None
    } else {
        Some((decode(url.username()), decode(url.password().unwrap_or(""))))
    };
    Ok(ProxyEndpoint {
        scheme,
        host,
        port: url.port().unwrap_or(default_port),
        credentials,
        display,
    })
}

/// `host:port` as written in a request target (IPv6 literals bracketed).
fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// `"proxy "` for a proxy hop, nothing for the destination itself.
fn hop(is_proxy: bool) -> &'static str {
    if is_proxy { "proxy " } else { "" }
}

/// Orders resolved addresses for connecting: the family the resolver
/// prefers first, then alternating families, so that one broken family
/// cannot occupy all the early attempts.
fn interleave_families(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let Some(first) = addrs.first() else {
        return addrs;
    };
    let preferred_v6 = first.is_ipv6();
    let (mut preferred, mut other): (Vec<SocketAddr>, Vec<SocketAddr>) = addrs
        .into_iter()
        .partition(|addr| addr.is_ipv6() == preferred_v6);
    let mut out = Vec::with_capacity(preferred.len() + other.len());
    preferred.reverse();
    other.reverse();
    loop {
        match (preferred.pop(), other.pop()) {
            (None, None) => return out,
            (a, b) => out.extend(a.into_iter().chain(b)),
        }
    }
}

/// Connects to the first of `addrs` that answers.
///
/// Addresses are tried in order. The next one is started as soon as an
/// attempt fails, or when `fallback_delay` passes without any attempt
/// finishing — earlier attempts keep running and the first connection
/// established wins. When every address fails, the first failure is
/// reported: it belongs to the address the resolver preferred.
async fn connect_any(
    addrs: Vec<SocketAddr>,
    fallback_delay: Duration,
) -> std::io::Result<TcpStream> {
    let mut remaining = addrs.into_iter();
    let mut attempts = FuturesUnordered::new();
    let mut first_error: Option<std::io::Error> = None;
    loop {
        if attempts.is_empty() {
            match remaining.next() {
                Some(addr) => attempts.push(TcpStream::connect(addr)),
                None => {
                    return Err(first_error.unwrap_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::AddrNotAvailable,
                            "the name resolved to no address",
                        )
                    }));
                }
            }
        }
        let more = remaining.len() > 0;
        tokio::select! {
            outcome = attempts.next() => match outcome {
                Some(Ok(stream)) => return Ok(stream),
                Some(Err(error)) => {
                    first_error.get_or_insert(error);
                    if let Some(addr) = remaining.next() {
                        attempts.push(TcpStream::connect(addr));
                    }
                }
                // Refilled at the top of the loop.
                None => {}
            },
            _ = tokio::time::sleep(fallback_delay), if more => {
                if let Some(addr) = remaining.next() {
                    attempts.push(TcpStream::connect(addr));
                }
            }
        }
    }
}

async fn tcp_connect(host: &str, port: u16, is_proxy: bool) -> Result<TcpStream, UpstreamError> {
    let shown = format!("{}{}", hop(is_proxy), authority(host, port));
    // An IP literal "resolves" to itself without a lookup.
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| failure("connect", format!("could not resolve {shown}: {e}")))?
        .collect();
    let stream = connect_any(interleave_families(addrs), ADDRESS_FALLBACK_DELAY)
        .await
        .map_err(|e| failure("connect", format!("could not connect to {shown}: {e}")))?;
    // Frames are small and latency-sensitive.
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

async fn tls_connect(
    io: BoxedIo,
    host: &str,
    config: Arc<rustls::ClientConfig>,
    is_proxy: bool,
) -> Result<BoxedIo, UpstreamError> {
    let name = ServerName::try_from(host.to_string())
        .map_err(|_| failure("tls", format!("`{host}` is not a valid TLS server name")))?;
    let stream = tokio_rustls::TlsConnector::from(config)
        .connect(name, io)
        .await
        .map_err(|e| {
            failure(
                "tls",
                format!("handshake with {}{host} failed: {e}", hop(is_proxy)),
            )
        })?;
    Ok(Box::new(stream))
}

/// Asks an HTTP proxy for a tunnel to `host:port`.
async fn http_connect_tunnel<S>(
    io: &mut S,
    host: &str,
    port: u16,
    proxy: &ProxyEndpoint,
) -> Result<(), UpstreamError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let destination = authority(host, port);
    let mut request = format!(
        "CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\nUser-Agent: {USER_AGENT}\r\nProxy-Connection: Keep-Alive\r\n"
    );
    if let Some((user, password)) = &proxy.credentials {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("\r\n");
    let via = &proxy.display;
    io.write_all(request.as_bytes())
        .await
        .map_err(|e| failure("connect", format!("proxy {via} closed the connection: {e}")))?;
    io.flush()
        .await
        .map_err(|e| failure("connect", format!("proxy {via} closed the connection: {e}")))?;

    // Read exactly up to the end of the response head: whatever follows
    // belongs to the tunnelled protocol.
    let mut head = Vec::with_capacity(256);
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_PROXY_HEAD_BYTES {
            return Err(failure(
                "connect",
                format!("proxy {via} sent an oversized response"),
            ));
        }
        let byte = io.read_u8().await.map_err(|e| {
            failure(
                "connect",
                format!("proxy {via} closed the connection before answering: {e}"),
            )
        })?;
        head.push(byte);
    }
    let text = String::from_utf8_lossy(&head);
    let status_line = text.lines().next().unwrap_or("");
    let mut pieces = status_line.split_whitespace();
    let version = pieces.next().unwrap_or("");
    let status: u16 = pieces.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    if !version.starts_with("HTTP/") || status == 0 {
        return Err(failure(
            "connect",
            format!("proxy {via} sent an invalid response"),
        ));
    }
    match status {
        200..=299 => Ok(()),
        407 => Err(failure(
            "connect",
            format!("proxy {via} requires authentication (HTTP 407)"),
        )),
        other => Err(failure(
            "connect",
            format!("proxy {via} refused the tunnel to {destination} (HTTP {other})"),
        )),
    }
}

/// Destination address in a SOCKS5 request.
enum SocksAddr {
    Ip(IpAddr),
    Domain(String),
}

/// Performs the SOCKS5 handshake (RFC 1928, username/password per RFC 1929)
/// asking for a connection to `addr:port`.
async fn socks5_connect<S>(
    io: &mut S,
    addr: SocksAddr,
    port: u16,
    proxy: &ProxyEndpoint,
) -> Result<(), UpstreamError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let via = &proxy.display;
    let io_error = |e: std::io::Error| {
        failure(
            "connect",
            format!("SOCKS proxy {via} closed the connection: {e}"),
        )
    };
    let protocol_error = |what: &str| failure("connect", format!("SOCKS proxy {via} {what}"));

    // Method negotiation.
    let greeting: &[u8] = if proxy.credentials.is_some() {
        &[0x05, 0x02, 0x00, 0x02]
    } else {
        &[0x05, 0x01, 0x00]
    };
    io.write_all(greeting).await.map_err(io_error)?;
    io.flush().await.map_err(io_error)?;
    let mut choice = [0u8; 2];
    io.read_exact(&mut choice).await.map_err(io_error)?;
    if choice[0] != 0x05 {
        return Err(protocol_error("does not speak SOCKS5"));
    }
    match choice[1] {
        0x00 => {}
        0x02 => {
            let Some((user, password)) = &proxy.credentials else {
                return Err(protocol_error("requires a username and password"));
            };
            if user.len() > 255 || password.len() > 255 {
                return Err(protocol_error("credentials are too long for SOCKS5"));
            }
            let mut auth = Vec::with_capacity(3 + user.len() + password.len());
            auth.push(0x01);
            auth.push(user.len() as u8);
            auth.extend_from_slice(user.as_bytes());
            auth.push(password.len() as u8);
            auth.extend_from_slice(password.as_bytes());
            io.write_all(&auth).await.map_err(io_error)?;
            io.flush().await.map_err(io_error)?;
            let mut verdict = [0u8; 2];
            io.read_exact(&mut verdict).await.map_err(io_error)?;
            if verdict[1] != 0x00 {
                return Err(protocol_error("rejected the username or password"));
            }
        }
        _ => {
            return Err(protocol_error(
                "accepts none of the offered authentication methods",
            ));
        }
    }

    // Connect request.
    let mut request = vec![0x05, 0x01, 0x00];
    match &addr {
        SocksAddr::Ip(IpAddr::V4(ip)) => {
            request.push(0x01);
            request.extend_from_slice(&ip.octets());
        }
        SocksAddr::Ip(IpAddr::V6(ip)) => {
            request.push(0x04);
            request.extend_from_slice(&ip.octets());
        }
        SocksAddr::Domain(name) => {
            if name.len() > 255 {
                return Err(protocol_error(
                    "cannot be given a host name longer than 255 bytes",
                ));
            }
            request.push(0x03);
            request.push(name.len() as u8);
            request.extend_from_slice(name.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    io.write_all(&request).await.map_err(io_error)?;
    io.flush().await.map_err(io_error)?;

    let mut reply = [0u8; 4];
    io.read_exact(&mut reply).await.map_err(io_error)?;
    if reply[0] != 0x05 {
        return Err(protocol_error("sent an invalid reply"));
    }
    if reply[1] != 0x00 {
        let reason = match reply[1] {
            0x01 => "general failure",
            0x02 => "connection not allowed by its rules",
            0x03 => "network unreachable",
            0x04 => "host unreachable",
            0x05 => "connection refused",
            0x06 => "TTL expired",
            0x07 => "command not supported",
            0x08 => "address type not supported",
            _ => "unknown error",
        };
        return Err(protocol_error(&format!(
            "could not reach the destination: {reason}"
        )));
    }
    // Discard the bound address.
    let remaining = match reply[3] {
        0x01 => 4 + 2,
        0x04 => 16 + 2,
        0x03 => usize::from(io.read_u8().await.map_err(io_error)?) + 2,
        _ => return Err(protocol_error("sent an invalid reply")),
    };
    let mut bound = vec![0u8; remaining];
    io.read_exact(&mut bound).await.map_err(io_error)?;
    Ok(())
}

/// Opens a byte stream to `host:port`, through `proxy` when given.
async fn dial(
    host: &str,
    port: u16,
    proxy: Option<&ProxyEndpoint>,
    tls: &Arc<rustls::ClientConfig>,
) -> Result<BoxedIo, UpstreamError> {
    let Some(proxy) = proxy else {
        return Ok(Box::new(tcp_connect(host, port, false).await?));
    };
    let tcp = tcp_connect(&proxy.host, proxy.port, true).await?;
    match proxy.scheme {
        ProxyScheme::Http => {
            let mut io = tcp;
            http_connect_tunnel(&mut io, host, port, proxy).await?;
            Ok(Box::new(io))
        }
        ProxyScheme::Https => {
            let mut io = tls_connect(Box::new(tcp), &proxy.host, tls.clone(), true).await?;
            http_connect_tunnel(&mut io, host, port, proxy).await?;
            Ok(io)
        }
        ProxyScheme::Socks5 | ProxyScheme::Socks5h => {
            let addr = match host.parse::<IpAddr>() {
                Ok(ip) => SocksAddr::Ip(ip),
                Err(_) if proxy.scheme == ProxyScheme::Socks5h => {
                    SocksAddr::Domain(host.to_string())
                }
                Err(_) => {
                    let resolved = tokio::net::lookup_host((host, port))
                        .await
                        .ok()
                        .and_then(|mut addrs| addrs.next())
                        .ok_or_else(|| failure("connect", format!("could not resolve {host}")))?;
                    SocksAddr::Ip(resolved.ip())
                }
            };
            let mut io = tcp;
            socks5_connect(&mut io, addr, port, proxy).await?;
            Ok(Box::new(io))
        }
    }
}

/// Headers the handshake itself owns; never copied from a built request.
fn is_handshake_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "upgrade"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-accept"
            | "sec-websocket-extensions"
    )
}

impl UpstreamClient {
    /// Opens a WebSocket to `path_and_query` (relative to the provider's
    /// versioned API root: `responses`, `realtime?model=gpt-realtime`) with
    /// the target's credential.
    ///
    /// `extra_headers` are added to the handshake after vetting (see
    /// [`build_ws_request`]): pass the client's `openai-beta`,
    /// `sec-websocket-protocol` and similar; credentials among them are
    /// dropped.
    pub async fn connect_ws(
        &self,
        target: &Target,
        path_and_query: &str,
        extra_headers: &HeaderMap,
    ) -> Result<UpstreamWebSocket, UpstreamError> {
        self.connect_ws_with(
            target,
            path_and_query,
            extra_headers,
            DEFAULT_WS_CONNECT_TIMEOUT,
        )
        .await
        .map(|connection| connection.stream)
    }

    /// [`UpstreamClient::connect_ws`] with an explicit time limit for the
    /// whole establishment, also returning the handshake response headers.
    pub async fn connect_ws_with(
        &self,
        target: &Target,
        path_and_query: &str,
        extra_headers: &HeaderMap,
        connect_timeout: Duration,
    ) -> Result<WsConnection, UpstreamError> {
        let connect_timeout = if connect_timeout.is_zero() {
            DEFAULT_WS_CONNECT_TIMEOUT
        } else {
            connect_timeout
        };
        let mut built = build_ws_request(target, path_and_query, extra_headers)?;
        let mut scrubber = Scrubber::for_target(target);
        if built.needs_access_token {
            let http = self.http_clients().client(&target.proxy, connect_timeout)?;
            if let Some((_, token, _)) = self.authorize(target, &mut built, &http).await? {
                scrubber.add(&token);
            }
        }

        let url = url::Url::parse(&built.url).map_err(|_| {
            local_error(format!(
                "provider `{}` has an invalid base URL",
                target.provider
            ))
        })?;
        let secure = url.scheme() == "wss";
        let host = match url.host() {
            Some(url::Host::Domain(d)) => d.to_string(),
            Some(url::Host::Ipv4(ip)) => ip.to_string(),
            Some(url::Host::Ipv6(ip)) => ip.to_string(),
            None => {
                return Err(local_error(format!(
                    "provider `{}`: base URL has no host",
                    target.provider
                )));
            }
        };
        let port = url
            .port_or_known_default()
            .unwrap_or(if secure { 443 } else { 80 });
        let shown = authority_of(&built.url);

        let mut request = built.url.as_str().into_client_request().map_err(|e| {
            local_error(format!(
                "provider `{}`: cannot build the WebSocket request: {e}",
                target.provider
            ))
        })?;
        for (name, value) in &built.headers {
            if !is_handshake_header(name.as_str()) {
                request.headers_mut().append(name.clone(), value.clone());
            }
        }

        let proxy = match &target.proxy {
            ProxySetting::Direct => None,
            ProxySetting::Url(raw) => Some(parse_proxy_endpoint(raw)?),
            ProxySetting::Inherit => match proxy_from_env(secure, &host, port) {
                Some(raw) => Some(parse_proxy_endpoint(&raw)?),
                None => None,
            },
        };

        let deadline = deadline_after(connect_timeout);
        let tls = self.tls().ws.clone();
        tracing::debug!(
            provider = %target.provider,
            host = %shown,
            proxy = proxy.as_ref().map(|p| p.display.as_str()).unwrap_or("none"),
            "opening upstream websocket"
        );

        let io = tokio::time::timeout_at(deadline, async {
            let io = dial(&host, port, proxy.as_ref(), &tls).await?;
            if secure {
                tls_connect(io, &host, tls.clone(), false).await
            } else {
                Ok(io)
            }
        })
        .await
        .map_err(|_| timed_out(&format!("connecting to {shown}"), connect_timeout))??;

        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_WS_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_WS_MESSAGE_BYTES));
        let handshake = tokio_tungstenite::client_async_with_config(request, io, Some(config));
        let outcome = tokio::time::timeout_at(deadline, handshake)
            .await
            .map_err(|_| {
                timed_out(
                    &format!("the WebSocket handshake with {shown}"),
                    connect_timeout,
                )
            })?;

        match outcome {
            Ok((stream, response)) => Ok(WsConnection {
                stream,
                headers: response.headers().clone(),
            }),
            Err(WsError::Http(response)) => {
                let status = response.status().as_u16();
                if status < 300 {
                    // Not an error status, but not an upgrade either: the
                    // path is served by something that is not a WebSocket
                    // endpoint.
                    return Err(failure(
                        "request",
                        format!(
                            "{shown} answered HTTP {status} instead of upgrading to a WebSocket"
                        ),
                    ));
                }
                let body = response.body().as_deref().unwrap_or(&[]);
                Err(classify_response(
                    target,
                    &scrubber,
                    status,
                    response.headers(),
                    body,
                ))
            }
            Err(WsError::Io(e)) => Err(failure(
                "read",
                format!("the WebSocket handshake with {shown} was cut short: {e}"),
            )),
            // These messages can quote what the upstream sent.
            Err(other) => Err(scrubber.error(failure(
                "request",
                format!("the WebSocket handshake with {shown} failed: {other}"),
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_endpoints() {
        let p = parse_proxy_endpoint("http://user%40corp:p%3Ass@proxy.local:3128").unwrap();
        assert_eq!(p.scheme, ProxyScheme::Http);
        assert_eq!(p.host, "proxy.local");
        assert_eq!(p.port, 3128);
        assert_eq!(
            p.credentials,
            Some(("user@corp".to_string(), "p:ss".to_string()))
        );
        assert_eq!(p.display, "http://redacted@proxy.local:3128");
        assert_eq!(
            format!("{p:?}"),
            "ProxyEndpoint(http://redacted@proxy.local:3128)"
        );

        let p = parse_proxy_endpoint("https://secure-proxy.example").unwrap();
        assert_eq!((p.scheme, p.port), (ProxyScheme::Https, 443));
        assert_eq!(p.credentials, None);
        let p = parse_proxy_endpoint("http://plain-proxy.example").unwrap();
        assert_eq!(p.port, 80);
        let p = parse_proxy_endpoint("socks5://10.0.0.1").unwrap();
        assert_eq!((p.scheme, p.port), (ProxyScheme::Socks5, 1080));
        let p = parse_proxy_endpoint("socks5h://[::1]:9050").unwrap();
        assert_eq!(
            (p.scheme, p.host.as_str(), p.port),
            (ProxyScheme::Socks5h, "::1", 9050)
        );

        let err = parse_proxy_endpoint("ftp://u:secret@x.test").unwrap_err();
        assert!(!err.info.message.contains("secret"), "{}", err.info.message);
        assert!(parse_proxy_endpoint("not a url").is_err());
    }

    #[test]
    fn address_families_alternate_starting_with_the_resolvers_choice() {
        let addr = |s: &str| s.parse::<SocketAddr>().unwrap();
        let v6a = addr("[2001:db8::1]:443");
        let v6b = addr("[2001:db8::2]:443");
        let v4a = addr("192.0.2.1:443");
        let v4b = addr("192.0.2.2:443");
        let v4c = addr("192.0.2.3:443");
        assert_eq!(
            interleave_families(vec![v6a, v6b, v4a, v4b, v4c]),
            vec![v6a, v4a, v6b, v4b, v4c]
        );
        assert_eq!(
            interleave_families(vec![v4a, v4b, v6a]),
            vec![v4a, v6a, v4b]
        );
        assert_eq!(interleave_families(vec![v4a, v4b]), vec![v4a, v4b]);
        assert_eq!(interleave_families(Vec::new()), Vec::new());
    }

    #[tokio::test]
    async fn a_refused_address_falls_through_to_the_next_one() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = listener.local_addr().unwrap();
        let dead = {
            let gone = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            gone.local_addr().unwrap()
        };
        // A refusal starts the next attempt at once; the fallback delay
        // (far longer than the test may take) is not waited for.
        let started = std::time::Instant::now();
        let stream = tokio::time::timeout(
            Duration::from_secs(20),
            connect_any(vec![dead, live], Duration::from_secs(3600)),
        )
        .await
        .expect("did not wait for the fallback delay")
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), live);
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[tokio::test]
    async fn a_silent_address_is_overtaken_after_the_fallback_delay() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = listener.local_addr().unwrap();
        // TEST-NET-1 (RFC 5737) is never routed: the attempt either hangs or
        // fails at once on a machine without a route. Both must end at the
        // live address well before any connect timeout.
        let silent: SocketAddr = "192.0.2.1:9".parse().unwrap();
        let stream = tokio::time::timeout(
            Duration::from_secs(5),
            connect_any(vec![silent, live], Duration::from_millis(50)),
        )
        .await
        .expect("the second address was tried while the first was pending")
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), live);
    }

    #[tokio::test]
    async fn when_every_address_fails_the_first_failure_is_reported() {
        let dead = {
            let gone = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            gone.local_addr().unwrap()
        };
        let error = connect_any(vec![dead, dead], Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
        let error = connect_any(Vec::new(), Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AddrNotAvailable);

        let failure = tcp_connect(&dead.ip().to_string(), dead.port(), true)
            .await
            .unwrap_err();
        assert!(
            failure
                .info
                .message
                .starts_with(&format!("connect: could not connect to proxy {dead}: ")),
            "{}",
            failure.info.message
        );
    }

    #[test]
    fn authorities_bracket_ipv6() {
        assert_eq!(authority("api.openai.com", 443), "api.openai.com:443");
        assert_eq!(authority("::1", 8080), "[::1]:8080");
    }

    #[tokio::test]
    async fn connect_tunnel_request_and_responses() {
        let proxy = parse_proxy_endpoint("http://user:pass@proxy.test:3128").unwrap();

        // Success: the request is well-formed and nothing beyond the head
        // is consumed.
        let (mut ours, mut theirs) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            while !seen.ends_with(b"\r\n\r\n") {
                seen.push(theirs.read_u8().await.unwrap());
            }
            theirs
                .write_all(b"HTTP/1.1 200 Connection established\r\nVia: test\r\n\r\nEXTRA")
                .await
                .unwrap();
            (String::from_utf8(seen).unwrap(), theirs)
        });
        http_connect_tunnel(&mut ours, "api.openai.com", 443, &proxy)
            .await
            .unwrap();
        let (request, _keep_open) = server.await.unwrap();
        assert!(
            request.starts_with("CONNECT api.openai.com:443 HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(request.contains("\r\nHost: api.openai.com:443\r\n"));
        // base64("user:pass")
        assert!(request.contains("\r\nProxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
        let mut extra = [0u8; 5];
        ours.read_exact(&mut extra).await.unwrap();
        assert_eq!(&extra, b"EXTRA");

        // Refusals.
        for (response, needle) in [
            (
                "HTTP/1.1 407 Proxy Authentication Required\r\n\r\n",
                "requires authentication",
            ),
            ("HTTP/1.1 403 Forbidden\r\n\r\n", "refused the tunnel"),
            ("garbage\r\n\r\n", "invalid response"),
        ] {
            let (mut ours, mut theirs) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                let _ = theirs.read(&mut buffer).await;
                let _ = theirs.write_all(response.as_bytes()).await;
                // Keep the pipe open until the client has read the answer.
                let _ = theirs.read(&mut buffer).await;
            });
            let err = http_connect_tunnel(&mut ours, "h.test", 443, &proxy)
                .await
                .unwrap_err();
            assert!(err.info.message.contains(needle), "{}", err.info.message);
            assert!(err.info.message.starts_with("connect: "));
            assert!(!err.info.message.contains("pass"), "{}", err.info.message);
        }
    }

    #[tokio::test]
    async fn socks5_with_password_and_domain() {
        let proxy = parse_proxy_endpoint("socks5h://alice:wonder@socks.test:1080").unwrap();
        let (mut ours, mut theirs) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let mut greeting = [0u8; 4];
            theirs.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [0x05, 0x02, 0x00, 0x02]);
            theirs.write_all(&[0x05, 0x02]).await.unwrap();
            let mut auth = [0u8; 1 + 1 + 5 + 1 + 6];
            theirs.read_exact(&mut auth).await.unwrap();
            assert_eq!(&auth[..], b"\x01\x05alice\x06wonder");
            theirs.write_all(&[0x01, 0x00]).await.unwrap();
            let mut request = [0u8; 4 + 1 + 14 + 2];
            theirs.read_exact(&mut request).await.unwrap();
            assert_eq!(&request[..5], &[0x05, 0x01, 0x00, 0x03, 14]);
            assert_eq!(&request[5..19], b"api.openai.com");
            assert_eq!(&request[19..], &443u16.to_be_bytes());
            theirs
                .write_all(&[0x05, 0x00, 0x00, 0x01, 10, 0, 0, 1, 0x1f, 0x90])
                .await
                .unwrap();
            theirs
        });
        socks5_connect(
            &mut ours,
            SocksAddr::Domain("api.openai.com".into()),
            443,
            &proxy,
        )
        .await
        .unwrap();
        let _keep_open = server.await.unwrap();
    }

    #[tokio::test]
    async fn socks5_failures_are_explained() {
        let proxy = parse_proxy_endpoint("socks5://socks.test:1080").unwrap();

        // The proxy cannot reach the destination.
        let (mut ours, mut theirs) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut greeting = [0u8; 3];
            theirs.read_exact(&mut greeting).await.unwrap();
            theirs.write_all(&[0x05, 0x00]).await.unwrap();
            let mut request = [0u8; 10];
            theirs.read_exact(&mut request).await.unwrap();
            assert_eq!(&request[..4], &[0x05, 0x01, 0x00, 0x01]);
            theirs
                .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            let mut rest = [0u8; 8];
            let _ = theirs.read(&mut rest).await;
        });
        let err = socks5_connect(
            &mut ours,
            SocksAddr::Ip("10.1.2.3".parse().unwrap()),
            443,
            &proxy,
        )
        .await
        .unwrap_err();
        assert!(
            err.info.message.contains("connection refused"),
            "{}",
            err.info.message
        );

        // The proxy insists on credentials we do not have.
        let (mut ours, mut theirs) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut greeting = [0u8; 3];
            theirs.read_exact(&mut greeting).await.unwrap();
            theirs.write_all(&[0x05, 0xff]).await.unwrap();
            let mut rest = [0u8; 8];
            let _ = theirs.read(&mut rest).await;
        });
        let err = socks5_connect(&mut ours, SocksAddr::Domain("x.test".into()), 443, &proxy)
            .await
            .unwrap_err();
        assert!(
            err.info.message.contains("authentication methods"),
            "{}",
            err.info.message
        );

        // Not a SOCKS server at all.
        let (mut ours, mut theirs) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut greeting = [0u8; 3];
            theirs.read_exact(&mut greeting).await.unwrap();
            theirs
                .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
                .await
                .unwrap();
            let mut rest = [0u8; 8];
            let _ = theirs.read(&mut rest).await;
        });
        let err = socks5_connect(&mut ours, SocksAddr::Domain("x.test".into()), 443, &proxy)
            .await
            .unwrap_err();
        assert!(
            err.info.message.contains("does not speak SOCKS5"),
            "{}",
            err.info.message
        );
    }

    #[test]
    fn handshake_headers_are_reserved() {
        for name in [
            "host",
            "connection",
            "upgrade",
            "sec-websocket-key",
            "sec-websocket-version",
        ] {
            assert!(is_handshake_header(name), "{name}");
        }
        for name in [
            "authorization",
            "openai-beta",
            "sec-websocket-protocol",
            "user-agent",
        ] {
            assert!(!is_handshake_header(name), "{name}");
        }
    }
}
