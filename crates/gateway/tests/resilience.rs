//! When things go wrong: failover and cooldowns, final errors, streaming
//! bootstrap retries, in-band stream errors, timeouts and cancellation.

mod support;

use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::{Duration, Instant};
use support::{Answer, Behaviour, Harness, Output, Wire, body};
use switchyard_core::stream::StreamEvent;
use switchyard_core::{ErrorKind, FailureClass, Protocol};
use switchyard_gateway::Reply;
use switchyard_scheduler::{CredentialSnapshot, CredentialStatus};
use switchyard_telemetry::Mode;

/// One Chat provider with two keys, tried in order.
const TWO_KEYS: &str = r#"
[routing]
strategy = "fill-first"

[[providers]]
name = "chat"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-a", "key-b"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#;

/// The same with three keys and room for more attempts.
const THREE_KEYS: &str = r#"
[routing]
strategy = "fill-first"
max_attempts = 5

[[providers]]
name = "chat"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-a", "key-b", "key-c"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#;

fn credentials(harness: &Harness) -> Vec<CredentialSnapshot> {
    harness
        .gateway
        .scheduler()
        .snapshot()
        .into_iter()
        .flat_map(|provider| provider.credentials)
        .collect()
}

async fn dead_address() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

// ---------------------------------------------------------------------------
// Failover
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_429_fails_over_and_rests_the_credential() {
    let harness = Harness::start(TWO_KEYS).await;
    harness.fake.script("key-a", [Behaviour::rate_limited(30)]);
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"]);

    // The scheduler rests the model on the first key for as long as the
    // upstream asked.
    let creds = credentials(&harness);
    assert_eq!(creds[0].model_cooldowns.len(), 1, "{:?}", creds[0]);
    assert_eq!(creds[0].model_cooldowns[0].model, "up-chat");
    assert_eq!(creds[0].model_cooldowns[0].reason, FailureClass::RateLimit);
    assert_eq!(creds[0].failures, 1);
    assert_eq!(creds[0].last_error.as_ref().map(|e| e.status), Some(429));
    assert_eq!(creds[1].successes, 1);
    assert!(creds[1].model_cooldowns.is_empty());

    // While it rests, requests go straight to the second key.
    harness.fake.clear();
    let again = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(again.status, 200);
    assert_eq!(harness.fake.keys(), vec!["key-b"]);

    // The record shows both attempts.
    let record = harness.record(&output.request_id);
    assert!(record.ok);
    assert_eq!(record.attempts.len(), 2);
    assert_eq!(record.attempts[0].status, 429);
    assert!(!record.attempts[0].ok);
    assert!(
        record.attempts[0]
            .error
            .as_deref()
            .unwrap()
            .contains("Rate limit")
    );
    assert_eq!(record.attempts[1].status, 200);
    assert_eq!(record.credential_id, record.attempts[1].credential_id);
    assert_ne!(
        record.attempts[0].credential_id,
        record.attempts[1].credential_id
    );
    assert!(record.error.is_none());
}

#[tokio::test]
async fn failed_attempts_are_announced_on_the_credential_topic() {
    let harness = Harness::start(TWO_KEYS).await;
    let mut events = harness.gateway.telemetry().subscribe();
    harness.fake.script("key-a", [Behaviour::rate_limited(30)]);
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 200);

    let mut credential_events = Vec::new();
    while let Ok(event) = events.try_recv() {
        if event.topic() == "credential" {
            credential_events.push(event.data());
        }
    }
    // One for the failed attempt; successes are not announced.
    assert_eq!(credential_events.len(), 1, "{credential_events:?}");
    let event = &credential_events[0];
    assert_eq!(event["provider"], "chat");
    assert_eq!(
        event["credential"]["id"],
        credentials(&harness)[0].id.as_str()
    );
    assert_eq!(
        event["credential"]["model_cooldowns"][0]["model"],
        "up-chat"
    );
    assert_eq!(event["credential"]["last_error"]["status"], 429);
    assert!(
        !event.to_string().contains("key-a"),
        "no secrets on the bus"
    );
}

#[tokio::test]
async fn a_500_fails_over_and_rests_the_credential() {
    let harness = Harness::start(TWO_KEYS).await;
    harness
        .fake
        .script("key-a", [Behaviour::error(500, "internal error")]);
    let output = harness.ask(Protocol::Anthropic, "m", false).await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"]);
    let creds = credentials(&harness);
    assert_eq!(creds[0].model_cooldowns[0].reason, FailureClass::Server);
}

#[tokio::test]
async fn a_transport_error_fails_over() {
    let config = r#"
[[providers]]
name = "dead"
kind = "openai-compat"
base_url = "{dead}/v1"
api_keys = ["key-dead"]
priority = 10
[[providers.models]]
id = "up-chat"
alias = "m"

[[providers]]
name = "live"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-live"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#
    .replace("{dead}", &dead_address().await);
    let harness = Harness::start(&config).await;
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(harness.fake.keys(), vec!["key-live"]);

    let record = harness.record(&output.request_id);
    assert_eq!(record.attempts.len(), 2);
    assert_eq!(record.attempts[0].provider, "dead");
    assert_eq!(record.attempts[0].status, 0, "no response was received");
    assert!(
        record.attempts[0]
            .error
            .as_deref()
            .unwrap()
            .starts_with("connect:"),
        "{:?}",
        record.attempts[0].error
    );
    assert_eq!(record.provider.as_deref(), Some("live"));
    assert_eq!(output.header("x-switchyard-provider"), Some("live"));
}

#[tokio::test]
async fn a_400_is_returned_at_once_without_failover_or_cooldown() {
    let harness = Harness::start(TWO_KEYS).await;
    harness
        .fake
        .script("key-a", [Behaviour::error(400, "messages: field required")]);
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 400);
    assert_eq!(harness.fake.count(), 1, "no other credential was tried");
    // Same protocol on both sides: the upstream's own error body.
    assert_eq!(
        output.json(),
        support::fake::error_body(Wire::Chat, 400, "messages: field required")
    );

    let creds = credentials(&harness);
    assert_eq!(creds[0].status, CredentialStatus::Ready);
    assert!(creds[0].model_cooldowns.is_empty());
    assert_eq!(
        creds[0].failures, 0,
        "a request fault is not the credential's"
    );
    harness.fake.clear();
    harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(harness.fake.keys(), vec!["key-a"], "the key is still first");

    let record = harness.record(&output.request_id);
    assert_eq!(record.status, 400);
    assert!(!record.ok);
    assert_eq!(record.attempts.len(), 1);
    assert_eq!(record.error.as_ref().unwrap().kind, "invalid_request");
    assert_eq!(record.error.as_ref().unwrap().upstream_status, Some(400));
}

#[tokio::test]
async fn an_upstream_400_is_rendered_in_the_clients_protocol_when_translated() {
    let harness = Harness::start(TWO_KEYS).await;
    harness
        .fake
        .script("key-a", [Behaviour::error(400, "messages: field required")]);
    let output = harness.ask(Protocol::Anthropic, "m", false).await;
    assert_eq!(output.status, 400);
    assert_eq!(harness.fake.count(), 1);
    let body = output.json();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["message"], "messages: field required");

    harness
        .fake
        .script("key-a", [Behaviour::error(400, "bad contents")]);
    let output = harness.ask(Protocol::Gemini, "m", false).await;
    assert_eq!(output.status, 400);
    assert_eq!(output.json()["error"]["status"], "INVALID_ARGUMENT");
    assert_eq!(output.json()["error"]["message"], "bad contents");
}

#[tokio::test]
async fn the_last_upstream_error_decides_the_reply_and_its_retry_after() {
    let harness = Harness::start(TWO_KEYS).await;
    harness.fake.script("key-a", [Behaviour::rate_limited(7)]);
    harness.fake.script("key-b", [Behaviour::rate_limited(9)]);
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 429);
    assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"]);
    assert_eq!(output.header("retry-after"), Some("9"));
    assert_eq!(
        output.json(),
        support::fake::error_body(Wire::Chat, 429, "Rate limit reached"),
        "same protocol: the upstream's body verbatim"
    );
    let record = harness.record(&output.request_id);
    assert_eq!(record.status, 429);
    assert_eq!(record.attempts.len(), 2);
    assert_eq!(record.error.as_ref().unwrap().kind, "rate_limit");

    // Now everything is cooling down: no upstream call, a local 429 with
    // the time until the first credential recovers.
    harness.fake.clear();
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 429);
    assert_eq!(harness.fake.count(), 0);
    let retry: u64 = output.header("retry-after").unwrap().parse().unwrap();
    assert!((1..=7).contains(&retry), "retry-after {retry}");
    assert_eq!(output.json()["error"]["code"], "model_cooldown");
    let record = harness.record(&output.request_id);
    assert!(record.attempts.is_empty());
    assert_eq!(record.provider, None);

    // The same in a translated request: the gateway's rendering, in the
    // client's envelope, still with the header.
    let output = harness.ask(Protocol::Anthropic, "m", false).await;
    assert_eq!(output.status, 429);
    assert_eq!(output.json()["error"]["type"], "rate_limit_error");
    assert!(output.header("retry-after").is_some());
}

#[tokio::test]
async fn a_translated_final_429_keeps_the_upstreams_message_and_retry_after() {
    let harness = Harness::start(TWO_KEYS).await;
    harness.fake.script("key-a", [Behaviour::rate_limited(3)]);
    harness.fake.script("key-b", [Behaviour::rate_limited(5)]);
    let output = harness.ask(Protocol::Anthropic, "m", true).await;
    assert!(
        !output.streamed,
        "nothing was streamed: a plain error reply"
    );
    assert_eq!(output.status, 429);
    assert_eq!(output.header("retry-after"), Some("5"));
    let body = output.json();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert_eq!(body["error"]["message"], "Rate limit reached");
}

#[tokio::test]
async fn a_short_cooldown_is_waited_out_when_max_wait_allows() {
    let config = r#"
[routing]
max_wait_secs = 3

[[providers]]
name = "chat"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-a"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#;
    let harness = Harness::start(config).await;
    harness.fake.script(
        "key-a",
        [Behaviour::rate_limited(1), Behaviour::rate_limited(1)],
    );
    // The request that runs into the limit waits for its only credential
    // and tries again — once: the second refusal is its answer.
    let started = Instant::now();
    let first = harness.ask(Protocol::OpenaiChat, "m", false).await;
    let waited = started.elapsed();
    assert_eq!(first.status, 429, "{:?}", first.body);
    assert_eq!(first.header("retry-after"), Some("1"));
    assert!(waited >= Duration::from_millis(800), "waited {waited:?}");
    assert!(waited < Duration::from_secs(2), "waited {waited:?}");
    assert_eq!(harness.fake.count(), 2, "one wait, one retry");
    assert_eq!(harness.record(&first.request_id).attempts.len(), 2);

    // The next request finds the credential resting for about a second,
    // which is within `max_wait_secs`: it waits and is served.
    let started = Instant::now();
    let second = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(second.status, 200, "{:?}", second.body);
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(400), "waited {waited:?}");
    assert!(waited < Duration::from_secs(3), "waited {waited:?}");
    assert_eq!(harness.fake.count(), 3);
}

#[tokio::test]
async fn a_cooldown_longer_than_max_wait_is_not_waited_for() {
    let config = r#"
[routing]
max_wait_secs = 1

[[providers]]
name = "chat"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-a"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#;
    let harness = Harness::start(config).await;
    harness.fake.script("key-a", [Behaviour::rate_limited(30)]);
    let started = Instant::now();
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 429);
    assert_eq!(output.header("retry-after"), Some("30"));
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(harness.fake.count(), 1);
}

#[tokio::test]
async fn a_request_fault_is_never_waited_out() {
    let config = r#"
[routing]
max_wait_secs = 3

[[providers]]
name = "chat"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-a"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#;
    let harness = Harness::start(config).await;
    harness
        .fake
        .script("key-a", [Behaviour::error(400, "bad request")]);
    let started = Instant::now();
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 400);
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(harness.fake.count(), 1);
}

#[tokio::test]
async fn the_wait_for_a_cooldown_ends_when_the_client_leaves() {
    let config = r#"
[routing]
max_wait_secs = 30

[[providers]]
name = "chat"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-a"]
[[providers.models]]
id = "up-chat"
alias = "m"
"#;
    let harness = Harness::start(config).await;
    harness.fake.script("key-a", [Behaviour::rate_limited(20)]);
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut request = harness.request(Protocol::OpenaiChat, "m", false);
    request.cancel = cancel.clone();
    let gateway = harness.gateway.clone();
    let pending = tokio::spawn(async move { gateway.generate(request).await });
    support::eventually("the first attempt was refused", || {
        credentials(&harness)[0].failures == 1
    })
    .await;
    cancel.cancel();
    let output = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .expect("the wait is abandoned at once")
        .unwrap();
    let output = Output::read(output).await;
    assert_eq!(output.status, 499);
    let record = harness.record(&output.request_id);
    assert_eq!(record.attempts.len(), 1);
    assert_eq!(record.attempts[0].status, 429);
    assert_eq!(harness.fake.count(), 1);
}

#[tokio::test]
async fn without_max_wait_a_cooling_model_fails_immediately() {
    let config = TWO_KEYS.replace(
        "api_keys = [\"key-a\", \"key-b\"]",
        "api_keys = [\"key-a\"]",
    );
    let harness = Harness::start(&config).await;
    harness.fake.script("key-a", [Behaviour::rate_limited(1)]);
    harness.ask(Protocol::OpenaiChat, "m", false).await;
    let started = Instant::now();
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 429);
    assert!(started.elapsed() < Duration::from_millis(300));
    assert_eq!(harness.fake.count(), 1);
}

#[tokio::test]
async fn an_upstream_401_is_a_502_for_the_client() {
    let harness = Harness::start(TWO_KEYS).await;
    for key in ["key-a", "key-b"] {
        harness.fake.script(
            key,
            [Behaviour::error(401, "Incorrect API key provided: key-…")],
        );
    }
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 502);
    assert_eq!(
        harness.fake.keys(),
        vec!["key-a", "key-b"],
        "auth failures fail over"
    );
    let text = String::from_utf8_lossy(&output.body).to_string();
    assert!(!text.contains("Incorrect API key"), "{text}");
    assert!(!text.contains("invalid_api_key"), "{text}");
    assert!(
        output.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("rejected the gateway's credential"),
        "{text}"
    );

    // Both credentials are out of rotation as a whole.
    for credential in credentials(&harness) {
        assert_eq!(credential.status, CredentialStatus::Cooling);
        assert_eq!(credential.cooldown_reason, Some(FailureClass::Auth));
    }
    let record = harness.record(&output.request_id);
    assert_eq!(record.status, 502);
    assert_eq!(record.error.as_ref().unwrap().upstream_status, Some(401));
}

#[tokio::test]
async fn a_200_that_is_not_json_is_a_failed_attempt() {
    let harness = Harness::start(TWO_KEYS).await;
    harness.fake.script("key-a", [Behaviour::Garbage]);
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 200);
    assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"]);

    // Translation path too, and when it was the last attempt: 502.
    let harness = Harness::start(TWO_KEYS).await;
    harness.fake.script("key-a", [Behaviour::Garbage]);
    harness.fake.script("key-b", [Behaviour::Garbage]);
    let output = harness.ask(Protocol::Gemini, "m", false).await;
    assert_eq!(output.status, 502);
    assert_eq!(output.json()["error"]["code"], 502);
    assert_eq!(harness.fake.count(), 2);
}

#[tokio::test]
async fn max_attempts_bounds_the_loop() {
    let config = THREE_KEYS.replace("max_attempts = 5", "max_attempts = 2");
    let harness = Harness::start(&config).await;
    for key in ["key-a", "key-b", "key-c"] {
        harness.fake.always(key, Behaviour::error(500, "down"));
    }
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 500, "verbatim upstream status");
    assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"]);
    assert_eq!(harness.record(&output.request_id).attempts.len(), 2);
}

#[tokio::test]
async fn malformed_client_requests_are_400_in_the_clients_envelope() {
    let harness = Harness::start(TWO_KEYS).await;
    for (protocol, raw) in [
        (Protocol::OpenaiChat, &b""[..]),
        (Protocol::OpenaiChat, &b"   "[..]),
        (Protocol::Anthropic, &b"{not json"[..]),
        (Protocol::OpenaiResponses, &b"{\"input\": \"no model\"}"[..]),
        (Protocol::OpenaiChat, &b"[1, 2]"[..]),
    ] {
        let mut request = harness.request(protocol, "m", false);
        request.body = bytes::Bytes::copy_from_slice(raw);
        let output = Output::read(harness.gateway.generate(request).await).await;
        assert_eq!(output.status, 400, "{protocol}: {raw:?}");
        assert!(!output.error_message(protocol).is_empty());
        let record = harness.record(&output.request_id);
        assert_eq!(record.status, 400);
        assert_eq!(record.error.as_ref().unwrap().kind, "invalid_request");
        assert!(record.attempts.is_empty());
    }
    assert_eq!(harness.fake.count(), 0);
    assert_eq!(harness.gateway.telemetry().gauges().in_flight(), 0);

    // A body the client's own codec cannot decode fails the translation
    // path with a 400, not a failed attempt.
    let request = harness.request_with(
        Protocol::Anthropic,
        json!({"model": "m", "max_tokens": 5, "messages": "not a list"}),
        "m",
        false,
    );
    let output = Output::read(harness.gateway.generate(request).await).await;
    assert_eq!(output.status, 400, "{:?}", output.body);
    assert_eq!(harness.fake.count(), 0);
    assert!(credentials(&harness).iter().all(|c| c.failures == 0));
}

#[tokio::test]
async fn an_oversized_upstream_response_is_a_502() {
    let config = format!("[server]\nbody_limit_mb = 1\n{TWO_KEYS}");
    let harness = Harness::start(&config).await;
    let huge = "x".repeat(2 * 1024 * 1024);
    harness.fake.script("key-a", [Behaviour::text(&huge)]);
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 502);
    assert_eq!(
        output.json()["error"]["code"],
        "upstream_response_too_large"
    );
    assert_eq!(
        harness.fake.count(),
        1,
        "the same answer would be too large again"
    );
    // The credential answered; nothing is held against it.
    assert!(credentials(&harness)[0].model_cooldowns.is_empty());
}

#[tokio::test]
async fn many_requests_at_once() {
    let harness = std::sync::Arc::new(Harness::start(support::FOUR_PROVIDERS).await);
    let mut tasks = Vec::new();
    for index in 0..48usize {
        let harness = harness.clone();
        tasks.push(tokio::spawn(async move {
            let client = support::PROTOCOLS[index % 4];
            let upstream = support::PROTOCOLS[(index / 4) % 4];
            let (model, _, _) = support::route(upstream);
            let output = harness.ask(client, model, index % 2 == 0).await;
            output.response(client).text()
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), "Hello from the fake upstream");
    }
    support::eventually("gauges return to zero", || {
        let gauges = harness.gateway.telemetry().gauges();
        gauges.in_flight() == 0 && gauges.active_streams() == 0
    })
    .await;
    assert_eq!(harness.gateway.telemetry().gauges().totals().requests, 48);
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stream_that_fails_before_its_first_event_is_retried_elsewhere() {
    for (name, broken) in [
        ("dies", Behaviour::DieBeforeFirstEvent),
        ("empty", Behaviour::EmptyStream),
        (
            "error event",
            Behaviour::StreamError {
                status: 500,
                message: "The server had an error".to_string(),
            },
        ),
    ] {
        for client in [Protocol::OpenaiChat, Protocol::Anthropic] {
            let label = format!("{name}, client {client}");
            let harness = Harness::start(TWO_KEYS).await;
            harness.fake.script("key-a", [broken.clone()]);
            let output = harness.ask(client, "m", true).await;
            assert!(output.streamed, "{label}: {:?}", output.body);
            assert_eq!(
                output.response(client).text(),
                "Hello from the fake upstream",
                "{label}"
            );
            assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"], "{label}");

            let record = harness.record(&output.request_id);
            assert!(record.ok, "{label}: {record:?}");
            assert_eq!(record.attempts.len(), 2, "{label}");
            assert!(!record.attempts[0].ok, "{label}");
            assert!(record.attempts[1].ok, "{label}");
            // The failed attempt was reported: the first key rests.
            let creds = credentials(&harness);
            assert_eq!(creds[0].failures, 1, "{label}");
            assert_eq!(creds[0].model_cooldowns.len(), 1, "{label}");
        }
    }
}

#[tokio::test]
async fn an_in_stream_rate_limit_is_classified_like_an_http_429() {
    let harness = Harness::start(TWO_KEYS).await;
    harness.fake.script(
        "key-a",
        [Behaviour::StreamError {
            status: 429,
            message: "Rate limit reached".to_string(),
        }],
    );
    let output = harness.ask(Protocol::OpenaiChat, "m", true).await;
    assert!(output.streamed);
    assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"]);
    let creds = credentials(&harness);
    assert_eq!(creds[0].model_cooldowns[0].reason, FailureClass::RateLimit);
}

#[tokio::test]
async fn bootstrap_retries_limit_in_stream_failures() {
    let config = format!("[streaming]\nbootstrap_retries = 1\n{THREE_KEYS}");
    let harness = Harness::start(&config).await;
    for key in ["key-a", "key-b", "key-c"] {
        harness.fake.always(key, Behaviour::DieBeforeFirstEvent);
    }
    let output = harness.ask(Protocol::OpenaiChat, "m", true).await;
    assert!(!output.streamed, "the failure is a plain error reply");
    assert_eq!(output.status, 502, "{:?}", output.body);
    // One attempt plus one bootstrap retry, although max_attempts is 5.
    assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"]);

    // HTTP-level failures are not bootstrap failures: with every key
    // answering 500 all three are tried.
    let harness = Harness::start(&config).await;
    for key in ["key-a", "key-b", "key-c"] {
        harness.fake.always(key, Behaviour::error(500, "down"));
    }
    let output = harness.ask(Protocol::OpenaiChat, "m", true).await;
    assert_eq!(output.status, 500);
    assert_eq!(harness.fake.count(), 3);
}

#[tokio::test]
async fn a_stream_that_dies_after_its_first_event_ends_in_band_without_retry() {
    for client in [Protocol::OpenaiChat, Protocol::Anthropic] {
        let harness = Harness::start(TWO_KEYS).await;
        harness.fake.script(
            "key-a",
            [Behaviour::DieAfter {
                frames: 3,
                answer: Answer::text("this answer is cut short"),
            }],
        );
        let output = harness.ask(client, "m", true).await;
        assert!(output.streamed, "{client}: the stream had started");
        assert_eq!(harness.fake.keys(), vec!["key-a"], "{client}: no retry");

        // The client got the beginning and then an error in its own
        // protocol's stream shape.
        let events = output.canonical(client);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(!text.is_empty(), "{client}: {events:?}");
        assert!(
            "this answer is cut short".starts_with(&text),
            "{client}: {text}"
        );
        match events.last() {
            Some(StreamEvent::Error(error)) => {
                assert_eq!(error.kind, ErrorKind::Upstream, "{client}: {error:?}");
            }
            other => panic!("{client}: expected a terminal error, got {other:?}"),
        }

        let record = harness.record(&output.request_id);
        assert_eq!(record.status, 200, "{client}: headers were already sent");
        assert!(!record.ok, "{client}");
        assert!(record.error.is_some(), "{client}");
        assert_eq!(record.attempts.len(), 1, "{client}");
        // The failure was reported.
        let creds = credentials(&harness);
        assert_eq!(creds[0].failures, 1, "{client}");
        assert_eq!(creds[1].requests, 0, "{client}");
        assert_eq!(harness.gateway.telemetry().gauges().active_streams(), 0);
    }
}

#[tokio::test]
async fn a_stream_the_upstream_cuts_short_is_reported_to_the_client() {
    // The upstream closes the connection cleanly, but before its terminal
    // event. Passthrough (Chat) and translation (Anthropic) alike must not
    // let that look like a complete answer.
    for client in [Protocol::OpenaiChat, Protocol::Anthropic] {
        let harness = Harness::start(TWO_KEYS).await;
        harness.fake.script(
            "key-a",
            [Behaviour::EndAfter {
                frames: 3,
                answer: Answer::text("this answer is cut short"),
            }],
        );
        let output = harness.ask(client, "m", true).await;
        assert!(output.streamed, "{client}");
        assert_eq!(harness.fake.count(), 1, "{client}: no retry");
        let events = output.canonical(client);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::TextDelta { .. })),
            "{client}: {events:?}"
        );
        let failed = match events.last() {
            Some(StreamEvent::Error(_)) => true,
            Some(StreamEvent::Finish { reason, .. }) => {
                *reason == switchyard_core::ir::FinishReason::Error
            }
            _ => false,
        };
        assert!(
            failed,
            "{client}: the stream must not end normally: {events:?}"
        );
        let record = harness.record(&output.request_id);
        assert!(!record.ok, "{client}");
        assert_eq!(credentials(&harness)[0].failures, 1, "{client}");
    }
}

#[tokio::test]
async fn a_silent_upstream_is_a_failed_attempt_before_the_first_event() {
    let config = format!("[streaming]\nidle_timeout_secs = 1\n{TWO_KEYS}");
    let harness = Harness::start(&config).await;
    harness.fake.script("key-a", [Behaviour::Silence]);
    let started = Instant::now();
    let output = harness.ask(Protocol::OpenaiChat, "m", true).await;
    assert!(output.streamed, "{:?}", output.body);
    assert_eq!(
        output.response(Protocol::OpenaiChat).text(),
        "Hello from the fake upstream"
    );
    assert_eq!(harness.fake.keys(), vec!["key-a", "key-b"]);
    assert!(started.elapsed() >= Duration::from_millis(900));
    let record = harness.record(&output.request_id);
    assert_eq!(record.attempts[0].status, 408);
    // The abandoned upstream connection was closed.
    assert!(
        harness
            .fake
            .wait_for_disconnects(1, Duration::from_secs(3))
            .await
    );
}

#[tokio::test]
async fn silence_after_the_first_event_ends_the_stream_with_a_timeout_error() {
    let config = format!("[streaming]\nidle_timeout_secs = 1\n{TWO_KEYS}");
    let harness = Harness::start(&config).await;
    harness.fake.script(
        "key-a",
        [Behaviour::SilenceAfter {
            frames: 3,
            answer: Answer::text("then nothing more"),
        }],
    );
    let output = harness.ask(Protocol::Anthropic, "m", true).await;
    assert!(output.streamed);
    assert_eq!(harness.fake.count(), 1, "no retry after the first event");
    match output.canonical(Protocol::Anthropic).last() {
        Some(StreamEvent::Error(error)) => {
            assert_eq!(error.kind, ErrorKind::Timeout, "{error:?}");
        }
        other => panic!("expected a timeout error, got {other:?}"),
    }
    let record = harness.record(&output.request_id);
    assert!(!record.ok);
    assert_eq!(record.error.as_ref().unwrap().kind, "timeout");
    assert!(
        harness
            .fake
            .wait_for_disconnects(1, Duration::from_secs(3))
            .await
    );
}

#[tokio::test]
async fn a_slow_first_byte_is_waited_for() {
    let harness = Harness::start(TWO_KEYS).await;
    harness.fake.script(
        "key-a",
        [Behaviour::Slow {
            delay: Duration::from_millis(300),
            then: Box::new(Behaviour::text("worth the wait")),
        }],
    );
    let output = harness.ask(Protocol::OpenaiChat, "m", true).await;
    assert_eq!(
        output.response(Protocol::OpenaiChat).text(),
        "worth the wait"
    );
    assert_eq!(harness.fake.keys(), vec!["key-a"]);
    let record = harness.record(&output.request_id);
    assert!(record.ttfb_ms.unwrap() >= 250, "{:?}", record.ttfb_ms);
    assert!(record.duration_ms >= record.ttfb_ms.unwrap());
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

async fn open_stream(
    harness: &Harness,
    cancel: tokio_util::sync::CancellationToken,
) -> switchyard_gateway::StreamReply {
    harness.fake.script(
        "key-a",
        [Behaviour::SilenceAfter {
            frames: 3,
            answer: Answer::text("the client will leave before this ends"),
        }],
    );
    let mut request = harness.request(Protocol::OpenaiChat, "m", true);
    request.cancel = cancel;
    match harness.gateway.generate(request).await {
        Reply::Stream(stream) => stream,
        Reply::Full(full) => panic!("expected a stream, got {full:?}"),
    }
}

#[tokio::test]
async fn cancelling_mid_stream_drops_the_upstream_connection_and_records_the_request() {
    let harness = Harness::start(TWO_KEYS).await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut stream = open_stream(&harness, cancel.clone()).await;
    let first = stream.events.recv().await.expect("a first event");
    assert!(first.data.contains("chat.completion.chunk"));
    assert_eq!(harness.gateway.telemetry().gauges().active_streams(), 1);
    assert_eq!(harness.gateway.telemetry().gauges().in_flight(), 1);

    cancel.cancel();
    assert!(
        harness
            .fake
            .wait_for_disconnects(1, Duration::from_secs(3))
            .await,
        "the upstream connection must be dropped"
    );
    // The channel closes once the request is settled.
    while stream.events.recv().await.is_some() {}
    let record = harness.record(&stream.request_id);
    assert_eq!(record.status, 499);
    assert!(!record.ok);
    assert_eq!(record.error.as_ref().unwrap().kind, "client_disconnect");
    assert_eq!(record.attempts.len(), 1);
    assert_eq!(record.mode, Some(Mode::Passthrough));
    let gauges = harness.gateway.telemetry().gauges();
    assert_eq!((gauges.in_flight(), gauges.active_streams()), (0, 0));
    // Leaving is not held against the credential.
    let creds = credentials(&harness);
    assert_eq!(creds[0].failures, 0);
    assert!(creds[0].model_cooldowns.is_empty());
}

#[tokio::test]
async fn dropping_the_receiver_cancels_the_upstream_call_too() {
    let harness = Harness::start(TWO_KEYS).await;
    let mut stream = open_stream(&harness, tokio_util::sync::CancellationToken::new()).await;
    stream.events.recv().await.expect("a first event");
    let request_id = stream.request_id.clone();
    drop(stream);
    assert!(
        harness
            .fake
            .wait_for_disconnects(1, Duration::from_secs(3))
            .await
    );
    support::eventually("the request is recorded", || {
        harness
            .gateway
            .telemetry()
            .usage()
            .get(&request_id)
            .is_some()
    })
    .await;
    assert_eq!(harness.record(&request_id).status, 499);
    support::eventually("gauges return to zero", || {
        let gauges = harness.gateway.telemetry().gauges();
        gauges.in_flight() == 0 && gauges.active_streams() == 0
    })
    .await;
}

#[tokio::test]
async fn a_client_that_leaves_with_the_whole_response_was_served() {
    let harness = Harness::start(TWO_KEYS).await;
    // Every event of the answer, then the upstream keeps the connection
    // open. The client stops reading at the terminal event, as many do.
    harness.fake.script(
        "key-a",
        [Behaviour::SilenceAfter {
            frames: usize::MAX,
            answer: Answer::text("complete answer"),
        }],
    );
    let reply = harness
        .gateway
        .generate(harness.request(Protocol::OpenaiChat, "m", true))
        .await;
    let Reply::Stream(mut stream) = reply else {
        panic!("expected a stream");
    };
    let mut text = String::new();
    while let Some(event) = stream.events.recv().await {
        if event.is_done_marker() {
            break;
        }
        text.push_str(&event.data);
    }
    assert!(text.contains("e answer"), "{text}");
    let request_id = stream.request_id.clone();
    drop(stream);

    assert!(
        harness
            .fake
            .wait_for_disconnects(1, Duration::from_secs(3))
            .await
    );
    support::eventually("the request is recorded", || {
        harness
            .gateway
            .telemetry()
            .usage()
            .get(&request_id)
            .is_some()
    })
    .await;
    let record = harness.record(&request_id);
    assert_eq!(record.status, 200, "{record:?}");
    assert!(record.ok);
    assert_eq!(record.error, None);
    assert_eq!(record.usage.output_tokens, 7);
    assert_eq!(credentials(&harness)[0].successes, 1);
}

#[tokio::test]
async fn cancelling_a_pending_call_records_a_499_and_blames_nobody() {
    let harness = Harness::start(TWO_KEYS).await;
    harness.fake.script(
        "key-a",
        [Behaviour::Slow {
            delay: Duration::from_secs(20),
            then: Box::new(Behaviour::text("never seen")),
        }],
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut request = harness.request(Protocol::OpenaiChat, "m", false);
    request.cancel = cancel.clone();
    let gateway = harness.gateway.clone();
    let pending = tokio::spawn(async move { gateway.generate(request).await });
    support::eventually("the upstream call is in progress", || {
        harness.fake.count() == 1
    })
    .await;
    cancel.cancel();
    let output = Output::read(pending.await.unwrap()).await;
    assert_eq!(output.status, 499);
    assert_eq!(harness.fake.count(), 1, "no failover after a cancellation");

    let record = harness.record(&output.request_id);
    assert_eq!(record.status, 499);
    assert_eq!(record.error.as_ref().unwrap().kind, "client_disconnect");
    let creds = credentials(&harness);
    assert_eq!(creds[0].failures, 0);
    assert_eq!(creds[0].status, CredentialStatus::Ready);
    assert_eq!(harness.gateway.telemetry().gauges().in_flight(), 0);
}

#[tokio::test]
async fn dropping_the_request_future_still_records_the_request() {
    let harness = Harness::start(TWO_KEYS).await;
    harness.fake.script(
        "key-a",
        [Behaviour::Slow {
            delay: Duration::from_secs(20),
            then: Box::new(Behaviour::text("never seen")),
        }],
    );
    let mut request = harness.request(Protocol::OpenaiChat, "m", false);
    request.request_id = Some("req-dropped-1".to_string());
    let gateway = harness.gateway.clone();
    let pending = tokio::spawn(async move { gateway.generate(request).await });
    support::eventually("the upstream call is in progress", || {
        harness.fake.count() == 1
    })
    .await;
    pending.abort();
    let _ = pending.await;
    support::eventually("the request is recorded", || {
        harness
            .gateway
            .telemetry()
            .usage()
            .get("req-dropped-1")
            .is_some()
    })
    .await;
    let record = harness.record("req-dropped-1");
    assert_eq!(record.status, 499);
    // The call that was in progress is named: it may be billed.
    assert_eq!(record.provider.as_deref(), Some("chat"));
    assert_eq!(record.upstream_model.as_deref(), Some("up-chat"));
    assert!(record.credential_id.is_some());
    assert_eq!(harness.gateway.telemetry().gauges().in_flight(), 0);
}

#[tokio::test]
async fn a_request_body_for_the_wrong_protocol_never_reaches_an_upstream() {
    let harness = Harness::start(TWO_KEYS).await;
    // A Chat body on the Gemini route: the Gemini codec finds no model in
    // the URL or the body it understands.
    let mut request = harness.request(Protocol::Gemini, "m", false);
    request.path_model = None;
    request.body = bytes::Bytes::from(body(Protocol::Anthropic, "", false, false).to_string());
    let output = Output::read(harness.gateway.generate(request).await).await;
    assert_eq!(output.status, 400);
    assert_eq!(output.json()["error"]["status"], "INVALID_ARGUMENT");
    assert_eq!(harness.fake.count(), 0);
}
