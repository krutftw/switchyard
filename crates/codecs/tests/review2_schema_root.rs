//! Review finding (round 2): a Gemini client's `responseSchema` whose root is
//! not an object, on its way to an OpenAI upstream. This test currently FAILS.
//!
//! Gemini's structured output takes any schema at the root; the example in
//! Google's own guide is a list (`{"type":"ARRAY","items":{…}}`), and
//! `responseMimeType: "text/x.enum"` goes with a bare string enum. OpenAI's
//! `json_schema` format (Chat `response_format`, Responses `text.format`)
//! only takes an object at the root — Structured Outputs guide, "Supported
//! schemas": "Root objects must not be `anyOf` and must be an object" — and
//! answers anything else with 400 `invalid_request_error` ("schema must be a
//! JSON Schema of 'type: \"object\"', got 'type: \"array\"'"), a request
//! fault that no failover repairs.
//!
//! The Chat encoder already knows the rule for *function parameters*
//! (`schema::normalize_parameters` coerces a foreign root to an object
//! schema; the same error text). The `response_format` schema of a foreign
//! request is forwarded with whatever root it has, and so is the Responses
//! encoder's `text.format.schema`. Round 1 fixed the equivalent problem for
//! the Anthropic encoder (`output_config.format`), which now falls back to a
//! system instruction for schemas the API cannot take.
//!
//! Either outcome is accepted here: a `json_schema` format whose root is an
//! object schema, or no `json_schema` format with the schema described to
//! the model in the system slot.

mod support;

use serde_json::{Value, json};
use support::harness::{CHAT, GEMINI, RESPONSES, known_caps, translate_request, validate_request};
use switchyard_core::Protocol;

fn gemini_request(mime: &str, schema: Value) -> Value {
    json!({
        "contents": [{"role": "user", "parts": [{"text": "List two cookie recipes."}]}],
        "generationConfig": {"responseMimeType": mime, "responseSchema": schema}
    })
}

/// `(format, system text)` of an OpenAI upstream body.
fn format_and_system(upstream: Protocol, body: &Value) -> (Value, String) {
    match upstream {
        Protocol::OpenaiChat => (
            body["response_format"].clone(),
            body["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|message| message["role"] == "system" || message["role"] == "developer")
                .map(|message| message["content"].to_string())
                .collect(),
        ),
        _ => (
            body["text"]["format"].clone(),
            body["instructions"].to_string(),
        ),
    }
}

#[test]
fn a_gemini_response_schema_without_an_object_root_is_fitted_for_openai() {
    let cases = [
        (
            "list of recipes",
            "recipeName",
            gemini_request(
                "application/json",
                json!({"type": "ARRAY", "items": {"type": "OBJECT", "properties": {
                    "recipeName": {"type": "STRING"},
                    "ingredients": {"type": "ARRAY", "items": {"type": "STRING"}}
                }, "required": ["recipeName"]}}),
            ),
        ),
        (
            "enum",
            "Percussion",
            gemini_request(
                "text/x.enum",
                json!({"type": "STRING", "enum": ["Percussion", "String", "Woodwind"]}),
            ),
        ),
    ];
    let mut failures = Vec::new();
    for (label, marker, request) in &cases {
        for upstream in [CHAT, RESPONSES] {
            let caps = known_caps(upstream);
            let body = translate_request(GEMINI, upstream, request, &caps.ctx())
                .expect("the request translates");
            if let Err(violations) = validate_request(upstream, &body) {
                panic!("{label} -> {upstream}: {violations:?}\n{body}");
            }
            let (format, system) = format_and_system(upstream, &body);
            if format["type"] == "json_schema" {
                let schema = match upstream {
                    Protocol::OpenaiChat => &format["json_schema"]["schema"],
                    _ => &format["schema"],
                };
                if schema["type"] != "object" {
                    failures.push(format!(
                        "{label} -> {upstream}: json_schema format with root type {} (OpenAI: \
                         \"Root objects … must be an object\"): {schema}",
                        schema["type"]
                    ));
                }
            } else if !system.contains(marker) {
                failures.push(format!(
                    "{label} -> {upstream}: no json_schema format, and the schema is not \
                     described to the model either (format {format}, system {system:.200})"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
