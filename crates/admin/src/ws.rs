//! `GET /ws`: the live-event WebSocket.
//!
//! Server → client, JSON text frames `{"type": …, "data": …}`:
//!
//! * `hello` — once, first: `{version, topics, server_time, started_at}`;
//! * every event of the telemetry bus as `Event::to_frame` renders it:
//!   `request.started`, `request.finished`, `log`, `credential`,
//!   `config.reloaded`;
//! * `stats` — once a second: the usage store's tick plus the gauges;
//! * `subscribed` — the answer to a `subscribe` message: `{topics}`;
//! * `pong` — the answer to a `ping` message (no `data`);
//! * `lagged` — `{missed}`: this connection fell behind and that many
//!   events were dropped for it. The stream continues.
//!
//! Client → server: `{"type":"subscribe","topics":[…]}` replaces the set of
//! frame types the connection receives (initially all of them; `hello` and
//! `stats` are topics like the others), and `{"type":"ping"}`. Anything
//! else is ignored.
//!
//! A connection never slows the gateway down: events it cannot take in time
//! are dropped for it (and counted in `lagged`), and a client that does not
//! take a frame within ten seconds, or stops answering WebSocket pings, is
//! disconnected.

use crate::auth::{AuthContext, digest};
use crate::error::ApiFailure;
use crate::{AdminState, Shared};
use axum::Extension;
use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Duration;
use switchyard_core::util::now_unix_ms;
use switchyard_gateway::Gateway;
use switchyard_telemetry::TOPICS;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, MissedTickBehavior};

/// How often a `stats` frame is sent.
const STATS_EVERY: Duration = Duration::from_secs(1);
/// How often a WebSocket ping is sent.
const PING_EVERY: Duration = Duration::from_secs(20);
/// A client that has sent nothing — not even a pong — for this long is gone.
const SILENCE_LIMIT: Duration = Duration::from_secs(50);
/// Longest a single frame may take to be accepted by the socket.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest message a client may send; the protocol's messages are tiny.
const MAX_CLIENT_MESSAGE: usize = 64 * 1024;

/// Frame types beyond the telemetry topics.
const HELLO: &str = "hello";
const STATS: &str = "stats";

/// Every topic a client can subscribe to, in documentation order.
fn all_topics() -> Vec<&'static str> {
    let mut topics = vec![HELLO];
    topics.extend(TOPICS);
    topics.push(STATS);
    topics
}

/// The upgrade. The guard in front of this route has already checked the
/// ticket and the loopback rule.
pub(crate) async fn live(
    State(state): State<Shared>,
    Extension(context): Extension<AuthContext>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(_) => {
            return ApiFailure::bad_request("this route only answers WebSocket upgrade requests")
                .into_response();
        }
    };
    if state.shutdown.is_cancelled() {
        return ApiFailure::new(
            http::StatusCode::SERVICE_UNAVAILABLE,
            "the gateway is shutting down",
        )
        .into_response();
    }
    upgrade
        .max_message_size(MAX_CLIENT_MESSAGE)
        .max_frame_size(MAX_CLIENT_MESSAGE)
        .on_upgrade(move |socket| async move {
            let _counted = Counted::new(&state);
            serve(&state, socket, context).await;
        })
}

/// Counts a live socket for as long as it exists, whatever way it ends.
struct Counted<'a>(&'a AdminState);

impl<'a> Counted<'a> {
    fn new(state: &'a AdminState) -> Self {
        state.live_sockets.fetch_add(1, Ordering::AcqRel);
        Counted(state)
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.live_sockets.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A message from the client.
#[derive(Deserialize)]
struct ClientMessage {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    topics: Option<Vec<Value>>,
}

/// The frame types a connection wants. `None`: everything.
#[derive(Debug, Default)]
struct Subscription {
    topics: Option<HashSet<String>>,
}

impl Subscription {
    fn wants(&self, topic: &str) -> bool {
        self.topics
            .as_ref()
            .is_none_or(|topics| topics.contains(topic))
    }

    /// Replaces the wanted set. Entries that are not strings are ignored;
    /// unknown names are kept, so a newer dashboard can ask an older
    /// gateway for topics it does not have yet without an error.
    fn set(&mut self, topics: Vec<Value>) -> Vec<String> {
        let mut wanted = Vec::new();
        for topic in topics {
            if let Value::String(topic) = topic
                && !wanted.contains(&topic)
            {
                wanted.push(topic);
            }
        }
        self.topics = Some(wanted.iter().cloned().collect());
        wanted
    }
}

/// What a text message from the client asks for.
#[derive(Debug, PartialEq)]
enum Request {
    Subscribe(Vec<Value>),
    Ping,
    Ignore,
}

fn parse_client_message(text: &str) -> Request {
    match serde_json::from_str::<ClientMessage>(text) {
        Ok(message) => match message.kind.as_str() {
            "subscribe" => Request::Subscribe(message.topics.unwrap_or_default()),
            "ping" => Request::Ping,
            _ => Request::Ignore,
        },
        Err(_) => Request::Ignore,
    }
}

fn frame(kind: &str, data: Value) -> Message {
    Message::text(json!({ "type": kind, "data": data }).to_string())
}

/// The `stats` payload: the usage store's tick (requests and tokens per
/// minute, error rate, latency percentiles) with the live gauges and the
/// totals since start laid over it.
fn stats_data(gateway: &Gateway) -> Value {
    let telemetry = gateway.telemetry();
    let now = now_unix_ms();
    let mut data = serde_json::to_value(telemetry.stats_tick(now)).unwrap_or_else(|_| json!({}));
    let gauges = telemetry.status(now);
    if let Value::Object(map) = &mut data {
        map.insert("in_flight".to_string(), json!(gauges.in_flight));
        map.insert("active_streams".to_string(), json!(gauges.active_streams));
        map.insert("ws_connections".to_string(), json!(gauges.ws_connections));
        map.insert("uptime_ms".to_string(), json!(gauges.uptime_ms));
        map.insert("totals".to_string(), json!(gauges.totals));
    }
    data
}

/// Whether the connection may stay: the admin interface is still on, the
/// secret it was opened under is still the secret, and its peer is still
/// allowed. Checked once a second, so switching the admin API off or
/// rotating the secret ends live sessions too.
fn still_admitted(state: &AdminState, context: &AuthContext) -> bool {
    let access = state.access();
    access
        .secret()
        .is_some_and(|secret| digest(secret.as_bytes()) == context.secret_digest)
        && (!context.peer.remote || access.allow_remote)
}

/// Sends one frame, giving up on a client that does not take it in time.
async fn send(socket: &mut WebSocket, message: Message) -> bool {
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, socket.send(message)).await,
        Ok(Ok(()))
    )
}

async fn close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    // The peer may already be gone; there is nobody left to tell.
    let _ = tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Close(Some(frame)))).await;
}

async fn serve(state: &AdminState, mut socket: WebSocket, context: AuthContext) {
    let gateway = &state.gateway;
    // Subscribed before `hello` goes out: whatever happens after the client
    // has seen `hello` reaches it.
    let mut events = gateway.telemetry().subscribe();
    let mut subscription = Subscription::default();

    // `started_at` tells a reconnecting client whether it is talking to the
    // process it knew: after a restart, log sequence numbers and the totals
    // since start begin again.
    let hello = json!({
        "version": Gateway::version(),
        "topics": all_topics(),
        "server_time": now_unix_ms(),
        "started_at": gateway.telemetry().gauges().started_at(),
    });
    if !send(&mut socket, frame(HELLO, hello)).await {
        return;
    }

    let mut stats = tokio::time::interval(STATS_EVERY);
    stats.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut ping = tokio::time::interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_heard = Instant::now();

    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => {
                close(&mut socket, close_code::AWAY, "the gateway is shutting down").await;
                return;
            }
            _ = stats.tick() => {
                if !still_admitted(state, &context) {
                    close(&mut socket, close_code::POLICY, "admin access was revoked").await;
                    return;
                }
                if subscription.wants(STATS)
                    && !send(&mut socket, frame(STATS, stats_data(gateway))).await
                {
                    return;
                }
            }
            _ = ping.tick() => {
                if last_heard.elapsed() > SILENCE_LIMIT {
                    close(&mut socket, close_code::AWAY, "no answer to pings").await;
                    return;
                }
                if !send(&mut socket, Message::Ping(Bytes::new())).await {
                    return;
                }
            }
            event = events.recv() => match event {
                Ok(event) => {
                    if subscription.wants(event.topic())
                        && !send(&mut socket, Message::text(event.to_frame().to_string())).await
                    {
                        return;
                    }
                }
                Err(RecvError::Lagged(missed)) => {
                    if !send(&mut socket, frame("lagged", json!({ "missed": missed }))).await {
                        return;
                    }
                }
                // The gateway is gone.
                Err(RecvError::Closed) => {
                    close(&mut socket, close_code::AWAY, "the gateway is shutting down").await;
                    return;
                }
            },
            message = socket.recv() => {
                last_heard = Instant::now();
                match message {
                    Some(Ok(Message::Text(text))) => match parse_client_message(text.as_str()) {
                        Request::Subscribe(topics) => {
                            let topics = subscription.set(topics);
                            if !send(&mut socket, frame("subscribed", json!({ "topics": topics }))).await {
                                return;
                            }
                        }
                        Request::Ping => {
                            let pong = Message::text(json!({ "type": "pong" }).to_string());
                            if !send(&mut socket, pong).await {
                                return;
                            }
                        }
                        Request::Ignore => {}
                    },
                    // Pings are answered by the WebSocket layer; pongs and
                    // binary frames only prove the client is alive.
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_))) => {}
                    // A close frame (already answered by the layer), a
                    // protocol error or the end of the connection.
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn topics_cover_the_bus_and_the_two_extras() {
        assert_eq!(
            all_topics(),
            [
                "hello",
                "request.started",
                "request.finished",
                "log",
                "credential",
                "config.reloaded",
                "stats",
            ]
        );
    }

    #[test]
    fn a_new_connection_wants_everything_and_subscribe_narrows() {
        let mut subscription = Subscription::default();
        for topic in all_topics() {
            assert!(subscription.wants(topic));
        }
        let kept = subscription.set(vec![
            json!("stats"),
            json!("log"),
            json!(7),
            json!("stats"),
            json!("future.topic"),
        ]);
        assert_eq!(kept, ["stats", "log", "future.topic"]);
        assert!(subscription.wants("stats"));
        assert!(subscription.wants("log"));
        assert!(!subscription.wants("request.started"));
        assert!(!subscription.wants("hello"));
        // An empty list silences the stream.
        assert!(subscription.set(Vec::new()).is_empty());
        assert!(!subscription.wants("stats"));
    }

    #[test]
    fn client_messages_are_read_leniently() {
        assert_eq!(
            parse_client_message(r#"{"type":"subscribe","topics":["log"]}"#),
            Request::Subscribe(vec![json!("log")])
        );
        assert_eq!(
            parse_client_message(r#"{"type":"subscribe"}"#),
            Request::Subscribe(Vec::new())
        );
        assert_eq!(
            parse_client_message(r#"{"type":"ping","extra":1}"#),
            Request::Ping
        );
        assert_eq!(parse_client_message(r#"{"type":"dance"}"#), Request::Ignore);
        assert_eq!(parse_client_message("not json"), Request::Ignore);
        assert_eq!(
            parse_client_message(r#"{"topics":["log"]}"#),
            Request::Ignore
        );
        assert_eq!(
            parse_client_message(r#"{"type":"subscribe","topics":"log"}"#),
            Request::Ignore
        );
    }
}
