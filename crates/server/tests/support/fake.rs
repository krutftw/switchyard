//! A tiny fake upstream speaking just enough of the OpenAI API for the
//! server tests: embeddings, a Chat Completions stream with a pause in it,
//! a recording Responses endpoint, and a Realtime WebSocket that echoes.

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use bytes::Bytes;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One request the fake received.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub path: String,
    pub query: String,
    pub authorization: Option<String>,
    pub subprotocols: Option<String>,
    pub body: Value,
}

#[derive(Clone, Default)]
struct Shared {
    recorded: Arc<Mutex<Vec<Recorded>>>,
    /// How long the Chat stream pauses between its two text chunks.
    pause: Arc<Mutex<Duration>>,
    /// Close frames the Realtime endpoint received: code and reason.
    closes: Arc<Mutex<Vec<(u16, String)>>>,
    /// Realtime sessions that have ended.
    ended: Arc<Mutex<usize>>,
}

impl Shared {
    fn record(&self, path: &str, query: Option<String>, headers: &HeaderMap, body: Value) -> usize {
        let text = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let mut recorded = self.recorded.lock().unwrap();
        recorded.push(Recorded {
            path: path.to_string(),
            query: query.unwrap_or_default(),
            authorization: text("authorization"),
            subprotocols: text("sec-websocket-protocol"),
            body,
        });
        recorded.len()
    }
}

/// The running fake.
pub struct Fake {
    addr: SocketAddr,
    shared: Shared,
}

impl Fake {
    pub async fn start() -> Fake {
        let shared = Shared::default();
        let app = Router::new()
            .route("/v1/embeddings", post(embeddings))
            .route("/v1/chat/completions", post(chat))
            .route("/v1/responses", post(responses))
            .route("/v1/realtime", any(realtime))
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Fake { addr, shared }
    }

    /// `http://127.0.0.1:port`
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Everything received so far, in order.
    pub fn recorded(&self) -> Vec<Recorded> {
        self.shared.recorded.lock().unwrap().clone()
    }

    /// The requests received on `path`.
    pub fn on(&self, path: &str) -> Vec<Recorded> {
        self.recorded()
            .into_iter()
            .filter(|recorded| recorded.path == path)
            .collect()
    }

    /// Makes the Chat stream pause this long between its text chunks.
    pub fn pause_chat(&self, pause: Duration) {
        *self.shared.pause.lock().unwrap() = pause;
    }

    /// The close frames Realtime sessions received from the gateway.
    pub fn closes(&self) -> Vec<(u16, String)> {
        self.shared.closes.lock().unwrap().clone()
    }

    /// How many Realtime sessions have ended.
    pub fn ended_sessions(&self) -> usize {
        *self.shared.ended.lock().unwrap()
    }
}

fn json_body(body: &Bytes) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

async fn embeddings(
    State(shared): State<Shared>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = json_body(&body);
    shared.record("/v1/embeddings", query, &headers, body.clone());
    let reply = json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.25, -0.5]}],
        "model": body["model"],
        "usage": {"prompt_tokens": 2, "total_tokens": 2}
    });
    (
        [
            ("content-type", "application/json"),
            ("x-ratelimit-remaining-requests", "41"),
        ],
        reply.to_string(),
    )
        .into_response()
}

fn sse(event: Option<&str>, data: &Value) -> Bytes {
    let mut text = String::new();
    if let Some(event) = event {
        text.push_str(&format!("event: {event}\n"));
    }
    text.push_str(&format!("data: {data}\n\n"));
    Bytes::from(text)
}

fn event_stream(
    chunks: impl futures::Stream<Item = Result<Bytes, Infallible>> + Send + 'static,
) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(chunks))
        .unwrap()
}

/// A Chat Completions stream: "Hel", a pause, "lo", the finish chunk.
async fn chat(
    State(shared): State<Shared>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = json_body(&body);
    shared.record("/v1/chat/completions", query, &headers, body.clone());
    let pause = *shared.pause.lock().unwrap();
    let model = body["model"].clone();
    let chunk = move |delta: Value, finish: Value| {
        json!({
            "id": "chatcmpl-fake", "object": "chat.completion.chunk", "created": 1, "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
    };
    if body["stream"] != json!(true) {
        return (
            [("content-type", "application/json")],
            json!({
                "id": "chatcmpl-fake", "object": "chat.completion", "created": 1,
                "model": body["model"],
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello"},
                             "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })
            .to_string(),
        )
            .into_response();
    }
    let chunks = async_stream(move |tx| async move {
        let _ = tx
            .send(sse(
                None,
                &chunk(json!({"role": "assistant", "content": "Hel"}), Value::Null),
            ))
            .await;
        tokio::time::sleep(pause).await;
        let _ = tx
            .send(sse(None, &chunk(json!({"content": "lo"}), Value::Null)))
            .await;
        let _ = tx.send(sse(None, &chunk(json!({}), json!("stop")))).await;
        let _ = tx.send(Bytes::from_static(b"data: [DONE]\n\n")).await;
    });
    event_stream(chunks)
}

/// Runs `producer` in a task and returns what it sends as a body stream.
fn async_stream<F, Fut>(
    producer: F,
) -> impl futures::Stream<Item = Result<Bytes, Infallible>> + Send + 'static
where
    F: FnOnce(tokio::sync::mpsc::Sender<Bytes>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    tokio::spawn(producer(tx));
    futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|bytes| (Ok(bytes), rx))
    })
}

/// The text of the last item of a Responses `input`.
fn last_input_text(body: &Value) -> String {
    let Some(last) = body["input"].as_array().and_then(|input| input.last()) else {
        return String::new();
    };
    match &last["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect(),
        _ => last["output"].as_str().unwrap_or("").to_string(),
    }
}

/// A recording Responses endpoint. The n-th request is answered by
/// response `resp_n`: a function call when the last input says "use tool",
/// a message "answer n" otherwise. A stream whose last input says "fail
/// midway" breaks with an `error` event after `response.created`.
async fn responses(
    State(shared): State<Shared>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = json_body(&body);
    let n = shared.record("/v1/responses", query, &headers, body.clone());
    let id = format!("resp_{n}");
    let model = body["model"].clone();
    let wants_tool = last_input_text(&body).contains("use tool");
    let item = if wants_tool {
        json!({"type": "function_call", "id": format!("fc_{n}"), "call_id": format!("call_{n}"),
               "name": "lookup", "arguments": "{}", "status": "completed"})
    } else {
        json!({"type": "message", "id": format!("msg_{n}"), "role": "assistant", "status": "completed",
               "content": [{"type": "output_text", "text": format!("answer {n}"), "annotations": []}]})
    };
    let response = |status: &str, output: Value| {
        json!({"id": id, "object": "response", "created_at": 1, "status": status, "model": model,
               "output": output,
               "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}})
    };
    if body["stream"] != json!(true) {
        return (
            [("content-type", "application/json")],
            response("completed", json!([item])).to_string(),
        )
            .into_response();
    }

    let mut events = vec![sse(
        Some("response.created"),
        &json!({"type": "response.created", "sequence_number": 0,
                "response": response("in_progress", json!([]))}),
    )];
    if last_input_text(&body).contains("fail midway") {
        // A stream that breaks after its first event.
        events.push(sse(
            Some("error"),
            &json!({"type": "error", "sequence_number": 1, "code": "server_error",
                    "message": "the upstream fell over", "param": null}),
        ));
        return event_stream(futures::stream::iter(events.into_iter().map(Ok)));
    }
    if wants_tool {
        let mut added = item.clone();
        added["arguments"] = json!("");
        added["status"] = json!("in_progress");
        events.push(sse(
            Some("response.output_item.added"),
            &json!({"type": "response.output_item.added", "sequence_number": 1,
                    "output_index": 0, "item": added}),
        ));
        events.push(sse(
            Some("response.function_call_arguments.done"),
            &json!({"type": "response.function_call_arguments.done", "sequence_number": 2,
                    "item_id": item["id"], "output_index": 0, "arguments": "{}"}),
        ));
    } else {
        events.push(sse(
            Some("response.output_item.added"),
            &json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0,
                    "item": {"type": "message", "id": item["id"], "role": "assistant",
                             "status": "in_progress", "content": []}}),
        ));
        events.push(sse(
            Some("response.output_text.delta"),
            &json!({"type": "response.output_text.delta", "sequence_number": 2,
                    "item_id": item["id"], "output_index": 0, "content_index": 0,
                    "delta": format!("answer {n}")}),
        ));
    }
    events.push(sse(
        Some("response.output_item.done"),
        &json!({"type": "response.output_item.done", "sequence_number": 3, "output_index": 0,
                "item": item}),
    ));
    events.push(sse(
        Some("response.completed"),
        &json!({"type": "response.completed", "sequence_number": 4,
                "response": response("completed", json!([item]))}),
    ));
    event_stream(futures::stream::iter(events.into_iter().map(Ok)))
}

/// The Realtime endpoint: greets, then echoes. The text `close-me` makes
/// it close with code 4001; `fail-me` with 1011.
async fn realtime(
    State(shared): State<Shared>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    shared.record("/v1/realtime", query, &headers, Value::Null);
    upgrade
        .protocols(["realtime"])
        .on_upgrade(move |socket| async move {
            realtime_session(socket, &shared).await;
            *shared.ended.lock().unwrap() += 1;
        })
}

async fn realtime_session(mut socket: WebSocket, shared: &Shared) {
    let greeting = json!({"type": "session.created", "session": {"id": "sess_fake"}});
    if socket
        .send(Message::Text(greeting.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    while let Some(Ok(message)) = socket.recv().await {
        match message {
            Message::Text(text) => {
                let (code, reason) = match text.as_str() {
                    "close-me" => (4001, "bye from upstream"),
                    "fail-me" => (1011, "upstream exploded"),
                    "respond" => {
                        let done = json!({"type": "response.done", "response": {"usage": {
                            "total_tokens": 30, "input_tokens": 20, "output_tokens": 10,
                            "input_token_details": {"cached_tokens": 5}
                        }}});
                        if socket
                            .send(Message::Text(done.to_string().into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        continue;
                    }
                    _ => {
                        let echo = format!("echo:{}", text.as_str());
                        if socket.send(Message::Text(echo.into())).await.is_err() {
                            return;
                        }
                        continue;
                    }
                };
                let _ = socket
                    .send(Message::Close(Some(CloseFrame {
                        code,
                        reason: reason.into(),
                    })))
                    .await;
                // Wait for the peer's close before letting go.
                while let Some(Ok(_)) = socket.recv().await {}
                return;
            }
            Message::Binary(bytes) => {
                if socket.send(Message::Binary(bytes)).await.is_err() {
                    return;
                }
            }
            Message::Close(frame) => {
                if let Some(frame) = frame {
                    shared
                        .closes
                        .lock()
                        .unwrap()
                        .push((frame.code, frame.reason.to_string()));
                }
                return;
            }
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }
}
