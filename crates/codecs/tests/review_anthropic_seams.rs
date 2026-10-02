//! Review findings at the Anthropic seams. These tests currently FAIL.

mod support;

use serde_json::{Value, json};
use support::harness::{ANTHROPIC, CHAT, GEMINI, RESPONSES, known_caps, translate_request};
use switchyard_core::Protocol;

fn to(client: Protocol, upstream: Protocol, body: &Value) -> Value {
    let caps = known_caps(upstream);
    translate_request(client, upstream, body, &caps.ctx())
        .unwrap_or_else(|error| panic!("{client} -> {upstream} does not translate: {error}"))
}

/// Collects what Anthropic's structured outputs refuse in a schema
/// (platform docs, "JSON Schema limitations": `additionalProperties` "must be
/// set to `false` for objects"; numerical constraints, string length
/// constraints and array constraints beyond `minItems` 0/1 are "not
/// supported"; "If you use an unsupported feature, you'll receive a 400
/// error" — `output_format.schema: For 'object' type, 'additionalProperties'
/// must be explicitly set to false`).
fn unsupported(schema: &Value, path: &str, out: &mut Vec<String>) {
    let Some(map) = schema.as_object() else {
        return;
    };
    let is_object = map.get("type") == Some(&json!("object")) || map.contains_key("properties");
    if is_object && map.get("additionalProperties") != Some(&json!(false)) {
        out.push(format!(
            "{path}: object schema without `additionalProperties: false`"
        ));
    }
    for keyword in [
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minLength",
        "maxLength",
        "maxItems",
    ] {
        if map.contains_key(keyword) {
            out.push(format!("{path}: unsupported keyword `{keyword}`"));
        }
    }
    if map
        .get("minItems")
        .and_then(Value::as_u64)
        .is_some_and(|n| n > 1)
    {
        out.push(format!("{path}: `minItems` above 1"));
    }
    if let Some(Value::Object(properties)) = map.get("properties") {
        for (name, child) in properties {
            unsupported(child, &format!("{path}.{name}"), out);
        }
    }
    if let Some(items) = map.get("items") {
        unsupported(items, &format!("{path}[]"), out);
    }
    for union in ["anyOf", "allOf"] {
        for (i, child) in map
            .get(union)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            unsupported(child, &format!("{path}.{union}[{i}]"), out);
        }
    }
}

/// A JSON schema written for Gemini (`responseSchema`) or for OpenAI's
/// non-strict `json_schema` is copied verbatim into Anthropic's
/// `output_config.format`, whose schema dialect is much narrower: Anthropic
/// answers 400 (a request fault, so no failover). The reference never had
/// this problem because it expresses the schema as a system instruction
/// (notes 06 §1.7); a codec that uses the native field has to fit the schema
/// to it, or fall back to the instruction when it cannot.
#[test]
fn foreign_json_schemas_are_fitted_to_anthropic_structured_outputs() {
    let cases: Vec<(Protocol, Value)> = vec![
        (
            GEMINI,
            json!({
                "contents": [{"role": "user", "parts": [{"text": "Extract the person."}]}],
                "generationConfig": {
                    "responseMimeType": "application/json",
                    "responseSchema": {
                        "type": "OBJECT",
                        "properties": {
                            "name": {"type": "STRING"},
                            "age": {"type": "INTEGER", "minimum": 0, "maximum": 150},
                            "tags": {"type": "ARRAY", "minItems": 2, "items": {
                                "type": "OBJECT",
                                "properties": {"label": {"type": "STRING", "maxLength": 8}}
                            }}
                        },
                        "required": ["name"],
                        "propertyOrdering": ["name", "age", "tags"]
                    }
                }
            }),
        ),
        (
            CHAT,
            json!({
                "model": "gpt-5.5",
                "messages": [{"role": "user", "content": "Extract the person."}],
                "response_format": {"type": "json_schema", "json_schema": {
                    "name": "person",
                    "schema": {
                        "type": "object",
                        "properties": {"name": {"type": "string"}, "age": {"type": "integer"}},
                        "required": ["name"]
                    }
                }}
            }),
        ),
        (
            RESPONSES,
            json!({
                "model": "gpt-5.5",
                "input": "Extract the person.",
                "text": {"format": {"type": "json_schema", "name": "person", "schema": {
                    "type": "object",
                    "properties": {"address": {"type": "object", "properties": {"city": {"type": "string"}}}}
                }}}
            }),
        ),
    ];
    let mut failures = Vec::new();
    for (client, body) in &cases {
        let encoded = to(*client, ANTHROPIC, body);
        // Either form is acceptable: a schema Anthropic takes in
        // `output_config.format`, or no native format at all (the schema
        // described in `system`, as the reference does).
        let Some(schema) = encoded
            .get("output_config")
            .and_then(|config| config.get("format"))
            .and_then(|format| format.get("schema"))
        else {
            let system = encoded["system"].to_string();
            if !system.contains("name") {
                failures.push(format!(
                    "{client}: the schema is neither in output_config.format nor described in system"
                ));
            }
            continue;
        };
        let mut problems = Vec::new();
        unsupported(schema, "schema", &mut problems);
        for problem in problems {
            failures.push(format!("{client}: {problem}"));
        }
    }
    assert!(
        failures.is_empty(),
        "Anthropic would refuse these `output_config.format` schemas:\n{}",
        failures.join("\n")
    );
}

/// A Messages client's documents that are made of text (`source.type:
/// "content"`, used with citations for RAG) and its `search_result` blocks
/// are the user's prompt. The decoder keeps them as opaque Anthropic blocks,
/// which every other upstream drops: the model is asked "read these" with
/// nothing to read, and nothing tells the client. A plain-text document
/// (`source.type: "text"`) in the same request does reach the upstream.
#[test]
fn a_messages_clients_text_documents_and_search_results_reach_other_upstreams() {
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": [
            {"type": "document", "title": "Handbook", "context": "internal",
             "source": {"type": "content", "content": [
                 {"type": "text", "text": "Retries use exponential backoff."},
                 {"type": "text", "text": "The retry budget is five attempts."}
             ]},
             "citations": {"enabled": true}},
            {"type": "search_result", "source": "https://example.com/policy", "title": "Policy",
             "content": [{"type": "text", "text": "Timeouts are thirty seconds."}]},
            {"type": "text", "text": "What is the retry policy?"}
        ]}]
    });
    let mut failures = Vec::new();
    for upstream in [CHAT, RESPONSES, GEMINI] {
        let wire = to(ANTHROPIC, upstream, &body).to_string();
        for text in [
            "Retries use exponential backoff.",
            "The retry budget is five attempts.",
            "Timeouts are thirty seconds.",
        ] {
            if !wire.contains(text) {
                failures.push(format!(
                    "{upstream}: `{text}` did not reach the upstream body"
                ));
            }
        }
        assert!(
            wire.contains("What is the retry policy?"),
            "{upstream}: the question itself is missing"
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
