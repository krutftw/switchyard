//! Hardening: things one client, or one careless upstream, must not be able
//! to do to everybody else.
//!
//! * Optional endpoints (raw side endpoints, token counting, upstream
//!   WebSockets) are refused for reasons that say nothing about a
//!   credential's ability to generate; such refusals must not take a model
//!   or a key out of rotation.
//! * What an upstream says about a failure reaches the client without the
//!   credential it may quote — and what a model *writes* reaches the client
//!   untouched, even when the "credential" is an ordinary word.
//! * A configuration the store refuses is announced on the event bus.

// The shared support module re-exports more than this file uses.
#[allow(unused_imports)]
mod support;

use axum::Router;
use axum::body::Body;
use axum::http::HeaderMap as AxumHeaders;
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method};
use serde_json::{Value, json};
use std::time::Duration;
use support::{Behaviour, CLIENT_KEY, FOUR_PROVIDERS, Harness, Kind, Output};
use switchyard_core::Protocol;
use switchyard_core::config::ProviderKind;
use switchyard_gateway::{ClientRequest, RawRequest, WsOpenRequest};
use switchyard_scheduler::{CredentialSnapshot, CredentialStatus};
use switchyard_telemetry::Event;
use tokio_util::sync::CancellationToken;

const ONE_KEY: &str = r#"
[[providers]]
name = "oai"
kind = "openai"
wire_api = "chat"
base_url = "{base}/v1"
api_keys = ["key-a"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#;

const TWO_KEYS: &str = r#"
[routing]
strategy = "fill-first"

[[providers]]
name = "oai"
kind = "openai"
wire_api = "chat"
base_url = "{base}/v1"
api_keys = ["key-a", "key-b"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#;

fn credentials(harness: &Harness) -> Vec<CredentialSnapshot> {
    harness.gateway.scheduler().snapshot()[0]
        .credentials
        .clone()
}

fn assert_all_ready(harness: &Harness, what: &str) {
    for credential in credentials(harness) {
        assert_eq!(
            credential.status,
            CredentialStatus::Ready,
            "{what} rested a credential: {credential:?}"
        );
        assert!(
            credential.model_cooldowns.is_empty(),
            "{what} rested a model: {:?}",
            credential.model_cooldowns
        );
        assert_eq!(
            credential.failures, 0,
            "{what} was held against a credential"
        );
    }
}

fn raw_request(harness: &Harness, path: &str, model: &str) -> RawRequest {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {CLIENT_KEY}")).unwrap(),
    );
    RawRequest {
        path: path.to_string(),
        method: Method::POST,
        body: Bytes::from(json!({"input": "hello", "model": model}).to_string()),
        content_type: Some("application/json".to_string()),
        query: None,
        model: model.to_string(),
        headers,
        identity: harness.identity(),
        client_ip: None,
        endpoint: format!("POST /v1/{path}"),
        cancel: CancellationToken::new(),
    }
}

fn ws_request(harness: &Harness, model: &str) -> WsOpenRequest {
    WsOpenRequest {
        identity: harness.identity(),
        model: model.to_string(),
        path_and_query: "realtime?model={model}".to_string(),
        headers: HeaderMap::new(),
        endpoint: "GET /v1/realtime".to_string(),
        client_ip: None,
        require_kind: Some(ProviderKind::Openai),
    }
}

// ---------------------------------------------------------------------------
// Optional endpoints
// ---------------------------------------------------------------------------

/// A key that may generate but not embed (a restricted key), and an
/// embeddings request that names a chat model: the upstream refuses with
/// the very statuses it uses for a revoked key or a missing model. Any
/// authenticated client can provoke both.
#[tokio::test]
async fn a_side_endpoint_refusal_is_not_held_against_the_credential() {
    for (status, message, client_status) in [
        (
            401,
            "You have insufficient permissions for this operation. Missing scopes: model.request",
            502,
        ),
        (
            403,
            "You are not allowed to generate embeddings from this model",
            502,
        ),
        // The fake's 404 carries `code: model_not_found`, as OpenAI's does.
        (
            404,
            "The model `up-chat` does not exist or you do not have access to it.",
            404,
        ),
        // Not an error the gateway has a better word for: passed on as it is.
        (405, "Method Not Allowed", 405),
    ] {
        let harness = Harness::start(TWO_KEYS).await;
        for key in ["key-a", "key-b"] {
            harness
                .fake
                .script(key, [Behaviour::error(status, message)]);
        }
        let raw = Output::read(
            harness
                .gateway
                .raw(raw_request(&harness, "embeddings", "m"))
                .await,
        )
        .await;
        assert_eq!(raw.status, client_status, "upstream {status}");
        assert_eq!(
            harness.fake.keys(),
            vec!["key-a", "key-b"],
            "upstream {status}: the next credential gets its chance"
        );
        assert_all_ready(&harness, &format!("a {status} from a side endpoint"));

        // The attempts are on the record all the same.
        let record = harness.record(&raw.request_id);
        assert_eq!(record.attempts.len(), 2);
        assert_eq!(record.attempts[0].status, status);
        assert!(!record.ok);

        let chat = harness.ask(Protocol::OpenaiChat, "m", false).await;
        assert_eq!(chat.status, 200, "{}", String::from_utf8_lossy(&chat.body));
    }
}

/// A rate limit or a server fault is the upstream's state, whatever the
/// endpoint: those are reported as usual.
#[tokio::test]
async fn a_side_endpoint_rate_limit_or_outage_is_reported() {
    let harness = Harness::start(TWO_KEYS).await;
    harness.fake.script("key-a", [Behaviour::rate_limited(30)]);
    harness
        .fake
        .script("key-b", [Behaviour::error(500, "the upstream exploded")]);
    let raw = Output::read(
        harness
            .gateway
            .raw(raw_request(&harness, "embeddings", "m"))
            .await,
    )
    .await;
    assert_eq!(raw.status, 500);
    for credential in credentials(&harness) {
        assert_eq!(credential.failures, 1, "{credential:?}");
        assert_eq!(credential.model_cooldowns.len(), 1, "{credential:?}");
    }
}

/// The counting endpoint is optional too: a key that may not count (or an
/// upstream that guards the endpoint separately) yields an estimate, and
/// the key keeps serving generation.
#[tokio::test]
async fn a_refused_count_is_estimated_and_blames_nobody() {
    for status in [401, 403, 404] {
        let harness = Harness::start(FOUR_PROVIDERS).await;
        harness.fake.script(
            "key-anthropic-1",
            [Behaviour::error(
                status,
                "count_tokens is not available to this key",
            )],
        );
        let request = ClientRequest::new(
            Protocol::Anthropic,
            "POST /v1/messages/count_tokens",
            Bytes::from(
                json!({
                    "model": "m-anthropic",
                    "messages": [{"role": "user", "content": "How many tokens is this?"}]
                })
                .to_string(),
            ),
            harness.identity(),
        );
        let output = Output::read(harness.gateway.count_tokens(request).await).await;
        assert_eq!(output.status, 200, "upstream {status}: {:?}", output.body);
        let tokens = output.json()["input_tokens"].as_u64().unwrap();
        assert!(
            tokens > 0 && tokens != 42,
            "an estimate, not the fake's count"
        );
        assert_eq!(harness.fake.last().kind, Kind::Count);

        let anthropic = &harness.gateway.scheduler().snapshot()[2].credentials[0];
        assert_eq!(
            anthropic.status,
            CredentialStatus::Ready,
            "upstream {status}"
        );
        assert!(anthropic.model_cooldowns.is_empty());
        assert_eq!(anthropic.failures, 0);
        // The request succeeded; the refused upstream call is on its record.
        let record = harness.record(&output.request_id);
        assert!(record.ok);
        assert_eq!(record.provider.as_deref(), Some("anthropic"));
        assert_eq!(record.attempts.len(), 1);
        assert_eq!(record.attempts[0].status, status);
        assert!(!record.attempts[0].ok);
    }
}

/// A counting call that fails because the upstream is in trouble is still a
/// failed attempt.
#[tokio::test]
async fn a_count_that_hits_a_rate_limit_is_reported() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    harness
        .fake
        .script("key-anthropic-1", [Behaviour::rate_limited(30)]);
    let request = ClientRequest::new(
        Protocol::Anthropic,
        "POST /v1/messages/count_tokens",
        Bytes::from(
            json!({"model": "m-anthropic", "messages": [{"role": "user", "content": "hi"}]})
                .to_string(),
        ),
        harness.identity(),
    );
    let output = Output::read(harness.gateway.count_tokens(request).await).await;
    assert_eq!(output.status, 429);
    assert_eq!(output.header("retry-after"), Some("30"));
    let anthropic = &harness.gateway.scheduler().snapshot()[2].credentials[0];
    assert_eq!(anthropic.model_cooldowns.len(), 1);
}

/// A handshake refused with a status that, for an HTTP call, would mean a
/// rejected key: over WebSocket it usually means "not for this key" or "not
/// here", and the HTTP fallback must keep working.
#[tokio::test]
async fn a_refused_websocket_handshake_is_not_held_against_the_credential() {
    for status in [401, 403, 404] {
        let harness = Harness::start(TWO_KEYS).await;
        for key in ["key-a", "key-b"] {
            harness
                .fake
                .script(key, [Behaviour::error(status, "realtime is not enabled")]);
        }
        let error = harness
            .gateway
            .open_upstream_ws(ws_request(&harness, "m"))
            .await
            .expect_err("no credential connects");
        assert!(error.status >= 400, "{error:?}");
        assert_eq!(harness.fake.count(), 2, "both credentials were tried");
        assert_all_ready(&harness, &format!("a {status} on the handshake"));
        let chat = harness.ask(Protocol::OpenaiChat, "m", false).await;
        assert_eq!(chat.status, 200, "{}", String::from_utf8_lossy(&chat.body));
    }
}

/// An upstream that answers the upgrade request like any other request —
/// with a plain HTTP response — does not speak WebSocket there. The
/// transport reports that as a failure of the connection; it is a failure
/// of the WebSocket route, not of the credential.
#[tokio::test]
async fn an_upstream_that_does_not_upgrade_is_not_held_against_the_credential() {
    async fn plain_http() -> Response {
        Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "id": "chatcmpl-1", "object": "chat.completion", "created": 1,
                    "model": "up-chat",
                    "choices": [{"index": 0,
                                 "message": {"role": "assistant", "content": "ok"},
                                 "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })
                .to_string(),
            ))
            .unwrap()
    }
    let base = serve(Router::new().fallback(plain_http)).await;
    let harness = Harness::start(&ONE_KEY.replace("{base}", &base)).await;
    let error = harness
        .gateway
        .open_upstream_ws(ws_request(&harness, "m"))
        .await
        .expect_err("the upstream does not upgrade");
    assert_eq!(error.status, 502, "{error:?}");
    assert_all_ready(&harness, "an upstream that does not upgrade");
    let page = harness
        .gateway
        .telemetry()
        .usage()
        .requests(&switchyard_telemetry::RequestQuery::default());
    let record = page.items.first().expect("the attempt is recorded");
    assert_eq!(record.attempts.len(), 1);
    assert!(!record.ok);
    let chat = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(chat.status, 200, "{}", String::from_utf8_lossy(&chat.body));
}

/// The client's `OpenAI-Organization` / `OpenAI-Project` never reach an
/// upstream: not on generation, not on a raw call, not on a handshake.
#[tokio::test]
async fn the_clients_vendor_account_headers_stay_with_the_gateway() {
    let harness = Harness::start(ONE_KEY).await;
    let tagged = |headers: &mut HeaderMap| {
        headers.insert(
            "openai-organization",
            HeaderValue::from_static("org-client"),
        );
        headers.insert("openai-project", HeaderValue::from_static("proj_client"));
        headers.insert("openai-beta", HeaderValue::from_static("assistants=v2"));
    };

    let mut request = harness.request(Protocol::OpenaiChat, "m", false);
    tagged(&mut request.headers);
    assert_eq!(harness.gateway.generate(request).await.status(), 200);

    let mut raw = raw_request(&harness, "embeddings", "m");
    tagged(&mut raw.headers);
    assert_eq!(harness.gateway.raw(raw).await.status(), 200);

    let mut ws = ws_request(&harness, "m");
    tagged(&mut ws.headers);
    let session = harness.gateway.open_upstream_ws(ws).await.unwrap();
    session.finish(switchyard_gateway::WsOutcome::closed());

    let seen = harness.fake.requests();
    assert_eq!(seen.len(), 3);
    for recorded in seen {
        assert_eq!(
            recorded.header("openai-organization"),
            None,
            "{:?}",
            recorded.kind
        );
        assert_eq!(
            recorded.header("openai-project"),
            None,
            "{:?}",
            recorded.kind
        );
        // Protocol switches are the client's to choose.
        assert_eq!(recorded.header("openai-beta"), Some("assistants=v2"));
    }
}

// ---------------------------------------------------------------------------
// Secrets in streams
// ---------------------------------------------------------------------------

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn sse(frames: Vec<String>) -> Response {
    let stream = futures::stream::iter(
        frames
            .into_iter()
            .map(|frame| Ok::<_, std::io::Error>(Bytes::from(frame))),
    );
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn bearer(headers: &AxumHeaders) -> String {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string()
}

fn chat_chunk(delta: Value, finish: Value) -> String {
    let chunk = json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "up",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
    });
    format!("data: {chunk}\n\n")
}

/// A Chat Completions upstream whose stream starts and then fails with an
/// error chunk that quotes the key it was called with.
async fn leaky_chat(headers: AxumHeaders) -> Response {
    let key = bearer(&headers);
    let error = json!({"error": {
        "message": format!("worker crashed while serving key {key}"),
        "type": "server_error", "param": null, "code": null
    }});
    sse(vec![
        chat_chunk(json!({"role": "assistant", "content": ""}), Value::Null),
        chat_chunk(json!({"content": "Hel"}), Value::Null),
        format!("data: {error}\n\n"),
    ])
}

/// The same from a Responses upstream: an `error` event and a
/// `response.failed` event, each quoting the key.
async fn leaky_responses(headers: AxumHeaders) -> Response {
    let key = bearer(&headers);
    let response = |status: &str| {
        json!({"id": "resp_1", "object": "response", "created_at": 1, "status": status,
               "model": "up", "output": []})
    };
    let frame = |event: &str, data: Value| format!("event: {event}\ndata: {data}\n\n");
    let mut failed = response("failed");
    failed["error"] = json!({"code": "server_error",
                             "message": format!("worker crashed while serving key {key}")});
    sse(vec![
        frame(
            "response.created",
            json!({"type": "response.created", "sequence_number": 0,
                   "response": response("in_progress")}),
        ),
        frame(
            "response.output_item.added",
            json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0,
                   "item": {"id": "msg_1", "type": "message", "role": "assistant",
                            "status": "in_progress", "content": []}}),
        ),
        frame(
            "response.failed",
            json!({"type": "response.failed", "sequence_number": 2, "response": failed}),
        ),
    ])
}

const SECRET: &str = "sk-proj-supersecretkeymaterial0123456789";

async fn leak_harness(app: Router, wire_api: &str) -> Harness {
    let base = serve(app).await;
    Harness::start(&format!(
        r#"
[[providers]]
name = "oai"
kind = "openai"
wire_api = "{wire_api}"
base_url = "{base}/v1"
api_keys = ["{SECRET}"]
[[providers.models]]
id = "up"
alias = "m"
"#
    ))
    .await
}

#[tokio::test]
async fn in_stream_errors_of_openai_upstreams_reach_no_client_with_the_key() {
    for (wire_api, upstream) in [
        ("chat", Protocol::OpenaiChat),
        ("responses", Protocol::OpenaiResponses),
    ] {
        for client in support::PROTOCOLS {
            let app = match upstream {
                Protocol::OpenaiChat => Router::new().fallback(leaky_chat),
                _ => Router::new().fallback(leaky_responses),
            };
            let harness = leak_harness(app, wire_api).await;
            let output = harness.ask(client, "m", true).await;
            // Whether the failure arrives in-band (the stream was already
            // committed) or as an error reply (nothing had been sent yet)
            // depends on the protocol pair; the key must be in neither.
            if client == upstream {
                assert!(output.streamed, "{client}: forwarded from the first event");
            }
            let text = if output.streamed {
                output.wire_text()
            } else {
                String::from_utf8_lossy(&output.body).into_owned()
            };
            assert!(
                !text.contains(SECRET),
                "{client} on {upstream}: the upstream key reached the client:\n{text}"
            );
            assert!(
                text.contains("worker crashed"),
                "{client} on {upstream}: the client is told what failed:\n{text}"
            );
            let record = harness.record(&output.request_id);
            assert!(!record.ok);
            assert!(!format!("{record:?}").contains(SECRET));
        }
    }
}

/// Self-hosted servers are configured with "keys" that are ordinary words.
/// Scrubbing is for what an upstream says about a failure; what the model
/// writes is the client's, word for word.
#[tokio::test]
async fn content_that_happens_to_contain_the_key_is_not_touched() {
    async fn talkative(_headers: AxumHeaders) -> Response {
        sse(vec![
            chat_chunk(json!({"role": "assistant", "content": ""}), Value::Null),
            chat_chunk(
                json!({"content": "lm-studio is a desktop app"}),
                Value::Null,
            ),
            chat_chunk(json!({}), json!("stop")),
            "data: [DONE]\n\n".to_string(),
        ])
    }
    let base = serve(Router::new().fallback(talkative)).await;
    let harness = Harness::start(&format!(
        r#"
[[providers]]
name = "local"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["lm-studio"]
[[providers.models]]
id = "up"
alias = "m"
"#
    ))
    .await;
    for client in [Protocol::OpenaiChat, Protocol::Anthropic] {
        let output = harness.ask(client, "m", true).await;
        assert_eq!(
            output.response(client).text(),
            "lm-studio is a desktop app",
            "{client}"
        );
    }
}

/// With body capture on, the stored copy of the upstream's events is
/// scrubbed as well.
#[tokio::test]
async fn captured_upstream_events_do_not_keep_the_key() {
    let base = serve(Router::new().fallback(leaky_chat)).await;
    let harness = Harness::start(&format!(
        r#"
[logging]
request_log = "all"

[[providers]]
name = "oai"
kind = "openai"
wire_api = "chat"
base_url = "{base}/v1"
api_keys = ["{SECRET}"]
[[providers.models]]
id = "up"
alias = "m"
"#
    ))
    .await;
    let output = harness.ask(Protocol::OpenaiChat, "m", true).await;
    assert!(output.streamed);
    harness.gateway.telemetry().flush().await.unwrap();
    let bodies = harness
        .gateway
        .telemetry()
        .bodies()
        .read(&output.request_id)
        .expect("captured");
    let upstream = bodies.upstream_response.expect("the upstream events");
    assert!(upstream.contains("worker crashed"), "{upstream}");
    assert!(!upstream.contains(SECRET), "{upstream}");
    assert!(!bodies.client_response.unwrap_or_default().contains(SECRET));
}

// ---------------------------------------------------------------------------
// Records and captures of side requests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_raw_request_is_captured_with_what_was_sent_and_answered() {
    let harness = Harness::start(&format!("[logging]\nrequest_log = \"errors\"\n{ONE_KEY}")).await;
    harness
        .fake
        .script("key-a", [Behaviour::error(500, "the embedder fell over")]);
    let raw = Output::read(
        harness
            .gateway
            .raw(raw_request(&harness, "embeddings", "m"))
            .await,
    )
    .await;
    assert_eq!(raw.status, 500);
    harness.gateway.telemetry().flush().await.unwrap();
    let bodies = harness
        .gateway
        .telemetry()
        .bodies()
        .read(&raw.request_id)
        .expect("a failed request is captured in `errors` mode");
    let sent: Value =
        serde_json::from_str(&bodies.upstream_request.expect("what was sent")).unwrap();
    assert_eq!(sent["model"], "up-chat");
    assert!(
        bodies
            .upstream_response
            .expect("what the upstream answered")
            .contains("the embedder fell over")
    );
}

#[tokio::test]
async fn an_abandoned_raw_or_counting_call_names_the_credential_it_used() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let slow = || Behaviour::Slow {
        delay: Duration::from_secs(20),
        then: Box::new(Behaviour::text("never seen")),
    };

    harness.fake.script("key-chat-1", [slow()]);
    let cancel = CancellationToken::new();
    let mut request = raw_request(&harness, "embeddings", "m-chat");
    request.cancel = cancel.clone();
    let gateway = harness.gateway.clone();
    let pending = tokio::spawn(async move { gateway.raw(request).await });
    support::eventually("the raw call is in progress", || harness.fake.count() == 1).await;
    cancel.cancel();
    let output = Output::read(pending.await.unwrap()).await;
    assert_eq!(output.status, 499);
    let record = harness.record(&output.request_id);
    assert_eq!(record.provider.as_deref(), Some("chat"));
    assert_eq!(record.attempts.len(), 1);
    assert_eq!(record.attempts[0].status, 499);

    harness.fake.script("key-anthropic-1", [slow()]);
    let cancel = CancellationToken::new();
    let mut request = ClientRequest::new(
        Protocol::Anthropic,
        "POST /v1/messages/count_tokens",
        Bytes::from(
            json!({"model": "m-anthropic", "messages": [{"role": "user", "content": "hi"}]})
                .to_string(),
        ),
        harness.identity(),
    );
    request.cancel = cancel.clone();
    let gateway = harness.gateway.clone();
    let pending = tokio::spawn(async move { gateway.count_tokens(request).await });
    support::eventually("the counting call is in progress", || {
        harness.fake.count() == 2
    })
    .await;
    cancel.cancel();
    let output = Output::read(pending.await.unwrap()).await;
    assert_eq!(output.status, 499);
    let record = harness.record(&output.request_id);
    assert_eq!(record.provider.as_deref(), Some("anthropic"));
    assert_eq!(record.upstream_model.as_deref(), Some("claude-up"));
    assert_eq!(record.attempts.len(), 1);

    // Nobody was blamed.
    for provider in harness.gateway.scheduler().snapshot() {
        for credential in provider.credentials {
            assert_eq!(credential.failures, 0);
            assert_eq!(credential.status, CredentialStatus::Ready);
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_rejected_configuration_is_announced_and_changes_nothing() {
    let harness = Harness::start(ONE_KEY).await;
    let mut events = harness.gateway.telemetry().subscribe();
    let good = std::fs::read_to_string(&harness.config_path).unwrap();

    // Someone saves a broken file; the reload is refused.
    std::fs::write(
        &harness.config_path,
        format!("{good}\n[routing]\nmax_attempts = 0\n"),
    )
    .unwrap();
    harness
        .gateway
        .config_store()
        .reload_from_disk()
        .await
        .expect_err("an invalid configuration is refused");

    let announced = async {
        loop {
            match events.recv().await {
                Ok(Event::ConfigReloaded { ok, message, .. }) => return (ok, message),
                Ok(_) => {}
                Err(error) => panic!("event bus closed: {error}"),
            }
        }
    };
    let (ok, message) = tokio::time::timeout(Duration::from_secs(5), announced)
        .await
        .expect("the rejection is announced");
    assert!(!ok);
    assert!(message.contains("rejected"), "{message}");
    assert!(message.contains("routing.max_attempts"), "{message}");

    // The previous configuration is still the live one.
    assert_eq!(harness.gateway.config().routing.max_attempts, 3);
    assert_eq!(
        harness.ask(Protocol::OpenaiChat, "m", false).await.status,
        200
    );
}
