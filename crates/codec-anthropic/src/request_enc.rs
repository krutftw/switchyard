//! Upstream side: canonical [`Request`] → Messages request body.

use crate::blocks::{encode_document, encode_image, encode_text, tool_call_input};
use crate::reasoning::{
    drop_manual_thinking_without_turn_start, drop_thinking_for_forced_tool_choice, fix_sampling,
    write_reasoning, write_summary,
};
use crate::util::{
    THIS, TOOL_EXTRAS_KEY, ToolIds, Unmatched, is_block, sanitize_tool_name, str_field,
};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use switchyard_core::ir::{
    self, BuiltinKind, MediaSource, Message, Part, Request, ResponseFormat, Role, Tool, ToolChoice,
    ToolResult, empty_object_schema,
};
use switchyard_core::reasoning::{Depth, Fitted, ModelThinking};
use switchyard_core::{CodecError, UpstreamCtx};

/// `max_tokens` when neither the request nor the model catalog gives one.
pub(crate) const DEFAULT_MAX_TOKENS: u64 = 4096;
/// The same, when the request asks the model to think: thinking tokens count
/// against `max_tokens`, so the small default would leave no room to answer.
pub(crate) const DEFAULT_MAX_TOKENS_THINKING: u64 = 32_000;

/// `content` of the `tool_result` synthesised for a tool call the
/// conversation never answered.
pub(crate) const INTERRUPTED_TOOL_RESULT: &str =
    "Tool call was interrupted before any output was recorded.";

/// Text standing in for a tool result that has no call to answer and no
/// content of its own.
pub(crate) const EMPTY_TOOL_RESULT: &str = "Tool result was empty.";

/// Longest `metadata.user_id` the API accepts.
const MAX_USER_ID_CHARS: usize = 256;

/// Text of the user turn inserted when the conversation would otherwise be
/// empty or start with an assistant turn (the API requires a user turn
/// first).
pub(crate) const PLACEHOLDER_USER_TURN: &str = "(continued)";

/// Appended to `system` for `ResponseFormat::JsonObject`, which the Messages
/// API cannot express natively (it only has schema-constrained output).
pub(crate) const JSON_OBJECT_INSTRUCTION: &str = "Respond with a single valid JSON object and \
     nothing else: no explanations and no markdown code fences.";

/// Top-level request fields without an IR slot that are copied back when the
/// request was decoded from an Anthropic client.
const FORWARDED_EXTRAS: &[&str] = &[
    "speed",
    "container",
    "context_management",
    "mcp_servers",
    "inference_geo",
    "cache_control",
    "fallbacks",
];

fn same_family(origin: switchyard_core::Protocol) -> bool {
    origin.family() == THIS.family()
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Encodes a tool result. `tool_use_id` is still the canonical call id here;
/// [`assign_tool_ids`] maps it once the turns are final.
fn encode_tool_result(result: &ToolResult, native: bool) -> Value {
    // cache_control is not allowed inside tool_result content; a marker on a
    // nested part moves up to the tool_result when it has none of its own.
    let mut cache = result.cache_control.clone();
    let mut blocks: Vec<Value> = Vec::new();
    for part in &result.content {
        if cache.is_none() {
            cache = part.cache_control().cloned();
        }
        let encoded = match part {
            Part::Text(text) => encode_text(&text.text, None),
            Part::Image(media) => encode_image(media, native, false),
            Part::Document(media) => encode_document(media, native, false),
            Part::Opaque(opaque) if same_family(opaque.origin) && opaque.raw.is_object() => {
                Some(opaque.raw.clone())
            }
            _ => None,
        };
        blocks.extend(encoded);
    }

    let mut block = Map::new();
    block.insert("type".to_string(), json!("tool_result"));
    block.insert(
        "tool_use_id".to_string(),
        Value::String(result.call_id.clone()),
    );
    let single_text = match blocks.as_slice() {
        [only] if str_field(only, "type") == Some("text") => {
            str_field(only, "text").map(str::to_string)
        }
        _ => None,
    };
    if let Some(text) = single_text {
        block.insert("content".to_string(), Value::String(text));
    } else if !blocks.is_empty() {
        block.insert("content".to_string(), Value::Array(blocks));
    } else if result.is_error {
        // The API refuses an error result without content.
        block.insert("content".to_string(), json!("Tool call failed."));
    }
    if result.is_error {
        block.insert("is_error".to_string(), json!(true));
    }
    if let Some(cache) = cache {
        block.insert("cache_control".to_string(), cache);
    }
    Value::Object(block)
}

fn encode_user_part(part: &Part, native: bool) -> Option<Value> {
    match part {
        Part::Text(text) => encode_text(&text.text, text.cache_control.as_ref()),
        Part::Image(media) => encode_image(media, native, true),
        Part::Document(media) => encode_document(media, native, true),
        Part::ToolResult(result) => Some(encode_tool_result(result, native)),
        Part::Opaque(opaque) if same_family(opaque.origin) && opaque.raw.is_object() => {
            Some(opaque.raw.clone())
        }
        // Audio has no Messages representation; tool calls, reasoning and
        // refusals do not belong in a user turn.
        _ => None,
    }
}

fn encode_assistant_part(part: &Part) -> Option<Value> {
    match part {
        Part::Text(text) => encode_text(&text.text, text.cache_control.as_ref()),
        Part::Refusal(refusal) => encode_text(&refusal.text, None),
        Part::Reasoning(reasoning) => {
            // Thinking is only replayable with the signature Anthropic issued
            // for it; unsigned or foreign reasoning would be rejected.
            let signature = reasoning
                .signature
                .as_ref()
                .filter(|s| s.valid_for(THIS) && !s.data.trim().is_empty())?;
            if reasoning.redacted {
                Some(json!({"type": "redacted_thinking", "data": signature.data}))
            } else {
                Some(json!({
                    "type": "thinking",
                    "thinking": reasoning.text,
                    "signature": signature.data,
                }))
            }
        }
        Part::ToolCall(call) => {
            let mut block = Map::new();
            block.insert("type".to_string(), json!("tool_use"));
            // The canonical id for now; see `assign_tool_ids`.
            block.insert("id".to_string(), Value::String(call.id.clone()));
            block.insert(
                "name".to_string(),
                Value::String(sanitize_tool_name(&call.name)),
            );
            block.insert("input".to_string(), tool_call_input(call));
            if let Some(cache) = &call.cache_control {
                block.insert("cache_control".to_string(), cache.clone());
            }
            Some(Value::Object(block))
        }
        Part::Opaque(opaque) if same_family(opaque.origin) && opaque.raw.is_object() => {
            Some(opaque.raw.clone())
        }
        // Media and tool results cannot appear in an assistant turn.
        _ => None,
    }
}

/// A mid-conversation system message as the user turn that replaces it.
fn system_as_user(message: &Message) -> Option<Message> {
    let text = message
        .parts
        .iter()
        .filter_map(Part::as_text)
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return None;
    }
    Some(Message::user_text(format!("<system>\n{text}\n</system>")))
}

/// Replaces the canonical call ids on `tool_use` / `tool_result` blocks with
/// ids the API accepts (see [`ToolIds`]). A result that answers no call of
/// the assistant turn before it is left with an empty id, which
/// [`repair_tool_pairing`] takes as "answers nothing".
fn assign_tool_ids(turns: &mut [(Role, Vec<Value>)]) {
    let mut ids = ToolIds::default();
    for (role, blocks) in turns {
        if *role == Role::Assistant {
            ids.assistant_blocks(blocks);
        } else {
            ids.user_blocks(blocks, Unmatched::Mark);
        }
    }
}

fn tool_use_ids(blocks: &[Value]) -> Vec<String> {
    blocks
        .iter()
        .filter(|block| is_block(block, "tool_use"))
        .filter_map(|block| str_field(block, "id"))
        .map(str::to_string)
        .collect()
}

/// The content of a `tool_result` that answers no call, as ordinary user
/// blocks.
fn orphan_result_as_content(result: &Value) -> Vec<Value> {
    let mut blocks: Vec<Value> = match result.get("content") {
        Some(Value::String(text)) if !text.trim().is_empty() => {
            vec![json!({"type": "text", "text": text})]
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| {
                matches!(
                    str_field(item, "type"),
                    Some("text" | "image" | "document" | "search_result")
                )
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": EMPTY_TOOL_RESULT}));
    }
    blocks
}

/// Makes every `tool_use` / `tool_result` pair line up the way the API
/// insists on: the results of the calls of an assistant turn are in the very
/// next user turn, and a result answers a call of the turn right before it.
///
/// Histories translated from other protocols (or trimmed by a client) break
/// this in two ways, each of which is a guaranteed 400:
///
/// * a result whose call is not in the preceding assistant turn — it is
///   turned into plain user content, so what the tool said is kept;
/// * a call that was never answered — it gets an error result saying so.
fn repair_tool_pairing(turns: &mut Vec<(Role, Vec<Value>)>) {
    for at in 0..turns.len() {
        if turns[at].0 != Role::User {
            continue;
        }
        let expected: HashSet<String> = match at.checked_sub(1).map(|prev| &turns[prev]) {
            Some((Role::Assistant, blocks)) => tool_use_ids(blocks).into_iter().collect(),
            _ => HashSet::new(),
        };
        let mut answered: HashSet<String> = HashSet::new();
        let mut results: Vec<Value> = Vec::new();
        let mut rest: Vec<Value> = Vec::new();
        for block in std::mem::take(&mut turns[at].1) {
            if !is_block(&block, "tool_result") {
                rest.push(block);
                continue;
            }
            let id = str_field(&block, "tool_use_id").unwrap_or("").to_string();
            if expected.contains(&id) && answered.insert(id) {
                results.push(block);
            } else {
                rest.extend(orphan_result_as_content(&block));
            }
        }
        results.extend(rest);
        turns[at].1 = results;
    }

    let mut at = 0;
    while at < turns.len() {
        if turns[at].0 == Role::Assistant {
            let calls = tool_use_ids(&turns[at].1);
            let next_is_user = matches!(turns.get(at + 1), Some((Role::User, _)));
            let answered: HashSet<&str> = match turns.get(at + 1) {
                Some((Role::User, blocks)) => blocks
                    .iter()
                    .filter(|block| is_block(block, "tool_result"))
                    .filter_map(|block| str_field(block, "tool_use_id"))
                    .collect(),
                _ => HashSet::new(),
            };
            let missing: Vec<Value> = calls
                .iter()
                .filter(|id| !answered.contains(id.as_str()))
                .map(|id| {
                    json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": INTERRUPTED_TOOL_RESULT,
                        "is_error": true,
                    })
                })
                .collect();
            if !missing.is_empty() {
                if next_is_user {
                    let blocks = &mut turns[at + 1].1;
                    let after_results = blocks
                        .iter()
                        .take_while(|block| is_block(block, "tool_result"))
                        .count();
                    blocks.splice(after_results..after_results, missing);
                } else {
                    turns.insert(at + 1, (Role::User, missing));
                }
            }
        }
        at += 1;
    }
}

/// How many messages at the head of the conversation are system
/// instructions. By the IR contract those live in [`Request::system`]; a
/// request that has them in `messages` anyway means the same thing, so they
/// are sent as top-level `system` rather than as tagged user text.
fn leading_system_messages(request: &Request) -> usize {
    request
        .messages
        .iter()
        .take_while(|message| message.role == Role::System)
        .count()
}

fn encode_messages(request: &Request) -> Vec<Value> {
    let native = same_family(request.source);
    // 1. Mid-conversation system messages do not exist in every model's
    //    Messages API; they become tagged user text.
    let prepared: Vec<Message> = request.messages[leading_system_messages(request)..]
        .iter()
        .filter_map(|message| match message.role {
            Role::System => system_as_user(message),
            _ => Some(message.clone()),
        })
        .collect();
    // 2. Merge same-role neighbours, tool results first in user turns.
    let normalized = ir::normalize_turns(&prepared);

    // 3. Encode; parts this protocol cannot carry may leave a turn empty, so
    //    merge once more on the wire blocks.
    let mut turns: Vec<(Role, Vec<Value>)> = Vec::new();
    for message in &normalized {
        let blocks: Vec<Value> = message
            .parts
            .iter()
            .filter_map(|part| match message.role {
                Role::Assistant => encode_assistant_part(part),
                _ => encode_user_part(part, native),
            })
            .collect();
        if blocks.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((role, existing)) if *role == message.role => existing.extend(blocks),
            _ => turns.push((message.role, blocks)),
        }
    }
    for (role, blocks) in &mut turns {
        if *role == Role::User && blocks.iter().any(|b| is_block(b, "tool_result")) {
            let (results, rest): (Vec<Value>, Vec<Value>) = std::mem::take(blocks)
                .into_iter()
                .partition(|b| is_block(b, "tool_result"));
            *blocks = results;
            blocks.extend(rest);
        }
    }

    // 4. Calls get ids the API accepts and results the id of their call;
    //    then calls and results must pair up turn by turn.
    assign_tool_ids(&mut turns);
    repair_tool_pairing(&mut turns);

    // 5. A trailing assistant turn is a prefill: it may not end in a thinking
    //    block nor in whitespace.
    if let Some((Role::Assistant, blocks)) = turns.last_mut() {
        while blocks
            .last()
            .is_some_and(|b| is_block(b, "thinking") || is_block(b, "redacted_thinking"))
        {
            blocks.pop();
        }
        let trimmed = blocks
            .last()
            .filter(|b| is_block(b, "text"))
            .and_then(|b| str_field(b, "text"))
            .map(|text| text.trim_end().to_string());
        if let Some(trimmed) = trimmed {
            if trimmed.is_empty() {
                blocks.pop();
            } else if let Some(Value::Object(last)) = blocks.last_mut() {
                last.insert("text".to_string(), Value::String(trimmed));
            }
        }
        if blocks.is_empty() {
            turns.pop();
        }
    }

    // 6. The conversation must exist and open with a user turn.
    if !matches!(turns.first(), Some((Role::User, _))) {
        turns.insert(
            0,
            (
                Role::User,
                vec![json!({"type": "text", "text": PLACEHOLDER_USER_TURN})],
            ),
        );
    }

    turns
        .into_iter()
        .map(|(role, blocks)| {
            let role = if role == Role::Assistant {
                "assistant"
            } else {
                "user"
            };
            json!({"role": role, "content": blocks})
        })
        .collect()
}

// ---------------------------------------------------------------------------
// System, tools
// ---------------------------------------------------------------------------

fn encode_system(request: &Request) -> Vec<Value> {
    let leading = &request.messages[..leading_system_messages(request)];
    let mut blocks: Vec<Value> = request
        .system
        .iter()
        .chain(leading.iter().flat_map(|message| message.parts.iter()))
        .filter_map(|part| match part {
            Part::Text(text) => encode_text(&text.text, text.cache_control.as_ref()),
            _ => None,
        })
        .collect();
    if matches!(request.response_format, Some(ResponseFormat::JsonObject)) {
        blocks.push(json!({"type": "text", "text": JSON_OBJECT_INSTRUCTION}));
    }
    blocks
}

fn object_like(schema: &Map<String, Value>) -> bool {
    match schema.get("type") {
        None => true,
        Some(Value::String(kind)) => kind == "object",
        Some(Value::Array(kinds)) => kinds.iter().any(|k| k.as_str() == Some("object")),
        Some(_) => false,
    }
}

/// Makes a parameters schema acceptable as `input_schema`: it must be an
/// object schema and may not have `anyOf` / `oneOf` / `allOf` at the root.
/// Root unions are folded: the properties of object-like branches are merged
/// into the root (existing definitions win) and, for `allOf` only, their
/// `required` lists too. Nested schemas are untouched.
fn input_schema(parameters: &Value) -> Value {
    let Value::Object(schema) = parameters else {
        return empty_object_schema();
    };
    let mut schema = schema.clone();
    for keyword in ["anyOf", "oneOf", "allOf"] {
        let Some(Value::Array(branches)) = schema.shift_remove(keyword) else {
            continue;
        };
        for branch in branches {
            let Value::Object(branch) = branch else {
                continue;
            };
            if !object_like(&branch) {
                continue;
            }
            if let Some(Value::Object(properties)) = branch.get("properties") {
                let slot = schema
                    .entry("properties")
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(root) = slot {
                    for (name, definition) in properties {
                        if !root.contains_key(name) {
                            root.insert(name.clone(), definition.clone());
                        }
                    }
                }
            }
            if keyword == "allOf"
                && let Some(Value::Array(required)) = branch.get("required")
            {
                let slot = schema
                    .entry("required")
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(root) = slot {
                    for name in required {
                        if !root.contains(name) {
                            root.push(name.clone());
                        }
                    }
                }
            }
        }
    }
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        schema.insert("type".to_string(), json!("object"));
    }
    Value::Object(schema)
}

/// Encodes the tool list.
///
/// Tool names must be unique in a request. Two declarations end up with the
/// same name when a client repeats one, or when two names differ only in
/// characters the API does not allow. The first declaration of a name stays
/// and later ones are left out, except that a name which needed no change is
/// never displaced by one that only looks like it after sanitising.
fn encode_tools(request: &Request) -> Vec<Value> {
    // Tool fields the IR cannot hold, remembered by the decoder of this crate.
    let extras = request
        .extra
        .get(TOOL_EXTRAS_KEY)
        .filter(|_| same_family(request.source));
    // Each tool with "its name had to be changed".
    let mut tools: Vec<(Value, bool)> = Vec::new();
    let mut foreign_web_search = false;
    for tool in &request.tools {
        match tool {
            Tool::Function(function) => {
                let mut out = Map::new();
                let name = sanitize_tool_name(&function.name);
                let renamed = name != function.name;
                out.insert("name".to_string(), Value::String(name));
                if let Some(description) = &function.description {
                    out.insert(
                        "description".to_string(),
                        Value::String(description.clone()),
                    );
                }
                out.insert(
                    "input_schema".to_string(),
                    input_schema(&function.parameters),
                );
                if let Some(strict) = function.strict {
                    out.insert("strict".to_string(), Value::Bool(strict));
                }
                if let Some(cache) = &function.cache_control {
                    out.insert("cache_control".to_string(), cache.clone());
                }
                if let Some(Value::Object(fields)) =
                    extras.and_then(|extras| extras.get(&function.name))
                {
                    for (key, value) in fields {
                        if !out.contains_key(key) {
                            out.insert(key.clone(), value.clone());
                        }
                    }
                }
                tools.push((Value::Object(out), renamed));
            }
            // A free-form tool takes raw text; as a Messages tool that text
            // travels in a single string property.
            Tool::Custom(custom) => {
                let mut out = Map::new();
                let name = sanitize_tool_name(&custom.name);
                let renamed = name != custom.name;
                out.insert("name".to_string(), Value::String(name));
                if let Some(description) = &custom.description {
                    out.insert(
                        "description".to_string(),
                        Value::String(description.clone()),
                    );
                }
                out.insert(
                    "input_schema".to_string(),
                    json!({
                        "type": "object",
                        "properties": {"input": {"type": "string"}},
                        "required": ["input"],
                    }),
                );
                tools.push((Value::Object(out), renamed));
            }
            Tool::Builtin(builtin) => {
                if same_family(builtin.origin) {
                    if builtin.raw.is_object() {
                        tools.push((builtin.raw.clone(), false));
                    }
                } else if builtin.kind == BuiltinKind::WebSearch && !foreign_web_search {
                    // The basic variant is the one every model and platform
                    // that has web search accepts.
                    foreign_web_search = true;
                    tools.push((
                        json!({"type": "web_search_20250305", "name": "web_search"}),
                        false,
                    ));
                }
            }
        }
    }

    let exact: HashSet<String> = tools
        .iter()
        .filter(|(_, renamed)| !renamed)
        .filter_map(|(tool, _)| str_field(tool, "name").map(str::to_string))
        .collect();
    let mut taken: HashSet<String> = HashSet::new();
    tools
        .into_iter()
        .filter(|(tool, renamed)| match str_field(tool, "name") {
            Some(name) => !(*renamed && exact.contains(name)) && taken.insert(name.to_string()),
            None => true,
        })
        .map(|(tool, _)| tool)
        .collect()
}

fn encode_tool_choice(request: &Request) -> Option<Value> {
    let serial = request.parallel_tool_calls == Some(false);
    let mut choice = match &request.tool_choice {
        Some(ToolChoice::Auto) => json!({"type": "auto"}),
        Some(ToolChoice::None) => return Some(json!({"type": "none"})),
        Some(ToolChoice::Required) => json!({"type": "any"}),
        Some(ToolChoice::Tool { name }) => {
            json!({"type": "tool", "name": sanitize_tool_name(name)})
        }
        None if serial => json!({"type": "auto"}),
        None => return None,
    };
    if serial && let Some(object) = choice.as_object_mut() {
        object.insert("disable_parallel_tool_use".to_string(), json!(true));
    }
    Some(choice)
}

/// Anthropic's `service_tier` is `auto | standard_only`; other vendors'
/// spellings are mapped where the meaning carries over and dropped otherwise.
fn encode_service_tier(tier: &str) -> Option<&'static str> {
    match tier {
        "auto" | "priority" => Some("auto"),
        "standard_only" | "default" | "standard" => Some("standard_only"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// Encodes a canonical request as a Messages body.
///
/// Decisions worth knowing:
///
/// * **`max_tokens`** is mandatory: the request's limit (capped at the
///   model's output limit when that is known), else the model's output limit,
///   else [`DEFAULT_MAX_TOKENS_THINKING`] when the request asks for thinking,
///   else [`DEFAULT_MAX_TOKENS`].
/// * **Turns** go through [`ir::normalize_turns`]; [`Role::System`] messages
///   at the head of the conversation join the top-level `system`; a
///   mid-conversation one becomes user text wrapped in `<system>…</system>`
///   and merges into the neighbouring user turn; an empty conversation or
///   one opening with an assistant turn gets a leading user turn
///   [`PLACEHOLDER_USER_TURN`]; a trailing assistant turn loses trailing
///   thinking blocks and trailing whitespace.
/// * **Text** that is empty or only whitespace is left out, in messages,
///   tool results and `system`: the API refuses such blocks.
/// * **Reasoning parts** are sent only with a signature issued by Anthropic;
///   everything else (unsigned, foreign) is dropped. Citations on text are
///   dropped (they are not replayable without the API's `encrypted_index`).
/// * **Ids and names**: tool-call ids are mapped onto `[a-zA-Z0-9_-]`,
///   unique per call and consistent between a call and its result (see
///   [`ToolIds`]); tool names likewise (up to 128 characters), and a tool
///   whose name is already taken is left out.
/// * **Manual thinking** (`enabled` + budget) is only sent when the body
///   stays valid: it is left out when no accepted budget fits under
///   `max_tokens`, when tool use is forced, and — for Claude models, whose
///   validator has the rule — when the assistant turn in progress does not
///   open with a thinking block (a tool loop whose signed thinking is not
///   available, because another vendor served it or the client has no slot
///   for it).
/// * **Tool pairing**: a tool result without a call in the preceding
///   assistant turn becomes plain user content and an unanswered call gets
///   an error result ([`INTERRUPTED_TOOL_RESULT`]); the API rejects both
///   situations outright.
/// * **Structured output**: a JSON schema goes to `output_config.format`;
///   "any JSON object" becomes a system instruction.
/// * **Sampling**: `temperature` is clamped to `[0, 1]`; with thinking active
///   all sampling parameters are dropped, otherwise `top_p` is dropped when
///   `temperature` is present.
pub(crate) fn encode_request(
    request: &Request,
    ctx: &UpstreamCtx<'_>,
) -> Result<Value, CodecError> {
    let depth = request.reasoning.as_ref().and_then(|r| r.depth);
    let wants_thinking = depth.is_some_and(|d| d != Depth::Off)
        && !matches!(ctx.thinking, ModelThinking::Unsupported);
    let model_max = ctx.max_output_tokens.filter(|max| *max > 0);
    let max_tokens = match (request.max_output_tokens, model_max) {
        (Some(wanted), Some(max)) => wanted.min(max),
        (Some(wanted), None) => wanted,
        (None, Some(max)) => max,
        (None, None) if wants_thinking => DEFAULT_MAX_TOKENS_THINKING,
        (None, None) => DEFAULT_MAX_TOKENS,
    };

    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(request.model.clone()));
    body.insert("max_tokens".to_string(), json!(max_tokens));

    let system = encode_system(request);
    if !system.is_empty() {
        body.insert("system".to_string(), Value::Array(system));
    }
    body.insert(
        "messages".to_string(),
        Value::Array(encode_messages(request)),
    );

    let tools = encode_tools(request);
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
        // `tool_choice` is only valid alongside tools.
        if let Some(choice) = encode_tool_choice(request) {
            body.insert("tool_choice".to_string(), choice);
        }
    }

    if let Some(ResponseFormat::JsonSchema { schema, .. }) = &request.response_format {
        body.insert(
            "output_config".to_string(),
            json!({"format": {"type": "json_schema", "schema": schema}}),
        );
    }

    let stops: Vec<Value> = request
        .stop
        .iter()
        .filter(|stop| !stop.trim().is_empty())
        .map(|stop| Value::String(stop.clone()))
        .collect();
    if !stops.is_empty() {
        body.insert("stop_sequences".to_string(), Value::Array(stops));
    }
    if let Some(temperature) = request.temperature.filter(|t| t.is_finite()) {
        body.insert(
            "temperature".to_string(),
            json!(temperature.clamp(0.0, 1.0)),
        );
    }
    if let Some(top_p) = request.top_p.filter(|p| p.is_finite()) {
        body.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(top_k) = request.top_k {
        body.insert("top_k".to_string(), json!(top_k));
    }
    // `metadata.user_id` is limited to 256 characters; a longer identifier
    // from another protocol is left out rather than failing the request.
    if let Some(user) = request
        .user
        .as_deref()
        .filter(|u| !u.trim().is_empty() && u.chars().count() <= MAX_USER_ID_CHARS)
    {
        body.insert("metadata".to_string(), json!({"user_id": user}));
    }
    if let Some(tier) = request
        .service_tier
        .as_deref()
        .and_then(encode_service_tier)
    {
        body.insert("service_tier".to_string(), json!(tier));
    }
    if request.stream {
        body.insert("stream".to_string(), json!(true));
    }
    if same_family(request.source) {
        for key in FORWARDED_EXTRAS {
            if let Some(value) = request.extra.get(*key) {
                body.insert((*key).to_string(), value.clone());
            }
        }
    }

    let mut body = Value::Object(body);
    if let Some(depth) = depth {
        write_reasoning(&mut body, Fitted::Use(depth), ctx);
    }
    if let Some(object) = body.as_object_mut() {
        if let Some(summary) = request.reasoning.as_ref().and_then(|r| r.summary) {
            write_summary(object, summary);
        }
        drop_thinking_for_forced_tool_choice(object);
        drop_manual_thinking_without_turn_start(object);
        fix_sampling(object, true);
    }
    Ok(body)
}

fn countable(part: &Part) -> bool {
    match part {
        // The counting endpoint rejects media by URL or file id.
        Part::Image(media) | Part::Document(media) => match &media.source {
            MediaSource::Base64 { .. } => true,
            MediaSource::Url { url } => url.starts_with("data:"),
            MediaSource::FileRef { .. } => false,
        },
        _ => true,
    }
}

fn countable_parts(parts: &[Part]) -> Vec<Part> {
    parts
        .iter()
        .filter(|part| countable(part))
        .map(|part| match part {
            Part::ToolResult(result) => Part::ToolResult(ToolResult {
                content: countable_parts(&result.content),
                ..result.clone()
            }),
            other => other.clone(),
        })
        .collect()
}

/// Encodes the body of `POST /v1/messages/count_tokens`: the subset of a
/// Messages body the endpoint accepts (`model`, `system`, `messages`,
/// `tools`, `tool_choice`, `thinking`, `output_config`). Inputs the endpoint
/// rejects — provider-executed tools, media referenced by URL or file id —
/// are left out, so the count is a slight underestimate for such requests
/// rather than an error.
pub(crate) fn encode_count_request(request: &Request, ctx: &UpstreamCtx<'_>) -> Option<Value> {
    let mut request = request.clone();
    request.stream = false;
    request
        .tools
        .retain(|tool| !matches!(tool, Tool::Builtin(_)));
    if request.tools.is_empty() {
        request.tool_choice = None;
        request.parallel_tool_calls = None;
    }
    for message in &mut request.messages {
        message.parts = countable_parts(&message.parts);
    }
    let body = encode_request(&request, ctx).ok()?;
    let Value::Object(mut body) = body else {
        return None;
    };
    body.retain(|key, _| {
        matches!(
            key.as_str(),
            "model"
                | "system"
                | "messages"
                | "tools"
                | "tool_choice"
                | "thinking"
                | "output_config"
        )
    });
    Some(Value::Object(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn schema_root_unions_are_folded() {
        let schema = json!({
            "anyOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]},
                {"properties": {"b": {"type": "integer"}}},
                {"type": "string"}
            ]
        });
        assert_eq!(
            input_schema(&schema),
            json!({"properties": {"a": {"type": "string"}, "b": {"type": "integer"}},
                   "type": "object"})
        );
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "number"}},
            "allOf": [
                {"properties": {"a": {"type": "string"}, "c": {"type": "boolean"}},
                 "required": ["c"]},
                {"required": ["a", "c"]}
            ]
        });
        assert_eq!(
            input_schema(&schema),
            json!({"type": "object",
                   "properties": {"a": {"type": "number"}, "c": {"type": "boolean"}},
                   "required": ["c", "a"]})
        );
    }

    #[test]
    fn schema_must_be_an_object_schema() {
        assert_eq!(
            input_schema(&Value::Null),
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            input_schema(&json!("x")),
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            input_schema(&json!({"properties": {"q": {"type": "string"}}})),
            json!({"properties": {"q": {"type": "string"}}, "type": "object"})
        );
        let untouched = json!({"type": "object", "properties": {"q": {"anyOf": [{"type": "string"}]}},
                               "additionalProperties": false});
        assert_eq!(input_schema(&untouched), untouched);
    }

    #[test]
    fn service_tiers() {
        assert_eq!(encode_service_tier("auto"), Some("auto"));
        assert_eq!(encode_service_tier("default"), Some("standard_only"));
        assert_eq!(encode_service_tier("flex"), None);
    }
}
