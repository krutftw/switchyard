//! Requests: decoding a client's Responses body into the IR, encoding the IR
//! as a body for a Responses upstream, and the small raw-body helpers used
//! for same-protocol passthrough.

use crate::common::{
    BlobKind, P, ToolIndex, annotation_from_citation, call_id_of, decode_content_part,
    encode_media_part, fit_call_id, fnv64_hex, is_content_part_type, non_empty, qualify,
    reasoning_item_text, signature_from_client, stringish, type_of,
};
use crate::names::UpstreamNames;
use crate::reasoning::{read_reasoning, write_reasoning, write_summary};
use crate::schema::portable_parameters;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, CustomTool, FunctionTool, Message, OpaquePart, Part, Reasoning,
    Request, ResponseFormat, Role, Tool, ToolCall, ToolCallKind, ToolChoice, ToolResult,
};
use switchyard_core::protocol::Family;
use switchyard_core::reasoning::{Depth, Fitted, ModelThinking, Summary};
use switchyard_core::util::{new_call_id, str_field, u64_field};
use switchyard_core::{CodecError, Protocol, RequestMeta, RequestPath, UpstreamCtx};

/// The smallest `max_output_tokens` the vendor accepts; lower values are a
/// 400 there, and other protocols routinely send single-digit limits.
const MIN_MAX_OUTPUT_TOKENS: u64 = 16;

/// Longest item id the vendor accepts.
const MAX_ITEM_ID_CHARS: usize = 64;

/// Longest end-user identifier (`user`, `safety_identifier`) the vendor
/// accepts.
const MAX_USER_CHARS: usize = 64;

/// Top-level keys that only make sense on the WebSocket transport or are
/// consumed by the decoder itself; they never go into [`Request::extra`].
const NON_EXTRA_KEYS: &[&str] = &[
    "model",
    "stream",
    "instructions",
    "input",
    "tools",
    "reasoning",
    "max_output_tokens",
    "parallel_tool_calls",
    "temperature",
    "top_p",
    "store",
    "previous_response_id",
    "metadata",
    "user",
    "prompt_cache_key",
    "service_tier",
    "type",
    "generate",
    "stream_id",
];

/// Fields the token-counting endpoint (`POST /v1/responses/input_tokens`)
/// documents. Everything else `encode_request` writes is generation-only.
const COUNT_KEYS: &[&str] = &[
    "model",
    "instructions",
    "input",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning",
    "text",
    "truncation",
    "previous_response_id",
    "conversation",
];

/// Text of the system message that accompanies `text.format: json_object`
/// when the conversation of another protocol's client never says "JSON" (see
/// [`output_format`]).
pub(crate) const JSON_OBJECT_INSTRUCTION: &str = "Respond with a single valid JSON object and \
     nothing else: no explanations and no markdown code fences.";

/// Lead-in of the instruction that describes a JSON schema the `json_schema`
/// format cannot take (see [`output_format`]).
pub(crate) const JSON_SCHEMA_INSTRUCTION: &str = "Respond with a single valid JSON value that \
     conforms to the JSON Schema below and nothing else: no explanations and no markdown code \
     fences.";

/// `service_tier` values the vendor documents. Tiers of other vendors
/// (`standard_only`, …) must not be forwarded.
const SERVICE_TIERS: &[&str] = &["auto", "default", "flex", "priority", "scale", "ultrafast"];

// ---------------------------------------------------------------------------
// Raw-body helpers
// ---------------------------------------------------------------------------

/// Model name and stream flag of a Responses request body.
pub(crate) fn request_meta(
    body: &Value,
    path: &RequestPath<'_>,
) -> Result<RequestMeta, CodecError> {
    let root = body
        .as_object()
        .ok_or_else(|| CodecError::invalid("request body must be a JSON object"))?;
    let from_path = || {
        path.model
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .ok_or_else(|| CodecError::invalid_param("model", "missing required field `model`"))
    };
    let model = match root.get("model") {
        Some(Value::String(model)) if !model.trim().is_empty() => model.clone(),
        Some(Value::String(_)) | Some(Value::Null) | None => from_path()?,
        Some(_) => {
            return Err(CodecError::invalid_param(
                "model",
                "`model` must be a string",
            ));
        }
    };
    let stream = match root.get("stream") {
        Some(Value::Bool(stream)) => *stream,
        _ => path.stream.unwrap_or(false),
    };
    Ok(RequestMeta { model, stream })
}

/// Overwrites `model` in a request body.
pub(crate) fn set_request_model(body: &mut Value, model: &str) {
    if let Some(root) = body.as_object_mut() {
        root.insert("model".into(), Value::String(model.to_string()));
    }
}

/// Makes the `stream` flag of a forwarded body agree with how the gateway is
/// going to call the upstream. Nothing else is touched: a Responses body that
/// was valid for the client is valid for the upstream.
pub(crate) fn prepare_passthrough(body: &mut Value, stream: bool) {
    let Some(root) = body.as_object_mut() else {
        return;
    };
    if stream {
        root.insert("stream".into(), Value::Bool(true));
    } else if root.contains_key("stream") {
        root.insert("stream".into(), Value::Bool(false));
    }
}

// ---------------------------------------------------------------------------
// decode_request
// ---------------------------------------------------------------------------

/// Which message the next grouped part may be appended to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    /// Reasoning, assistant text and tool calls of one assistant turn.
    Assistant,
    /// Consecutive tool outputs.
    ToolResults,
    /// Consecutive bare content parts found at item level.
    BareUser,
}

/// Accumulates IR messages while walking the item list.
#[derive(Default)]
struct Conversation {
    system: Vec<Part>,
    messages: Vec<Message>,
    /// The kind of group the last message is, when it may still grow.
    open: Option<Group>,
    /// A conversation item (anything but a system/developer message) was seen.
    started: bool,
    /// Tool calls not yet answered, oldest first: `(call id, name)`.
    pending_calls: Vec<(String, String)>,
    /// Call ids named explicitly by some output item of the input. Such calls
    /// are never handed to an output that lacks a call id.
    explicit_outputs: HashSet<String>,
    /// The signature of the tool call that comes next: the blob of the
    /// carrier reasoning item directly ahead of it (see
    /// [`BlobKind::Call`]).
    call_signature: Option<switchyard_core::Signature>,
}

impl Conversation {
    fn push_grouped(&mut self, group: Group, role: Role, part: Part) {
        self.started = true;
        if self.open != Some(group) || self.messages.is_empty() {
            self.messages.push(Message::new(role, Vec::new()));
            self.open = Some(group);
        }
        if let Some(last) = self.messages.last_mut() {
            last.parts.push(part);
        }
    }

    fn push_assistant(&mut self, part: Part) {
        self.push_grouped(Group::Assistant, Role::Assistant, part);
    }

    fn push_message(&mut self, message: Message) {
        self.open = None;
        self.messages.push(message);
    }

    /// Picks the call an output without a call id answers: the oldest pending
    /// call that no explicit output in the input claims, preferring one with
    /// the output's tool name.
    fn claim_pending(&mut self, name: Option<&str>) -> Option<String> {
        let free = |entry: &(String, String)| !self.explicit_outputs.contains(&entry.0);
        let position = name
            .and_then(|name| {
                self.pending_calls
                    .iter()
                    .position(|entry| free(entry) && entry.1 == name)
            })
            .or_else(|| self.pending_calls.iter().position(free))?;
        Some(self.pending_calls.remove(position).0)
    }
}

/// Decodes a client's Responses request.
///
/// Item list → messages: every `message` item becomes one IR message, except
/// that the items of one assistant turn (reasoning, assistant messages, tool
/// calls, provider-specific call items) are folded into a single assistant
/// message and consecutive tool outputs into a single user message. `system`
/// / `developer` messages ahead of the conversation join `instructions` in
/// [`Request::system`]; later ones stay in place with [`Role::System`].
pub(crate) fn decode_request(body: &Value, path: &RequestPath<'_>) -> Result<Request, CodecError> {
    let meta = request_meta(body, path)?;
    let root = body
        .as_object()
        .ok_or_else(|| CodecError::invalid("request body must be a JSON object"))?;
    let mut request = Request::new(meta.model, P);
    request.stream = meta.stream;

    let mut conversation = Conversation::default();
    match root.get("instructions") {
        Some(Value::String(text)) if !text.is_empty() => {
            conversation.system.push(Part::text(text.clone()));
        }
        // The item-list form of `instructions` (prompt templates) carries
        // messages; only their content matters here.
        Some(Value::Array(items)) => {
            for item in items {
                let content = item.get("content").unwrap_or(item);
                conversation
                    .system
                    .extend(decode_message_content(Some(content)));
            }
        }
        _ => {}
    }

    let mut late_tools: Vec<&Value> = Vec::new();
    match root.get("input") {
        None | Some(Value::Null) => {
            let continues = ["previous_response_id", "prompt", "conversation"]
                .iter()
                .any(|key| root.get(*key).is_some_and(|v| !v.is_null()));
            if !continues {
                return Err(CodecError::invalid_param(
                    "input",
                    "missing required field `input`",
                ));
            }
        }
        Some(Value::String(text)) => {
            if !text.is_empty() {
                conversation.started = true;
                conversation.push_message(Message::user_text(text.clone()));
            }
        }
        Some(Value::Array(items)) => {
            conversation.explicit_outputs = items
                .iter()
                .filter(|item| is_output_type(type_of(item)))
                .map(|item| call_id_of(item, true))
                .filter(|id| !id.is_empty())
                .collect();
            for item in items {
                decode_item(item, &mut conversation, &mut late_tools);
            }
        }
        Some(item @ Value::Object(_)) => decode_item(item, &mut conversation, &mut late_tools),
        Some(_) => {
            return Err(CodecError::invalid_param(
                "input",
                "`input` must be a string or an array of input items",
            ));
        }
    }
    request.system = conversation.system;
    request.messages = conversation.messages;

    // Tools: top-level declarations first, then `additional_tools` items in
    // input order; the first declaration of a name wins.
    let mut seen = HashSet::new();
    match root.get("tools") {
        None | Some(Value::Null) => {}
        Some(Value::Array(tools)) => {
            for tool in tools {
                decode_tool(tool, None, &mut request.tools, &mut seen);
            }
        }
        Some(_) => {
            return Err(CodecError::invalid_param(
                "tools",
                "`tools` must be an array",
            ));
        }
    }
    for tool in late_tools {
        decode_tool(tool, None, &mut request.tools, &mut seen);
    }

    if let Some(choice) = root.get("tool_choice").filter(|v| !v.is_null()) {
        let (decoded, keep_raw) = decode_tool_choice(choice);
        request.tool_choice = decoded;
        if keep_raw {
            request.extra.insert("tool_choice".into(), choice.clone());
        }
    }

    // Namespaced tools were flattened to `<namespace>__<name>`. History and
    // `tool_choice` may name such a tool by its local name alone; they must
    // end up with the name the declaration got, or the upstream is told to
    // call (or shown a call to) a tool it was never offered.
    let declared = ToolIndex::from_request(body);

    // `allowed_tools` restricts the model to some of the declared tools
    // without rewriting `tools` (which keeps the prompt cache warm). The IR
    // has no such notion and neither have the other protocols, so the tool
    // list itself is narrowed: offering every tool with the mode alone would
    // let the model call exactly the tools the client excluded.
    if let Some(allowed) = root.get("tool_choice").and_then(allowed_tools) {
        let named: HashSet<&str> = allowed
            .iter()
            .filter_map(|entry| entry.name.as_deref())
            .map(|name| declared.flat_name(name).unwrap_or(name))
            .collect();
        request.tools.retain(|tool| match tool {
            Tool::Builtin(builtin) => allowed.iter().any(|entry| entry.permits(&builtin.raw)),
            named_tool => named_tool.name().is_some_and(|name| named.contains(name)),
        });
        if request.tools.is_empty() {
            request.tool_choice = Some(ToolChoice::None);
        }
    }
    let flat_name = |name: &mut String| {
        if let Some(flat) = declared.flat_name(name)
            && flat != name.as_str()
        {
            *name = flat.to_string();
        }
    };
    for message in &mut request.messages {
        for part in &mut message.parts {
            if let Part::ToolCall(call) = part {
                flat_name(&mut call.name);
            }
        }
    }
    if let Some(ToolChoice::Tool { name }) = &mut request.tool_choice {
        flat_name(name);
    }
    request.parallel_tool_calls = root.get("parallel_tool_calls").and_then(Value::as_bool);
    request.max_output_tokens = u64_field(body, "max_output_tokens");
    request.temperature = root.get("temperature").and_then(Value::as_f64);
    request.top_p = root.get("top_p").and_then(Value::as_f64);

    let reasoning = read_reasoning(body);
    if !reasoning.is_empty() {
        request.reasoning = Some(reasoning);
    }

    // `text.format` is the structured-output switch; the rest of `text`
    // (verbosity) has no IR slot and rides along in `extra`.
    let text = root.get("text").filter(|v| v.is_object());
    let format = text
        .and_then(|t| t.get("format"))
        // Chat-style spelling, sent by clients ported from Chat Completions.
        .or_else(|| root.get("response_format"))
        .filter(|v| v.is_object());
    request.response_format = format.and_then(decode_format);
    if let Some(Value::Object(text)) = text {
        let rest: Map<String, Value> = text
            .iter()
            .filter(|(key, _)| key.as_str() != "format")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if !rest.is_empty() {
            request.extra.insert("text".into(), Value::Object(rest));
        }
    }

    request.user = non_empty(body, "user")
        .or_else(|| non_empty(body, "safety_identifier"))
        .map(str::to_string);
    request.metadata = root
        .get("metadata")
        .and_then(Value::as_object)
        .filter(|m| !m.is_empty())
        .cloned();
    request.service_tier = non_empty(body, "service_tier").map(str::to_string);
    request.prompt_cache_key = non_empty(body, "prompt_cache_key").map(str::to_string);
    request.previous_response_id = non_empty(body, "previous_response_id").map(str::to_string);
    request.store = root.get("store").and_then(Value::as_bool);

    for (key, value) in root {
        let consumed = NON_EXTRA_KEYS.contains(&key.as_str())
            || matches!(key.as_str(), "tool_choice" | "text" | "response_format");
        if consumed || value.is_null() || request.extra.contains_key(key) {
            continue;
        }
        request.extra.insert(key.clone(), value.clone());
    }
    Ok(request)
}

fn is_output_type(kind: &str) -> bool {
    matches!(kind, "function_call_output" | "custom_tool_call_output")
}

fn decode_item<'a>(
    item: &'a Value,
    conversation: &mut Conversation,
    late_tools: &mut Vec<&'a Value>,
) {
    if let Value::String(text) = item {
        // A bare string inside the item list: treat it as user text.
        if !text.is_empty() {
            conversation.push_grouped(Group::BareUser, Role::User, Part::text(text.clone()));
        }
        return;
    }
    if !item.is_object() {
        return;
    }
    let kind = match type_of(item) {
        "" if item.get("role").is_some() => "message",
        "" if item.get("text").is_some() => "input_text",
        other => other,
    };
    // Only the item directly behind a carrier can be the call it signs.
    let call_signature = conversation.call_signature.take();
    match kind {
        "message" => decode_message(item, conversation),
        "function_call" | "custom_tool_call" => {
            let custom = kind == "custom_tool_call";
            let mut id = call_id_of(item, false);
            if id.is_empty() {
                id = new_call_id();
            }
            let name = str_field(item, "name").unwrap_or("").trim();
            let name = match non_empty(item, "namespace") {
                Some(namespace) => qualify(namespace, name),
                None => name.to_string(),
            };
            if let Some(text) = non_empty(item, "reasoning_content") {
                conversation.push_assistant(Part::reasoning(text));
            }
            conversation.pending_calls.push((id.clone(), name.clone()));
            conversation.push_assistant(Part::ToolCall(ToolCall {
                id,
                name,
                arguments: stringish(item.get(if custom { "input" } else { "arguments" })),
                kind: if custom {
                    ToolCallKind::Custom
                } else {
                    ToolCallKind::Function
                },
                signature: call_signature,
                cache_control: None,
            }));
        }
        "function_call_output" | "custom_tool_call_output" => {
            let content = decode_tool_output(item.get("output"));
            push_tool_result(item, content, conversation);
        }
        "reasoning" => {
            let text = reasoning_item_text(item);
            let blob = non_empty(item, "encrypted_content").map(signature_from_client);
            // An id-only item refers to state stored at the vendor; it cannot
            // be replayed anywhere and carries nothing to translate.
            if text.is_empty() && blob.is_none() {
                return;
            }
            let (signature, redacted) = match blob {
                // The signature of the tool call that follows: it goes back
                // onto the call. Text a client put on the carrier stays as
                // unsigned reasoning.
                Some((signature, BlobKind::Call)) => {
                    conversation.call_signature = Some(signature);
                    (None, false)
                }
                Some((signature, kind)) => (Some(signature), kind == BlobKind::Redacted),
                None => (None, false),
            };
            if text.is_empty() && signature.is_none() {
                return;
            }
            conversation.push_assistant(Part::Reasoning(Reasoning {
                id: non_empty(item, "id").map(str::to_string),
                text,
                signature,
                redacted,
            }));
        }
        // References to stored items and per-turn configuration carry no
        // conversation content (the latter is read by `read_reasoning`).
        "item_reference" | "configuration_update" => {}
        "additional_tools" => {
            if let Some(tools) = item.get("tools").and_then(Value::as_array) {
                late_tools.extend(tools.iter());
            }
        }
        other if is_content_part_type(other) => {
            if let Some(part) = decode_content_part(item).filter(part_has_content) {
                conversation.push_grouped(Group::BareUser, Role::User, part);
            }
        }
        other => {
            let part = Part::Opaque(OpaquePart {
                origin: P,
                raw: item.clone(),
            });
            // Results the client hands back (`computer_call_output`,
            // `mcp_approval_response`, …) belong to the user side.
            if other.ends_with("_output") || other.ends_with("_response") {
                conversation.push_grouped(Group::ToolResults, Role::User, part);
            } else {
                conversation.push_assistant(part);
            }
        }
    }
}

/// Adds a tool result, pairing it with the call it answers. An output that
/// names no call takes the oldest unanswered one (see
/// [`Conversation::claim_pending`]).
fn push_tool_result(item: &Value, content: Vec<Part>, conversation: &mut Conversation) {
    let name = non_empty(item, "name");
    let mut call_id = call_id_of(item, true);
    if call_id.is_empty() {
        call_id = conversation.claim_pending(name).unwrap_or_default();
    } else {
        conversation
            .pending_calls
            .retain(|entry| entry.0 != call_id);
    }
    conversation.push_grouped(
        Group::ToolResults,
        Role::User,
        Part::ToolResult(ToolResult {
            call_id,
            name: name.map(str::to_string),
            content,
            is_error: false,
            cache_control: item.get("cache_control").filter(|v| !v.is_null()).cloned(),
        }),
    );
}

/// Empty text and refusal parts carry nothing and upset stricter vendors.
fn part_has_content(part: &Part) -> bool {
    match part {
        Part::Text(text) => !text.text.is_empty(),
        Part::Refusal(refusal) => !refusal.text.is_empty(),
        _ => true,
    }
}

fn decode_message_content(content: Option<&Value>) -> Vec<Part> {
    match content {
        Some(Value::String(text)) if !text.is_empty() => vec![Part::text(text.clone())],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(decode_content_part)
            .filter(part_has_content)
            .collect(),
        Some(part @ Value::Object(_)) => decode_content_part(part)
            .filter(part_has_content)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn decode_message(item: &Value, conversation: &mut Conversation) {
    let role = str_field(item, "role")
        .unwrap_or("user")
        .trim()
        .to_ascii_lowercase();
    let parts = decode_message_content(item.get("content"));
    match role.as_str() {
        "assistant" | "model" => {
            if let Some(text) = non_empty(item, "reasoning_content") {
                conversation.push_assistant(Part::reasoning(text));
            }
            for part in parts {
                conversation.push_assistant(part);
            }
        }
        "system" | "developer" => {
            if parts.is_empty() {
                return;
            }
            if conversation.started {
                conversation.push_message(Message::new(Role::System, parts));
            } else {
                conversation.system.extend(parts);
            }
        }
        // Not a Responses role, but clients ported from Chat Completions
        // send tool results this way.
        "tool" | "function" => push_tool_result(item, parts, conversation),
        _ => {
            conversation.started = true;
            if !parts.is_empty() {
                conversation.push_message(Message::new(Role::User, parts));
            }
        }
    }
}

/// Recognises a string output that is really a serialised list of content
/// parts carrying an image (tools that return screenshots are often wired up
/// to stringify whatever they produce). Left as text, the image would reach
/// the model as a base64 blob in prose.
///
/// The bar is deliberately high so ordinary JSON-looking results stay text:
/// the string must parse as an array, hold at least one well-formed image
/// part, and every text / image part in it must be well-formed (string
/// `text`; string URL or file id; string-or-absent `detail`). Entries of
/// other kinds are kept as text holding their JSON.
fn stringified_parts(text: &str) -> Option<Vec<Part>> {
    let trimmed = text.trim();
    if !trimmed.starts_with('[') {
        return None;
    }
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(trimmed) else {
        return None;
    };
    let is_text_or_absent =
        |value: Option<&Value>| value.is_none_or(|v| v.is_null() || v.is_string());
    let mut parts = Vec::with_capacity(entries.len());
    let mut has_image = false;
    for entry in &entries {
        match type_of(entry) {
            "text" | "input_text" | "output_text" => {
                parts.push(Part::text(entry.get("text")?.as_str()?));
            }
            "image_url" | "input_image" => {
                let nested = entry.get("image_url").filter(|v| v.is_object());
                if !is_text_or_absent(entry.get("detail"))
                    || !is_text_or_absent(nested.and_then(|n| n.get("detail")))
                {
                    return None;
                }
                match decode_content_part(entry)? {
                    image @ Part::Image(_) => parts.push(image),
                    _ => return None,
                }
                has_image = true;
            }
            _ => parts.push(Part::text(entry.to_string())),
        }
    }
    has_image.then_some(parts)
}

/// `function_call_output.output`: a string, an array of content parts, or
/// (from lenient clients) any JSON value.
fn decode_tool_output(output: Option<&Value>) -> Vec<Part> {
    match output {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) if text.is_empty() => Vec::new(),
        Some(Value::String(text)) => match stringified_parts(text) {
            Some(parts) => parts.into_iter().filter(part_has_content).collect(),
            None => vec![Part::text(text.clone())],
        },
        Some(Value::Array(entries)) => {
            let decoded: Option<Vec<Part>> = entries
                .iter()
                .map(|entry| match decode_content_part(entry) {
                    Some(Part::Opaque(_)) | None => None,
                    Some(part) => Some(part),
                })
                .collect();
            match decoded {
                Some(parts) => parts.into_iter().filter(part_has_content).collect(),
                // Not a content-part list: the array itself is the result.
                None => vec![Part::text(Value::Array(entries.clone()).to_string())],
            }
        }
        Some(other) => vec![Part::text(other.to_string())],
    }
}

fn decode_tool(
    tool: &Value,
    namespace: Option<&str>,
    out: &mut Vec<Tool>,
    seen: &mut HashSet<String>,
) {
    if !tool.is_object() {
        return;
    }
    let kind = match type_of(tool) {
        "" => "function",
        other => other,
    };
    match kind {
        "namespace" => {
            let Some(name) = non_empty(tool, "name") else {
                return;
            };
            if let Some(children) = tool.get("tools").and_then(Value::as_array) {
                for child in children {
                    decode_tool(child, Some(name), out, seen);
                }
            }
        }
        "function" | "custom" => {
            // The Chat-style nested form is accepted next to the flat one.
            let inner = tool.get(kind).filter(|v| v.is_object());
            let field = |key: &str| {
                tool.get(key)
                    .filter(|v| !v.is_null())
                    .or_else(|| inner.and_then(|i| i.get(key)).filter(|v| !v.is_null()))
            };
            let Some(name) = field("name").and_then(Value::as_str).map(str::trim) else {
                return;
            };
            if name.is_empty() {
                return;
            }
            let name = match namespace {
                Some(namespace) => qualify(namespace, name),
                None => name.to_string(),
            };
            if !seen.insert(name.clone()) {
                return;
            }
            let description = field("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            if kind == "custom" {
                out.push(Tool::Custom(CustomTool {
                    name,
                    description,
                    format: field("format").cloned(),
                }));
            } else {
                let parameters = ["parameters", "parametersJsonSchema", "input_schema"]
                    .iter()
                    .find_map(|key| field(key))
                    .cloned()
                    .unwrap_or(Value::Null);
                out.push(Tool::Function(FunctionTool {
                    name,
                    description,
                    parameters,
                    strict: field("strict").and_then(Value::as_bool),
                    cache_control: None,
                }));
            }
        }
        other => {
            let builtin = if other.starts_with("web_search") {
                BuiltinKind::WebSearch
            } else if other == "code_interpreter" {
                BuiltinKind::CodeExecution
            } else {
                BuiltinKind::Other(other.to_string())
            };
            out.push(Tool::Builtin(BuiltinTool {
                kind: builtin,
                origin: P,
                raw: tool.clone(),
            }));
        }
    }
}

/// Decodes `tool_choice`. The flag says the wire value could not be fully
/// expressed and should be kept verbatim for same-family upstreams.
fn decode_tool_choice(choice: &Value) -> (Option<ToolChoice>, bool) {
    let by_mode = |mode: &str| match mode.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(ToolChoice::Auto),
        "none" => Some(ToolChoice::None),
        "required" | "any" => Some(ToolChoice::Required),
        _ => None,
    };
    match choice {
        Value::String(mode) => (by_mode(mode), false),
        Value::Object(_) => {
            let kind = type_of(choice);
            match kind {
                "" | "function" | "custom" | "tool" => {
                    let inner = ["function", "custom"]
                        .iter()
                        .find_map(|key| choice.get(*key).filter(|v| v.is_object()));
                    let name = non_empty(choice, "name")
                        .or_else(|| inner.and_then(|i| non_empty(i, "name")));
                    let namespace = non_empty(choice, "namespace")
                        .or_else(|| inner.and_then(|i| non_empty(i, "namespace")));
                    match name {
                        Some(name) => {
                            let name = match namespace {
                                Some(namespace) => qualify(namespace, name),
                                None => name.to_string(),
                            };
                            (Some(ToolChoice::Tool { name }), false)
                        }
                        None => (None, false),
                    }
                }
                // The mode; the caller narrows the tool list to the allowed
                // ones (see `allowed_tools`).
                "allowed_tools" => {
                    let mode = str_field(allowed_tools_spec(choice), "mode").and_then(by_mode);
                    (Some(mode.unwrap_or(ToolChoice::Auto)), true)
                }
                "auto" | "none" | "required" | "any" => (by_mode(kind), false),
                // A hosted tool is being forced. No other vendor can honour
                // that, and "required" could force an unrelated function, so
                // the portable reading is "auto".
                _ => (Some(ToolChoice::Auto), true),
            }
        }
        _ => (None, false),
    }
}

/// Where an `allowed_tools` choice keeps its `mode` and `tools`: on the
/// choice itself, or (Chat Completions' spelling, sent by clients ported
/// from it) under a nested `allowed_tools` object.
fn allowed_tools_spec(choice: &Value) -> &Value {
    choice
        .get("allowed_tools")
        .filter(|nested| nested.is_object())
        .unwrap_or(choice)
}

/// One entry of the `tools` list of an `allowed_tools` choice.
struct AllowedTool {
    /// The entry's `type`: `function`, `custom`, or a hosted tool's type.
    kind: String,
    /// Flat name of a function or custom tool (namespace-qualified when the
    /// entry names a namespace).
    name: Option<String>,
    /// `server_label` of an `mcp` entry.
    server_label: Option<String>,
}

impl AllowedTool {
    /// Whether this entry allows the hosted tool declared as `declaration`.
    fn permits(&self, declaration: &Value) -> bool {
        let declared = type_of(declaration);
        let same_kind = self.kind == declared
            // `web_search`, `web_search_preview` and their dated variants
            // name one tool.
            || (self.kind.starts_with("web_search") && declared.starts_with("web_search"));
        same_kind
            && self
                .server_label
                .as_deref()
                .is_none_or(|label| non_empty(declaration, "server_label") == Some(label))
    }
}

/// The tools an `allowed_tools` choice restricts the model to; `None` for
/// every other kind of choice.
fn allowed_tools(choice: &Value) -> Option<Vec<AllowedTool>> {
    if type_of(choice) != "allowed_tools" {
        return None;
    }
    let entries = allowed_tools_spec(choice)
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    Some(
        entries
            .iter()
            .filter(|entry| entry.is_object())
            .map(|entry| {
                let kind = match type_of(entry) {
                    "" => "function",
                    other => other,
                };
                let inner = entry.get(kind).filter(|v| v.is_object());
                let field = |key: &str| {
                    non_empty(entry, key).or_else(|| inner.and_then(|inner| non_empty(inner, key)))
                };
                let name = matches!(kind, "function" | "custom")
                    .then(|| field("name"))
                    .flatten()
                    .map(|name| match field("namespace") {
                        Some(namespace) => qualify(namespace, name),
                        None => name.to_string(),
                    });
                AllowedTool {
                    kind: kind.to_string(),
                    name,
                    server_label: non_empty(entry, "server_label").map(str::to_string),
                }
            })
            .collect(),
    )
}

fn decode_format(format: &Value) -> Option<ResponseFormat> {
    match type_of(format) {
        "text" => Some(ResponseFormat::Text),
        "json_object" => Some(ResponseFormat::JsonObject),
        "json_schema" => {
            // Responses puts the schema fields directly under `format`;
            // Chat nests them under `json_schema`. Accept both.
            let nested = format.get("json_schema").filter(|v| v.is_object());
            let field = |key: &str| {
                format
                    .get(key)
                    .filter(|v| !v.is_null())
                    .or_else(|| nested.and_then(|n| n.get(key)).filter(|v| !v.is_null()))
            };
            Some(ResponseFormat::JsonSchema {
                name: field("name").and_then(Value::as_str).map(str::to_string),
                description: field("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                schema: field("schema").cloned().unwrap_or(Value::Null),
                strict: field("strict").and_then(Value::as_bool),
            })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// encode_request
// ---------------------------------------------------------------------------

/// Builds the `input` item list.
struct InputBuilder<'a> {
    items: Vec<Value>,
    /// Content of the message item being assembled, with its wire role.
    pending: Option<(&'static str, Vec<Value>)>,
    /// Reasoning items waiting for the item they precede. The vendor rejects
    /// a reasoning item that is not followed by output of the same turn, so
    /// they are only written once such an item shows up.
    held_reasoning: Vec<Value>,
    /// Call ids of custom tool calls, to type their outputs.
    custom_calls: HashSet<&'a str>,
    /// Provider file handles in this request were issued by OpenAI.
    openai_source: bool,
    /// The target model takes reasoning items as input. One that is known
    /// not to reason rejects them outright, however valid the blob.
    replay_reasoning: bool,
    signed_reasoning: bool,
    /// How the upstream is told the tool names of this request.
    names: &'a UpstreamNames,
}

impl InputBuilder<'_> {
    fn flush(&mut self) {
        if let Some((role, content)) = self.pending.take()
            && !content.is_empty()
        {
            self.items
                .push(json!({"type": "message", "role": role, "content": content}));
        }
    }

    fn release_reasoning(&mut self) {
        if !self.held_reasoning.is_empty() {
            self.signed_reasoning = true;
            self.items.append(&mut self.held_reasoning);
        }
    }

    fn content(&mut self, role: &'static str, part: Value) {
        if self.pending.as_ref().is_some_and(|(open, _)| *open != role) {
            self.flush();
        }
        if self.pending.is_none() {
            self.release_reasoning();
            self.pending = Some((role, Vec::new()));
        }
        if let Some((_, content)) = self.pending.as_mut() {
            content.push(part);
        }
    }

    fn item(&mut self, item: Value) {
        self.flush();
        self.release_reasoning();
        self.items.push(item);
    }

    fn opaque(&mut self, role: &'static str, opaque: &OpaquePart) {
        // Only blocks this very protocol produced are valid items here.
        if opaque.origin != P || !opaque.raw.is_object() {
            return;
        }
        if is_content_part_type(type_of(&opaque.raw)) {
            self.content(role, opaque.raw.clone());
        } else {
            self.item(opaque.raw.clone());
        }
    }

    fn tool_result(&mut self, result: &ToolResult) {
        let custom = self.custom_calls.contains(result.call_id.as_str());
        let has_media = result
            .content
            .iter()
            .any(|p| matches!(p, Part::Image(_) | Part::Document(_) | Part::Audio(_)));
        let output = if has_media {
            let mut parts = Vec::new();
            for part in &result.content {
                match part {
                    Part::Text(text) if !text.text.is_empty() => {
                        parts.push(json!({"type": "input_text", "text": text.text}));
                    }
                    // Audio has no slot inside a tool output.
                    Part::Image(_) | Part::Document(_) => {
                        if let Some(encoded) = encode_media_part(part, self.openai_source) {
                            parts.push(encoded);
                        }
                    }
                    _ => {}
                }
            }
            if parts.is_empty() {
                // Nothing expressible was left; an empty array is not a
                // valid output, an empty string is.
                Value::String(String::new())
            } else {
                Value::Array(parts)
            }
        } else {
            Value::String(result.text())
        };
        self.item(json!({
            "type": if custom { "custom_tool_call_output" } else { "function_call_output" },
            "call_id": fit_call_id(&result.call_id),
            "output": output,
        }));
    }

    fn user_message(&mut self, role: &'static str, parts: &[Part]) {
        for part in parts {
            match part {
                Part::Text(text) => {
                    if !text.text.is_empty() {
                        self.content(role, json!({"type": "input_text", "text": text.text}));
                    }
                }
                Part::Image(_) | Part::Document(_) | Part::Audio(_) => {
                    if let Some(encoded) = encode_media_part(part, self.openai_source) {
                        self.content(role, encoded);
                    }
                }
                Part::ToolResult(result) => self.tool_result(result),
                Part::Opaque(opaque) => self.opaque(role, opaque),
                // Model-side parts have no meaning in a user turn.
                Part::ToolCall(_) | Part::Reasoning(_) | Part::Refusal(_) => {}
            }
        }
        self.flush();
    }

    fn assistant_message(&mut self, parts: &[Part]) {
        for part in parts {
            match part {
                Part::Text(text) => {
                    if text.text.is_empty() {
                        continue;
                    }
                    let mut out = Map::new();
                    out.insert("type".into(), json!("output_text"));
                    out.insert("text".into(), json!(text.text));
                    let annotations: Vec<Value> = text
                        .citations
                        .iter()
                        .filter_map(annotation_from_citation)
                        .collect();
                    if !annotations.is_empty() {
                        out.insert("annotations".into(), Value::Array(annotations));
                    }
                    self.content("assistant", Value::Object(out));
                }
                Part::Refusal(refusal) => {
                    self.content(
                        "assistant",
                        json!({"type": "refusal", "refusal": refusal.text}),
                    );
                }
                Part::ToolCall(call) => {
                    let call_id = fit_call_id(&call.id);
                    let name = self.names.wire(&call.name);
                    let item = match call.kind {
                        ToolCallKind::Custom => json!({
                            "type": "custom_tool_call",
                            "call_id": call_id,
                            "name": name,
                            "input": call.arguments,
                        }),
                        ToolCallKind::Function => json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            // Strict upstreams reject an empty argument string.
                            "arguments": if call.arguments.trim().is_empty() {
                                "{}"
                            } else {
                                call.arguments.as_str()
                            },
                        }),
                    };
                    self.item(item);
                }
                Part::Reasoning(reasoning) => {
                    // Without a blob this vendor issued the item is rejected,
                    // and a foreign blob cannot be decrypted: drop both. A
                    // blob a Chat Completions upstream issued is as foreign
                    // here as any other (it is some compatible server's
                    // signature, not OpenAI's encrypted reasoning).
                    let Some(signature) = reasoning.signature.as_ref().filter(|s| s.origin == P)
                    else {
                        continue;
                    };
                    if !self.replay_reasoning {
                        continue;
                    }
                    self.flush();
                    let mut item = Map::new();
                    item.insert("type".into(), json!("reasoning"));
                    // The id is optional next to the blob, and the vendor
                    // rejects one that is not in its own `rs_…` shape.
                    if let Some(id) = reasoning.id.as_deref().filter(|id| {
                        id.starts_with("rs_") && id.chars().count() <= MAX_ITEM_ID_CHARS
                    }) {
                        item.insert("id".into(), json!(id));
                    }
                    let summary = if reasoning.text.is_empty() {
                        Vec::new()
                    } else {
                        vec![json!({"type": "summary_text", "text": reasoning.text})]
                    };
                    item.insert("summary".into(), Value::Array(summary));
                    item.insert("encrypted_content".into(), json!(signature.data));
                    self.held_reasoning.push(Value::Object(item));
                }
                Part::Opaque(opaque) => self.opaque("assistant", opaque),
                // Generated media and stray tool results cannot be replayed
                // as assistant input.
                Part::Image(_) | Part::Audio(_) | Part::Document(_) | Part::ToolResult(_) => {}
            }
        }
        self.flush();
        // Reasoning that nothing followed would be rejected.
        self.held_reasoning.clear();
    }
}

fn encode_tools(request: &Request, names: &UpstreamNames) -> Vec<Value> {
    let mut tools = Vec::new();
    for tool in &request.tools {
        match tool {
            Tool::Function(function) => {
                let mut out = Map::new();
                out.insert("type".into(), json!("function"));
                out.insert("name".into(), json!(names.wire(&function.name)));
                if let Some(description) = &function.description {
                    out.insert("description".into(), json!(description));
                }
                // A schema written for another protocol is made acceptable
                // here; a Responses client's own is forwarded as written.
                let parameters = if request.source == P {
                    function.parameters_or_empty()
                } else {
                    portable_parameters(&function.parameters)
                };
                out.insert("parameters".into(), parameters);
                // Responses defaults `strict` to true, every other protocol
                // to false: an absent flag must be spelled out unless the
                // request came from a Responses client (whose omission meant
                // the Responses default). `strict` of another vendor's
                // client is not taken over: strict mode here wants every
                // property listed in `required`, which a schema written
                // for Anthropic's strict tools does not promise, and
                // answers anything else with a 400.
                match function.strict {
                    Some(strict) if request.source.family() == Family::Openai => {
                        out.insert("strict".into(), json!(strict));
                    }
                    _ if request.source != P => {
                        out.insert("strict".into(), json!(false));
                    }
                    _ => {}
                }
                tools.push(Value::Object(out));
            }
            Tool::Custom(custom) => {
                let mut out = Map::new();
                out.insert("type".into(), json!("custom"));
                out.insert("name".into(), json!(names.wire(&custom.name)));
                if let Some(description) = &custom.description {
                    out.insert("description".into(), json!(description));
                }
                if let Some(format) = &custom.format {
                    out.insert("format".into(), format.clone());
                }
                tools.push(Value::Object(out));
            }
            Tool::Builtin(builtin) => {
                if builtin.origin == P {
                    tools.push(builtin.raw.clone());
                    continue;
                }
                // Another family's declaration: this API's own default for
                // the same kind of tool, once.
                let Some(default) = hosted_tool(&builtin.kind) else {
                    continue;
                };
                if !tools.contains(&default) {
                    tools.push(default);
                }
            }
        }
    }
    tools
}

/// This API's default declaration of the hosted tool that does what a
/// provider-executed tool of another vendor does. `None` when there is no
/// hosted equivalent.
fn hosted_tool(kind: &BuiltinKind) -> Option<Value> {
    match kind {
        BuiltinKind::WebSearch => Some(json!({"type": "web_search"})),
        BuiltinKind::CodeExecution => {
            Some(json!({"type": "code_interpreter", "container": {"type": "auto"}}))
        }
        BuiltinKind::WebFetch | BuiltinKind::Other(_) => None,
    }
}

/// The choice that forces the hosted tool another vendor's provider tool
/// named `name` was mapped to (see [`hosted_tool`]): a Messages client's
/// `{"type":"tool","name":"web_search"}` forces its web-search server tool,
/// which reaches this API as the hosted `web_search`.
fn hosted_choice(request: &Request, name: &str) -> Option<Value> {
    request.tools.iter().find_map(|tool| match tool {
        Tool::Builtin(builtin)
            if builtin.origin != P && non_empty(&builtin.raw, "name") == Some(name) =>
        {
            let kind = hosted_tool(&builtin.kind)?.get("type")?.clone();
            Some(json!({"type": kind}))
        }
        _ => None,
    })
}

/// A forced tool the body does not offer (a provider tool of another
/// protocol this API has no equivalent for, say) is refused by the vendor.
/// The request then says `none`: a restriction that cannot be honoured must
/// not turn into permission to call any tool. A forced provider tool that
/// *was* mapped to a hosted one forces that one ([`hosted_choice`]). A
/// Responses client's own choice is replayed.
fn encode_tool_choice(request: &Request, choice: &ToolChoice, names: &UpstreamNames) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => {
            // Built-in tools have no name: only functions and custom tools
            // can be found here.
            let declared = request
                .tools
                .iter()
                .find(|tool| tool.name() == Some(name.as_str()));
            let wire = names.wire(name);
            match declared {
                Some(Tool::Custom(_)) => json!({"type": "custom", "name": wire}),
                None if request.source != P => {
                    hosted_choice(request, name).unwrap_or_else(|| json!("none"))
                }
                _ => json!({"type": "function", "name": wire}),
            }
        }
    }
}

/// The `user` value for an end-user identifier.
///
/// The vendor limits the identifier (`user`, `safety_identifier`) to
/// [`MAX_USER_CHARS`] characters and answers a longer one with a 400. An
/// identifier written for another vendor is not bound by that (Anthropic's
/// `metadata.user_id` may be 256 characters long, and Claude Code's is about
/// 150), so a longer one is replaced by a prefix of it plus a hash of all of
/// it: still stable per end user, which is all the field is for. An OpenAI
/// client's own value is replayed as written.
fn end_user_id(user: &str, openai_source: bool) -> String {
    if openai_source || user.chars().count() <= MAX_USER_CHARS {
        return user.to_string();
    }
    let prefix: String = user.chars().take(MAX_USER_CHARS - 17).collect();
    format!("{prefix}_{}", fnv64_hex(user))
}

/// How the output format of a request reaches a Responses upstream.
#[derive(Debug, Default)]
struct OutputFormat {
    /// The `text.format` value, when there is one to send.
    field: Option<Value>,
    /// Text appended to `instructions`.
    instruction: Option<String>,
    /// Text of a system message put at the head of `input`.
    input_note: Option<String>,
}

/// Whether any message of the conversation says "JSON", in any case. The
/// leading instructions are deliberately not looked at: the vendor's rule
/// speaks of the *input messages*.
fn mentions_json(request: &Request) -> bool {
    fn says_json(parts: &[Part]) -> bool {
        parts.iter().any(|part| match part {
            Part::Text(text) => text.text.to_ascii_lowercase().contains("json"),
            Part::ToolResult(result) => says_json(&result.content),
            _ => false,
        })
    }
    request.messages.iter().any(|m| says_json(&m.parts))
}

/// A schema as the root of a `json_schema` format: this API takes an object
/// schema there and nothing else ("schema must be a JSON Schema of 'type:
/// \"object\"'"). A root that describes an object without saying so (Gemini's
/// dialect allows that) is given its type; `None` for every other root (an
/// array, a bare enum, a union).
fn object_rooted(schema: &Value) -> Option<Value> {
    let root = schema.as_object()?;
    let retyped = || {
        let mut typed = Map::new();
        typed.insert("type".into(), json!("object"));
        typed.extend(
            root.iter()
                .filter(|(key, _)| key.as_str() != "type")
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        Some(Value::Object(typed))
    };
    match root.get("type") {
        Some(Value::String(kind)) if kind == "object" => Some(schema.clone()),
        // "An object or null" (a nullable root): the object is what can be
        // asked for.
        Some(Value::Array(kinds)) if kinds.iter().any(|k| k.as_str() == Some("object")) => {
            retyped()
        }
        None | Some(Value::Null) if root.contains_key("properties") => retyped(),
        _ => None,
    }
}

/// Decides how `request.response_format` is expressed.
///
/// An OpenAI client's format (Responses or Chat Completions, whose rules are
/// the same) is written as it is. For a client of another vendor:
///
/// * a schema format without a schema can only mean "some JSON" and becomes
///   `json_object`;
/// * a schema whose root is not an object schema (Gemini's `responseSchema`
///   takes any root; the example in its guide is an array) cannot be a
///   `json_schema` format: the vendor answers 400, a request fault no
///   failover repairs. `json_object` would force an object, so the request
///   then carries no format at all and the schema is described in the
///   instructions instead, the way JSON output is obtained from an API
///   without a native field for it;
/// * `json_object` needs the word "JSON" somewhere in the input ("Response
///   input messages must contain the word 'json' in some form to use
///   'text.format' of type 'json_object'"), a rule other vendors' JSON
///   modes do not have. When no message of a request that was written for
///   another protocol says it (a Chat Completions client may have said it
///   in a system message, which travels as `instructions`), a system
///   message at the head of `input` does.
fn output_format(request: &Request) -> OutputFormat {
    let Some(format) = &request.response_format else {
        return OutputFormat::default();
    };
    let other_vendor = request.source.family() != Family::Openai;
    let json_object = || OutputFormat {
        field: Some(json!({"type": "json_object"})),
        instruction: None,
        input_note: (request.source != P && !mentions_json(request))
            .then(|| JSON_OBJECT_INSTRUCTION.to_string()),
    };
    match format {
        ResponseFormat::Text => OutputFormat {
            field: Some(json!({"type": "text"})),
            ..OutputFormat::default()
        },
        ResponseFormat::JsonObject => json_object(),
        ResponseFormat::JsonSchema { schema, .. } if other_vendor && schema.is_null() => {
            json_object()
        }
        ResponseFormat::JsonSchema {
            name,
            description,
            schema,
            strict,
        } => {
            let schema = if !other_vendor {
                schema.clone()
            } else {
                match object_rooted(schema) {
                    Some(rooted) => rooted,
                    None => {
                        let mut text = JSON_SCHEMA_INSTRUCTION.to_string();
                        if let Some(name) = name.as_deref().filter(|n| !n.trim().is_empty()) {
                            text.push_str(&format!("\nSchema name: {name}"));
                        }
                        if let Some(description) =
                            description.as_deref().filter(|d| !d.trim().is_empty())
                        {
                            text.push_str(&format!("\nSchema description: {description}"));
                        }
                        text.push_str(&format!("\nJSON Schema:\n{schema}"));
                        return OutputFormat {
                            instruction: Some(text),
                            ..OutputFormat::default()
                        };
                    }
                }
            };
            let mut out = Map::new();
            out.insert("type".into(), json!("json_schema"));
            // `name` is mandatory on this API.
            out.insert("name".into(), json!(name.as_deref().unwrap_or("response")));
            if let Some(description) = description {
                out.insert("description".into(), json!(description));
            }
            out.insert("schema".into(), schema);
            if let Some(strict) = strict {
                out.insert("strict".into(), json!(strict));
            }
            OutputFormat {
                field: Some(Value::Object(out)),
                ..OutputFormat::default()
            }
        }
    }
}

/// Encodes an IR request as a body for a Responses upstream.
///
/// * Leading system parts become `instructions`; [`Role::System`] messages
///   inside the conversation become `system` message items.
/// * Each IR message becomes one `message` item, split around the parts that
///   are items of their own: tool calls (`function_call` /
///   `custom_tool_call`), tool results (`function_call_output` /
///   `custom_tool_call_output`), reasoning and provider-specific blocks.
/// * Reasoning is replayed only when it carries a blob a Responses upstream
///   issued and is followed by output of the same turn; everything else is
///   dropped.
/// * Tool names that came through another protocol are made valid for this
///   API (see [`crate::names`]); a forced tool the body does not offer
///   becomes `tool_choice: "none"`, and a forced provider tool of another
///   vendor that was mapped to a hosted tool forces that tool.
/// * A model known not to reason ([`ModelThinking::Unsupported`]) gets no
///   reasoning items at all, like it gets no reasoning settings: the vendor
///   rejects both.
/// * `reasoning.summary` is written when the request asks for reasoning
///   text. A Chat Completions client asks by turning reasoning on (it has no
///   other way to), so its effort brings `summary: "auto"` along unless it
///   said otherwise. OpenAI only generates summaries for verified
///   organisations on some models. Nothing has to be configured for one
///   that is not: the gateway asks an upstream that refuses the summary
///   once more without it and then leaves the field out by itself for that
///   provider (or model).
/// * Output format: see [`output_format`] for what becomes of a JSON schema
///   and of JSON mode written for another vendor.
/// * When the model may reason and the request does not rely on stored state
///   (`store: true` or `previous_response_id`), the body asks for
///   `reasoning.encrypted_content` and sets `store: false`, so the reasoning
///   can be replayed statelessly on the next turn.
/// * `store` is otherwise written as the request gave it. A request from
///   another protocol that says nothing gets `store: false`, because only
///   this API stores responses by default.
/// * `max_output_tokens` is kept within what the vendor accepts: at least 16
///   and, when the model's limit is known, at most that.
/// * Function tool schemas that came through another protocol are repaired
///   where this API would reject them (see [`crate::schema`]).
/// * Features Responses has no field for (`top_k`, `seed`, penalties, stop
///   sequences, candidate count, cache markers, tool-result error flags) are
///   dropped.
pub(crate) fn encode_request(
    request: &Request,
    ctx: &UpstreamCtx<'_>,
) -> Result<Value, CodecError> {
    let same_protocol = request.source == P;
    let unsupported = matches!(ctx.thinking, ModelThinking::Unsupported);
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));

    let output = output_format(request);
    let mut instructions = request.system_text();
    if let Some(instruction) = &output.instruction {
        if !instructions.is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(instruction);
    }
    if !instructions.is_empty() {
        body.insert("instructions".into(), json!(instructions));
    }

    let names = UpstreamNames::for_request(request);
    let mut builder = InputBuilder {
        names: &names,
        items: Vec::new(),
        pending: None,
        held_reasoning: Vec::new(),
        custom_calls: request
            .messages
            .iter()
            .flat_map(Message::tool_calls)
            .filter(|call| call.kind == ToolCallKind::Custom)
            .map(|call| call.id.as_str())
            .collect(),
        openai_source: request.source.family() == Family::Openai,
        replay_reasoning: !unsupported,
        signed_reasoning: false,
    };
    for message in &request.messages {
        match message.role {
            Role::User => builder.user_message("user", &message.parts),
            Role::System => builder.user_message("system", &message.parts),
            Role::Assistant => builder.assistant_message(&message.parts),
        }
    }
    let signed_reasoning = builder.signed_reasoning;
    let mut input = builder.items;
    if let Some(note) = &output.input_note {
        input.insert(
            0,
            json!({"type": "message", "role": "system",
                   "content": [{"type": "input_text", "text": note}]}),
        );
    }
    body.insert("input".into(), Value::Array(input));

    // Tool settings without tools are rejected by strict upstreams.
    let tools = encode_tools(request, &names);
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        let raw_choice = request.extra.get("tool_choice").filter(|_| same_protocol);
        if let Some(choice) = raw_choice {
            body.insert("tool_choice".into(), choice.clone());
        } else if let Some(choice) = &request.tool_choice {
            body.insert(
                "tool_choice".into(),
                encode_tool_choice(request, choice, &names),
            );
        }
        if let Some(parallel) = request.parallel_tool_calls {
            body.insert("parallel_tool_calls".into(), json!(parallel));
        }
    }

    let config = request.reasoning.clone().unwrap_or_default();
    if !unsupported {
        let mut scratch = Value::Object(Map::new());
        if let Some(depth) = config.depth {
            write_reasoning(&mut scratch, Fitted::Use(depth), ctx);
        }
        // A Chat Completions client has no field that asks for reasoning
        // text: servers of that protocol send `reasoning_content` whenever
        // the model reasons. Turning reasoning on with `reasoning_effort` is
        // therefore read as wanting to see it (notes 12 §8.1, §8.3), or such
        // a client would never get any from this API, which returns
        // summaries only on request. (Effort `none` decodes to an explicit
        // "no summaries".)
        let summary = config.summary.or_else(|| {
            (request.source == Protocol::OpenaiChat
                && config.depth.is_some_and(|depth| depth != Depth::Off))
            .then_some(Summary::Auto)
        });
        if let (Some(summary), Some(root)) = (summary, scratch.as_object_mut()) {
            write_summary(root, summary);
        }
        if let Some(reasoning) = scratch.get("reasoning") {
            body.insert("reasoning".into(), reasoning.clone());
        }
    }

    let mut text = match request.extra.get("text") {
        Some(Value::Object(text)) if same_protocol => text.clone(),
        _ => Map::new(),
    };
    if request.source == Protocol::OpenaiChat {
        // Chat Completions spells it as a top-level `verbosity`.
        if let Some(verbosity) = request.extra.get("verbosity").filter(|v| v.is_string()) {
            text.insert("verbosity".into(), verbosity.clone());
        }
    }
    if let Some(format) = output.field {
        text.insert("format".into(), format);
    }
    if !text.is_empty() {
        body.insert("text".into(), Value::Object(text));
    }

    if let Some(limit) = request.max_output_tokens {
        // Clients of other protocols must always state a limit and routinely
        // ask for more than this model can produce, which the vendor answers
        // with a 400 rather than by capping.
        let ceiling = ctx
            .max_output_tokens
            .filter(|ceiling| *ceiling >= MIN_MAX_OUTPUT_TOKENS)
            .unwrap_or(u64::MAX);
        body.insert(
            "max_output_tokens".into(),
            json!(limit.clamp(MIN_MAX_OUTPUT_TOKENS, ceiling)),
        );
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".into(), json!(top_p));
    }

    // Stateless reasoning replay. Asking a model that cannot reason for
    // encrypted reasoning is an error upstream, so this needs evidence.
    let may_reason = match ctx.thinking {
        ModelThinking::Unsupported => false,
        ModelThinking::Supported(_) => true,
        ModelThinking::Unknown => {
            signed_reasoning
                || config.depth.is_some_and(|depth| depth != Depth::Off)
                || config.summary.is_some_and(|summary| summary.is_on())
        }
    };
    let stateful = request.store == Some(true) || request.previous_response_id.is_some();
    let mut include: Vec<Value> = match request.extra.get("include") {
        Some(Value::Array(entries)) if same_protocol => entries.clone(),
        _ => Vec::new(),
    };
    if unsupported {
        include.retain(|entry| entry.as_str() != Some("reasoning.encrypted_content"));
    }
    if may_reason && !stateful {
        if !include
            .iter()
            .any(|entry| entry.as_str() == Some("reasoning.encrypted_content"))
        {
            include.push(json!("reasoning.encrypted_content"));
        }
        body.insert("store".into(), json!(false));
    } else if let Some(store) = request.store {
        body.insert("store".into(), json!(store));
    } else if !same_protocol {
        // This API keeps every response at the vendor unless told not to.
        // No other protocol does (Chat Completions stores on request only,
        // Anthropic and Gemini not at all), so for their clients silence
        // means "do not store" and has to be spelled out here. A Responses
        // client's silence is left as it is: it asked for this API's default.
        body.insert("store".into(), json!(false));
    }
    if !include.is_empty() {
        body.insert("include".into(), Value::Array(include));
    }

    if let Some(previous) = &request.previous_response_id
        && request.source.family() == Family::Openai
    {
        body.insert("previous_response_id".into(), json!(previous));
    }
    if let Some(metadata) = request.metadata.as_ref().filter(|m| !m.is_empty()) {
        // The vendor only takes string values.
        let metadata: Map<String, Value> = metadata
            .iter()
            .map(|(key, value)| {
                let value = match value {
                    Value::String(_) => value.clone(),
                    other => Value::String(other.to_string()),
                };
                (key.clone(), value)
            })
            .collect();
        body.insert("metadata".into(), Value::Object(metadata));
    }

    // `safety_identifier` superseded `user`; a Responses client's choice of
    // field is respected, other clients get the long-standing one.
    let safety = request
        .extra
        .get("safety_identifier")
        .filter(|v| v.is_string() && request.source.family() == Family::Openai);
    if let Some(user) = &request.user
        && safety.and_then(Value::as_str) != Some(user.as_str())
    {
        body.insert(
            "user".into(),
            json!(end_user_id(user, request.source.family() == Family::Openai)),
        );
    }
    if let Some(safety) = safety {
        body.insert("safety_identifier".into(), safety.clone());
    }
    if let Some(key) = &request.prompt_cache_key {
        body.insert("prompt_cache_key".into(), json!(key));
    }
    if let Some(tier) = &request.service_tier
        && (request.source.family() == Family::Openai || SERVICE_TIERS.contains(&tier.as_str()))
    {
        body.insert("service_tier".into(), json!(tier));
    }
    body.insert("stream".into(), json!(request.stream));

    if same_protocol {
        for (key, value) in &request.extra {
            let handled = matches!(
                key.as_str(),
                "tool_choice" | "text" | "include" | "safety_identifier"
            );
            if !handled && !body.contains_key(key) {
                body.insert(key.clone(), value.clone());
            }
        }
    } else if request.source == Protocol::OpenaiChat {
        for key in ["top_logprobs", "prompt_cache_retention"] {
            if let Some(value) = request.extra.get(key) {
                body.insert(key.into(), value.clone());
            }
        }
    }
    Ok(Value::Object(body))
}

/// Body for `POST /v1/responses/input_tokens`: the generation body reduced to
/// the fields that endpoint documents (so no `stream`, sampling or storage
/// settings).
pub(crate) fn encode_count_request(request: &Request, ctx: &UpstreamCtx<'_>) -> Option<Value> {
    let Ok(Value::Object(full)) = encode_request(request, ctx) else {
        return None;
    };
    let body: Map<String, Value> = full
        .into_iter()
        .filter(|(key, _)| COUNT_KEYS.contains(&key.as_str()))
        .collect();
    Some(Value::Object(body))
}
