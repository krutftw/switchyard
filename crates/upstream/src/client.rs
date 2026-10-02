//! Sending requests to upstream providers.
//!
//! [`UpstreamClient::send`] is the one entry point for HTTP calls: it builds
//! the request ([`crate::request::build_request`]), obtains an access token
//! when the target authenticates with a service account, sends, and turns
//! the outcome into either an [`UpstreamResponse`] or an [`UpstreamError`]
//! the scheduler can act on.
//!
//! # Time limits
//!
//! * Non-streaming calls are bounded by [`Timeouts::request`], measured from
//!   the moment the request is sent until the last byte of the response.
//! * Streaming calls are bounded by [`Timeouts::connect`] only. They return
//!   as soon as the response headers arrive; whoever consumes the byte
//!   stream enforces the idle timeout.
//!
//! # Errors
//!
//! Failures without an HTTP response are [`switchyard_core::FailureClass::Transport`] and
//! their message starts with the phase that failed — `connect`, `tls`,
//! `timeout`, `read` or `request` — followed by the upstream's host. URLs
//! are never quoted with their query string, and credentials never appear.
//! Timeouts carry status `408` so that
//! [`UpstreamError::to_api_error`] reports them as a gateway timeout; every
//! other transport failure has status `0`.
//!
//! An upstream may quote the credential it was shown ("Invalid API key:
//! sk-…"). Everything the call presented — the API key, a minted access
//! token, configured credential headers — is replaced by `[redacted]` in the
//! error body before it is classified, so neither the message, the kept body
//! nor the debug log can carry it.

use crate::classify::{classify, filter_response_headers};
use crate::http::HttpClients;
use crate::proxy::redact_proxy_url;
use crate::request::{BuiltRequest, build_request, local_error, redact_url};
use crate::secrets::Scrubber;
use crate::target::{Auth, Operation, Target, Timeouts};
use crate::tls::TlsConfigs;
use crate::vertex::{ServiceAccount, TokenSource};
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use http::{HeaderMap, Method};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use switchyard_core::UpstreamError;
use switchyard_core::config::ProxySetting;
use switchyard_core::util::truncate_chars;
use tokio::time::Instant;

/// Largest error body read from an upstream. Only the first
/// [`crate::classify::MAX_ERROR_BODY_BYTES`] of it are kept on the error;
/// reading a little further lets a slow error page finish cleanly so the
/// connection can be reused.
const MAX_ERROR_BODY_READ: usize = 1024 * 1024;

/// Time allowed for reading the body of a failed streaming call (streaming
/// calls have no overall deadline of their own).
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Service accounts whose token sources are remembered.
const MAX_TOKEN_SOURCES: usize = 256;

/// The body of a successful upstream response.
pub enum UpstreamBody {
    /// The complete body (non-streaming calls).
    Full(Bytes),
    /// The body as it arrives (streaming calls). Dropping the stream closes
    /// the upstream connection.
    Stream(BoxStream<'static, Result<Bytes, UpstreamError>>),
}

impl UpstreamBody {
    /// Reads the whole body, whichever form it is in.
    pub async fn collect(self) -> Result<Bytes, UpstreamError> {
        match self {
            UpstreamBody::Full(bytes) => Ok(bytes),
            UpstreamBody::Stream(mut stream) => {
                let mut out = bytes::BytesMut::new();
                while let Some(chunk) = stream.next().await {
                    out.extend_from_slice(&chunk?);
                }
                Ok(out.freeze())
            }
        }
    }

    /// Whether this is a byte stream.
    pub fn is_stream(&self) -> bool {
        matches!(self, UpstreamBody::Stream(_))
    }
}

impl fmt::Debug for UpstreamBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpstreamBody::Full(bytes) => write!(f, "Full({} bytes)", bytes.len()),
            UpstreamBody::Stream(_) => f.write_str("Stream(..)"),
        }
    }
}

/// A 2xx upstream response.
#[derive(Debug)]
pub struct UpstreamResponse {
    pub status: u16,
    /// Response headers with hop-by-hop, cookie, framing and CORS headers
    /// removed (see [`filter_response_headers`]).
    pub headers: HeaderMap,
    pub body: UpstreamBody,
}

impl UpstreamResponse {
    /// The response's `Content-Type`, lower-cased, without parameters.
    pub fn media_type(&self) -> Option<String> {
        let value = self
            .headers
            .get(http::header::CONTENT_TYPE)?
            .to_str()
            .ok()?;
        let media = value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        (!media.is_empty()).then_some(media)
    }
}

/// Which part of a call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Connect,
    Tls,
    ConnectTimeout,
    RequestTimeout,
    Read,
    Request,
}

/// The chain of causes of a reqwest error as one line, without URLs that
/// carry a query string.
pub(crate) fn describe_transport(error: &reqwest::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string();
        // Wrappers usually repeat their cause; keep each fact once.
        if !text.is_empty() && !parts.iter().any(|seen| seen.contains(&text)) {
            parts.push(text);
        }
        source = cause.source();
    }
    if parts.is_empty() {
        parts.push(if error.is_timeout() {
            "timed out".to_string()
        } else if error.is_connect() {
            "connection failed".to_string()
        } else if error.is_body() || error.is_decode() {
            "the body could not be read".to_string()
        } else {
            "the request failed".to_string()
        });
    }
    scrub_urls(&parts.join(": "))
}

/// Replaces every URL in `text` by its redacted form.
pub(crate) fn scrub_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let next = ["https://", "http://", "wss://", "ws://"]
            .iter()
            .filter_map(|scheme| rest.find(scheme))
            .min();
        let Some(start) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '>' | ','))
            .unwrap_or(tail.len());
        out.push_str(&redact_url(&tail[..end]));
        rest = &tail[end..];
    }
}

/// Whether `error` is a rustls error, possibly wrapped in `io::Error`s.
///
/// The TLS layers report through `io::Error::new(kind, tls_error)` — more
/// than once on the way up — and `io::Error::source()` skips the wrapped
/// error itself, so the wrappers have to be opened by hand.
fn is_tls_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = error;
    // Wrappers are two or three deep in practice.
    for _ in 0..8 {
        if current.is::<rustls::Error>() {
            return true;
        }
        match current
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
        {
            Some(inner) => current = inner,
            None => return false,
        }
    }
    false
}

/// Whether a rustls error is anywhere among the causes of `error`.
fn caused_by_tls(error: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if is_tls_error(cause) {
            return true;
        }
        source = cause.source();
    }
    false
}

fn phase_of(error: &reqwest::Error) -> Phase {
    if error.is_connect() {
        if error.is_timeout() {
            return Phase::ConnectTimeout;
        }
        // Fall back on the wording for TLS failures raised by other layers
        // (a proxy's own TLS, a different TLS backend).
        let detail = describe_transport(error).to_ascii_lowercase();
        let tls = caused_by_tls(error)
            || [
                "certificate",
                "tls handshake",
                "invalid peer",
                "fatal alert",
            ]
            .iter()
            .any(|marker| detail.contains(marker));
        return if tls { Phase::Tls } else { Phase::Connect };
    }
    if error.is_timeout() {
        Phase::RequestTimeout
    } else if error.is_body() || error.is_decode() {
        Phase::Read
    } else {
        Phase::Request
    }
}

fn human(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{} s", duration.as_secs())
    } else {
        format!("{} ms", duration.as_millis())
    }
}

/// `host[:port]` of a URL, for messages.
pub(crate) fn authority_of(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => match (parsed.host_str(), parsed.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            _ => "the upstream".to_string(),
        },
        Err(_) => "the upstream".to_string(),
    }
}

fn via(proxy: &ProxySetting) -> String {
    match proxy {
        ProxySetting::Url(url) => format!(" (via proxy {})", redact_proxy_url(url)),
        _ => String::new(),
    }
}

/// Builds the transport error for a failed call to `url`.
fn transport_error(
    error: &reqwest::Error,
    url: &str,
    proxy: &ProxySetting,
    timeouts: Timeouts,
    reading_body: bool,
) -> UpstreamError {
    let host = authority_of(url);
    let via = via(proxy);
    let detail = describe_transport(error);
    let mut phase = phase_of(error);
    if reading_body && phase == Phase::Request {
        phase = Phase::Read;
    }
    let message = match phase {
        Phase::Connect => format!("connect: could not connect to {host}{via}: {detail}"),
        Phase::Tls => format!("tls: handshake with {host}{via} failed: {detail}"),
        Phase::ConnectTimeout => format!(
            "timeout: connecting to {host}{via} took longer than {}",
            human(timeouts.connect)
        ),
        // No deadline of ours expired (streams have none): the connection
        // itself timed out, e.g. an unanswered HTTP/2 keep-alive ping.
        Phase::RequestTimeout if timeouts.request.is_zero() => {
            format!("timeout: the connection to {host}{via} stopped responding")
        }
        Phase::RequestTimeout if reading_body => format!(
            "timeout: {host} did not finish its response within {}",
            human(timeouts.request)
        ),
        Phase::RequestTimeout => format!(
            "timeout: {host} did not answer within {}",
            human(timeouts.request)
        ),
        Phase::Read => format!("read: the response from {host} was cut short: {detail}"),
        Phase::Request => format!("request: the call to {host}{via} failed: {detail}"),
    };
    let mut out = UpstreamError::transport(message);
    if matches!(phase, Phase::ConnectTimeout | Phase::RequestTimeout) {
        // See the module docs: lets `to_api_error` answer 504.
        out.status = 408;
    }
    out
}

/// The instant `limit` from now. A limit too large for the clock to
/// represent (a configured timeout of `u64::MAX` seconds, `Duration::MAX`
/// for "no limit") means "practically never" instead of panicking in
/// `Instant + Duration`.
pub(crate) fn deadline_after(limit: Duration) -> Instant {
    /// Thirty years: far enough to never fire, near enough for any clock.
    const FAR_FUTURE: Duration = Duration::from_secs(30 * 365 * 24 * 3600);
    let now = Instant::now();
    // The fallback is unreachable on any real clock; it only keeps the
    // function free of a panicking path.
    now.checked_add(limit.min(FAR_FUTURE)).unwrap_or(now)
}

/// Classifies a non-2xx answer after removing the call's own credentials
/// from its body, and records it in the debug log.
pub(crate) fn classify_response(
    target: &Target,
    scrubber: &Scrubber,
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
) -> UpstreamError {
    let text = String::from_utf8_lossy(body);
    // Scrubbed before classification, not after: the copy kept on the error
    // is truncated, and a key cut in half by the truncation could no longer
    // be recognised.
    let clean = scrubber.text(&text);
    let error = classify(
        target.kind,
        target.protocol,
        status,
        headers,
        clean.as_bytes(),
    );
    tracing::debug!(
        provider = %target.provider,
        provider_kind = %target.kind,
        protocol = %target.protocol,
        status,
        class = ?error.class,
        retry_after_ms = error.retry_after_ms,
        error_type = error.info.error_type.as_deref().unwrap_or(""),
        "upstream call failed: {}",
        truncate_chars(&error.info.message, 300)
    );
    error
}

/// Reads at most `cap` bytes of a response body, giving up quietly on
/// errors: a truncated error body is still worth classifying.
async fn read_capped(mut response: reqwest::Response, cap: usize, limit: Duration) -> Vec<u8> {
    let deadline = deadline_after(limit);
    let mut out = Vec::new();
    while out.len() < cap {
        match tokio::time::timeout_at(deadline, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                let room = cap - out.len();
                out.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
        }
    }
    out
}

/// Sends requests to upstream providers.
///
/// Cheap to share behind an [`Arc`]; holds the HTTP connection pools and
/// the access-token caches.
pub struct UpstreamClient {
    http: HttpClients,
    tls: TlsConfigs,
    tokens: Mutex<HashMap<String, Arc<TokenSource>>>,
}

impl fmt::Debug for UpstreamClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamClient")
            .field("http", &self.http)
            .field("token_sources", &self.tokens.lock().len())
            .finish()
    }
}

impl UpstreamClient {
    /// A client using the process-wide TLS configuration.
    pub fn new() -> Result<UpstreamClient, UpstreamError> {
        let tls = crate::tls::shared()
            .map_err(|e| local_error(format!("tls: cannot build the TLS configuration: {e}")))?;
        Ok(UpstreamClient::with_tls(tls.clone()))
    }

    /// A client using the given TLS configurations.
    pub fn with_tls(tls: TlsConfigs) -> UpstreamClient {
        UpstreamClient {
            http: HttpClients::with_tls(tls.http.clone()),
            tls,
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// The HTTP client cache.
    pub fn http_clients(&self) -> &HttpClients {
        &self.http
    }

    pub(crate) fn tls(&self) -> &TlsConfigs {
        &self.tls
    }

    /// The token source for a service account. Sources are remembered by
    /// the account's identity (e-mail, token endpoint, key fingerprint), so
    /// re-parsing the same key file keeps its cached token.
    pub fn token_source(&self, account: &Arc<ServiceAccount>) -> Arc<TokenSource> {
        let key = account.cache_key();
        let mut tokens = self.tokens.lock();
        if let Some(source) = tokens.get(&key) {
            return source.clone();
        }
        if tokens.len() >= MAX_TOKEN_SOURCES {
            tokens.clear();
        }
        let source = Arc::new(TokenSource::new(account.clone()));
        tokens.insert(key, source.clone());
        source
    }

    /// An OAuth access token for a service-account target, minted through
    /// the target's proxy. `None` for targets that authenticate otherwise.
    pub async fn access_token(
        &self,
        target: &Target,
        connect_timeout: Duration,
    ) -> Result<Option<String>, UpstreamError> {
        let Auth::ServiceAccount(account) = &target.auth else {
            return Ok(None);
        };
        let http = self.http.client(&target.proxy, connect_timeout)?;
        self.token_source(account).token(&http).await.map(Some)
    }

    /// Installs the bearer token on `built` when the target needs one.
    /// Returns the source and token used, and whether the token is fresh.
    pub(crate) async fn authorize(
        &self,
        target: &Target,
        built: &mut BuiltRequest,
        http: &reqwest::Client,
    ) -> Result<Option<(Arc<TokenSource>, String, bool)>, UpstreamError> {
        if !built.needs_access_token {
            return Ok(None);
        }
        let Auth::ServiceAccount(account) = &target.auth else {
            return Err(local_error(format!(
                "provider `{}`: no credential to mint an access token from",
                target.provider
            )));
        };
        let source = self.token_source(account);
        let (token, fresh) = source.token_and_origin(http).await?;
        built.set_bearer(&token)?;
        Ok(Some((source, token, fresh)))
    }

    /// Performs `op` against `target`.
    ///
    /// `body` is sent as given; `client_headers` are the headers of the
    /// client's request to the gateway, of which only an allow-list is
    /// forwarded.
    ///
    /// A 2xx answer yields an [`UpstreamResponse`] — with the complete body,
    /// or for [`Operation::Generate`] with `stream: true` a byte stream that
    /// starts as soon as the response headers have arrived. Anything else
    /// yields an [`UpstreamError`] built by [`classify`] (non-2xx) or
    /// describing the transport failure.
    pub async fn send(
        &self,
        target: &Target,
        op: &Operation,
        body: Bytes,
        client_headers: &HeaderMap,
        timeouts: Timeouts,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let built = build_request(target, op, &body, client_headers)?;
        self.send_built(target, built, body, op.is_stream(), timeouts)
            .await
    }

    /// Sends an already described request. Used by [`UpstreamClient::send`]
    /// and by model discovery.
    pub(crate) async fn send_built(
        &self,
        target: &Target,
        mut built: BuiltRequest,
        body: Bytes,
        stream: bool,
        timeouts: Timeouts,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let http = self.http.client(&target.proxy, timeouts.connect)?;
        let token = self.authorize(target, &mut built, &http).await?;
        let mut scrubber = Scrubber::for_target(target);
        if let Some((_, used, _)) = &token {
            scrubber.add(used);
        }
        let first = execute(
            &http,
            target,
            &built,
            body.clone(),
            stream,
            timeouts,
            &scrubber,
        )
        .await;

        // A cached token the upstream no longer accepts (revoked, or expired
        // early): mint a new one and try once more. A token minted for this
        // very call is not retried — the account itself is the problem.
        match (first, token) {
            (Err(error), Some((source, used, false))) if error.status == 401 => {
                source.invalidate(&used).await;
                let (fresh, _) = source.token_and_origin(&http).await?;
                built.set_bearer(&fresh)?;
                scrubber.add(&fresh);
                execute(&http, target, &built, body, stream, timeouts, &scrubber).await
            }
            (result, _) => result,
        }
    }
}

async fn execute(
    http: &reqwest::Client,
    target: &Target,
    built: &BuiltRequest,
    body: Bytes,
    stream: bool,
    timeouts: Timeouts,
    scrubber: &Scrubber,
) -> Result<UpstreamResponse, UpstreamError> {
    tracing::debug!(
        provider = %target.provider,
        method = %built.method,
        url = %redact_url(&built.url),
        stream,
        "upstream request"
    );
    let mut request = http
        .request(built.method.clone(), built.url.as_str())
        .headers(built.headers.clone());
    let bodyless = matches!(built.method, Method::GET | Method::HEAD) && body.is_empty();
    if !bodyless {
        request = request.body(body);
    }
    // Streams have no overall deadline; error messages must not name one.
    let timeouts = if stream {
        Timeouts {
            request: Duration::ZERO,
            ..timeouts
        }
    } else {
        timeouts
    };
    if !timeouts.request.is_zero() {
        request = request.timeout(timeouts.request);
    }

    let response = request
        .send()
        .await
        .map_err(|e| transport_error(&e, &built.url, &target.proxy, timeouts, false))?;
    let status = response.status();
    let headers = response.headers().clone();

    if !status.is_success() {
        let limit = if timeouts.request.is_zero() {
            ERROR_BODY_TIMEOUT
        } else {
            timeouts.request
        };
        let body = read_capped(response, MAX_ERROR_BODY_READ, limit).await;
        return Err(classify_response(
            target,
            scrubber,
            status.as_u16(),
            &headers,
            &body,
        ));
    }

    let headers = filter_response_headers(&headers);
    if stream {
        let url = built.url.clone();
        let proxy = target.proxy.clone();
        let bytes = response
            .bytes_stream()
            .map(move |chunk| chunk.map_err(|e| transport_error(&e, &url, &proxy, timeouts, true)));
        return Ok(UpstreamResponse {
            status: status.as_u16(),
            headers,
            body: UpstreamBody::Stream(bytes.boxed()),
        });
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| transport_error(&e, &built.url, &target.proxy, timeouts, true))?;
    Ok(UpstreamResponse {
        status: status.as_u16(),
        headers,
        body: UpstreamBody::Full(bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server that answers a TLS ClientHello with plain HTTP.
    async fn plain_http_server() -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut hello = [0u8; 4096];
                    let _ = socket.read(&mut hello).await;
                    let _ = socket
                        .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                        .await;
                    // Give the client time to read the answer before the
                    // connection goes away.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn tls_failures_are_recognised_through_io_wrappers() {
        let addr = plain_http_server().await;
        let http = HttpClients::new()
            .unwrap()
            .client(&ProxySetting::Direct, Duration::from_secs(5))
            .unwrap();
        let url = format!("https://{addr}/v1/x?key=secret-in-query");
        let error = http.get(&url).send().await.unwrap_err();
        assert!(caused_by_tls(&error), "{error:?}");
        assert_eq!(phase_of(&error), Phase::Tls);

        let described = transport_error(
            &error,
            &url,
            &ProxySetting::Direct,
            Timeouts::default(),
            false,
        );
        let message = &described.info.message;
        assert!(
            message.starts_with(&format!("tls: handshake with {addr} failed: ")),
            "{message}"
        );
        assert!(!message.contains("secret-in-query"), "{message}");
        assert_eq!(described.status, 0);
    }

    #[test]
    fn io_wrapped_tls_errors_are_found_at_any_depth() {
        let tls = rustls::Error::General("boom".into());
        assert!(is_tls_error(&tls));
        let once = std::io::Error::new(std::io::ErrorKind::InvalidData, tls);
        let twice = std::io::Error::other(once);
        assert!(is_tls_error(&twice));
        let plain = std::io::Error::other("connection reset");
        assert!(!is_tls_error(&plain));
    }

    #[test]
    fn urls_in_messages_lose_their_query() {
        assert_eq!(
            scrub_urls("error sending request for url (https://host.test/v1/x?key=SECRET&a=1)"),
            "error sending request for url (https://host.test/v1/x?<redacted>)"
        );
        assert_eq!(
            scrub_urls("a http://u:p@h.test:8080/p b wss://w.test/x?y=1"),
            "a http://h.test:8080/p b wss://w.test/x?<redacted>"
        );
        assert_eq!(scrub_urls("no urls here"), "no urls here");
        assert_eq!(scrub_urls(""), "");
    }

    #[test]
    fn authorities() {
        assert_eq!(
            authority_of("https://api.openai.com/v1/responses?x=1"),
            "api.openai.com"
        );
        assert_eq!(authority_of("http://127.0.0.1:8080/v1"), "127.0.0.1:8080");
        assert_eq!(authority_of("nonsense"), "the upstream");
    }

    #[tokio::test]
    async fn deadlines_never_overflow() {
        let now = Instant::now();
        assert!(deadline_after(Duration::from_secs(5)) >= now + Duration::from_secs(5));
        assert!(deadline_after(Duration::from_secs(5)) < now + Duration::from_secs(6));
        for huge in [Duration::MAX, Duration::from_secs(u64::MAX)] {
            assert!(deadline_after(huge) > now + Duration::from_secs(365 * 24 * 3600));
        }
        assert!(deadline_after(Duration::ZERO) >= now);
    }

    #[test]
    fn credentials_are_scrubbed_before_the_body_is_truncated() {
        let key = "sk-upstream-test-key-0123456789";
        let target = Target {
            provider: "p".into(),
            kind: switchyard_core::config::ProviderKind::OpenaiCompat,
            base_url: "http://127.0.0.1:1/v1".into(),
            protocol: switchyard_core::Protocol::OpenaiChat,
            model: "m".into(),
            auth: Auth::ApiKey(key.into()),
            headers: Vec::new(),
            proxy: ProxySetting::Direct,
            project: String::new(),
            location: String::new(),
        };
        let scrubber = Scrubber::for_target(&target);
        // The key straddles the 64 KiB boundary of the kept copy.
        let padding = "x".repeat(crate::classify::MAX_ERROR_BODY_BYTES - 10);
        let body = format!("{padding}{key} trailing text");
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/plain"),
        );
        let error = classify_response(&target, &scrubber, 401, &headers, body.as_bytes());
        let kept = error.body.unwrap();
        assert!(kept.len() <= crate::classify::MAX_ERROR_BODY_BYTES);
        assert!(!kept.contains("sk-upstream"), "a key fragment survived");
        assert!(kept.ends_with("[redacted]"), "{}", &kept[kept.len() - 20..]);
    }

    #[test]
    fn durations_for_humans() {
        assert_eq!(human(Duration::from_secs(30)), "30 s");
        assert_eq!(human(Duration::from_millis(250)), "250 ms");
    }

    #[test]
    fn body_debug_does_not_dump_contents() {
        let body = UpstreamBody::Full(Bytes::from_static(b"secret payload"));
        assert_eq!(format!("{body:?}"), "Full(14 bytes)");
        assert!(!body.is_stream());
    }

    #[tokio::test]
    async fn collect_joins_stream_chunks_and_surfaces_errors() {
        let ok = UpstreamBody::Stream(
            futures::stream::iter(vec![
                Ok(Bytes::from_static(b"ab")),
                Ok(Bytes::from_static(b"cd")),
            ])
            .boxed(),
        );
        assert!(ok.is_stream());
        assert_eq!(ok.collect().await.unwrap(), Bytes::from_static(b"abcd"));

        let failing = UpstreamBody::Stream(
            futures::stream::iter(vec![
                Ok(Bytes::from_static(b"ab")),
                Err(UpstreamError::transport("read: cut short")),
            ])
            .boxed(),
        );
        assert_eq!(
            failing.collect().await.unwrap_err().info.message,
            "read: cut short"
        );
    }

    #[test]
    fn media_type_strips_parameters() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("Text/Event-Stream; charset=utf-8"),
        );
        let response = UpstreamResponse {
            status: 200,
            headers,
            body: UpstreamBody::Full(Bytes::new()),
        };
        assert_eq!(response.media_type().as_deref(), Some("text/event-stream"));
    }
}
