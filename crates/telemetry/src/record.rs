//! Request records: one per client request, published on the event bus,
//! aggregated by the usage store, persisted as JSONL and returned verbatim by
//! the admin API (`GET /requests`, `GET /requests/{id}`).
//!
//! Timestamps are unix milliseconds, durations are milliseconds, and absent
//! values serialise as `null` so the dashboard always sees the same shape.

use crate::redact::{redact_text, redact_url};
use serde::{Deserialize, Serialize};
use switchyard_core::error::{ApiError, ErrorKind};
use switchyard_core::protocol::Protocol;
use switchyard_core::usage::Usage;
use switchyard_core::util::truncate_chars;

/// Longest error message kept in a record, in characters.
pub const MAX_ERROR_MESSAGE_CHARS: usize = 2_000;

/// A fresh request id: a UUIDv7, so ids sort by creation time.
pub fn new_request_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// The creation time (unix milliseconds) encoded in a UUIDv7 request id;
/// `None` for anything else. Lets files be found by id without an index:
/// the id says which day to look in.
pub fn request_id_time_ms(id: &str) -> Option<i64> {
    let uuid = uuid::Uuid::parse_str(id).ok()?;
    if uuid.get_version_num() != 7 {
        return None;
    }
    let (secs, nanos) = uuid.get_timestamp()?.to_unix();
    i64::try_from(secs)
        .ok()?
        .checked_mul(1_000)?
        .checked_add(i64::from(nanos / 1_000_000))
}

/// How the client is connected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// Plain request / response.
    #[default]
    Http,
    /// Server-sent events.
    Sse,
    /// A WebSocket turn or session.
    Websocket,
}

impl Transport {
    pub const fn as_str(self) -> &'static str {
        match self {
            Transport::Http => "http",
            Transport::Sse => "sse",
            Transport::Websocket => "websocket",
        }
    }
}

/// How the request was served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Client and upstream speak the same protocol; the body was forwarded.
    Passthrough,
    /// The request went through the canonical model.
    Translated,
    /// Answered by the built-in mock provider.
    Mock,
    /// Raw proxying of a side endpoint (embeddings, images, realtime, …).
    Raw,
}

impl Mode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Passthrough => "passthrough",
            Mode::Translated => "translated",
            Mode::Mock => "mock",
            Mode::Raw => "raw",
        }
    }
}

/// Who made the request. Never contains the client key itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientInfo {
    /// Stable id of the client key.
    pub key_id: Option<String>,
    /// Display name of the client key.
    pub key_name: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

/// Why a request failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordError {
    /// Machine readable class, e.g. `rate_limit`, `upstream`, `timeout`,
    /// `client_disconnect`.
    pub kind: String,
    pub message: String,
    /// HTTP status the upstream answered with, when the failure came from
    /// there.
    #[serde(default)]
    pub upstream_status: Option<u16>,
}

impl RecordError {
    /// Builds an error entry. The message is passed through secret redaction
    /// and truncated to [`MAX_ERROR_MESSAGE_CHARS`]: upstream error bodies end
    /// up here and they are stored on disk and shown in the dashboard.
    pub fn new(kind: impl Into<String>, message: impl AsRef<str>) -> Self {
        RecordError {
            kind: kind.into(),
            message: clean_message(message.as_ref()),
            upstream_status: None,
        }
    }

    pub fn with_upstream_status(mut self, status: u16) -> Self {
        self.upstream_status = Some(status);
        self
    }

    /// From the error reported to the client; `kind` is the snake_case name
    /// of the [`ErrorKind`].
    pub fn from_api(error: &ApiError) -> Self {
        RecordError::new(error_kind_name(error.kind), &error.message)
    }
}

/// The snake_case name an [`ErrorKind`] serialises as.
pub fn error_kind_name(kind: ErrorKind) -> String {
    match serde_json::to_value(kind) {
        Ok(serde_json::Value::String(name)) => name,
        _ => "internal".to_string(),
    }
}

fn clean_message(message: &str) -> String {
    truncate_chars(&redact_text(message.trim()), MAX_ERROR_MESSAGE_CHARS)
}

/// One upstream call made while serving a request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub provider: String,
    #[serde(default)]
    pub credential_id: Option<String>,
    #[serde(default)]
    pub credential_label: Option<String>,
    pub upstream_model: String,
    pub upstream_protocol: Protocol,
    /// Upstream HTTP status; `0` when no response was received.
    #[serde(default)]
    pub status: u16,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub duration_ms: u64,
}

impl Attempt {
    /// A successful attempt; chain [`Attempt::failed`] to turn it into a
    /// failure.
    pub fn new(
        provider: impl Into<String>,
        upstream_model: impl Into<String>,
        upstream_protocol: Protocol,
    ) -> Self {
        Attempt {
            provider: provider.into(),
            credential_id: None,
            credential_label: None,
            upstream_model: upstream_model.into(),
            upstream_protocol,
            status: 200,
            ok: true,
            error: None,
            duration_ms: 0,
        }
    }

    pub fn with_credential(mut self, id: impl Into<String>, label: Option<String>) -> Self {
        self.credential_id = Some(id.into());
        self.credential_label = label;
        self
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    pub fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = duration_ms;
        self
    }

    /// Marks the attempt failed. The message is redacted and truncated like
    /// [`RecordError::new`].
    pub fn failed(mut self, status: u16, error: impl AsRef<str>) -> Self {
        self.status = status;
        self.ok = false;
        self.error = Some(clean_message(error.as_ref()));
        self
    }
}

/// What is known when a request begins. Published as `request.started`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestStart {
    /// UUIDv7.
    pub id: String,
    pub started_at: i64,
    #[serde(default)]
    pub client: ClientInfo,
    pub client_protocol: Protocol,
    /// Method and route, e.g. `POST /v1/messages`.
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub transport: Transport,
    #[serde(default)]
    pub stream: bool,
    /// Model name exactly as the client sent it (reasoning suffix included).
    #[serde(default)]
    pub requested_model: String,
}

impl RequestStart {
    /// A start record with a fresh id.
    ///
    /// `endpoint` is meant to be the method and route (`POST /v1/messages`).
    /// Should a query string come along, its secret parameters (`?key=`,
    /// one of the gateway's own auth locations) are redacted.
    pub fn new(
        client_protocol: Protocol,
        endpoint: impl Into<String>,
        requested_model: impl Into<String>,
        started_at: i64,
    ) -> Self {
        let endpoint = endpoint.into();
        RequestStart {
            id: new_request_id(),
            started_at,
            client: ClientInfo::default(),
            client_protocol,
            endpoint: if endpoint.contains('?') {
                redact_url(&endpoint)
            } else {
                endpoint
            },
            transport: Transport::Http,
            stream: false,
            requested_model: requested_model.into(),
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn with_client(mut self, client: ClientInfo) -> Self {
        self.client = client;
        self
    }

    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// Sets the stream flag; a streaming HTTP request is transported as SSE.
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        if stream && self.transport == Transport::Http {
            self.transport = Transport::Sse;
        }
        self
    }
}

/// Everything recorded about one client request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestRecord {
    /// UUIDv7.
    pub id: String,
    pub started_at: i64,
    #[serde(default)]
    pub finished_at: i64,
    #[serde(default)]
    pub duration_ms: u64,
    /// Time until the first byte was sent to the client.
    #[serde(default)]
    pub ttfb_ms: Option<u64>,
    #[serde(default)]
    pub client: ClientInfo,
    pub client_protocol: Protocol,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub transport: Transport,
    #[serde(default)]
    pub stream: bool,
    /// Model name exactly as the client sent it.
    #[serde(default)]
    pub requested_model: String,
    /// Client-facing model the name resolved to (suffix removed). Usage is
    /// aggregated under this name.
    #[serde(default)]
    pub client_model: Option<String>,
    #[serde(default)]
    pub upstream_model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub credential_id: Option<String>,
    #[serde(default)]
    pub credential_label: Option<String>,
    #[serde(default)]
    pub upstream_protocol: Option<Protocol>,
    #[serde(default)]
    pub mode: Option<Mode>,
    /// HTTP status sent to the client.
    #[serde(default)]
    pub status: u16,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub error: Option<RecordError>,
    #[serde(default)]
    pub usage: Usage,
    /// Estimated cost in USD, when a price is configured for the model.
    #[serde(default)]
    pub cost: Option<f64>,
    /// Reasoning depth that was applied, as a display label (`high`,
    /// `8192`, `none`, …).
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub attempts: Vec<Attempt>,
    /// Whether request / response bodies were captured for this request.
    #[serde(default)]
    pub has_bodies: bool,
}

impl RequestRecord {
    /// The part of the record that was known when the request began.
    pub fn start(&self) -> RequestStart {
        RequestStart {
            id: self.id.clone(),
            started_at: self.started_at,
            client: self.client.clone(),
            client_protocol: self.client_protocol,
            endpoint: self.endpoint.clone(),
            transport: self.transport,
            stream: self.stream,
            requested_model: self.requested_model.clone(),
        }
    }

    /// Name usage is aggregated under: the resolved client-facing model, or
    /// the requested name when resolution never happened.
    pub fn model_name(&self) -> &str {
        match self.client_model.as_deref() {
            Some(name) if !name.is_empty() => name,
            _ if !self.requested_model.is_empty() => &self.requested_model,
            _ => UNKNOWN,
        }
    }

    /// Provider name for aggregation; [`UNKNOWN`] when the request failed
    /// before routing.
    pub fn provider_name(&self) -> &str {
        match self.provider.as_deref() {
            Some(name) if !name.is_empty() => name,
            _ => UNKNOWN,
        }
    }

    /// Client key label for aggregation: the key's name, else its id, else
    /// [`ANONYMOUS`].
    pub fn key_name(&self) -> &str {
        match (
            self.client.key_name.as_deref(),
            self.client.key_id.as_deref(),
        ) {
            (Some(name), _) if !name.is_empty() => name,
            (_, Some(id)) if !id.is_empty() => id,
            _ => ANONYMOUS,
        }
    }
}

/// Aggregation name for requests without a provider or model.
pub const UNKNOWN: &str = "unknown";
/// Aggregation name for requests made without a client key.
pub const ANONYMOUS: &str = "anonymous";

/// Fills a [`RequestRecord`] progressively while a request is served.
///
/// ```
/// use switchyard_core::Protocol;
/// use switchyard_telemetry::{Attempt, Mode, RecordBuilder, RequestStart};
///
/// let start = RequestStart::new(Protocol::Anthropic, "POST /v1/messages", "sonnet(high)", 1_000);
/// let mut builder = RecordBuilder::new(start);
/// builder.set_client_model("sonnet").set_mode(Mode::Passthrough);
/// builder.push_attempt(
///     Attempt::new("anthropic", "claude-sonnet-4-5", Protocol::Anthropic).with_duration_ms(900),
/// );
/// builder.mark_first_byte(1_250);
/// let record = builder.finish(200, 2_000);
/// assert!(record.ok);
/// assert_eq!(record.duration_ms, 1_000);
/// assert_eq!(record.ttfb_ms, Some(250));
/// assert_eq!(record.provider.as_deref(), Some("anthropic"));
/// ```
#[derive(Clone, Debug)]
pub struct RecordBuilder {
    record: RequestRecord,
}

impl RecordBuilder {
    pub fn new(start: RequestStart) -> Self {
        RecordBuilder {
            record: RequestRecord {
                id: start.id,
                started_at: start.started_at,
                finished_at: start.started_at,
                duration_ms: 0,
                ttfb_ms: None,
                client: start.client,
                client_protocol: start.client_protocol,
                endpoint: start.endpoint,
                transport: start.transport,
                stream: start.stream,
                requested_model: start.requested_model,
                client_model: None,
                upstream_model: None,
                provider: None,
                credential_id: None,
                credential_label: None,
                upstream_protocol: None,
                mode: None,
                status: 0,
                ok: false,
                error: None,
                usage: Usage::default(),
                cost: None,
                reasoning: None,
                attempts: Vec::new(),
                has_bodies: false,
            },
        }
    }

    pub fn id(&self) -> &str {
        &self.record.id
    }

    pub fn started_at(&self) -> i64 {
        self.record.started_at
    }

    /// Snapshot for the `request.started` event.
    pub fn start(&self) -> RequestStart {
        self.record.start()
    }

    /// The record as filled so far.
    pub fn record(&self) -> &RequestRecord {
        &self.record
    }

    /// Direct access for fields without a dedicated setter.
    pub fn record_mut(&mut self) -> &mut RequestRecord {
        &mut self.record
    }

    pub fn set_client(&mut self, client: ClientInfo) -> &mut Self {
        self.record.client = client;
        self
    }

    pub fn set_requested_model(&mut self, model: impl Into<String>) -> &mut Self {
        self.record.requested_model = model.into();
        self
    }

    pub fn set_client_model(&mut self, model: impl Into<String>) -> &mut Self {
        self.record.client_model = Some(model.into());
        self
    }

    pub fn set_transport(&mut self, transport: Transport) -> &mut Self {
        self.record.transport = transport;
        self
    }

    pub fn set_stream(&mut self, stream: bool) -> &mut Self {
        self.record.stream = stream;
        if stream && self.record.transport == Transport::Http {
            self.record.transport = Transport::Sse;
        }
        self
    }

    pub fn set_provider(&mut self, provider: impl Into<String>) -> &mut Self {
        self.record.provider = Some(provider.into());
        self
    }

    pub fn set_credential(&mut self, id: impl Into<String>, label: Option<String>) -> &mut Self {
        self.record.credential_id = Some(id.into());
        self.record.credential_label = label;
        self
    }

    pub fn set_upstream_model(&mut self, model: impl Into<String>) -> &mut Self {
        self.record.upstream_model = Some(model.into());
        self
    }

    pub fn set_upstream_protocol(&mut self, protocol: Protocol) -> &mut Self {
        self.record.upstream_protocol = Some(protocol);
        self
    }

    pub fn set_mode(&mut self, mode: Mode) -> &mut Self {
        self.record.mode = Some(mode);
        self
    }

    pub fn set_reasoning(&mut self, label: impl Into<String>) -> &mut Self {
        self.record.reasoning = Some(label.into());
        self
    }

    /// Replaces the usage.
    pub fn set_usage(&mut self, usage: Usage) -> &mut Self {
        self.record.usage = usage;
        self
    }

    /// Folds a later usage snapshot in (see [`Usage::merge`]).
    pub fn merge_usage(&mut self, usage: &Usage) -> &mut Self {
        self.record.usage.merge(usage);
        self
    }

    pub fn set_cost(&mut self, cost: Option<f64>) -> &mut Self {
        self.record.cost = cost;
        self
    }

    pub fn set_has_bodies(&mut self, has_bodies: bool) -> &mut Self {
        self.record.has_bodies = has_bodies;
        self
    }

    pub fn set_error(&mut self, error: RecordError) -> &mut Self {
        self.record.error = Some(error);
        self
    }

    /// Clears a previously recorded error (a later attempt succeeded).
    pub fn clear_error(&mut self) -> &mut Self {
        self.record.error = None;
        self
    }

    /// Records an upstream attempt. The record's provider, credential,
    /// upstream model and upstream protocol follow the latest attempt, so
    /// after the loop they describe the attempt that answered.
    pub fn push_attempt(&mut self, attempt: Attempt) -> &mut Self {
        self.record.provider = Some(attempt.provider.clone());
        self.record.credential_id = attempt.credential_id.clone();
        self.record.credential_label = attempt.credential_label.clone();
        self.record.upstream_model = Some(attempt.upstream_model.clone());
        self.record.upstream_protocol = Some(attempt.upstream_protocol);
        self.record.attempts.push(attempt);
        self
    }

    /// Notes when the first byte went to the client. Only the first call
    /// counts.
    pub fn mark_first_byte(&mut self, now: i64) -> &mut Self {
        if self.record.ttfb_ms.is_none() {
            self.record.ttfb_ms = Some(elapsed_ms(self.record.started_at, now));
        }
        self
    }

    /// Completes the record. `status` is the HTTP status sent to the client
    /// (`101` for a WebSocket session), or `0` when nothing was sent at all.
    ///
    /// `ok` is true when the status is 1xx–3xx and no error was set: a
    /// stream that failed after a 200 is not ok, and neither is a request
    /// that ended before any response (status `0`), so it cannot be counted
    /// as a success by accident.
    pub fn finish(mut self, status: u16, now: i64) -> RequestRecord {
        self.record.status = status;
        self.record.finished_at = now.max(self.record.started_at);
        self.record.duration_ms = elapsed_ms(self.record.started_at, now);
        self.record.ok = (100..400).contains(&status) && self.record.error.is_none();
        self.record
    }
}

fn elapsed_ms(from: i64, to: i64) -> u64 {
    u64::try_from(to.saturating_sub(from)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn sample() -> RequestRecord {
        let start = RequestStart::new(
            Protocol::Anthropic,
            "POST /v1/messages",
            "sonnet(high)",
            1_000,
        )
        .with_id("0197a8d0-0000-7000-8000-000000000001")
        .with_client(ClientInfo {
            key_id: Some("k_1".into()),
            key_name: Some("laptop".into()),
            ip: Some("127.0.0.1".into()),
            user_agent: Some("curl/8.5.0".into()),
        })
        .with_stream(true);
        let mut b = RecordBuilder::new(start);
        b.set_client_model("sonnet")
            .set_mode(Mode::Translated)
            .set_reasoning("high")
            .set_usage(Usage {
                input_tokens: 10,
                cache_read_tokens: 90,
                cache_write_tokens: 0,
                output_tokens: 40,
                reasoning_tokens: 15,
            })
            .set_cost(Some(0.0125));
        b.push_attempt(
            Attempt::new("openai", "gpt-5", Protocol::OpenaiResponses)
                .with_credential("c_a", Some("key A".into()))
                .with_duration_ms(120)
                .failed(429, "rate limited"),
        );
        b.push_attempt(
            Attempt::new("openrouter", "openai/gpt-5", Protocol::OpenaiChat)
                .with_credential("c_b", None)
                .with_duration_ms(700),
        );
        b.mark_first_byte(1_300);
        b.mark_first_byte(1_900);
        b.finish(200, 2_000)
    }

    #[test]
    fn builder_fills_the_record() {
        let r = sample();
        assert_eq!(r.duration_ms, 1_000);
        assert_eq!(r.finished_at, 2_000);
        assert_eq!(r.ttfb_ms, Some(300));
        assert!(r.ok);
        assert_eq!(r.transport, Transport::Sse);
        assert_eq!(r.provider.as_deref(), Some("openrouter"));
        assert_eq!(r.credential_id.as_deref(), Some("c_b"));
        assert_eq!(r.credential_label, None);
        assert_eq!(r.upstream_model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(r.upstream_protocol, Some(Protocol::OpenaiChat));
        assert_eq!(r.attempts.len(), 2);
        assert!(!r.attempts[0].ok);
        assert_eq!(r.attempts[0].status, 429);
    }

    #[test]
    fn json_shape_is_snake_case_with_nulls() {
        let value = serde_json::to_value(sample()).unwrap();
        assert_eq!(
            value,
            json!({
                "id": "0197a8d0-0000-7000-8000-000000000001",
                "started_at": 1000,
                "finished_at": 2000,
                "duration_ms": 1000,
                "ttfb_ms": 300,
                "client": {
                    "key_id": "k_1",
                    "key_name": "laptop",
                    "ip": "127.0.0.1",
                    "user_agent": "curl/8.5.0"
                },
                "client_protocol": "anthropic",
                "endpoint": "POST /v1/messages",
                "transport": "sse",
                "stream": true,
                "requested_model": "sonnet(high)",
                "client_model": "sonnet",
                "upstream_model": "openai/gpt-5",
                "provider": "openrouter",
                "credential_id": "c_b",
                "credential_label": null,
                "upstream_protocol": "openai-chat",
                "mode": "translated",
                "status": 200,
                "ok": true,
                "error": null,
                "usage": {
                    "input_tokens": 10,
                    "cache_read_tokens": 90,
                    "cache_write_tokens": 0,
                    "output_tokens": 40,
                    "reasoning_tokens": 15
                },
                "cost": 0.0125,
                "reasoning": "high",
                "attempts": [
                    {
                        "provider": "openai",
                        "credential_id": "c_a",
                        "credential_label": "key A",
                        "upstream_model": "gpt-5",
                        "upstream_protocol": "openai-responses",
                        "status": 429,
                        "ok": false,
                        "error": "rate limited",
                        "duration_ms": 120
                    },
                    {
                        "provider": "openrouter",
                        "credential_id": "c_b",
                        "credential_label": null,
                        "upstream_model": "openai/gpt-5",
                        "upstream_protocol": "openai-chat",
                        "status": 200,
                        "ok": true,
                        "error": null,
                        "duration_ms": 700
                    }
                ],
                "has_bodies": false
            })
        );
    }

    #[test]
    fn record_round_trips_through_json() {
        let r = sample();
        let text = serde_json::to_string(&r).unwrap();
        let back: RequestRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn minimal_json_deserialises_with_defaults() {
        let r: RequestRecord = serde_json::from_value(json!({
            "id": "x",
            "started_at": 5,
            "client_protocol": "gemini"
        }))
        .unwrap();
        assert_eq!(r.status, 0);
        assert!(!r.ok);
        assert_eq!(r.usage, Usage::default());
        assert_eq!(r.transport, Transport::Http);
        assert_eq!(r.model_name(), UNKNOWN);
        assert_eq!(r.provider_name(), UNKNOWN);
        assert_eq!(r.key_name(), ANONYMOUS);
    }

    #[test]
    fn error_makes_a_200_not_ok() {
        let mut b = RecordBuilder::new(RequestStart::new(
            Protocol::OpenaiChat,
            "POST /v1/chat/completions",
            "gpt-5",
            0,
        ));
        b.set_error(RecordError::new("upstream", "stream broke").with_upstream_status(502));
        let r = b.finish(200, 10);
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().upstream_status, Some(502));
    }

    #[test]
    fn status_400_and_up_is_not_ok() {
        let start = RequestStart::new(Protocol::OpenaiChat, "POST /v1/chat/completions", "m", 0);
        assert!(!RecordBuilder::new(start.clone()).finish(404, 1).ok);
        assert!(RecordBuilder::new(start.clone()).finish(204, 1).ok);
        // A WebSocket session that was upgraded and ran is fine.
        assert!(RecordBuilder::new(start.clone()).finish(101, 1).ok);
        // Nothing was ever sent to the client: not a success.
        assert!(!RecordBuilder::new(start).finish(0, 1).ok);
    }

    #[test]
    fn clock_going_backwards_does_not_underflow() {
        let start = RequestStart::new(
            Protocol::Gemini,
            "POST /v1beta/models/x:generateContent",
            "x",
            100,
        );
        let mut b = RecordBuilder::new(start);
        b.mark_first_byte(50);
        let r = b.finish(200, 40);
        assert_eq!(r.duration_ms, 0);
        assert_eq!(r.ttfb_ms, Some(0));
        assert_eq!(r.finished_at, 100);
    }

    #[test]
    fn error_messages_are_redacted_and_truncated() {
        let e = RecordError::new(
            "authentication",
            "Incorrect API key provided: sk-proj-abcdefghijklmnopqrstuvwxyz",
        );
        assert_eq!(e.message, "Incorrect API key provided: sk-pro…wxyz");
        let long = RecordError::new("upstream", "x".repeat(5_000));
        assert_eq!(long.message.chars().count(), MAX_ERROR_MESSAGE_CHARS + 1);
        let a = Attempt::new("p", "m", Protocol::Gemini)
            .failed(401, "bad key AIzaSyA-0123456789abcdefghijklmnopqrstu");
        assert_eq!(a.error.as_deref(), Some("bad key AIzaSy…rstu"));
        // Transport errors quote the proxy URL and the headers that were
        // sent.
        let e = RecordError::new(
            "upstream",
            "proxy http://alice:hunter2@10.0.0.1:3128 refused Proxy-Authorization: Basic YWxpY2U6aHVudGVyMg==",
        );
        assert_eq!(
            e.message,
            "proxy http://[redacted]@10.0.0.1:3128 refused Proxy-Authorization: Basic [redacted]"
        );
    }

    #[test]
    fn endpoint_query_secrets_are_redacted() {
        let start = RequestStart::new(
            Protocol::Gemini,
            "POST /v1beta/models/gemini-2.5-pro:generateContent?key=AIzaSyA-0123456789abcdefghijklmnopqrstu&alt=sse",
            "gemini-2.5-pro",
            0,
        );
        assert_eq!(
            start.endpoint,
            "POST /v1beta/models/gemini-2.5-pro:generateContent?key=AIzaSy…rstu&alt=sse"
        );
        let plain = RequestStart::new(Protocol::Anthropic, "POST /v1/messages", "sonnet", 0);
        assert_eq!(plain.endpoint, "POST /v1/messages");
    }

    #[test]
    fn api_error_kind_names() {
        let e = RecordError::from_api(&ApiError::rate_limit("slow down"));
        assert_eq!(e.kind, "rate_limit");
        assert_eq!(e.message, "slow down");
        assert_eq!(
            error_kind_name(ErrorKind::InvalidRequest),
            "invalid_request"
        );
    }

    #[test]
    fn aggregation_names() {
        let mut r = sample();
        assert_eq!(r.model_name(), "sonnet");
        assert_eq!(r.provider_name(), "openrouter");
        assert_eq!(r.key_name(), "laptop");
        r.client_model = None;
        r.client.key_name = None;
        r.provider = None;
        assert_eq!(r.model_name(), "sonnet(high)");
        assert_eq!(r.key_name(), "k_1");
        assert_eq!(r.provider_name(), UNKNOWN);
    }

    #[test]
    fn request_ids_are_uuid_v7_and_ordered() {
        let a = new_request_id();
        let b = new_request_id();
        assert_eq!(uuid::Uuid::parse_str(&a).unwrap().get_version_num(), 7);
        assert_ne!(a, b);
        let start = RequestStart::new(Protocol::OpenaiChat, "POST /v1/chat/completions", "m", 1);
        assert_eq!(start.id.len(), 36);
    }

    #[test]
    fn request_id_carries_its_creation_time() {
        let before = switchyard_core::util::now_unix_ms();
        let at = request_id_time_ms(&new_request_id()).unwrap();
        let after = switchyard_core::util::now_unix_ms();
        assert!(
            (before..=after).contains(&at),
            "{before} <= {at} <= {after}"
        );
        // The first 48 bits of a UUIDv7 are the unix-millisecond timestamp.
        assert_eq!(
            request_id_time_ms("01a0f9d1-6c00-7000-8000-000000000001"),
            Some(0x01a0_f9d1_6c00)
        );
        assert_eq!(request_id_time_ms("not-a-uuid"), None);
        // A random (version 4) UUID has no timestamp.
        assert_eq!(
            request_id_time_ms("3d813cbb-47fb-42ba-91df-831e1593ac29"),
            None
        );
    }

    #[test]
    fn enum_wire_names() {
        assert_eq!(
            serde_json::to_value(Transport::Websocket).unwrap(),
            "websocket"
        );
        assert_eq!(
            serde_json::to_value(Mode::Passthrough).unwrap(),
            "passthrough"
        );
        assert_eq!(serde_json::to_value(Mode::Raw).unwrap(), "raw");
        assert_eq!(Transport::Sse.as_str(), "sse");
        assert_eq!(Mode::Mock.as_str(), "mock");
    }
}
