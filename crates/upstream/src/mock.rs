//! The built-in mock provider (`mock://`).
//!
//! It answers canonical requests locally, so the dashboard playground, demos
//! and the test-suite work without any API key or network. The model name
//! selects the behaviour:
//!
//! | model | behaviour |
//! |---|---|
//! | `mock-echo` | repeats the last user text |
//! | `mock-lorem` | three paragraphs of filler text |
//! | `mock-think` | a reasoning block (with a signature), then an answer |
//! | `mock-tools` | calls the first offered function tool; once the conversation ends with a tool result, answers with a summary of it |
//! | `mock-slow` | like `mock-echo`, 300 ms between chunks |
//! | `mock-error-429` | fails: rate limited, `Retry-After` 2 s |
//! | `mock-error-500` | fails: server error |
//! | `mock-error-401` | fails: rejected credential |
//!
//! Any other name behaves like `mock-echo`. Content is deterministic: the
//! same request always produces the same parts, the same response id and the
//! same usage (computed as characters / 4). `max_output_tokens` is honoured
//! for text, ending the response with [`FinishReason::Length`].
//!
//! Streams pause a little between chunks (about 20 ms, 300 ms for
//! `mock-slow`) so that streaming is visible in a UI.
//!
//! # Failing models and streams
//!
//! [`mock_stream`] cannot return an error, so for a failing model it yields
//! `Start` followed by a terminal `Error` event. A caller that wants the
//! failure to count as a failed *attempt* (with failover and cooldowns)
//! should ask [`mock_error`] first.

use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Map, Value, json};
use std::time::Duration;
use switchyard_core::ir::{
    FinishReason, Part, Reasoning, Request, Response, Role, Signature, Tool,
};
use switchyard_core::reasoning::{Depth, Effort, ThinkingSupport};
use switchyard_core::stream::{BlockStart, StreamEvent, response_to_events};
use switchyard_core::util::now_unix;
use switchyard_core::{FailureClass, ModelInfo, Protocol, UpstreamError, UpstreamErrorInfo, Usage};

/// Pause between stream chunks.
const CHUNK_DELAY: Duration = Duration::from_millis(20);

/// Pause between stream chunks of `mock-slow`.
const SLOW_DELAY: Duration = Duration::from_millis(300);

/// Words per text delta.
const WORDS_PER_CHUNK: usize = 3;

/// Characters per tool-argument fragment.
const ARGS_FRAGMENT_CHARS: usize = 12;

/// Wait advertised by `mock-error-429`.
const MOCK_RETRY_AFTER_MS: u64 = 2_000;

const LOREM: [&str; 3] = [
    "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat.",
    "Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur. Excepteur sint occaecat cupidatat non proident, sunt in culpa qui officia deserunt mollit anim id est laborum.",
    "Curabitur pretium tincidunt lacus. Nulla gravida orci a odio. Nullam varius, turpis et commodo pharetra, est eros bibendum elit, nec luctus magna felis sollicitudin mauris. Integer in mauris eu nibh euismod gravida.",
];

/// What a mock model does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Behaviour {
    Echo,
    Lorem,
    Think,
    Tools,
    Slow,
    Error(u16),
}

fn behaviour(model: &str) -> Behaviour {
    let name = model.trim().to_ascii_lowercase();
    // The id may arrive with a provider prefix (`mock/mock-echo`).
    let name = name.rsplit('/').next().unwrap_or(&name);
    if name.starts_with("mock-error-429") {
        Behaviour::Error(429)
    } else if name.starts_with("mock-error-500") {
        Behaviour::Error(500)
    } else if name.starts_with("mock-error-401") {
        Behaviour::Error(401)
    } else if name.starts_with("mock-lorem") {
        Behaviour::Lorem
    } else if name.starts_with("mock-think") {
        Behaviour::Think
    } else if name.starts_with("mock-tools") {
        Behaviour::Tools
    } else if name.starts_with("mock-slow") {
        Behaviour::Slow
    } else {
        Behaviour::Echo
    }
}

/// The models the mock provider serves.
pub fn mock_models() -> Vec<ModelInfo> {
    let entry = |id: &str, name: &str, description: &str| ModelInfo {
        id: id.to_string(),
        display_name: Some(name.to_string()),
        description: Some(description.to_string()),
        owned_by: Some("switchyard".to_string()),
        created: None,
        context_window: Some(128_000),
        max_output_tokens: Some(8_192),
        thinking: None,
        // Unlike discovered models, everything about these is known.
        known: true,
    };
    let mut models = vec![
        entry("mock-echo", "Mock Echo", "Repeats the last user message."),
        entry(
            "mock-lorem",
            "Mock Lorem",
            "Streams three paragraphs of filler text.",
        ),
        entry(
            "mock-think",
            "Mock Think",
            "Emits a reasoning block, then an answer.",
        ),
        entry(
            "mock-tools",
            "Mock Tools",
            "Calls the first offered function tool, then summarises its result.",
        ),
        entry(
            "mock-slow",
            "Mock Slow",
            "Echoes slowly: 300 ms between chunks.",
        ),
        entry(
            "mock-error-429",
            "Mock Error 429",
            "Always fails with a rate-limit error.",
        ),
        entry(
            "mock-error-500",
            "Mock Error 500",
            "Always fails with a server error.",
        ),
        entry(
            "mock-error-401",
            "Mock Error 401",
            "Always fails with an authentication error.",
        ),
    ];
    models[2].thinking = Some(ThinkingSupport {
        zero_allowed: true,
        dynamic_allowed: true,
        ..ThinkingSupport::levels(&[Effort::Low, Effort::Medium, Effort::High])
    });
    models
}

/// The failure a mock model is configured to produce, if any.
pub fn mock_error(request: &Request) -> Option<UpstreamError> {
    let Behaviour::Error(status) = behaviour(&request.model) else {
        return None;
    };
    let (class, error_type, code, message, retry_after_ms) = match status {
        429 => (
            FailureClass::RateLimit,
            "rate_limit_error",
            "rate_limit_exceeded",
            "Mock rate limit reached. Please try again in 2s.",
            Some(MOCK_RETRY_AFTER_MS),
        ),
        401 => (
            FailureClass::Auth,
            "authentication_error",
            "invalid_api_key",
            "Mock credential rejected.",
            None,
        ),
        _ => (
            FailureClass::Server,
            "server_error",
            "internal_error",
            "Mock upstream failure.",
            None,
        ),
    };
    let body = json!({"error": {"message": message, "type": error_type, "code": code}});
    Some(UpstreamError {
        status,
        class,
        info: UpstreamErrorInfo {
            message: message.to_string(),
            error_type: Some(error_type.to_string()),
            code: Some(code.to_string()),
            retry_after_ms,
        },
        retry_after_ms,
        body: Some(body.to_string()),
        content_type: Some("application/json".to_string()),
    })
}

/// Tokens for `chars` characters: a quarter, rounded up.
fn tokens(chars: usize) -> u64 {
    chars.div_ceil(4) as u64
}

fn part_chars(part: &Part) -> usize {
    match part {
        Part::Text(t) => t.text.chars().count(),
        Part::Reasoning(r) => r.text.chars().count(),
        Part::Refusal(r) => r.text.chars().count(),
        Part::ToolCall(c) => c.name.chars().count() + c.arguments.chars().count(),
        Part::ToolResult(r) => r.content.iter().map(part_chars).sum(),
        // Media is not "read" by the mock; count a nominal cost.
        Part::Image(_) | Part::Audio(_) | Part::Document(_) => 256 * 4,
        Part::Opaque(o) => o.raw.to_string().chars().count(),
    }
}

fn prompt_chars(request: &Request) -> usize {
    let system: usize = request.system.iter().map(part_chars).sum();
    let messages: usize = request
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .map(part_chars)
        .sum();
    let tools: usize = request
        .tools
        .iter()
        .map(|tool| match tool {
            Tool::Function(f) => {
                f.name.chars().count()
                    + f.description.as_deref().map_or(0, |d| d.chars().count())
                    + if f.parameters.is_null() {
                        0
                    } else {
                        f.parameters.to_string().chars().count()
                    }
            }
            Tool::Custom(c) => {
                c.name.chars().count() + c.description.as_deref().map_or(0, |d| d.chars().count())
            }
            Tool::Builtin(b) => b.raw.to_string().chars().count(),
        })
        .sum();
    system + messages + tools
}

/// Text of the most recent user message that has any.
fn last_user_text(request: &Request) -> Option<String> {
    request
        .messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::User)
        .map(|m| m.text())
        .find(|text| !text.trim().is_empty())
}

/// A deterministic sample value for a JSON Schema.
fn sample_value(name: &str, schema: &Value) -> Value {
    if let Some(first) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|e| e.first())
    {
        return first.clone();
    }
    if let Some(constant) = schema.get("const") {
        return constant.clone();
    }
    // `"type"` may be a list (`["string","null"]`): use the first real type.
    let kind = match schema.get("type") {
        Some(Value::String(s)) => s.as_str(),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .find(|k| *k != "null")
            .unwrap_or("string"),
        _ => "string",
    };
    match kind {
        "integer" | "number" => schema
            .get("minimum")
            .filter(|m| m.is_number())
            .cloned()
            .unwrap_or_else(|| json!(1)),
        "boolean" => json!(true),
        "array" => json!([]),
        "object" => Value::Object(sample_arguments(schema)),
        "null" => Value::Null,
        _ => Value::String(format!("mock-{name}")),
    }
}

/// Arguments satisfying a tool's schema: every `required` property gets a
/// deterministic value (`"mock-<name>"` for strings, the first `enum` value
/// where there is one).
fn sample_arguments(schema: &Value) -> Map<String, Value> {
    let mut arguments = Map::new();
    let properties = schema.get("properties").and_then(Value::as_object);
    let required = schema.get("required").and_then(Value::as_array);
    for name in required.into_iter().flatten().filter_map(Value::as_str) {
        let property = properties.and_then(|p| p.get(name)).unwrap_or(&Value::Null);
        arguments.insert(name.to_string(), sample_value(name, property));
    }
    arguments
}

/// The tool results the conversation ends with, when it ends with any.
fn trailing_tool_results(request: &Request) -> Vec<(String, String)> {
    let Some(last) = request.messages.last().filter(|m| m.role == Role::User) else {
        return Vec::new();
    };
    last.tool_results()
        .map(|result| {
            let name = result
                .name
                .clone()
                .or_else(|| {
                    request
                        .tool_name_for_call(&result.call_id)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "tool".to_string());
            (name, result.text())
        })
        .collect()
}

/// FNV-1a, for deterministic ids.
fn fnv1a(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.bytes().chain(std::iter::once(0)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// Cuts `text` to at most `max_chars` characters.
fn clip(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        (text.to_string(), false)
    } else {
        (text.chars().take(max_chars).collect(), true)
    }
}

/// Builds the complete answer for `request`. Pure apart from the timestamp.
fn build_response(request: &Request, upstream_protocol: Protocol) -> Response {
    let kind = behaviour(&request.model);
    let last_text = last_user_text(request);
    let fingerprint = fnv1a(&[
        &request.model,
        last_text.as_deref().unwrap_or(""),
        &request.messages.len().to_string(),
    ]);
    let mut response = Response::new(format!("mock-{fingerprint:016x}"), request.model.clone());
    response.created = now_unix();

    let mut parts: Vec<Part> = Vec::new();
    let mut finish = FinishReason::Stop;
    let mut text: Option<String> = None;

    match kind {
        Behaviour::Echo | Behaviour::Slow | Behaviour::Error(_) => {
            text = Some(last_text.unwrap_or_else(|| {
                "Hello from the Switchyard mock provider. Send a message and it will be echoed."
                    .to_string()
            }));
        }
        Behaviour::Lorem => text = Some(LOREM.join("\n\n")),
        Behaviour::Think => {
            let topic = last_text.unwrap_or_else(|| "nothing in particular".to_string());
            let (topic, _) = clip(topic.trim(), 200);
            let thinking_off = request
                .reasoning
                .as_ref()
                .is_some_and(|r| matches!(r.depth, Some(Depth::Off)));
            if !thinking_off {
                let thoughts = format!(
                    "The user wrote: \"{topic}\". Let me think about this step by step. First, I restate the question. Second, I consider what a helpful answer looks like. Third, I write it down."
                );
                parts.push(Part::Reasoning(Reasoning {
                    id: None,
                    signature: Some(Signature::new(
                        upstream_protocol,
                        format!("mock-signature-{:016x}", fnv1a(&[&thoughts])),
                    )),
                    text: thoughts,
                    redacted: false,
                }));
            }
            text = Some(format!(
                "After thinking it over, here is my answer to \"{topic}\": everything checks out."
            ));
        }
        Behaviour::Tools => {
            let results = trailing_tool_results(request);
            let function = request.tools.iter().find_map(|tool| match tool {
                Tool::Function(f) => Some(f),
                _ => None,
            });
            let may_call = !matches!(request.tool_choice, Some(switchyard_core::ToolChoice::None));
            if !results.is_empty() {
                let summary: Vec<String> = results
                    .iter()
                    .map(|(name, output)| {
                        let (output, cut) = clip(output.trim(), 400);
                        let ellipsis = if cut { "…" } else { "" };
                        format!("The tool `{name}` returned: {output}{ellipsis}")
                    })
                    .collect();
                text = Some(summary.join("\n"));
            } else if let (Some(function), true) = (function, may_call) {
                let arguments = Value::Object(sample_arguments(&function.parameters_or_empty()));
                let calls_so_far = request
                    .messages
                    .iter()
                    .map(|m| m.tool_calls().count())
                    .sum::<usize>();
                parts.push(Part::tool_call(
                    format!("call_mock_{:04}", calls_so_far + 1),
                    function.name.clone(),
                    arguments.to_string(),
                ));
                finish = FinishReason::ToolCalls;
            } else {
                text = Some(
                    "No function tool was offered (or tool use was disabled), so there is nothing to call."
                        .to_string(),
                );
            }
        }
    }

    if let Some(text) = text {
        // The limit covers reasoning too, as it does for real providers.
        let spent: u64 = parts.iter().map(|p| tokens(part_chars(p))).sum();
        let text = match request.max_output_tokens {
            Some(limit) => {
                let room = limit.saturating_sub(spent).min(u64::from(u32::MAX)) as usize;
                let (clipped, cut) = clip(&text, room.saturating_mul(4));
                if cut {
                    finish = FinishReason::Length;
                }
                clipped
            }
            None => text,
        };
        if !text.is_empty() || parts.is_empty() {
            parts.push(Part::text(text));
        }
    }

    let reasoning_chars: usize = parts
        .iter()
        .filter(|p| matches!(p, Part::Reasoning(_)))
        .map(part_chars)
        .sum();
    let output_tokens: u64 = parts.iter().map(|p| tokens(part_chars(p))).sum();
    response.usage = Usage {
        input_tokens: tokens(prompt_chars(request)).max(1),
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens,
        reasoning_tokens: tokens(reasoning_chars).min(output_tokens),
    };
    response.parts = parts;
    response.finish = finish;
    response
}

/// Splits text into chunks of a few words, keeping every character.
fn word_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut words = 0;
    let mut in_word = false;
    for c in text.chars() {
        let is_space = c.is_whitespace();
        if in_word && is_space {
            words += 1;
        }
        // Start a new chunk at the first character of the next word.
        if !is_space && !in_word && words >= WORDS_PER_CHUNK {
            chunks.push(std::mem::take(&mut current));
            words = 0;
        }
        in_word = !is_space;
        current.push(c);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn char_chunks(text: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(size.max(1))
        .map(|c| c.iter().collect())
        .collect()
}

/// The event sequence for a complete response, with deltas cut into small
/// pieces. Accumulating it yields `response` again.
fn chunked_events(response: &Response) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    for event in response_to_events(response) {
        match event {
            StreamEvent::TextDelta { index, text } => {
                events.extend(
                    word_chunks(&text)
                        .into_iter()
                        .map(|text| StreamEvent::TextDelta { index, text }),
                );
            }
            StreamEvent::ReasoningDelta { index, text } => {
                events.extend(
                    word_chunks(&text)
                        .into_iter()
                        .map(|text| StreamEvent::ReasoningDelta { index, text }),
                );
            }
            StreamEvent::ToolArgsDelta { index, fragment } => {
                events.extend(
                    char_chunks(&fragment, ARGS_FRAGMENT_CHARS)
                        .into_iter()
                        .map(|fragment| StreamEvent::ToolArgsDelta { index, fragment }),
                );
            }
            other => events.push(other),
        }
    }
    events
}

fn is_delta(event: &StreamEvent) -> bool {
    matches!(
        event,
        StreamEvent::TextDelta { .. }
            | StreamEvent::ReasoningDelta { .. }
            | StreamEvent::ToolArgsDelta { .. }
    )
}

/// Streams the mock's answer to `request` as canonical events.
///
/// `upstream_protocol` is the protocol the mock is standing in for; it
/// becomes the origin of the reasoning signature `mock-think` emits, so the
/// signature is treated like one issued by a real upstream of that protocol.
///
/// The sequence always satisfies the contract of
/// [`switchyard_core::stream`]. For a failing model it is `Start`, `Error`
/// (see the module docs).
pub fn mock_stream(
    request: &Request,
    upstream_protocol: Protocol,
) -> BoxStream<'static, StreamEvent> {
    if let Some(error) = mock_error(request) {
        let start = StreamEvent::Start {
            id: String::new(),
            model: request.model.clone(),
            created: now_unix(),
        };
        return futures::stream::iter([start, StreamEvent::Error(error.to_api_error())]).boxed();
    }
    let delay = match behaviour(&request.model) {
        Behaviour::Slow => SLOW_DELAY,
        _ => CHUNK_DELAY,
    };
    let events = chunked_events(&build_response(request, upstream_protocol));
    async_stream::stream! {
        for event in events {
            // Pause before each piece of content, so the first one also
            // takes a moment (time to first token) and nothing trails the
            // last one.
            if is_delta(&event) || matches!(event, StreamEvent::BlockStart { block: BlockStart::Whole { .. }, .. }) {
                tokio::time::sleep(delay).await;
            }
            yield event;
        }
    }
    .boxed()
}

/// Answers `request` in one piece.
///
/// Failing models return their [`UpstreamError`]. `upstream_protocol` has
/// the same meaning as for [`mock_stream`].
pub async fn mock_response(
    request: &Request,
    upstream_protocol: Protocol,
) -> Result<Response, UpstreamError> {
    let delay = match behaviour(&request.model) {
        Behaviour::Slow => SLOW_DELAY,
        _ => CHUNK_DELAY,
    };
    // A real upstream never answers instantly either.
    tokio::time::sleep(delay).await;
    match mock_error(request) {
        Some(error) => Err(error),
        None => Ok(build_response(request, upstream_protocol)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use switchyard_core::ir::{FunctionTool, Message, ToolChoice};
    use switchyard_core::reasoning::ReasoningConfig;
    use switchyard_core::stream::{Accumulator, validate_sequence};

    fn request(model: &str, user: &str) -> Request {
        let mut r = Request::new(model, Protocol::OpenaiChat);
        r.messages.push(Message::user_text(user));
        r
    }

    async fn collect(request: &Request, protocol: Protocol) -> Vec<StreamEvent> {
        mock_stream(request, protocol).collect().await
    }

    fn accumulate(events: &[StreamEvent]) -> Response {
        let mut acc = Accumulator::new();
        for event in events {
            acc.push(event);
        }
        acc.into_response()
    }

    fn weather_tool() -> Tool {
        Tool::Function(FunctionTool {
            name: "get_weather".into(),
            description: Some("Look up the weather.".into()),
            parameters: json!({
                "type": "object",
                "properties": {
                    "city": {"type": "string"},
                    "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]},
                    "days": {"type": "integer", "minimum": 3},
                    "detailed": {"type": "boolean"},
                    "note": {"type": "string"}
                },
                "required": ["city", "unit", "days", "detailed"]
            }),
            strict: None,
            cache_control: None,
        })
    }

    #[test]
    fn model_list() {
        let models = mock_models();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "mock-echo",
                "mock-lorem",
                "mock-think",
                "mock-tools",
                "mock-slow",
                "mock-error-429",
                "mock-error-500",
                "mock-error-401"
            ]
        );
        assert!(models.iter().all(|m| m.known && m.display_name.is_some()));
        assert!(models[2].thinking.is_some());
        assert!(models[0].thinking.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn every_model_streams_a_valid_sequence_that_matches_the_full_response() {
        for model in mock_models() {
            let mut req = request(&model.id, "What is the capital of France?");
            req.tools.push(weather_tool());
            for protocol in Protocol::ALL {
                let events = collect(&req, protocol).await;
                validate_sequence(&events)
                    .unwrap_or_else(|e| panic!("{} over {protocol}: {e}", model.id));
                if mock_error(&req).is_some() {
                    assert!(matches!(events.last(), Some(StreamEvent::Error(_))));
                    assert!(mock_response(&req, protocol).await.is_err());
                    continue;
                }
                let streamed = accumulate(&events);
                let mut full = mock_response(&req, protocol).await.unwrap();
                // The timestamp is the only thing allowed to differ.
                full.created = streamed.created;
                assert_eq!(streamed, full, "{} over {protocol}", model.id);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn echo_repeats_the_last_user_text() {
        let mut req = request("mock-echo", "first");
        req.messages.push(Message::assistant_text("reply"));
        req.messages.push(Message::user_text(
            "The quick brown fox jumps over the lazy dog.",
        ));
        let response = mock_response(&req, Protocol::OpenaiChat).await.unwrap();
        assert_eq!(
            response.text(),
            "The quick brown fox jumps over the lazy dog."
        );
        assert_eq!(response.finish, FinishReason::Stop);
        assert_eq!(response.model, "mock-echo");
        assert!(response.id.starts_with("mock-"));

        // Streaming delivers it in several deltas.
        let events = collect(&req, Protocol::OpenaiChat).await;
        let deltas = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::TextDelta { .. }))
            .count();
        assert_eq!(deltas, 3);
        assert_eq!(accumulate(&events).text(), response.text());

        // Deterministic.
        let again = mock_response(&req, Protocol::OpenaiChat).await.unwrap();
        assert_eq!(again.id, response.id);
        assert_eq!(again.parts, response.parts);
        assert_eq!(again.usage, response.usage);
    }

    #[tokio::test(start_paused = true)]
    async fn echo_without_user_text_and_unknown_models() {
        let empty = Request::new("mock-echo", Protocol::Gemini);
        let response = mock_response(&empty, Protocol::Gemini).await.unwrap();
        assert!(
            response
                .text()
                .starts_with("Hello from the Switchyard mock provider")
        );

        let other = request("mock/Something-Else", "hi there");
        assert_eq!(
            mock_response(&other, Protocol::Gemini)
                .await
                .unwrap()
                .text(),
            "hi there"
        );
        let prefixed = request("demo/mock-lorem", "hi");
        assert!(
            mock_response(&prefixed, Protocol::Gemini)
                .await
                .unwrap()
                .text()
                .starts_with("Lorem")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn usage_is_a_quarter_of_the_characters() {
        // 40 characters in, 40 characters out.
        let text = "0123456789012345678901234567890123456789";
        let req = request("mock-echo", text);
        let response = mock_response(&req, Protocol::OpenaiChat).await.unwrap();
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 10,
                ..Usage::default()
            }
        );
        // Rounded up, never zero on the input side.
        let short = mock_response(&request("mock-echo", "hello"), Protocol::OpenaiChat)
            .await
            .unwrap();
        assert_eq!(
            (short.usage.input_tokens, short.usage.output_tokens),
            (2, 2)
        );
        let empty = Request::new("mock-echo", Protocol::OpenaiChat);
        assert_eq!(
            mock_response(&empty, Protocol::OpenaiChat)
                .await
                .unwrap()
                .usage
                .input_tokens,
            1
        );
        // The stream reports the same numbers.
        let events = collect(&req, Protocol::OpenaiChat).await;
        assert_eq!(accumulate(&events).usage, response.usage);
    }

    #[tokio::test(start_paused = true)]
    async fn lorem_is_three_paragraphs() {
        let response = mock_response(&request("mock-lorem", "go"), Protocol::Anthropic)
            .await
            .unwrap();
        let text = response.text();
        assert_eq!(text.split("\n\n").count(), 3);
        assert!(text.starts_with("Lorem ipsum"));
        let events = collect(&request("mock-lorem", "go"), Protocol::Anthropic).await;
        assert!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::TextDelta { .. }))
                .count()
                > 20
        );
        assert_eq!(accumulate(&events).text(), text);
    }

    #[tokio::test(start_paused = true)]
    async fn think_emits_signed_reasoning_before_the_answer() {
        let req = request("mock-think", "Why is the sky blue?");
        for protocol in Protocol::ALL {
            let events = collect(&req, protocol).await;
            validate_sequence(&events).unwrap();
            // Reasoning block first, then text.
            assert!(matches!(
                &events[1],
                StreamEvent::BlockStart {
                    index: 0,
                    block: BlockStart::Reasoning {
                        redacted: false,
                        ..
                    }
                }
            ));
            let signature = events.iter().find_map(|e| match e {
                StreamEvent::ReasoningSignature { signature, .. } => Some(signature.clone()),
                _ => None,
            });
            let signature = signature.expect("a signature");
            assert_eq!(signature.origin, protocol);
            assert!(signature.data.starts_with("mock-signature-"));
            assert!(signature.valid_for(protocol));

            let response = accumulate(&events);
            assert_eq!(response.parts.len(), 2);
            assert!(response.reasoning_text().contains("Why is the sky blue?"));
            assert!(response.text().contains("Why is the sky blue?"));
            assert!(response.usage.reasoning_tokens > 0);
            assert!(response.usage.reasoning_tokens < response.usage.output_tokens);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn think_respects_reasoning_off() {
        let mut req = request("mock-think", "quick question");
        req.reasoning = Some(ReasoningConfig::with_depth(Depth::Off));
        let response = mock_response(&req, Protocol::Anthropic).await.unwrap();
        assert_eq!(response.parts.len(), 1);
        assert_eq!(response.usage.reasoning_tokens, 0);
        assert!(response.reasoning_text().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn tools_calls_the_first_function_with_schema_shaped_arguments() {
        let mut req = request("mock-tools", "What's the weather in Paris?");
        req.tools
            .push(Tool::Builtin(switchyard_core::ir::BuiltinTool {
                kind: switchyard_core::ir::BuiltinKind::WebSearch,
                origin: Protocol::Anthropic,
                raw: json!({"type": "web_search_20250305", "name": "web_search"}),
            }));
        req.tools.push(weather_tool());
        req.tools.push(Tool::Function(FunctionTool {
            name: "second".into(),
            description: None,
            parameters: Value::Null,
            strict: None,
            cache_control: None,
        }));

        let events = collect(&req, Protocol::OpenaiResponses).await;
        validate_sequence(&events).unwrap();
        let response = accumulate(&events);
        assert_eq!(response.finish, FinishReason::ToolCalls);
        let calls: Vec<_> = response.tool_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].id, "call_mock_0001");
        // Required properties only, in schema order, with deterministic
        // values; the optional `note` is left out.
        assert_eq!(
            calls[0].arguments,
            r#"{"city":"mock-city","unit":"celsius","days":3,"detailed":true}"#
        );
        // The arguments arrive in fragments.
        assert!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::ToolArgsDelta { .. }))
                .count()
                > 1
        );
        assert!(response.text().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn tools_answers_once_a_result_is_present() {
        let mut req = request("mock-tools", "What's the weather in Paris?");
        req.tools.push(weather_tool());
        req.messages.push(Message::new(
            Role::Assistant,
            vec![Part::tool_call(
                "call_mock_0001",
                "get_weather",
                r#"{"city":"mock-city"}"#,
            )],
        ));
        req.messages.push(Message::new(
            Role::User,
            vec![Part::tool_result_text("call_mock_0001", "18°C and sunny")],
        ));
        let response = mock_response(&req, Protocol::OpenaiChat).await.unwrap();
        assert_eq!(response.finish, FinishReason::Stop);
        assert_eq!(response.tool_calls().count(), 0);
        assert_eq!(
            response.text(),
            "The tool `get_weather` returned: 18°C and sunny"
        );

        // A follow-up user message after the result starts a new round.
        req.messages.push(Message::user_text("And tomorrow?"));
        let response = mock_response(&req, Protocol::OpenaiChat).await.unwrap();
        assert_eq!(response.finish, FinishReason::ToolCalls);
        assert_eq!(response.tool_calls().next().unwrap().id, "call_mock_0002");
    }

    #[tokio::test(start_paused = true)]
    async fn tools_without_a_callable_function() {
        let none_offered = request("mock-tools", "hi");
        let response = mock_response(&none_offered, Protocol::OpenaiChat)
            .await
            .unwrap();
        assert_eq!(response.finish, FinishReason::Stop);
        assert!(response.text().contains("nothing to call"));

        let mut forbidden = request("mock-tools", "hi");
        forbidden.tools.push(weather_tool());
        forbidden.tool_choice = Some(ToolChoice::None);
        let response = mock_response(&forbidden, Protocol::OpenaiChat)
            .await
            .unwrap();
        assert_eq!(response.tool_calls().count(), 0);

        // A tool without a schema is called with empty arguments.
        let mut bare = request("mock-tools", "hi");
        bare.tools.push(Tool::Function(FunctionTool {
            name: "ping".into(),
            description: None,
            parameters: Value::Null,
            strict: None,
            cache_control: None,
        }));
        let response = mock_response(&bare, Protocol::OpenaiChat).await.unwrap();
        assert_eq!(response.tool_calls().next().unwrap().arguments, "{}");
    }

    #[test]
    fn sample_values_follow_the_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "s": {"type": "string"},
                "e": {"enum": ["a", "b"]},
                "c": {"const": 7},
                "n": {"type": "number"},
                "nullable": {"type": ["null", "integer"]},
                "list": {"type": "array", "items": {"type": "string"}},
                "nested": {"type": "object", "properties": {"x": {"type": "string"}, "y": {"type": "string"}}, "required": ["x"]},
                "untyped": {}
            },
            "required": ["s", "e", "c", "n", "nullable", "list", "nested", "untyped", "undeclared"]
        });
        assert_eq!(
            Value::Object(sample_arguments(&schema)),
            json!({
                "s": "mock-s",
                "e": "a",
                "c": 7,
                "n": 1,
                "nullable": 1,
                "list": [],
                "nested": {"x": "mock-x"},
                "untyped": "mock-untyped",
                "undeclared": "mock-undeclared"
            })
        );
        assert!(sample_arguments(&json!({"type": "object"})).is_empty());
        assert!(sample_arguments(&Value::Null).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn max_output_tokens_truncates_text() {
        let mut req = request("mock-lorem", "go");
        req.max_output_tokens = Some(5);
        let response = mock_response(&req, Protocol::OpenaiChat).await.unwrap();
        assert_eq!(response.finish, FinishReason::Length);
        assert_eq!(response.text().chars().count(), 20);
        assert_eq!(response.usage.output_tokens, 5);
        let events = collect(&req, Protocol::OpenaiChat).await;
        validate_sequence(&events).unwrap();
        assert_eq!(accumulate(&events).finish, FinishReason::Length);

        // A generous limit changes nothing.
        req.max_output_tokens = Some(100_000);
        let response = mock_response(&req, Protocol::OpenaiChat).await.unwrap();
        assert_eq!(response.finish, FinishReason::Stop);
    }

    #[test]
    fn error_models() {
        let e = mock_error(&request("mock-error-429", "x")).unwrap();
        assert_eq!(e.status, 429);
        assert_eq!(e.class, FailureClass::RateLimit);
        assert_eq!(e.retry_after_ms, Some(2_000));
        assert_eq!(e.info.retry_after_ms, Some(2_000));
        assert_eq!(e.info.error_type.as_deref(), Some("rate_limit_error"));
        assert_eq!(e.content_type.as_deref(), Some("application/json"));
        // The body is what classify() would make the same error from.
        let reclassified = crate::classify(
            switchyard_core::config::ProviderKind::Mock,
            Protocol::OpenaiChat,
            429,
            &http::HeaderMap::new(),
            e.body.as_deref().unwrap().as_bytes(),
        );
        assert_eq!(reclassified.class, e.class);
        assert_eq!(reclassified.retry_after_ms, e.retry_after_ms);
        assert_eq!(reclassified.info.message, e.info.message);

        let e = mock_error(&request("mock-error-500", "x")).unwrap();
        assert_eq!(
            (e.status, e.class, e.retry_after_ms),
            (500, FailureClass::Server, None)
        );
        let e = mock_error(&request("mock-error-401", "x")).unwrap();
        assert_eq!(
            (e.status, e.class, e.retry_after_ms),
            (401, FailureClass::Auth, None)
        );
        assert!(mock_error(&request("mock-echo", "x")).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn error_models_fail_in_both_modes() {
        let req = request("mock-error-429", "x");
        let err = mock_response(&req, Protocol::OpenaiChat).await.unwrap_err();
        assert_eq!(err.status, 429);
        let events = collect(&req, Protocol::OpenaiChat).await;
        validate_sequence(&events).unwrap();
        assert_eq!(events.len(), 2);
        match &events[1] {
            StreamEvent::Error(api) => {
                assert_eq!(api.status, 429);
                assert_eq!(api.retry_after_secs, Some(2));
            }
            other => panic!("expected an error event, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn chunks_are_spaced_out_and_slow_is_slower() {
        let fast = request("mock-echo", "one two three four five six seven");
        let started = tokio::time::Instant::now();
        let events = collect(&fast, Protocol::OpenaiChat).await;
        let fast_elapsed = started.elapsed();
        let deltas = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::TextDelta { .. }))
            .count();
        assert_eq!(deltas, 3);
        assert_eq!(fast_elapsed, CHUNK_DELAY * 3);

        let slow = request("mock-slow", "one two three four five six seven");
        let started = tokio::time::Instant::now();
        let events = collect(&slow, Protocol::OpenaiChat).await;
        assert_eq!(started.elapsed(), SLOW_DELAY * 3);
        assert_eq!(
            accumulate(&events).text(),
            "one two three four five six seven"
        );

        let started = tokio::time::Instant::now();
        mock_response(&slow, Protocol::OpenaiChat).await.unwrap();
        assert_eq!(started.elapsed(), SLOW_DELAY);
    }

    #[test]
    fn chunking_preserves_every_character() {
        for text in [
            "",
            "one",
            "one two three four five six seven",
            "  leading and   multiple   spaces  ",
            "line one\n\nline two\twith tab",
            "ünïcödé wörds äre fine 你好 世界 again and again",
        ] {
            let chunks = word_chunks(text);
            assert_eq!(chunks.concat(), text, "{text:?}");
            assert!(chunks.iter().all(|c| !c.is_empty()));
        }
        assert_eq!(word_chunks("a b c d e f g"), ["a b c ", "d e f ", "g"]);
        assert_eq!(char_chunks("abcdefg", 3), ["abc", "def", "g"]);
        assert!(char_chunks("", 3).is_empty());
    }
}
