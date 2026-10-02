//! Turning gateway replies into HTTP responses: complete bodies, SSE
//! streams with keep-alive comments, and Gemini's JSON-array streaming.

use axum::body::Body;
use axum::response::Response;
use bytes::{BufMut, Bytes, BytesMut};
use http::header::{ALLOW, CACHE_CONTROL, CONTENT_TYPE};
use http::{HeaderName, HeaderValue, StatusCode};
use serde_json::Value;
use std::convert::Infallible;
use std::time::Duration;
use switchyard_codecs::chat::chat_chunk_to_completions;
use switchyard_core::{ApiError, Protocol, SseEvent, sse};
use switchyard_gateway::{FullReply, Gateway, StreamReply};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::DropGuard;

const JSON: HeaderValue = HeaderValue::from_static("application/json");

/// Adds `(name, value)` pairs produced by the gateway to a response,
/// skipping any that are not valid header text rather than failing the
/// response over a debugging header.
fn add_headers(response: &mut Response, headers: &[(String, String)]) {
    for (name, value) in headers {
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        response.headers_mut().append(name, value);
    }
}

/// A complete reply, as the gateway produced it: status, headers,
/// content type and body verbatim.
pub(crate) fn full(reply: FullReply) -> Response {
    let mut response = Response::new(Body::from(reply.body));
    *response.status_mut() =
        StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(content_type) = HeaderValue::from_str(&reply.content_type) {
        response.headers_mut().insert(CONTENT_TYPE, content_type);
    }
    add_headers(&mut response, &reply.headers);
    response
}

/// An error the server detected itself, in `protocol`'s envelope.
pub(crate) fn error(gateway: &Gateway, protocol: Protocol, error: &ApiError) -> Response {
    full(gateway.error_reply(protocol, error))
}

/// `405 Method Not Allowed` with the `Allow` header.
pub(crate) fn method_not_allowed(
    gateway: &Gateway,
    protocol: Protocol,
    method: &http::Method,
    allow: &'static str,
) -> Response {
    let api = ApiError::invalid_request(format!(
        "method {method} is not allowed on this path; use {allow}"
    ))
    .with_status(405)
    .with_code("method_not_allowed");
    let mut response = error(gateway, protocol, &api);
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static(allow));
    response
}

/// A JSON document with status 200.
pub(crate) fn json(value: &Value) -> Response {
    let mut response = Response::new(Body::from(value.to_string()));
    response.headers_mut().insert(CONTENT_TYPE, JSON);
    response
}

/// What is done to each event of a stream before it is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventMap {
    /// Written as the gateway produced it.
    Verbatim,
    /// Chat Completions chunks rewritten as legacy `text_completion`
    /// chunks; chunks with nothing for such a client are dropped.
    LegacyCompletions,
}

impl EventMap {
    fn apply(self, event: SseEvent) -> Option<SseEvent> {
        match self {
            EventMap::Verbatim => Some(event),
            EventMap::LegacyCompletions => {
                if event.is_done_marker() {
                    return Some(event);
                }
                match serde_json::from_str::<Value>(&event.data) {
                    Ok(chunk) => chat_chunk_to_completions(&chunk).map(|rewritten| SseEvent {
                        event: event.event,
                        data: rewritten.to_string(),
                    }),
                    // Not a chunk at all: not this shim's business.
                    Err(_) => Some(event),
                }
            }
        }
    }
}

/// How a stream is framed on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Framing {
    /// Server-sent events, with a `: keep-alive` comment after this much
    /// silence (`None`: never).
    Sse { keepalive: Option<Duration> },
    /// One JSON array whose elements are the events' data (Gemini's
    /// `streamGenerateContent` without `alt=sse`).
    JsonArray,
}

/// The state of a streamed response body.
struct StreamBody {
    events: mpsc::Receiver<SseEvent>,
    framing: Framing,
    map: EventMap,
    /// When the next keep-alive comment is due; pushed back by everything
    /// that is written.
    quiet_until: Option<Instant>,
    /// Elements written so far (JSON array framing).
    written: usize,
    /// Cancels the request if the body is dropped before the stream ends —
    /// which is what hyper does when the client goes away.
    cancel: Option<DropGuard>,
    finished: bool,
}

impl StreamBody {
    fn rearm(&mut self) {
        if let Framing::Sse {
            keepalive: Some(interval),
        } = self.framing
        {
            self.quiet_until = Some(Instant::now() + interval);
        }
    }

    /// The stream ended by itself: nothing is left to cancel.
    fn complete(&mut self) {
        self.finished = true;
        if let Some(guard) = self.cancel.take() {
            drop(guard.disarm());
        }
    }

    /// The next chunk of the body, or `None` when it is complete.
    async fn next_chunk(&mut self) -> Option<Bytes> {
        if self.finished {
            return None;
        }
        loop {
            let received = match self.quiet_until {
                Some(deadline) => {
                    match tokio::time::timeout_at(deadline, self.events.recv()).await {
                        Ok(received) => received,
                        Err(_) => {
                            self.rearm();
                            return Some(sse::comment("keep-alive"));
                        }
                    }
                }
                None => self.events.recv().await,
            };
            let Some(event) = received else {
                self.complete();
                return match self.framing {
                    Framing::Sse { .. } => None,
                    Framing::JsonArray => Some(Bytes::from_static(if self.written == 0 {
                        b"[]"
                    } else {
                        b"]"
                    })),
                };
            };
            let Some(event) = self.map.apply(event) else {
                continue;
            };
            match self.framing {
                Framing::Sse { .. } => {
                    self.rearm();
                    return Some(event.to_bytes());
                }
                Framing::JsonArray => {
                    let data = event.data.trim();
                    // `[DONE]` and empty events have no place in an array.
                    if data.is_empty() || event.is_done_marker() {
                        continue;
                    }
                    let mut chunk = BytesMut::with_capacity(data.len() + 1);
                    chunk.put_u8(if self.written == 0 { b'[' } else { b',' });
                    chunk.put_slice(data.as_bytes());
                    self.written += 1;
                    return Some(chunk.freeze());
                }
            }
        }
    }
}

/// A streamed reply. `cancel` is the guard of the request's cancellation
/// token: it fires if the client disconnects before the stream is over and
/// is disarmed when the stream ends by itself.
pub(crate) fn stream(
    reply: StreamReply,
    framing: Framing,
    map: EventMap,
    cancel: DropGuard,
) -> Response {
    let mut state = StreamBody {
        events: reply.events,
        framing,
        map,
        quiet_until: None,
        written: 0,
        cancel: Some(cancel),
        finished: false,
    };
    state.rearm();
    let chunks = futures::stream::unfold(state, |mut state| async move {
        let chunk = state.next_chunk().await?;
        Some((Ok::<Bytes, Infallible>(chunk), state))
    });
    let mut response = Response::new(Body::from_stream(chunks));
    let headers = response.headers_mut();
    match framing {
        Framing::Sse { .. } => {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
            headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            // Tells nginx-style proxies not to buffer the stream.
            headers.insert(
                HeaderName::from_static("x-accel-buffering"),
                HeaderValue::from_static("no"),
            );
        }
        Framing::JsonArray => {
            headers.insert(CONTENT_TYPE, JSON);
            headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            headers.insert(
                HeaderName::from_static("x-accel-buffering"),
                HeaderValue::from_static("no"),
            );
        }
    }
    add_headers(&mut response, &reply.headers);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn reply(events: mpsc::Receiver<SseEvent>) -> StreamReply {
        StreamReply {
            headers: vec![
                ("x-request-id".into(), "req-1".into()),
                ("x-switchyard-provider".into(), "mock".into()),
                ("bad header name".into(), "dropped".into()),
            ],
            protocol: Protocol::OpenaiChat,
            request_id: "req-1".into(),
            events,
        }
    }

    async fn chunks(response: Response) -> Vec<String> {
        let mut stream = response.into_body().into_data_stream();
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.push(String::from_utf8(chunk.unwrap().to_vec()).unwrap());
        }
        out
    }

    #[tokio::test]
    async fn sse_writes_one_chunk_per_event_and_adds_nothing() {
        let (tx, rx) = mpsc::channel(8);
        let token = CancellationToken::new();
        let response = stream(
            reply(rx),
            Framing::Sse { keepalive: None },
            EventMap::Verbatim,
            token.clone().drop_guard(),
        );
        assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
        assert_eq!(response.headers()[CACHE_CONTROL], "no-cache");
        assert_eq!(response.headers()["x-accel-buffering"], "no");
        assert_eq!(response.headers()["x-request-id"], "req-1");
        assert_eq!(response.headers()["x-switchyard-provider"], "mock");
        tx.send(SseEvent::named("message_start", "{\"a\":1}"))
            .await
            .unwrap();
        tx.send(SseEvent::data("[DONE]")).await.unwrap();
        drop(tx);
        assert_eq!(
            chunks(response).await,
            vec![
                "event: message_start\ndata: {\"a\":1}\n\n".to_string(),
                "data: [DONE]\n\n".to_string()
            ]
        );
        // A stream that ended by itself cancels nothing.
        assert!(!token.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn keep_alive_comments_fill_silence_and_events_reset_the_timer() {
        let (tx, rx) = mpsc::channel(8);
        let response = stream(
            reply(rx),
            Framing::Sse {
                keepalive: Some(Duration::from_secs(10)),
            },
            EventMap::Verbatim,
            CancellationToken::new().drop_guard(),
        );
        let feeder = tokio::spawn(async move {
            // An event every 6 s: never 10 s of silence.
            for n in 0..3 {
                tokio::time::sleep(Duration::from_secs(6)).await;
                tx.send(SseEvent::data(format!("{n}"))).await.unwrap();
            }
            // Then 25 s of nothing: two comments.
            tokio::time::sleep(Duration::from_secs(25)).await;
            tx.send(SseEvent::data("last")).await.unwrap();
        });
        let written = chunks(response).await;
        feeder.await.unwrap();
        assert_eq!(
            written,
            vec![
                "data: 0\n\n",
                "data: 1\n\n",
                "data: 2\n\n",
                ": keep-alive\n\n",
                ": keep-alive\n\n",
                "data: last\n\n"
            ]
        );
    }

    #[tokio::test]
    async fn dropping_the_body_cancels_the_request() {
        let (tx, rx) = mpsc::channel(8);
        let token = CancellationToken::new();
        let response = stream(
            reply(rx),
            Framing::Sse { keepalive: None },
            EventMap::Verbatim,
            token.clone().drop_guard(),
        );
        tx.send(SseEvent::data("one")).await.unwrap();
        let mut body = response.into_body().into_data_stream();
        assert!(body.next().await.is_some());
        assert!(!token.is_cancelled());
        drop(body);
        assert!(token.is_cancelled());
        // The gateway sees its channel closed, too.
        assert!(tx.is_closed());
    }

    #[tokio::test]
    async fn json_array_framing() {
        let (tx, rx) = mpsc::channel(8);
        let response = stream(
            reply(rx),
            Framing::JsonArray,
            EventMap::Verbatim,
            CancellationToken::new().drop_guard(),
        );
        assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
        tx.send(SseEvent::data("{\"n\":1}")).await.unwrap();
        tx.send(SseEvent::data("")).await.unwrap();
        tx.send(SseEvent::named("error", "{\"error\":{\"code\":500}}"))
            .await
            .unwrap();
        drop(tx);
        let written = chunks(response).await;
        assert_eq!(
            written,
            vec!["[{\"n\":1}", ",{\"error\":{\"code\":500}}", "]"]
        );
        let parsed: Value = serde_json::from_str(&written.concat()).unwrap();
        assert_eq!(parsed, json!([{"n": 1}, {"error": {"code": 500}}]));

        // Nothing at all is still a JSON array.
        let (tx, rx) = mpsc::channel::<SseEvent>(1);
        drop(tx);
        let response = stream(
            reply(rx),
            Framing::JsonArray,
            EventMap::Verbatim,
            CancellationToken::new().drop_guard(),
        );
        assert_eq!(chunks(response).await, vec!["[]"]);
    }

    #[tokio::test]
    async fn legacy_completions_chunks_are_rewritten_and_empty_ones_dropped() {
        let (tx, rx) = mpsc::channel(8);
        let response = stream(
            reply(rx),
            Framing::Sse { keepalive: None },
            EventMap::LegacyCompletions,
            CancellationToken::new().drop_guard(),
        );
        let chunk = |delta: Value, finish: Value| {
            json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "m",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
            .to_string()
        };
        for data in [
            chunk(json!({"role": "assistant"}), Value::Null),
            chunk(json!({"content": "Hel"}), Value::Null),
            chunk(json!({}), json!("stop")),
            json!({"error": {"message": "boom", "type": "server_error"}}).to_string(),
            "[DONE]".to_string(),
        ] {
            tx.send(SseEvent::data(data)).await.unwrap();
        }
        drop(tx);
        let written = chunks(response).await;
        assert_eq!(written.len(), 4, "{written:?}");
        let first: Value =
            serde_json::from_str(written[0].trim().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(first["object"], "text_completion");
        assert_eq!(first["choices"][0]["text"], "Hel");
        let second: Value =
            serde_json::from_str(written[1].trim().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(second["choices"][0]["finish_reason"], "stop");
        assert!(written[2].contains("\"boom\""));
        assert_eq!(written[3], "data: [DONE]\n\n");
    }

    #[test]
    fn full_replies_are_copied_verbatim() {
        let response = full(FullReply {
            status: 429,
            headers: vec![
                ("x-request-id".into(), "req-9".into()),
                ("retry-after".into(), "7".into()),
                ("x-bad\nname".into(), "x".into()),
                ("x-bad-value".into(), "a\nb".into()),
            ],
            content_type: "application/json".into(),
            body: Bytes::from_static(b"{\"error\":{}}"),
            request_id: "req-9".into(),
        });
        assert_eq!(response.status(), 429);
        assert_eq!(response.headers()["retry-after"], "7");
        assert_eq!(response.headers()["x-request-id"], "req-9");
        assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
        assert!(response.headers().get("x-bad-value").is_none());
    }
}
