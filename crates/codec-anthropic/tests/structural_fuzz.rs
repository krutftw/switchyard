//! Structural checks over many generated inputs (fixed seed, so the runs
//! are identical every time).
//!
//! * `encode_request` must produce a body the Messages API accepts
//!   structurally, whatever canonical request it is given;
//! * `prepare_passthrough` must leave a Claude-bound body with a replayable
//!   history, and do so idempotently;
//! * the stream encoder must produce a well-formed event stream for any
//!   contract-valid sequence, one that the stream decoder turns back into a
//!   contract-valid sequence.

mod common;

use common::{decode_events, encode_stream, wire};
use serde_json::{Value, json};
use std::collections::HashSet;
use switchyard_codec_anthropic::AnthropicCodec;
use switchyard_core::ir::{
    FinishReason, FunctionTool, MediaPart, Message, OpaquePart, Part, Reasoning, Request, Role,
    Signature, Tool, ToolCallKind, ToolChoice, ToolResult,
};
use switchyard_core::reasoning::{Depth, Effort, ModelThinking, ReasoningConfig, ThinkingSupport};
use switchyard_core::stream::{BlockStart, StreamEvent};
use switchyard_core::{Codec, Protocol, UpstreamCtx};

/// xorshift64*: small, deterministic, good enough to explore shapes.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const TEXTS: &[&str] = &[
    "hello",
    "",
    " ",
    "\n\n",
    "answer",
    "trailing  ",
    "\t",
    "ok",
    "42",
];
const CALL_IDS: &[&str] = &[
    "call_0",
    "call_1",
    "functions.f:0",
    "functions_f_0",
    "toolu_01A",
    "",
    "a b",
    "a.b",
];
const TOOL_NAMES: &[&str] = &["f", "get.weather", "get_weather", "g", "f"];

fn is_ident(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn blocks(message: &Value) -> &[Value] {
    message["content"]
        .as_array()
        .map_or(&[], |blocks| blocks.as_slice())
}

fn of_type<'a>(message: &'a Value, kind: &str) -> Vec<&'a Value> {
    blocks(message)
        .iter()
        .filter(|block| block["type"] == kind)
        .collect()
}

// ---------------------------------------------------------------------------
// encode_request
// ---------------------------------------------------------------------------

fn random_part(rng: &mut Rng, role: Role) -> Part {
    let text = (*rng.pick(TEXTS)).to_string();
    match (role, rng.below(10)) {
        (Role::Assistant, 0..=2) => Part::tool_call(
            *rng.pick(CALL_IDS),
            *rng.pick(TOOL_NAMES),
            *rng.pick(&["{}", "", "{\"a\":1}", "null", "[1]", "{\"a\":"]),
        ),
        (Role::Assistant, 3) => Part::Reasoning(Reasoning {
            id: None,
            text,
            signature: match rng.below(4) {
                0 => Some(Signature::new(Protocol::Anthropic, "EqQBCkYIBBgCKkD")),
                1 => Some(Signature::new(Protocol::Gemini, "CpsBAVSoXO4")),
                2 => Some(Signature::new(Protocol::Anthropic, "")),
                _ => None,
            },
            redacted: rng.chance(20),
        }),
        (Role::Assistant, 4) => Part::Opaque(OpaquePart {
            origin: *rng.pick(&[Protocol::Anthropic, Protocol::Gemini]),
            raw: json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                        "input": {}}),
        }),
        (Role::User, 0..=2) => Part::ToolResult(ToolResult {
            call_id: (*rng.pick(CALL_IDS)).to_string(),
            name: None,
            content: if rng.chance(70) {
                vec![Part::text(text)]
            } else {
                Vec::new()
            },
            is_error: rng.chance(20),
            cache_control: None,
        }),
        (Role::User, 3) => Part::Image(MediaPart::base64("image/png", *rng.pick(&["iVBOR", ""]))),
        (Role::User, 4) => Part::Audio(MediaPart::base64("audio/wav", "UklGR")),
        _ => Part::text(text),
    }
}

fn random_request(rng: &mut Rng) -> Request {
    let mut request = Request::new(
        "claude-sonnet-4-5",
        *rng.pick(&[
            Protocol::OpenaiChat,
            Protocol::Anthropic,
            Protocol::Gemini,
            Protocol::OpenaiResponses,
        ]),
    );
    for _ in 0..rng.below(9) {
        let role = *rng.pick(&[
            Role::User,
            Role::User,
            Role::Assistant,
            Role::Assistant,
            Role::System,
        ]);
        let parts = (0..rng.below(4)).map(|_| random_part(rng, role)).collect();
        request.messages.push(Message::new(role, parts));
    }
    for _ in 0..rng.below(3) {
        request.system.push(Part::text(*rng.pick(TEXTS)));
    }
    for _ in 0..rng.below(4) {
        request.tools.push(Tool::Function(FunctionTool {
            name: (*rng.pick(TOOL_NAMES)).to_string(),
            description: None,
            parameters: Value::Null,
            strict: None,
            cache_control: None,
        }));
    }
    request.tool_choice = match rng.below(6) {
        0 => Some(ToolChoice::Auto),
        1 => Some(ToolChoice::Required),
        2 => Some(ToolChoice::Tool { name: "f".into() }),
        3 => Some(ToolChoice::None),
        _ => None,
    };
    request.max_output_tokens = *rng.pick(&[
        None,
        Some(1),
        Some(256),
        Some(1024),
        Some(1025),
        Some(20_000),
    ]);
    request.temperature = *rng.pick(&[None, Some(0.5), Some(1.0)]);
    request.top_p = *rng.pick(&[None, Some(0.9)]);
    request.top_k = *rng.pick(&[None, Some(40)]);
    request.reasoning = match rng.below(7) {
        0 => Some(ReasoningConfig::with_depth(Depth::Off)),
        1 => Some(ReasoningConfig::with_depth(Depth::Auto)),
        2 => Some(ReasoningConfig::with_depth(Depth::Level(Effort::High))),
        3 => Some(ReasoningConfig::with_depth(Depth::Budget(8192))),
        4 => Some(ReasoningConfig::with_depth(Depth::Budget(100))),
        5 => Some(ReasoningConfig::with_depth(Depth::Budget(200_000))),
        _ => None,
    };
    request
}

/// The message that opens the assistant turn in progress, if one is.
fn open_turn_start(messages: &[Value]) -> Option<&Value> {
    let mut start = None;
    for (at, message) in messages.iter().enumerate().rev() {
        if message["role"] == "assistant" {
            start = Some(message);
        } else if of_type(message, "tool_result").is_empty() {
            if at == messages.len() - 1 {
                return None;
            }
            break;
        }
    }
    start
}

fn check_request_body(body: &Value) -> Result<(), String> {
    let messages = body["messages"].as_array().ok_or("no messages")?;
    if messages.is_empty() {
        return Err("no messages".into());
    }
    if messages[0]["role"] != "user" {
        return Err("does not open with a user turn".into());
    }
    let mut seen_ids: HashSet<String> = HashSet::new();
    for (at, message) in messages.iter().enumerate() {
        let role = message["role"].as_str().ok_or("role")?;
        if at > 0 && messages[at - 1]["role"] == role {
            return Err(format!("two {role} turns in a row at {at}"));
        }
        if blocks(message).is_empty() {
            return Err(format!("turn {at} has no content"));
        }
        for block in blocks(message) {
            let text_of = |key: &str| block[key].as_str().unwrap_or("");
            match text_of("type") {
                "text" if text_of("text").trim().is_empty() => {
                    return Err(format!("blank text block in turn {at}"));
                }
                "thinking" if text_of("signature").is_empty() => {
                    return Err(format!("unsigned thinking in turn {at}"));
                }
                "redacted_thinking" if text_of("data").is_empty() => {
                    return Err(format!("empty redacted_thinking in turn {at}"));
                }
                "image" if block["source"]["data"] == "" => {
                    return Err("image without data".into());
                }
                "tool_use" => {
                    let id = block["id"].as_str().unwrap_or("");
                    if !is_ident(id) {
                        return Err(format!("tool_use id {id:?} is not valid"));
                    }
                    if !seen_ids.insert(id.to_string()) {
                        return Err(format!("tool_use id {id:?} is used twice"));
                    }
                    if !is_ident(block["name"].as_str().unwrap_or("")) {
                        return Err("tool_use name is not valid".into());
                    }
                    if !block["input"].is_object() {
                        return Err("tool_use input is not an object".into());
                    }
                }
                _ => {}
            }
        }
        if role == "assistant" {
            let calls: Vec<&str> = of_type(message, "tool_use")
                .iter()
                .filter_map(|block| block["id"].as_str())
                .collect();
            if !calls.is_empty() {
                let next = messages
                    .get(at + 1)
                    .ok_or(format!("calls of turn {at} are never answered"))?;
                let answers: Vec<&str> = of_type(next, "tool_result")
                    .iter()
                    .filter_map(|block| block["tool_use_id"].as_str())
                    .collect();
                let mut sorted_calls = calls.clone();
                let mut sorted_answers = answers.clone();
                sorted_calls.sort_unstable();
                sorted_answers.sort_unstable();
                if sorted_calls != sorted_answers {
                    return Err(format!(
                        "turn {at} calls {calls:?} but turn {} answers {answers:?}",
                        at + 1
                    ));
                }
            }
        } else {
            let results = of_type(message, "tool_result").len();
            if blocks(message)[..results]
                .iter()
                .any(|block| block["type"] != "tool_result")
            {
                return Err(format!("tool results are not first in turn {at}"));
            }
            let previous_calls = at
                .checked_sub(1)
                .map_or(0, |prev| of_type(&messages[prev], "tool_use").len());
            if results != previous_calls {
                return Err(format!(
                    "turn {at} has {results} results for {previous_calls} calls"
                ));
            }
        }
    }
    if let Some(last) = messages.last().filter(|m| m["role"] == "assistant") {
        let last_block = blocks(last).last().ok_or("empty prefill")?;
        if last_block["type"] == "thinking" || last_block["type"] == "redacted_thinking" {
            return Err("prefill ends with thinking".into());
        }
        if let Some(text) = last_block["text"].as_str()
            && text.trim_end() != text
        {
            return Err("prefill ends with whitespace".into());
        }
    }

    if let Some(system) = body.get("system") {
        let system = system.as_array().ok_or("system is not a block list")?;
        if system.is_empty()
            || system
                .iter()
                .any(|block| block["text"].as_str().unwrap_or("").trim().is_empty())
        {
            return Err("blank system block".into());
        }
    }

    let mut names: HashSet<&str> = HashSet::new();
    for tool in body["tools"].as_array().map_or(&[][..], |t| t.as_slice()) {
        let name = tool["name"].as_str().unwrap_or("");
        if !is_ident(name) || !names.insert(name) {
            return Err(format!("tool name {name:?} is invalid or repeated"));
        }
    }
    if body.get("tool_choice").is_some() && body.get("tools").is_none() {
        return Err("tool_choice without tools".into());
    }

    let max_tokens = body["max_tokens"].as_u64().ok_or("max_tokens is missing")?;
    let thinking = body.get("thinking");
    let kind = thinking.and_then(|t| t["type"].as_str()).unwrap_or("");
    let active = matches!(kind, "enabled" | "adaptive");
    if kind == "enabled" {
        let budget = thinking
            .and_then(|t| t["budget_tokens"].as_u64())
            .ok_or("enabled without a budget")?;
        if budget < 1024 || budget >= max_tokens {
            return Err(format!("budget {budget} with max_tokens {max_tokens}"));
        }
        if let Some(start) = open_turn_start(messages) {
            let first = blocks(start)[0]["type"].as_str().unwrap_or("");
            if first != "thinking" && first != "redacted_thinking" {
                return Err(format!(
                    "manual thinking, but the turn in progress opens with {first}"
                ));
            }
        }
    }
    if kind == "disabled" && thinking != Some(&json!({"type": "disabled"})) {
        return Err("disabled thinking carries other fields".into());
    }
    if active && matches!(body["tool_choice"]["type"].as_str(), Some("any" | "tool")) {
        return Err("thinking with forced tool use".into());
    }
    let sampling = |key: &str| body.get(key).is_some();
    if active && (sampling("temperature") || sampling("top_p") || sampling("top_k")) {
        return Err("sampling parameters next to thinking".into());
    }
    if sampling("temperature") && sampling("top_p") {
        return Err("temperature and top_p together".into());
    }
    Ok(())
}

#[test]
fn encode_request_output_is_structurally_valid_for_any_request() {
    let budget_only = ThinkingSupport {
        min: 1024,
        max: 128_000,
        zero_allowed: true,
        dynamic_allowed: false,
        levels: Vec::new(),
    };
    let levels = ThinkingSupport::levels(&[Effort::Low, Effort::Medium, Effort::High]);
    let contexts = [
        UpstreamCtx::default(),
        UpstreamCtx {
            thinking: ModelThinking::Supported(&budget_only),
            max_output_tokens: Some(64_000),
            ..UpstreamCtx::default()
        },
        UpstreamCtx {
            thinking: ModelThinking::Supported(&budget_only),
            ..UpstreamCtx::default()
        },
        UpstreamCtx {
            thinking: ModelThinking::Supported(&levels),
            max_output_tokens: Some(32_000),
            ..UpstreamCtx::default()
        },
        UpstreamCtx {
            thinking: ModelThinking::Unsupported,
            max_output_tokens: Some(8192),
            ..UpstreamCtx::default()
        },
    ];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    // How often the generator reaches the shapes the checks are about.
    let (mut manual, mut manual_in_tool_loop, mut tool_turns, mut renamed_ids) = (0, 0, 0, 0);
    for case in 0..6000 {
        let request = random_request(&mut rng);
        let ctx = &contexts[case % contexts.len()];
        let body = AnthropicCodec
            .encode_request(&request, ctx)
            .expect("encode_request never fails");
        if let Err(violation) = check_request_body(&body) {
            panic!("case {case}: {violation}\nrequest: {request:#?}\nbody: {body:#}");
        }
        let messages = body["messages"].as_array().unwrap();
        let calls: Vec<&Value> = messages
            .iter()
            .flat_map(|message| of_type(message, "tool_use"))
            .collect();
        tool_turns += usize::from(!calls.is_empty());
        renamed_ids += usize::from(
            calls
                .iter()
                .any(|call| !CALL_IDS.contains(&call["id"].as_str().unwrap_or(""))),
        );
        if body["thinking"]["type"] == "enabled" {
            manual += 1;
            manual_in_tool_loop += usize::from(open_turn_start(messages).is_some());
        }
        // Deterministic: the same request encodes to the same body.
        assert_eq!(
            AnthropicCodec.encode_request(&request, ctx).unwrap(),
            body,
            "case {case}"
        );
        // And the body survives this codec's own decoder.
        AnthropicCodec
            .decode_request(&body, &Default::default())
            .expect("own output decodes");
    }
    assert!(
        manual > 300 && manual_in_tool_loop > 5 && tool_turns > 1500 && renamed_ids > 500,
        "the generator no longer reaches the interesting shapes: {manual} bodies with manual \
         thinking, {manual_in_tool_loop} of them inside a turn in progress, {tool_turns} with \
         tool calls, {renamed_ids} with rewritten call ids"
    );
}

// ---------------------------------------------------------------------------
// prepare_passthrough
// ---------------------------------------------------------------------------

fn random_wire_block(rng: &mut Rng, role: &str) -> Value {
    let text = *rng.pick(TEXTS);
    match (role, rng.below(8)) {
        ("assistant", 0..=2) => json!({"type": "tool_use", "id": *rng.pick(CALL_IDS),
                                       "name": "f", "input": {}}),
        ("assistant", 3) => json!({"type": "thinking", "thinking": text,
                                   "signature": *rng.pick(&["", "EqQBCkYIBBgCKkD"])}),
        ("assistant", 4) => json!({"type": "redacted_thinking",
                                   "data": *rng.pick(&["", "EmwKAhgBEgy3va3pzix"])}),
        ("user", 0..=2) => json!({"type": "tool_result", "tool_use_id": *rng.pick(CALL_IDS),
                                  "content": text}),
        _ => json!({"type": "text", "text": text}),
    }
}

#[test]
fn prepare_passthrough_leaves_a_replayable_history_for_claude() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for case in 0..4000 {
        let messages: Vec<Value> = (0..rng.below(8))
            .map(|_| {
                let role = *rng.pick(&["user", "assistant"]);
                if rng.chance(15) {
                    return json!({"role": role, "content": *rng.pick(TEXTS)});
                }
                let content: Vec<Value> = (0..rng.below(4))
                    .map(|_| random_wire_block(&mut rng, role))
                    .collect();
                json!({"role": role, "content": content})
            })
            .collect();
        let original = json!({
            "model": "claude-sonnet-4-5", "max_tokens": 16000,
            "thinking": *rng.pick(&[json!({"type": "enabled", "budget_tokens": 4096}),
                                    json!({"type": "adaptive"})]),
            "messages": messages
        });
        let mut body = original.clone();
        AnthropicCodec.prepare_passthrough(&mut body, false, &UpstreamCtx::default());

        let mut again = body.clone();
        AnthropicCodec.prepare_passthrough(&mut again, false, &UpstreamCtx::default());
        assert_eq!(again, body, "case {case}: not idempotent\n{original:#}");

        let messages = body["messages"].as_array().unwrap();
        let mut seen: HashSet<&str> = HashSet::new();
        for message in messages.iter().filter(|m| m["role"] == "assistant") {
            for block in blocks(message) {
                let fine = match block["type"].as_str().unwrap_or("") {
                    "thinking" => !block["signature"].as_str().unwrap_or("").is_empty(),
                    "redacted_thinking" => !block["data"].as_str().unwrap_or("").is_empty(),
                    "text" => !block["text"].as_str().unwrap_or("").trim().is_empty(),
                    "tool_use" => {
                        let id = block["id"].as_str().unwrap_or("");
                        is_ident(id) && seen.insert(id)
                    }
                    _ => true,
                };
                assert!(
                    fine,
                    "case {case}: {block} survived\n{original:#}\n{body:#}"
                );
            }
        }
        for message in messages.iter().filter(|m| m["role"] == "user") {
            for result in of_type(message, "tool_result") {
                let id = result["tool_use_id"].as_str().unwrap_or("");
                // An id-less result answers nothing; that is for the API to say.
                assert!(
                    id.is_empty() || is_ident(id),
                    "case {case}: tool_use_id {id:?}\n{body:#}"
                );
            }
        }
        if body["thinking"]["type"] == "enabled"
            && let Some(start) = open_turn_start(messages)
        {
            let first = blocks(start)
                .first()
                .and_then(|block| block["type"].as_str())
                .unwrap_or("");
            assert!(
                first == "thinking" || first == "redacted_thinking",
                "case {case}: manual thinking kept for a turn that opens with {first:?}\n{body:#}"
            );
        }
        // What the client wrote in its own turns is not rewritten.
        let user_text = |body: &Value| -> Vec<Value> {
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "user")
                .flat_map(|m| match &m["content"] {
                    Value::Array(blocks) => blocks
                        .iter()
                        .filter(|b| b["type"] == "text")
                        .cloned()
                        .collect(),
                    other => vec![other.clone()],
                })
                .collect()
        };
        assert_eq!(user_text(&body), user_text(&original), "case {case}");
    }
}

// ---------------------------------------------------------------------------
// Stream encoder
// ---------------------------------------------------------------------------

fn random_sequence(rng: &mut Rng) -> Vec<StreamEvent> {
    let mut events = vec![StreamEvent::Start {
        id: (*rng.pick(&["msg_01ABC", "", "chatcmpl-9"])).to_string(),
        model: "upstream".into(),
        created: 0,
    }];
    let blocks = rng.below(5);
    for index in 0..blocks as u32 {
        let texts = rng.below(3);
        match rng.below(6) {
            0 | 1 => {
                events.push(StreamEvent::BlockStart {
                    index,
                    block: BlockStart::Text,
                });
                for _ in 0..texts {
                    events.push(StreamEvent::TextDelta {
                        index,
                        text: (*rng.pick(TEXTS)).to_string(),
                    });
                }
            }
            2 => {
                let redacted = rng.chance(25);
                events.push(StreamEvent::BlockStart {
                    index,
                    block: BlockStart::Reasoning { id: None, redacted },
                });
                if !redacted {
                    for _ in 0..texts {
                        events.push(StreamEvent::ReasoningDelta {
                            index,
                            text: (*rng.pick(TEXTS)).to_string(),
                        });
                    }
                }
                if rng.chance(50) {
                    events.push(StreamEvent::ReasoningSignature {
                        index,
                        signature: Signature::new(
                            *rng.pick(&[Protocol::Anthropic, Protocol::Gemini]),
                            *rng.pick(&["EqQBCkYIBBgCKkD", ""]),
                        ),
                    });
                }
            }
            3 => {
                events.push(StreamEvent::BlockStart {
                    index,
                    block: BlockStart::ToolCall {
                        id: (*rng.pick(CALL_IDS)).to_string(),
                        name: "f".into(),
                        kind: *rng.pick(&[ToolCallKind::Function, ToolCallKind::Custom]),
                        signature: None,
                    },
                });
                for fragment in *rng.pick(&[
                    &[][..],
                    &["{\"a\":", "1}"][..],
                    &["", "{}"][..],
                    &["[1]"][..],
                    &["{\"a\":"][..],
                    &["null"][..],
                ]) {
                    events.push(StreamEvent::ToolArgsDelta {
                        index,
                        fragment: (*fragment).to_string(),
                    });
                }
            }
            4 => {
                events.push(StreamEvent::BlockStart {
                    index,
                    block: BlockStart::Refusal,
                });
                for _ in 0..texts {
                    events.push(StreamEvent::TextDelta {
                        index,
                        text: (*rng.pick(TEXTS)).to_string(),
                    });
                }
            }
            _ => events.push(StreamEvent::BlockStart {
                index,
                block: BlockStart::Whole {
                    part: match rng.below(3) {
                        0 => Part::Image(MediaPart::base64("image/png", "iVBOR")),
                        1 => Part::Opaque(OpaquePart {
                            origin: Protocol::Anthropic,
                            raw: json!({"type": "server_tool_use", "id": "srvtoolu_1",
                                        "name": "web_search", "input": {}}),
                        }),
                        _ => Part::text(*rng.pick(TEXTS)),
                    },
                },
            }),
        }
        // A sequence may die with a block still open.
        if index as usize == blocks - 1 && rng.chance(10) {
            return events;
        }
        events.push(StreamEvent::BlockStop { index });
    }
    match rng.below(8) {
        0 => {}
        1 => events.push(StreamEvent::Error(switchyard_core::ApiError::upstream(
            "boom",
        ))),
        2 => events.push(StreamEvent::Finish {
            reason: FinishReason::Error,
            stop_sequence: None,
        }),
        _ => events.push(StreamEvent::Finish {
            reason: rng
                .pick(&[
                    FinishReason::Stop,
                    FinishReason::ToolCalls,
                    FinishReason::Length,
                ])
                .clone(),
            stop_sequence: None,
        }),
    }
    events
}

#[test]
fn stream_encoder_output_is_a_well_formed_stream_for_any_sequence() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for case in 0..6000 {
        let events = random_sequence(&mut rng);
        let out = encode_stream(&events, "sonnet");
        let named = wire(&out);
        let fail = |what: &str| -> ! {
            panic!("case {case}: {what}\nevents: {events:#?}\nwire: {named:#?}")
        };

        if named[0].0 != "message_start" {
            fail("does not begin with message_start");
        }
        let ended_in_error = named.last().is_some_and(|(name, _)| name == "error");
        if !ended_in_error {
            let tail: Vec<&str> = named[named.len() - 2..]
                .iter()
                .map(|(name, _)| name.as_str())
                .collect();
            if tail != ["message_delta", "message_stop"] {
                fail("does not end with message_delta + message_stop");
            }
        }
        let mut open: Option<(u64, String, bool)> = None;
        let mut next_index = 0;
        for (name, data) in &named[1..] {
            match name.as_str() {
                "content_block_start" => {
                    if open.is_some() {
                        fail("block starts while another is open");
                    }
                    if data["index"] != json!(next_index) {
                        fail("block indices are not contiguous");
                    }
                    let kind = data["content_block"]["type"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    open = Some((next_index, kind, false));
                    next_index += 1;
                }
                "content_block_delta" => {
                    let Some((index, kind, content)) = &mut open else {
                        fail("delta outside a block");
                    };
                    if data["index"] != json!(*index) {
                        fail("delta for another block");
                    }
                    let delta = data["delta"]["type"].as_str().unwrap_or("");
                    let fits = match kind.as_str() {
                        "text" => matches!(delta, "text_delta" | "citations_delta"),
                        "thinking" => matches!(delta, "thinking_delta" | "signature_delta"),
                        "tool_use" => delta == "input_json_delta",
                        _ => false,
                    };
                    if !fits {
                        fail("delta kind does not match its block");
                    }
                    *content = true;
                }
                "content_block_stop" => {
                    let Some((index, kind, content)) = open.take() else {
                        fail("stop without a block");
                    };
                    if data["index"] != json!(index) {
                        fail("stop for another block");
                    }
                    // A text or thinking block is only announced for content.
                    if matches!(kind.as_str(), "text" | "thinking") && !content {
                        fail("an empty text or thinking block was announced");
                    }
                    // A tool call always gets at least one argument delta.
                    if kind == "tool_use" && !content {
                        fail("tool_use block without input_json_delta");
                    }
                }
                "message_delta" | "message_stop" | "error" => {
                    if open.is_some() {
                        fail("message ends while a block is open");
                    }
                }
                _ => fail("unexpected event"),
            }
        }

        // The stream this codec writes is one its decoder understands
        // (`decode_events` checks the sequence contract).
        let decoded = decode_events(&out);
        // For a sequence that finished, concatenated tool arguments are
        // never an invalid mix: either they are a JSON object or the call
        // was cut short and the stop reason says so.
        let finished = matches!(
            events.last(),
            Some(StreamEvent::Finish { reason, .. }) if *reason != FinishReason::Error
        );
        let response = common::accumulate(&decoded);
        for call in response.tool_calls().filter(|_| finished) {
            let parses =
                serde_json::from_str::<Value>(&call.arguments).is_ok_and(|value| value.is_object());
            if !parses && !call.arguments.is_empty() && response.finish == FinishReason::ToolCalls {
                fail("a half-written tool call is presented as complete");
            }
        }
    }
}
