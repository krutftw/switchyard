//! Review finding: a refusal of `reasoning.summary` that is about ONE model
//! or ONE value is remembered against the WHOLE provider, for every client,
//! until the configuration changes.
//!
//! `summary::is_summary_refusal` accepts any `400` whose `error.param` is
//! `"reasoning.summary"`, and `summary_refused_or_failed` then records the
//! provider in the gateway-wide `SummaryRefusals` set. That is right for the
//! refusal the feature exists for ("Your organization must be verified to
//! generate reasoning summaries": a property of the organisation, hence of
//! the provider). It is wrong for the other `400`s OpenAI sends with that
//! very `param`:
//!
//! * `Unsupported value: 'concise' is not supported with the '<model>' model.
//!   Supported values are: 'auto' and 'detailed'.` (`code:
//!   "unsupported_value"`) — the detail level one client asked for;
//! * `Unsupported parameter: 'reasoning.summary' is not supported with this
//!   model.` (`code: "unsupported_parameter"`) — one model that has no
//!   summaries.
//!
//! Neither says anything about the provider's other models, nor about what
//! other clients ask for. Healing the request at hand is fine; remembering
//! the provider is not: after a single such request, every later translated
//! request to that provider — any model, any client — silently loses the
//! reasoning text it asked for, with nothing but one `info` log line to show
//! for it, until somebody edits the configuration.
//!
//! Expected: only the organisation-level refusal (the verification message)
//! is remembered per provider; a refusal that names a model or a value is
//! healed for that request (or handed to the client) and nothing more.

#[allow(unused_imports)]
mod support;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{Answer, Behaviour, Harness, body};
use switchyard_core::Protocol;

const KEY: &str = "key-openai-1";

/// One verified OpenAI organisation serving two models.
const ONE_PROVIDER_TWO_MODELS: &str = r#"
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

fn refusal(message: &str, code: &str) -> Behaviour {
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

/// A verified organisation: asked for a summary, it gives one.
fn summarising() -> Behaviour {
    Behaviour::Reply(Answer::text("Summarised.").with_reasoning("Because.", "enc-0123456789"))
}

/// What another client's ordinary request is served with afterwards.
async fn the_next_client_still_gets_its_summary(harness: &Harness, model: &str) {
    harness.fake.clear();
    let mut request = body(Protocol::OpenaiChat, model, false, false);
    request["reasoning_effort"] = json!("high");
    let output = harness
        .ask_with(Protocol::OpenaiChat, request, model, false)
        .await;
    assert_eq!(output.status, 200, "{:?}", output.body);
    let calls = harness.fake.requests();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].body["reasoning"]["summary"],
        json!("auto"),
        "a refusal about one model / one value took the summary away from the whole provider: {}",
        calls[0].body
    );
    assert_eq!(
        output.response(Protocol::OpenaiChat).reasoning_text(),
        "Because.",
        "the client asked for reasoning and the (verified) upstream would have given it"
    );
}

/// One client asks for a detail level the model does not offer. That is
/// this request's problem.
#[tokio::test]
async fn an_unsupported_summary_value_is_not_held_against_the_provider() {
    let harness = Harness::start(ONE_PROVIDER_TWO_MODELS).await;
    harness.fake.script(
        KEY,
        [refusal(
            "Unsupported value: 'concise' is not supported with the 'up-small' model. \
             Supported values are: 'auto' and 'detailed'.",
            "unsupported_value",
        )],
    );
    harness.fake.always(KEY, summarising());

    // The OpenRouter-style spelling of "reason, and summarise concisely".
    let mut request = body(Protocol::OpenaiChat, "m-small", false, false);
    request["reasoning"] = json!({"effort": "high", "summary": "concise"});
    let output = harness
        .ask_with(Protocol::OpenaiChat, request, "m-small", false)
        .await;
    // Healed or told: either is defensible for the request itself.
    assert!(
        matches!(output.status, 200 | 400),
        "status {}: {:?}",
        output.status,
        output.body
    );
    let first = harness.fake.requests();
    assert_eq!(first[0].body["reasoning"]["summary"], json!("concise"));

    // Another model of the same provider, an ordinary request.
    the_next_client_still_gets_its_summary(&harness, "m-large").await;
    // And the same model, asked for what it does support.
    the_next_client_still_gets_its_summary(&harness, "m-small").await;
}

/// One model of the provider has no reasoning summaries at all. The others
/// do.
#[tokio::test]
async fn a_model_without_summaries_is_not_held_against_the_provider() {
    let harness = Harness::start(ONE_PROVIDER_TWO_MODELS).await;
    harness.fake.script(
        KEY,
        [refusal(
            "Unsupported parameter: 'reasoning.summary' is not supported with this model.",
            "unsupported_parameter",
        )],
    );
    harness.fake.always(KEY, summarising());

    let mut request = body(Protocol::OpenaiChat, "m-small", false, false);
    request["reasoning_effort"] = json!("high");
    let output = harness
        .ask_with(Protocol::OpenaiChat, request, "m-small", false)
        .await;
    assert!(
        matches!(output.status, 200 | 400),
        "status {}: {:?}",
        output.status,
        output.body
    );
    let first: Vec<Value> = harness
        .fake
        .requests()
        .into_iter()
        .map(|call| call.body["reasoning"]["summary"].clone())
        .collect();
    assert_eq!(first[0], json!("auto"));

    // The provider's other model summarises, and is still asked to.
    the_next_client_still_gets_its_summary(&harness, "m-large").await;
}
