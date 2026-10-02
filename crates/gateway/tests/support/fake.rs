//! A scriptable fake upstream: one axum server that speaks minimal but
//! protocol-correct OpenAI Chat Completions, OpenAI Responses, Anthropic
//! Messages and Gemini — complete responses and SSE streams — plus the
//! counting, model-listing, embeddings and WebSocket endpoints the gateway
//! uses.
//!
//! What it answers is scripted per API key ([`Fake::script`] queues
//! behaviours that are consumed one per request, [`Fake::always`] sets what
//! a key does once its queue is empty). Every request is recorded so tests
//! can assert on the exact upstream body and headers.
//!
//! The wire shapes are written out by hand from the vendors' API
//! references rather than produced with the gateway's codecs, so the tests
//! check the codecs against an independent rendering.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

/// The access token the fake token endpoint (`POST /token`) hands out.
pub const TOKEN: &str = "ya29.fake-access-token-0123456789";

/// The wire protocol a request arrived in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    Chat,
    Responses,
    Anthropic,
    Gemini,
}

/// What a request asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Generate { stream: bool },
    Count,
    Models,
    Raw,
    WebSocket,
}

/// The content of a successful answer.
#[derive(Clone, Debug)]
pub struct Answer {
    pub text: Option<String>,
    /// A tool call: name and arguments.
    pub tool: Option<(String, Value)>,
    /// Reasoning: visible text and the opaque signature that goes with it.
    pub reasoning: Option<(String, String)>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Answer {
    pub fn text(text: &str) -> Self {
        Answer {
            text: Some(text.to_string()),
            tool: None,
            reasoning: None,
            input_tokens: 11,
            output_tokens: 7,
        }
    }

    pub fn tool(name: &str, args: Value) -> Self {
        Answer {
            text: None,
            tool: Some((name.to_string(), args)),
            reasoning: None,
            input_tokens: 11,
            output_tokens: 7,
        }
    }

    pub fn with_reasoning(mut self, text: &str, signature: &str) -> Self {
        self.reasoning = Some((text.to_string(), signature.to_string()));
        self
    }

    pub fn with_usage(mut self, input: u64, output: u64) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }
}

/// How the fake answers one request.
#[derive(Clone, Debug)]
pub enum Behaviour {
    /// A normal answer (a stream when the request asked for one).
    Reply(Answer),
    /// An HTTP error with the vendor's error envelope.
    HttpError {
        status: u16,
        message: String,
        retry_after: Option<u64>,
    },
    /// `200`, then the vendor's in-stream error event before any content.
    StreamError { status: u16, message: String },
    /// `200`, then the connection breaks before any event.
    DieBeforeFirstEvent,
    /// The first `frames` wire events of the answer, then the connection
    /// breaks.
    DieAfter { frames: usize, answer: Answer },
    /// The first `frames` wire events of the answer, then the stream ends
    /// cleanly — but early.
    EndAfter { frames: usize, answer: Answer },
    /// `200` and a body that ends at once.
    EmptyStream,
    /// Waits before answering at all (no response headers until then).
    Slow {
        delay: Duration,
        then: Box<Behaviour>,
    },
    /// `200`, then nothing, for ever.
    Silence,
    /// The first `frames` wire events, then nothing, for ever.
    SilenceAfter { frames: usize, answer: Answer },
    /// `200` with a body that is not JSON.
    Garbage,
}

impl Behaviour {
    pub fn text(text: &str) -> Self {
        Behaviour::Reply(Answer::text(text))
    }

    pub fn error(status: u16, message: &str) -> Self {
        Behaviour::HttpError {
            status,
            message: message.to_string(),
            retry_after: None,
        }
    }

    pub fn rate_limited(retry_after: u64) -> Self {
        Behaviour::HttpError {
            status: 429,
            message: "Rate limit reached".to_string(),
            retry_after: Some(retry_after),
        }
    }
}

/// One request the fake received.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
    /// The body as JSON (`Null` when it is not JSON).
    pub body: Value,
    pub raw: Bytes,
    /// The API key the request presented.
    pub key: String,
    pub wire: Option<Wire>,
    pub kind: Kind,
    /// Model named by the body or, for Gemini, the URL.
    pub model: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

struct FakeState {
    requests: Mutex<Vec<Recorded>>,
    scripts: Mutex<HashMap<String, VecDeque<Behaviour>>>,
    defaults: Mutex<HashMap<String, Behaviour>>,
    disconnects: AtomicUsize,
    changed: Notify,
    count_tokens: AtomicU64,
    models: Mutex<Vec<String>>,
    tokens_minted: AtomicUsize,
}

impl FakeState {
    fn next(&self, key: &str) -> Behaviour {
        if let Some(next) = self
            .scripts
            .lock()
            .unwrap()
            .get_mut(key)
            .and_then(VecDeque::pop_front)
        {
            return next;
        }
        self.defaults
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .unwrap_or_else(|| Behaviour::text("Hello from the fake upstream"))
    }

    fn disconnected(&self) {
        self.disconnects.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}

/// A running fake upstream.
#[derive(Clone)]
pub struct Fake {
    pub addr: SocketAddr,
    state: Arc<FakeState>,
}

impl Fake {
    pub async fn start() -> Fake {
        let state = Arc::new(FakeState {
            requests: Mutex::new(Vec::new()),
            scripts: Mutex::new(HashMap::new()),
            defaults: Mutex::new(HashMap::new()),
            disconnects: AtomicUsize::new(0),
            changed: Notify::new(),
            count_tokens: AtomicU64::new(42),
            models: Mutex::new(vec!["disc-alpha".to_string(), "disc-beta".to_string()]),
            tokens_minted: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route("/v1/realtime", get(websocket))
            .route("/v1/responses", get(websocket).post(dispatch))
            .fallback(dispatch)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Fake { addr, state }
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Queues behaviours for requests presenting `key`, consumed in order.
    pub fn script(&self, key: &str, behaviours: impl IntoIterator<Item = Behaviour>) {
        self.state
            .scripts
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default()
            .extend(behaviours);
    }

    /// What `key` does whenever its queue is empty.
    pub fn always(&self, key: &str, behaviour: Behaviour) {
        self.state
            .defaults
            .lock()
            .unwrap()
            .insert(key.to_string(), behaviour);
    }

    /// The number the counting endpoints report.
    pub fn set_count(&self, tokens: u64) {
        self.state.count_tokens.store(tokens, Ordering::SeqCst);
    }

    /// The ids the model listings report.
    pub fn set_models(&self, ids: &[&str]) {
        *self.state.models.lock().unwrap() = ids.iter().map(|id| id.to_string()).collect();
    }

    /// How many OAuth access tokens the token endpoint handed out.
    pub fn tokens_minted(&self) -> usize {
        self.state.tokens_minted.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.requests.lock().unwrap().clone()
    }

    pub fn requests_with(&self, key: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.key == key)
            .collect()
    }

    /// Keys of the generation requests received, in order.
    pub fn keys(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.key).collect()
    }

    pub fn last(&self) -> Recorded {
        self.requests()
            .pop()
            .expect("the fake upstream received no request")
    }

    pub fn count(&self) -> usize {
        self.state.requests.lock().unwrap().len()
    }

    pub fn clear(&self) {
        self.state.requests.lock().unwrap().clear();
    }

    /// Streams the client abandoned while the fake was still holding them
    /// open.
    pub fn disconnects(&self) -> usize {
        self.state.disconnects.load(Ordering::SeqCst)
    }

    /// Waits until at least `n` streams were abandoned by the client.
    pub async fn wait_for_disconnects(&self, n: usize, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let notified = self.state.changed.notified();
            if self.disconnects() >= n {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.disconnects() >= n;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

fn api_key(headers: &HeaderMap) -> String {
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    text("authorization")
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(&v).to_string())
        .or_else(|| text("x-api-key"))
        .or_else(|| text("x-goog-api-key"))
        .unwrap_or_default()
}

fn classify(
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: &Value,
) -> (Option<Wire>, Kind, String) {
    let body_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let streaming = body.get("stream") == Some(&Value::Bool(true));
    let generate = Kind::Generate { stream: streaming };
    match (method.as_str(), path) {
        ("POST", "/v1/chat/completions") => (Some(Wire::Chat), generate, body_model),
        ("POST", "/v1/responses") => (Some(Wire::Responses), generate, body_model),
        ("POST", "/v1/responses/input_tokens") => (Some(Wire::Responses), Kind::Count, body_model),
        ("POST", "/v1/messages") => (Some(Wire::Anthropic), generate, body_model),
        ("POST", "/v1/messages/count_tokens") => (Some(Wire::Anthropic), Kind::Count, body_model),
        ("GET", "/v1/models") => {
            let wire = if headers.contains_key("anthropic-version") {
                Wire::Anthropic
            } else {
                Wire::Chat
            };
            (Some(wire), Kind::Models, String::new())
        }
        ("GET", "/v1beta/models") => (Some(Wire::Gemini), Kind::Models, String::new()),
        // Gemini API and Vertex AI: `…/models/{model}:{action}`. The action
        // decides the wire: Vertex serves Claude through `rawPredict`.
        ("POST", google) if google.contains("/models/") && google.contains(':') => {
            let tail = google.rsplit('/').next().unwrap_or("");
            let (model, action) = tail.rsplit_once(':').unwrap_or((tail, ""));
            let (wire, kind) = match action {
                "generateContent" => (Wire::Gemini, Kind::Generate { stream: false }),
                "streamGenerateContent" => (Wire::Gemini, Kind::Generate { stream: true }),
                "countTokens" => (Wire::Gemini, Kind::Count),
                "rawPredict" if model == "count-tokens" => (Wire::Anthropic, Kind::Count),
                "rawPredict" => (Wire::Anthropic, Kind::Generate { stream: false }),
                "streamRawPredict" => (Wire::Anthropic, Kind::Generate { stream: true }),
                _ => (Wire::Gemini, Kind::Raw),
            };
            (Some(wire), kind, model.to_string())
        }
        _ => (None, Kind::Raw, body_model),
    }
}

async fn dispatch(
    State(state): State<Arc<FakeState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    let path = uri.path().to_string();
    if path == "/token" {
        // Google's OAuth token endpoint, for service-account credentials.
        state.tokens_minted.fetch_add(1, Ordering::SeqCst);
        return json_response(json!({
            "access_token": TOKEN, "expires_in": 3600, "token_type": "Bearer"
        }));
    }
    let key = api_key(&headers);
    let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    let (wire, kind, model) = classify(&method, &path, &headers, &body);
    state.requests.lock().unwrap().push(Recorded {
        method,
        path,
        query: uri.query().map(str::to_string),
        headers,
        body: body.clone(),
        raw,
        key: key.clone(),
        wire,
        kind,
        model: model.clone(),
    });

    let mut behaviour = state.next(&key);
    while let Behaviour::Slow { delay, then } = behaviour {
        tokio::time::sleep(delay).await;
        behaviour = *then;
    }
    let wire_or_openai = wire.unwrap_or(Wire::Chat);
    if let Behaviour::HttpError {
        status,
        message,
        retry_after,
    } = &behaviour
    {
        return http_error(wire_or_openai, *status, message, *retry_after);
    }

    match (wire, kind) {
        (Some(wire), Kind::Generate { stream }) => {
            let include_usage = body
                .pointer("/stream_options/include_usage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            generate(state, wire, &model, stream, include_usage, behaviour)
        }
        (Some(wire), Kind::Count) => {
            let tokens = state.count_tokens.load(Ordering::SeqCst);
            json_response(match wire {
                Wire::Anthropic => json!({"input_tokens": tokens}),
                Wire::Gemini => json!({"totalTokens": tokens}),
                _ => json!({"object": "response.input_tokens", "input_tokens": tokens}),
            })
        }
        (Some(wire), Kind::Models) => {
            let ids = state.models.lock().unwrap().clone();
            json_response(model_listing(wire, &ids))
        }
        _ => json_response(json!({
            "object": "list",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.25, -0.5, 1.0]}],
            "model": model,
            "usage": {"prompt_tokens": 5, "total_tokens": 5}
        })),
    }
}

fn json_response(value: Value) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (
                header::HeaderName::from_static("x-request-id"),
                "req_fake_1",
            ),
            (
                header::HeaderName::from_static("x-ratelimit-remaining-requests"),
                "99",
            ),
        ],
        value.to_string(),
    )
        .into_response()
}

fn http_error(wire: Wire, status: u16, message: &str, retry_after: Option<u64>) -> Response {
    let mut response = (
        StatusCode::from_u16(status).unwrap(),
        [(header::CONTENT_TYPE, "application/json")],
        error_body(wire, status, message).to_string(),
    )
        .into_response();
    if let Some(secs) = retry_after {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, secs.to_string().parse().unwrap());
    }
    response
}

/// The vendor's error envelope for an HTTP error.
pub fn error_body(wire: Wire, status: u16, message: &str) -> Value {
    match wire {
        Wire::Chat | Wire::Responses => {
            let (kind, code) = match status {
                400 => ("invalid_request_error", Value::Null),
                401 => ("invalid_request_error", json!("invalid_api_key")),
                404 => ("invalid_request_error", json!("model_not_found")),
                429 => ("rate_limit_error", json!("rate_limit_exceeded")),
                _ => ("server_error", Value::Null),
            };
            json!({"error": {"message": message, "type": kind, "param": null, "code": code}})
        }
        Wire::Anthropic => {
            let kind = match status {
                400 => "invalid_request_error",
                401 => "authentication_error",
                403 => "permission_error",
                404 => "not_found_error",
                429 => "rate_limit_error",
                529 => "overloaded_error",
                _ => "api_error",
            };
            json!({"type": "error", "error": {"type": kind, "message": message}})
        }
        Wire::Gemini => {
            let state = match status {
                400 => "INVALID_ARGUMENT",
                401 => "UNAUTHENTICATED",
                403 => "PERMISSION_DENIED",
                404 => "NOT_FOUND",
                429 => "RESOURCE_EXHAUSTED",
                503 => "UNAVAILABLE",
                _ => "INTERNAL",
            };
            json!({"error": {"code": status, "message": message, "status": state}})
        }
    }
}

fn model_listing(wire: Wire, ids: &[String]) -> Value {
    match wire {
        Wire::Anthropic => json!({
            "data": ids.iter().map(|id| json!({
                "type": "model", "id": id, "display_name": id.to_uppercase(),
                "created_at": "2025-01-01T00:00:00Z"
            })).collect::<Vec<_>>(),
            "has_more": false,
            "first_id": ids.first(),
            "last_id": ids.last()
        }),
        Wire::Gemini => json!({
            "models": ids.iter().map(|id| json!({
                "name": format!("models/{id}"), "displayName": id.to_uppercase(),
                "inputTokenLimit": 1000, "outputTokenLimit": 100,
                "supportedGenerationMethods": ["generateContent", "countTokens"]
            })).collect::<Vec<_>>()
        }),
        _ => json!({
            "object": "list",
            "data": ids.iter().map(|id| json!({
                "id": id, "object": "model", "created": 1_700_000_000, "owned_by": "fake"
            })).collect::<Vec<_>>()
        }),
    }
}

fn generate(
    state: Arc<FakeState>,
    wire: Wire,
    model: &str,
    stream: bool,
    include_usage: bool,
    behaviour: Behaviour,
) -> Response {
    match behaviour {
        Behaviour::Reply(answer) if stream => sse(
            state,
            frames(wire, model, &answer, include_usage),
            Delivery::All,
        ),
        Behaviour::Reply(answer) => json_response(complete(wire, model, &answer)),
        Behaviour::StreamError { status, message } if stream => sse(
            state,
            vec![error_frame(wire, status, &message)],
            Delivery::All,
        ),
        Behaviour::StreamError { status, message } => http_error(wire, status, &message, None),
        Behaviour::DieBeforeFirstEvent => sse(state, Vec::new(), Delivery::Die(0)),
        Behaviour::DieAfter { frames: n, answer } => sse(
            state,
            frames(wire, model, &answer, include_usage),
            Delivery::Die(n),
        ),
        Behaviour::EndAfter { frames: n, answer } => sse(
            state,
            frames(wire, model, &answer, include_usage)
                .into_iter()
                .take(n)
                .collect(),
            Delivery::All,
        ),
        Behaviour::EmptyStream => sse(state, Vec::new(), Delivery::All),
        Behaviour::Silence => sse(state, Vec::new(), Delivery::Hang(0)),
        Behaviour::SilenceAfter { frames: n, answer } => sse(
            state,
            frames(wire, model, &answer, include_usage),
            Delivery::Hang(n),
        ),
        Behaviour::Garbage => (
            [(header::CONTENT_TYPE, "text/plain")],
            "this is definitely not json",
        )
            .into_response(),
        // Unwrapped by the caller.
        Behaviour::HttpError { .. } | Behaviour::Slow { .. } => {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// How the frames of a stream are delivered.
#[derive(Clone, Copy)]
enum Delivery {
    /// All of them, then a clean end.
    All,
    /// The first `n`, then the connection breaks.
    Die(usize),
    /// The first `n`, then silence until the client gives up.
    Hang(usize),
}

fn sse(state: Arc<FakeState>, frames: Vec<String>, delivery: Delivery) -> Response {
    let (tx, mut rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    tokio::spawn(async move {
        let limit = match delivery {
            Delivery::All => frames.len(),
            Delivery::Die(n) | Delivery::Hang(n) => n.min(frames.len()),
        };
        for frame in frames.into_iter().take(limit) {
            if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                state.disconnected();
                return;
            }
            // Each frame leaves as its own chunk.
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        match delivery {
            Delivery::All => {}
            Delivery::Die(_) => {
                // Let what was sent reach the client first.
                tokio::time::sleep(Duration::from_millis(40)).await;
                let _ = tx
                    .send(Err(std::io::Error::other("the fake upstream died")))
                    .await;
            }
            Delivery::Hang(_) => {
                tx.closed().await;
                state.disconnected();
            }
        }
    });
    let stream = futures::stream::poll_fn(move |cx| rx.poll_recv(cx));
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header("x-request-id", "req_fake_stream_1")
        .header("x-ratelimit-remaining-requests", "98")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn websocket(
    State(state): State<Arc<FakeState>>,
    upgrade: WebSocketUpgrade,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let key = api_key(&headers);
    let model = uri
        .query()
        .and_then(|q| {
            q.split('&')
                .find_map(|pair| pair.strip_prefix("model=").map(str::to_string))
        })
        .unwrap_or_default();
    state.requests.lock().unwrap().push(Recorded {
        method: Method::GET,
        path: uri.path().to_string(),
        query: uri.query().map(str::to_string),
        headers,
        body: Value::Null,
        raw: Bytes::new(),
        key: key.clone(),
        wire: None,
        kind: Kind::WebSocket,
        model: model.clone(),
    });
    if let Behaviour::HttpError {
        status,
        message,
        retry_after,
    } = state.next(&key)
    {
        return http_error(Wire::Responses, status, &message, retry_after);
    }
    upgrade.on_upgrade(move |mut socket| async move {
        let hello = json!({"type": "session.created", "model": model}).to_string();
        if socket.send(Message::Text(hello.into())).await.is_err() {
            return;
        }
        while let Some(Ok(message)) = socket.recv().await {
            match message {
                Message::Text(text) => {
                    let echo = format!("echo:{}", text.as_str());
                    if socket.send(Message::Text(echo.into())).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

const CREATED: i64 = 1_700_000_000;

fn finish(answer: &Answer) -> (&'static str, &'static str, &'static str) {
    // (chat, anthropic, gemini)
    if answer.tool.is_some() {
        ("tool_calls", "tool_use", "STOP")
    } else {
        ("stop", "end_turn", "STOP")
    }
}

fn halves(text: &str) -> (String, String) {
    let middle = text.chars().count() / 2;
    (
        text.chars().take(middle).collect(),
        text.chars().skip(middle).collect(),
    )
}

/// A complete (non-streamed) response.
pub fn complete(wire: Wire, model: &str, answer: &Answer) -> Value {
    let total = answer.input_tokens + answer.output_tokens;
    match wire {
        Wire::Chat => {
            let mut message = json!({"role": "assistant", "content": answer.text});
            if let Some((reasoning, _)) = &answer.reasoning {
                message["reasoning_content"] = json!(reasoning);
            }
            if let Some((name, args)) = &answer.tool {
                message["tool_calls"] = json!([{
                    "id": "call_fake_1", "type": "function",
                    "function": {"name": name, "arguments": args.to_string()}
                }]);
            }
            json!({
                "id": "chatcmpl-fake1", "object": "chat.completion", "created": CREATED,
                "model": model,
                "choices": [{"index": 0, "message": message, "finish_reason": finish(answer).0}],
                "usage": {
                    "prompt_tokens": answer.input_tokens,
                    "completion_tokens": answer.output_tokens,
                    "total_tokens": total
                }
            })
        }
        Wire::Responses => json!({
            "id": "resp_fake1", "object": "response", "created_at": CREATED,
            "status": "completed", "model": model,
            "output": responses_items(answer),
            "usage": {
                "input_tokens": answer.input_tokens,
                "output_tokens": answer.output_tokens,
                "total_tokens": total
            }
        }),
        Wire::Anthropic => {
            let mut content = Vec::new();
            if let Some((reasoning, signature)) = &answer.reasoning {
                content.push(
                    json!({"type": "thinking", "thinking": reasoning, "signature": signature}),
                );
            }
            if let Some(text) = &answer.text {
                content.push(json!({"type": "text", "text": text}));
            }
            if let Some((name, args)) = &answer.tool {
                content.push(
                    json!({"type": "tool_use", "id": "toolu_fake_1", "name": name, "input": args}),
                );
            }
            json!({
                "id": "msg_fake1", "type": "message", "role": "assistant", "model": model,
                "content": content,
                "stop_reason": finish(answer).1, "stop_sequence": null,
                "usage": {
                    "input_tokens": answer.input_tokens,
                    "output_tokens": answer.output_tokens
                }
            })
        }
        Wire::Gemini => json!({
            "candidates": [{
                "content": {"role": "model", "parts": gemini_parts(answer)},
                "finishReason": finish(answer).2, "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": answer.input_tokens,
                "candidatesTokenCount": answer.output_tokens,
                "totalTokenCount": total
            },
            "modelVersion": model,
            "responseId": "fake-response-1"
        }),
    }
}

fn responses_items(answer: &Answer) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some((reasoning, signature)) = &answer.reasoning {
        items.push(json!({
            "type": "reasoning", "id": "rs_fake1",
            "summary": [{"type": "summary_text", "text": reasoning}],
            "encrypted_content": signature
        }));
    }
    if let Some(text) = &answer.text {
        items.push(json!({
            "type": "message", "id": "msg_fake1", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        }));
    }
    if let Some((name, args)) = &answer.tool {
        items.push(json!({
            "type": "function_call", "id": "fc_fake1", "call_id": "call_fake_1",
            "name": name, "arguments": args.to_string(), "status": "completed"
        }));
    }
    items
}

fn gemini_parts(answer: &Answer) -> Vec<Value> {
    let mut parts = Vec::new();
    if let Some((reasoning, _)) = &answer.reasoning {
        parts.push(json!({"text": reasoning, "thought": true}));
    }
    if let Some(text) = &answer.text {
        parts.push(json!({"text": text}));
    }
    if let Some((name, args)) = &answer.tool {
        let mut part = json!({"functionCall": {"name": name, "args": args}});
        if let Some((_, signature)) = &answer.reasoning {
            part["thoughtSignature"] = json!(signature);
        }
        parts.push(part);
    }
    parts
}

fn frame(event: Option<&str>, data: &Value) -> String {
    match event {
        Some(event) => format!("event: {event}\ndata: {data}\n\n"),
        None => format!("data: {data}\n\n"),
    }
}

/// The wire events of a streamed response, one string per SSE event.
pub fn frames(wire: Wire, model: &str, answer: &Answer, include_usage: bool) -> Vec<String> {
    let total = answer.input_tokens + answer.output_tokens;
    let mut out = Vec::new();
    match wire {
        Wire::Chat => {
            let chunk = |delta: Value, finish: Value| {
                json!({
                    "id": "chatcmpl-fake1", "object": "chat.completion.chunk",
                    "created": CREATED, "model": model,
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
                })
            };
            out.push(frame(
                None,
                &chunk(json!({"role": "assistant", "content": ""}), Value::Null),
            ));
            if let Some((reasoning, _)) = &answer.reasoning {
                out.push(frame(
                    None,
                    &chunk(json!({"reasoning_content": reasoning}), Value::Null),
                ));
            }
            if let Some(text) = &answer.text {
                let (a, b) = halves(text);
                out.push(frame(None, &chunk(json!({"content": a}), Value::Null)));
                out.push(frame(None, &chunk(json!({"content": b}), Value::Null)));
            }
            if let Some((name, args)) = &answer.tool {
                let (a, b) = halves(&args.to_string());
                out.push(frame(
                    None,
                    &chunk(
                        json!({"tool_calls": [{
                            "index": 0, "id": "call_fake_1", "type": "function",
                            "function": {"name": name, "arguments": ""}
                        }]}),
                        Value::Null,
                    ),
                ));
                for fragment in [a, b] {
                    out.push(frame(
                        None,
                        &chunk(
                            json!({"tool_calls": [{"index": 0, "function": {"arguments": fragment}}]}),
                            Value::Null,
                        ),
                    ));
                }
            }
            out.push(frame(None, &chunk(json!({}), json!(finish(answer).0))));
            if include_usage {
                out.push(frame(
                    None,
                    &json!({
                        "id": "chatcmpl-fake1", "object": "chat.completion.chunk",
                        "created": CREATED, "model": model, "choices": [],
                        "usage": {
                            "prompt_tokens": answer.input_tokens,
                            "completion_tokens": answer.output_tokens,
                            "total_tokens": total
                        }
                    }),
                ));
            }
            out.push("data: [DONE]\n\n".to_string());
        }
        Wire::Responses => {
            let mut sequence = 0u64;
            let mut push = |out: &mut Vec<String>, kind: &str, mut data: Value| {
                data["type"] = json!(kind);
                data["sequence_number"] = json!(sequence);
                sequence += 1;
                out.push(frame(Some(kind), &data));
            };
            let shell = |status: &str, output: Vec<Value>, usage: Value| {
                json!({
                    "id": "resp_fake1", "object": "response", "created_at": CREATED,
                    "status": status, "model": model, "output": output, "usage": usage
                })
            };
            push(
                &mut out,
                "response.created",
                json!({"response": shell("in_progress", Vec::new(), Value::Null)}),
            );
            let items = responses_items(answer);
            for (index, item) in items.iter().enumerate() {
                let id = item["id"].as_str().unwrap_or("").to_string();
                match item["type"].as_str().unwrap_or("") {
                    "reasoning" => {
                        push(
                            &mut out,
                            "response.output_item.added",
                            json!({"output_index": index, "item": {
                                "type": "reasoning", "id": id, "summary": []
                            }}),
                        );
                        let text = item["summary"][0]["text"].clone();
                        push(
                            &mut out,
                            "response.reasoning_summary_part.added",
                            json!({"item_id": id, "output_index": index, "summary_index": 0,
                                   "part": {"type": "summary_text", "text": ""}}),
                        );
                        push(
                            &mut out,
                            "response.reasoning_summary_text.delta",
                            json!({"item_id": id, "output_index": index, "summary_index": 0,
                                   "delta": text}),
                        );
                        push(
                            &mut out,
                            "response.reasoning_summary_text.done",
                            json!({"item_id": id, "output_index": index, "summary_index": 0,
                                   "text": text}),
                        );
                        push(
                            &mut out,
                            "response.reasoning_summary_part.done",
                            json!({"item_id": id, "output_index": index, "summary_index": 0,
                                   "part": {"type": "summary_text", "text": text}}),
                        );
                    }
                    "message" => {
                        push(
                            &mut out,
                            "response.output_item.added",
                            json!({"output_index": index, "item": {
                                "type": "message", "id": id, "role": "assistant",
                                "status": "in_progress", "content": []
                            }}),
                        );
                        push(
                            &mut out,
                            "response.content_part.added",
                            json!({"item_id": id, "output_index": index, "content_index": 0,
                                   "part": {"type": "output_text", "text": "", "annotations": []}}),
                        );
                        let text = answer.text.clone().unwrap_or_default();
                        let (a, b) = halves(&text);
                        for delta in [a, b] {
                            push(
                                &mut out,
                                "response.output_text.delta",
                                json!({"item_id": id, "output_index": index, "content_index": 0,
                                       "delta": delta}),
                            );
                        }
                        push(
                            &mut out,
                            "response.output_text.done",
                            json!({"item_id": id, "output_index": index, "content_index": 0,
                                   "text": text}),
                        );
                        push(
                            &mut out,
                            "response.content_part.done",
                            json!({"item_id": id, "output_index": index, "content_index": 0,
                                   "part": {"type": "output_text", "text": text, "annotations": []}}),
                        );
                    }
                    _ => {
                        let arguments = item["arguments"].as_str().unwrap_or("").to_string();
                        push(
                            &mut out,
                            "response.output_item.added",
                            json!({"output_index": index, "item": {
                                "type": "function_call", "id": id,
                                "call_id": item["call_id"], "name": item["name"],
                                "arguments": "", "status": "in_progress"
                            }}),
                        );
                        let (a, b) = halves(&arguments);
                        for delta in [a, b] {
                            push(
                                &mut out,
                                "response.function_call_arguments.delta",
                                json!({"item_id": id, "output_index": index, "delta": delta}),
                            );
                        }
                        push(
                            &mut out,
                            "response.function_call_arguments.done",
                            json!({"item_id": id, "output_index": index, "arguments": arguments}),
                        );
                    }
                }
                push(
                    &mut out,
                    "response.output_item.done",
                    json!({"output_index": index, "item": item}),
                );
            }
            push(
                &mut out,
                "response.completed",
                json!({"response": shell("completed", items, json!({
                    "input_tokens": answer.input_tokens,
                    "output_tokens": answer.output_tokens,
                    "total_tokens": total
                }))}),
            );
        }
        Wire::Anthropic => {
            out.push(frame(
                Some("message_start"),
                &json!({"type": "message_start", "message": {
                    "id": "msg_fake1", "type": "message", "role": "assistant", "model": model,
                    "content": [], "stop_reason": null, "stop_sequence": null,
                    "usage": {"input_tokens": answer.input_tokens, "output_tokens": 1}
                }}),
            ));
            let mut index = 0;
            let mut block = |out: &mut Vec<String>, start: Value, deltas: Vec<Value>| {
                out.push(frame(
                    Some("content_block_start"),
                    &json!({"type": "content_block_start", "index": index, "content_block": start}),
                ));
                if index == 0 {
                    out.push(frame(Some("ping"), &json!({"type": "ping"})));
                }
                for delta in deltas {
                    out.push(frame(
                        Some("content_block_delta"),
                        &json!({"type": "content_block_delta", "index": index, "delta": delta}),
                    ));
                }
                out.push(frame(
                    Some("content_block_stop"),
                    &json!({"type": "content_block_stop", "index": index}),
                ));
                index += 1;
            };
            if let Some((reasoning, signature)) = &answer.reasoning {
                block(
                    &mut out,
                    json!({"type": "thinking", "thinking": "", "signature": ""}),
                    vec![
                        json!({"type": "thinking_delta", "thinking": reasoning}),
                        json!({"type": "signature_delta", "signature": signature}),
                    ],
                );
            }
            if let Some(text) = &answer.text {
                let (a, b) = halves(text);
                block(
                    &mut out,
                    json!({"type": "text", "text": ""}),
                    vec![
                        json!({"type": "text_delta", "text": a}),
                        json!({"type": "text_delta", "text": b}),
                    ],
                );
            }
            if let Some((name, args)) = &answer.tool {
                let (a, b) = halves(&args.to_string());
                block(
                    &mut out,
                    json!({"type": "tool_use", "id": "toolu_fake_1", "name": name, "input": {}}),
                    vec![
                        json!({"type": "input_json_delta", "partial_json": a}),
                        json!({"type": "input_json_delta", "partial_json": b}),
                    ],
                );
            }
            out.push(frame(
                Some("message_delta"),
                &json!({"type": "message_delta",
                        "delta": {"stop_reason": finish(answer).1, "stop_sequence": null},
                        "usage": {"output_tokens": answer.output_tokens}}),
            ));
            out.push(frame(
                Some("message_stop"),
                &json!({"type": "message_stop"}),
            ));
        }
        Wire::Gemini => {
            let chunk = |parts: Vec<Value>| {
                json!({
                    "candidates": [{"content": {"role": "model", "parts": parts}, "index": 0}],
                    "modelVersion": model, "responseId": "fake-response-1"
                })
            };
            if let Some((reasoning, _)) = &answer.reasoning {
                out.push(frame(
                    None,
                    &chunk(vec![json!({"text": reasoning, "thought": true})]),
                ));
            }
            if let Some(text) = &answer.text {
                let (a, b) = halves(text);
                out.push(frame(None, &chunk(vec![json!({"text": a})])));
                out.push(frame(None, &chunk(vec![json!({"text": b})])));
            }
            if answer.tool.is_some() {
                let call: Vec<Value> = gemini_parts(answer)
                    .into_iter()
                    .filter(|part| part.get("functionCall").is_some())
                    .collect();
                out.push(frame(None, &chunk(call)));
            }
            out.push(frame(
                None,
                &json!({
                    "candidates": [{
                        "content": {"role": "model", "parts": []},
                        "finishReason": finish(answer).2, "index": 0
                    }],
                    "usageMetadata": {
                        "promptTokenCount": answer.input_tokens,
                        "candidatesTokenCount": answer.output_tokens,
                        "totalTokenCount": total
                    },
                    "modelVersion": model, "responseId": "fake-response-1"
                }),
            ));
        }
    }
    out
}

/// The vendor's in-stream error event.
pub fn error_frame(wire: Wire, status: u16, message: &str) -> String {
    match wire {
        Wire::Chat => frame(None, &error_body(Wire::Chat, status, message)),
        Wire::Responses => frame(
            Some("error"),
            &json!({"type": "error", "code": "server_error", "message": message,
                    "param": null, "sequence_number": 0}),
        ),
        Wire::Anthropic => frame(Some("error"), &error_body(Wire::Anthropic, status, message)),
        Wire::Gemini => frame(None, &error_body(Wire::Gemini, status, message)),
    }
}
