//! What clients are told when an upstream refuses the *gateway's*
//! credential, and what the mock provider's scripted refusals leave behind.
//!
//! * No upstream `401` / `403` becomes the client's, whatever the transport
//!   classified it as, on any path (passthrough, translation, streams, raw
//!   side endpoints).
//! * While credentials rest, clients are told that and for how long — not
//!   what the upstream said to the request that ran into the failure. The
//!   operator finds that on the request record.
//! * A mock model that fails on purpose does not switch off the mock models
//!   that work.

// The shared support module re-exports more than this file uses.
#[allow(unused_imports)]
mod support;

use bytes::Bytes;
use http::{HeaderMap, Method};
use serde_json::json;
use support::{Behaviour, FOUR_PROVIDERS, Harness, Output};
use switchyard_core::config::ProviderKind;
use switchyard_core::{FailureClass, Protocol};
use switchyard_gateway::{ClientRequest, RawRequest, WsOpenRequest};
use switchyard_scheduler::{CredentialSnapshot, CredentialStatus};
use switchyard_telemetry::Mode;
use tokio_util::sync::CancellationToken;

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

/// What OpenAI answers a revoked key with; the key is masked by the vendor,
/// so it is not the string the gateway presented (and the transport's
/// scrubber does not recognise it).
const REJECTION: &str = "Incorrect API key provided: sk-proj-********************wxyz. \
                         You can find your API key at https://platform.openai.com/account/api-keys.";

/// What OpenAI answers when the key's project has not been given the model.
const NO_ACCESS: &str = "The model `up-chat` does not exist or your project \
                         `proj_OPERATORS` does not have access to it.";

fn text(output: &Output) -> String {
    if output.streamed {
        output.wire_text()
    } else {
        String::from_utf8_lossy(&output.body).to_string()
    }
}

fn credentials(harness: &Harness, provider: usize) -> Vec<CredentialSnapshot> {
    harness.gateway.scheduler().snapshot()[provider]
        .credentials
        .clone()
}

fn raw_request(harness: &Harness, model: &str) -> RawRequest {
    RawRequest {
        path: "embeddings".to_string(),
        method: Method::POST,
        body: Bytes::from(json!({"input": "hello", "model": model}).to_string()),
        content_type: Some("application/json".to_string()),
        query: None,
        model: model.to_string(),
        headers: HeaderMap::new(),
        identity: harness.identity(),
        client_ip: None,
        endpoint: "POST /v1/embeddings".to_string(),
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

fn count_request(harness: &Harness, model: &str) -> ClientRequest {
    ClientRequest::new(
        Protocol::Anthropic,
        "POST /v1/messages/count_tokens",
        Bytes::from(
            json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}).to_string(),
        ),
        harness.identity(),
    )
}

// ---------------------------------------------------------------------------
// While credentials rest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_rest_is_explained_to_the_operator_not_to_the_client() {
    let harness = Harness::start(TWO_KEYS).await;
    for key in ["key-a", "key-b"] {
        harness.fake.always(key, Behaviour::error(401, REJECTION));
    }
    let first = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(first.status, 502);
    assert_eq!(harness.fake.count(), 2);

    // The client is told what it can act on: nothing serves the model now,
    // and when to come back.
    let later = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(later.status, 429, "{}", text(&later));
    let error = &later.json()["error"];
    assert_eq!(error["code"], "model_cooldown");
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("cooling down"), "{message}");
    assert!(!message.contains("upstream error"), "{message}");
    assert!(!message.contains("Incorrect API key"), "{message}");
    let wait: u64 = later
        .header("retry-after")
        .expect("a rest comes with Retry-After")
        .parse()
        .unwrap();
    assert!(
        wait > 60,
        "the rest of a rejected credential is long: {wait}"
    );
    assert_eq!(
        harness.fake.count(),
        2,
        "resting credentials are not called"
    );

    // The operator is told why.
    let record = harness.record(&later.request_id);
    assert_eq!(record.status, 429);
    assert!(record.attempts.is_empty());
    let recorded = record.error.as_ref().expect("the record says why");
    assert_eq!(recorded.kind, "rate_limit");
    assert!(
        recorded.message.contains("cooling down")
            && recorded.message.contains("last upstream error: 401"),
        "{}",
        recorded.message
    );
}

#[tokio::test]
async fn every_operation_keeps_a_rest_private() {
    let harness = Harness::start(TWO_KEYS).await;
    for key in ["key-a", "key-b"] {
        harness.fake.always(key, Behaviour::error(401, REJECTION));
    }
    assert_eq!(
        harness.ask(Protocol::OpenaiChat, "m", false).await.status,
        502
    );
    let private = |what: &str, said: &str| {
        for secret in ["Incorrect API key", "wxyz", "platform.openai.com"] {
            assert!(!said.contains(secret), "{what} quotes the upstream: {said}");
        }
    };

    let counted = Output::read(
        harness
            .gateway
            .count_tokens(count_request(&harness, "m"))
            .await,
    )
    .await;
    assert_eq!(counted.status, 429);
    private("count_tokens", &text(&counted));

    let raw = Output::read(harness.gateway.raw(raw_request(&harness, "m")).await).await;
    assert_eq!(raw.status, 429);
    private("raw", &text(&raw));

    let refused = harness
        .gateway
        .open_upstream_ws(ws_request(&harness, "m"))
        .await
        .expect_err("nothing can serve the session");
    assert_eq!(refused.status, 429);
    assert!(refused.retry_after_secs.is_some());
    private("open_upstream_ws", &refused.message);

    assert_eq!(
        harness.fake.count(),
        2,
        "resting credentials are not called"
    );
}

/// The failure behind a rest happened to an earlier request — possibly
/// another client's. Its wording is not repeated to whoever asks next, also
/// when it is an ordinary rate limit.
#[tokio::test]
async fn a_rate_limit_rest_does_not_repeat_what_the_upstream_said() {
    let harness = Harness::start(TWO_KEYS).await;
    for key in ["key-a", "key-b"] {
        harness.fake.always(
            key,
            Behaviour::HttpError {
                status: 429,
                message: "Rate limit reached for up-chat in organization org-OPERATORS on \
                          tokens per min"
                    .to_string(),
                retry_after: Some(30),
            },
        );
    }
    let first = harness.ask(Protocol::Anthropic, "m", false).await;
    assert_eq!(first.status, 429);

    for (client, stream) in [(Protocol::Anthropic, false), (Protocol::OpenaiChat, true)] {
        let later = harness.ask(client, "m", stream).await;
        assert_eq!(later.status, 429);
        assert!(!later.streamed);
        assert!(
            !text(&later).contains("org-OPERATORS"),
            "{client}: {}",
            text(&later)
        );
        let wait: u64 = later.header("retry-after").unwrap().parse().unwrap();
        assert!((1..=30).contains(&wait), "{wait}");
        let record = harness.record(&later.request_id);
        assert!(
            record
                .error
                .as_ref()
                .is_some_and(|error| error.message.contains("last upstream error: 429")),
            "{:?}",
            record.error
        );
    }
    assert_eq!(harness.fake.count(), 2);
}

// ---------------------------------------------------------------------------
// The request that runs into a refusal
// ---------------------------------------------------------------------------

/// A `403` the transport files under "model not found" (so the scheduler
/// rests the model, not the key) is still the gateway's credential being
/// refused: a 502 for the client in every protocol, streaming or not, and
/// on a raw side endpoint — with the upstream's words kept for the operator.
#[tokio::test]
async fn a_403_for_the_gateways_project_is_a_502_on_every_path() {
    for (client, stream) in [
        (Protocol::OpenaiChat, false),
        (Protocol::OpenaiChat, true),
        (Protocol::Anthropic, false),
        (Protocol::Gemini, true),
        (Protocol::OpenaiResponses, false),
    ] {
        let harness = Harness::start(TWO_KEYS).await;
        for key in ["key-a", "key-b"] {
            harness.fake.always(key, Behaviour::error(403, NO_ACCESS));
        }
        let output = harness.ask(client, "m", stream).await;
        let label = format!("{client} (stream={stream})");
        assert!(!output.streamed, "{label}");
        assert_eq!(output.status, 502, "{label}: {}", text(&output));
        assert!(
            !text(&output).contains("proj_OPERATORS"),
            "{label}: {}",
            text(&output)
        );
        assert!(
            output
                .error_message(client)
                .contains("the gateway's credential"),
            "{label}: {}",
            text(&output)
        );
        assert_eq!(harness.fake.count(), 2, "{label}: the next key is tried");

        // The scheduler rests the model on each key, not the keys.
        for credential in credentials(&harness, 0) {
            assert_eq!(
                credential.model_cooldowns.len(),
                1,
                "{label}: {credential:?}"
            );
            assert_eq!(
                credential.model_cooldowns[0].reason,
                FailureClass::ModelNotFound,
                "{label}"
            );
            // (The provider's only model rests, which is what makes the
            // credential count as cooling — not a rest of the key itself.)
            assert_eq!(
                credential.cooldown_reason,
                Some(FailureClass::ModelNotFound),
                "{label}"
            );
        }
        // The operator sees what the upstream said.
        let record = harness.record(&output.request_id);
        assert_eq!(record.status, 502, "{label}");
        assert_eq!(record.attempts.len(), 2, "{label}");
        for attempt in &record.attempts {
            assert_eq!(attempt.status, 403, "{label}");
            assert!(
                attempt
                    .error
                    .as_deref()
                    .is_some_and(|said| said.contains("proj_OPERATORS")),
                "{label}: {:?}",
                attempt.error
            );
        }
        assert_eq!(
            record
                .error
                .as_ref()
                .and_then(|error| error.upstream_status),
            Some(403),
            "{label}"
        );
    }

    let harness = Harness::start(TWO_KEYS).await;
    for key in ["key-a", "key-b"] {
        harness.fake.always(key, Behaviour::error(403, NO_ACCESS));
    }
    let raw = Output::read(harness.gateway.raw(raw_request(&harness, "m")).await).await;
    assert_eq!(raw.status, 502, "{}", text(&raw));
    assert!(!text(&raw).contains("proj_OPERATORS"), "{}", text(&raw));
}

/// A `403` that is about something the *request* names — a file uploaded
/// under another project — is the client's to fix: it is told what the
/// upstream said, as a request fault, not as a permission problem of its
/// gateway key. Nothing rests and no other credential is tried.
#[tokio::test]
async fn a_403_about_the_requests_own_file_is_a_400_with_the_explanation() {
    const NO_FILE: &str =
        "You do not have permission to access the File abc123 or it may not exist.";
    for client in [Protocol::Gemini, Protocol::OpenaiChat] {
        let harness = Harness::start(FOUR_PROVIDERS).await;
        harness
            .fake
            .always("key-gemini-1", Behaviour::error(403, NO_FILE));
        let output = harness.ask(client, "m-gemini", false).await;
        assert_eq!(output.status, 400, "{client}: {}", text(&output));
        assert_eq!(output.error_message(client), NO_FILE, "{client}");
        assert_eq!(harness.fake.count(), 1, "{client}");
        let gemini = &credentials(&harness, 3)[0];
        assert_eq!(gemini.status, CredentialStatus::Ready, "{client}");
        assert!(gemini.model_cooldowns.is_empty(), "{client}");
        assert_eq!(gemini.failures, 0, "{client}");
    }
}

/// Other upstream errors are still the upstream's own words for a client of
/// the same protocol (DESIGN.md section 8, step 6).
#[tokio::test]
async fn other_upstream_errors_are_still_forwarded_verbatim() {
    let harness = Harness::start(TWO_KEYS).await;
    for key in ["key-a", "key-b"] {
        harness.fake.always(
            key,
            Behaviour::error(404, "The model `up-chat` does not exist"),
        );
    }
    let output = harness.ask(Protocol::OpenaiChat, "m", false).await;
    assert_eq!(output.status, 404);
    assert_eq!(
        output.json(),
        json!({"error": {
            "message": "The model `up-chat` does not exist",
            "type": "invalid_request_error",
            "param": null,
            "code": "model_not_found"
        }})
    );
}

// ---------------------------------------------------------------------------
// The mock provider
// ---------------------------------------------------------------------------

const MOCK: &str = r#"
[[providers]]
name = "demo"
kind = "mock"
"#;

#[tokio::test]
async fn a_scripted_mock_401_fails_every_time_and_rests_nothing() {
    let harness = Harness::start(MOCK).await;
    for stream in [false, true, false] {
        let output = harness
            .ask(Protocol::OpenaiChat, "mock-error-401", stream)
            .await;
        // The scripted failure itself each time, not "cooling down".
        assert_eq!(output.status, 502, "{}", text(&output));
        assert_eq!(output.json()["error"]["code"], "upstream_auth_error");
        let record = harness.record(&output.request_id);
        assert_eq!(record.mode, Some(Mode::Mock));
        assert_eq!(record.attempts.len(), 1);
        assert_eq!(record.attempts[0].status, 401);
    }
    let credential = &credentials(&harness, 0)[0];
    assert_eq!(credential.status, CredentialStatus::Ready, "{credential:?}");
    assert_eq!(credential.cooldown_until, None);
    assert!(credential.model_cooldowns.is_empty());

    for stream in [false, true] {
        let output = harness.ask(Protocol::Anthropic, "mock-echo", stream).await;
        assert_eq!(output.response(Protocol::Anthropic).text(), "hi");
    }
}

/// The per-model failures are still reported: they rest the failing model
/// (which shows what a cooldown looks like) and nothing else.
#[tokio::test]
async fn the_other_scripted_mock_failures_rest_only_themselves() {
    let harness = Harness::start(MOCK).await;
    for model in ["mock-error-429", "mock-error-500"] {
        let output = harness.ask(Protocol::OpenaiChat, model, false).await;
        assert!(output.status >= 400, "{model}");
    }
    let credential = &credentials(&harness, 0)[0];
    assert_eq!(credential.cooldown_until, None, "{credential:?}");
    let rested: Vec<&str> = credential
        .model_cooldowns
        .iter()
        .map(|cooldown| cooldown.model.as_str())
        .collect();
    assert_eq!(rested, vec!["mock-error-429", "mock-error-500"]);
    assert_eq!(
        harness
            .ask(Protocol::Gemini, "mock-lorem", false)
            .await
            .status,
        200
    );
}

/// The admin's provider test addresses the credential directly; a scripted
/// 401 must not rest it either.
#[tokio::test]
async fn testing_the_mock_provider_with_its_401_model_leaves_it_usable() {
    let harness = Harness::start(MOCK).await;
    let test = harness
        .gateway
        .test_provider("demo", Some("mock-error-401"))
        .await;
    assert!(!test.ok);
    assert_eq!(test.status, 401);
    assert_eq!(test.model.as_deref(), Some("mock-error-401"));
    let credential = &credentials(&harness, 0)[0];
    assert_eq!(credential.status, CredentialStatus::Ready, "{credential:?}");
    assert_eq!(credential.cooldown_until, None);

    let healthy = harness.gateway.test_provider("demo", None).await;
    assert!(healthy.ok, "{healthy:?}");
    assert_eq!(
        harness
            .ask(Protocol::OpenaiResponses, "mock-echo", false)
            .await
            .status,
        200
    );

    // A per-model scripted failure is reported as before.
    let limited = harness
        .gateway
        .test_provider("demo", Some("mock-error-429"))
        .await;
    assert_eq!((limited.ok, limited.status), (false, 429));
    let credential = &credentials(&harness, 0)[0];
    assert_eq!(credential.model_cooldowns.len(), 1, "{credential:?}");
    assert_eq!(credential.cooldown_until, None);
}
