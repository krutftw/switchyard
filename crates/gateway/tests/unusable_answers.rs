//! An upstream that answers `200` with something that is not an answer.
//!
//! * The provider test reads the body: a generation the upstream reports as
//!   failed, an error envelope, or a web page is not a passed test, and is
//!   charged to the credential like the same answer to a request would be.
//! * What the gateway says about a body it could not use never contains the
//!   credential the upstream was called with, should the body quote it.

#[allow(unused_imports)]
mod support;

use pretty_assertions::assert_eq;
use serde_json::json;
use support::{Behaviour, FOUR_PROVIDERS, Harness, PROTOCOLS};
use switchyard_core::{FailureClass, Protocol};
use switchyard_scheduler::{CredentialSnapshot, CredentialStatus};

/// An upstream key that does not look like one (a self-hosted server's
/// shared secret): nothing can recognise it by its shape, only the gateway
/// knows what it called the upstream with.
const UPSTREAM_KEY: &str = "house-inference-passphrase-20260901";

/// One provider per upstream protocol, all with [`UPSTREAM_KEY`].
fn leaky_providers() -> String {
    FOUR_PROVIDERS
        .replace("key-chat-1", UPSTREAM_KEY)
        .replace("key-responses-1", UPSTREAM_KEY)
        .replace("key-anthropic-1", UPSTREAM_KEY)
        .replace("key-gemini-1", UPSTREAM_KEY)
}

fn credential(harness: &Harness, provider: &str) -> CredentialSnapshot {
    harness
        .gateway
        .scheduler()
        .snapshot()
        .into_iter()
        .find(|entry| entry.name == provider)
        .unwrap_or_else(|| panic!("no provider {provider}"))
        .credentials
        .remove(0)
}

#[tokio::test]
async fn a_failed_generation_fails_the_provider_test_whatever_it_had_produced() {
    let reasoning = json!([{"type": "reasoning", "id": "rs_1", "summary": [],
        "encrypted_content": "gAAAAABo-encrypted-reasoning-0123456789"}]);
    for (output, code, status, class) in [
        (
            json!([]),
            "rate_limit_exceeded",
            429,
            FailureClass::RateLimit,
        ),
        (
            reasoning.clone(),
            "rate_limit_exceeded",
            429,
            FailureClass::RateLimit,
        ),
        (reasoning.clone(), "server_error", 502, FailureClass::Server),
        (reasoning, "insufficient_quota", 429, FailureClass::Quota),
    ] {
        let label = format!("{code}, output {output}");
        let harness = Harness::start(FOUR_PROVIDERS).await;
        harness.fake.always(
            "key-responses-1",
            Behaviour::Json {
                status: 200,
                body: json!({
                    "id": "resp_failed1", "object": "response", "status": "failed",
                    "model": "up-responses", "output": output,
                    "error": {"code": code, "message": "The generation did not finish."},
                    "usage": null
                }),
            },
        );
        let test = harness.gateway.test_provider("responses", None).await;
        assert!(!test.ok, "{label}: {test:?}");
        // The status the failure amounts to, not the `200` it came with.
        assert_eq!(test.status, status, "{label}");
        assert_eq!(
            test.error.as_deref(),
            Some("The generation did not finish."),
            "{label}"
        );
        assert_eq!(test.model.as_deref(), Some("up-responses"), "{label}");

        let credential = credential(&harness, "responses");
        assert_eq!(
            (credential.successes, credential.failures),
            (0, 1),
            "{label}: {credential:?}"
        );
        let rested = credential
            .cooldown_reason
            .or_else(|| credential.model_cooldowns.first().map(|rest| rest.reason));
        assert_eq!(rested, Some(class), "{label}: {credential:?}");
    }
}

#[tokio::test]
async fn a_failed_generation_reported_as_a_request_fault_fails_the_test_and_rests_nothing() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    harness.fake.always(
        "key-responses-1",
        Behaviour::failed_response("invalid_prompt", "Flagged."),
    );
    let test = harness.gateway.test_provider("responses", None).await;
    assert_eq!((test.ok, test.status), (false, 400), "{test:?}");
    let credential = credential(&harness, "responses");
    assert_eq!(credential.status, CredentialStatus::Ready);
    assert!(credential.model_cooldowns.is_empty(), "{credential:?}");
}

#[tokio::test]
async fn a_200_that_is_not_a_response_fails_the_provider_test() {
    for (provider, upstream) in [
        ("chat", Protocol::OpenaiChat),
        ("responses", Protocol::OpenaiResponses),
        ("anthropic", Protocol::Anthropic),
        ("gemini", Protocol::Gemini),
    ] {
        let (_, _, key) = support::route(upstream);
        for behaviour in [
            // The web page behind a mistyped base URL.
            Behaviour::Garbage,
            // JSON, but nothing a request could be served with.
            Behaviour::Json {
                status: 200,
                body: json!({"error": {"message": "Service is being deployed.", "code": 503}}),
            },
            Behaviour::Json {
                status: 200,
                body: json!(["not", "a", "response"]),
            },
        ] {
            let label = format!("{provider}, {behaviour:?}");
            let harness = Harness::start(FOUR_PROVIDERS).await;
            harness.fake.always(key, behaviour);
            let test = harness.gateway.test_provider(provider, None).await;
            assert!(!test.ok, "{label}: {test:?}");
            assert_eq!(test.status, 502, "{label}");
            assert!(
                !test.error.as_deref().unwrap_or("").trim().is_empty(),
                "{label}"
            );
            // Charged like the same answer to a translated request: the
            // model rests on this credential.
            let credential = credential(&harness, provider);
            assert_eq!(
                (credential.successes, credential.failures),
                (0, 1),
                "{label}: {credential:?}"
            );
            assert_eq!(
                credential.model_cooldowns.first().map(|rest| rest.reason),
                Some(FailureClass::Server),
                "{label}"
            );

            // An answer that is one passes again and ends the rest.
            harness.fake.always(key, Behaviour::text("pong"));
            let test = harness.gateway.test_provider(provider, None).await;
            assert!(test.ok, "{label}: {test:?}");
            assert_eq!(test.status, 200, "{label}");
            assert!(
                credential_is_ready(&harness, provider),
                "{label}: the passed test did not end the rest"
            );
        }
    }
}

fn credential_is_ready(harness: &Harness, provider: &str) -> bool {
    let credential = credential(harness, provider);
    credential.status == CredentialStatus::Ready && credential.model_cooldowns.is_empty()
}

/// The error envelope a careless upstream answers `200` with, quoting the
/// key it was called with.
fn quoting_envelope() -> Behaviour {
    Behaviour::Json {
        status: 200,
        body: json!({"error": {
            "message": format!("Incorrect API key provided: {UPSTREAM_KEY}."),
            "type": "invalid_request_error"
        }}),
    }
}

/// A request whose upstream answers `200` with an error envelope instead
/// of a response learns what the upstream said — a translated request as
/// the reason its answer was unusable, a passthrough request as the body
/// itself — without the gateway's upstream credential, wherever the words
/// end up.
#[tokio::test]
async fn an_unusable_answer_is_described_without_the_upstream_credential() {
    for upstream in PROTOCOLS {
        let (model, _, _) = support::route(upstream);
        for client in PROTOCOLS {
            let label = format!("{client} -> {upstream}");
            let harness = Harness::start(&leaky_providers()).await;
            harness.fake.always(UPSTREAM_KEY, quoting_envelope());
            let output = harness.ask(client, model, false).await;
            let told = String::from_utf8_lossy(&output.body).into_owned();
            assert!(!told.contains(UPSTREAM_KEY), "{label}: {told}");
            assert!(
                told.contains("Incorrect API key provided"),
                "{label}: {told}"
            );
            if client == upstream && client != Protocol::OpenaiResponses {
                // Passthrough hands the upstream's body on, as valid JSON
                // still. (A Responses body that carries nothing but an
                // error is a failed attempt, see `failed_responses.rs`.)
                assert_eq!(output.status, 200, "{label}");
                assert_eq!(
                    output.json()["error"]["message"],
                    "Incorrect API key provided: [redacted].",
                    "{label}"
                );
                assert_eq!(output.json()["error"]["type"], "invalid_request_error");
            } else {
                assert!(output.status >= 400, "{label}: {told}");
            }
            let record = harness.record(&output.request_id);
            let recorded = format!("{:?} {:?}", record.error, record.attempts);
            assert!(!recorded.contains(UPSTREAM_KEY), "{label}: {recorded}");
        }
    }
}

/// Content is never rewritten: an answer that happens to contain the
/// upstream key's characters — self-hosted servers use keys such as
/// `ollama` — reaches a passthrough client as the upstream wrote it.
#[tokio::test]
async fn a_real_answer_is_forwarded_as_it_is() {
    for protocol in PROTOCOLS {
        let (model, _, _) = support::route(protocol);
        let harness = Harness::start(&leaky_providers()).await;
        harness.fake.always(
            UPSTREAM_KEY,
            Behaviour::text(&format!("The word is {UPSTREAM_KEY}.")),
        );
        let output = harness.ask(protocol, model, false).await;
        assert_eq!(output.status, 200, "{protocol}: {:?}", output.body);
        assert_eq!(
            output.response(protocol).text(),
            format!("The word is {UPSTREAM_KEY}."),
            "{protocol}"
        );
    }
}

#[tokio::test]
async fn the_provider_test_describes_an_unusable_answer_without_the_credential() {
    for provider in ["chat", "responses", "anthropic", "gemini"] {
        let harness = Harness::start(&leaky_providers()).await;
        harness.fake.always(UPSTREAM_KEY, quoting_envelope());
        let test = harness.gateway.test_provider(provider, None).await;
        assert!(!test.ok, "{provider}: {test:?}");
        let said = serde_json::to_string(&test).unwrap();
        assert!(!said.contains(UPSTREAM_KEY), "{provider}: {said}");
        let credential = credential(&harness, provider);
        let kept = format!("{credential:?}");
        assert!(!kept.contains(UPSTREAM_KEY), "{provider}: {kept}");
    }
}
