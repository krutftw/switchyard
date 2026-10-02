//! Reasoning summaries on a Responses upstream whose organisation is not
//! verified.
//!
//! A request translated for a Responses upstream asks for
//! `reasoning.summary` when the client asked for reasoning. OpenAI refuses
//! that field to organisations it has not verified (`400`, `param:
//! "reasoning.summary"`). The gateway wrote the field, so the gateway takes
//! it back: one repeat on the same credential without it, nothing held
//! against the credential, and the provider remembered so the next
//! translated request does not ask. A Responses client that writes the
//! field itself (passthrough) is told what the upstream said.

mod support;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::fake::summary_refusal;
use support::{Answer, Behaviour, FOUR_PROVIDERS, Harness, Kind, body};
use switchyard_core::Protocol;
use switchyard_core::ir::FinishReason;
use switchyard_telemetry::Mode;

const KEY: &str = "key-responses-1";
const MODEL: &str = "m-responses";

/// A Chat Completions request that turns reasoning on.
fn chat_with_reasoning(stream: bool) -> Value {
    let mut request = body(Protocol::OpenaiChat, MODEL, stream, false);
    request["reasoning_effort"] = json!("high");
    request
}

/// An upstream that behaves like an unverified organisation for good.
async fn unverified() -> Harness {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    harness.fake.always(
        KEY,
        Behaviour::UnverifiedOrg(Answer::text("Thought through.")),
    );
    harness
}

/// The scheduler's counters of the Responses provider's only credential:
/// attempts reported, successes, failures.
fn counters(harness: &Harness) -> (u64, u64, u64) {
    let providers = harness.gateway.scheduler().snapshot();
    let provider = providers
        .iter()
        .find(|provider| provider.name == "responses")
        .expect("the responses provider");
    let credential = &provider.credentials[0];
    (
        credential.requests,
        credential.successes,
        credential.failures,
    )
}

fn summary_of(body: &Value) -> &Value {
    &body["reasoning"]["summary"]
}

#[tokio::test]
async fn a_refused_summary_is_dropped_and_the_request_served() {
    let harness = unverified().await;

    // --- the first request runs into the refusal -------------------------
    let output = harness
        .ask_with(
            Protocol::OpenaiChat,
            chat_with_reasoning(false),
            MODEL,
            false,
        )
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    let response = output.response(Protocol::OpenaiChat);
    assert_eq!(response.text(), "Thought through.");
    assert_eq!(response.finish, FinishReason::Stop);

    let calls = harness.fake.requests();
    assert_eq!(calls.len(), 2, "one refusal, one repeat");
    assert_eq!(*summary_of(&calls[0].body), json!("auto"));
    assert_eq!(calls[0].body["reasoning"]["effort"], "high");
    // The repeat: the same request, the same credential, minus the field.
    assert_eq!(*summary_of(&calls[1].body), Value::Null);
    assert_eq!(calls[1].body["reasoning"], json!({"effort": "high"}));
    assert_eq!(calls[1].key, calls[0].key);
    let mut without = calls[0].body.clone();
    without["reasoning"]
        .as_object_mut()
        .unwrap()
        .shift_remove("summary");
    assert_eq!(calls[1].body, without, "nothing else changed");

    // Both calls are on the record; the request succeeded.
    let record = harness.record(&output.request_id);
    assert!(record.ok, "{record:?}");
    assert_eq!(record.status, 200);
    assert_eq!(record.error, None);
    assert_eq!(record.mode, Some(Mode::Translated));
    assert_eq!(record.attempts.len(), 2, "{:?}", record.attempts);
    assert!(!record.attempts[0].ok);
    assert_eq!(record.attempts[0].status, 400);
    assert!(
        record.attempts[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("must be verified"),
        "{:?}",
        record.attempts[0]
    );
    assert!(record.attempts[1].ok);
    assert_eq!(record.attempts[1].status, 200);
    assert_eq!(
        record.attempts[0].credential_id,
        record.attempts[1].credential_id
    );
    // The scheduler heard of one attempt, a success.
    assert_eq!(counters(&harness), (1, 1, 0));

    // --- later translated requests do not ask ----------------------------
    harness.fake.clear();
    let output = harness
        .ask_with(
            Protocol::OpenaiChat,
            chat_with_reasoning(false),
            MODEL,
            false,
        )
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    let calls = harness.fake.requests();
    assert_eq!(calls.len(), 1, "no refusal to run into any more");
    assert_eq!(calls[0].body["reasoning"], json!({"effort": "high"}));
    assert_eq!(harness.record(&output.request_id).attempts.len(), 1);

    // Whatever protocol they speak, streaming or not.
    harness.fake.clear();
    let mut anthropic = body(Protocol::Anthropic, MODEL, true, false);
    anthropic["max_tokens"] = json!(4096);
    // This client asks for the summary in so many words.
    anthropic["thinking"] =
        json!({"type": "enabled", "budget_tokens": 2048, "display": "summarized"});
    let output = harness
        .ask_with(Protocol::Anthropic, anthropic, MODEL, true)
        .await;
    assert!(output.streamed, "{:?}", output.body);
    assert_eq!(
        output.response(Protocol::Anthropic).text(),
        "Thought through."
    );
    let calls = harness.fake.requests();
    assert_eq!(calls.len(), 1);
    assert_eq!(*summary_of(&calls[0].body), Value::Null);
    assert_eq!(calls[0].body["reasoning"]["generate_summary"], Value::Null);
    assert!(
        calls[0].body["reasoning"]["effort"].is_string(),
        "the effort is still asked for: {}",
        calls[0].body
    );
    assert_eq!(counters(&harness), (3, 3, 0));
}

#[tokio::test]
async fn a_refused_summary_is_dropped_from_a_stream_too() {
    let harness = unverified().await;
    let output = harness
        .ask_with(Protocol::OpenaiChat, chat_with_reasoning(true), MODEL, true)
        .await;
    assert!(
        output.streamed,
        "status {}: {:?}",
        output.status, output.body
    );
    let response = output.response(Protocol::OpenaiChat);
    assert_eq!(response.text(), "Thought through.");

    let calls = harness.fake.requests();
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|call| call.kind == Kind::Generate { stream: true })
    );
    assert_eq!(*summary_of(&calls[0].body), json!("auto"));
    assert_eq!(*summary_of(&calls[1].body), Value::Null);

    let record = harness.record(&output.request_id);
    assert!(record.ok, "{record:?}");
    assert_eq!(record.attempts.len(), 2);
    assert_eq!(
        (record.attempts[0].ok, record.attempts[0].status),
        (false, 400)
    );
    assert!(record.attempts[1].ok);
    assert_eq!(counters(&harness), (1, 1, 0));

    // The next stream does not ask.
    harness.fake.clear();
    let output = harness
        .ask_with(Protocol::OpenaiChat, chat_with_reasoning(true), MODEL, true)
        .await;
    assert!(output.streamed);
    assert_eq!(harness.fake.count(), 1);
    assert_eq!(*summary_of(&harness.fake.last().body), Value::Null);
}

#[tokio::test]
async fn a_passthrough_request_keeps_its_summary_and_is_told_of_the_refusal() {
    let harness = unverified().await;
    let responses_request = |stream: bool| {
        let mut request = body(Protocol::OpenaiResponses, MODEL, stream, false);
        request["reasoning"] = json!({"effort": "high", "summary": "detailed"});
        request
    };

    // Before the gateway knows anything about this upstream, and after a
    // translated request has taught it: a Responses client's own field is
    // never touched.
    for round in 0..2 {
        for stream in [false, true] {
            harness.fake.clear();
            let output = harness
                .ask_with(
                    Protocol::OpenaiResponses,
                    responses_request(stream),
                    MODEL,
                    stream,
                )
                .await;
            let label = format!("round {round}, stream={stream}");
            assert_eq!(output.status, 400, "{label}");
            assert!(!output.streamed, "{label}");
            // The upstream's own body, as it is.
            assert_eq!(output.json(), summary_refusal(), "{label}");
            let calls = harness.fake.requests();
            assert_eq!(calls.len(), 1, "{label}: no repeat");
            assert_eq!(*summary_of(&calls[0].body), json!("detailed"), "{label}");

            let record = harness.record(&output.request_id);
            assert_eq!(record.mode, Some(Mode::Passthrough), "{label}");
            assert_eq!(record.attempts.len(), 1, "{label}");
            assert_eq!(record.status, 400, "{label}");
        }
        if round == 0 {
            harness.fake.clear();
            let output = harness
                .ask_with(
                    Protocol::OpenaiChat,
                    chat_with_reasoning(false),
                    MODEL,
                    false,
                )
                .await;
            assert_eq!(output.status, 200);
            assert_eq!(harness.fake.count(), 2, "the translated request is healed");
        }
    }
    // A request fault rests nothing, and the passthrough refusals were
    // request faults.
    let (_, successes, failures) = counters(&harness);
    assert_eq!((successes, failures), (1, 0));
}

/// A Responses client's body is re-encoded rather than forwarded when it
/// carries another vendor's signature. The summary request in it is still
/// the client's own.
#[tokio::test]
async fn a_re_encoded_responses_request_keeps_its_summary_too() {
    let harness = unverified().await;
    // Teach the gateway first: even then the client's field stays.
    let output = harness
        .ask_with(
            Protocol::OpenaiChat,
            chat_with_reasoning(false),
            MODEL,
            false,
        )
        .await;
    assert_eq!(output.status, 200);

    harness.fake.clear();
    let request = json!({
        "model": MODEL,
        "reasoning": {"effort": "high", "summary": "detailed"},
        "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            // Reasoning an Anthropic upstream signed earlier in this
            // conversation.
            {"type": "reasoning", "id": "rs_1", "summary": [],
             "encrypted_content": "sy1.a.c2lnbmVkLWJ5LWFudGhyb3BpYw=="},
            {"role": "assistant", "content": [{"type": "output_text", "text": "hello"}]},
            {"role": "user", "content": [{"type": "input_text", "text": "and now?"}]}
        ]
    });
    let output = harness
        .ask_with(Protocol::OpenaiResponses, request, MODEL, false)
        .await;
    assert_eq!(output.status, 400, "{:?}", output.body);
    assert!(
        output
            .error_message(Protocol::OpenaiResponses)
            .contains("must be verified"),
        "{:?}",
        output.body
    );
    let calls = harness.fake.requests();
    assert_eq!(calls.len(), 1, "no repeat");
    assert_eq!(*summary_of(&calls[0].body), json!("detailed"));
    assert!(!calls[0].body.to_string().contains("sy1."));
    assert_eq!(
        harness.record(&output.request_id).mode,
        Some(Mode::Translated)
    );
}

#[tokio::test]
async fn the_repeat_is_not_one_of_the_routing_attempts() {
    let harness = Harness::start(&format!("[routing]\nmax_attempts = 1\n{FOUR_PROVIDERS}")).await;
    harness.fake.always(
        KEY,
        Behaviour::UnverifiedOrg(Answer::text("Thought through.")),
    );
    for stream in [false, true] {
        // A fresh configuration forgets what was learnt, so both rounds run
        // into the refusal.
        harness
            .reconfigure(&format!(
                "{}\n[routing]\nmax_attempts = 1\n# round {stream}\n{FOUR_PROVIDERS}",
                support::PREAMBLE
            ))
            .await;
        harness.fake.always(
            KEY,
            Behaviour::UnverifiedOrg(Answer::text("Thought through.")),
        );
        harness.fake.clear();
        let output = harness
            .ask_with(
                Protocol::OpenaiChat,
                chat_with_reasoning(stream),
                MODEL,
                stream,
            )
            .await;
        assert_eq!(output.status, 200, "stream={stream}: {:?}", output.body);
        assert_eq!(
            output.response(Protocol::OpenaiChat).text(),
            "Thought through."
        );
        assert_eq!(harness.fake.count(), 2, "stream={stream}");
    }
}

#[tokio::test]
async fn what_was_learnt_is_forgotten_when_the_configuration_changes() {
    let harness = unverified().await;
    let ask = || {
        harness.ask_with(
            Protocol::OpenaiChat,
            chat_with_reasoning(false),
            MODEL,
            false,
        )
    };
    assert_eq!(ask().await.status, 200);
    assert_eq!(harness.fake.count(), 2);
    harness.fake.clear();
    assert_eq!(ask().await.status, 200);
    assert_eq!(harness.fake.count(), 1);

    // The organisation gets verified and the operator saves the
    // configuration (anything at all): the gateway asks again, and is
    // answered.
    harness
        .reconfigure(&format!(
            "{}\n[routing]\nmax_attempts = 2\n{FOUR_PROVIDERS}",
            support::PREAMBLE
        ))
        .await;
    harness.fake.always(
        KEY,
        Behaviour::Reply(Answer::text("Summarised.").with_reasoning("Because.", "enc-0123456789")),
    );
    harness.fake.clear();
    let output = ask().await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(harness.fake.count(), 1);
    assert_eq!(*summary_of(&harness.fake.last().body), json!("auto"));
    assert!(
        String::from_utf8_lossy(&output.body).contains("Because."),
        "the summary reaches the client: {:?}",
        output.body
    );
}

#[tokio::test]
async fn an_upstream_that_refuses_again_is_not_asked_a_third_time() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    // Refuses whatever the request looks like.
    harness.fake.always(
        KEY,
        Behaviour::Json {
            status: 400,
            body: summary_refusal(),
        },
    );
    let output = harness
        .ask_with(
            Protocol::OpenaiChat,
            chat_with_reasoning(false),
            MODEL,
            false,
        )
        .await;
    assert_eq!(output.status, 400, "{:?}", output.body);
    assert!(
        output
            .error_message(Protocol::OpenaiChat)
            .contains("must be verified"),
        "{:?}",
        output.body
    );
    assert_eq!(harness.fake.count(), 2, "the refusal, one repeat, the end");
    let record = harness.record(&output.request_id);
    assert_eq!(record.attempts.len(), 2);
    assert!(record.attempts.iter().all(|attempt| attempt.status == 400));
    // A request fault: nothing rests.
    let (_, _, failures) = counters(&harness);
    assert_eq!(failures, 0);
}

#[tokio::test]
async fn other_request_faults_are_not_repeated() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    harness.fake.script(
        KEY,
        [
            // Another parameter of the same request.
            Behaviour::Json {
                status: 400,
                body: json!({"error": {
                    "message": "Unsupported value: 'high' is not supported with this model.",
                    "type": "invalid_request_error",
                    "param": "reasoning.effort",
                    "code": "unsupported_value"
                }}),
            },
            // The very words, with a status that means something else.
            Behaviour::Json {
                status: 429,
                body: summary_refusal(),
            },
        ],
    );
    let output = harness
        .ask_with(
            Protocol::OpenaiChat,
            chat_with_reasoning(false),
            MODEL,
            false,
        )
        .await;
    assert_eq!(output.status, 400, "{:?}", output.body);
    assert_eq!(harness.fake.count(), 1);

    harness.fake.clear();
    let output = harness
        .ask_with(
            Protocol::OpenaiChat,
            chat_with_reasoning(false),
            MODEL,
            false,
        )
        .await;
    assert_eq!(output.status, 429, "{:?}", output.body);
    assert_eq!(harness.fake.count(), 1);
    // Neither taught the gateway anything: the field is still asked for.
    assert_eq!(*summary_of(&harness.fake.last().body), json!("auto"));
}

/// A relay in front of the API may hand the refusal on as a complete
/// response that failed (HTTP `200`, `status: "failed"`) instead of as a
/// `400`. It is the same refusal, healed the same way — as it is when it
/// arrives inside a stream.
#[tokio::test]
async fn a_refusal_reported_in_a_failed_response_body_is_healed_too() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let refusal = summary_refusal()["error"].clone();
    harness.fake.script(
        KEY,
        [Behaviour::Json {
            status: 200,
            body: json!({
                "id": "resp_refused", "object": "response", "status": "failed",
                "model": "up-responses", "output": [], "error": refusal, "usage": null
            }),
        }],
    );
    harness
        .fake
        .always(KEY, Behaviour::Reply(Answer::text("Thought through.")));

    let output = harness
        .ask_with(
            Protocol::OpenaiChat,
            chat_with_reasoning(false),
            MODEL,
            false,
        )
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(
        output.response(Protocol::OpenaiChat).text(),
        "Thought through."
    );
    let calls = harness.fake.requests();
    assert_eq!(calls.len(), 2, "one refusal, one repeat");
    assert_eq!(*summary_of(&calls[0].body), json!("auto"));
    assert_eq!(calls[1].body["reasoning"], json!({"effort": "high"}));
    let record = harness.record(&output.request_id);
    assert_eq!(record.attempts.len(), 2);
    assert_eq!(
        (record.attempts[0].ok, record.attempts[0].status),
        (false, 400)
    );
    assert_eq!(counters(&harness), (1, 1, 0));

    // The provider is remembered.
    harness.fake.clear();
    let output = harness
        .ask_with(
            Protocol::OpenaiChat,
            chat_with_reasoning(false),
            MODEL,
            false,
        )
        .await;
    assert_eq!(output.status, 200);
    assert_eq!(harness.fake.count(), 1);
    assert_eq!(*summary_of(&harness.fake.last().body), Value::Null);

    // A Responses client that wrote the field itself is told, as ever.
    harness.fake.script(
        KEY,
        [Behaviour::Json {
            status: 200,
            body: json!({
                "id": "resp_refused", "object": "response", "status": "failed",
                "model": "up-responses", "output": [],
                "error": summary_refusal()["error"], "usage": null
            }),
        }],
    );
    harness.fake.clear();
    let mut own = body(Protocol::OpenaiResponses, MODEL, false, false);
    own["reasoning"] = json!({"effort": "high", "summary": "detailed"});
    let output = harness
        .ask_with(Protocol::OpenaiResponses, own, MODEL, false)
        .await;
    assert_eq!(output.status, 400, "{:?}", output.body);
    assert_eq!(harness.fake.count(), 1, "no repeat");
    assert_eq!(*summary_of(&harness.fake.last().body), json!("detailed"));
}

/// The refusal is remembered — and acted on within the request — per
/// provider: another provider the same request fails over to has refused
/// nothing and is asked for the summary. The next request asks only the
/// provider nothing is known against.
#[tokio::test]
async fn a_refusal_is_held_against_the_provider_that_refused_only() {
    // Without session affinity, so that the second request is routed by
    // the strategy alone rather than to whoever served the first.
    const TWO_ORGANISATIONS: &str = r#"
[routing]
strategy = "fill-first"
session_affinity = false

[[providers]]
name = "org-unverified"
kind = "openai"
wire_api = "responses"
base_url = "{base}/v1"
api_keys = ["key-unverified"]
[[providers.models]]
id = "up-responses"
alias = "m"

[[providers]]
name = "org-verified"
kind = "openai"
wire_api = "responses"
base_url = "{base}/v1"
api_keys = ["key-verified"]
[[providers.models]]
id = "up-responses"
alias = "m"
"#;
    for stream in [false, true] {
        let harness = Harness::start(TWO_ORGANISATIONS).await;
        harness.fake.script(
            "key-unverified",
            [
                Behaviour::UnverifiedOrg(Answer::text("never seen")),
                // Asked again without the field, it turns out to be down.
                Behaviour::error(500, "The server had an error."),
            ],
        );
        harness.fake.always(
            "key-unverified",
            Behaviour::UnverifiedOrg(Answer::text("Thought through.")),
        );
        harness.fake.always(
            "key-verified",
            Behaviour::Reply(
                Answer::text("Summarised.").with_reasoning("Because.", "enc-0123456789"),
            ),
        );
        let mut request = body(Protocol::OpenaiChat, "m", stream, false);
        request["reasoning_effort"] = json!("high");
        let output = harness
            .ask_with(Protocol::OpenaiChat, request.clone(), "m", stream)
            .await;
        assert_eq!(output.status, 200, "stream={stream}: {:?}", output.body);
        let response = output.response(Protocol::OpenaiChat);
        assert_eq!(response.text(), "Summarised.", "stream={stream}");
        assert_eq!(
            response.reasoning_text(),
            "Because.",
            "stream={stream}: the summary the client asked for reaches it"
        );
        let asked = |harness: &Harness| -> Vec<(String, Value)> {
            harness
                .fake
                .requests()
                .into_iter()
                .map(|call| (call.key.clone(), summary_of(&call.body).clone()))
                .collect()
        };
        assert_eq!(
            asked(&harness),
            vec![
                ("key-unverified".to_string(), json!("auto")),
                ("key-unverified".to_string(), Value::Null),
                ("key-verified".to_string(), json!("auto")),
            ],
            "stream={stream}"
        );
        let record = harness.record(&output.request_id);
        let statuses: Vec<u16> = record.attempts.iter().map(|a| a.status).collect();
        assert_eq!(statuses, vec![400, 500, 200], "stream={stream}");

        // Once the first provider is back, it is the one that is not asked.
        let scheduler = harness.gateway.scheduler();
        for credential in scheduler.snapshot().into_iter().flat_map(|p| p.credentials) {
            scheduler.reset_cooldowns(&credential.id);
        }
        harness.fake.clear();
        let output = harness
            .ask_with(Protocol::OpenaiChat, request, "m", stream)
            .await;
        assert_eq!(output.status, 200, "stream={stream}");
        assert_eq!(
            asked(&harness),
            vec![("key-unverified".to_string(), Value::Null)],
            "stream={stream}"
        );
    }
}

/// Counting has no use for a summary, so a translated counting body never
/// asks for one — and an unverified organisation can count.
#[tokio::test]
async fn a_translated_counting_request_does_not_ask_for_a_summary() {
    let harness = unverified().await;
    harness.fake.set_count(321);
    let mut counting = json!({
        "model": MODEL,
        "messages": [{"role": "user", "content": "How many tokens is this?"}],
        "thinking": {"type": "enabled", "budget_tokens": 2048, "display": "summarized"}
    });
    let request = support::request_as(
        harness.identity(),
        Protocol::Anthropic,
        counting.take(),
        MODEL,
        false,
    );
    let output = support::Output::read(harness.gateway.count_tokens(request).await).await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(output.json()["input_tokens"], 321);

    let calls = harness.fake.requests();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].kind, Kind::Count);
    assert_eq!(*summary_of(&calls[0].body), Value::Null);
    assert_eq!(calls[0].body["reasoning"]["generate_summary"], Value::Null);
    assert!(
        calls[0].body["reasoning"]["effort"].is_string(),
        "the effort is part of what is counted: {}",
        calls[0].body
    );
}

#[tokio::test]
async fn a_request_without_reasoning_never_carried_the_field() {
    let harness = unverified().await;
    let output = harness.ask(Protocol::OpenaiChat, MODEL, false).await;
    assert_eq!(output.status, 200);
    assert_eq!(harness.fake.count(), 1);
    assert_eq!(harness.fake.last().body.get("reasoning"), None);
}

// ---------------------------------------------------------------------------
// Refusals that are not about the organisation
// ---------------------------------------------------------------------------

/// One (verified) organisation serving two models.
const TWO_MODELS: &str = r#"
[[providers]]
name = "openai"
kind = "openai"
wire_api = "responses"
base_url = "{base}/v1"
api_keys = ["key-openai-1"]
[[providers.models]]
id = "up-small"
alias = "m-small"
[[providers.models]]
id = "up-large"
alias = "m-large"
"#;
const TWO_MODELS_KEY: &str = "key-openai-1";

/// A `400` that names `reasoning.summary` without a word about the
/// organisation.
fn parameter_refusal(message: &str, code: &str) -> Behaviour {
    Behaviour::Json {
        status: 400,
        body: json!({"error": {
            "message": message,
            "type": "invalid_request_error",
            "param": "reasoning.summary",
            "code": code
        }}),
    }
}

/// A Chat Completions request for `model` that turns reasoning on.
fn chat_for(model: &str) -> Value {
    let mut request = body(Protocol::OpenaiChat, model, false, false);
    request["reasoning_effort"] = json!("high");
    request
}

/// What the upstream was asked by way of a summary, call by call.
fn summaries_asked(harness: &Harness) -> Vec<Value> {
    harness
        .fake
        .requests()
        .iter()
        .map(|call| summary_of(&call.body).clone())
        .collect()
}

#[tokio::test]
async fn a_model_that_takes_no_summaries_is_remembered_by_itself() {
    let harness = Harness::start(TWO_MODELS).await;
    harness.fake.script(
        TWO_MODELS_KEY,
        [parameter_refusal(
            "Unsupported parameter: 'reasoning.summary' is not supported with this model.",
            "unsupported_parameter",
        )],
    );
    harness.fake.always(
        TWO_MODELS_KEY,
        Behaviour::Reply(Answer::text("Summarised.").with_reasoning("Because.", "enc-0123456789")),
    );
    let ask =
        |model: &'static str| harness.ask_with(Protocol::OpenaiChat, chat_for(model), model, false);

    // The plain "auto" is refused: healed, like the organisation's refusal.
    let output = ask("m-small").await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(summaries_asked(&harness), vec![json!("auto"), Value::Null]);
    let record = harness.record(&output.request_id);
    assert_eq!(record.attempts.len(), 2, "{:?}", record.attempts);
    assert_eq!(
        (record.attempts[0].status, record.attempts[1].status),
        (400, 200)
    );

    // That model is not asked again …
    harness.fake.clear();
    assert_eq!(ask("m-small").await.status, 200);
    assert_eq!(summaries_asked(&harness), vec![Value::Null]);
    assert_eq!(
        harness.fake.last().body["reasoning"],
        json!({"effort": "high"})
    );

    // … while the provider's other model is, and summarises.
    harness.fake.clear();
    let output = ask("m-large").await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    assert_eq!(summaries_asked(&harness), vec![json!("auto")]);
    assert_eq!(
        output.response(Protocol::OpenaiChat).reasoning_text(),
        "Because."
    );

    // Forgotten, like everything else, when the configuration changes.
    harness
        .reconfigure(&format!(
            "{}\n[routing]\nmax_attempts = 2\n{TWO_MODELS}",
            support::PREAMBLE
        ))
        .await;
    harness.fake.clear();
    assert_eq!(ask("m-small").await.status, 200);
    assert_eq!(summaries_asked(&harness), vec![json!("auto")]);
}

#[tokio::test]
async fn a_refused_detail_level_is_this_requests_affair_only() {
    let harness = Harness::start(TWO_MODELS).await;
    let refusal = || {
        parameter_refusal(
            "Unsupported value: 'concise' is not supported with the 'up-small' model. \
             Supported values are: 'auto' and 'detailed'.",
            "unsupported_value",
        )
    };
    harness.fake.always(
        TWO_MODELS_KEY,
        Behaviour::Reply(Answer::text("Summarised.").with_reasoning("Because.", "enc-0123456789")),
    );

    // Twice over: nothing the first request ran into is remembered, so the
    // second is treated exactly like it.
    for _ in 0..2 {
        harness.fake.clear();
        harness.fake.script(TWO_MODELS_KEY, [refusal()]);
        let mut request = chat_for("m-small");
        request
            .as_object_mut()
            .unwrap()
            .shift_remove("reasoning_effort");
        request["reasoning"] = json!({"effort": "high", "summary": "concise"});
        let output = harness
            .ask_with(Protocol::OpenaiChat, request, "m-small", false)
            .await;
        // The request itself is healed: one repeat, without the summary.
        assert_eq!(output.status, 200, "{:?}", output.body);
        assert_eq!(
            summaries_asked(&harness),
            vec![json!("concise"), Value::Null]
        );
        assert_eq!(harness.record(&output.request_id).attempts.len(), 2);
    }

    // Everybody else — the same model included — is asked as usual.
    for model in ["m-small", "m-large"] {
        harness.fake.clear();
        let output = harness
            .ask_with(Protocol::OpenaiChat, chat_for(model), model, false)
            .await;
        assert_eq!(output.status, 200, "{:?}", output.body);
        assert_eq!(summaries_asked(&harness), vec![json!("auto")], "{model}");
        assert_eq!(
            output.response(Protocol::OpenaiChat).reasoning_text(),
            "Because.",
            "{model}"
        );
    }
    // The scheduler heard of four requests, all successes.
    let providers = harness.gateway.scheduler().snapshot();
    let credential = &providers[0].credentials[0];
    assert_eq!(
        (
            credential.requests,
            credential.successes,
            credential.failures
        ),
        (4, 4, 0)
    );
}
