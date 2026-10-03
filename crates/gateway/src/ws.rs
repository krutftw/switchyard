//! Upstream WebSockets on behalf of a client: the Realtime relay and the
//! Responses upstream-WebSocket relay. The gateway picks the credential and
//! opens the socket; the server crate relays frames and tells the gateway
//! how the session ended.
//!
//! An upstream WebSocket is an optional endpoint: a handshake that is
//! refused because the upstream (or a proxy in front of it) has no such
//! endpoint, or does not let this key use it, must not take the model out
//! of rotation for HTTP requests — including the HTTP fallback the server
//! then uses for the same client. Only what the upstream's API answers and
//! would answer any call with (rate limits, exhausted quota, server faults)
//! is reported to the scheduler; see `failover`.

use crate::failover::{Failover, Failure, Limits, Next};
use crate::gateway::Inner;
use crate::generate::{
    attempt_record, check_identity, elapsed_ms, failed_attempt_record, no_route,
};
use crate::recorder::{DropOutcome, Recorder};
use crate::target::protocol_for;
use crate::types::WsOpenRequest;
use http::HeaderMap;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use std::sync::Arc;
use std::time::{Duration, Instant};
use switchyard_core::util::now_unix_ms;
use switchyard_core::{ApiError, FailureClass, Protocol, UpstreamError, Usage};
use switchyard_scheduler::{Lease, Outcome};
use switchyard_telemetry::{Mode, RecordError, Transport};
use switchyard_upstream::{Target, UpstreamWebSocket};
use tokio_util::sync::CancellationToken;

/// Protocol WebSocket sessions are recorded and failed over under: both
/// relays carry OpenAI Responses / Realtime frames.
const WS_PROTOCOL: Protocol = Protocol::OpenaiResponses;

/// Status recorded for an established session.
const SWITCHING_PROTOCOLS: u16 = 101;

/// Characters of a model id that go into a query string unescaped.
const QUERY_SAFE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Client headers that may be offered to an upstream WebSocket handshake.
/// Everything else the client sent — its gateway key, cookies, `origin`,
/// forwarding headers, its own user agent — stays with the gateway. So do
/// `openai-organization` and `openai-project`: they name the client's own
/// account, and next to the gateway's key the upstream answers them with a
/// `401`.
const HANDSHAKE_HEADERS: [&str; 4] = [
    "openai-beta",
    "openai-safety-identifier",
    "sec-websocket-protocol",
    "x-client-request-id",
];

/// The part of the client's headers that goes into the upstream handshake.
fn handshake_headers(client: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in HANDSHAKE_HEADERS {
        for value in client.get_all(name) {
            out.append(http::HeaderName::from_static(name), value.clone());
        }
    }
    out
}

/// Whether a failed handshake is a failure of the WebSocket *path* rather
/// than an answer of the upstream's API: the connection could not be
/// established, the upstream asked for a different protocol (`426`), or it
/// answered the upgrade with something that is not a handshake response.
/// The transport classifies all of these as transport failures.
///
/// None of them is held against the credential. WebSockets take their own
/// route to the upstream — a hand-made connection that, unlike HTTP calls,
/// ignores the operating system's proxy settings and speaks no HTTP/2 — so
/// a host that cannot be reached this way may serve HTTP requests perfectly
/// well, and those find out by themselves if it does not.
fn websocket_path_failure(error: &UpstreamError) -> bool {
    error.class == FailureClass::Transport
}

/// What a dropped session guard records.
const ABORTED: DropOutcome = DropOutcome {
    status: SWITCHING_PROTOCOLS,
    kind: "aborted",
    message: "the WebSocket session ended without being finished",
};

/// How a relayed WebSocket session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsEnd {
    /// Either side closed the connection in an orderly way.
    Closed,
    /// The upstream socket failed (reported to the scheduler as a failure
    /// of the credential's upstream).
    UpstreamFailed(String),
    /// The client's side failed; the credential is not at fault.
    ClientFailed(String),
}

/// What the server knows about a session when it ends, for
/// [`UpstreamWsSession::finish`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsOutcome {
    /// How the session ended.
    pub end: WsEnd,
    /// Token usage observed on the relayed frames, if the server tracked
    /// any.
    pub usage: Usage,
}

impl WsOutcome {
    /// An orderly close.
    pub fn closed() -> Self {
        WsOutcome {
            end: WsEnd::Closed,
            usage: Usage::default(),
        }
    }

    /// The upstream socket failed. `message` must not contain credentials.
    pub fn upstream_failed(message: impl Into<String>) -> Self {
        WsOutcome {
            end: WsEnd::UpstreamFailed(message.into()),
            usage: Usage::default(),
        }
    }

    /// The client's socket failed.
    pub fn client_failed(message: impl Into<String>) -> Self {
        WsOutcome {
            end: WsEnd::ClientFailed(message.into()),
            usage: Usage::default(),
        }
    }

    /// Adds the usage observed during the session.
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }
}

/// Settles a session exactly once: by [`UpstreamWsSession::finish`], or on
/// drop.
struct WsGuard {
    inner: Arc<Inner>,
    lease: Lease,
    /// Time the handshake took, reported as the attempt's latency.
    latency_ms: u64,
    /// For removing the credential from what the upstream said.
    target: Target,
    recorder: Option<Recorder>,
}

impl WsGuard {
    fn settle(&mut self, outcome: Option<WsOutcome>) {
        let Some(mut recorder) = self.recorder.take() else {
            return;
        };
        let success = Outcome::Success {
            latency_ms: self.latency_ms,
        };
        match outcome {
            // Dropped without `finish`: the handshake worked, so the
            // credential is fine; the recorder's own drop publishes the
            // session as aborted.
            None => self.inner.report(&self.lease, success),
            Some(outcome) => {
                match outcome.end {
                    WsEnd::Closed => self.inner.report(&self.lease, success),
                    WsEnd::ClientFailed(message) => {
                        self.inner.report(&self.lease, success);
                        recorder.builder().set_error(RecordError::new(
                            "client_disconnect",
                            self.target.redact(&message),
                        ));
                    }
                    WsEnd::UpstreamFailed(message) => {
                        // A close reason is the upstream's own text.
                        let failure = UpstreamError::transport(self.target.redact(&message));
                        self.inner.report(&self.lease, Outcome::Failure(&failure));
                        recorder
                            .builder()
                            .set_error(RecordError::new("upstream", &failure.info.message));
                    }
                }
                recorder.set_usage(outcome.usage);
                recorder.finish(SWITCHING_PROTOCOLS);
            }
        }
    }
}

impl Drop for WsGuard {
    fn drop(&mut self) {
        self.settle(None);
    }
}

/// An open upstream WebSocket and the bookkeeping that goes with it.
///
/// Relay frames through [`socket`](Self::socket) — a `Stream` and a `Sink`
/// of `switchyard_upstream::WsMessage`, used through a mutable borrow
/// (`session.socket.next()`, `session.socket.send(..)`) — and call
/// [`finish`](Self::finish) when the session ends. Dropping the session
/// without `finish` records it as aborted.
pub struct UpstreamWsSession {
    /// The upstream socket.
    pub socket: UpstreamWebSocket,
    /// Model id the upstream was addressed with.
    pub upstream_model: String,
    /// Name of the provider serving the session.
    pub provider: String,
    /// Id of the session's request record.
    pub request_id: String,
    /// Headers of the upstream's `101` response — in particular
    /// `sec-websocket-protocol`, which a relay has to echo to its client.
    pub handshake_headers: HeaderMap,
    guard: WsGuard,
}

impl std::fmt::Debug for UpstreamWsSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamWsSession")
            .field("provider", &self.provider)
            .field("upstream_model", &self.upstream_model)
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}

impl UpstreamWsSession {
    /// Ends the session: publishes its request record (status `101`,
    /// duration, usage, the error if it failed) and reports the outcome to
    /// the scheduler. The socket is dropped, which closes it if the relay
    /// has not already done so.
    pub fn finish(mut self, outcome: WsOutcome) {
        self.guard.settle(Some(outcome));
    }

    /// `text` with the credential this session's upstream was called with
    /// replaced by `[redacted]`.
    ///
    /// Use it on anything the upstream *said about a failure* before it is
    /// shown to the client or logged: a close reason, the message of an
    /// error frame. Careless upstreams quote the key they were given.
    /// (Messages passed to [`finish`](Self::finish) are scrubbed by the
    /// gateway itself.) Do not run ordinary content through it: self-hosted
    /// servers use keys such as `ollama`, a word a model may well write.
    pub fn redact(&self, text: &str) -> String {
        self.guard.target.redact(text)
    }
}

impl Inner {
    /// See [`crate::Gateway::open_upstream_ws`].
    pub(crate) async fn open_upstream_ws(
        self: &Arc<Self>,
        request: WsOpenRequest,
    ) -> Result<UpstreamWsSession, ApiError> {
        let config = self.store.current();
        let mut recorder = self.begin_record(
            &config,
            WS_PROTOCOL,
            &request.endpoint,
            Transport::Websocket,
            &request.identity,
            request.client_ip.clone(),
            &request.headers,
            None,
        );
        recorder
            .builder()
            .set_requested_model(request.model.clone())
            .set_mode(Mode::Raw);
        recorder.announce();

        let refuse = |mut recorder: Recorder, error: ApiError| {
            recorder.fail_with(&error, None);
            recorder.finish(error.status);
            Err(error)
        };

        let resolution = self.scheduler.resolve(&request.model);
        if let Err(error) = check_identity(
            &self.keys.load(),
            &request.identity,
            &request.model,
            resolution.as_ref().ok(),
            None,
        ) {
            return refuse(recorder, error);
        }
        let mut resolved = match resolution {
            Ok(resolved) => resolved,
            Err(error) => return refuse(recorder, ApiError::from(error)),
        };
        if let Some(kind) = request.require_kind {
            resolved.retain_routes(|route| route.kind == kind);
            if !resolved.has_routes() {
                let error = no_route(&resolved.base, &format!("a provider of kind `{kind}`"));
                return refuse(recorder, error);
            }
        }
        recorder.builder().set_client_model(resolved.base.clone());

        // The upgrade request cannot be cancelled half-way by the server.
        let cancel = CancellationToken::new();
        let mut failover = Failover::new(
            self,
            &resolved,
            None,
            WS_PROTOCOL,
            &cancel,
            Limits::from_config(&config),
        );
        let connect_limit = Duration::from_secs(config.upstream.connect_timeout_secs.max(1));
        let offered = handshake_headers(&request.headers);
        loop {
            let lease = match failover.next().await {
                Next::Lease(lease) => lease,
                Next::Stop | Next::Cancelled => break,
            };
            let started = Instant::now();
            let protocol = protocol_for(
                lease.credential.kind,
                lease.upstream_protocol,
                &lease.upstream_model,
            );
            recorder.begin_attempt(&lease, protocol);
            let connected = match self
                .target_for(
                    &config,
                    &lease.provider_config,
                    &lease.credential,
                    protocol,
                    &lease.upstream_model,
                )
                .await
            {
                Ok(target) => {
                    let model = utf8_percent_encode(&lease.upstream_model, QUERY_SAFE).to_string();
                    let path = request.path_and_query.replace("{model}", &model);
                    self.upstream
                        .connect_ws_with(&target, &path, &offered, connect_limit)
                        .await
                        .map(|connection| (connection, target))
                }
                Err(error) => Err(error),
            };
            match connected {
                Ok((connection, target)) => {
                    recorder
                        .builder()
                        .push_attempt(
                            attempt_record(&lease, protocol, started)
                                .with_status(SWITCHING_PROTOCOLS),
                        )
                        .clear_error()
                        .mark_first_byte(now_unix_ms());
                    recorder.on_drop(ABORTED);
                    return Ok(UpstreamWsSession {
                        socket: connection.stream,
                        upstream_model: lease.upstream_model.clone(),
                        provider: lease.credential.provider.clone(),
                        request_id: recorder.id().to_string(),
                        handshake_headers: connection.headers,
                        guard: WsGuard {
                            inner: Arc::clone(self),
                            latency_ms: elapsed_ms(started),
                            lease: *lease,
                            target,
                            recorder: Some(recorder),
                        },
                    });
                }
                Err(error) => {
                    let failure = Failure::upstream(error, protocol);
                    recorder
                        .builder()
                        .push_attempt(failed_attempt_record(&lease, &failure, started));
                    if websocket_path_failure(&failure.error) {
                        failover.passed_over(&lease, failure);
                    } else {
                        failover.failed_optional(&lease, failure);
                    }
                }
            }
        }

        let error = failover.into_error();
        recorder.fail_finally(&error);
        recorder.finish(error.api.status);
        Err(error.api)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_are_escaped_for_the_query() {
        let encode = |model: &str| utf8_percent_encode(model, QUERY_SAFE).to_string();
        assert_eq!(encode("gpt-realtime"), "gpt-realtime");
        assert_eq!(encode("gpt-4o_mini.2025~x"), "gpt-4o_mini.2025~x");
        assert_eq!(encode("org/model&x=1"), "org%2Fmodel%26x%3D1");
    }

    #[test]
    fn only_allow_listed_headers_reach_the_handshake() {
        let mut client = HeaderMap::new();
        for (name, value) in [
            ("openai-beta", "realtime=v1"),
            ("openai-organization", "org-client"),
            ("openai-project", "proj_client"),
            ("sec-websocket-protocol", "realtime"),
            ("x-client-request-id", "abc"),
            ("authorization", "Bearer client-key"),
            ("cookie", "a=b"),
            ("origin", "https://app.example"),
            ("x-forwarded-for", "203.0.113.9"),
            ("user-agent", "client/1.0"),
            ("sec-websocket-key", "x"),
        ] {
            client.insert(name, http::HeaderValue::from_static(value));
        }
        client.append(
            "sec-websocket-protocol",
            http::HeaderValue::from_static("openai-beta.realtime-v1"),
        );
        let offered = handshake_headers(&client);
        let mut names: Vec<&str> = offered.keys().map(|name| name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "openai-beta",
                "sec-websocket-protocol",
                "x-client-request-id"
            ]
        );
        assert_eq!(offered.get_all("sec-websocket-protocol").iter().count(), 2);
    }

    #[test]
    fn failures_of_the_websocket_path_are_told_from_answers_of_the_api() {
        let mut upgrade_required = UpstreamError::transport("upgrade required");
        upgrade_required.status = 426;
        assert!(websocket_path_failure(&upgrade_required));
        for message in [
            "request: 127.0.0.1:9 answered HTTP 200 instead of upgrading to a WebSocket",
            "connect: connection refused",
            "tls: handshake failed",
            "read: the WebSocket handshake with example.test was cut short",
            "timeout: connecting to example.test took longer than 10 s",
        ] {
            assert!(
                websocket_path_failure(&UpstreamError::transport(message)),
                "{message}"
            );
        }
        for (status, class) in [
            (429, FailureClass::RateLimit),
            (500, FailureClass::Server),
            (401, FailureClass::Auth),
            (404, FailureClass::ModelNotFound),
        ] {
            let answered = crate::target::local_failure(class, status, "x");
            assert!(!websocket_path_failure(&answered), "{status}");
        }
    }

    #[test]
    fn outcomes() {
        assert_eq!(WsOutcome::closed().end, WsEnd::Closed);
        let usage = Usage {
            input_tokens: 3,
            ..Usage::default()
        };
        let failed = WsOutcome::upstream_failed("reset").with_usage(usage);
        assert_eq!(failed.end, WsEnd::UpstreamFailed("reset".into()));
        assert_eq!(failed.usage, usage);
        assert_eq!(
            WsOutcome::client_failed("gone").end,
            WsEnd::ClientFailed("gone".into())
        );
    }
}
