//! Matrix (a) again, for the Claude generations whose API is narrower than
//! the one the other matrices target (`claude-sonnet-4-5`).
//!
//! Notes 15 §5.2 record two rules that depend on the model:
//!
//! * Claude 4.6 and later take no assistant prefill ("This model does not
//!   support assistant message prefill. The conversation must end with a user
//!   message.");
//! * Claude Opus 5.5, Sonnet 5.5, Fable 5.1 and Mythos 5.1 take no forced
//!   tool use ("tool_choice: type \"tool\" and \"any\" are not supported for
//!   this model."), "including on count_tokens".
//!
//! Clients of the other protocols prefill and force tools as a matter of
//! course (`tool_choice: "required"` is what agent frameworks send), so every
//! scenario of every other client protocol is translated for such a model and
//! must come out as a body the validator accepts under those rules. What the
//! body says is compared with the translation for `claude-sonnet-4-5`, which
//! has neither rule: nothing but the forced choice and the prefill may
//! differ.
//!
//! A Messages client's own request is not part of this: it is replayed as
//! written, and the API's answer to it is the client's to deal with.

mod support;

use serde_json::{Value, json};
use support::harness::{
    ANTHROPIC, CHAT, Failures, GEMINI, RESPONSES, declared_tools, short, translate_request_for,
    validate_request,
};
use support::scenarios;
use switchyard_core::reasoning::{Effort, ModelThinking, ThinkingSupport};
use switchyard_core::{Protocol, UpstreamCtx};

/// A model with both rules.
const NEWEST: &str = "claude-sonnet-5-5";
/// A model with neither.
const BASELINE: &str = "claude-sonnet-4-5";

/// "Claude adaptive-only (newest): `{zero_allowed, dynamic_allowed,
/// levels:[low,medium,high,xhigh,max]}`" (notes 12 §1).
fn newest_caps() -> ThinkingSupport {
    ThinkingSupport {
        min: 0,
        max: 0,
        zero_allowed: true,
        dynamic_allowed: true,
        levels: vec![
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::Xhigh,
            Effort::Max,
        ],
    }
}

fn last_role(body: &Value) -> Option<&str> {
    body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["role"].as_str())
}

fn run_client(client: Protocol) {
    let mut failures = Failures::default();
    let caps = newest_caps();
    let known = UpstreamCtx {
        thinking: ModelThinking::Supported(&caps),
        max_output_tokens: Some(128_000),
        ..UpstreamCtx::default()
    };
    let unknown = UpstreamCtx::default();
    let mut forced_seen = 0;
    let mut prefill_seen = 0;
    for scenario in scenarios::requests(client) {
        for (label, ctx) in [("known", &known), ("unknown", &unknown)] {
            let context = format!("{}/{label}", scenario.name);
            let translate =
                |model: &str| translate_request_for(client, ANTHROPIC, model, &scenario.body, ctx);
            let (baseline, body) = match (translate(BASELINE), translate(NEWEST)) {
                (Ok(baseline), Ok(body)) => (baseline, body),
                (Err(error), _) | (_, Err(error)) => {
                    failures.push(&context, format!("translation failed: {error}"));
                    continue;
                }
            };
            failures.report(&context, validate_request(ANTHROPIC, &body));

            // Forced tool use.
            let choice = &baseline["tool_choice"];
            match choice["type"].as_str() {
                Some("any") => {
                    forced_seen += 1;
                    failures.check(
                        &context,
                        body["tool_choice"]["type"] == "auto",
                        format!("`any` became {}", body["tool_choice"]),
                    );
                    failures.check(
                        &context,
                        body["tools"] == baseline["tools"],
                        "the tool list changed although no single tool was forced",
                    );
                }
                Some("tool") => {
                    forced_seen += 1;
                    failures.check(
                        &context,
                        body["tool_choice"]["type"] == "auto",
                        format!("a forced tool became {}", body["tool_choice"]),
                    );
                    // The model may still call nothing but the forced tool.
                    let declared = declared_tools(ANTHROPIC, &body);
                    failures.check(
                        &context,
                        json!(declared) == json!([choice["name"]]),
                        format!(
                            "{} was forced; the model is offered {declared:?}",
                            choice["name"]
                        ),
                    );
                }
                _ => {
                    failures.check(
                        &context,
                        body.get("tool_choice") == baseline.get("tool_choice")
                            && body.get("tools") == baseline.get("tools"),
                        "tools or tool_choice differ although nothing was forced",
                    );
                }
            }
            // What the choice carries besides its type survives.
            failures.check(
                &context,
                body["tool_choice"].get("disable_parallel_tool_use")
                    == choice.get("disable_parallel_tool_use"),
                "disable_parallel_tool_use was lost",
            );

            // Prefill.
            let baseline_messages = baseline["messages"].as_array().cloned().unwrap_or_default();
            let expected: &[Value] = if last_role(&baseline) == Some("assistant") {
                prefill_seen += 1;
                &baseline_messages[..baseline_messages.len() - 1]
            } else {
                &baseline_messages
            };
            failures.check(
                &context,
                body["messages"].as_array().map(Vec::as_slice) == Some(expected),
                format!(
                    "the messages differ by more than a dropped prefill: {}",
                    body["messages"]
                ),
            );
            failures.check(
                &context,
                last_role(&body) == Some("user"),
                "the conversation does not end with a user message",
            );
        }
    }
    assert!(forced_seen >= 4, "the forced-tool scenarios disappeared");
    if client != GEMINI {
        assert!(prefill_seen >= 2, "the prefill scenario disappeared");
    }
    failures.finish(&format!("{} -> {NEWEST}", short(client)));
}

#[test]
fn chat_to_the_newest_claude() {
    run_client(CHAT);
}

#[test]
fn responses_to_the_newest_claude() {
    run_client(RESPONSES);
}

#[test]
fn gemini_to_the_newest_claude() {
    run_client(GEMINI);
}

/// The count request is held to the same rules ("including on
/// count_tokens").
#[test]
fn count_requests_for_the_newest_claude_force_nothing() {
    use switchyard_codecs::codec;
    let mut failures = Failures::default();
    let mut forced_seen = 0;
    for client in [CHAT, RESPONSES, GEMINI] {
        for scenario in scenarios::requests(client) {
            let context = format!("{} / {}", short(client), scenario.name);
            let mut request = match support::harness::decode_request(client, &scenario.body) {
                Ok(request) => request,
                Err(error) => {
                    failures.push(&context, format!("decode failed: {error}"));
                    continue;
                }
            };
            forced_seen += usize::from(matches!(
                request.tool_choice,
                Some(switchyard_core::ir::ToolChoice::Required)
                    | Some(switchyard_core::ir::ToolChoice::Tool { .. })
            ));
            request.model = NEWEST.to_string();
            let Some(body) =
                codec(ANTHROPIC).encode_count_request(&request, &UpstreamCtx::default())
            else {
                failures.push(&context, "no count request");
                continue;
            };
            let forced = matches!(body["tool_choice"]["type"].as_str(), Some("any" | "tool"));
            failures.check(
                &context,
                !forced,
                format!("the count request forces a tool: {}", body["tool_choice"]),
            );
            failures.check(
                &context,
                last_role(&body) == Some("user"),
                "the count request ends with an assistant message",
            );
        }
    }
    assert!(forced_seen >= 6, "the forced-tool scenarios disappeared");
    failures.finish("count requests for the newest Claude");
}
