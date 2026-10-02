//! The translation pipeline, as the gateway runs it for a translated request
//! (`docs/DESIGN.md` §2): `C.decode_request` → fit reasoning →
//! `U.encode_request` → upstream → `U.decode_response` / `U.stream_decoder` →
//! `C.encode_response` / `C.stream_encoder`.

use super::validators::{self, Report};
use serde_json::Value;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use switchyard_codecs::codec;
use switchyard_core::ir::{Request, Response};
use switchyard_core::reasoning::{Effort, Fitted, ModelThinking, ThinkingSupport, normalize_depth};
use switchyard_core::stream::{Accumulator, StreamEvent, validate_sequence};
use switchyard_core::{
    ClientCtx, CodecError, Protocol, RequestPath, SseEvent, SseParser, UpstreamCtx,
};

pub const CHAT: Protocol = Protocol::OpenaiChat;
pub const RESPONSES: Protocol = Protocol::OpenaiResponses;
pub const ANTHROPIC: Protocol = Protocol::Anthropic;
pub const GEMINI: Protocol = Protocol::Gemini;

/// Model name a client of each protocol asks for in the scenarios.
pub fn client_model(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenaiChat => "gpt-5.5",
        Protocol::OpenaiResponses => "gpt-5.5",
        Protocol::Anthropic => "claude-sonnet-4-5",
        Protocol::Gemini => "gemini-2.5-pro",
    }
}

/// Upstream model id requests are routed to. The Anthropic one names a
/// Claude model on purpose: the codec applies Anthropic's own validation
/// rules by model family.
pub fn upstream_model(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenaiChat => "gpt-5.5",
        Protocol::OpenaiResponses => "gpt-5.5",
        Protocol::Anthropic => "claude-sonnet-4-5",
        Protocol::Gemini => "gemini-2.5-pro",
    }
}

/// Reasoning capabilities and output limit of a representative model per
/// upstream protocol, from the catalog shapes in notes 12 §1.
pub struct ModelCaps {
    pub thinking: ThinkingSupport,
    pub max_output_tokens: u64,
}

impl ModelCaps {
    pub fn ctx(&self) -> UpstreamCtx<'_> {
        UpstreamCtx {
            thinking: ModelThinking::Supported(&self.thinking),
            max_output_tokens: Some(self.max_output_tokens),
            ..UpstreamCtx::default()
        }
    }
}

pub fn known_caps(upstream: Protocol) -> ModelCaps {
    match upstream {
        // "OpenAI reasoning models: {levels:[low,medium,high,xhigh]}".
        Protocol::OpenaiChat | Protocol::OpenaiResponses => ModelCaps {
            thinking: ThinkingSupport::levels(&[
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::Xhigh,
            ]),
            max_output_tokens: 128_000,
        },
        // "Claude manual-only (4.5 line): {min:1024,max:128000,zero_allowed}".
        Protocol::Anthropic => ModelCaps {
            thinking: ThinkingSupport {
                min: 1024,
                max: 128_000,
                zero_allowed: true,
                dynamic_allowed: false,
                levels: Vec::new(),
            },
            max_output_tokens: 64_000,
        },
        // "Gemini 2.5 pro: {min:128,max:32768,dynamic_allowed}".
        Protocol::Gemini => ModelCaps {
            thinking: ThinkingSupport {
                min: 128,
                max: 32_768,
                zero_allowed: false,
                dynamic_allowed: true,
                levels: Vec::new(),
            },
            max_output_tokens: 65_536,
        },
    }
}

/// The facts a client request carries in its URL. Only Gemini has any.
pub fn request_path(client: Protocol, stream: bool) -> RequestPath<'static> {
    match client {
        Protocol::Gemini => RequestPath {
            model: Some(client_model(Protocol::Gemini)),
            stream: Some(stream),
        },
        _ => RequestPath::default(),
    }
}

/// `C.decode_request` for a scenario body.
pub fn decode_request(client: Protocol, body: &Value) -> Result<Request, CodecError> {
    // The Gemini scenarios that stream say so with a private marker, since
    // the protocol has no body field for it.
    let stream = body
        .get("__stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut body = body.clone();
    if let Some(root) = body.as_object_mut() {
        root.shift_remove("__stream");
    }
    codec(client).decode_request(&body, &request_path(client, stream))
}

/// What `switchyard_translate::thinking` does on the translation path: a
/// depth the client asked for is fitted to the target model, nothing is
/// invented when it asked for none.
pub fn fit_reasoning(request: &mut Request, thinking: ModelThinking<'_>, target: Protocol) {
    let Some(depth) = request.reasoning.as_ref().and_then(|r| r.depth) else {
        return;
    };
    match normalize_depth(depth, thinking, target) {
        Fitted::Use(depth) => {
            if let Some(reasoning) = &mut request.reasoning {
                reasoning.depth = Some(depth);
            }
        }
        Fitted::Strip => {
            if let Some(reasoning) = &mut request.reasoning {
                reasoning.depth = None;
                if reasoning.is_empty() {
                    request.reasoning = None;
                }
            }
        }
    }
}

/// Client body → upstream body, with the model routed to `upstream`'s.
pub fn translate_request(
    client: Protocol,
    upstream: Protocol,
    body: &Value,
    ctx: &UpstreamCtx<'_>,
) -> Result<Value, CodecError> {
    translate_request_for(client, upstream, upstream_model(upstream), body, ctx)
}

/// Client body → upstream body for the upstream model `model`.
pub fn translate_request_for(
    client: Protocol,
    upstream: Protocol,
    model: &str,
    body: &Value,
    ctx: &UpstreamCtx<'_>,
) -> Result<Value, CodecError> {
    let mut request = decode_request(client, body)?;
    request.model = model.to_string();
    fit_reasoning(&mut request, ctx.thinking, upstream);
    codec(upstream).encode_request(&request, ctx)
}

pub fn validate_request(protocol: Protocol, body: &Value) -> Report {
    match protocol {
        Protocol::OpenaiChat => validators::validate_chat_request(body),
        Protocol::OpenaiResponses => validators::validate_responses_request(body),
        Protocol::Anthropic => validators::validate_anthropic_request(body),
        Protocol::Gemini => validators::validate_gemini_request(body),
    }
}

/// Validates a body that was translated from `client`'s protocol for an
/// upstream of `upstream`'s. The same as [`validate_request`], except that a
/// Chat body made from a request of another protocol has to suit an
/// OpenAI-*compatible* server: a request is only translated to Chat when the
/// provider speaks nothing else
/// ([`validators::validate_chat_compatible_request`]).
pub fn validate_translated_request(client: Protocol, upstream: Protocol, body: &Value) -> Report {
    if upstream == Protocol::OpenaiChat && client != Protocol::OpenaiChat {
        validators::validate_chat_compatible_request(body)
    } else {
        validate_request(upstream, body)
    }
}

pub fn validate_response(protocol: Protocol, body: &Value) -> Report {
    match protocol {
        Protocol::OpenaiChat => validators::validate_chat_response(body),
        Protocol::OpenaiResponses => validators::validate_responses_response(body),
        Protocol::Anthropic => validators::validate_anthropic_response(body),
        Protocol::Gemini => validators::validate_gemini_response(body),
    }
}

pub fn validate_stream(protocol: Protocol, events: &[SseEvent]) -> Report {
    match protocol {
        Protocol::OpenaiChat => validators::validate_chat_stream(events),
        Protocol::OpenaiResponses => validators::validate_responses_stream(events),
        Protocol::Anthropic => validators::validate_anthropic_stream(events),
        Protocol::Gemini => validators::validate_gemini_stream(events),
    }
}

/// Context for rendering output to a client that sent `request`.
pub fn client_ctx(client: Protocol, request: &Value) -> ClientCtx {
    ClientCtx::new(client_model(client)).with_request(Arc::new(request.clone()))
}

/// Parses an SSE transcript, feeding the parser awkward chunk sizes.
pub fn parse_sse(transcript: &str) -> Vec<SseEvent> {
    let mut parser = SseParser::new();
    let mut events = Vec::new();
    for chunk in transcript.as_bytes().chunks(41) {
        events.extend(parser.push(chunk).expect("within the size limit"));
    }
    events.extend(parser.finish());
    events
}

/// Serialises wire events and parses them back, as they travel to a client.
pub fn over_the_wire(events: &[SseEvent]) -> Vec<SseEvent> {
    let mut wire = Vec::new();
    for event in events {
        wire.extend_from_slice(&event.to_bytes());
    }
    let mut parser = SseParser::new();
    let mut out = parser.push(&wire).expect("within the size limit");
    out.extend(parser.finish());
    out
}

/// Runs wire events through a protocol's stream decoder (including
/// `finish()`), checking the sequence contract of the result.
pub fn decode_stream(protocol: Protocol, events: &[SseEvent]) -> Vec<StreamEvent> {
    let mut decoder = codec(protocol).stream_decoder();
    let mut out = Vec::new();
    for event in events {
        out.extend(
            decoder
                .decode(event)
                .unwrap_or_else(|e| panic!("{protocol} stream decoder failed: {e}")),
        );
    }
    out.extend(decoder.finish());
    if let Err(violation) = validate_sequence(&out) {
        panic!("{protocol} stream decoder broke the sequence contract: {violation}\n{out:#?}");
    }
    out
}

/// Renders canonical events as a protocol's wire stream.
pub fn encode_stream(protocol: Protocol, ctx: &ClientCtx, events: &[StreamEvent]) -> Vec<SseEvent> {
    let mut encoder = codec(protocol).stream_encoder(ctx);
    let mut out = Vec::new();
    for event in events {
        out.extend(encoder.encode(event));
    }
    out.extend(encoder.finish());
    out
}

pub fn accumulate(events: &[StreamEvent]) -> Response {
    let mut accumulator = Accumulator::new();
    for event in events {
        accumulator.push(event);
    }
    accumulator.into_response()
}

/// Runs `f`, turning a panic into an error message so one failure does not
/// hide the others of a matrix cell.
pub fn no_panic<T>(what: &str, f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_else(|| "non-string panic payload".to_string());
        format!("{what}: PANIC: {message}")
    })
}

/// Collects failures of a matrix cell and reports them together.
#[derive(Default)]
pub struct Failures(Vec<String>);

impl Failures {
    pub fn push(&mut self, context: &str, message: impl AsRef<str>) {
        self.0.push(format!("[{context}] {}", message.as_ref()));
    }

    pub fn report(&mut self, context: &str, report: Report) {
        if let Err(violations) = report {
            for violation in violations {
                self.push(context, violation);
            }
        }
    }

    pub fn check(&mut self, context: &str, condition: bool, message: impl AsRef<str>) {
        if !condition {
            self.push(context, message);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Panics with the recorded failures grouped by kind: the messages are
    /// compared with their numbers and quoted names blanked out, and each
    /// kind is shown once, with how often it occurred and its first
    /// instance. For the randomised tests, where one defect fails hundreds
    /// of cases.
    pub fn finish_grouped(self, title: &str) {
        if self.0.is_empty() {
            return;
        }
        let mut groups: Vec<(String, usize, String)> = Vec::new();
        for failure in &self.0 {
            // Drop the context (it names the seed) and blank what varies.
            let message = failure
                .split_once("] ")
                .map_or(failure.as_str(), |(_, rest)| rest);
            let mut key = String::new();
            let mut quoted = false;
            for c in message.chars() {
                match c {
                    '`' => {
                        quoted = !quoted;
                        key.push(c);
                    }
                    _ if quoted => {}
                    c if c.is_ascii_digit() => key.push('#'),
                    c => key.push(c),
                }
            }
            match groups.iter_mut().find(|(existing, _, _)| *existing == key) {
                Some(group) => group.1 += 1,
                None => groups.push((key, 1, failure.clone())),
            }
        }
        let lines: Vec<String> = groups
            .iter()
            .map(|(_, count, first)| format!("{count:>5}x {first:.700}"))
            .collect();
        panic!(
            "{title}: {} failure(s) of {} kind(s)\n  {}",
            self.0.len(),
            groups.len(),
            lines.join("\n  ")
        );
    }

    /// Panics with every recorded failure.
    pub fn finish(self, title: &str) {
        if !self.0.is_empty() {
            panic!(
                "{title}: {} failure(s)\n  {}",
                self.0.len(),
                self.0.join("\n  ")
            );
        }
    }
}

/// Short name of a protocol for test output.
pub fn short(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenaiChat => "chat",
        Protocol::OpenaiResponses => "responses",
        Protocol::Anthropic => "anthropic",
        Protocol::Gemini => "gemini",
    }
}

/// How a tool name looks after any upstream's sanitising: every character
/// outside `[a-zA-Z0-9]` reads as `_`, leading underscores do not count and
/// case is ignored. Two names with the same loose form are "the same tool"
/// for the semantic checks.
pub fn loose_name(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    mapped.trim_start_matches('_').to_string()
}

/// Whether `wire` is what an upstream encoder may have made of the client's
/// tool name `declared`: the same loose spelling, possibly cut to the
/// upstream's length limit.
pub fn same_tool(declared: &str, wire: &str) -> bool {
    let declared = loose_name(declared);
    let wire_loose = loose_name(wire);
    declared == wire_loose || (wire.len() >= 63 && declared.starts_with(&wire_loose))
}

/// The names of the tool calls in the conversation history of an upstream
/// request, per protocol.
pub fn history_calls(protocol: Protocol, body: &Value) -> Vec<String> {
    let name = |v: &Value| v.get("name").and_then(Value::as_str).map(str::to_string);
    match protocol {
        Protocol::OpenaiChat => body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|message| message.get("tool_calls").and_then(Value::as_array))
            .flatten()
            .filter_map(|call| {
                call.get("function")
                    .or_else(|| call.get("custom"))
                    .and_then(name)
            })
            .collect(),
        Protocol::OpenaiResponses => body["input"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| {
                matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("function_call" | "custom_tool_call")
                )
            })
            .filter_map(name)
            .collect(),
        Protocol::Anthropic => body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|message| message.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
            .filter_map(name)
            .collect(),
        Protocol::Gemini => body["contents"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|content| content.get("parts").and_then(Value::as_array))
            .flatten()
            .filter_map(|part| part.get("functionCall").and_then(name))
            .collect(),
    }
}

/// The tool names an upstream request declares, per protocol.
pub fn declared_tools(protocol: Protocol, body: &Value) -> Vec<String> {
    let tools = body.get("tools").and_then(Value::as_array);
    let name = |v: &Value| v.get("name").and_then(Value::as_str).map(str::to_string);
    match protocol {
        Protocol::OpenaiChat => tools
            .into_iter()
            .flatten()
            .filter_map(|tool| {
                tool.get("function")
                    .or_else(|| tool.get("custom"))
                    .and_then(name)
            })
            .collect(),
        Protocol::OpenaiResponses => tools
            .into_iter()
            .flatten()
            .filter(|tool| {
                matches!(
                    tool.get("type").and_then(Value::as_str),
                    Some("function" | "custom")
                )
            })
            .filter_map(name)
            .collect(),
        Protocol::Anthropic => tools
            .into_iter()
            .flatten()
            .filter(|tool| tool.get("input_schema").is_some())
            .filter_map(name)
            .collect(),
        Protocol::Gemini => tools
            .into_iter()
            .flatten()
            .filter_map(|tool| tool.get("functionDeclarations").and_then(Value::as_array))
            .flatten()
            .filter_map(name)
            .collect(),
    }
}
