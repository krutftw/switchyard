//! The rest of the gateway's surface: authentication, model listings, token
//! counting, raw proxying, the mock provider, Vertex AI, upstream
//! WebSockets, provider tests and discovery, configuration changes, and
//! what telemetry records.

mod support;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use http::{HeaderMap, HeaderValue, Method};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{
    Answer, Behaviour, CLIENT_KEY, FOUR_PROVIDERS, Harness, Kind, Output, PROTOCOLS, Wire,
    arguments, body, tool_declaration,
};
use switchyard_core::config::ProviderKind;
use switchyard_core::ir::{FinishReason, Part};
use switchyard_core::{ApiError, ErrorKind, FailureClass, Protocol};
use switchyard_gateway::{
    ClientRequest, Gateway, GatewayOptions, PresentedCredentials, RawRequest, Reply, StartError,
    WsMessage, WsOpenRequest, WsOutcome,
};
use switchyard_scheduler::CredentialStatus;
use switchyard_telemetry::{Mode, Transport};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Start-up
// ---------------------------------------------------------------------------

#[tokio::test]
async fn start_fails_on_a_missing_or_invalid_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let missing = Gateway::start(GatewayOptions::new(dir.path().join("nope.toml"))).await;
    assert!(matches!(missing, Err(StartError::Config(_))));

    let path = dir.path().join("switchyard.toml");
    std::fs::write(&path, "[server]\nport = 0\n").unwrap();
    let error = Gateway::start(GatewayOptions::new(&path))
        .await
        .expect_err("port 0 is invalid");
    assert!(error.to_string().contains("server.port"), "{error}");
}

#[tokio::test]
async fn accessors_describe_the_running_gateway() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let gateway = &harness.gateway;
    assert_eq!(Gateway::version(), env!("CARGO_PKG_VERSION"));
    assert!(gateway.started_at() <= std::time::SystemTime::now());
    assert_eq!(gateway.config().providers.len(), 4);
    assert_eq!(
        gateway.config_store().path(),
        gateway.config_store().dir().join("switchyard.toml")
    );
    for protocol in PROTOCOLS {
        assert_eq!(gateway.codec(protocol).protocol(), protocol);
    }
    // The data directory is resolved against the configuration file's.
    assert_eq!(
        gateway.telemetry().data_dir(),
        Some(gateway.config_store().dir().join("data").as_path())
    );
    assert_eq!(gateway.scheduler().snapshot().len(), 4);
    // The handle is cheap to clone and usable across tasks.
    fn assert_send_sync<T: Send + Sync + Clone + 'static>(_: &T) {}
    assert_send_sync(gateway);
    gateway.shutdown().await;
}

// ---------------------------------------------------------------------------
// Authentication, allow-lists, rate limits
// ---------------------------------------------------------------------------

const KEYS: &str = r#"
[[auth.keys]]
key = "sy-limited-key-000001"
name = "limited"
models = ["m-chat", "m-gem*"]
rate_limit_rpm = 2

[[auth.keys]]
key = "sy-disabled-key-00001"
name = "off"
enabled = false
"#;

fn presented(
    authorization: Option<&str>,
    x_api_key: Option<&str>,
    x_goog: Option<&str>,
    query: Option<&str>,
) -> PresentedCredentials {
    PresentedCredentials {
        authorization: authorization.map(str::to_string),
        x_api_key: x_api_key.map(str::to_string),
        x_goog_api_key: x_goog.map(str::to_string),
        query_key: query.map(str::to_string),
        ws_ticket: None,
    }
}

#[tokio::test]
async fn authentication_precedence_and_errors() {
    let harness = Harness::start(&format!("{KEYS}\n{FOUR_PROVIDERS}")).await;
    let gateway = &harness.gateway;
    for credentials in [
        presented(Some(&format!("Bearer {CLIENT_KEY}")), None, None, None),
        presented(Some(CLIENT_KEY), None, None, None),
        presented(None, Some(CLIENT_KEY), None, None),
        presented(None, None, Some(CLIENT_KEY), None),
        presented(None, None, None, Some(CLIENT_KEY)),
        // A wrong Authorization next to a right x-api-key authenticates.
        presented(Some("Bearer wrong"), Some(CLIENT_KEY), None, None),
    ] {
        let identity = gateway.authenticate(&credentials).unwrap();
        assert_eq!(identity.key_name.as_deref(), Some("tester"));
        assert_eq!(
            identity.key_id,
            Some(switchyard_config_store::client_key_id(CLIENT_KEY))
        );
        assert!(!identity.anonymous && !identity.internal);
    }
    // The first matching candidate wins.
    let identity = gateway
        .authenticate(&presented(
            Some("Bearer sy-limited-key-000001"),
            Some(CLIENT_KEY),
            None,
            None,
        ))
        .unwrap();
    assert_eq!(identity.key_name.as_deref(), Some("limited"));
    assert_eq!(identity.rate_limit_rpm(), Some(2));

    let missing = gateway
        .authenticate(&PresentedCredentials::default())
        .unwrap_err();
    assert_eq!(
        (missing.status, missing.message.as_str()),
        (401, "missing API key")
    );
    let invalid = gateway
        .authenticate(&presented(None, Some("nope"), None, None))
        .unwrap_err();
    assert_eq!(
        (invalid.status, invalid.message.as_str()),
        (401, "invalid API key")
    );
    let disabled = gateway
        .authenticate(&presented(None, Some("sy-disabled-key-00001"), None, None))
        .unwrap_err();
    assert_eq!(disabled.message, "invalid API key");

    // The server renders such errors in the route's protocol.
    let reply = gateway.error_reply(Protocol::Anthropic, &missing);
    assert_eq!(reply.status, 401);
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");
    assert_eq!(
        reply.header("x-request-id"),
        Some(reply.request_id.as_str())
    );
    let reply = gateway.error_reply(
        Protocol::Gemini,
        &ApiError::rate_limit("slow down").with_retry_after(Duration::from_secs(12)),
    );
    assert_eq!(reply.header("retry-after"), Some("12"));
}

#[tokio::test]
async fn websocket_tickets_stand_for_their_key_once() {
    let harness = Harness::start(&format!("{KEYS}\n{FOUR_PROVIDERS}")).await;
    let gateway = &harness.gateway;
    let with_ticket = |ticket: &str| PresentedCredentials {
        ws_ticket: Some(ticket.to_string()),
        ..PresentedCredentials::default()
    };
    let limited = harness.identity_of("sy-limited-key-000001");
    let ticket = gateway.issue_ws_ticket(&limited).unwrap();
    assert_eq!(ticket.expires_in, 30);
    assert!(!format!("{ticket:?}").contains(&ticket.ticket));

    let identity = gateway.authenticate(&with_ticket(&ticket.ticket)).unwrap();
    assert_eq!(identity.key_name.as_deref(), Some("limited"));
    assert_eq!(identity.key_id, limited.key_id);
    assert_eq!(identity.rate_limit_rpm(), Some(2));
    assert!(identity.allows_model("m-chat") && !identity.allows_model("m-resp"));

    // Once only; and a ticket nobody minted is a wrong key.
    for ticket in [ticket.ticket.as_str(), "made-up-ticket"] {
        let refused = gateway.authenticate(&with_ticket(ticket)).unwrap_err();
        assert_eq!(
            (refused.status, refused.message.as_str()),
            (401, "invalid API key")
        );
    }

    // Minting is not a request: nothing was recorded.
    assert!(
        gateway
            .telemetry()
            .usage()
            .requests(&switchyard_telemetry::RequestQuery::default())
            .items
            .is_empty()
    );
    // The playground's identity has its own way in.
    assert!(
        gateway
            .issue_ws_ticket(&gateway.dashboard_identity())
            .is_err()
    );
}

#[tokio::test]
async fn optional_authentication_admits_anonymous_clients() {
    let harness = Harness::start_raw(&format!(
        "[upstream]\nproxy = \"direct\"\n[auth]\nrequired = false\n{FOUR_PROVIDERS}"
    ))
    .await;
    let identity = harness
        .gateway
        .authenticate(&PresentedCredentials::default())
        .unwrap();
    assert!(identity.anonymous);
    let request = ClientRequest::new(
        Protocol::OpenaiChat,
        "POST /v1/chat/completions",
        Bytes::from(body(Protocol::OpenaiChat, "m-chat", false, false).to_string()),
        identity,
    );
    let output = Output::read(harness.gateway.generate(request).await).await;
    assert_eq!(output.status, 200);
    let record = harness.record(&output.request_id);
    assert_eq!(record.client.key_id, None);
    assert_eq!(record.key_name(), "anonymous");
}

#[tokio::test]
async fn the_allow_list_limits_requests_and_listings() {
    let harness = Harness::start(&format!("{KEYS}\n{FOUR_PROVIDERS}")).await;
    let limited = harness.identity_of("sy-limited-key-000001");
    assert!(limited.allows_model("m-chat"));
    assert!(!limited.allows_model("m-anthropic"));

    // Allowed.
    let mut request = harness.request(Protocol::OpenaiChat, "m-chat", false);
    request.identity = limited.clone();
    assert_eq!(harness.gateway.generate(request).await.status(), 200);

    // Not allowed: 403 naming the model, in the client's envelope, before
    // anything is sent upstream — whether or not the model exists.
    harness.fake.clear();
    for model in ["m-anthropic", "m-anthropic(high)", "does-not-exist"] {
        let mut request = harness.request(Protocol::Anthropic, model, false);
        request.identity = limited.clone();
        let output = Output::read(harness.gateway.generate(request).await).await;
        assert_eq!(output.status, 403, "{model}");
        assert_eq!(output.json()["error"]["type"], "permission_error");
        let base = model.trim_end_matches("(high)");
        assert!(
            output.json()["error"]["message"]
                .as_str()
                .unwrap()
                .contains(base),
            "{:?}",
            output.body
        );
        let record = harness.record(&output.request_id);
        assert_eq!(record.status, 403);
        assert_eq!(record.client.key_name.as_deref(), Some("limited"));
        assert_eq!(record.error.as_ref().unwrap().kind, "permission");
    }
    assert_eq!(harness.fake.count(), 0);

    // Listings show only what the key may use.
    let listing = harness.gateway.models(Protocol::OpenaiChat, &limited);
    let ids: Vec<&str> = listing["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["m-chat", "m-gemini"]);
    assert!(
        harness
            .gateway
            .model(Protocol::OpenaiChat, &limited, "m-chat")
            .is_ok()
    );
    let hidden = harness
        .gateway
        .model(Protocol::OpenaiChat, &limited, "m-anthropic")
        .unwrap_err();
    assert_eq!(hidden.status, 404);
}

#[tokio::test]
async fn the_rate_limit_is_a_sliding_minute_per_key() {
    let harness = Harness::start(&format!("{KEYS}\n{FOUR_PROVIDERS}")).await;
    let ask = |protocol: Protocol| {
        let mut request = harness.request(protocol, "m-chat", false);
        request.identity = harness.identity_of("sy-limited-key-000001");
        harness.gateway.generate(request)
    };
    assert_eq!(ask(Protocol::OpenaiChat).await.status(), 200);
    assert_eq!(ask(Protocol::OpenaiChat).await.status(), 200);
    let output = Output::read(ask(Protocol::Gemini).await).await;
    assert_eq!(output.status, 429);
    let retry: u64 = output.header("retry-after").unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry), "{retry}");
    assert_eq!(output.json()["error"]["status"], "RESOURCE_EXHAUSTED");
    assert_eq!(
        harness.fake.count(),
        2,
        "the third request was not forwarded"
    );
    let record = harness.record(&output.request_id);
    assert_eq!(record.status, 429);
    assert_eq!(record.error.as_ref().unwrap().kind, "rate_limit");

    // Other keys are not affected, and neither is the dashboard.
    assert_eq!(
        harness
            .ask(Protocol::OpenaiChat, "m-chat", false)
            .await
            .status,
        200
    );
    let mut request = harness.request(Protocol::OpenaiChat, "m-chat", false);
    request.identity = harness.gateway.dashboard_identity();
    let output = Output::read(harness.gateway.generate(request).await).await;
    assert_eq!(output.status, 200);
    assert_eq!(harness.record(&output.request_id).key_name(), "dashboard");
}

// ---------------------------------------------------------------------------
// Model listings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn models_are_listed_in_each_protocols_shape() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let identity = harness.identity();
    let expected = ["m-anthropic", "m-chat", "m-gemini", "m-responses"];

    for protocol in [Protocol::OpenaiChat, Protocol::OpenaiResponses] {
        let listing = harness.gateway.models(protocol, &identity);
        assert_eq!(listing["object"], "list");
        let ids: Vec<&str> = listing["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, expected, "{protocol}");
        assert_eq!(listing["data"][0]["object"], "model");
    }

    let anthropic = harness.gateway.models(Protocol::Anthropic, &identity);
    let ids: Vec<&str> = anthropic["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, expected);
    assert_eq!(anthropic["data"][0]["type"], "model");
    assert_eq!(anthropic["has_more"], false);

    let gemini = harness.gateway.models(Protocol::Gemini, &identity);
    let names: Vec<&str> = gemini["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        expected
            .map(|id| format!("models/{id}"))
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );

    // Single models.
    let one = harness
        .gateway
        .model(Protocol::OpenaiChat, &identity, "m-chat")
        .unwrap();
    assert_eq!(one["id"], "m-chat");
    let one = harness
        .gateway
        .model(Protocol::Anthropic, &identity, "m-anthropic")
        .unwrap();
    assert_eq!(one["id"], "m-anthropic");
    for id in ["m-gemini", "models/m-gemini"] {
        let one = harness
            .gateway
            .model(Protocol::Gemini, &identity, id)
            .unwrap();
        assert_eq!(one["name"], "models/m-gemini", "{id}");
    }
    let missing = harness
        .gateway
        .model(Protocol::OpenaiChat, &identity, "nope")
        .unwrap_err();
    assert_eq!(missing.status, 404);
    assert_eq!(missing.code.as_deref(), Some("model_not_found"));
    // The `models/` prefix is Gemini's; other protocols take ids literally.
    assert!(
        harness
            .gateway
            .model(Protocol::OpenaiChat, &identity, "models/m-chat")
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// Token counting
// ---------------------------------------------------------------------------

fn count_request(harness: &Harness, protocol: Protocol, model: &str) -> ClientRequest {
    let body = match protocol {
        Protocol::Anthropic => json!({
            "model": model,
            "system": "You are terse.",
            "messages": [{"role": "user", "content": "How many tokens is this?"}]
        }),
        Protocol::Gemini => json!({
            "contents": [{"role": "user", "parts": [{"text": "How many tokens is this?"}]}]
        }),
        _ => json!({"model": model, "input": "How many tokens is this?"}),
    };
    let mut request = harness.request_with(protocol, body, model, false);
    request.endpoint = "POST count".to_string();
    request
}

fn counted(protocol: Protocol, output: &Output) -> u64 {
    assert_eq!(output.status, 200, "{:?}", output.body);
    let body = output.json();
    switchyard_codecs::codec(protocol)
        .decode_count_response(&body)
        .unwrap_or_else(|| panic!("not a {protocol} count: {body}"))
}

#[tokio::test]
async fn count_tokens_uses_the_upstreams_endpoint_natively() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    harness.fake.set_count(42);
    for (protocol, path) in [
        (Protocol::Anthropic, "/v1/messages/count_tokens"),
        (Protocol::Gemini, "/v1beta/models/gemini-up:countTokens"),
        (Protocol::OpenaiResponses, "/v1/responses/input_tokens"),
    ] {
        let (model, upstream_model, key) = support::route(protocol);
        harness.fake.clear();
        let request = count_request(&harness, protocol, model);
        let output = Output::read(harness.gateway.count_tokens(request).await).await;
        assert_eq!(counted(protocol, &output), 42, "{protocol}");

        let recorded = harness.fake.last();
        assert_eq!(recorded.kind, Kind::Count, "{protocol}");
        assert_eq!(recorded.path, path, "{protocol}");
        assert_eq!(recorded.key, key, "{protocol}");
        assert_eq!(recorded.model, upstream_model, "{protocol}");
        // The body is the client's, not a generation body.
        assert!(recorded.body.get("max_tokens").is_none(), "{protocol}");
        assert!(recorded.body.get("stream").is_none(), "{protocol}");
        let record = harness.record(&output.request_id);
        assert_eq!(record.mode, Some(Mode::Passthrough), "{protocol}");
        assert!(record.ok, "{protocol}");
        assert!(!record.stream, "{protocol}");
    }
}

#[tokio::test]
async fn count_tokens_translates_between_protocols() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    harness.fake.set_count(77);
    for client in [
        Protocol::Anthropic,
        Protocol::Gemini,
        Protocol::OpenaiResponses,
    ] {
        for upstream in [
            Protocol::Anthropic,
            Protocol::Gemini,
            Protocol::OpenaiResponses,
        ] {
            if client == upstream {
                continue;
            }
            let (model, upstream_model, _) = support::route(upstream);
            harness.fake.clear();
            let request = count_request(&harness, client, model);
            let output = Output::read(harness.gateway.count_tokens(request).await).await;
            let label = format!("{client} -> {upstream}");
            assert_eq!(counted(client, &output), 77, "{label}");
            let recorded = harness.fake.last();
            assert_eq!(recorded.kind, Kind::Count, "{label}");
            assert_eq!(recorded.wire, Some(support::wire(upstream)), "{label}");
            assert_eq!(recorded.model, upstream_model, "{label}");
            assert!(
                recorded
                    .body
                    .to_string()
                    .contains("How many tokens is this?"),
                "{label}: {}",
                recorded.body
            );
            assert_eq!(
                harness.record(&output.request_id).mode,
                Some(Mode::Translated),
                "{label}"
            );
        }
    }
}

#[tokio::test]
async fn count_tokens_estimates_when_the_upstream_cannot_count() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    // Chat Completions has no counting endpoint: nothing is sent upstream.
    let request = count_request(&harness, Protocol::Anthropic, "m-chat");
    let output = Output::read(harness.gateway.count_tokens(request).await).await;
    let estimate = counted(Protocol::Anthropic, &output);
    // "You are terse." + "How many tokens is this?" is 38 characters.
    assert_eq!(estimate, 10);
    assert_eq!(harness.fake.count(), 0);

    // An upstream whose protocol has the endpoint but which answers 404:
    // estimated as well, and the credential is not rested for it.
    for status in [404, 405, 501] {
        harness.fake.script(
            "key-anthropic-1",
            [Behaviour::error(status, "no such route")],
        );
        let request = count_request(&harness, Protocol::Anthropic, "m-anthropic");
        let output = Output::read(harness.gateway.count_tokens(request).await).await;
        assert_eq!(counted(Protocol::Anthropic, &output), 10, "{status}");
    }
    let anthropic = harness
        .gateway
        .scheduler()
        .snapshot()
        .into_iter()
        .find(|p| p.name == "anthropic")
        .unwrap();
    assert_eq!(anthropic.credentials[0].status, CredentialStatus::Ready);
    assert!(anthropic.credentials[0].model_cooldowns.is_empty());
    assert_eq!(anthropic.credentials[0].failures, 0);

    // Other failures are failures.
    harness
        .fake
        .script("key-anthropic-1", [Behaviour::error(500, "down")]);
    let request = count_request(&harness, Protocol::Anthropic, "m-anthropic");
    let output = Output::read(harness.gateway.count_tokens(request).await).await;
    assert_eq!(output.status, 500);
}

#[tokio::test]
async fn count_tokens_errors() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    // Chat Completions clients have no counting shape.
    let request = harness.request(Protocol::OpenaiChat, "m-chat", false);
    let output = Output::read(harness.gateway.count_tokens(request).await).await;
    assert_eq!(output.status, 404);
    assert_eq!(output.json()["error"]["type"], "invalid_request_error");

    let request = count_request(&harness, Protocol::Anthropic, "no-such-model");
    let output = Output::read(harness.gateway.count_tokens(request).await).await;
    assert_eq!(output.status, 404);
    assert_eq!(output.json()["error"]["type"], "not_found_error");

    let mut request = count_request(&harness, Protocol::Anthropic, "m-anthropic");
    request.body = Bytes::from_static(b"{broken");
    let output = Output::read(harness.gateway.count_tokens(request).await).await;
    assert_eq!(output.status, 400);
}

// ---------------------------------------------------------------------------
// Raw proxy
// ---------------------------------------------------------------------------

fn raw_request(harness: &Harness, model: &str) -> RawRequest {
    let mut headers = HeaderMap::new();
    headers.insert("accept", HeaderValue::from_static("application/json"));
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {CLIENT_KEY}")).unwrap(),
    );
    RawRequest {
        path: "embeddings".to_string(),
        method: Method::POST,
        body: Bytes::from(
            json!({"input": ["hello", "world"], "model": model, "dimensions": 3}).to_string(),
        ),
        content_type: Some("application/json".to_string()),
        query: None,
        model: model.to_string(),
        headers,
        identity: harness.identity(),
        client_ip: Some("203.0.113.9".to_string()),
        endpoint: "POST /v1/embeddings".to_string(),
        cancel: CancellationToken::new(),
    }
}

const RAW: &str = r#"
[routing]
strategy = "fill-first"

[[providers]]
name = "oai"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-raw-a", "key-raw-b"]
[[providers.models]]
id = "text-embedding-up"
alias = "embed"

[[providers]]
name = "anthropic"
kind = "anthropic"
base_url = "{base}"
api_keys = ["key-anthropic-1"]
[[providers.models]]
id = "claude-up"
alias = "m-anthropic"
"#;

#[tokio::test]
async fn raw_requests_are_forwarded_with_the_model_replaced() {
    let harness = Harness::start(RAW).await;
    let output = Output::read(harness.gateway.raw(raw_request(&harness, "embed")).await).await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    let reply = output.json();
    assert_eq!(reply["data"][0]["embedding"], json!([0.25, -0.5, 1.0]));
    assert_eq!(
        reply["model"], "text-embedding-up",
        "the upstream's answer, verbatim"
    );

    let recorded = harness.fake.last();
    assert_eq!(recorded.path, "/v1/embeddings");
    assert_eq!(recorded.method, Method::POST);
    assert_eq!(recorded.key, "key-raw-a");
    assert_eq!(
        recorded.body,
        json!({"input": ["hello", "world"], "model": "text-embedding-up", "dimensions": 3})
    );
    assert_eq!(recorded.header("content-type"), Some("application/json"));
    assert!(!format!("{:?}", recorded.headers).contains(CLIENT_KEY));

    let record = harness.record(&output.request_id);
    assert_eq!(record.mode, Some(Mode::Raw));
    assert_eq!(record.endpoint, "POST /v1/embeddings");
    assert_eq!(record.client_model.as_deref(), Some("embed"));
    assert_eq!(record.upstream_model.as_deref(), Some("text-embedding-up"));
    assert_eq!(record.usage.input_tokens, 5);
    assert_eq!(record.client.ip.as_deref(), Some("203.0.113.9"));
    assert!(record.ok);
    assert_eq!(output.header("x-ratelimit-remaining-requests"), Some("99"));
    assert_eq!(output.header("x-upstream-request-id"), Some("req_fake_1"));
}

#[tokio::test]
async fn raw_requests_fail_over_and_report_errors() {
    let harness = Harness::start(RAW).await;
    harness
        .fake
        .script("key-raw-a", [Behaviour::error(500, "down")]);
    let output = Output::read(harness.gateway.raw(raw_request(&harness, "embed")).await).await;
    assert_eq!(output.status, 200);
    assert_eq!(harness.fake.keys(), vec!["key-raw-a", "key-raw-b"]);
    assert_eq!(harness.record(&output.request_id).attempts.len(), 2);

    // A request fault is returned as the upstream wrote it, at once.
    harness.fake.clear();
    harness
        .fake
        .script("key-raw-b", [Behaviour::error(400, "input is too long")]);
    let output = Output::read(harness.gateway.raw(raw_request(&harness, "embed")).await).await;
    assert_eq!(output.status, 400);
    assert_eq!(output.json()["error"]["message"], "input is too long");
    assert_eq!(harness.fake.count(), 1);

    // Models without an OpenAI-compatible route, and unknown models.
    for model in ["m-anthropic", "nope"] {
        let output = Output::read(harness.gateway.raw(raw_request(&harness, model)).await).await;
        assert_eq!(output.status, 404, "{model}");
        assert_eq!(output.json()["error"]["code"], "model_not_found");
    }
}

// ---------------------------------------------------------------------------
// Mock provider
// ---------------------------------------------------------------------------

const MOCK: &str = r#"
[[providers]]
name = "demo"
kind = "mock"
"#;

#[tokio::test]
async fn the_mock_provider_answers_in_all_four_protocols() {
    let harness = Harness::start(MOCK).await;
    for protocol in PROTOCOLS {
        for stream in [false, true] {
            let label = format!("{protocol}, stream={stream}");
            let output = harness.ask(protocol, "mock-echo", stream).await;
            assert_eq!(output.streamed, stream, "{label}: {:?}", output.body);
            let response = output.response(protocol);
            assert!(
                response.text().contains("hi"),
                "{label}: {:?}",
                response.text()
            );
            assert_eq!(response.finish, FinishReason::Stop, "{label}");
            assert_eq!(response.model, "mock-echo", "{label}");
            assert!(response.usage.output_tokens > 0, "{label}");

            let record = harness.record(&output.request_id);
            assert_eq!(record.mode, Some(Mode::Mock), "{label}");
            assert_eq!(record.provider.as_deref(), Some("demo"), "{label}");
            assert!(record.ok, "{label}");
            assert!(record.usage.output_tokens > 0, "{label}");
        }
    }
    assert_eq!(harness.fake.count(), 0, "the mock needs no network");
}

#[tokio::test]
async fn the_mock_provider_calls_tools_and_thinks() {
    let harness = Harness::start(MOCK).await;
    for protocol in PROTOCOLS {
        for stream in [false, true] {
            let label = format!("{protocol}, stream={stream}");
            let request = body(protocol, "mock-tools", stream, true);
            let output = harness
                .ask_with(protocol, request, "mock-tools", stream)
                .await;
            let response = output.response(protocol);
            let call = response
                .tool_calls()
                .next()
                .unwrap_or_else(|| panic!("{label}"));
            assert_eq!(call.name, "get_weather", "{label}");
            assert_eq!(
                arguments(&call.arguments),
                json!({"city": "mock-city"}),
                "{label}"
            );

            let output = harness.ask(protocol, "mock-think", stream).await;
            let response = output.response(protocol);
            assert!(
                response
                    .parts
                    .iter()
                    .any(|p| matches!(p, Part::Reasoning(_))),
                "{label}: {:?}",
                response.parts
            );
            assert!(!response.text().is_empty(), "{label}");
        }
    }
}

#[tokio::test]
async fn failing_mock_models_fail_like_upstreams() {
    let harness = Harness::start(MOCK).await;
    for stream in [false, true] {
        let output = harness
            .ask(Protocol::Anthropic, "mock-error-429", stream)
            .await;
        assert!(!output.streamed);
        assert_eq!(output.status, 429);
        // Rendered in the client's envelope, not the mock's.
        assert_eq!(output.json()["type"], "error");
        assert_eq!(output.json()["error"]["type"], "rate_limit_error");
        assert_eq!(output.header("retry-after"), Some("2"));
    }
    let output = harness
        .ask(Protocol::OpenaiChat, "mock-error-500", false)
        .await;
    assert_eq!(output.status, 502);
    let output = harness.ask(Protocol::Gemini, "mock-error-401", false).await;
    assert_eq!(output.status, 502);
    assert!(
        output.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("rejected the gateway's credential")
    );
    let record = harness.record(&output.request_id);
    assert_eq!(record.mode, Some(Mode::Mock));
    assert_eq!(record.attempts.len(), 1);
    assert_eq!(record.attempts[0].status, 401);
}

#[tokio::test]
async fn mock_models_are_listed_and_counted() {
    let harness = Harness::start(MOCK).await;
    let listing = harness
        .gateway
        .models(Protocol::OpenaiChat, &harness.identity());
    let ids: Vec<&str> = listing["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"mock-echo") && ids.contains(&"mock-tools"),
        "{ids:?}"
    );

    let request = count_request(&harness, Protocol::Anthropic, "mock-echo");
    let output = Output::read(harness.gateway.count_tokens(request).await).await;
    assert_eq!(counted(Protocol::Anthropic, &output), 10);

    let models = harness.gateway.discover("demo").await.unwrap();
    assert!(models.iter().any(|m| m.id == "mock-think"));
    let test = harness.gateway.test_provider("demo", None).await;
    assert!(test.ok, "{test:?}");
}

// ---------------------------------------------------------------------------
// Vertex AI
// ---------------------------------------------------------------------------

/// A throwaway RSA key generated for tests only.
const TEST_KEY_PEM: &str = include_str!("fixtures/test_rsa_pkcs8.pem");

const VERTEX: &str = r#"
[[providers]]
name = "vx"
kind = "vertex"
base_url = "{base}"
location = "us-central1"
[[providers.credentials]]
service_account_file = "sa.json"
[[providers.models]]
id = "gemini-vx"
[[providers.models]]
id = "claude-vx@20250101"
alias = "claude-vx"
"#;

async fn vertex_harness() -> Harness {
    // The key file names the fake's token endpoint, so the file has to be
    // written once the fake's address is known: start, then drop it in.
    let harness = Harness::start(VERTEX).await;
    let key_file = json!({
        "type": "service_account",
        "project_id": "demo-project",
        "private_key_id": "0123456789abcdef",
        "private_key": TEST_KEY_PEM,
        "client_email": "gateway@demo-project.iam.gserviceaccount.com",
        "client_id": "100000000000000000001",
        "token_uri": format!("{}/token", harness.fake.base()),
    });
    std::fs::write(harness.dir.path().join("sa.json"), key_file.to_string()).unwrap();
    // The gateway looked for the file when it applied the configuration,
    // found none and set the credential aside; have it look again.
    harness.reload().await;
    harness
}

#[tokio::test]
async fn vertex_gemini_models_are_called_with_a_minted_token() {
    let harness = vertex_harness().await;
    for stream in [false, true] {
        harness.fake.clear();
        let output = harness.ask(Protocol::Gemini, "gemini-vx", stream).await;
        assert_eq!(
            output.response(Protocol::Gemini).text(),
            "Hello from the fake upstream",
            "stream={stream}: {:?}",
            output.body
        );
        let recorded = harness.fake.last();
        let action = if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        assert_eq!(
            recorded.path,
            format!(
                "/v1/projects/demo-project/locations/us-central1/publishers/google/models/gemini-vx:{action}"
            )
        );
        assert_eq!(
            recorded.key,
            support::fake::TOKEN,
            "the minted access token"
        );
        assert_eq!(
            harness.record(&output.request_id).mode,
            Some(Mode::Passthrough)
        );
    }
    // The key file was read once and the token minted once.
    assert_eq!(harness.fake.tokens_minted(), 1);
}

#[tokio::test]
async fn vertex_claude_models_are_spoken_to_in_anthropic_messages() {
    let harness = vertex_harness().await;
    // An Anthropic client: same protocol, so the body is forwarded — with
    // Vertex's conventions applied.
    let output = harness.ask(Protocol::Anthropic, "claude-vx", false).await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    let recorded = harness.fake.last();
    assert_eq!(
        recorded.path,
        "/v1/projects/demo-project/locations/us-central1/publishers/anthropic/models/claude-vx@20250101:rawPredict"
    );
    assert_eq!(recorded.body["anthropic_version"], "vertex-2023-10-16");
    assert!(recorded.body.get("model").is_none(), "{}", recorded.body);
    assert_eq!(recorded.body["messages"][0]["content"], "hi");
    let record = harness.record(&output.request_id);
    assert_eq!(record.upstream_protocol, Some(Protocol::Anthropic));
    assert_eq!(record.mode, Some(Mode::Passthrough));
    assert_eq!(output.json()["model"], "claude-vx");

    // A Chat client is translated to Anthropic Messages, streaming too.
    let output = harness.ask(Protocol::OpenaiChat, "claude-vx", true).await;
    assert_eq!(
        output.response(Protocol::OpenaiChat).text(),
        "Hello from the fake upstream"
    );
    let recorded = harness.fake.last();
    assert!(
        recorded.path.ends_with(":streamRawPredict"),
        "{}",
        recorded.path
    );
    assert_eq!(recorded.wire, Some(Wire::Anthropic));
    assert_eq!(
        harness.record(&output.request_id).mode,
        Some(Mode::Translated)
    );
}

/// The file is looked for when the configuration is applied: a credential
/// whose file cannot be read is set aside with the reason, instead of
/// looking ready until the first request fails on it. (The marks
/// themselves are tested in `integration_pass.rs`.)
#[tokio::test]
async fn an_unreadable_service_account_file_sets_the_credential_aside_not_the_gateway() {
    // No sa.json is written.
    let harness = Harness::start(VERTEX).await;
    let credential = &harness.gateway.scheduler().snapshot()[0].credentials[0];
    assert!(!credential.usable, "{credential:?}");
    let reason = credential.unusable_reason.as_deref().unwrap_or_default();
    assert!(reason.contains("sa.json"), "{reason}");
    // The absolute path is not disclosed.
    assert!(!reason.contains(":\\") && !reason.contains('/'), "{reason}");

    let output = harness.ask(Protocol::Gemini, "gemini-vx", false).await;
    assert_eq!(output.status, 503, "{:?}", output.body);
    let record = harness.record(&output.request_id);
    assert!(record.attempts.is_empty(), "{:?}", record.attempts);
    // Nothing is held against the credential: it was never tried.
    let credential = &harness.gateway.scheduler().snapshot()[0].credentials[0];
    assert_eq!(credential.cooldown_reason, None);
    assert_eq!(harness.fake.count(), 0);
}

// ---------------------------------------------------------------------------
// Upstream WebSockets
// ---------------------------------------------------------------------------

const REALTIME: &str = r#"
[routing]
strategy = "fill-first"

[[providers]]
name = "oai"
kind = "openai"
base_url = "{base}/v1"
api_keys = ["key-ws-a", "key-ws-b"]
[[providers.models]]
id = "gpt-realtime-up"
alias = "realtime"

[[providers]]
name = "anthropic"
kind = "anthropic"
base_url = "{base}"
api_keys = ["key-anthropic-1"]
[[providers.models]]
id = "claude-up"
alias = "m-anthropic"
"#;

fn ws_request(harness: &Harness, model: &str) -> WsOpenRequest {
    let mut headers = HeaderMap::new();
    headers.insert("openai-beta", HeaderValue::from_static("realtime=v1"));
    headers.insert("origin", HeaderValue::from_static("https://app.example"));
    headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.9"));
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {CLIENT_KEY}")).unwrap(),
    );
    WsOpenRequest {
        identity: harness.identity(),
        model: model.to_string(),
        path_and_query: "realtime?model={model}".to_string(),
        headers,
        endpoint: "GET /v1/realtime".to_string(),
        client_ip: None,
        require_kind: Some(ProviderKind::Openai),
    }
}

#[tokio::test]
async fn an_upstream_websocket_is_opened_relayed_and_recorded() {
    let harness = Harness::start(REALTIME).await;
    let mut session = harness
        .gateway
        .open_upstream_ws(ws_request(&harness, "realtime"))
        .await
        .expect("the upstream socket opens");
    assert_eq!(session.provider, "oai");
    assert_eq!(session.upstream_model, "gpt-realtime-up");

    let recorded = harness.fake.last();
    assert_eq!(recorded.kind, Kind::WebSocket);
    assert_eq!(recorded.path, "/v1/realtime");
    assert_eq!(recorded.query.as_deref(), Some("model=gpt-realtime-up"));
    assert_eq!(
        recorded.key, "key-ws-a",
        "the gateway's credential, not the client's"
    );
    assert_eq!(recorded.header("openai-beta"), Some("realtime=v1"));
    // Only the allow-listed client headers are offered to the upstream.
    assert_eq!(recorded.header("origin"), None);
    assert_eq!(recorded.header("x-forwarded-for"), None);

    let hello = session.socket.next().await.unwrap().unwrap();
    let hello: Value = serde_json::from_str(hello.to_text().unwrap()).unwrap();
    assert_eq!(
        hello,
        json!({"type": "session.created", "model": "gpt-realtime-up"})
    );
    session
        .socket
        .send(WsMessage::text("ping-1"))
        .await
        .unwrap();
    let echo = session.socket.next().await.unwrap().unwrap();
    assert_eq!(echo.to_text().unwrap(), "echo:ping-1");

    let request_id = session.request_id.clone();
    assert!(
        harness
            .gateway
            .telemetry()
            .usage()
            .get(&request_id)
            .is_none()
    );
    let usage = switchyard_core::Usage {
        input_tokens: 9,
        output_tokens: 4,
        ..Default::default()
    };
    session.finish(WsOutcome::closed().with_usage(usage));

    let record = harness.record(&request_id);
    assert_eq!(record.status, 101);
    assert!(record.ok);
    assert_eq!(record.transport, Transport::Websocket);
    assert_eq!(record.mode, Some(Mode::Raw));
    assert_eq!(record.endpoint, "GET /v1/realtime");
    assert_eq!(record.client_model.as_deref(), Some("realtime"));
    assert_eq!(record.upstream_model.as_deref(), Some("gpt-realtime-up"));
    assert_eq!(record.usage, usage);
    assert_eq!(record.attempts.len(), 1);
    assert_eq!(record.attempts[0].status, 101);
    let credential = &harness.gateway.scheduler().snapshot()[0].credentials[0];
    assert_eq!((credential.requests, credential.successes), (1, 1));
}

#[tokio::test]
async fn websocket_handshake_failures_fail_over() {
    let harness = Harness::start(REALTIME).await;
    harness
        .fake
        .script("key-ws-a", [Behaviour::rate_limited(20)]);
    let session = harness
        .gateway
        .open_upstream_ws(ws_request(&harness, "realtime"))
        .await
        .expect("the second credential connects");
    assert_eq!(harness.fake.keys(), vec!["key-ws-a", "key-ws-b"]);
    let request_id = session.request_id.clone();
    // Dropped without `finish`: recorded as aborted.
    drop(session);
    let record = harness.record(&request_id);
    assert_eq!(record.attempts.len(), 2);
    assert_eq!(record.attempts[0].status, 429);
    assert!(!record.ok);
    assert_eq!(record.error.as_ref().unwrap().kind, "aborted");
    let credentials = &harness.gateway.scheduler().snapshot()[0].credentials;
    assert_eq!(credentials[0].model_cooldowns.len(), 1);
    assert_eq!(credentials[1].successes, 1);

    // Every credential refusing the handshake: an error, in the gateway's
    // terms (an upstream 401 is a 502).
    let harness = Harness::start(REALTIME).await;
    for key in ["key-ws-a", "key-ws-b"] {
        harness.fake.script(key, [Behaviour::error(401, "bad key")]);
    }
    let error = harness
        .gateway
        .open_upstream_ws(ws_request(&harness, "realtime"))
        .await
        .expect_err("no credential connects");
    assert_eq!(error.status, 502);
    assert_eq!(error.kind, ErrorKind::Upstream);
    assert!(!error.message.contains("bad key"));
    assert_eq!(harness.fake.count(), 2);
}

#[tokio::test]
async fn websocket_requests_respect_kind_and_model_rules() {
    let harness = Harness::start(REALTIME).await;
    // Served only by a provider of another kind.
    let error = harness
        .gateway
        .open_upstream_ws(ws_request(&harness, "m-anthropic"))
        .await
        .unwrap_err();
    assert_eq!(error.status, 404);
    assert!(error.message.contains("openai"), "{}", error.message);
    let error = harness
        .gateway
        .open_upstream_ws(ws_request(&harness, "nope"))
        .await
        .unwrap_err();
    assert_eq!(error.status, 404);
    assert_eq!(harness.fake.count(), 0);

    // An upstream failure mid-session is held against the credential.
    let session = harness
        .gateway
        .open_upstream_ws(ws_request(&harness, "realtime"))
        .await
        .unwrap();
    let request_id = session.request_id.clone();
    session.finish(WsOutcome::upstream_failed("connection reset by peer"));
    let record = harness.record(&request_id);
    assert_eq!(record.status, 101);
    assert!(!record.ok);
    assert_eq!(record.error.as_ref().unwrap().kind, "upstream");
    assert_eq!(
        harness.gateway.scheduler().snapshot()[0].credentials[0].failures,
        1
    );
}

// ---------------------------------------------------------------------------
// Provider tests and discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_provider_sends_a_tiny_request_and_reports_the_outcome() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    for (provider, upstream) in [
        ("chat", Protocol::OpenaiChat),
        ("responses", Protocol::OpenaiResponses),
        ("anthropic", Protocol::Anthropic),
        ("gemini", Protocol::Gemini),
    ] {
        let (_, upstream_model, key) = support::route(upstream);
        harness.fake.clear();
        let test = harness.gateway.test_provider(provider, None).await;
        assert!(test.ok, "{provider}: {test:?}");
        assert_eq!(test.status, 200);
        assert_eq!(test.model.as_deref(), Some(upstream_model));
        assert!(test.credential.is_some());
        assert_eq!(test.error, None);

        let recorded = harness.fake.last();
        assert_eq!(recorded.key, key, "{provider}");
        assert_eq!(recorded.kind, Kind::Generate { stream: false });
        let sent = support::decode_upstream(&recorded, upstream);
        assert_eq!(sent.messages[0].text(), "ping", "{provider}");
        assert_eq!(sent.max_output_tokens, Some(16), "{provider}");
    }
    // A client-facing alias is turned into the upstream id.
    let test = harness.gateway.test_provider("chat", Some("m-chat")).await;
    assert_eq!(test.model.as_deref(), Some("up-chat"));
    let serialised = serde_json::to_value(&test).unwrap();
    assert_eq!(serialised["ok"], true);
    assert!(serialised.get("error").is_none());
}

#[tokio::test]
async fn test_provider_reports_failures_and_feeds_the_scheduler() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    harness.fake.script(
        "key-chat-1",
        [Behaviour::error(401, "Incorrect API key provided")],
    );
    let test = harness.gateway.test_provider("chat", None).await;
    assert!(!test.ok);
    assert_eq!(test.status, 401);
    assert!(test.error.as_deref().unwrap().contains("Incorrect API key"));
    let credential = |harness: &Harness| {
        harness
            .gateway
            .scheduler()
            .snapshot()
            .into_iter()
            .find(|p| p.name == "chat")
            .unwrap()
            .credentials
            .remove(0)
    };
    assert_eq!(credential(&harness).status, CredentialStatus::Cooling);
    assert_eq!(
        credential(&harness).cooldown_reason,
        Some(FailureClass::Auth)
    );
    // While it rests, clients cannot use it…
    assert_eq!(
        harness
            .ask(Protocol::OpenaiChat, "m-chat", false)
            .await
            .status,
        429
    );

    // …but a test addresses the credential directly, whatever its state,
    // and one that fails for the request's sake blames nobody.
    harness
        .fake
        .script("key-chat-1", [Behaviour::error(400, "bad request")]);
    let test = harness.gateway.test_provider("chat", None).await;
    assert_eq!((test.ok, test.status), (false, 400));

    assert!(!harness.gateway.test_provider("nope", None).await.ok);
    let unknown = harness.gateway.test_provider("nope", None).await;
    assert_eq!(unknown.status, 0);
    assert!(unknown.error.unwrap().contains("unknown provider"));
}

#[tokio::test]
async fn a_successful_test_puts_a_resting_model_back_into_rotation() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    harness
        .fake
        .script("key-chat-1", [Behaviour::error(500, "down")]);
    assert_eq!(
        harness
            .ask(Protocol::OpenaiChat, "m-chat", false)
            .await
            .status,
        500
    );
    assert_eq!(
        harness
            .ask(Protocol::OpenaiChat, "m-chat", false)
            .await
            .status,
        429
    );
    assert!(harness.gateway.test_provider("chat", None).await.ok);
    assert_eq!(
        harness
            .ask(Protocol::OpenaiChat, "m-chat", false)
            .await
            .status,
        200
    );
}

const DISCOVERY: &str = r#"
[[providers]]
name = "auto"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-auto-1"]

[[providers]]
name = "fixed"
kind = "anthropic"
base_url = "{base}"
api_keys = ["key-fixed-1"]
[[providers.models]]
id = "claude-fixed"
"#;

#[tokio::test]
async fn models_are_discovered_at_start_and_on_demand() {
    let harness = Harness::start(DISCOVERY).await;
    // At start, in the background, for providers without configured models.
    support::eventually("the upstream's models become routable", || {
        harness.gateway.scheduler().resolve("disc-alpha").is_ok()
    })
    .await;
    let listed: Vec<_> = harness
        .fake
        .requests()
        .into_iter()
        .filter(|r| r.kind == Kind::Models)
        .collect();
    assert_eq!(
        listed.len(),
        1,
        "only the provider that wants discovery is asked"
    );
    assert_eq!(listed[0].key, "key-auto-1");
    let output = harness.ask(Protocol::OpenaiChat, "disc-beta", false).await;
    assert_eq!(output.status, 200);
    assert_eq!(harness.fake.last().model, "disc-beta");

    // On demand: the new list replaces the old one.
    harness.fake.set_models(&["disc-gamma"]);
    let models = harness.gateway.discover("auto").await.unwrap();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["disc-gamma"]
    );
    assert!(harness.gateway.scheduler().resolve("disc-gamma").is_ok());
    assert!(harness.gateway.scheduler().resolve("disc-alpha").is_err());

    // Errors are reported, and change nothing.
    harness
        .fake
        .script("key-auto-1", [Behaviour::error(500, "listing is down")]);
    let error = harness.gateway.discover("auto").await.unwrap_err();
    assert_eq!(error.status, 502);
    assert!(error.message.contains("listing is down"));
    assert!(harness.gateway.scheduler().resolve("disc-gamma").is_ok());
    let error = harness.gateway.discover("nope").await.unwrap_err();
    assert_eq!(error.status, 404);
}

// ---------------------------------------------------------------------------
// Configuration changes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_configuration_change_is_applied_without_a_restart() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let levels: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = levels.clone();
    harness
        .gateway
        .on_log_level(move |level| seen.lock().unwrap().push(level.to_string()));
    assert_eq!(
        harness
            .ask(Protocol::OpenaiChat, "m-extra", false)
            .await
            .status,
        404
    );

    // Rest a credential first: its state must survive the reload.
    harness
        .fake
        .script("key-chat-1", [Behaviour::error(500, "down")]);
    assert_eq!(
        harness
            .ask(Protocol::OpenaiChat, "m-chat", false)
            .await
            .status,
        500
    );

    let new_config = format!(
        r#"
[upstream]
proxy = "direct"

[logging]
level = "debug"

[[auth.keys]]
key = "sy-replacement-key-0001"
name = "replacement"
{FOUR_PROVIDERS}
[[providers]]
name = "extra"
kind = "openai-compat"
base_url = "{{base}}/v1"
api_keys = ["key-extra-1"]
[[providers.models]]
id = "up-extra"
alias = "m-extra"

[[providers]]
name = "demo"
kind = "mock"
"#
    );
    harness.reconfigure(&new_config).await;

    // The removed key stops authenticating; the new one works.
    let old = harness
        .gateway
        .authenticate(&PresentedCredentials {
            x_api_key: Some(CLIENT_KEY.to_string()),
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(old.message, "invalid API key");
    let identity = harness.identity_of("sy-replacement-key-0001");
    assert_eq!(identity.key_name.as_deref(), Some("replacement"));

    // The new provider is routable, and so is a mock added on the fly.
    let request = harness.request_by(&identity, Protocol::OpenaiChat, "m-extra", false);
    let output = Output::read(harness.gateway.generate(request).await).await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(harness.fake.last().key, "key-extra-1");
    let request = harness.request_by(&identity, Protocol::Anthropic, "mock-echo", false);
    assert_eq!(harness.gateway.generate(request).await.status(), 200);
    let listing = harness.gateway.models(Protocol::OpenaiChat, &identity);
    assert!(listing.to_string().contains("m-extra"));

    // Runtime state of unchanged credentials is kept.
    let request = harness.request_by(&identity, Protocol::OpenaiChat, "m-chat", false);
    assert_eq!(harness.gateway.generate(request).await.status(), 429);

    assert_eq!(*levels.lock().unwrap(), vec!["debug".to_string()]);
    assert_eq!(harness.gateway.config().providers.len(), 6);
}

#[tokio::test]
async fn rate_limit_counts_survive_a_reload() {
    let harness = Harness::start(&format!("{KEYS}\n{FOUR_PROVIDERS}")).await;
    let ask = || {
        let mut request = harness.request(Protocol::OpenaiChat, "m-chat", false);
        request.identity = harness.identity_of("sy-limited-key-000001");
        harness.gateway.generate(request)
    };
    assert_eq!(ask().await.status(), 200);
    assert_eq!(ask().await.status(), 200);
    // Same keys, another setting changed.
    let new_config = format!(
        "{}\n[routing]\nmax_attempts = 2\n{KEYS}\n{FOUR_PROVIDERS}",
        support::PREAMBLE
    );
    harness.reconfigure(&new_config).await;
    assert_eq!(harness.gateway.config().routing.max_attempts, 2);
    assert_eq!(ask().await.status(), 429, "the window was not reset");
}

// ---------------------------------------------------------------------------
// Telemetry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn requests_are_announced_recorded_priced_and_persisted() {
    let pricing = r#"
[[pricing]]
model = "up-chat"
input = 1.0
output = 2.0
"#;
    let harness = Harness::start(&format!("{FOUR_PROVIDERS}\n{pricing}")).await;
    let mut events = harness.gateway.telemetry().subscribe();

    let mut request = harness.request(Protocol::Anthropic, "m-chat", false);
    request
        .headers
        .insert("user-agent", HeaderValue::from_static("test-client/1.0"));
    request.client_ip = Some("198.51.100.7".to_string());
    request.request_id = Some("req-explicit-0001".to_string());
    let output = Output::read(harness.gateway.generate(request).await).await;
    assert_eq!(output.request_id, "req-explicit-0001");
    assert_eq!(output.header("x-request-id"), Some("req-explicit-0001"));

    let started = events.recv().await.unwrap();
    assert_eq!(started.topic(), "request.started");
    assert_eq!(started.data()["id"], "req-explicit-0001");
    assert_eq!(started.data()["requested_model"], "m-chat");
    let finished = events.recv().await.unwrap();
    assert_eq!(finished.topic(), "request.finished");
    assert_eq!(finished.data()["id"], "req-explicit-0001");

    let record = harness.record("req-explicit-0001");
    assert_eq!(record.endpoint, "POST /v1/messages");
    assert_eq!(record.client.ip.as_deref(), Some("198.51.100.7"));
    assert_eq!(record.client.user_agent.as_deref(), Some("test-client/1.0"));
    assert_eq!(
        record.client.key_id,
        Some(switchyard_config_store::client_key_id(CLIENT_KEY))
    );
    assert_eq!(record.provider.as_deref(), Some("chat"));
    assert!(
        record
            .credential_id
            .as_deref()
            .unwrap()
            .starts_with("chat:")
    );
    assert!(record.credential_label.is_some());
    assert!(
        !format!("{record:?}").contains("key-chat-1"),
        "no secrets in records"
    );
    assert_eq!(
        (record.usage.input_tokens, record.usage.output_tokens),
        (11, 7)
    );
    let cost = record.cost.expect("a price is configured");
    assert!(
        (cost - (11.0 * 1.0 + 7.0 * 2.0) / 1_000_000.0).abs() < 1e-12,
        "{cost}"
    );
    assert!(record.duration_ms >= record.ttfb_ms.unwrap());
    assert!(!record.has_bodies, "request logging is off by default");

    // Models without a price have no cost.
    let output = harness.ask(Protocol::OpenaiChat, "m-gemini", false).await;
    assert_eq!(harness.record(&output.request_id).cost, None);

    // Shutdown flushes: the record is in the usage file.
    harness.gateway.shutdown().await;
    let usage_dir = harness.dir.path().join("data").join("usage");
    let mut persisted = String::new();
    for entry in std::fs::read_dir(&usage_dir).unwrap() {
        persisted.push_str(&std::fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    assert!(persisted.contains("req-explicit-0001"), "{persisted}");
    assert!(!persisted.contains("key-chat-1"));
    assert!(!persisted.contains(CLIENT_KEY));
}

#[tokio::test]
async fn bodies_are_captured_when_request_logging_is_on() {
    let logging = "[logging]\nrequest_log = \"all\"\n";
    let harness = Harness::start(&format!("{logging}\n{FOUR_PROVIDERS}")).await;

    // A translated, complete request.
    let mut request = harness.request(Protocol::Anthropic, "m-chat", false);
    request
        .headers
        .insert("x-api-key", HeaderValue::from_str(CLIENT_KEY).unwrap());
    let output = Output::read(harness.gateway.generate(request).await).await;
    assert!(harness.record(&output.request_id).has_bodies);
    harness.gateway.telemetry().flush().await.unwrap();
    let bodies = harness
        .gateway
        .telemetry()
        .bodies()
        .read(&output.request_id)
        .expect("bodies were captured");
    assert!(
        bodies
            .client_request
            .as_deref()
            .unwrap()
            .contains("\"m-chat\"")
    );
    assert!(
        bodies
            .upstream_request
            .as_deref()
            .unwrap()
            .contains("\"up-chat\"")
    );
    assert!(
        bodies
            .upstream_response
            .as_deref()
            .unwrap()
            .contains("chat.completion")
    );
    assert!(
        bodies
            .client_response
            .as_deref()
            .unwrap()
            .contains("Hello from the fake upstream")
    );
    // Credentials are redacted on both sides.
    assert!(!format!("{bodies:?}").contains(CLIENT_KEY));
    assert!(!format!("{bodies:?}").contains("key-chat-1"));
    assert!(bodies.client_headers.contains_key("x-api-key"));
    assert!(bodies.upstream_headers.contains_key("authorization"));

    // A stream: the raw upstream events and what the client was sent.
    let output = harness.ask(Protocol::Anthropic, "m-chat", true).await;
    assert!(output.streamed);
    harness.gateway.telemetry().flush().await.unwrap();
    let bodies = harness
        .gateway
        .telemetry()
        .bodies()
        .read(&output.request_id)
        .expect("stream bodies were captured");
    let upstream = bodies.upstream_response.unwrap();
    assert!(
        upstream.contains("chat.completion.chunk") && upstream.contains("[DONE]"),
        "{upstream}"
    );
    let client = bodies.client_response.unwrap();
    assert!(
        client.contains("event: message_start") && client.contains("message_stop"),
        "{client}"
    );

    // Errors are captured too.
    let output = harness
        .ask(Protocol::OpenaiChat, "no-such-model", false)
        .await;
    harness.gateway.telemetry().flush().await.unwrap();
    let bodies = harness
        .gateway
        .telemetry()
        .bodies()
        .read(&output.request_id)
        .unwrap();
    assert!(bodies.client_response.unwrap().contains("model_not_found"));
    assert_eq!(bodies.upstream_request, None);
}

#[tokio::test]
async fn upstream_rate_limit_headers_are_passed_through_when_enabled() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    for stream in [false, true] {
        let output = harness.ask(Protocol::OpenaiChat, "m-chat", stream).await;
        assert!(
            output.header("x-ratelimit-remaining-requests").is_some(),
            "stream={stream}"
        );
        assert!(
            output.header("x-upstream-request-id").is_some(),
            "stream={stream}"
        );
        assert_eq!(output.header("x-switchyard-provider"), Some("chat"));
        assert_eq!(
            output.header("x-request-id"),
            Some(output.request_id.as_str())
        );
    }

    let off = "[upstream]\nproxy = \"direct\"\npassthrough_headers = false\n\n[[auth.keys]]\nkey = \"sy-test-key-0123456789\"\n";
    let harness = Harness::start_raw(&format!("{off}\n{FOUR_PROVIDERS}")).await;
    let output = harness.ask(Protocol::OpenaiChat, "m-chat", false).await;
    assert_eq!(output.status, 200);
    assert_eq!(output.header("x-ratelimit-remaining-requests"), None);
    assert_eq!(output.header("x-upstream-request-id"), None);
    assert!(output.header("x-request-id").is_some());
}

#[tokio::test]
async fn sessions_stay_on_the_credential_that_served_them() {
    let config = r#"
[[providers]]
name = "chat"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-a", "key-b", "key-c"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#;
    let harness = Harness::start(config).await;
    // Round-robin would rotate; an explicit session pins the credential.
    let mut keys = Vec::new();
    for _ in 0..4 {
        let mut request = harness.request(Protocol::OpenaiChat, "m", false);
        request.session = Some("conversation-1".to_string());
        assert_eq!(harness.gateway.generate(request).await.status(), 200);
        keys.push(harness.fake.last().key);
    }
    assert!(keys.iter().all(|key| key == &keys[0]), "{keys:?}");

    // So does the same conversation opening (the fingerprint), while
    // different conversations spread out.
    harness.fake.clear();
    let conversation =
        |opening: &str| json!({"model": "m", "messages": [{"role": "user", "content": opening}]});
    for _ in 0..3 {
        harness
            .ask_with(
                Protocol::OpenaiChat,
                conversation("the very same opening"),
                "m",
                false,
            )
            .await;
    }
    let same = harness.fake.keys();
    assert!(same.iter().all(|key| key == &same[0]), "{same:?}");
    harness.fake.clear();
    for index in 0..6 {
        harness
            .ask_with(
                Protocol::OpenaiChat,
                conversation(&format!("a different opening {index}")),
                "m",
                false,
            )
            .await;
    }
    let spread: std::collections::HashSet<String> = harness.fake.keys().into_iter().collect();
    assert!(spread.len() > 1, "{spread:?}");

    // A header names the session as well.
    harness.fake.clear();
    for _ in 0..3 {
        let mut request = harness.request(Protocol::OpenaiChat, "m", false);
        request
            .headers
            .insert("x-session-id", HeaderValue::from_static("header-session"));
        harness.gateway.generate(request).await;
    }
    let pinned = harness.fake.keys();
    assert!(pinned.iter().all(|key| key == &pinned[0]), "{pinned:?}");
}

#[tokio::test]
async fn replies_can_be_inspected_without_reading_them() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let full = harness
        .gateway
        .generate(harness.request(Protocol::OpenaiChat, "m-chat", false))
        .await;
    assert_eq!(full.status(), 200);
    assert!(!full.request_id().is_empty());
    let Reply::Full(full) = full else {
        panic!("expected a complete reply");
    };
    assert_eq!(full.content_type, "application/json");
    assert_eq!(full.header("x-request-id"), Some(full.request_id.as_str()));

    let stream = harness
        .gateway
        .generate(harness.request(Protocol::OpenaiChat, "m-chat", true))
        .await;
    assert_eq!(stream.status(), 200);
    let Reply::Stream(stream) = stream else {
        panic!("expected a stream");
    };
    assert_eq!(stream.protocol, Protocol::OpenaiChat);
    assert_eq!(stream.header("x-switchyard-model"), Some("up-chat"));
    let output = Output::read(Reply::Stream(stream)).await;
    assert_eq!(
        output.events.last().map(|e| e.data.as_str()),
        Some("[DONE]")
    );
    // An unused import guard for the tool helper.
    let _ = tool_declaration(Protocol::OpenaiChat, "x");
    let _ = Answer::text("x");
}
