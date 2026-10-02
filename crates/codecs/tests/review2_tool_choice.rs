//! Review findings (round 2) on tool restrictions that change their meaning
//! on the way to another protocol. These tests currently FAIL.
//!
//! 1. A client that restricts the model to a *subset* of its tools
//!    (Responses `tool_choice: {"type":"allowed_tools", …}`, Gemini
//!    `functionCallingConfig.allowedFunctionNames` with more than one name)
//!    gets the whole tool list offered upstream with "call any of them": the
//!    model may call exactly the tools the client excluded, and the client's
//!    tool loop then runs them. The Chat decoder narrows the tool list for the
//!    same construct (notes 06 §1.6: "The emitted `tools` list is **filtered**
//!    to the allowed names", notes 07 §1.6 the same for Gemini targets); the
//!    Responses and the Gemini decoder keep only the mode.
//! 2. An Anthropic client that *forces* its web-search server tool
//!    (`tool_choice: {"type":"tool","name":"web_search"}`) is translated for
//!    a Responses upstream into the hosted `web_search` tool together with
//!    `tool_choice: "none"`: the one tool the client insisted on is
//!    forbidden.

mod support;

use serde_json::{Value, json};
use support::harness::{
    ANTHROPIC, CHAT, GEMINI, RESPONSES, declared_tools, known_caps, translate_request,
    validate_request,
};
use switchyard_core::Protocol;

fn to(client: Protocol, upstream: Protocol, body: &Value) -> Value {
    let caps = known_caps(upstream);
    let translated = translate_request(client, upstream, body, &caps.ctx())
        .unwrap_or_else(|error| panic!("{client} -> {upstream} does not translate: {error}"));
    if let Err(violations) = validate_request(upstream, &translated) {
        panic!("{client} -> {upstream}: invalid body {violations:?}\n{translated}");
    }
    translated
}

/// The names the upstream model is allowed to call according to the body:
/// the declared functions, narrowed by the upstream's own way of restricting
/// them when the body uses it (Gemini `allowedFunctionNames`, OpenAI
/// `tool_choice: {"type":"allowed_tools", …}`). Either a narrowed tool list
/// or the native restriction satisfies the test.
fn callable(upstream: Protocol, body: &Value) -> Vec<String> {
    let declared = declared_tools(upstream, body);
    let names = |entries: &Value| -> Option<Vec<String>> {
        Some(
            entries
                .as_array()?
                .iter()
                .filter_map(|entry| {
                    entry
                        .as_str()
                        .or_else(|| entry["name"].as_str())
                        .or_else(|| entry["function"]["name"].as_str())
                        .map(str::to_string)
                })
                .collect(),
        )
    };
    let choice = &body["tool_choice"];
    let allowed = match upstream {
        Protocol::Gemini => {
            names(&body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"])
        }
        Protocol::OpenaiResponses if choice["type"] == "allowed_tools" => names(&choice["tools"]),
        Protocol::OpenaiChat if choice["type"] == "allowed_tools" => {
            names(&choice["allowed_tools"]["tools"])
        }
        _ => None,
    };
    match allowed {
        Some(allowed) => declared
            .into_iter()
            .filter(|name| allowed.contains(name))
            .collect(),
        None => declared,
    }
}

#[test]
fn a_restriction_to_some_tools_does_not_become_permission_for_all() {
    let parameters = json!({"type": "object", "properties": {}});
    let responses_client = json!({
        "model": "gpt-5.5",
        "input": "Confirm or cancel the order.",
        "tools": [
            {"type": "function", "name": "confirm_order", "parameters": parameters},
            {"type": "function", "name": "cancel_order", "parameters": parameters},
            {"type": "function", "name": "delete_account", "parameters": parameters}
        ],
        "tool_choice": {"type": "allowed_tools", "mode": "required", "tools": [
            {"type": "function", "name": "confirm_order"},
            {"type": "function", "name": "cancel_order"}
        ]}
    });
    let gemini_client = json!({
        "contents": [{"role": "user", "parts": [{"text": "Confirm or cancel the order."}]}],
        "tools": [{"functionDeclarations": [
            {"name": "confirm_order", "description": "Confirm"},
            {"name": "cancel_order", "description": "Cancel"},
            {"name": "delete_account", "description": "Delete"}
        ]}],
        "toolConfig": {"functionCallingConfig": {
            "mode": "ANY", "allowedFunctionNames": ["confirm_order", "cancel_order"]
        }}
    });
    let mut failures = Vec::new();
    for (client, body) in [(RESPONSES, &responses_client), (GEMINI, &gemini_client)] {
        for upstream in [CHAT, RESPONSES, ANTHROPIC, GEMINI] {
            if upstream == client {
                continue;
            }
            let translated = to(client, upstream, body);
            let mut names = callable(upstream, &translated);
            names.sort();
            if names != ["cancel_order", "confirm_order"] {
                failures.push(format!(
                    "{client} -> {upstream}: the model may call {names:?}; the client allowed only \
                     confirm_order and cancel_order"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_forced_web_search_is_not_forbidden_on_a_responses_upstream() {
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 3}],
        "tool_choice": {"type": "tool", "name": "web_search"},
        "messages": [{"role": "user", "content": "What changed in Rust 1.90?"}]
    });
    let translated = to(ANTHROPIC, RESPONSES, &body);
    assert_eq!(
        translated["tools"],
        json!([{"type": "web_search"}]),
        "the server tool is mapped to the hosted one"
    );
    assert_ne!(
        translated["tool_choice"],
        json!("none"),
        "the client forced web search; the upstream is told not to use any tool: {translated}"
    );
}
