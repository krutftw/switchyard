//! Responses over WebSocket — `GET /v1/responses` (`docs/DESIGN.md` §10).
//!
//! Each client message is one JSON object (`response.create`, or the legacy
//! `response.append`); each server message is one Responses streaming event
//! as a text frame. Turns run one at a time through the gateway's normal
//! pipeline with client protocol `openai-responses`, so any model of any
//! provider can be used; the conversation is kept here
//! ([`super::transcript`]) and the full input is rebuilt for every turn.
//!
//! The socket is read all the time — also while a turn runs — so that
//! pings are answered, further requests queue up, and a client that closes
//! or vanishes cancels its turn. The gateway pings in turn, and gives up on
//! a client that leaves its pings unanswered: quickly during a turn, when
//! the client is reading and silence means it is gone; slowly between
//! turns, when many clients do not look at the socket at all. Anything a
//! client sends counts as an answer, down to the bytes of a request that is
//! still being uploaded.
//!
//! Turns always take the HTTP path to the upstream. Relaying to an
//! upstream's own Responses WebSocket (`websocket = true` on an `openai`
//! provider) is not implemented: such providers are served like any other.

use super::transcript::{self, Completed, Fault, Prepared, Transcript};
use super::{ClientSocket, GOING_AWAY, INTERNAL_ERROR};
use crate::app::Context;
use crate::body;
use crate::handlers;
use crate::lifecycle::Lifecycle;
use axum::response::Response;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use http::request::Parts;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::time::Duration;
use switchyard_codecs::responses::ws_error_frame;
use switchyard_core::util::now_unix;
use switchyard_core::{Protocol, SseEvent};
use switchyard_gateway::{ClientRequest, FullReply, Reply, Transport};
use tokio::sync::mpsc;
use tokio::time::{Interval, MissedTickBehavior};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_util::sync::{CancellationToken, DropGuard};

/// Requests that may wait behind the turn in progress.
const MAX_QUEUED: usize = 16;

/// Keep-alive pings in a row that may go unanswered during a turn before
/// the client is considered gone. "Answered" means that anything at all
/// arrived since: a pong, a message, or more bytes of a message still on its
/// way.
const MAX_UNANSWERED_PINGS: u32 = 2;

/// How long a client may leave pings unanswered while no turn is running.
///
/// During a turn a client reads the events it is sent, and whatever reads a
/// WebSocket answers its pings. Between turns some clients do not read at
/// all — they come back to the socket when they have something to send —
/// and holding that against them would cost them the connection (and the
/// conversation kept on it) after every pause. A connection that really is
/// dead still ends when this time is up, or sooner, when the operating
/// system gives up on delivering the pings.
const IDLE_SILENCE: Duration = Duration::from_secs(600);

/// How many keep-alive pings in a row a client may leave unanswered.
fn patience(keepalive: Duration, in_turn: bool) -> u32 {
    if in_turn {
        return MAX_UNANSWERED_PINGS;
    }
    let pings = IDLE_SILENCE
        .as_millis()
        .div_ceil(keepalive.as_millis().max(1));
    u32::try_from(pings)
        .unwrap_or(u32::MAX)
        .max(MAX_UNANSWERED_PINGS)
}

/// The label of a turn's request record.
const ENDPOINT: &str = "WS /v1/responses";

/// `GET /v1/responses`: accepts the upgrade (the request is already
/// authenticated) and starts the session.
pub(crate) fn upgrade(context: Context, parts: &mut Parts) -> Response {
    let handshake = match super::handshake(context.gateway(), parts) {
        Ok(handshake) => handshake,
        Err(response) => return *response,
    };
    // Read once, when the connection is made.
    let max_message = body::limit_bytes(context.gateway().config().server.body_limit_mb);
    let keepalive = handlers::keepalive(context.gateway());
    handshake.accept(None, max_message, move |socket| {
        Session {
            id: uuid::Uuid::new_v4().to_string(),
            context,
            keepalive,
            max_bytes: max_message,
        }
        .run(socket)
    })
}

/// One client connection.
struct Session {
    /// Identifies the connection to the gateway: session affinity keeps all
    /// its turns on one credential.
    id: String,
    context: Context,
    keepalive: Option<Duration>,
    /// The body limit: bounds a message, the requests waiting in the queue
    /// taken together, and the request rebuilt for a turn.
    max_bytes: usize,
}

/// A request waiting for its turn.
struct Queued {
    message: Map<String, Value>,
    bytes: usize,
}

/// The turn in progress.
struct Turn {
    /// The request as sent upstream; remembered, with the response, when
    /// the turn completes.
    request: Map<String, Value>,
    /// The size of that request as JSON.
    request_bytes: usize,
    /// The `stream_id` the client tagged the request with, echoed on every
    /// frame of the turn.
    lane: Option<String>,
    state: TurnState,
    tracker: Tracker,
    /// Cancels the gateway request when the turn is dropped unfinished.
    _cancel: DropGuard,
}

enum TurnState {
    /// Waiting for the pipeline's reply.
    Starting(BoxFuture<'static, Reply>),
    /// Forwarding the stream.
    Streaming(mpsc::Receiver<SseEvent>),
}

/// What a turn produced next.
enum Step {
    Reply(Reply),
    Event(Option<SseEvent>),
}

/// Waits for the turn's next step; forever when there is no turn.
async fn advance(turn: &mut Option<Turn>) -> Step {
    match turn.as_mut().map(|turn| &mut turn.state) {
        None => std::future::pending().await,
        Some(TurnState::Starting(reply)) => Step::Reply(reply.await),
        Some(TurnState::Streaming(events)) => Step::Event(events.recv().await),
    }
}

/// Waits for the next keep-alive tick; forever when keep-alives are off.
async fn tick(ping: &mut Option<Interval>) {
    match ping {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// How a turn ended, as far as its events tell.
#[derive(Clone, Debug, PartialEq)]
enum Outcome {
    /// A terminal success event was seen.
    Completed {
        output: Vec<Value>,
        response_id: Option<String>,
    },
    /// The stream carried an error.
    Failed,
}

/// Watches the events of one turn.
#[derive(Debug, Default)]
struct Tracker {
    /// Items of `response.output_item.done` events with their
    /// `output_index`.
    items: Vec<(Option<u64>, Value)>,
    outcome: Option<Outcome>,
}

impl Tracker {
    fn observe(&mut self, event: &SseEvent) {
        // Events are named after their type; one that is not (no name, or a
        // generic one such as `message`) is identified by its payload.
        // Deltas — nearly everything — are not parsed at all.
        let named = event
            .event
            .as_deref()
            .filter(|name| name.starts_with("response.") || *name == "error");
        if named.is_some_and(|name| !Tracker::is_interesting(name)) {
            return;
        }
        let Ok(payload) = serde_json::from_str::<Value>(&event.data) else {
            return;
        };
        let kind = named
            .or_else(|| payload.get("type").and_then(Value::as_str))
            .unwrap_or("");
        match kind {
            "response.created" => {
                // One stream may carry several responses; items belong to
                // the latest.
                self.items.clear();
            }
            "response.output_item.done" => {
                if let Some(item) = payload.get("item").filter(|item| item.is_object()) {
                    let index = payload.get("output_index").and_then(Value::as_u64);
                    self.items.push((index, item.clone()));
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                let response = payload.get("response");
                let output = response
                    .and_then(|response| response.get("output"))
                    .and_then(Value::as_array)
                    .filter(|output| !output.is_empty())
                    .cloned()
                    .unwrap_or_else(|| self.collected());
                let response_id = response
                    .and_then(|response| response.get("id"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                self.outcome = Some(Outcome::Completed {
                    output,
                    response_id,
                });
            }
            // An error after the terminal event changes nothing.
            "response.failed" | "error" if self.outcome.is_none() => {
                self.outcome = Some(Outcome::Failed);
            }
            _ => {}
        }
    }

    fn is_interesting(name: &str) -> bool {
        matches!(
            name,
            "response.created"
                | "response.output_item.done"
                | "response.completed"
                | "response.incomplete"
                | "response.done"
                | "response.failed"
                | "error"
        )
    }

    /// The items seen in `output_item.done` events: by index, then the ones
    /// without an index in arrival order.
    fn collected(&self) -> Vec<Value> {
        let mut items: Vec<&(Option<u64>, Value)> = self.items.iter().collect();
        items.sort_by_key(|(index, _)| (index.is_none(), *index));
        items.into_iter().map(|(_, item)| item.clone()).collect()
    }
}

/// A frame with the turn's `stream_id` as its first member, the way the
/// vendor tags the events of a named lane.
fn with_lane(frame: String, lane: Option<&str>) -> String {
    let Some(lane) = lane else {
        return frame;
    };
    let Some(rest) = frame.trim_start().strip_prefix('{') else {
        return frame;
    };
    let lane = Value::String(lane.to_string());
    if rest.trim_start().starts_with('}') {
        format!("{{\"stream_id\":{lane}{rest}")
    } else {
        format!("{{\"stream_id\":{lane},{rest}")
    }
}

/// The error frame for a turn the pipeline answered with an error: the
/// `error` object of the reply's body, under the status it came with.
fn error_frame(reply: &FullReply) -> Value {
    let status = if (400..=599).contains(&reply.status) {
        reply.status
    } else {
        // A non-streamed success is not something this endpoint can relay.
        502
    };
    let body = serde_json::from_slice::<Value>(&reply.body).ok();
    let mut frame = match body.as_ref().and_then(|body| body.get("error")) {
        Some(error @ Value::Object(_)) => {
            json!({"type": "error", "status": status, "error": error})
        }
        _ => {
            let message = body
                .as_ref()
                .and_then(|body| body.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("the request could not be served");
            ws_error_frame(status, message, None, None)
        }
    };
    // On a socket there are no response headers; the vendor puts the ones
    // that matter into the error object.
    if let Some(wait) = reply.header("retry-after")
        && let Some(error) = frame.get_mut("error").and_then(Value::as_object_mut)
    {
        error.insert("headers".to_string(), json!({"retry-after": wait}));
    }
    frame
}

/// Whether an error status leaves the connection usable: the request was
/// at fault and the client can send a better one. Everything else — rate
/// limits, upstream and gateway failures — closes the connection, so the
/// client reconnects and is routed afresh.
fn is_request_fault(status: u16) -> bool {
    matches!(status, 400 | 403 | 404 | 409 | 413 | 422)
}

/// Why the session loop ended.
enum End {
    /// The connection is gone or unusable; nothing more can be said.
    Gone,
    /// The client sent a close frame.
    ClientClosed,
    /// The client sent a message over the size limit.
    Oversized,
    /// Close with this code and reason.
    Close(u16, &'static str),
}

async fn send_text(socket: &mut ClientSocket, text: String) -> bool {
    socket.send(Message::Text(text.into())).await.is_ok()
}

impl Session {
    async fn run(self, mut socket: ClientSocket) {
        let _connected = self.context.gateway().telemetry().track_ws();
        // Without a lifecycle (the router is served by something other than
        // `BoundServer::serve`) these tokens simply never fire.
        let (shutdown, kill) = match &self.context.lifecycle {
            Some(Lifecycle { shutdown, kill, .. }) => (shutdown.clone(), kill.clone()),
            None => (CancellationToken::new(), CancellationToken::new()),
        };
        let mut ping = self.keepalive.map(|every| {
            let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
            interval
        });

        let mut transcript = Transcript::default();
        let mut queue: VecDeque<Queued> = VecDeque::new();
        let mut queued_bytes = 0usize;
        let mut turn: Option<Turn> = None;
        // What the client had sent in all at the last keep-alive tick, and
        // the pings it has left unanswered since.
        let mut heard = socket.get_ref().bytes_heard();
        let mut unanswered = 0u32;
        let mut draining = false;

        let end = loop {
            if turn.is_none() {
                if draining {
                    break End::Close(GOING_AWAY, "the server is shutting down");
                }
                if let Some(next) = queue.pop_front() {
                    queued_bytes = queued_bytes.saturating_sub(next.bytes);
                    match self.begin(&mut transcript, next.message) {
                        Begun::Turn(started) => turn = Some(*started),
                        Begun::Frames(frames) => {
                            let mut delivered = true;
                            for frame in frames {
                                delivered = delivered && send_text(&mut socket, frame).await;
                            }
                            if !delivered {
                                break End::Gone;
                            }
                        }
                    }
                    continue;
                }
            }

            tokio::select! {
                biased;
                _ = kill.cancelled() => {
                    break End::Close(GOING_AWAY, "the server is shutting down");
                }
                _ = shutdown.cancelled(), if !draining => {
                    // The turn in progress may finish; nothing new starts.
                    draining = true;
                }
                incoming = socket.next() => {
                    let payload: Bytes = match incoming {
                        None => break End::Gone,
                        Some(Err(WsError::Capacity(_))) => break End::Oversized,
                        Some(Err(_)) => break End::Gone,
                        Some(Ok(Message::Close(_))) => break End::ClientClosed,
                        Some(Ok(Message::Text(text))) => text.into(),
                        Some(Ok(Message::Binary(bytes))) => bytes,
                        // Pings are answered by the WebSocket layer.
                        Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {
                            continue;
                        }
                    };
                    let accepted = accept(&payload, queue.len(), queued_bytes, self.max_bytes);
                    match accepted {
                        Ok(message) => {
                            queued_bytes = queued_bytes.saturating_add(payload.len());
                            queue.push_back(Queued { message, bytes: payload.len() });
                        }
                        Err(fault) => {
                            if !send_text(&mut socket, fault.frame().to_string()).await {
                                break End::Gone;
                            }
                        }
                    }
                }
                step = advance(&mut turn), if turn.is_some() => match step {
                    Step::Reply(Reply::Stream(stream)) => {
                        if let Some(turn) = turn.as_mut() {
                            turn.state = TurnState::Streaming(stream.events);
                        }
                    }
                    Step::Reply(Reply::Full(full)) => {
                        let lane = turn.take().and_then(|turn| turn.lane);
                        let frame = with_lane(error_frame(&full).to_string(), lane.as_deref());
                        if !send_text(&mut socket, frame).await {
                            break End::Gone;
                        }
                        if !is_request_fault(full.status) {
                            break End::Close(INTERNAL_ERROR, "the request failed upstream");
                        }
                    }
                    Step::Event(Some(event)) => {
                        // `[DONE]` is SSE framing; a socket has none.
                        if event.is_done_marker() {
                            continue;
                        }
                        let lane = match turn.as_mut() {
                            Some(turn) => {
                                turn.tracker.observe(&event);
                                turn.lane.clone()
                            }
                            None => None,
                        };
                        if !send_text(&mut socket, with_lane(event.data, lane.as_deref())).await {
                            break End::Gone;
                        }
                    }
                    Step::Event(None) => {
                        let Some(finished) = turn.take() else {
                            continue;
                        };
                        match finished.tracker.outcome {
                            Some(Outcome::Completed { output, response_id }) => {
                                transcript.commit(
                                    Completed {
                                        lane: finished.lane,
                                        request: finished.request,
                                        request_bytes: finished.request_bytes,
                                        output,
                                        response_id,
                                    },
                                    self.max_bytes,
                                );
                            }
                            Some(Outcome::Failed) => {
                                break End::Close(INTERNAL_ERROR, "the response failed upstream");
                            }
                            None => {
                                break End::Close(
                                    INTERNAL_ERROR,
                                    "the response stream ended before it completed",
                                );
                            }
                        }
                    }
                },
                _ = tick(&mut ping) => {
                    // Bytes, not messages: a client in the middle of sending
                    // a large request cannot answer a ping (its pong waits
                    // behind the frame on the wire), but it is plainly there.
                    let heard_now = socket.get_ref().bytes_heard();
                    if heard_now != heard {
                        heard = heard_now;
                        unanswered = 0;
                    }
                    let allowed = self
                        .keepalive
                        .map_or(MAX_UNANSWERED_PINGS, |every| patience(every, turn.is_some()));
                    if unanswered >= allowed {
                        tracing::debug!(session = %self.id, "a WebSocket client stopped answering pings");
                        break End::Gone;
                    }
                    unanswered = unanswered.saturating_add(1);
                    if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                        break End::Gone;
                    }
                }
            }
        };

        // Whatever was in progress ends with the connection.
        drop(turn);
        match end {
            End::Gone => {}
            End::ClientClosed => super::await_close(&mut socket).await,
            End::Oversized => super::refuse_oversized(&mut socket).await,
            End::Close(code, reason) => super::close(&mut socket, code, reason).await,
        }
    }

    /// Starts on a queued message: a turn through the pipeline, or frames
    /// to answer with right away (a prewarm's two events, an error).
    fn begin(&self, transcript: &mut Transcript, message: Map<String, Value>) -> Begun {
        let lane_hint = transcript::lane_of(&message);
        let (request, body, lane) = match transcript.prepare(message, now_unix(), self.max_bytes) {
            Ok(Prepared::Turn {
                request,
                body,
                lane,
            }) => (request, body, lane),
            Ok(Prepared::Prewarm {
                created,
                completed,
                lane,
            }) => {
                return Begun::Frames(vec![
                    with_lane(created.to_string(), lane.as_deref()),
                    with_lane(completed.to_string(), lane.as_deref()),
                ]);
            }
            Err(fault) => {
                return Begun::Frames(vec![fault_frame(&fault, lane_hint.as_deref())]);
            }
        };

        let request_bytes = body.len();
        let cancel = CancellationToken::new();
        let mut client_request = ClientRequest::new(
            Protocol::OpenaiResponses,
            ENDPOINT,
            body,
            self.context.identity.clone(),
        );
        client_request.transport = Transport::Websocket;
        client_request.session = Some(self.id.clone());
        client_request.headers = self.context.headers.clone();
        client_request.client_ip = self.context.client_ip.clone();
        client_request.cancel = cancel.clone();

        let gateway = self.context.gateway().clone();
        let reply: BoxFuture<'static, Reply> =
            Box::pin(async move { gateway.generate(client_request).await });
        Begun::Turn(Box::new(Turn {
            request,
            request_bytes,
            lane,
            state: TurnState::Starting(reply),
            tracker: Tracker::default(),
            _cancel: cancel.drop_guard(),
        }))
    }
}

/// What starting on a message led to.
enum Begun {
    Turn(Box<Turn>),
    Frames(Vec<String>),
}

fn fault_frame(fault: &Fault, lane: Option<&str>) -> String {
    with_lane(fault.frame().to_string(), lane)
}

/// Parses a client message and checks that it may join the queue.
fn accept(
    payload: &[u8],
    queued: usize,
    queued_bytes: usize,
    max_queued_bytes: usize,
) -> Result<Map<String, Value>, Fault> {
    let message = match serde_json::from_slice::<Value>(payload) {
        Ok(Value::Object(message)) => message,
        _ => {
            return Err(Fault::invalid(
                "invalid websocket request: each message must be one JSON object",
            ));
        }
    };
    let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
    if !transcript::is_request_type(kind) {
        return Err(Fault::invalid(format!(
            "unsupported websocket request type: {}",
            transcript::shown_type(&message)
        )));
    }
    if queued >= MAX_QUEUED || queued_bytes.saturating_add(payload.len()) > max_queued_bytes {
        return Err(Fault {
            status: 429,
            message: format!(
                "too many requests are waiting on this websocket (at most {MAX_QUEUED}); wait for a response to finish"
            ),
            code: Some("websocket_queue_full"),
            param: None,
        });
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn named(name: &str, data: Value) -> SseEvent {
        SseEvent::named(name, data.to_string())
    }

    #[test]
    fn the_tracker_takes_the_output_of_the_terminal_event() {
        let mut tracker = Tracker::default();
        tracker.observe(&named(
            "response.created",
            json!({"type": "response.created"}),
        ));
        tracker.observe(&named(
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "delta": "hi"}),
        ));
        assert_eq!(tracker.outcome, None);
        tracker.observe(&named(
            "response.completed",
            json!({"type": "response.completed", "response": {
                "id": "resp_1", "output": [{"type": "message", "id": "m1"}]
            }}),
        ));
        assert_eq!(
            tracker.outcome,
            Some(Outcome::Completed {
                output: vec![json!({"type": "message", "id": "m1"})],
                response_id: Some("resp_1".into()),
            })
        );
    }

    #[test]
    fn an_empty_terminal_output_falls_back_to_the_items_seen() {
        let mut tracker = Tracker::default();
        // Items of an earlier response in the same stream do not count.
        tracker.observe(&named(
            "response.output_item.done",
            json!({"output_index": 0, "item": {"id": "stale"}}),
        ));
        tracker.observe(&named("response.created", json!({})));
        for (index, id) in [(Some(1), "b"), (None, "z"), (Some(0), "a")] {
            let mut event = json!({"type": "response.output_item.done", "item": {"id": id}});
            if let Some(index) = index {
                event["output_index"] = json!(index);
            }
            // Unnamed events are recognised by their type.
            tracker.observe(&SseEvent::data(event.to_string()));
        }
        tracker.observe(&named(
            "response.incomplete",
            json!({"response": {"id": "resp_2", "output": []}}),
        ));
        assert_eq!(
            tracker.outcome,
            Some(Outcome::Completed {
                output: vec![json!({"id": "a"}), json!({"id": "b"}), json!({"id": "z"})],
                response_id: Some("resp_2".into()),
            })
        );
    }

    #[test]
    fn errors_fail_the_turn_unless_it_already_completed() {
        let mut tracker = Tracker::default();
        tracker.observe(&named("error", json!({"type": "error", "message": "x"})));
        assert_eq!(tracker.outcome, Some(Outcome::Failed));

        let mut tracker = Tracker::default();
        tracker.observe(&named("response.failed", json!({"response": {}})));
        assert_eq!(tracker.outcome, Some(Outcome::Failed));

        let mut tracker = Tracker::default();
        tracker.observe(&named(
            "response.completed",
            json!({"response": {"id": "r"}}),
        ));
        tracker.observe(&named("error", json!({})));
        assert!(matches!(tracker.outcome, Some(Outcome::Completed { .. })));

        // Garbage is ignored.
        let mut tracker = Tracker::default();
        tracker.observe(&SseEvent::data("not json"));
        tracker.observe(&SseEvent::named("response.completed", "{not json"));
        assert_eq!(tracker.outcome, None);
    }

    #[test]
    fn lanes_are_spliced_in_front() {
        assert_eq!(with_lane("{\"a\":1}".into(), None), "{\"a\":1}");
        assert_eq!(
            with_lane("{\"a\":1}".into(), Some("main")),
            "{\"stream_id\":\"main\",\"a\":1}"
        );
        assert_eq!(with_lane("{}".into(), Some("x")), "{\"stream_id\":\"x\"}");
        assert_eq!(with_lane("[1]".into(), Some("x")), "[1]");
        let spliced: Value =
            serde_json::from_str(&with_lane(" { \"type\": \"error\" }".into(), Some("l-1")))
                .unwrap();
        assert_eq!(spliced, json!({"stream_id": "l-1", "type": "error"}));
    }

    fn reply(status: u16, body: &str, headers: &[(&str, &str)]) -> FullReply {
        FullReply {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            content_type: "application/json".into(),
            body: Bytes::from(body.to_string()),
            request_id: "req".into(),
        }
    }

    #[test]
    fn error_frames_carry_the_replys_error_object() {
        let frame = error_frame(&reply(
            404,
            r#"{"error":{"message":"unknown model `x`","type":"invalid_request_error","param":"model","code":"model_not_found"}}"#,
            &[],
        ));
        assert_eq!(
            frame,
            json!({"type": "error", "status": 404, "error": {
                "message": "unknown model `x`", "type": "invalid_request_error",
                "param": "model", "code": "model_not_found"
            }})
        );

        let frame = error_frame(&reply(
            429,
            r#"{"error":{"message":"cooling down","type":"rate_limit_error"}}"#,
            &[("retry-after", "7")],
        ));
        assert_eq!(frame["status"], 429);
        assert_eq!(frame["error"]["headers"], json!({"retry-after": "7"}));

        // Not an OpenAI envelope: a frame is built from the status.
        let frame = error_frame(&reply(502, "<html>bad gateway</html>", &[]));
        assert_eq!(frame["status"], 502);
        assert_eq!(frame["error"]["type"], "server_error");
        assert_eq!(frame["error"]["message"], "the request could not be served");

        // A success that is not a stream cannot be relayed.
        let frame = error_frame(&reply(200, "{}", &[]));
        assert_eq!(frame["status"], 502);
    }

    #[test]
    fn request_faults_keep_the_connection() {
        for status in [400, 403, 404, 409, 413, 422] {
            assert!(is_request_fault(status), "{status}");
        }
        for status in [200, 401, 408, 429, 499, 500, 502, 503, 504] {
            assert!(!is_request_fault(status), "{status}");
        }
    }

    #[test]
    fn an_idle_client_is_given_longer_to_answer_than_one_in_a_turn() {
        let secs = Duration::from_secs;
        // During a turn the client is reading: two pings, then gone.
        assert_eq!(patience(secs(15), true), 2);
        assert_eq!(patience(secs(1), true), 2);
        // Between turns it may not be reading at all.
        assert_eq!(patience(secs(15), false), 40);
        assert_eq!(patience(secs(1), false), 600);
        assert_eq!(patience(secs(7), false), 86, "rounded up");
        // Never less patient than during a turn, whatever the interval.
        assert_eq!(patience(secs(3_600), false), 2);
        assert_eq!(patience(Duration::ZERO, false), 600_000);
    }

    #[test]
    fn only_requests_join_the_queue() {
        let ok = accept(br#"{"type":"response.create","model":"m"}"#, 0, 0, 1024);
        assert_eq!(ok.unwrap()["model"], "m");
        assert!(accept(br#"{"type":"response.append"}"#, 15, 0, 1024).is_ok());

        let not_json = accept(b"hello", 0, 0, 1024).unwrap_err();
        assert_eq!(not_json.status, 400);
        let not_object = accept(b"[1,2]", 0, 0, 1024).unwrap_err();
        assert_eq!(not_object.status, 400);
        let unknown = accept(br#"{"type":"response.cancel"}"#, 0, 0, 1024).unwrap_err();
        assert_eq!(
            unknown.message,
            "unsupported websocket request type: response.cancel"
        );
        let untyped = accept(br#"{"model":"m"}"#, 0, 0, 1024).unwrap_err();
        assert_eq!(
            untyped.message,
            "unsupported websocket request type: (missing)"
        );

        let full = accept(br#"{"type":"response.create"}"#, 16, 0, 1024).unwrap_err();
        assert_eq!(full.status, 429);
        assert_eq!(full.code, Some("websocket_queue_full"));
        let heavy = accept(br#"{"type":"response.create"}"#, 1, 1020, 1024).unwrap_err();
        assert_eq!(heavy.status, 429);
    }
}
