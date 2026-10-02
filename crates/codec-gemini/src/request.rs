//! `generateContent` requests: decoding a client's request into the canonical
//! model and encoding a canonical request for a Gemini upstream.

use crate::parts::{
    SKIP_SIGNATURE, call_args_text, call_name, candidate_metadata, decode_file_data,
    decode_function_response, decode_inline_data, encode_function_response, encode_media,
    explicit_id, is_metadata_only, opaque, request_signature, split_redacted, upstream_signature,
};
use crate::raw::{empty_user_turn, request_meta, request_root};
use crate::reasoning::{read_reasoning, set_include_thoughts, write_reasoning};
use crate::schema::{from_gemini_schema, sanitize_schema};
use crate::util::{
    f64_value, is_synthetic_call_id, num_f64, num_i64, num_u64, pick, pick_in, pick_str,
    sanitize_function_name, stable_call_id,
};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, FunctionTool, Message, Part, Reasoning, Request, ResponseFormat,
    Role, TextPart, Tool, ToolCall, ToolCallKind, ToolChoice, ToolResult, normalize_turns,
};
use switchyard_core::reasoning::{Depth, Fitted, ModelThinking};
use switchyard_core::{CodecError, Family, Protocol, RequestPath, UpstreamCtx};

/// Gemini accepts at most this many stop sequences.
const MAX_STOP_SEQUENCES: usize = 5;

/// Result text of a function response synthesised for a tool call that the
/// client's history never answered. Gemini rejects a model turn whose calls
/// are not all answered by the next turn.
const INTERRUPTED_RESULT: &str = "call interrupted, no output";

// ===========================================================================
// Decoding (client request -> canonical)
// ===========================================================================

/// Function calls that have not been answered yet, in call order. Gemini
/// pairs a `functionResponse` with its call by name and position, so the ids
/// the canonical model needs are recovered here.
///
/// A turn may hold any number of calls, so every operation is constant time:
/// the calls sit in slots that are emptied when answered, and are found
/// through per-name and per-id queues of slot numbers.
#[derive(Default)]
struct CallTracker {
    /// `(name, id)` of the calls of the current round in call order; `None`
    /// once answered.
    slots: Vec<Option<(String, String)>>,
    by_name: HashMap<String, VecDeque<usize>>,
    by_id: HashMap<String, VecDeque<usize>>,
    /// Every slot before this one has been looked at by `claim_oldest`.
    oldest: usize,
    answered: bool,
}

impl CallTracker {
    fn push(&mut self, name: &str, id: &str) {
        // A new round of calls after responses were seen: whatever was left
        // unanswered before can no longer be answered.
        if self.answered {
            *self = CallTracker::default();
        }
        let slot = self.slots.len();
        self.slots.push(Some((name.to_string(), id.to_string())));
        self.by_name
            .entry(name.to_string())
            .or_default()
            .push_back(slot);
        self.by_id
            .entry(id.to_string())
            .or_default()
            .push_back(slot);
    }

    /// Takes the oldest call of `queue` that is still unanswered.
    fn take(
        slots: &mut [Option<(String, String)>],
        queue: Option<&mut VecDeque<usize>>,
    ) -> Option<(String, String)> {
        let queue = queue?;
        while let Some(slot) = queue.pop_front() {
            if let Some(call) = slots.get_mut(slot).and_then(Option::take) {
                return Some(call);
            }
        }
        None
    }

    fn claim_id(&mut self, id: &str) {
        self.answered = true;
        Self::take(&mut self.slots, self.by_id.get_mut(id));
    }

    fn claim_name(&mut self, name: &str) -> Option<String> {
        self.answered = true;
        Self::take(&mut self.slots, self.by_name.get_mut(name)).map(|(_, id)| id)
    }

    fn claim_oldest(&mut self) -> Option<(String, String)> {
        self.answered = true;
        while let Some(slot) = self.slots.get_mut(self.oldest) {
            self.oldest += 1;
            if let Some(call) = slot.take() {
                return Some(call);
            }
        }
        None
    }
}

fn items(value: Option<&Value>) -> &[Value] {
    match value {
        Some(Value::Array(list)) => list.as_slice(),
        Some(Value::Null) | None => &[],
        Some(single) => std::slice::from_ref(single),
    }
}

/// Decodes a `generateContent` / `streamGenerateContent` request, or the
/// body of a `countTokens` call in either of its forms (bare `contents`, or a
/// whole request wrapped in `generateContentRequest`).
pub(crate) fn decode_request(body: &Value, path: &RequestPath<'_>) -> Result<Request, CodecError> {
    let meta = request_meta(body, path)?;
    let body = request_root(body);
    let Some(root) = body.as_object() else {
        return Err(CodecError::invalid(
            "the request body must be a JSON object",
        ));
    };
    let mut request = Request::new(meta.model, Protocol::Gemini);
    request.stream = meta.stream;

    if let Some(system) = pick_in(root, &["systemInstruction", "system_instruction"]) {
        request.system = decode_system(system);
    }

    let contents = match root.get("contents") {
        Some(Value::Array(list)) => list.as_slice(),
        // Vertex samples send a single content object.
        Some(single @ (Value::Object(_) | Value::String(_))) => std::slice::from_ref(single),
        Some(_) => {
            return Err(CodecError::invalid_param(
                "contents",
                "`contents` must be an array of content objects",
            ));
        }
        None => {
            return Err(CodecError::invalid_param(
                "contents",
                "`contents` is required",
            ));
        }
    };
    let mut tracker = CallTracker::default();
    for (index, content) in contents.iter().enumerate() {
        decode_content(content, index, &mut tracker, &mut request);
    }

    for tool in items(root.get("tools")) {
        decode_tool(tool, &mut request.tools);
    }
    if let Some(config) = pick_in(root, &["toolConfig", "tool_config"]) {
        let decoded = decode_tool_config(config);
        request.tool_choice = decoded.choice;
        if decoded.lossy {
            request
                .extra
                .insert("toolConfig".to_string(), config.clone());
        }
        // A restriction to some of the declared functions. The IR has no
        // such notion and neither have the other protocols, so the list of
        // functions itself is narrowed: passing on the mode with every
        // function declared would let the model call exactly the ones the
        // client excluded. Provider-executed tools are not function calls
        // and are not affected.
        if let Some(allowed) = decoded.restricted_to {
            request.tools.retain(|tool| match tool.name() {
                Some(name) => allowed.iter().any(|allowed| allowed == name),
                None => true,
            });
            if !request.tools.iter().any(|tool| tool.name().is_some()) {
                request.tool_choice = Some(ToolChoice::None);
            }
        }
    }
    if let Some(Value::Object(config)) = pick_in(root, &["generationConfig", "generation_config"]) {
        decode_generation_config(config, &mut request);
    }
    let reasoning = read_reasoning(body);
    if !reasoning.is_empty() {
        request.reasoning = Some(reasoning);
    }

    for (key, value) in root {
        if value.is_null() {
            continue;
        }
        match key.as_str() {
            "contents" | "systemInstruction" | "system_instruction" | "tools" | "toolConfig"
            | "tool_config" | "generationConfig" | "generation_config" | "model" | "stream" => {}
            "safetySettings" | "safety_settings" => {
                request
                    .extra
                    .insert("safetySettings".to_string(), value.clone());
            }
            "cachedContent" | "cached_content" => {
                request
                    .extra
                    .insert("cachedContent".to_string(), value.clone());
            }
            "labels" => {
                if let Value::Object(labels) = value {
                    request.metadata = Some(labels.clone());
                }
            }
            "serviceTier" | "service_tier" => {
                request.service_tier = value.as_str().map(str::to_owned)
            }
            "store" => request.store = value.as_bool(),
            _ => {
                request.extra.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(request)
}

/// `systemInstruction`: a content object (its role is ignored), a bare part
/// list, or — from lenient clients — a plain string.
fn decode_system(system: &Value) -> Vec<Part> {
    let parts = match system {
        Value::String(text) => {
            return if text.is_empty() {
                Vec::new()
            } else {
                vec![Part::text(text.clone())]
            };
        }
        Value::Array(_) => items(Some(system)),
        Value::Object(map) if map.contains_key("parts") => items(map.get("parts")),
        // A single part object: `{"text": "…"}`.
        Value::Object(_) => std::slice::from_ref(system),
        _ => &[],
    };
    let mut out = Vec::new();
    for part in parts {
        match part {
            Value::String(text) if !text.is_empty() => out.push(Part::text(text.clone())),
            Value::Object(map) => {
                if map.get("thought").and_then(Value::as_bool).unwrap_or(false) {
                    continue;
                }
                if let Some(text) = map.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        out.push(Part::text(text));
                    }
                } else if let Some(media) =
                    pick_in(map, &["inlineData", "inline_data"]).and_then(decode_inline_data)
                {
                    out.push(media);
                } else if let Some(media) =
                    pick_in(map, &["fileData", "file_data"]).and_then(decode_file_data)
                {
                    out.push(media);
                }
            }
            _ => {}
        }
    }
    out
}

fn has_any_part(parts: &[Value], spellings: &[&str]) -> bool {
    parts.iter().any(|part| pick(part, spellings).is_some())
}

fn decode_content(content: &Value, index: usize, tracker: &mut CallTracker, request: &mut Request) {
    let parts: &[Value] = match content {
        // A bare string stands for a user turn.
        Value::String(_) => std::slice::from_ref(content),
        Value::Object(map) => items(map.get("parts")),
        _ => return,
    };
    let role = match pick_str(content, &["role"])
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "model" | "assistant" => Role::Assistant,
        "system" | "developer" => Role::System,
        "user" | "function" | "tool" => Role::User,
        // No (or an unknown) role: Gemini reads it as a user turn, unless it
        // can only be the model's.
        _ if has_any_part(parts, &["functionCall", "function_call"])
            && !has_any_part(parts, &["functionResponse", "function_response"]) =>
        {
            Role::Assistant
        }
        _ => Role::User,
    };

    // Consecutive model contents are one assistant turn. A client that
    // streamed the turn may have recorded one content per chunk
    // (`google-genai` chats do), which splits a thought from the part that
    // carries its signature and the reasoning from the call it led to; every
    // other protocol needs them in one message, and the encoders merge
    // same-role neighbours anyway.
    let mut main = match request.messages.last() {
        Some(last) if role == Role::Assistant && last.role == Role::Assistant => request
            .messages
            .pop()
            .map(|message| message.parts)
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    // Function responses found in a model turn belong to the user.
    let mut displaced = Vec::new();
    for (position, part) in parts.iter().enumerate() {
        decode_part(
            part,
            role,
            index,
            position,
            tracker,
            &mut main,
            &mut displaced,
        );
    }

    if role == Role::System {
        main.retain(|part| matches!(part, Part::Text(_)));
        if main.is_empty() {
            return;
        }
        // Instructions ahead of the conversation are the system prompt.
        if request.messages.is_empty() {
            request.system.extend(main);
        } else {
            request.messages.push(Message::new(Role::System, main));
        }
        return;
    }
    if !main.is_empty() {
        request.messages.push(Message::new(role, main));
    }
    if !displaced.is_empty() {
        request.messages.push(Message::new(Role::User, displaced));
    }
}

fn decode_part(
    part: &Value,
    role: Role,
    content_index: usize,
    part_index: usize,
    tracker: &mut CallTracker,
    out: &mut Vec<Part>,
    displaced: &mut Vec<Part>,
) {
    let map = match part {
        Value::String(text) => {
            if !text.is_empty() {
                out.push(Part::text(text.clone()));
            }
            return;
        }
        Value::Object(map) => map,
        _ => return,
    };
    let signature = request_signature(part);
    let thought = map.get("thought").and_then(Value::as_bool).unwrap_or(false);

    if let Some(call) = pick_in(map, &["functionCall", "function_call"]) {
        // Only the model calls functions.
        if role != Role::Assistant {
            return;
        }
        let name = call_name(call);
        let arguments = call_args_text(call);
        let id = explicit_id(call)
            .map(str::to_owned)
            .unwrap_or_else(|| stable_call_id("call", content_index, part_index, name, &arguments));
        tracker.push(name, &id);
        out.push(Part::ToolCall(ToolCall {
            id,
            name: name.to_string(),
            arguments,
            kind: ToolCallKind::Function,
            signature,
            cache_control: None,
        }));
        return;
    }

    if let Some(response) = pick_in(map, &["functionResponse", "function_response"]) {
        let mut name = call_name(response).to_string();
        let call_id = if let Some(id) = explicit_id(response) {
            tracker.claim_id(id);
            id.to_string()
        } else if name.is_empty() {
            match tracker.claim_oldest() {
                Some((pending_name, id)) => {
                    name = pending_name;
                    id
                }
                None => stable_call_id(
                    "response",
                    content_index,
                    part_index,
                    "",
                    &response.to_string(),
                ),
            }
        } else {
            tracker.claim_name(&name).unwrap_or_else(|| {
                stable_call_id(
                    "response",
                    content_index,
                    part_index,
                    &name,
                    &response.to_string(),
                )
            })
        };
        let (content, is_error) = decode_function_response(response);
        let result = Part::ToolResult(ToolResult {
            call_id,
            name: (!name.is_empty()).then_some(name),
            content,
            is_error,
            cache_control: None,
        });
        if role == Role::Assistant {
            displaced.push(result);
        } else {
            out.push(result);
        }
        return;
    }

    if let Some(text) = map.get("text").and_then(Value::as_str) {
        if thought {
            if role == Role::Assistant {
                push_thought(text, signature, out);
            }
        } else if text.is_empty() {
            // `{"text": "", "thoughtSignature": …}` carries reasoning state only.
            if let Some(signature) = signature {
                push_signature_carrier(role, signature, out);
            }
        } else {
            out.push(Part::Text(TextPart {
                text: text.to_string(),
                signature,
                ..TextPart::default()
            }));
        }
        return;
    }

    if let Some(inline) = pick_in(map, &["inlineData", "inline_data"]) {
        out.extend(decode_inline_data(inline));
        return;
    }
    if let Some(file) = pick_in(map, &["fileData", "file_data"]) {
        out.extend(decode_file_data(file));
        return;
    }
    if is_metadata_only(map) {
        if let Some(signature) = signature {
            push_signature_carrier(role, signature, out);
        }
        return;
    }
    out.push(opaque(part));
}

/// A thought part. Consecutive thought parts are one reasoning block, up to
/// and including the part that carries the signature: that is how a streamed
/// thought comes back from a client (text chunk by chunk, then the signature
/// in a part of its own), and the vendor that issued the signature only
/// accepts it together with the whole text it signed.
fn push_thought(text: &str, signature: Option<switchyard_core::Signature>, out: &mut Vec<Part>) {
    let (signature, redacted) = match signature.map(split_redacted) {
        Some((signature, redacted)) => (Some(signature), redacted),
        None => (None, false),
    };
    if redacted {
        // Withheld reasoning is nothing but its payload and never part of
        // the block before it.
        out.push(Part::Reasoning(Reasoning {
            id: None,
            text: String::new(),
            signature,
            redacted: true,
        }));
        return;
    }
    if let Some(Part::Reasoning(open)) = out.last_mut()
        && open.signature.is_none()
        && !open.redacted
    {
        open.text.push_str(text);
        open.signature = signature;
        return;
    }
    if text.is_empty() && signature.is_none() {
        return;
    }
    out.push(Part::Reasoning(Reasoning {
        id: None,
        text: text.to_string(),
        signature,
        redacted: false,
    }));
}

/// A part that is nothing but a thought signature: it completes the
/// reasoning right before it, or stands alone as signed, text-less reasoning
/// (or as withheld reasoning, when the signature says so).
fn push_signature_carrier(role: Role, signature: switchyard_core::Signature, out: &mut Vec<Part>) {
    if role != Role::Assistant {
        return;
    }
    push_thought("", Some(signature), out);
}

fn builtin(kind: BuiltinKind, key: &str, value: &Value) -> Tool {
    let mut raw = Map::new();
    raw.insert(key.to_string(), value.clone());
    Tool::Builtin(BuiltinTool {
        kind,
        origin: Protocol::Gemini,
        raw: Value::Object(raw),
    })
}

fn decode_tool(tool: &Value, out: &mut Vec<Tool>) {
    let Some(map) = tool.as_object() else {
        return;
    };
    for (key, value) in map {
        if value.is_null() {
            continue;
        }
        match key.as_str() {
            "functionDeclarations" | "function_declarations" => {
                for declaration in items(Some(value)) {
                    let Some(name) = pick_str(declaration, &["name"]).filter(|n| !n.is_empty())
                    else {
                        continue;
                    };
                    // `parametersJsonSchema` is standard JSON Schema already;
                    // `parameters` is Gemini's dialect.
                    let parameters = match pick(
                        declaration,
                        &["parametersJsonSchema", "parameters_json_schema"],
                    ) {
                        Some(schema) => schema.clone(),
                        None => pick(declaration, &["parameters"])
                            .map(from_gemini_schema)
                            .unwrap_or(Value::Null),
                    };
                    out.push(Tool::Function(FunctionTool {
                        name: name.to_string(),
                        description: pick_str(declaration, &["description"]).map(str::to_owned),
                        parameters,
                        strict: None,
                        cache_control: None,
                    }));
                }
            }
            "googleSearch"
            | "google_search"
            | "googleSearchRetrieval"
            | "google_search_retrieval" => {
                out.push(builtin(BuiltinKind::WebSearch, key, value));
            }
            "codeExecution" | "code_execution" => {
                out.push(builtin(BuiltinKind::CodeExecution, key, value))
            }
            "urlContext" | "url_context" => out.push(builtin(BuiltinKind::WebFetch, key, value)),
            other => out.push(builtin(BuiltinKind::Other(other.to_string()), key, value)),
        }
    }
}

/// What a `toolConfig` says about calling tools.
struct ToolConfig {
    choice: Option<ToolChoice>,
    /// The canonical choice lost information (`VALIDATED`, several allowed
    /// names, retrieval settings): the raw object is kept for Gemini
    /// upstreams.
    lossy: bool,
    /// `allowedFunctionNames` that the choice does not express by itself:
    /// the only functions the model may call.
    restricted_to: Option<Vec<String>>,
}

/// `toolConfig` -> tool choice.
///
/// `allowedFunctionNames` limits the functions the model may call in the
/// modes that have it (`ANY`, `VALIDATED`). A single name under `ANY` is a
/// forced tool; any other list is returned as a restriction for the caller
/// to apply to the tool list.
fn decode_tool_config(config: &Value) -> ToolConfig {
    let calling = pick(
        config,
        &["functionCallingConfig", "function_calling_config"],
    );
    let mode = calling
        .and_then(|c| pick_str(c, &["mode"]))
        .map(|m| m.trim().to_ascii_uppercase())
        .unwrap_or_default();
    let allowed: Vec<&str> = calling
        .and_then(|c| pick(c, &["allowedFunctionNames", "allowed_function_names"]))
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let other_settings = config.as_object().is_some_and(|map| {
        map.keys()
            .any(|k| k != "functionCallingConfig" && k != "function_calling_config")
    });
    let restriction =
        || (!allowed.is_empty()).then(|| allowed.iter().map(|name| name.to_string()).collect());
    let (choice, lossy, restricted_to) = match mode.as_str() {
        "AUTO" => (Some(ToolChoice::Auto), false, None),
        "NONE" => (Some(ToolChoice::None), false, None),
        "ANY" if allowed.len() == 1 => (
            Some(ToolChoice::Tool {
                name: allowed[0].to_string(),
            }),
            false,
            None,
        ),
        "ANY" => (
            Some(ToolChoice::Required),
            !allowed.is_empty(),
            restriction(),
        ),
        "VALIDATED" => (Some(ToolChoice::Auto), true, restriction()),
        _ => (None, false, None),
    };
    ToolConfig {
        choice,
        lossy: lossy || other_settings,
        restricted_to,
    }
}

fn decode_generation_config(config: &Map<String, Value>, request: &mut Request) {
    let mut leftovers = Map::new();
    let mut mime: Option<&str> = None;
    let mut dialect_schema: Option<&Value> = None;
    let mut json_schema: Option<&Value> = None;
    for (key, value) in config {
        if value.is_null() {
            continue;
        }
        match key.as_str() {
            "temperature" => request.temperature = num_f64(value),
            "topP" | "top_p" => request.top_p = num_f64(value),
            "topK" | "top_k" => request.top_k = num_u64(value),
            "maxOutputTokens" | "max_output_tokens" => request.max_output_tokens = num_u64(value),
            "stopSequences" | "stop_sequences" => {
                request.stop = items(Some(value))
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
            }
            "candidateCount" | "candidate_count" => {
                request.candidate_count = num_u64(value).and_then(|n| u32::try_from(n).ok());
            }
            "seed" => request.seed = num_i64(value),
            "presencePenalty" | "presence_penalty" => request.presence_penalty = num_f64(value),
            "frequencyPenalty" | "frequency_penalty" => request.frequency_penalty = num_f64(value),
            "responseMimeType" | "response_mime_type" => mime = value.as_str(),
            "responseSchema" | "response_schema" => dialect_schema = Some(value),
            "responseJsonSchema" | "response_json_schema" | "_responseJsonSchema" => {
                json_schema = Some(value)
            }
            // Read separately, by `read_reasoning`.
            "thinkingConfig" | "thinking_config" => {}
            _ => {
                leftovers.insert(key.clone(), value.clone());
            }
        }
    }
    let wants_json = mime.is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"));
    request.response_format = if let Some(schema) = json_schema {
        Some(json_schema_format(schema.clone()))
    } else if let Some(schema) = dialect_schema {
        Some(json_schema_format(from_gemini_schema(schema)))
    } else if wants_json {
        Some(ResponseFormat::JsonObject)
    } else {
        None
    };
    if let Some(mime) = mime
        && !wants_json
        && !mime.trim().eq_ignore_ascii_case("text/plain")
    {
        // `text/x.enum` and friends have no canonical equivalent. The MIME
        // type and the schema that goes with it are kept as the client wrote
        // them, so a Gemini upstream gets the same output mode back (see
        // `encode_generation_config`); the canonical format above is what
        // the other protocols can make of it.
        leftovers.insert(
            "responseMimeType".to_string(),
            Value::String(mime.to_string()),
        );
        if let Some(schema) = json_schema {
            leftovers.insert("responseJsonSchema".to_string(), schema.clone());
        } else if let Some(schema) = dialect_schema {
            leftovers.insert("responseSchema".to_string(), schema.clone());
        }
    }
    if !leftovers.is_empty() {
        request
            .extra
            .insert("generationConfig".to_string(), Value::Object(leftovers));
    }
}

fn json_schema_format(schema: Value) -> ResponseFormat {
    ResponseFormat::JsonSchema {
        name: None,
        description: None,
        schema,
        strict: None,
    }
}

// ===========================================================================
// Encoding (canonical -> upstream request)
// ===========================================================================

/// One part (or, for a tool result, one group of parts) of an outgoing
/// content, with what the post-processing steps need to know about it.
enum Piece {
    Text(Value),
    Call {
        value: Map<String, Value>,
        id: String,
        name: String,
        signed: bool,
    },
    Result {
        values: Vec<Value>,
        call_id: String,
        text: String,
        /// The tool result named its function itself (a Gemini client's
        /// `functionResponse.name`), as opposed to a name looked up by id.
        named: bool,
    },
    Other(Value),
}

struct Turn {
    role: &'static str,
    pieces: Vec<Piece>,
}

/// Encodes a canonical request as a `generateContent` body. The model and the
/// stream flag are not part of the body: they belong in the URL.
pub(crate) fn encode_request(
    request: &Request,
    ctx: &UpstreamCtx<'_>,
) -> Result<Value, CodecError> {
    let native = request.source.family() == Family::Google;
    let mut body = Map::new();
    body.insert(
        "contents".to_string(),
        Value::Array(encode_contents(request, native, false)),
    );
    if let Some(system) = encode_system(&request.system) {
        body.insert("systemInstruction".to_string(), system);
    }
    let (tools, declared) = encode_tools(request);
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(config) = encode_tool_config(request, &declared, native) {
        body.insert("toolConfig".to_string(), config);
    }
    let generation = encode_generation_config(request, ctx, native);
    if !generation.is_empty() {
        body.insert("generationConfig".to_string(), Value::Object(generation));
    }
    if native {
        // Fields only a Gemini client can have sent; meaningless (or rejected)
        // when they come from another vendor's request.
        for key in ["safetySettings", "cachedContent"] {
            if let Some(value) = request.extra.get(key) {
                body.insert(key.to_string(), value.clone());
            }
        }
        if let Some(labels) = request
            .metadata
            .as_ref()
            .filter(|labels| !labels.is_empty())
        {
            body.insert("labels".to_string(), Value::Object(labels.clone()));
        }
        if let Some(tier) = &request.service_tier {
            body.insert("serviceTier".to_string(), Value::String(tier.clone()));
        }
        if let Some(store) = request.store {
            body.insert("store".to_string(), Value::Bool(store));
        }
    }

    let mut body = Value::Object(body);
    if let Some(reasoning) = &request.reasoning
        && !matches!(ctx.thinking, ModelThinking::Unsupported)
    {
        if let Some(depth) = reasoning.depth {
            write_reasoning(&mut body, Fitted::Use(depth), ctx);
        }
        // "No summaries" is Gemini's default. It is only spelled out next to a
        // depth: a lone `includeThoughts: false` would create a thinking
        // config on models that may not accept one.
        match reasoning.summary {
            Some(summary) => {
                if summary.is_on() || reasoning.depth.is_some() {
                    set_include_thoughts(&mut body, summary.is_on());
                }
            }
            // A Chat Completions client has no field that asks for the
            // reasoning text: servers of that protocol send
            // `reasoning_content` whenever the model reasons. Turning
            // reasoning on with `reasoning_effort` is therefore taken as
            // wanting to see it, or such a client would never get any from
            // Gemini, whose default is to keep its thoughts to itself.
            // (Effort `none` decodes to an explicit "no summaries", handled
            // above.)
            None => {
                if request.source == Protocol::OpenaiChat
                    && reasoning.depth.is_some_and(|depth| depth != Depth::Off)
                {
                    set_include_thoughts(&mut body, true);
                }
            }
        }
    }
    Ok(body)
}

/// Encodes the body of a `countTokens` call.
///
/// The plain `{"contents": […]}` form is used when there is nothing else to
/// count. System instructions and tools are only counted through the
/// `generateContentRequest` form, which must name the model. (Vertex AI takes
/// the fields unwrapped: see [`crate::adapt_for_vertex`].)
pub(crate) fn encode_count_request(request: &Request) -> Value {
    let native = request.source.family() == Family::Google;
    let contents = Value::Array(encode_contents(request, native, true));
    let system = encode_system(&request.system);
    let (tools, _) = encode_tools(request);
    if system.is_none() && tools.is_empty() {
        return json!({"contents": contents});
    }
    let model = if request.model.contains('/') {
        request.model.clone()
    } else {
        format!("models/{}", request.model)
    };
    let mut inner = Map::new();
    inner.insert("model".to_string(), Value::String(model));
    inner.insert("contents".to_string(), contents);
    if let Some(system) = system {
        inner.insert("systemInstruction".to_string(), system);
    }
    if !tools.is_empty() {
        inner.insert("tools".to_string(), Value::Array(tools));
    }
    json!({"generateContentRequest": inner})
}

/// `systemInstruction` is text only and has no role.
fn encode_system(system: &[Part]) -> Option<Value> {
    let parts: Vec<Value> = system
        .iter()
        .filter_map(Part::as_text)
        .filter(|text| !text.is_empty())
        .map(|text| json!({"text": text}))
        .collect();
    (!parts.is_empty()).then(|| json!({"parts": parts}))
}

/// Whether a call id is worth sending: only ids a Gemini client supplied
/// itself. Ids minted by this gateway, and ids issued by other vendors, mean
/// nothing to Gemini, which pairs calls and responses by position and name.
fn upstream_id(native: bool, id: &str) -> Option<&str> {
    (native && !id.is_empty() && !is_synthetic_call_id(id)).then_some(id)
}

fn text_piece(text: &TextPart) -> Option<Piece> {
    let signature = upstream_signature(text.signature.as_ref());
    if text.text.is_empty() && signature.is_none() {
        return None;
    }
    let mut part = Map::new();
    part.insert("text".to_string(), Value::String(text.text.clone()));
    if let Some(signature) = signature {
        part.insert(
            "thoughtSignature".to_string(),
            Value::String(signature.to_string()),
        );
    }
    Some(Piece::Text(Value::Object(part)))
}

fn native_opaque(part: &Part) -> Option<Piece> {
    match part {
        Part::Opaque(opaque)
            if opaque.origin.family() == Family::Google
                && opaque.raw.is_object()
                && candidate_metadata(&opaque.raw).is_none() =>
        {
            Some(Piece::Other(opaque.raw.clone()))
        }
        _ => None,
    }
}

fn model_pieces(message: &Message, native: bool) -> Vec<Piece> {
    let mut pieces = Vec::new();
    for part in &message.parts {
        match part {
            Part::Text(text) => pieces.extend(text_piece(text)),
            Part::Reasoning(reasoning) => {
                // Another vendor's reasoning is bound to its signature and is
                // not replayed to Gemini, with or without the blob.
                if reasoning
                    .signature
                    .as_ref()
                    .is_some_and(|s| !s.valid_for(Protocol::Gemini))
                {
                    continue;
                }
                let signature = upstream_signature(reasoning.signature.as_ref());
                if reasoning.text.is_empty() && signature.is_none() {
                    continue;
                }
                let mut thought = Map::new();
                thought.insert("text".to_string(), Value::String(reasoning.text.clone()));
                // Signed, text-less reasoning is Gemini's signature carrier
                // part, which is not flagged as a thought.
                if !reasoning.text.is_empty() {
                    thought.insert("thought".to_string(), Value::Bool(true));
                }
                if let Some(signature) = signature {
                    thought.insert(
                        "thoughtSignature".to_string(),
                        Value::String(signature.to_string()),
                    );
                }
                pieces.push(Piece::Other(Value::Object(thought)));
            }
            Part::ToolCall(call) => {
                let name = sanitize_function_name(&call.name);
                if name.is_empty() {
                    continue;
                }
                let args = match call.kind {
                    ToolCallKind::Custom => json!({"input": call.arguments}),
                    ToolCallKind::Function => call.arguments_value(),
                };
                let mut function_call = Map::new();
                function_call.insert("name".to_string(), Value::String(name.clone()));
                function_call.insert("args".to_string(), args);
                if let Some(id) = upstream_id(native, &call.id) {
                    function_call.insert("id".to_string(), Value::String(id.to_string()));
                }
                let mut value = Map::new();
                value.insert("functionCall".to_string(), Value::Object(function_call));
                let signature = upstream_signature(call.signature.as_ref());
                if let Some(signature) = signature {
                    value.insert(
                        "thoughtSignature".to_string(),
                        Value::String(signature.to_string()),
                    );
                }
                pieces.push(Piece::Call {
                    value,
                    id: call.id.clone(),
                    name,
                    signed: signature.is_some(),
                });
            }
            Part::Image(_) | Part::Audio(_) | Part::Document(_) => {
                pieces.extend(encode_media(part, native).map(Piece::Other));
            }
            Part::Refusal(refusal) => {
                if !refusal.text.is_empty() {
                    pieces.push(Piece::Text(json!({"text": refusal.text})));
                }
            }
            Part::Opaque(_) => pieces.extend(native_opaque(part)),
            Part::ToolResult(_) => {}
        }
    }
    pieces
}

/// The name of the first call with each id anywhere in the conversation: what
/// [`Request::tool_name_for_call`] answers, indexed once instead of searched
/// for every tool result.
fn call_names(request: &Request) -> HashMap<&str, &str> {
    let mut names = HashMap::new();
    for call in request.messages.iter().flat_map(Message::tool_calls) {
        names.entry(call.id.as_str()).or_insert(call.name.as_str());
    }
    names
}

fn user_pieces(message: &Message, call_names: &HashMap<&str, &str>, native: bool) -> Vec<Piece> {
    let mut pieces = Vec::new();
    for part in &message.parts {
        match part {
            Part::Text(text) => pieces.extend(text_piece(text)),
            Part::ToolResult(result) => {
                let own_name = result.name.as_deref().filter(|name| !name.is_empty());
                // The conversation-wide lookup is only a first guess: call ids
                // may repeat from turn to turn, so `pair_results` replaces it
                // with the name of the call this result is paired with.
                let name = own_name
                    .or_else(|| call_names.get(result.call_id.as_str()).copied())
                    .unwrap_or("unknown_function");
                let id = upstream_id(native, &result.call_id);
                pieces.push(Piece::Result {
                    values: encode_function_response(name, result, id, native),
                    call_id: result.call_id.clone(),
                    text: result.text(),
                    named: own_name.is_some(),
                });
            }
            Part::Image(_) | Part::Audio(_) | Part::Document(_) => {
                pieces.extend(encode_media(part, native).map(Piece::Other));
            }
            Part::Opaque(_) => pieces.extend(native_opaque(part)),
            Part::ToolCall(_) | Part::Reasoning(_) | Part::Refusal(_) => {}
        }
    }
    pieces
}

/// A system message in the middle of the conversation has no Gemini
/// equivalent; it is delivered as a marked user turn.
fn system_pieces(message: &Message) -> Vec<Piece> {
    let text = message
        .parts
        .iter()
        .filter_map(Part::as_text)
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return Vec::new();
    }
    vec![Piece::Text(json!({
        "text": format!("<system-reminder>\n{text}\n</system-reminder>")
    }))]
}

fn synthetic_result(name: &str, call_id: &str) -> Piece {
    Piece::Result {
        values: vec![json!({
            "functionResponse": {"name": name, "response": {"result": INTERRUPTED_RESULT}}
        })],
        call_id: call_id.to_string(),
        text: INTERRUPTED_RESULT.to_string(),
        named: true,
    }
}

/// Sets the `name` of the `functionResponse` part a result was encoded as.
fn rename_response(values: &mut [Value], name: &str) {
    if let Some(Value::Object(response)) = values
        .first_mut()
        .and_then(|part| part.get_mut("functionResponse"))
    {
        response.insert("name".to_string(), Value::String(name.to_string()));
    }
}

/// Makes the function responses of a user turn line up with the calls of the
/// model turn before it. Gemini pairs them by position and name and requires
/// one response per call.
///
/// A response matched to a call takes that call's name. Matching is scoped to
/// this pair of turns, which is what makes clients that reuse a call id from
/// one turn to the next (`call_1` every time, or an empty id) work: a
/// conversation-wide lookup by id would name the response after an earlier
/// turn's tool. Only a name a Gemini client wrote itself is left alone.
///
/// When the responses are exactly the answers to the calls they are put in
/// call order. Otherwise, for requests that did not come from a Gemini client
/// (`native` requests are sent as the client built them): responses that
/// answer no call become user text, and calls nobody answered get a
/// synthetic "interrupted" response.
fn pair_results(calls: &[(String, String)], user: &mut Turn, native: bool) {
    let result_slots: Vec<usize> = user
        .pieces
        .iter()
        .enumerate()
        .filter(|(_, piece)| matches!(piece, Piece::Result { .. }))
        .map(|(index, _)| index)
        .collect();
    if result_slots.is_empty() && calls.is_empty() {
        return;
    }

    // Match every call to the first unused response with its id.
    let mut used = vec![false; result_slots.len()];
    let matched: Vec<Option<usize>> = {
        let mut waiting: HashMap<&str, VecDeque<usize>> = HashMap::new();
        for (k, slot) in result_slots.iter().enumerate() {
            if let Piece::Result { call_id, .. } = &user.pieces[*slot] {
                waiting.entry(call_id.as_str()).or_default().push_back(k);
            }
        }
        calls
            .iter()
            .map(|(call_id, _)| {
                let found = waiting
                    .get_mut(call_id.as_str())
                    .and_then(VecDeque::pop_front);
                if let Some(k) = found {
                    used[k] = true;
                }
                found
            })
            .collect()
    };
    for ((_, call_name), found) in calls.iter().zip(&matched) {
        if let Some(k) = found
            && let Piece::Result { values, named, .. } = &mut user.pieces[result_slots[*k]]
            && !(native && *named)
        {
            rename_response(values, call_name);
        }
    }
    let exact = matched.iter().all(Option::is_some) && used.iter().all(|u| *u);
    if !exact && native {
        return;
    }

    let mut pieces: Vec<Option<Piece>> = std::mem::take(&mut user.pieces)
        .into_iter()
        .map(Some)
        .collect();
    let mut ordered: Vec<Piece> = Vec::with_capacity(calls.len());
    for ((call_id, name), found) in calls.iter().zip(&matched) {
        let piece = found
            .and_then(|k| pieces[result_slots[k]].take())
            .unwrap_or_else(|| synthetic_result(name, call_id));
        ordered.push(piece);
    }
    // Responses go where the first response was; leftovers become text.
    let anchor = result_slots.first().copied().unwrap_or(0);
    let mut rebuilt = Vec::with_capacity(pieces.len() + ordered.len());
    let mut ordered = Some(ordered);
    for (index, piece) in pieces.into_iter().enumerate() {
        if index == anchor {
            rebuilt.extend(ordered.take().unwrap_or_default());
        }
        match piece {
            Some(Piece::Result { values, text, .. }) => {
                if !text.trim().is_empty() {
                    rebuilt.push(Piece::Text(json!({"text": text})));
                }
                // Media of an orphaned result is still worth showing.
                rebuilt.extend(values.into_iter().skip(1).map(Piece::Other));
            }
            Some(other) => rebuilt.push(other),
            None => {}
        }
    }
    rebuilt.extend(ordered.take().unwrap_or_default());
    user.pieces = rebuilt;
}

/// Vertex rejects a user turn in which text follows a function response, so
/// text parts move in front (relative order kept on both sides).
fn hoist_text(turn: &mut Turn) {
    let mut seen_result = false;
    let needs_reorder = turn.pieces.iter().any(|piece| match piece {
        Piece::Result { .. } => {
            seen_result = true;
            false
        }
        Piece::Text(_) => seen_result,
        _ => false,
    });
    if !needs_reorder {
        return;
    }
    let (texts, rest): (Vec<Piece>, Vec<Piece>) = std::mem::take(&mut turn.pieces)
        .into_iter()
        .partition(|piece| matches!(piece, Piece::Text(_)));
    turn.pieces = texts;
    turn.pieces.extend(rest);
}

/// The first `functionCall` of a model turn must carry a thought signature.
/// One that has no Gemini signature gets the documented bypass value; later
/// calls of the same turn stay unsigned, as in native parallel calls.
fn sign_first_call(turn: &mut Turn) {
    for piece in &mut turn.pieces {
        if let Piece::Call { value, signed, .. } = piece {
            if !*signed {
                value.insert(
                    "thoughtSignature".to_string(),
                    Value::String(SKIP_SIGNATURE.to_string()),
                );
                *signed = true;
            }
            return;
        }
    }
}

fn encode_contents(request: &Request, native: bool, for_count: bool) -> Vec<Value> {
    let mut turns: Vec<Turn> = Vec::new();
    let call_names = call_names(request);
    for message in normalize_turns(&request.messages) {
        let (role, pieces) = match message.role {
            Role::System => ("user", system_pieces(&message)),
            Role::User => ("user", user_pieces(&message, &call_names, native)),
            Role::Assistant => ("model", model_pieces(&message, native)),
        };
        if pieces.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some(last) if last.role == role => last.pieces.extend(pieces),
            _ => turns.push(Turn { role, pieces }),
        }
    }

    for index in 0..turns.len() {
        if turns[index].role != "user" {
            continue;
        }
        let calls: Vec<(String, String)> =
            match index.checked_sub(1).map(|previous| &turns[previous]) {
                Some(previous) if previous.role == "model" => previous
                    .pieces
                    .iter()
                    .filter_map(|piece| match piece {
                        Piece::Call { id, name, .. } => Some((id.clone(), name.clone())),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
        pair_results(&calls, &mut turns[index], native);
        hoist_text(&mut turns[index]);
    }
    turns.retain(|turn| !turn.pieces.is_empty());

    // A conversation must end with the user. A trailing model turn is either
    // a prefill or a tool call nobody answered; Gemini continues neither, so
    // it is dropped. A Gemini client's own trailing turn is left to Gemini.
    if !native && !for_count {
        while turns.len() > 1 && turns.last().is_some_and(|turn| turn.role == "model") {
            turns.pop();
        }
    }

    // `contents` must hold at least one turn and open with the user. A
    // request that is nothing but instructions (valid on both OpenAI
    // protocols: a lone system message, or `instructions` with an empty
    // `input`) would otherwise go out as `contents: []`, which Gemini
    // refuses.
    let mut contents: Vec<Value> = Vec::with_capacity(turns.len() + 1);
    if turns.first().is_none_or(|turn| turn.role == "model") {
        contents.push(empty_user_turn());
    }
    for mut turn in turns {
        if turn.role == "model" {
            sign_first_call(&mut turn);
        }
        let mut parts = Vec::with_capacity(turn.pieces.len());
        for piece in turn.pieces {
            match piece {
                Piece::Text(value) | Piece::Other(value) => parts.push(value),
                Piece::Call { value, .. } => parts.push(Value::Object(value)),
                Piece::Result { values, .. } => parts.extend(values),
            }
        }
        contents.push(json!({"role": turn.role, "parts": parts}));
    }
    contents
}

/// Parameters of the function declaration that stands in for a free-form
/// (custom) tool: its raw input travels as one string argument.
fn custom_tool_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"input": {"type": "string", "description": "The raw input for the tool."}},
        "required": ["input"]
    })
}

/// Builds `tools` and returns it with the (sanitised) declared function
/// names.
///
/// All functions go into one `functionDeclarations` tool. Built-in tools a
/// Gemini client declared are forwarded verbatim. Built-in tools of another
/// vendor are mapped to Gemini's equivalent (`googleSearch`, `urlContext`,
/// `codeExecution`) only when the request declares no functions: most Gemini
/// models reject a request that combines built-in tools with function
/// calling, and the functions are what the client cannot do without.
fn encode_tools(request: &Request) -> (Vec<Value>, Vec<String>) {
    let mut declarations: Vec<Value> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut native_builtins: Vec<Value> = Vec::new();
    let mut mapped_builtins: Vec<Value> = Vec::new();
    for tool in &request.tools {
        let (name, description, schema) = match tool {
            Tool::Function(function) => {
                let mut schema = sanitize_schema(&function.parameters_or_empty());
                if let Value::Object(map) = &mut schema
                    && !map.contains_key("type")
                {
                    map.insert("type".to_string(), Value::String("object".to_string()));
                }
                (&function.name, function.description.as_deref(), schema)
            }
            Tool::Custom(custom) => (
                &custom.name,
                custom.description.as_deref(),
                custom_tool_schema(),
            ),
            Tool::Builtin(builtin) => {
                if builtin.origin.family() == Family::Google {
                    if builtin.raw.is_object() {
                        native_builtins.push(builtin.raw.clone());
                    }
                } else {
                    let mapped = match builtin.kind {
                        BuiltinKind::WebSearch => json!({"googleSearch": {}}),
                        BuiltinKind::WebFetch => json!({"urlContext": {}}),
                        BuiltinKind::CodeExecution => json!({"codeExecution": {}}),
                        BuiltinKind::Other(_) => continue,
                    };
                    if !mapped_builtins.contains(&mapped) {
                        mapped_builtins.push(mapped);
                    }
                }
                continue;
            }
        };
        let name = sanitize_function_name(name);
        // Two names that sanitise to the same string would be a duplicate
        // declaration, which Gemini rejects; the first one wins.
        if name.is_empty() || !seen.insert(name.clone()) {
            continue;
        }
        let mut declaration = Map::new();
        declaration.insert("name".to_string(), Value::String(name.clone()));
        if let Some(description) = description.filter(|d| !d.is_empty()) {
            declaration.insert(
                "description".to_string(),
                Value::String(description.to_string()),
            );
        }
        declaration.insert("parametersJsonSchema".to_string(), schema);
        declarations.push(Value::Object(declaration));
        names.push(name);
    }
    let mut tools = Vec::new();
    let has_functions = !declarations.is_empty();
    if has_functions {
        tools.push(json!({"functionDeclarations": declarations}));
    }
    tools.extend(native_builtins);
    if !has_functions {
        tools.extend(mapped_builtins);
    }
    (tools, names)
}

fn encode_tool_config(request: &Request, declared: &[String], native: bool) -> Option<Value> {
    if native
        && let Some(raw) = request
            .extra
            .get("toolConfig")
            .filter(|raw| raw.is_object())
    {
        return Some(raw.clone());
    }
    let choice = request.tool_choice.as_ref()?;
    // A calling mode without declarations is rejected.
    if declared.is_empty() {
        return None;
    }
    let config = match choice {
        ToolChoice::Auto => json!({"mode": "AUTO"}),
        ToolChoice::None => json!({"mode": "NONE"}),
        ToolChoice::Required => json!({"mode": "ANY"}),
        ToolChoice::Tool { name } => {
            let name = sanitize_function_name(name);
            if declared.contains(&name) {
                json!({"mode": "ANY", "allowedFunctionNames": [name]})
            } else {
                // Never let the model call a tool the client did not select.
                json!({"mode": "NONE"})
            }
        }
    };
    Some(json!({"functionCallingConfig": config}))
}

fn encode_generation_config(
    request: &Request,
    ctx: &UpstreamCtx<'_>,
    native: bool,
) -> Map<String, Value> {
    let mut config = Map::new();
    if let Some(temperature) = request.temperature {
        config.insert("temperature".to_string(), f64_value(temperature));
    }
    if let Some(top_p) = request.top_p {
        config.insert("topP".to_string(), f64_value(top_p));
    }
    if let Some(top_k) = request.top_k {
        config.insert("topK".to_string(), Value::from(top_k));
    }
    if let Some(requested) = request.max_output_tokens {
        // Gemini rejects a limit above what the model can produce.
        let limit = match ctx.max_output_tokens {
            Some(model_limit) if model_limit > 0 => requested.min(model_limit),
            _ => requested,
        };
        config.insert("maxOutputTokens".to_string(), Value::from(limit));
    }
    if !request.stop.is_empty() {
        let stops: Vec<&String> = request.stop.iter().take(MAX_STOP_SEQUENCES).collect();
        config.insert("stopSequences".to_string(), json!(stops));
    }
    if let Some(count) = request.candidate_count
        && native
    {
        config.insert("candidateCount".to_string(), Value::from(count));
    }
    // Gemini's seed is a 32-bit integer.
    if let Some(seed) = request.seed.and_then(|seed| i32::try_from(seed).ok()) {
        config.insert("seed".to_string(), Value::from(seed));
    }
    // Several Gemini models reject the penalty fields outright, and many
    // OpenAI clients send an explicit zero by default; zero is the default
    // here too, so it is simply not sent.
    for (key, penalty) in [
        ("presencePenalty", request.presence_penalty),
        ("frequencyPenalty", request.frequency_penalty),
    ] {
        if let Some(penalty) = penalty
            && (native || penalty != 0.0)
        {
            config.insert(key.to_string(), f64_value(penalty));
        }
    }
    // A Gemini client's own output mode that is not JSON (`text/x.enum`):
    // the decoder kept the MIME type and its schema verbatim, and they are
    // replayed below instead of the canonical format, which would turn the
    // request into JSON mode and the answer into a quoted string.
    let own_mode = native
        && request
            .extra
            .get("generationConfig")
            .and_then(|leftovers| leftovers.get("responseMimeType"))
            .is_some_and(Value::is_string);
    match &request.response_format {
        _ if own_mode => {}
        Some(ResponseFormat::JsonObject) => {
            config.insert(
                "responseMimeType".to_string(),
                Value::from("application/json"),
            );
        }
        Some(ResponseFormat::JsonSchema { schema, .. }) => {
            config.insert(
                "responseMimeType".to_string(),
                Value::from("application/json"),
            );
            // `responseJsonSchema` takes standard JSON Schema as is.
            if !schema.is_null() {
                config.insert("responseJsonSchema".to_string(), schema.clone());
            }
        }
        Some(ResponseFormat::Text) | None => {}
    }
    if native && let Some(Value::Object(leftovers)) = request.extra.get("generationConfig") {
        for (key, value) in leftovers {
            if !config.contains_key(key) {
                config.insert(key.clone(), value.clone());
            }
        }
    }
    config
}
