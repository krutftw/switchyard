//! Matrix (g): model listings and token counting, per protocol.
//!
//! * `encode_models` / `encode_model` produce the vendor's listing shape
//!   (OpenAI: notes 99 R13; Anthropic: notes 15 §5.9; Gemini: notes 15 §6.7).
//! * Token counting exists where the vendor has an endpoint (Anthropic
//!   `count_tokens`, Gemini `countTokens`, Responses `input_tokens`): the
//!   count response has the vendor's shape and reads back, vendor count
//!   bodies are understood, and for every pair `(C, U)` a `C` request turns
//!   into a count request `U`'s endpoint accepts.

mod support;

use serde_json::{Value, json};
use support::harness::{
    ANTHROPIC, CHAT, Failures, GEMINI, RESPONSES, decode_request, known_caps, short,
    upstream_model, validate_request,
};
use support::scenarios;
use switchyard_codecs::codec;
use switchyard_core::reasoning::{Effort, ThinkingSupport};
use switchyard_core::{ModelInfo, Protocol};

fn models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "gpt-5.5".into(),
            display_name: Some("GPT-5.5".into()),
            description: Some("A reasoning model.".into()),
            owned_by: Some("openai".into()),
            created: Some(1_759_104_000),
            context_window: Some(400_000),
            max_output_tokens: Some(128_000),
            thinking: Some(ThinkingSupport::levels(&[
                Effort::Low,
                Effort::Medium,
                Effort::High,
            ])),
            known: true,
        },
        // A model the gateway knows only by name.
        ModelInfo::bare("team/my-local-model"),
        ModelInfo {
            id: "claude-haiku-3-5".into(),
            known: true,
            ..ModelInfo::default()
        },
    ]
}

/// One entry of an OpenAI model list: exactly `id`, `object`, `created`,
/// `owned_by` (notes 99 R13).
fn check_openai_model(entry: &Value, model: &ModelInfo, failures: &mut Failures) {
    let context = model.id.as_str();
    let keys: Vec<&str> = entry
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    failures.check(
        context,
        sorted == ["created", "id", "object", "owned_by"],
        format!("keys {keys:?}"),
    );
    failures.check(context, entry["id"] == model.id, "id");
    failures.check(
        context,
        entry["object"] == "model",
        "object must be `model`",
    );
    failures.check(
        context,
        entry["created"].is_u64(),
        "created must be an integer",
    );
    failures.check(
        context,
        entry["owned_by"].as_str().is_some_and(|s| !s.is_empty()),
        "owned_by must be a non-empty string",
    );
}

fn openai_listing(protocol: Protocol) {
    let mut failures = Failures::default();
    let models = models();
    let list = codec(protocol).encode_models(&models);
    failures.check("list", list["object"] == "list", "object must be `list`");
    let data = list["data"].as_array().cloned().unwrap_or_default();
    failures.check(
        "list",
        data.len() == models.len(),
        "one entry per model, in order",
    );
    for (entry, model) in data.iter().zip(&models) {
        check_openai_model(entry, model, &mut failures);
        failures.check(
            &model.id,
            &codec(protocol).encode_model(model) == entry,
            "encode_model differs from the list entry",
        );
    }
    failures.check(
        "known",
        data[0]["created"] == 1_759_104_000 && data[0]["owned_by"] == "openai",
        "known metadata is reported",
    );
    failures.finish(&format!("{} model listing", short(protocol)));
}

#[test]
fn chat_model_listing() {
    openai_listing(CHAT);
}

#[test]
fn responses_model_listing() {
    openai_listing(RESPONSES);
}

/// Notes 15 §5.9: `{"data":[{"type":"model","id","display_name",
/// "created_at",…}],"first_id","last_id","has_more"}`; limits may be null.
#[test]
fn anthropic_model_listing() {
    let mut failures = Failures::default();
    let models = models();
    let list = codec(ANTHROPIC).encode_models(&models);
    failures.check("list", list["has_more"] == false, "has_more must be false");
    failures.check("list", list["first_id"] == "gpt-5.5", "first_id");
    failures.check("list", list["last_id"] == "claude-haiku-3-5", "last_id");
    let data = list["data"].as_array().cloned().unwrap_or_default();
    failures.check("list", data.len() == models.len(), "one entry per model");
    for (entry, model) in data.iter().zip(&models) {
        let context = model.id.as_str();
        failures.check(context, entry["type"] == "model", "type must be `model`");
        failures.check(context, entry["id"] == model.id, "id");
        failures.check(
            context,
            entry["display_name"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "display_name must be a non-empty string",
        );
        // RFC 3339, e.g. `2026-07-24T00:00:00Z`.
        let created = entry["created_at"].as_str().unwrap_or("");
        failures.check(
            context,
            created.len() == 20 && created.ends_with('Z') && created.as_bytes()[10] == b'T',
            format!("created_at `{created}` is not an RFC 3339 UTC timestamp"),
        );
        for key in ["max_input_tokens", "max_tokens"] {
            failures.check(
                context,
                entry[key].is_u64() || entry[key].is_null(),
                format!("{key} must be an integer or null"),
            );
        }
        failures.check(
            context,
            &codec(ANTHROPIC).encode_model(model) == entry,
            "encode_model differs from the list entry",
        );
    }
    failures.check(
        "known",
        data[0]["display_name"] == "GPT-5.5",
        "display name",
    );
    failures.check(
        "known",
        data[0]["created_at"] == "2025-09-29T00:00:00Z",
        format!("created_at {}", data[0]["created_at"]),
    );
    failures.check(
        "known",
        data[0]["max_input_tokens"] == 400_000 && data[0]["max_tokens"] == 128_000,
        "limits",
    );
    let empty = codec(ANTHROPIC).encode_models(&[]);
    failures.check(
        "empty",
        empty["first_id"].is_null() && empty["last_id"].is_null() && empty["data"] == json!([]),
        "an empty list has null cursors",
    );
    failures.finish("anthropic model listing");
}

/// Notes 15 §6.7: `{"models":[{"name":"models/<id>","displayName",
/// "description","inputTokenLimit","outputTokenLimit",
/// "supportedGenerationMethods":[…]}]}`.
#[test]
fn gemini_model_listing() {
    let mut failures = Failures::default();
    let models = models();
    let list = codec(GEMINI).encode_models(&models);
    let data = list["models"].as_array().cloned().unwrap_or_default();
    failures.check("list", data.len() == models.len(), "one entry per model");
    failures.check(
        "list",
        list.get("nextPageToken").is_none(),
        "a single page has no nextPageToken",
    );
    for (entry, model) in data.iter().zip(&models) {
        let context = model.id.as_str();
        failures.check(
            context,
            entry["name"] == format!("models/{}", model.id),
            format!("name {}", entry["name"]),
        );
        failures.check(
            context,
            entry["displayName"].as_str().is_some_and(|s| !s.is_empty()),
            "displayName",
        );
        let methods: Vec<&str> = entry["supportedGenerationMethods"]
            .as_array()
            .map(|m| m.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        for method in ["generateContent", "streamGenerateContent", "countTokens"] {
            failures.check(
                context,
                methods.contains(&method),
                format!("{method} is not listed"),
            );
        }
        for key in ["inputTokenLimit", "outputTokenLimit"] {
            failures.check(
                context,
                entry.get(key).is_none_or(Value::is_u64),
                format!("{key} must be an integer when present"),
            );
        }
        failures.check(
            context,
            &codec(GEMINI).encode_model(model) == entry,
            "encode_model differs from the list entry",
        );
    }
    failures.check(
        "known",
        data[0]["inputTokenLimit"] == 400_000 && data[0]["outputTokenLimit"] == 128_000,
        "limits",
    );
    failures.check(
        "known",
        data[0]["thinking"] == true && data[2]["thinking"] == false,
        "thinking flag",
    );
    // A name that already has the resource prefix is not prefixed twice.
    let prefixed = codec(GEMINI).encode_model(&ModelInfo::bare("models/gemini-2.5-pro"));
    failures.check(
        "prefixed",
        prefixed["name"] == "models/gemini-2.5-pro",
        format!("name {}", prefixed["name"]),
    );
    failures.finish("gemini model listing");
}

/// The count response of each protocol, and the vendor's own (richer) body.
#[test]
fn count_responses_have_the_vendor_shape() {
    assert_eq!(
        codec(CHAT).encode_count_response(42),
        None,
        "Chat Completions has no counting endpoint"
    );
    assert_eq!(
        codec(CHAT).decode_count_response(&json!({"input_tokens": 1})),
        None
    );

    // Notes 15 §5.8: `{"input_tokens": 2095}`.
    assert_eq!(
        codec(ANTHROPIC).encode_count_response(2095),
        Some(json!({"input_tokens": 2095}))
    );
    assert_eq!(
        codec(ANTHROPIC).decode_count_response(&json!({"input_tokens": 2095})),
        Some(2095)
    );

    // Notes 15 §6.7: `{"totalTokens":N,"cachedContentTokenCount":N,…}`.
    let gemini = codec(GEMINI)
        .encode_count_response(31)
        .expect("Gemini counts tokens");
    assert_eq!(gemini["totalTokens"], 31);
    assert_eq!(
        codec(GEMINI).decode_count_response(&json!({
            "totalTokens": 31, "cachedContentTokenCount": 0,
            "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 31}]
        })),
        Some(31)
    );

    let responses = codec(RESPONSES)
        .encode_count_response(11)
        .expect("Responses counts tokens");
    assert_eq!(
        responses,
        json!({"object": "response.input_tokens", "input_tokens": 11})
    );
    assert_eq!(codec(RESPONSES).decode_count_response(&responses), Some(11));

    // Every count response reads back through its own decoder.
    for protocol in [RESPONSES, ANTHROPIC, GEMINI] {
        for count in [0u64, 1, 123_456] {
            let body = codec(protocol)
                .encode_count_response(count)
                .expect("a count response");
            assert_eq!(
                codec(protocol).decode_count_response(&body),
                Some(count),
                "{protocol}"
            );
        }
    }
}

/// Fields the Anthropic counting endpoint takes (notes 15 §5.8).
const ANTHROPIC_COUNT_KEYS: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "tool_choice",
    "thinking",
    "output_config",
];
/// Fields the Responses `input_tokens` endpoint takes.
const RESPONSES_COUNT_KEYS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning",
    "text",
    "truncation",
    "previous_response_id",
    "conversation",
];

fn count_requests(client: Protocol) {
    let mut failures = Failures::default();
    for upstream in Protocol::ALL {
        let caps = known_caps(upstream);
        for scenario in scenarios::requests(client) {
            // Scenarios a native client would not send to its own vendor are
            // forwarded as written on the same protocol.
            if client == upstream && scenario.name == "odd_tool_names" {
                continue;
            }
            let context = format!("{} / {}", short(upstream), scenario.name);
            let mut request = match decode_request(client, &scenario.body) {
                Ok(request) => request,
                Err(error) => {
                    failures.push(&context, format!("decode failed: {error}"));
                    continue;
                }
            };
            request.model = upstream_model(upstream).to_string();
            let body = codec(upstream).encode_count_request(&request, &caps.ctx());
            match (upstream, body) {
                (Protocol::OpenaiChat, None) => {}
                (Protocol::OpenaiChat, Some(_)) => {
                    failures.push(&context, "Chat has no counting endpoint")
                }
                (_, None) => failures.push(&context, "no count request"),
                (Protocol::Anthropic, Some(body)) => {
                    let keys: Vec<&String> = body
                        .as_object()
                        .map(|m| m.keys().collect())
                        .unwrap_or_default();
                    for key in &keys {
                        failures.check(
                            &context,
                            ANTHROPIC_COUNT_KEYS.contains(&key.as_str()),
                            format!("`{key}` is not a count_tokens field"),
                        );
                    }
                    // The endpoint refuses media by URL or file id.
                    let wire = body.to_string();
                    failures.check(
                        &context,
                        !wire.contains("\"type\":\"url\""),
                        "media by URL in a count request",
                    );
                    // Otherwise the body is a Messages body without a limit.
                    let mut generation = body.clone();
                    generation["max_tokens"] = json!(64_000);
                    failures.report(&context, validate_request(upstream, &generation));
                }
                (Protocol::Gemini, Some(body)) => {
                    // Either bare `contents`, or the whole request wrapped in
                    // `generateContentRequest`, which must name the model
                    // (notes 15 §6.7). Never both.
                    let root = body.as_object().cloned().unwrap_or_default();
                    let keys: Vec<&str> = root.keys().map(String::as_str).collect();
                    let mut inner = match keys.as_slice() {
                        ["contents"] => body.clone(),
                        ["generateContentRequest"] => {
                            let mut inner = body["generateContentRequest"].clone();
                            let model = inner["model"].as_str().unwrap_or("").to_string();
                            failures.check(
                                &context,
                                model == format!("models/{}", upstream_model(upstream)),
                                format!("model `{model}`"),
                            );
                            inner
                                .as_object_mut()
                                .expect("an object")
                                .shift_remove("model");
                            inner
                        }
                        other => {
                            failures
                                .push(&context, format!("unexpected count body keys {other:?}"));
                            continue;
                        }
                    };
                    // A count request may end with a model turn.
                    if let Some(contents) = inner["contents"].as_array_mut()
                        && contents.last().is_some_and(|last| last["role"] == "model")
                    {
                        contents.push(json!({"role": "user", "parts": [{"text": "x"}]}));
                    }
                    failures.report(&context, validate_request(upstream, &inner));
                }
                (Protocol::OpenaiResponses, Some(body)) => {
                    for key in body
                        .as_object()
                        .map(|m| m.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default()
                    {
                        failures.check(
                            &context,
                            RESPONSES_COUNT_KEYS.contains(&key.as_str()),
                            format!("`{key}` is not an input_tokens field"),
                        );
                    }
                    failures.report(&context, validate_request(upstream, &body));
                }
            }
        }
    }
    failures.finish(&format!("{} count requests", short(client)));
}

#[test]
fn chat_count_requests() {
    count_requests(CHAT);
}

#[test]
fn responses_count_requests() {
    count_requests(RESPONSES);
}

#[test]
fn anthropic_count_requests() {
    count_requests(ANTHROPIC);
}

#[test]
fn gemini_count_requests() {
    count_requests(GEMINI);
}
