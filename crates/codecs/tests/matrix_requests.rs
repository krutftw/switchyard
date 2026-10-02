//! Matrix (a): every client request scenario, for every ordered pair of
//! client protocol `C` and upstream protocol `U` (including `C == U`), goes
//! `C.decode_request` → `U.encode_request` and must come out as a body the
//! upstream vendor accepts and that still says what the client said.
//!
//! Each pair runs twice: with a known target model (capabilities from the
//! catalog shapes of notes 12 §1) and with `ModelThinking::Unknown`.

mod support;

use serde_json::Value;
use support::harness::{
    ANTHROPIC, CHAT, Failures, GEMINI, RESPONSES, declared_tools, history_calls, known_caps,
    no_panic, same_tool, short, translate_request, upstream_model, validate_translated_request,
};
use support::scenarios::{self, ClientRequest};
use switchyard_core::reasoning::Depth;
use switchyard_core::{Protocol, UpstreamCtx};

/// Scenarios whose body the client's *own* vendor would refuse. Translated
/// to another protocol they are repaired; forwarded to the same protocol
/// they are replayed as the client wrote them, so the vendor validator is
/// not applied to that one pair.
fn own_vendor_refuses(client: Protocol, scenario: &str) -> bool {
    matches!(
        (client, scenario),
        // Dots, colons and 100 characters in an OpenAI function name: only
        // Chat-compatible servers take them, and a Chat client's tool names
        // are its own business.
        (
            Protocol::OpenaiChat | Protocol::OpenaiResponses,
            "odd_tool_names"
        )
    )
}

/// The text of the upstream body's system slot.
fn system_slot(upstream: Protocol, body: &Value) -> String {
    match upstream {
        Protocol::OpenaiChat => body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .take_while(|m| m["role"] == "system" || m["role"] == "developer")
            .map(|m| m["content"].to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        Protocol::OpenaiResponses => body["instructions"].to_string(),
        Protocol::Anthropic => body["system"].to_string(),
        Protocol::Gemini => body["systemInstruction"].to_string(),
    }
}

/// What the upstream body says about calling tools, in one vocabulary:
/// `auto`, `none`, `required`, `tool` (one tool forced), or `unset`.
fn tool_mode(upstream: Protocol, body: &Value) -> &'static str {
    let by_name = |mode: &str| match mode {
        "auto" | "AUTO" => "auto",
        "none" | "NONE" => "none",
        "required" | "any" | "ANY" => "required",
        _ => "tool",
    };
    match upstream {
        Protocol::OpenaiChat | Protocol::OpenaiResponses => match &body["tool_choice"] {
            Value::String(mode) => by_name(mode),
            Value::Object(choice) => match choice.get("type").and_then(Value::as_str) {
                // The restriction itself, with its own mode.
                Some("allowed_tools") => {
                    let mode = choice
                        .get("mode")
                        .or_else(|| {
                            choice
                                .get("allowed_tools")
                                .and_then(|spec| spec.get("mode"))
                        })
                        .and_then(Value::as_str)
                        .unwrap_or("auto");
                    by_name(mode)
                }
                _ => "tool",
            },
            _ => "unset",
        },
        Protocol::Anthropic => match body["tool_choice"]["type"].as_str() {
            Some(mode) => by_name(mode),
            None => "unset",
        },
        Protocol::Gemini => {
            let config = &body["toolConfig"]["functionCallingConfig"];
            let one_name = config["allowedFunctionNames"]
                .as_array()
                .is_some_and(|names| names.len() == 1);
            match config["mode"].as_str() {
                Some("ANY") if one_name => "tool",
                Some(mode) => by_name(mode),
                None => "unset",
            }
        }
    }
}

/// Whether the upstream body asks the model to reason, not to reason, or
/// says nothing.
#[derive(Debug, PartialEq, Eq)]
enum Asked {
    Reasoning,
    NoReasoning,
    Nothing,
}

fn reasoning_asked(upstream: Protocol, body: &Value) -> Asked {
    match upstream {
        Protocol::OpenaiChat => match body["reasoning_effort"].as_str() {
            Some("none") => Asked::NoReasoning,
            Some(_) => Asked::Reasoning,
            None => Asked::Nothing,
        },
        Protocol::OpenaiResponses => match body["reasoning"]["effort"].as_str() {
            Some("none") => Asked::NoReasoning,
            Some(_) => Asked::Reasoning,
            None => Asked::Nothing,
        },
        Protocol::Anthropic => match body["thinking"]["type"].as_str() {
            Some("disabled") => Asked::NoReasoning,
            Some(_) => Asked::Reasoning,
            None if body["output_config"]["effort"].is_string() => Asked::Reasoning,
            None => Asked::Nothing,
        },
        Protocol::Gemini => {
            let config = &body["generationConfig"]["thinkingConfig"];
            if config["thinkingBudget"] == 0 {
                Asked::NoReasoning
            } else if config["thinkingBudget"].is_number() || config["thinkingLevel"].is_string() {
                Asked::Reasoning
            } else {
                Asked::Nothing
            }
        }
    }
}

/// What the upstream body must say about reasoning for a client depth, with
/// the representative model of `known_caps` (the exact values are the
/// subject of `matrix_reasoning.rs`).
fn expected_asked(upstream: Protocol, scenario: &ClientRequest) -> Option<Asked> {
    let depth = scenario.depth?;
    // Anthropic refuses thinking next to forced tool use, so the codec leaves
    // it out (notes 12 §7.3, `disableThinkingIfToolChoiceForced`).
    let forced = matches!(
        scenario.name,
        "tool_forced" | "tool_required" | "odd_tool_names"
    );
    Some(match (upstream, depth) {
        (Protocol::Anthropic, Depth::Off) => Asked::NoReasoning,
        (Protocol::Anthropic, _) if forced => Asked::Nothing,
        // The model of `known_caps` is a level-only OpenAI model without
        // `none`: "off" becomes its lowest level (notes 12 §5.2 step 6).
        (Protocol::OpenaiChat | Protocol::OpenaiResponses, Depth::Off) => Asked::Reasoning,
        // Gemini 2.5 pro cannot be switched off: "off" is its minimum budget.
        (Protocol::Gemini, Depth::Off) => Asked::Reasoning,
        // "Auto" — the provider decides — is spelled on the OpenAI protocols
        // by leaving the effort out.
        (Protocol::OpenaiChat | Protocol::OpenaiResponses, Depth::Auto) => Asked::Nothing,
        (_, _) => Asked::Reasoning,
    })
}

fn check_semantics(
    client: Protocol,
    upstream: Protocol,
    scenario: &ClientRequest,
    body: &Value,
    known: bool,
    failures: &mut Failures,
    context: &str,
) {
    let wire = body.to_string();
    for text in &scenario.texts {
        failures.check(
            context,
            wire.contains(text),
            format!("text `{text}` did not reach the upstream body"),
        );
    }
    let system = system_slot(upstream, body);
    for text in &scenario.system {
        failures.check(
            context,
            system.contains(text),
            format!("system text `{text}` is not in the upstream's system slot ({system:.120})"),
        );
    }
    for media in &scenario.media {
        failures.check(
            context,
            wire.contains(media.marker),
            format!("{:?} payload did not reach the upstream body", media.kind),
        );
    }

    let declared = declared_tools(upstream, body);
    failures.check(
        context,
        declared.len() == scenario.tools.len(),
        format!(
            "{} tool(s) declared upstream ({declared:?}) for {} client tool(s)",
            declared.len(),
            scenario.tools.len()
        ),
    );
    for tool in &scenario.tools {
        let matches = declared.iter().filter(|name| same_tool(tool, name)).count();
        failures.check(
            context,
            matches == 1,
            format!("client tool `{tool}` matches {matches} upstream declaration(s): {declared:?}"),
        );
        // A name that is valid for the upstream as it stands must not change.
        if client == upstream && !own_vendor_refuses(client, scenario.name) && tool.len() <= 64 {
            let kept = declared.iter().any(|name| name == tool) || tool.contains(['.', ':']);
            failures.check(
                context,
                kept,
                format!("tool `{tool}` was renamed on its own protocol"),
            );
        }
    }

    // Every call in the history names a tool exactly as the request declares
    // it: an upstream encoder that renames a declaration must rename the
    // calls (and the forced choice, which the validators check) with it.
    for call in history_calls(upstream, body) {
        failures.check(
            context,
            declared.contains(&call),
            format!("the history calls `{call}`, which the request does not declare: {declared:?}"),
        );
    }

    // A client that restricts the model to some of its tools (`allowed_tools`,
    // several `allowedFunctionNames`): the excluded tool is not offered, and
    // the model still has to call one of the others.
    if matches!(scenario.name, "allowed_tools" | "allowed_functions") {
        failures.check(
            context,
            !wire.contains("delete_account"),
            "the tool the client excluded is offered to the model",
        );
        failures.check(
            context,
            tool_mode(upstream, body) == "required",
            format!(
                "the model is no longer required to call a tool: {}",
                tool_mode(upstream, body)
            ),
        );
    }
    // A Messages client that forces its web-search server tool: where the
    // upstream is given a search tool, it must not be told to use no tool.
    if scenario.name == "forced_web_search" && wire.contains("web_search") && upstream != CHAT {
        failures.check(
            context,
            tool_mode(upstream, body) != "none",
            format!("web search was forced and the upstream is told `none`: {body}"),
        );
    }
    if scenario.name == "forced_web_search" && upstream == RESPONSES {
        failures.check(
            context,
            body["tool_choice"] == serde_json::json!({"type": "web_search"}),
            format!(
                "the forced server tool does not force the hosted one: {}",
                body["tool_choice"]
            ),
        );
    }

    if upstream != GEMINI {
        failures.check(
            context,
            body["model"] == upstream_model(upstream),
            format!(
                "model is {} instead of the upstream model id",
                body["model"]
            ),
        );
        let stream = body["stream"].as_bool().unwrap_or(false);
        failures.check(
            context,
            stream == scenario.stream,
            format!(
                "stream is {stream}, the client asked for {}",
                scenario.stream
            ),
        );
    }

    if known && let Some(expected) = expected_asked(upstream, scenario) {
        let asked = reasoning_asked(upstream, body);
        // A tool loop whose signed thinking is not available (another vendor
        // issued it, or none did) cannot keep manual thinking on Anthropic:
        // the codec leaves it out for that request (documented on
        // `drop_manual_thinking_without_turn_start`).
        let loop_without_thinking = upstream == ANTHROPIC
            && client != ANTHROPIC
            && scenario.name == "tool_loop"
            && expected == Asked::Reasoning;
        if !loop_without_thinking {
            failures.check(
                context,
                asked == expected,
                format!("reasoning: upstream body says {asked:?}, expected {expected:?}"),
            );
        }
    }
}

fn run_pair(client: Protocol, upstream: Protocol) {
    let mut failures = Failures::default();
    let caps = known_caps(upstream);
    let known_ctx = caps.ctx();
    let unknown_ctx = UpstreamCtx::default();
    let scenarios = scenarios::requests(client);
    assert!(scenarios.len() >= 20, "the scenario library shrank");
    for scenario in &scenarios {
        for (label, ctx, known) in [
            ("known", &known_ctx, true),
            ("unknown", &unknown_ctx, false),
        ] {
            let context = format!("{}/{label}", scenario.name);
            let translated = no_panic(&context, || {
                translate_request(client, upstream, &scenario.body, ctx)
            });
            let body = match translated {
                Err(panic) => {
                    failures.push(&context, panic);
                    continue;
                }
                Ok(Err(error)) => {
                    failures.push(&context, format!("translation failed: {error}"));
                    continue;
                }
                Ok(Ok(body)) => body,
            };
            if !(client == upstream && own_vendor_refuses(client, scenario.name)) {
                failures.report(
                    &context,
                    validate_translated_request(client, upstream, &body),
                );
            }
            check_semantics(
                client,
                upstream,
                scenario,
                &body,
                known,
                &mut failures,
                &context,
            );
        }
    }
    failures.finish(&format!("{} -> {}", short(client), short(upstream)));
}

macro_rules! pairs {
    ($($name:ident: $client:expr => $upstream:expr;)*) => {
        $(
            #[test]
            fn $name() {
                run_pair($client, $upstream);
            }
        )*
    };
}

pairs! {
    chat_to_chat: CHAT => CHAT;
    chat_to_responses: CHAT => RESPONSES;
    chat_to_anthropic: CHAT => ANTHROPIC;
    chat_to_gemini: CHAT => GEMINI;
    responses_to_chat: RESPONSES => CHAT;
    responses_to_responses: RESPONSES => RESPONSES;
    responses_to_anthropic: RESPONSES => ANTHROPIC;
    responses_to_gemini: RESPONSES => GEMINI;
    anthropic_to_chat: ANTHROPIC => CHAT;
    anthropic_to_responses: ANTHROPIC => RESPONSES;
    anthropic_to_anthropic: ANTHROPIC => ANTHROPIC;
    anthropic_to_gemini: ANTHROPIC => GEMINI;
    gemini_to_chat: GEMINI => CHAT;
    gemini_to_responses: GEMINI => RESPONSES;
    gemini_to_anthropic: GEMINI => ANTHROPIC;
    gemini_to_gemini: GEMINI => GEMINI;
}
