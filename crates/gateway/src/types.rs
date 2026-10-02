//! The request and reply types of the gateway's public API.

use crate::auth::ClientIdentity;
use bytes::Bytes;
use http::{HeaderMap, Method};
use serde::Serialize;
use std::fmt;
use std::path::PathBuf;
use switchyard_config_store::ConfigStoreError;
use switchyard_core::config::ProviderKind;
use switchyard_core::{Protocol, SseEvent};
use switchyard_telemetry::Transport;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How to start a [`crate::Gateway`].
#[derive(Clone, Debug)]
pub struct GatewayOptions {
    /// The configuration file (`switchyard.toml`). It must exist and be
    /// valid; relative paths inside it resolve against its directory.
    pub config_path: PathBuf,
    /// Watch the file and apply valid edits while running. Defaults to true;
    /// with false only edits made through the config store are applied.
    pub watch_config: bool,
}

impl GatewayOptions {
    /// Options for `config_path` with file watching on.
    pub fn new(config_path: impl Into<PathBuf>) -> Self {
        GatewayOptions {
            config_path: config_path.into(),
            watch_config: true,
        }
    }

    /// Switches file watching on or off.
    pub fn watch(mut self, watch_config: bool) -> Self {
        self.watch_config = watch_config;
        self
    }
}

impl Default for GatewayOptions {
    /// `./switchyard.toml`, watched.
    fn default() -> Self {
        GatewayOptions::new("switchyard.toml")
    }
}

/// Why a gateway could not be started.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The configuration file is missing, unreadable or invalid.
    #[error("{0}")]
    Config(#[from] ConfigStoreError),
    /// The outbound HTTP stack could not be set up (TLS configuration).
    #[error("cannot set up upstream connections: {0}")]
    Upstream(String),
}

/// The places a client may have put its API key, as found on the request.
/// The gateway decides which of them counts
/// ([`crate::Gateway::authenticate`]).
#[derive(Clone, Default)]
pub struct PresentedCredentials {
    /// The `Authorization` header, verbatim.
    pub authorization: Option<String>,
    /// The `x-api-key` header.
    pub x_api_key: Option<String>,
    /// The `x-goog-api-key` header.
    pub x_goog_api_key: Option<String>,
    /// The `key` query parameter.
    pub query_key: Option<String>,
}

impl PresentedCredentials {
    /// Reads the three credential headers out of a request's headers and
    /// takes the `key` query parameter as given.
    pub fn from_headers(headers: &HeaderMap, query_key: Option<String>) -> Self {
        let text = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        PresentedCredentials {
            authorization: text("authorization"),
            x_api_key: text("x-api-key"),
            x_goog_api_key: text("x-goog-api-key"),
            query_key,
        }
    }
}

impl fmt::Debug for PresentedCredentials {
    // Which slots are filled, never what is in them.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PresentedCredentials")
            .field("authorization", &self.authorization.is_some())
            .field("x_api_key", &self.x_api_key.is_some())
            .field("x_goog_api_key", &self.x_goog_api_key.is_some())
            .field("query_key", &self.query_key.is_some())
            .finish()
    }
}

/// Request headers whose values `Debug` output shows. They describe the
/// client software and the request's format and cannot carry a credential.
///
/// Everything else is shown by name only. A deny-list would not do: the
/// client's gateway key travels in `authorization`, `x-api-key` or
/// `x-goog-api-key`, but also inside `sec-websocket-protocol` (browser
/// Realtime clients), in the original URL a reverse proxy passes along, in
/// cookies, and in whatever header a deployment's own front end invents.
const SHOWN_HEADERS: [&str; 13] = [
    "accept",
    "accept-encoding",
    "anthropic-beta",
    "anthropic-version",
    "content-encoding",
    "content-length",
    "content-type",
    "openai-beta",
    "user-agent",
    "x-stainless-lang",
    "x-stainless-package-version",
    "x-stainless-runtime",
    "x-stainless-timeout",
];

/// Stands in for a value `Debug` output does not show.
const HIDDEN: &str = "[redacted]";

/// `Debug` for a request's headers: every name, and the values of the
/// [`SHOWN_HEADERS`] only.
struct HeadersDebug<'a>(&'a HeaderMap);

impl fmt::Debug for HeadersDebug<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for (name, value) in self.0 {
            let shown = SHOWN_HEADERS.contains(&name.as_str());
            match value.to_str() {
                Ok(value) if shown => map.entry(&name.as_str(), &value),
                _ => map.entry(&name.as_str(), &format_args!("{HIDDEN}")),
            };
        }
        map.finish()
    }
}

/// `Debug` for a body: its size. Bodies are the clients' conversations —
/// megabytes of them — and have no place in a log line.
struct BodyDebug<'a>(&'a Bytes);

impl fmt::Debug for BodyDebug<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0.len())
    }
}

/// `Debug` for a query string (or a path with one): the parameter names
/// without their values, one of which may be the client's `key`.
struct QueryDebug<'a>(&'a str);

impl fmt::Debug for QueryDebug<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (path, query) = match self.0.split_once('?') {
            Some((path, query)) => (Some(path), query),
            None => (None, self.0),
        };
        f.write_str("\"")?;
        if let Some(path) = path {
            write!(f, "{}?", path.escape_debug())?;
        }
        for (index, pair) in query.split('&').enumerate() {
            if index > 0 {
                f.write_str("&")?;
            }
            match pair.split_once('=') {
                // `{model}` is the placeholder of `WsOpenRequest`.
                Some((name, "{model}")) => write!(f, "{}={{model}}", name.escape_debug())?,
                Some((name, _)) => write!(f, "{}={HIDDEN}", name.escape_debug())?,
                None => write!(f, "{}", pair.escape_debug())?,
            }
        }
        f.write_str("\"")
    }
}

/// One generation (or token-counting) request from a client.
///
/// The `Debug` output is safe to log: header values that could carry the
/// client's key are not printed, and the body is shown by size only.
#[derive(Clone)]
pub struct ClientRequest {
    /// Protocol the client speaks; decides how the body is read and how the
    /// reply — errors included — is rendered.
    pub protocol: Protocol,
    /// Method and route for the request record, e.g. `POST /v1/messages`.
    pub endpoint: String,
    /// The request body as received (after any content decoding).
    pub body: Bytes,
    /// Gemini: the model named in the URL.
    pub path_model: Option<String>,
    /// Gemini: whether the URL asks for a stream
    /// (`:streamGenerateContent`).
    pub path_stream: Option<bool>,
    /// The client's request headers. The gateway forwards only an
    /// allow-list of them upstream (`anthropic-beta`, `anthropic-version`,
    /// `openai-beta`) and reads the session and user-agent headers from
    /// them. `openai-organization` and `openai-project` are never
    /// forwarded: they name the client's own account with the vendor, not
    /// the gateway's.
    pub headers: HeaderMap,
    /// Who is asking ([`crate::Gateway::authenticate`] or
    /// [`crate::Gateway::dashboard_identity`]).
    pub identity: ClientIdentity,
    /// The client's address, for the request record.
    pub client_ip: Option<String>,
    /// How the client is connected, for the request record. A streaming
    /// request arriving as [`Transport::Http`] is recorded as SSE.
    pub transport: Transport,
    /// Request id to use; one is minted when `None`.
    pub request_id: Option<String>,
    /// Explicit session-affinity key, e.g. a WebSocket connection id. When
    /// `None` the gateway derives one from headers and the body.
    pub session: Option<String>,
    /// Cancelled by the server when the client goes away. The gateway then
    /// abandons the upstream call and records the request as cancelled.
    pub cancel: CancellationToken,
}

impl ClientRequest {
    /// A plain HTTP request with no extra headers, a fresh cancellation
    /// token and nothing taken from the URL. Fill in the public fields for
    /// anything else.
    pub fn new(
        protocol: Protocol,
        endpoint: impl Into<String>,
        body: impl Into<Bytes>,
        identity: ClientIdentity,
    ) -> Self {
        ClientRequest {
            protocol,
            endpoint: endpoint.into(),
            body: body.into(),
            path_model: None,
            path_stream: None,
            headers: HeaderMap::new(),
            identity,
            client_ip: None,
            transport: Transport::Http,
            request_id: None,
            session: None,
            cancel: CancellationToken::new(),
        }
    }
}

impl fmt::Debug for ClientRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientRequest")
            .field("protocol", &self.protocol)
            .field("endpoint", &self.endpoint)
            .field("body", &BodyDebug(&self.body))
            .field("path_model", &self.path_model)
            .field("path_stream", &self.path_stream)
            .field("headers", &HeadersDebug(&self.headers))
            .field("identity", &self.identity)
            .field("client_ip", &self.client_ip)
            .field("transport", &self.transport)
            .field("request_id", &self.request_id)
            .field("session", &self.session)
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}

/// What the gateway answers.
#[derive(Debug)]
pub enum Reply {
    /// A complete response: a success body, or an error with its status.
    Full(FullReply),
    /// A stream whose first event is ready.
    Stream(StreamReply),
}

impl Reply {
    /// The request id of either kind of reply.
    pub fn request_id(&self) -> &str {
        match self {
            Reply::Full(full) => &full.request_id,
            Reply::Stream(stream) => &stream.request_id,
        }
    }

    /// The HTTP status to answer with (`200` for a stream).
    pub fn status(&self) -> u16 {
        match self {
            Reply::Full(full) => full.status,
            Reply::Stream(_) => 200,
        }
    }
}

/// A complete response.
///
/// The `Debug` output shows the body by size only.
#[derive(Clone)]
pub struct FullReply {
    /// HTTP status.
    pub status: u16,
    /// Response headers to add (lower-case names): `x-request-id`,
    /// `retry-after`, the `x-switchyard-*` debugging headers and, when
    /// enabled, the upstream's rate-limit headers. `content-type` is not
    /// among them.
    pub headers: Vec<(String, String)>,
    /// The body's media type.
    pub content_type: String,
    /// The body, in the client's protocol.
    pub body: Bytes,
    /// Id of the request record.
    pub request_id: String,
}

impl FullReply {
    /// The value of a response header, by lower-case name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

impl fmt::Debug for FullReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FullReply")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("content_type", &self.content_type)
            .field("body", &BodyDebug(&self.body))
            .field("request_id", &self.request_id)
            .finish()
    }
}

/// A streamed response. The upstream has produced its first event, so the
/// status is `200` whatever happens next: later failures arrive in-band, in
/// the protocol's own error shape.
///
/// Dropping `events` cancels the upstream call. The channel closing means
/// the stream is over.
#[derive(Debug)]
pub struct StreamReply {
    /// Response headers to add, as for [`FullReply::headers`].
    pub headers: Vec<(String, String)>,
    /// The protocol the events are written in (the client's).
    pub protocol: Protocol,
    /// Id of the request record.
    pub request_id: String,
    /// The events, in order. Bounded: a slow client slows the upstream down.
    pub events: mpsc::Receiver<SseEvent>,
}

impl StreamReply {
    /// The value of a response header, by lower-case name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A request for an OpenAI-style side endpoint that is forwarded verbatim
/// (embeddings, image generation, speech, moderations).
///
/// The `Debug` output is safe to log, as for [`ClientRequest`]; the query
/// string is shown without its values.
#[derive(Clone)]
pub struct RawRequest {
    /// Upstream path relative to the provider's API root, e.g. `embeddings`.
    pub path: String,
    /// HTTP method to use upstream.
    pub method: Method,
    /// The body, forwarded unchanged apart from the `model` field of a JSON
    /// body.
    pub body: Bytes,
    /// `Content-Type` of the body.
    pub content_type: Option<String>,
    /// Query string to forward, without the leading `?`. Must not carry the
    /// client's gateway key.
    pub query: Option<String>,
    /// The model the request names; selects the provider.
    pub model: String,
    /// The client's request headers; forwarded as for
    /// [`ClientRequest::headers`], plus `accept`.
    pub headers: HeaderMap,
    /// Who is asking.
    pub identity: ClientIdentity,
    /// The client's address, for the request record.
    pub client_ip: Option<String>,
    /// Method and route for the request record, e.g. `POST /v1/embeddings`.
    pub endpoint: String,
    /// Cancelled by the server when the client goes away.
    pub cancel: CancellationToken,
}

impl fmt::Debug for RawRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawRequest")
            .field("path", &self.path)
            .field("method", &self.method)
            .field("body", &BodyDebug(&self.body))
            .field("content_type", &self.content_type)
            .field("query", &self.query.as_deref().map(QueryDebug))
            .field("model", &self.model)
            .field("headers", &HeadersDebug(&self.headers))
            .field("identity", &self.identity)
            .field("client_ip", &self.client_ip)
            .field("endpoint", &self.endpoint)
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}

/// A request to open a WebSocket to an upstream on behalf of a client: the
/// Realtime relay and the Responses upstream-WebSocket relay.
///
/// The `Debug` output is safe to log, as for [`ClientRequest`] — in
/// particular it does not print `sec-websocket-protocol`, where browser
/// clients put their key.
#[derive(Clone)]
pub struct WsOpenRequest {
    /// Who is asking.
    pub identity: ClientIdentity,
    /// The model the client named; selects the provider.
    pub model: String,
    /// Upstream path and query relative to the provider's API root, with
    /// `{model}` standing for the upstream model id, e.g.
    /// `realtime?model={model}` or `responses`.
    pub path_and_query: String,
    /// The client's request headers. Only an allow-list is offered to the
    /// upstream handshake: `openai-beta`, `openai-safety-identifier`,
    /// `sec-websocket-protocol` and `x-client-request-id`.
    pub headers: HeaderMap,
    /// Method and route for the request record, e.g. `GET /v1/realtime`.
    pub endpoint: String,
    /// The client's address, for the request record.
    pub client_ip: Option<String>,
    /// Only use providers of this kind (`openai` for the Realtime API).
    pub require_kind: Option<ProviderKind>,
}

impl fmt::Debug for WsOpenRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsOpenRequest")
            .field("identity", &self.identity)
            .field("model", &self.model)
            .field("path_and_query", &QueryDebug(&self.path_and_query))
            .field("headers", &HeadersDebug(&self.headers))
            .field("endpoint", &self.endpoint)
            .field("client_ip", &self.client_ip)
            .field("require_kind", &self.require_kind)
            .finish()
    }
}

/// The result of [`crate::Gateway::test_provider`]. Serialises as the body
/// of the admin API's provider test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderTest {
    /// Whether the upstream answered the test request with a response a
    /// request could be served with.
    pub ok: bool,
    /// Upstream HTTP status; `0` when no response was received. For a
    /// `2xx` whose body is not a usable response (a generation the upstream
    /// reports as failed, a body that is no response), the status that
    /// failure amounts to: `429`, `400` or `502`.
    pub status: u16,
    /// Time the call took.
    pub latency_ms: u64,
    /// Upstream model id the test was made with, when one could be chosen.
    pub model: Option<String>,
    /// Label of the credential that was used.
    pub credential: Option<String>,
    /// What went wrong. Never contains credentials.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Where the discovery of a provider's model list stands. Serialises as
/// `"off"`, `"pending"`, `"ok"` or `"failed"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscoveryStatus {
    /// The provider's upstream is not asked: the provider is disabled, has
    /// `discover = false`, lists its models itself, or is a `mock`.
    Off,
    /// The upstream is being asked, or is about to be, and has not answered
    /// since the provider's settings last changed.
    Pending,
    /// The latest listing succeeded.
    Ok,
    /// The latest listing failed; the list of an earlier success, if there
    /// was one, is still in use.
    Failed,
}

/// The discovery state of one provider, see
/// [`crate::Gateway::discovery_states`]. Every field is always serialised.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiscoveryState {
    /// Where discovery stands.
    pub state: DiscoveryStatus,
    /// Unix milliseconds: when the latest listing succeeded or failed, or —
    /// while `pending` — when it was started. `None` for `off`.
    pub at: Option<i64>,
    /// Why the latest listing failed (`failed` only): one line, credentials
    /// removed.
    pub error: Option<String>,
    /// Models in the upstream's list that is in use: the latest successful
    /// listing's, also after a later one failed. `0` when there is none.
    pub models: usize,
}

impl DiscoveryState {
    /// The state of a provider whose upstream is not asked.
    pub(crate) const OFF: DiscoveryState = DiscoveryState {
        state: DiscoveryStatus::Off,
        at: None,
        error: None,
        models: 0,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    const KEY: &str = "client-key-that-must-not-be-printed";

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("authorization", format!("Bearer {KEY}")),
            ("proxy-authorization", format!("Basic {KEY}")),
            ("x-api-key", KEY.to_string()),
            ("x-goog-api-key", KEY.to_string()),
            ("api-key", KEY.to_string()),
            ("cookie", format!("session={KEY}")),
            (
                "sec-websocket-protocol",
                format!("realtime, openai-insecure-api-key.{KEY}"),
            ),
            ("x-original-uri", format!("/v1beta/models?key={KEY}")),
            ("x-my-frontends-token", KEY.to_string()),
            ("user-agent", "client/1.0".to_string()),
            ("content-type", "application/json".to_string()),
            ("anthropic-beta", "context-1m-2025-08-07".to_string()),
        ] {
            headers.append(name, HeaderValue::from_str(&value).unwrap());
        }
        // Not valid UTF-8: shown by name like everything unknown.
        headers.append("user-agent", HeaderValue::from_bytes(b"\xff\xfe").unwrap());
        headers
    }

    #[test]
    fn debug_shows_header_names_and_only_harmless_values() {
        let shown = format!("{:?}", HeadersDebug(&headers()));
        assert!(!shown.contains(KEY), "{shown}");
        for name in [
            "authorization",
            "x-api-key",
            "x-goog-api-key",
            "cookie",
            "sec-websocket-protocol",
            "x-my-frontends-token",
        ] {
            assert!(
                shown.contains(&format!("\"{name}\": [redacted]")),
                "{shown}"
            );
        }
        assert!(shown.contains("\"user-agent\": \"client/1.0\""), "{shown}");
        assert!(shown.contains("\"user-agent\": [redacted]"), "{shown}");
        assert!(
            shown.contains("\"content-type\": \"application/json\""),
            "{shown}"
        );
        assert!(
            shown.contains("\"anthropic-beta\": \"context-1m-2025-08-07\""),
            "{shown}"
        );
    }

    #[test]
    fn debug_shows_query_names_without_values() {
        let show = |query: &str| format!("{:?}", QueryDebug(query));
        assert_eq!(
            show(&format!("api-version=2024-10-21&key={KEY}&flag")),
            "\"api-version=[redacted]&key=[redacted]&flag\""
        );
        assert_eq!(show("realtime?model={model}"), "\"realtime?model={model}\"");
        assert_eq!(
            show(&format!("realtime?model={{model}}&key={KEY}")),
            "\"realtime?model={model}&key=[redacted]\""
        );
        assert_eq!(show("responses"), "\"responses\"");
        assert_eq!(show(""), "\"\"");
    }

    #[test]
    fn debug_of_requests_and_replies_shows_bodies_by_size() {
        const SAID: &str = "what the user wrote";
        let body = Bytes::from(format!("{{\"model\":\"m\",\"input\":\"{SAID}\"}}"));
        let size = format!("body: {} bytes", body.len());

        let mut request = ClientRequest::new(
            Protocol::OpenaiChat,
            "POST /v1/chat/completions",
            body.clone(),
            ClientIdentity::anonymous(),
        );
        request.headers = headers();
        let shown = format!("{request:?}");
        assert!(!shown.contains(KEY) && !shown.contains(SAID), "{shown}");
        assert!(shown.contains(&size), "{shown}");
        assert!(shown.contains("POST /v1/chat/completions"), "{shown}");
        assert!(shown.contains("cancelled: false"), "{shown}");

        let raw = RawRequest {
            path: "embeddings".into(),
            method: Method::POST,
            body: body.clone(),
            content_type: Some("application/json".into()),
            query: Some(format!("key={KEY}")),
            model: "m".into(),
            headers: headers(),
            identity: ClientIdentity::anonymous(),
            client_ip: None,
            endpoint: "POST /v1/embeddings".into(),
            cancel: CancellationToken::new(),
        };
        let shown = format!("{raw:?}");
        assert!(!shown.contains(KEY) && !shown.contains(SAID), "{shown}");
        assert!(shown.contains(&size), "{shown}");
        assert!(shown.contains("query: Some(\"key=[redacted]\")"), "{shown}");

        let ws = WsOpenRequest {
            identity: ClientIdentity::anonymous(),
            model: "m".into(),
            path_and_query: format!("realtime?model={{model}}&key={KEY}"),
            headers: headers(),
            endpoint: "GET /v1/realtime".into(),
            client_ip: None,
            require_kind: Some(ProviderKind::Openai),
        };
        let shown = format!("{ws:?}");
        assert!(!shown.contains(KEY), "{shown}");
        assert!(shown.contains("realtime?model={model}"), "{shown}");

        let reply = Reply::Full(FullReply {
            status: 200,
            headers: vec![("x-request-id".into(), "req-1".into())],
            content_type: "application/json".into(),
            body,
            request_id: "req-1".into(),
        });
        let shown = format!("{reply:?}");
        assert!(!shown.contains(SAID), "{shown}");
        assert!(shown.contains(&size) && shown.contains("req-1"), "{shown}");
    }
}
