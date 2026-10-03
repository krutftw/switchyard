//! The Realtime relay — `GET /v1/realtime?model=…` (`docs/DESIGN.md` §10).
//!
//! The gateway authenticates the client, opens the upstream's Realtime
//! WebSocket with one of its own credentials, and then passes frames both
//! ways until either side closes. Access is revalidated while open, and
//! upstream error messages are scrubbed of the upstream credential.

use super::{ClientSocket, GOING_AWAY, INTERNAL_ERROR};
use crate::app::Context;
use crate::body;
use axum::response::Response;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use http::header::SEC_WEBSOCKET_PROTOCOL;
use http::request::Parts;
use http::{HeaderMap, HeaderValue};
use serde_json::Value;
use std::collections::VecDeque;
use std::time::Duration;
use switchyard_core::config::ProviderKind;
use switchyard_core::{ApiError, Usage};
use switchyard_gateway::{UpstreamWsSession, WsMessage, WsOpenRequest, WsOutcome};
use tokio_tungstenite::tungstenite::Error as WsError;

/// The subprotocol browser clients use to present their key, since a
/// browser cannot set headers on a WebSocket.
const KEY_SUBPROTOCOL: &str = "openai-insecure-api-key.";

/// Subprotocols that carry the client's own vendor account. Like the
/// headers of the same names, they are not passed on next to the gateway's
/// credential.
const ACCOUNT_SUBPROTOCOLS: [&str; 2] = ["openai-organization.", "openai-project."];

/// The subprotocol that names the API itself.
const REALTIME: &str = "realtime";

/// How long a frame may wait for the upstream to take it.
const UPSTREAM_SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// The subprotocols a client offered, in order.
fn offered(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// The key a client presented as an `openai-insecure-api-key.<key>`
/// subprotocol.
pub(crate) fn subprotocol_key(headers: &HeaderMap) -> Option<String> {
    offered(headers).into_iter().find_map(|item| {
        item.strip_prefix(KEY_SUBPROTOCOL)
            .filter(|key| !key.is_empty())
            .map(str::to_string)
    })
}

/// Whether a subprotocol may be shown to the upstream or echoed to the
/// client: everything but the ones that carry a credential or an account.
fn is_plain(subprotocol: &str) -> bool {
    !subprotocol.starts_with(KEY_SUBPROTOCOL)
        && !ACCOUNT_SUBPROTOCOLS
            .iter()
            .any(|prefix| subprotocol.starts_with(prefix))
}

/// The client's headers as they are offered to the upstream handshake: the
/// same, except that the subprotocol list no longer contains the client's
/// gateway key or its account.
fn upstream_headers(client: &HeaderMap) -> HeaderMap {
    let mut headers = client.clone();
    let plain: Vec<String> = offered(client)
        .into_iter()
        .filter(|item| is_plain(item))
        .collect();
    headers.remove(SEC_WEBSOCKET_PROTOCOL);
    if !plain.is_empty()
        && let Ok(value) = HeaderValue::from_str(&plain.join(", "))
    {
        headers.insert(SEC_WEBSOCKET_PROTOCOL, value);
    }
    headers
}

/// The subprotocol to echo on the `101`: the one the upstream selected when
/// the client offered it, else `realtime` when the client offered that
/// (a browser aborts a handshake that selects none of its offers). Never
/// one that carries a key.
fn select_subprotocol(client: &HeaderMap, upstream: &HeaderMap) -> Option<HeaderValue> {
    let offered: Vec<String> = offered(client)
        .into_iter()
        .filter(|item| is_plain(item))
        .collect();
    let selected = upstream
        .get(SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|selected| offered.iter().any(|item| item == selected))
        .map(str::to_string)
        .or_else(|| {
            offered
                .iter()
                .find(|item| item.as_str() == REALTIME)
                .cloned()
        })?;
    HeaderValue::from_str(&selected).ok()
}

/// The path and query the upstream socket is opened with: the model
/// placeholder the gateway fills in, then the client's other parameters as
/// it sent them — without its key.
fn upstream_path(context: &Context) -> String {
    let mut path = String::from("realtime?model={model}");
    if let Some(rest) = context.query.without(&["model"]) {
        path.push('&');
        // Braces would be read as a placeholder.
        path.push_str(&rest.replace('{', "%7B").replace('}', "%7D"));
    }
    path
}

/// `GET /v1/realtime`: opens the upstream socket first — so a failure is
/// still an ordinary HTTP error — then accepts the client's upgrade and
/// relays.
pub(crate) async fn upgrade(context: Context, parts: &mut Parts) -> Response {
    let handshake = match super::handshake(context.gateway(), parts) {
        Ok(handshake) => handshake,
        Err(response) => return *response,
    };
    let Some(model) = context
        .query
        .get("model")
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
    else {
        return context.error(
            &ApiError::invalid_request("the `model` query parameter is required")
                .with_param("model"),
        );
    };

    let request = WsOpenRequest {
        identity: context.identity.clone(),
        model,
        path_and_query: upstream_path(&context),
        headers: upstream_headers(&context.headers),
        endpoint: "GET /v1/realtime".to_string(),
        client_ip: context.client_ip.clone(),
        require_kind: Some(ProviderKind::Openai),
    };
    let session = match context.gateway().open_upstream_ws(request).await {
        Ok(session) => session,
        Err(error) => return context.error(&error),
    };

    let subprotocol = select_subprotocol(&context.headers, &session.handshake_headers);
    let max_message = body::limit_bytes(context.gateway().config().server.body_limit_mb);
    // The session has one request record; the `101` names it.
    let request_id = HeaderValue::from_str(&session.request_id).ok();
    let mut response = handshake.accept(subprotocol, max_message, move |socket| {
        relay(context, socket, session)
    });
    if let Some(request_id) = request_id {
        response.headers_mut().insert("x-request-id", request_id);
    }
    response
}

/// Whether an upstream close code says the upstream failed, as opposed to
/// ending the session or objecting to what the client sent.
fn is_upstream_fault(code: u16) -> bool {
    matches!(code, 1006 | 1011 | 1012 | 1013 | 1014 | 1015)
}

/// Adds the usage of a `response.done` event to the session's total.
fn count_usage(frame: &str, total: &mut Usage) {
    // Audio deltas are most of the traffic; do not parse them.
    if !frame.contains("\"response.done\"") {
        return;
    }
    let Ok(event) = serde_json::from_str::<Value>(frame) else {
        return;
    };
    if event.get("type").and_then(Value::as_str) != Some("response.done") {
        return;
    }
    let Some(usage) = event.pointer("/response/usage") else {
        return;
    };
    let number = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
    let input = number("/input_tokens");
    let cached = number("/input_token_details/cached_tokens").min(input);
    total.input_tokens = total.input_tokens.saturating_add(input - cached);
    total.cache_read_tokens = total.cache_read_tokens.saturating_add(cached);
    total.output_tokens = total.output_tokens.saturating_add(number("/output_tokens"));
}

/// Pings passed on to one side of the relay and not yet answered by it.
///
/// The WebSocket layer answers every ping it reads by itself, so the side
/// that sent a ping already has its pong. The ping is still passed on (the
/// far side should see it), but the far side's answer must not be: the
/// pinger would get two pongs for one ping. Pongs nobody asked for — some
/// peers send them as heartbeats — are relayed like any other frame.
#[derive(Debug, Default)]
struct PingLedger {
    awaiting: VecDeque<Bytes>,
}

impl PingLedger {
    /// Pings remembered per side; older ones are forgotten.
    const CAPACITY: usize = 32;

    /// Notes a ping that was passed on.
    fn passed_on(&mut self, payload: &Bytes) {
        if self.awaiting.len() >= Self::CAPACITY {
            self.awaiting.pop_front();
        }
        self.awaiting.push_back(payload.clone());
    }

    /// Whether a pong answers a ping that was passed on (which, with every
    /// ping before it, is then settled).
    fn answers(&mut self, payload: &Bytes) -> bool {
        match self.awaiting.iter().position(|ping| ping == payload) {
            Some(position) => {
                self.awaiting.drain(..=position);
                true
            }
            None => false,
        }
    }
}

/// Passes frames between the client and the upstream until one side ends
/// the session, then tells the gateway how it ended.
async fn relay(context: Context, mut client: ClientSocket, mut session: UpstreamWsSession) {
    let _connected = context.gateway().telemetry().track_ws();
    let shutdown = context
        .lifecycle
        .as_ref()
        .map(|lifecycle| lifecycle.shutdown.clone())
        .unwrap_or_default();
    let mut usage = Usage::default();
    // Pings passed on to the upstream, and to the client.
    let mut to_upstream = PingLedger::default();
    let mut to_client = PingLedger::default();
    let mut access_check = tokio::time::interval(Duration::from_secs(1));
    access_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let outcome = loop {
        if context
            .gateway()
            .validate_session_identity(&context.identity)
            .is_err()
        {
            let goodbye = super::close_frame(1008, "client access changed; reconnect required");
            super::send_close(&mut session.socket, Some(goodbye)).await;
            super::close(
                &mut client,
                1008,
                "client access changed; reconnect required",
            )
            .await;
            break WsOutcome::closed();
        }
        tokio::select! {
            _ = access_check.tick() => {}
            _ = shutdown.cancelled() => {
                // A realtime session has no natural end to wait for.
                let goodbye = super::close_frame(GOING_AWAY, "the gateway is shutting down");
                super::send_close(&mut session.socket, Some(goodbye)).await;
                super::close(&mut client, GOING_AWAY, "the server is shutting down").await;
                break WsOutcome::closed();
            }
            from_client = client.next() => match from_client {
                Some(Ok(WsMessage::Close(frame))) => {
                    // Same code and reason, as far as they can be sent.
                    let frame = frame.map(|frame| {
                        super::close_frame(super::sendable_code(frame.code.into()), &frame.reason)
                    });
                    super::send_close(&mut session.socket, frame).await;
                    tokio::join!(
                        super::await_close(&mut session.socket),
                        super::await_close(&mut client),
                    );
                    break WsOutcome::closed();
                }
                Some(Ok(WsMessage::Frame(_))) => {}
                Some(Ok(WsMessage::Pong(payload))) if to_client.answers(&payload) => {}
                Some(Ok(message)) => {
                    if let WsMessage::Ping(payload) = &message {
                        to_upstream.passed_on(payload);
                    }
                    // An upstream that takes nothing for this long is gone.
                    let sent = tokio::time::timeout(UPSTREAM_SEND_TIMEOUT, session.socket.send(message)).await;
                    let failure = match sent {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(session.redact(&error.to_string())),
                        Err(_) => Some("the upstream stopped accepting data".to_string()),
                    };
                    if let Some(failure) = failure {
                        super::close(&mut client, INTERNAL_ERROR, "the upstream connection was lost").await;
                        break WsOutcome::upstream_failed(failure);
                    }
                }
                Some(Err(WsError::Capacity(_))) => {
                    let goodbye = super::close_frame(1000, "");
                    super::send_close(&mut session.socket, Some(goodbye)).await;
                    super::refuse_oversized(&mut client).await;
                    break WsOutcome::client_failed("the client sent a message over the size limit");
                }
                Some(Err(_)) | None => {
                    let goodbye = super::close_frame(1000, "");
                    super::send_close(&mut session.socket, Some(goodbye)).await;
                    super::await_close(&mut session.socket).await;
                    break WsOutcome::client_failed("the client disconnected without closing");
                }
            },
            from_upstream = session.socket.next() => match from_upstream {
                Some(Ok(WsMessage::Close(frame))) => {
                    let (code, reason) = match frame {
                        Some(frame) => (u16::from(frame.code), frame.reason.to_string()),
                        None => (1005, String::new()),
                    };
                    // A close reason is the upstream's own words.
                    let reason = session.redact(&reason);
                    tokio::join!(
                        super::close(&mut client, super::sendable_code(code), &reason),
                        super::await_close(&mut session.socket),
                    );
                    break if is_upstream_fault(code) {
                        WsOutcome::upstream_failed(format!(
                            "the upstream closed the session with code {code}: {reason}"
                        ))
                    } else {
                        WsOutcome::closed()
                    };
                }
                Some(Ok(WsMessage::Frame(_))) => {}
                Some(Ok(WsMessage::Pong(payload))) if to_upstream.answers(&payload) => {}
                Some(Ok(mut message)) => {
                    match &mut message {
                        WsMessage::Text(text) => {
                            count_usage(text.as_str(), &mut usage);
                            if let Some(redacted) = redact_error_event(text.as_str(), |value| session.redact(value)) {
                                *text = redacted.into();
                            }
                        }
                        WsMessage::Ping(payload) => to_client.passed_on(payload),
                        _ => {}
                    }
                    if client.send(message).await.is_err() {
                        let goodbye = super::close_frame(1000, "");
                        super::send_close(&mut session.socket, Some(goodbye)).await;
                        break WsOutcome::client_failed("the client disconnected without closing");
                    }
                }
                Some(Err(error)) => {
                    let message = session.redact(&error.to_string());
                    super::close(&mut client, INTERNAL_ERROR, "the upstream connection was lost").await;
                    break WsOutcome::upstream_failed(message);
                }
                None => {
                    super::close(&mut client, INTERNAL_ERROR, "the upstream connection was lost").await;
                    break WsOutcome::upstream_failed("the upstream connection ended without a close frame");
                }
            },
        }
    };
    session.finish(outcome.with_usage(usage));
}

/// Error payloads may quote a credential with JSON escapes, so redact
/// decoded strings and serialize them again without changing ordinary content.
fn redact_error_event(text: &str, redact: impl Fn(&str) -> String) -> Option<String> {
    let mut value: Value = serde_json::from_str(text).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    fn visit(value: &mut Value, redact: &impl Fn(&str) -> String) {
        match value {
            Value::String(text) => *text = redact(text),
            Value::Array(values) => values.iter_mut().for_each(|value| visit(value, redact)),
            Value::Object(values) => values.values_mut().for_each(|value| visit(value, redact)),
            _ => {}
        }
    }
    visit(&mut value, &redact);
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_frames_redact_decoded_strings_without_rewriting_content_frames() {
        let secret = "opaque\"test-value";
        let frame = serde_json::json!({"type":"error", "error":{"message":format!("invalid credential: {secret}")}}).to_string();
        let redacted =
            redact_error_event(&frame, |value| value.replace(secret, "[redacted]")).unwrap();
        let decoded: Value = serde_json::from_str(&redacted).unwrap();
        assert_eq!(
            decoded["error"]["message"],
            "invalid credential: [redacted]"
        );
        let escaped_type = r#"{"type":"\u0065rror","message":"opaque-value"}"#;
        let redacted = redact_error_event(escaped_type, |value| {
            value.replace("opaque-value", "[redacted]")
        })
        .unwrap();
        let decoded: Value = serde_json::from_str(&redacted).unwrap();
        assert_eq!(decoded["message"], "[redacted]");
        let content =
            serde_json::json!({"type":"response.output_text.delta", "delta":"error"}).to_string();
        assert!(
            redact_error_event(&content, |_| panic!("ordinary content is untouched")).is_none()
        );
    }
    use pretty_assertions::assert_eq;

    fn headers(protocols: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in protocols {
            headers.append(
                SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn the_key_subprotocol_is_found_and_never_passed_on() {
        let client = headers(&[
            "realtime, openai-insecure-api-key.sy-secret",
            "openai-organization.org_1, openai-project.proj_1, openai-beta.realtime-v1",
        ]);
        assert_eq!(subprotocol_key(&client).as_deref(), Some("sy-secret"));
        let upstream = upstream_headers(&client);
        assert_eq!(
            upstream[SEC_WEBSOCKET_PROTOCOL],
            "realtime, openai-beta.realtime-v1"
        );

        let only_key = headers(&["openai-insecure-api-key.sy-secret"]);
        assert!(
            upstream_headers(&only_key)
                .get(SEC_WEBSOCKET_PROTOCOL)
                .is_none()
        );
        assert_eq!(subprotocol_key(&headers(&["realtime"])), None);
        assert_eq!(
            subprotocol_key(&headers(&["openai-insecure-api-key."])),
            None
        );
        assert_eq!(subprotocol_key(&HeaderMap::new()), None);
    }

    #[test]
    fn the_echoed_subprotocol_is_one_the_client_offered() {
        let client =
            headers(&["realtime, openai-insecure-api-key.sy-secret, openai-beta.realtime-v1"]);
        // The upstream's choice, when the client offered it.
        assert_eq!(
            select_subprotocol(&client, &headers(&["openai-beta.realtime-v1"])).unwrap(),
            "openai-beta.realtime-v1"
        );
        // Otherwise `realtime`, which the client offered.
        assert_eq!(
            select_subprotocol(&client, &HeaderMap::new()).unwrap(),
            "realtime"
        );
        assert_eq!(
            select_subprotocol(&client, &headers(&["something-else"])).unwrap(),
            "realtime"
        );
        // Never the key, whatever an upstream echoes.
        let echoing = headers(&["openai-insecure-api-key.sy-secret"]);
        assert_eq!(select_subprotocol(&client, &echoing).unwrap(), "realtime");
        // Nothing offered, nothing selected.
        assert_eq!(
            select_subprotocol(&HeaderMap::new(), &headers(&["realtime"])),
            None
        );
        assert_eq!(
            select_subprotocol(&headers(&["openai-insecure-api-key.k"]), &HeaderMap::new()),
            None
        );
    }

    #[test]
    fn usage_is_summed_over_the_responses_of_a_session() {
        let mut usage = Usage::default();
        count_usage(
            r#"{"type":"response.output_audio.delta","delta":"AAAA"}"#,
            &mut usage,
        );
        assert!(usage.is_empty());
        let done = r#"{"type":"response.done","response":{"usage":{"total_tokens":150,"input_tokens":100,"output_tokens":50,"input_token_details":{"cached_tokens":30}}}}"#;
        count_usage(done, &mut usage);
        count_usage(done, &mut usage);
        assert_eq!(
            usage,
            Usage {
                input_tokens: 140,
                cache_read_tokens: 60,
                cache_write_tokens: 0,
                output_tokens: 100,
                reasoning_tokens: 0,
            }
        );
        // The words alone do not make an event.
        count_usage(r#"{"type":"x","note":"\"response.done\""}"#, &mut usage);
        count_usage("\"response.done\" not json", &mut usage);
        assert_eq!(usage.output_tokens, 100);
    }

    #[test]
    fn a_pong_that_answers_a_relayed_ping_is_not_relayed_back() {
        let mut ledger = PingLedger::default();
        let ping = |text: &'static str| Bytes::from_static(text.as_bytes());
        // A heartbeat pong nobody asked for is relayed.
        assert!(!ledger.answers(&ping("unsolicited")));

        ledger.passed_on(&ping("a"));
        ledger.passed_on(&ping("b"));
        ledger.passed_on(&ping("c"));
        // An answer settles its ping and the ones before it.
        assert!(ledger.answers(&ping("b")));
        assert!(!ledger.answers(&ping("a")));
        assert!(ledger.answers(&ping("c")));
        assert!(!ledger.answers(&ping("c")));

        // Pings that are never answered do not pile up.
        for _ in 0..1000 {
            ledger.passed_on(&ping("x"));
        }
        assert_eq!(ledger.awaiting.len(), PingLedger::CAPACITY);
    }

    #[test]
    fn close_codes_that_blame_the_upstream() {
        for code in [1006, 1011, 1012, 1013, 1014] {
            assert!(is_upstream_fault(code), "{code}");
        }
        for code in [1000, 1001, 1005, 1007, 1008, 1009, 4000] {
            assert!(!is_upstream_fault(code), "{code}");
        }
    }
}
