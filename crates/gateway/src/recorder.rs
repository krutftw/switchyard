//! The request record of one client request, finished exactly once.
//!
//! A [`Recorder`] is created when a request arrives and owns everything
//! that must be released or published when it ends: the in-flight gauge,
//! the [`RecordBuilder`], the captured bodies. Whatever path the request
//! takes — success, a rejected key, a panic in the stream task, the server
//! dropping the future because the client went away — the record is
//! published once: by [`Recorder::finish`], or failing that by `Drop`.

use crate::failover::FinalError;
use bytes::Bytes;
use http::HeaderMap;
use std::sync::Arc;
use switchyard_core::config::Config;
use switchyard_core::util::now_unix_ms;
use switchyard_core::{ApiError, Protocol, UpstreamError, Usage};
use switchyard_scheduler::Lease;
use switchyard_telemetry::{
    CapturedBodies, GaugeGuard, RecordBuilder, RecordError, RequestRecord, RequestStart, Telemetry,
    error_kind_name, redact_headers,
};

/// Slack kept beyond the capture limit, so the body store can tell that a
/// body was cut and say so.
const CAPTURE_SLACK_BYTES: usize = 1024;

/// What an unfinished recorder reports when it is dropped.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DropOutcome {
    pub status: u16,
    pub kind: &'static str,
    pub message: &'static str,
}

impl DropOutcome {
    /// The future serving the request was dropped: the client went away.
    pub(crate) const CLIENT_GONE: DropOutcome = DropOutcome {
        status: 499,
        kind: "client_disconnect",
        message: "the client closed the request before it was answered",
    };

    /// A background task that owned the request ended without finishing it.
    pub(crate) const TASK_ENDED: DropOutcome = DropOutcome {
        status: 500,
        kind: "internal",
        message: "the task serving the request ended unexpectedly",
    };
}

/// A bounded text buffer for a body that arrives in pieces (a stream).
#[derive(Debug, Default)]
pub(crate) struct CaptureBuf {
    text: String,
    limit: usize,
}

impl CaptureBuf {
    pub(crate) fn new(limit: usize) -> Self {
        CaptureBuf {
            text: String::new(),
            limit,
        }
    }

    /// Appends `bytes` (as lossy UTF-8) until the limit is reached.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if self.text.len() >= self.limit {
            return;
        }
        let room = self.limit - self.text.len();
        let piece = String::from_utf8_lossy(bytes);
        if piece.len() <= room {
            self.text.push_str(&piece);
        } else {
            let mut cut = room;
            while cut > 0 && !piece.is_char_boundary(cut) {
                cut -= 1;
            }
            self.text.push_str(&piece[..cut]);
            // Mark the buffer full even when the cut landed short.
            self.limit = self.text.len();
        }
    }

    pub(crate) fn into_text(self) -> Option<String> {
        (!self.text.is_empty()).then_some(self.text)
    }
}

/// Whose doing a failed request is, as far as the level of its log line
/// goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    /// The gateway decided on the error itself, from the request alone:
    /// a body it cannot read, a model it does not know or the key may not
    /// use, the key's own rate limit.
    Client,
    /// What the upstreams answered, or did not: the attempt loop's final
    /// error, a stream or session that broke.
    Upstream,
}

/// The level of the `request failed` line of a request that ended with
/// `status` and an error of `kind` (the record's `error.kind`).
///
/// Failures that are the gateway's or an upstream's — any 5xx, a `429`
/// that is not the client key's own rate limit (every credential of the
/// model resting, an upstream's rate limit), a stream or WebSocket session
/// that failed after its `200` / `101` — are logged at WARN. What the
/// client brought on itself — a request the gateway or the upstream
/// refused as faulty (other 4xx), the key's own rate limit, a client that
/// went away — stays at DEBUG, where every request is logged anyway.
pub(crate) fn failure_level(status: u16, kind: &str, cause: Cause) -> tracing::Level {
    let client_side = match status {
        _ if kind == "client_disconnect" => true,
        500.. => false,
        429 => cause == Cause::Client,
        400..=499 => true,
        // A failure under a success status: the stream or session broke.
        _ => false,
    };
    if client_side {
        tracing::Level::DEBUG
    } else {
        tracing::Level::WARN
    }
}

/// Logs the final line of a failed request, at [`failure_level`].
fn log_failure(record: &RequestRecord, cause: Cause) {
    let (kind, message) = match record.error.as_ref() {
        Some(error) => (error.kind.as_str(), error.message.as_str()),
        None => ("", "(no details)"),
    };
    let attempts = record.attempts.len();
    let model = record.model_name();
    // Two macros: a `tracing` level must be known where the event is
    // declared.
    if failure_level(record.status, kind, cause) == tracing::Level::WARN {
        tracing::warn!(
            request = %record.id,
            endpoint = %record.endpoint,
            model,
            status = record.status,
            kind,
            attempts,
            "request failed: {message}"
        );
    } else {
        tracing::debug!(
            request = %record.id,
            endpoint = %record.endpoint,
            model,
            status = record.status,
            kind,
            attempts,
            "request failed: {message}"
        );
    }
}

/// See the module docs.
pub(crate) struct Recorder {
    telemetry: Telemetry,
    config: Arc<Config>,
    builder: RecordBuilder,
    bodies: CapturedBodies,
    capture: bool,
    announced: bool,
    finished: bool,
    on_drop: DropOutcome,
    cause: Cause,
    _in_flight: GaugeGuard,
}

impl Recorder {
    /// Starts recording. The request counts as in flight from here on.
    /// Nothing is published until [`Recorder::announce`].
    pub(crate) fn begin(telemetry: &Telemetry, config: Arc<Config>, start: RequestStart) -> Self {
        Recorder {
            capture: telemetry.bodies().wants(true),
            _in_flight: telemetry.track_in_flight(),
            telemetry: telemetry.clone(),
            config,
            builder: RecordBuilder::new(start),
            bodies: CapturedBodies::default(),
            announced: false,
            finished: false,
            on_drop: DropOutcome::CLIENT_GONE,
            cause: Cause::Upstream,
        }
    }

    /// The record under construction.
    pub(crate) fn builder(&mut self) -> &mut RecordBuilder {
        &mut self.builder
    }

    /// The request id.
    pub(crate) fn id(&self) -> &str {
        self.builder.id()
    }

    /// Publishes `request.started`. Only the first call does anything.
    pub(crate) fn announce(&mut self) {
        if !self.announced {
            self.announced = true;
            self.telemetry.request_started(self.builder.start());
        }
    }

    /// Whether bodies are being collected for this request.
    pub(crate) fn capturing(&self) -> bool {
        self.capture
    }

    /// The most bytes of one body worth holding on to.
    pub(crate) fn capture_limit(&self) -> usize {
        self.telemetry
            .bodies()
            .max_body_bytes()
            .saturating_add(CAPTURE_SLACK_BYTES)
    }

    /// A buffer for a body that arrives in pieces, when capturing.
    pub(crate) fn capture_buf(&self) -> Option<CaptureBuf> {
        self.capture.then(|| CaptureBuf::new(self.capture_limit()))
    }

    fn clip(&self, bytes: &[u8]) -> String {
        let mut buf = CaptureBuf::new(self.capture_limit());
        buf.push(bytes);
        buf.text
    }

    pub(crate) fn capture_client_request(&mut self, headers: &HeaderMap, body: &Bytes) {
        if self.capture {
            self.bodies.client_headers = header_map(headers);
            self.bodies.client_request = Some(self.clip(body));
        }
    }

    /// Notes that an upstream attempt with `lease` is starting.
    ///
    /// The record names the provider, credential and upstream model from
    /// here on — also when the attempt never ends because the request is
    /// abandoned half-way: the call was made, and may be billed. The bodies
    /// captured for an earlier attempt are forgotten, so what the capture
    /// shows afterwards belongs to one attempt, the last.
    pub(crate) fn begin_attempt(&mut self, lease: &Lease, protocol: Protocol) {
        self.builder
            .set_provider(lease.credential.provider.clone())
            .set_credential(
                lease.credential.id.clone(),
                Some(lease.credential.label.clone()),
            )
            .set_upstream_model(lease.upstream_model.clone())
            .set_upstream_protocol(protocol);
        self.bodies.upstream_headers.clear();
        self.bodies.upstream_request = None;
        self.bodies.upstream_response = None;
    }

    /// Captures what the upstream answered a failed attempt with: its error
    /// body (credentials already removed by the transport). A failure that
    /// came without a body — a transport error, or a stream that broke,
    /// whose events are captured as they arrive — leaves the capture as it
    /// is.
    pub(crate) fn capture_upstream_failure(&mut self, error: &UpstreamError) {
        if let Some(body) = error.body.as_deref().filter(|body| !body.is_empty()) {
            self.capture_upstream_response(body.as_bytes());
        }
    }

    pub(crate) fn capture_upstream_request(&mut self, headers: Option<&HeaderMap>, body: &[u8]) {
        if self.capture {
            self.bodies.upstream_headers = headers.map(header_map).unwrap_or_default();
            self.bodies.upstream_request = Some(self.clip(body));
        }
    }

    pub(crate) fn capture_upstream_response(&mut self, body: &[u8]) {
        if self.capture {
            self.bodies.upstream_response = Some(self.clip(body));
        }
    }

    pub(crate) fn capture_upstream_response_text(&mut self, text: Option<String>) {
        if self.capture {
            self.bodies.upstream_response = text;
        }
    }

    pub(crate) fn capture_client_response(&mut self, body: &[u8]) {
        if self.capture {
            self.bodies.client_response = Some(self.clip(body));
        }
    }

    pub(crate) fn capture_client_response_text(&mut self, text: Option<String>) {
        if self.capture {
            self.bodies.client_response = text;
        }
    }

    /// Records the error the client is told about: one the gateway decided
    /// on itself, from the request ([`Cause::Client`] for the log line).
    pub(crate) fn fail_with(&mut self, error: &ApiError, upstream_status: Option<u16>) {
        let mut entry = RecordError::from_api(error);
        entry.upstream_status = upstream_status;
        self.builder.set_error(entry);
        self.cause = Cause::Client;
    }

    /// Records the error an attempt loop ended with: what the client is
    /// told, plus what only the operator is told about it (the upstream
    /// failure behind a rest, see `failover`).
    pub(crate) fn fail_finally(&mut self, error: &FinalError) {
        let mut entry = match &error.detail {
            Some(detail) => RecordError::new(
                error_kind_name(error.api.kind),
                format!("{} ({detail})", error.api.message),
            ),
            None => RecordError::from_api(&error.api),
        };
        entry.upstream_status = error.upstream_status;
        self.builder.set_error(entry);
    }

    /// Sets the usage and the cost it implies at the configured price of
    /// the upstream model.
    pub(crate) fn set_usage(&mut self, usage: Usage) {
        self.builder.set_usage(usage);
    }

    /// What to report should the recorder be dropped unfinished.
    pub(crate) fn on_drop(&mut self, outcome: DropOutcome) {
        self.on_drop = outcome;
    }

    /// Completes and publishes the record, and logs a failed request's
    /// `request failed` line (see [`failure_level`]). `status` is the HTTP
    /// status sent to the client.
    pub(crate) fn finish(mut self, status: u16) -> Arc<RequestRecord> {
        self.publish(status)
    }

    fn publish(&mut self, status: u16) -> Arc<RequestRecord> {
        self.finished = true;
        // A request rejected before its model was known still shows up in
        // the live view: started and finished in one go.
        self.announce();
        let mut record = self.builder.clone().finish(status, now_unix_ms());
        if record.cost.is_none() && !record.usage.is_empty() {
            record.cost = record
                .upstream_model
                .as_deref()
                .and_then(|model| self.config.price_for(model))
                .map(|price| price.cost(&record.usage));
        }
        if self.capture {
            let bodies = std::mem::take(&mut self.bodies);
            record.has_bodies = self.telemetry.capture_bodies(&record, bodies);
        }
        let record = self.telemetry.finish_request(record);
        // The one log line of a failed request: whatever path it took,
        // and only once, as the record.
        if !record.ok {
            log_failure(&record, self.cause);
        }
        record
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let outcome = self.on_drop;
        self.builder
            .set_error(RecordError::new(outcome.kind, outcome.message));
        self.publish(outcome.status);
    }
}

fn header_map(headers: &HeaderMap) -> std::collections::BTreeMap<String, String> {
    redact_headers(headers.iter().map(|(name, value)| {
        let value = if value.is_sensitive() {
            std::borrow::Cow::Borrowed(switchyard_telemetry::redact::REDACTED)
        } else {
            String::from_utf8_lossy(value.as_bytes())
        };
        (name.as_str(), value)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::Protocol;
    use switchyard_core::config::PriceConfig;
    use switchyard_telemetry::{Attempt, TelemetryOptions};

    #[test]
    fn sensitive_http_headers_stay_redacted_under_arbitrary_names() {
        let mut headers = HeaderMap::new();
        let mut secret = http::HeaderValue::from_static("opaque-test-value");
        secret.set_sensitive(true);
        headers.insert("x-custom-routing-data", secret);
        headers.insert(
            "content-type",
            http::HeaderValue::from_static("application/json"),
        );
        let captured = header_map(&headers);
        assert_eq!(captured["x-custom-routing-data"], "[redacted]");
        assert_eq!(captured["content-type"], "application/json");
    }

    fn start() -> RequestStart {
        RequestStart::new(
            Protocol::OpenaiChat,
            "POST /v1/chat/completions",
            "m",
            now_unix_ms(),
        )
    }

    #[test]
    fn capture_buffer_is_bounded_and_cuts_on_a_character_boundary() {
        let mut buf = CaptureBuf::new(5);
        buf.push("ab".as_bytes());
        buf.push("cé".as_bytes()); // é is two bytes: "abcé" is exactly five
        buf.push("more".as_bytes());
        assert_eq!(buf.into_text().as_deref(), Some("abcé"));

        let mut buf = CaptureBuf::new(4);
        buf.push("abcé".as_bytes()); // é would straddle the limit
        buf.push("x".as_bytes());
        assert_eq!(buf.into_text().as_deref(), Some("abc"));

        assert_eq!(CaptureBuf::new(10).into_text(), None);
    }

    #[test]
    fn finish_publishes_once_with_cost() {
        let telemetry = Telemetry::new(TelemetryOptions::default());
        let mut events = telemetry.subscribe();
        let mut config = Config::default();
        config.pricing.push(PriceConfig {
            model: "up-*".into(),
            input: 1.0,
            output: 2.0,
            cache_read: None,
            cache_write: None,
        });
        let mut recorder = Recorder::begin(&telemetry, Arc::new(config), start());
        assert_eq!(telemetry.gauges().in_flight(), 1);
        recorder.announce();
        recorder.announce();
        recorder
            .builder()
            .push_attempt(Attempt::new("p", "up-model", Protocol::OpenaiChat));
        recorder.set_usage(Usage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            ..Usage::default()
        });
        let record = recorder.finish(200);
        assert!(record.ok);
        assert_eq!(record.cost, Some(2.0));
        assert_eq!(telemetry.gauges().in_flight(), 0);

        assert_eq!(events.try_recv().unwrap().topic(), "request.started");
        assert_eq!(events.try_recv().unwrap().topic(), "request.finished");
        assert!(events.try_recv().is_err(), "published exactly once");
    }

    #[test]
    fn failures_of_the_gateway_or_an_upstream_are_warnings() {
        use Cause::{Client, Upstream};
        use tracing::Level;
        for (status, kind, cause) in [
            (500, "internal", Upstream),
            (502, "upstream", Upstream),
            (503, "overloaded", Upstream),
            (504, "timeout", Upstream),
            // A 5xx is never the client's doing, whoever decided on it.
            (500, "internal", Client),
            // Every credential resting; an upstream's rate limit.
            (429, "rate_limit", Upstream),
            // A stream that broke after its 200, a session after its 101.
            (200, "upstream", Upstream),
            (101, "upstream", Upstream),
            (101, "aborted", Upstream),
        ] {
            assert_eq!(
                failure_level(status, kind, cause),
                Level::WARN,
                "{status} {kind} {cause:?}"
            );
        }
        for (status, kind, cause) in [
            (400, "invalid_request", Client),
            (401, "authentication", Client),
            (403, "permission", Client),
            (404, "not_found", Client),
            (413, "invalid_request", Client),
            // The client key's own rate limit.
            (429, "rate_limit", Client),
            // An upstream refused the request as faulty.
            (400, "invalid_request", Upstream),
            // The client went away, before or after the answer began.
            (499, "client_disconnect", Upstream),
            (200, "client_disconnect", Upstream),
            (101, "client_disconnect", Upstream),
        ] {
            assert_eq!(
                failure_level(status, kind, cause),
                Level::DEBUG,
                "{status} {kind} {cause:?}"
            );
        }
    }

    #[test]
    fn errors_the_gateway_decides_on_itself_are_the_clients() {
        let telemetry = Telemetry::new(TelemetryOptions::default());
        let mut recorder = Recorder::begin(&telemetry, Arc::new(Config::default()), start());
        assert_eq!(recorder.cause, Cause::Upstream);
        recorder.fail_with(&ApiError::rate_limit("slow down"), None);
        assert_eq!(recorder.cause, Cause::Client);
        let record = recorder.finish(429);
        assert_eq!(
            failure_level(
                record.status,
                &record.error.as_ref().unwrap().kind,
                Cause::Client
            ),
            tracing::Level::DEBUG
        );
    }

    #[test]
    fn a_dropped_recorder_still_publishes_its_record() {
        let telemetry = Telemetry::new(TelemetryOptions::default());
        let mut events = telemetry.subscribe();
        let recorder = Recorder::begin(&telemetry, Arc::new(Config::default()), start());
        let id = recorder.id().to_string();
        drop(recorder);
        assert_eq!(telemetry.gauges().in_flight(), 0);
        assert_eq!(events.try_recv().unwrap().topic(), "request.started");
        assert_eq!(events.try_recv().unwrap().topic(), "request.finished");
        let record = telemetry.usage().get(&id).expect("recorded");
        assert_eq!(record.status, 499);
        assert!(!record.ok);
        assert_eq!(
            record.error.as_ref().map(|e| e.kind.as_str()),
            Some("client_disconnect")
        );

        let mut recorder = Recorder::begin(&telemetry, Arc::new(Config::default()), start());
        recorder.on_drop(DropOutcome::TASK_ENDED);
        let id = recorder.id().to_string();
        drop(recorder);
        assert_eq!(telemetry.usage().get(&id).unwrap().status, 500);
    }
}
