//! Chat Completions requests: decoding a client's request into the IR,
//! encoding an IR request for an upstream, and the raw-body helpers used for
//! same-protocol passthrough.

use crate::common::{
    PROTOCOL, Side, apply_message_cache_control, bool_of, cache_control_of, f64_of, i64_of,
    parts_from_content, reasoning_from_message, str_of, text_to_wire, tool_call_from_wire,
    tool_call_to_wire, user_part_to_wire, write_reasoning_fields,
};
use crate::reasoning;
use crate::schema::{normalize_parameters, normalize_schema};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use switchyard_core::codec::{MaxTokensField, RequestMeta, RequestPath, UpstreamCtx};
use switchyard_core::error::CodecError;
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, CustomTool, FunctionTool, Message, Part, Reasoning, RefusalPart,
    Request, ResponseFormat, Role, Tool, ToolChoice, ToolResult,
};
use switchyard_core::protocol::{Family, Protocol};
use switchyard_core::util::{new_call_id, u64_field};

/// Top-level request fields this codec reads into the IR (or deliberately
/// consumes). Everything else lands in [`Request::extra`].
const MODELLED_FIELDS: &[&str] = &[
    "model",
    "messages",
    "stream",
    "stream_options",
    "max_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "top_k",
    "n",
    "stop",
    "seed",
    "presence_penalty",
    "frequency_penalty",
    "response_format",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "functions",
    "function_call",
    "reasoning_effort",
    "reasoning",
    "thinking",
    "enable_thinking",
    "thinking_budget",
    "include_reasoning",
    "user",
    "metadata",
    "service_tier",
    "prompt_cache_key",
    "store",
    "web_search_options",
];

/// Chat fields the IR has no slot for that are safe to hand back to an
/// OpenAI-family upstream when the request also came from that family.
const PASSTHROUGH_EXTRAS: &[&str] = &[
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "modalities",
    "audio",
    "prediction",
    "verbosity",
    "safety_identifier",
];

/// `service_tier` values OpenAI accepts. Other vendors' tiers (Anthropic's
/// `standard_only`) would be rejected, so they are not forwarded.
const OPENAI_SERVICE_TIERS: &[&str] = &["auto", "default", "flex", "scale", "priority"];

/// Tool-message text used when a tool returned only media. Chat tool
/// messages are text-only, so the media travels in the next user message.
const TOOL_MEDIA_PLACEHOLDER: &str =
    "[The tool returned non-text content; it is attached to the next user message.]";

/// Lead-in of the user message that relays media returned by tools.
const TOOL_MEDIA_NOTICE: &str = "Content returned by the preceding tool call(s):";

// ---------------------------------------------------------------------------
// Inspect / patch
// ---------------------------------------------------------------------------

pub(crate) fn request_meta(
    body: &Value,
    path: &RequestPath<'_>,
) -> Result<RequestMeta, CodecError> {
    let obj = body
        .as_object()
        .ok_or_else(|| CodecError::invalid("request body must be a JSON object"))?;
    let from_path = || path.model.filter(|m| !m.is_empty()).map(str::to_string);
    let model = match obj.get("model") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        Some(Value::String(_)) | Some(Value::Null) | None => {
            from_path().ok_or_else(|| CodecError::invalid_param("model", "`model` is required"))?
        }
        Some(_) => {
            return Err(CodecError::invalid_param(
                "model",
                "`model` must be a string",
            ));
        }
    };
    Ok(RequestMeta {
        model,
        stream: stream_flag(obj, path),
    })
}

/// Chat streams only when `stream` is the JSON literal `true`.
fn stream_flag(obj: &Map<String, Value>, path: &RequestPath<'_>) -> bool {
    path.stream
        .unwrap_or_else(|| obj.get("stream") == Some(&Value::Bool(true)))
}

pub(crate) fn set_request_model(body: &mut Value, model: &str) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".into(), json!(model));
    }
}

/// See [`crate::ChatCodec`]'s `prepare_passthrough` for the contract.
pub(crate) fn prepare_passthrough(body: &mut Value, stream: bool, ctx: &UpstreamCtx<'_>) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if stream {
        if ctx.quirks.stream_usage {
            match obj.get_mut("stream_options") {
                Some(Value::Object(options)) => {
                    options.insert("include_usage".into(), Value::Bool(true));
                }
                _ => {
                    obj.insert("stream_options".into(), json!({"include_usage": true}));
                }
            }
        } else {
            obj.remove("stream_options");
        }
    }
    // An upstream configured for the legacy field does not understand the
    // new one and would silently ignore the client's limit; OpenAI's
    // reasoning models reject the legacy one outright.
    let (wanted, other) = match ctx.quirks.max_tokens_field {
        MaxTokensField::MaxTokens => ("max_tokens", "max_completion_tokens"),
        MaxTokensField::MaxCompletionTokens => ("max_completion_tokens", "max_tokens"),
    };
    if let Some(limit) = obj.remove(other)
        && !limit.is_null()
        && obj.get(wanted).is_none_or(Value::is_null)
    {
        obj.insert(wanted.into(), limit);
    }
}

// ---------------------------------------------------------------------------
// decode_request
// ---------------------------------------------------------------------------

pub(crate) fn decode_request(body: &Value, path: &RequestPath<'_>) -> Result<Request, CodecError> {
    let obj = body
        .as_object()
        .ok_or_else(|| CodecError::invalid("request body must be a JSON object"))?;
    let messages = match obj.get("messages") {
        Some(Value::Array(messages)) => messages,
        Some(_) => {
            return Err(CodecError::invalid_param(
                "messages",
                "`messages` must be an array",
            ));
        }
        None => {
            return Err(CodecError::invalid_param(
                "messages",
                "`messages` is required",
            ));
        }
    };
    let model = str_of(body, "model")
        .or(path.model)
        .unwrap_or_default()
        .to_string();

    let mut req = Request::new(model, PROTOCOL);
    req.stream = stream_flag(obj, path);
    decode_messages(messages, &mut req)?;

    req.max_output_tokens =
        u64_field(body, "max_completion_tokens").or_else(|| u64_field(body, "max_tokens"));
    req.temperature = f64_of(body, "temperature");
    req.top_p = f64_of(body, "top_p");
    req.top_k = u64_field(body, "top_k");
    req.candidate_count = u64_field(body, "n").map(|n| n.min(u32::MAX as u64) as u32);
    req.stop = match obj.get("stop") {
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    req.seed = i64_of(body, "seed");
    req.presence_penalty = f64_of(body, "presence_penalty");
    req.frequency_penalty = f64_of(body, "frequency_penalty");
    req.response_format = obj.get("response_format").and_then(decode_response_format);

    req.tools = decode_tools(obj);
    let (choice, allowed) = decode_tool_choice(obj);
    req.tool_choice = choice;
    if let Some(allowed) = allowed {
        // `allowed_tools` narrows the tool set without rewriting `tools`.
        // The IR has no such notion, so the list itself is narrowed.
        req.tools.retain(|t| match t.name() {
            Some(name) => allowed.iter().any(|a| a == name),
            None => true,
        });
        if !req.tools.iter().any(|t| t.name().is_some()) {
            req.tool_choice = Some(ToolChoice::None);
        }
    }
    req.parallel_tool_calls = bool_of(body, "parallel_tool_calls");
    if let Some(options) = obj.get("web_search_options").filter(|o| o.is_object()) {
        req.tools.push(Tool::Builtin(BuiltinTool {
            kind: BuiltinKind::WebSearch,
            origin: PROTOCOL,
            raw: json!({"web_search_options": options}),
        }));
    }

    let reasoning = reasoning::read_reasoning(body);
    if !reasoning.is_empty() {
        req.reasoning = Some(reasoning);
    }

    req.user = str_of(body, "user").map(str::to_string);
    req.metadata = obj.get("metadata").and_then(Value::as_object).cloned();
    req.service_tier = str_of(body, "service_tier").map(str::to_string);
    req.prompt_cache_key = str_of(body, "prompt_cache_key").map(str::to_string);
    req.store = bool_of(body, "store");

    for (key, value) in obj {
        if !MODELLED_FIELDS.contains(&key.as_str()) {
            req.extra.insert(key.clone(), value.clone());
        }
    }
    Ok(req)
}

fn decode_messages(messages: &[Value], req: &mut Request) -> Result<(), CodecError> {
    // Leading system/developer messages are instructions; later ones are
    // mid-conversation system turns.
    let mut seen_conversation = false;
    // Consecutive `tool` messages form one IR user message.
    let mut last_was_tool = false;
    // Ids of the calls in the latest assistant turn that have no result yet,
    // used to pair tool messages that omit `tool_call_id`.
    let mut pending_calls: Vec<String> = Vec::new();
    // Legacy `function_call`s have no id; results are paired by name.
    let mut legacy_calls: VecDeque<(String, String)> = VecDeque::new();

    for (i, m) in messages.iter().enumerate() {
        if !m.is_object() {
            return Err(CodecError::invalid_param(
                format!("messages[{i}]"),
                "each message must be an object",
            ));
        }
        let role = m
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
            .trim()
            .to_ascii_lowercase();
        let name = str_of(m, "name").map(str::to_string);
        match role.as_str() {
            "system" | "developer" => {
                let mut parts = parts_from_content(m.get("content"));
                apply_message_cache_control(m, &mut parts);
                if !seen_conversation {
                    req.system.extend(parts);
                } else if !parts.is_empty() {
                    req.messages.push(Message {
                        role: Role::System,
                        parts,
                        name,
                    });
                    last_was_tool = false;
                }
            }
            "assistant" | "model" => {
                seen_conversation = true;
                last_was_tool = false;
                pending_calls.clear();
                let parts = assistant_parts(m, &mut pending_calls, &mut legacy_calls);
                req.messages.push(Message {
                    role: Role::Assistant,
                    parts,
                    name,
                });
            }
            "tool" => {
                seen_conversation = true;
                let mut call_id = str_of(m, "tool_call_id").unwrap_or("").to_string();
                if call_id.is_empty() {
                    if !pending_calls.is_empty() {
                        call_id = pending_calls.remove(0);
                    }
                } else {
                    pending_calls.retain(|p| p != &call_id);
                }
                let result = tool_result(m, call_id, name);
                push_tool_result(req, &mut last_was_tool, result);
            }
            "function" => {
                seen_conversation = true;
                let fn_name = name.clone().unwrap_or_default();
                let call_id = match legacy_calls.iter().position(|(n, _)| *n == fn_name) {
                    Some(pos) => legacy_calls
                        .remove(pos)
                        .map(|(_, id)| id)
                        .unwrap_or_else(new_call_id),
                    None => new_call_id(),
                };
                let result = tool_result(m, call_id, name);
                push_tool_result(req, &mut last_was_tool, result);
            }
            // "user" and anything unrecognised: the content is something the
            // model should read, and user is the role that cannot break the
            // turn structure.
            _ => {
                seen_conversation = true;
                last_was_tool = false;
                let mut parts = parts_from_content(m.get("content"));
                apply_message_cache_control(m, &mut parts);
                req.messages.push(Message {
                    role: Role::User,
                    parts,
                    name,
                });
            }
        }
    }
    Ok(())
}

fn assistant_parts(
    m: &Value,
    pending_calls: &mut Vec<String>,
    legacy_calls: &mut VecDeque<(String, String)>,
) -> Vec<Part> {
    let mut parts: Vec<Part> = reasoning_from_message(m, Side::Client)
        .into_iter()
        .map(Part::Reasoning)
        .collect();
    parts.extend(parts_from_content(m.get("content")));
    if let Some(refusal) = str_of(m, "refusal") {
        parts.push(Part::Refusal(RefusalPart {
            text: refusal.to_string(),
        }));
    }
    if let Some(Value::Array(calls)) = m.get("tool_calls") {
        for tc in calls {
            if let Some(call) = tool_call_from_wire(tc, Side::Client) {
                pending_calls.push(call.id.clone());
                parts.push(Part::ToolCall(call));
            }
        }
    }
    if let Some(fc) = m.get("function_call").filter(|f| f.is_object())
        && let Some(name) = str_of(fc, "name")
    {
        let id = new_call_id();
        legacy_calls.push_back((name.to_string(), id.clone()));
        parts.push(Part::tool_call(
            id,
            name,
            crate::common::arguments_text(fc.get("arguments")),
        ));
    }
    apply_message_cache_control(m, &mut parts);
    parts
}

fn tool_result(m: &Value, call_id: String, name: Option<String>) -> ToolResult {
    let mut content = parts_from_content(m.get("content"));
    // Anthropic rejects cache markers inside a tool result, so a marker on
    // the message or on one of its parts moves to the result itself.
    let mut cache_control = cache_control_of(m);
    for part in &mut content {
        let slot = match part {
            Part::Text(t) => &mut t.cache_control,
            Part::Image(media) | Part::Audio(media) | Part::Document(media) => {
                &mut media.cache_control
            }
            _ => continue,
        };
        if let Some(cc) = slot.take()
            && cache_control.is_none()
        {
            cache_control = Some(cc);
        }
    }
    ToolResult {
        call_id,
        name,
        content,
        is_error: false,
        cache_control,
    }
}

fn push_tool_result(req: &mut Request, last_was_tool: &mut bool, result: ToolResult) {
    match req.messages.last_mut() {
        Some(last) if *last_was_tool && last.role == Role::User => {
            last.parts.push(Part::ToolResult(result));
        }
        _ => req
            .messages
            .push(Message::new(Role::User, vec![Part::ToolResult(result)])),
    }
    *last_was_tool = true;
}

fn decode_response_format(v: &Value) -> Option<ResponseFormat> {
    let kind = v.get("type")?.as_str()?.trim().to_ascii_lowercase();
    match kind.as_str() {
        "text" => Some(ResponseFormat::Text),
        "json_object" => Some(ResponseFormat::JsonObject),
        "json_schema" => {
            let spec = v.get("json_schema").filter(|s| s.is_object()).unwrap_or(v);
            match spec.get("schema") {
                Some(schema) if !schema.is_null() => Some(ResponseFormat::JsonSchema {
                    name: str_of(spec, "name").map(str::to_string),
                    description: str_of(spec, "description").map(str::to_string),
                    schema: schema.clone(),
                    strict: bool_of(spec, "strict"),
                }),
                // A schema format without a schema can only mean "some JSON".
                _ => Some(ResponseFormat::JsonObject),
            }
        }
        _ => None,
    }
}

fn function_tool(spec: &Value, outer: &Value) -> Option<Tool> {
    let name = str_of(spec, "name")?;
    let parameters = spec
        .get("parameters")
        .or_else(|| spec.get("parametersJsonSchema"))
        .cloned()
        .unwrap_or(Value::Null);
    Some(Tool::Function(FunctionTool {
        name: name.to_string(),
        description: str_of(spec, "description").map(str::to_string),
        parameters,
        strict: bool_of(spec, "strict").or_else(|| bool_of(outer, "strict")),
        cache_control: cache_control_of(outer).or_else(|| cache_control_of(spec)),
    }))
}

fn builtin_kind(kind: &str, tool: &Value) -> BuiltinKind {
    let has = |key: &str| tool.get(key).is_some();
    if kind.contains("web_search") || has("google_search") {
        BuiltinKind::WebSearch
    } else if kind.contains("web_fetch") || has("url_context") {
        BuiltinKind::WebFetch
    } else if kind.contains("code_interpreter")
        || kind.contains("code_execution")
        || has("code_execution")
    {
        BuiltinKind::CodeExecution
    } else {
        BuiltinKind::Other(kind.to_string())
    }
}

fn decode_tools(obj: &Map<String, Value>) -> Vec<Tool> {
    let mut tools = Vec::new();
    if let Some(Value::Array(items)) = obj.get("tools") {
        for item in items.iter().filter(|i| i.is_object()) {
            let function = item.get("function").filter(|f| f.is_object());
            let kind = match item.get("type").and_then(Value::as_str) {
                Some(kind) => kind,
                None if function.is_some() => "function",
                None => "",
            };
            match kind {
                // The flat `{type, name, parameters}` form is the Responses
                // spelling; clients mix the two up often enough to accept it.
                "function" => tools.extend(function_tool(function.unwrap_or(item), item)),
                "custom" => {
                    let spec = item.get("custom").filter(|c| c.is_object()).unwrap_or(item);
                    if let Some(name) = str_of(spec, "name") {
                        tools.push(Tool::Custom(CustomTool {
                            name: name.to_string(),
                            description: str_of(spec, "description").map(str::to_string),
                            format: spec.get("format").filter(|f| !f.is_null()).cloned(),
                        }));
                    }
                }
                other => tools.push(Tool::Builtin(BuiltinTool {
                    kind: builtin_kind(other, item),
                    origin: PROTOCOL,
                    raw: item.clone(),
                })),
            }
        }
    }
    // Deprecated `functions` predates `tools` and means the same thing.
    if let Some(Value::Array(items)) = obj.get("functions") {
        tools.extend(items.iter().filter_map(|f| function_tool(f, f)));
    }
    tools
}

/// Returns the tool choice and, for `allowed_tools`, the allowed names.
fn decode_tool_choice(obj: &Map<String, Value>) -> (Option<ToolChoice>, Option<Vec<String>>) {
    let simple = |kind: &str| match kind.trim().to_ascii_lowercase().as_str() {
        "none" => Some(ToolChoice::None),
        "auto" => Some(ToolChoice::Auto),
        "required" | "any" => Some(ToolChoice::Required),
        _ => None,
    };
    let named = |v: &Value| {
        ["function", "custom"]
            .iter()
            .find_map(|holder| v.get(holder).and_then(|h| str_of(h, "name")))
            .or_else(|| str_of(v, "name"))
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
    };
    let choice = obj
        .get("tool_choice")
        .filter(|v| !v.is_null())
        // Deprecated spelling: "none" | "auto" | {"name": …}.
        .or_else(|| obj.get("function_call").filter(|v| !v.is_null()));
    match choice {
        None => (None, None),
        Some(Value::String(s)) => (simple(s), None),
        Some(v @ Value::Object(_)) => {
            let kind = v
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            match kind.as_str() {
                "none" | "auto" | "required" | "any" => (simple(&kind), None),
                "allowed_tools" => {
                    let spec = v
                        .get("allowed_tools")
                        .filter(|s| s.is_object())
                        .unwrap_or(v);
                    let names = spec
                        .get("tools")
                        .and_then(Value::as_array)
                        .map(|tools| tools.iter().filter_map(named).collect::<Vec<_>>())
                        .unwrap_or_default();
                    let mode = spec.get("mode").and_then(Value::as_str).unwrap_or("auto");
                    let choice = match simple(mode) {
                        Some(ToolChoice::Required) => ToolChoice::Required,
                        _ => ToolChoice::Auto,
                    };
                    (Some(choice), Some(names))
                }
                _ => match named(v) {
                    Some(name) => (Some(ToolChoice::Tool { name }), None),
                    // A restriction that cannot be understood must not turn
                    // into permission to call anything.
                    None => (Some(ToolChoice::None), None),
                },
            }
        }
        Some(_) => (None, None),
    }
}

// ---------------------------------------------------------------------------
// encode_request
// ---------------------------------------------------------------------------

pub(crate) fn encode_request(
    request: &Request,
    ctx: &UpstreamCtx<'_>,
) -> Result<Value, CodecError> {
    let openai_source = request.source.family() == Family::Openai;
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));

    // Schemas a Chat client wrote are forwarded as they are; schemas written
    // for another protocol are normalised to what Chat upstreams accept.
    let foreign_schemas = request.source != PROTOCOL;

    let mut messages: Vec<Value> = Vec::new();
    messages.extend(system_message(&request.system, None));
    // Ids of the tool calls emitted so far that no tool message has answered
    // yet. Only these may be answered by a `tool` message.
    let mut awaiting: Vec<String> = Vec::new();
    for msg in &request.messages {
        match msg.role {
            Role::System => messages.extend(system_message(&msg.parts, msg.name.as_deref())),
            Role::User => encode_user(msg, openai_source, &mut awaiting, &mut messages),
            Role::Assistant => {
                if let Some(message) = encode_assistant(msg) {
                    awaiting.extend(call_ids(&message).into_iter().map(str::to_string));
                    messages.push(message);
                }
            }
        }
    }
    body.insert(
        "messages".into(),
        Value::Array(align_tool_messages(messages)),
    );

    if let Some(limit) = request.max_output_tokens {
        let field = match ctx.quirks.max_tokens_field {
            MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
            MaxTokensField::MaxTokens => "max_tokens",
        };
        body.insert(field.into(), json!(limit));
    }
    if let Some(v) = request.temperature {
        body.insert("temperature".into(), json!(v));
    }
    if let Some(v) = request.top_p {
        body.insert("top_p".into(), json!(v));
    }
    // `top_k` is not an OpenAI parameter. It is only replayed when the client
    // itself put it into a Chat request (for a server that understands it).
    if let Some(v) = request.top_k.filter(|_| request.source == PROTOCOL) {
        body.insert("top_k".into(), json!(v));
    }
    if let Some(n) = request.candidate_count {
        body.insert("n".into(), json!(n));
    }
    if !request.stop.is_empty() {
        body.insert("stop".into(), json!(request.stop));
    }
    if let Some(v) = request.seed {
        body.insert("seed".into(), json!(v));
    }
    if let Some(v) = request.presence_penalty {
        body.insert("presence_penalty".into(), json!(v));
    }
    if let Some(v) = request.frequency_penalty {
        body.insert("frequency_penalty".into(), json!(v));
    }
    if let Some(format) = &request.response_format {
        body.insert(
            "response_format".into(),
            encode_response_format(format, foreign_schemas),
        );
    }

    let (tools, web_search_options) = encode_tools(request, foreign_schemas);
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        // Both fields are rejected by OpenAI when no tools are declared.
        if let Some(choice) = &request.tool_choice {
            body.insert("tool_choice".into(), encode_tool_choice(choice, request));
        }
        if let Some(parallel) = request.parallel_tool_calls {
            body.insert("parallel_tool_calls".into(), json!(parallel));
        }
    }
    if let Some(depth) = request.reasoning.as_ref().and_then(|r| r.depth)
        && let Some(effort) = reasoning::effort_for(depth, ctx.thinking)
    {
        body.insert("reasoning_effort".into(), json!(effort));
    }
    if let Some(options) = web_search_options {
        body.insert("web_search_options".into(), options);
    }
    if let Some(user) = &request.user {
        body.insert("user".into(), json!(user));
    }
    // Other vendors' metadata means nothing to a Chat upstream. Chat only
    // allows `metadata` on stored completions ("The 'metadata' parameter is
    // only allowed when 'store' is enabled"), whereas Responses accepts it
    // unconditionally, so a Responses client's metadata is only forwarded
    // together with `store: true`. A Chat client's own body is replayed as
    // it was written.
    let metadata_allowed = match request.source {
        Protocol::OpenaiChat => true,
        Protocol::OpenaiResponses => request.store == Some(true),
        _ => false,
    };
    if metadata_allowed && let Some(metadata) = &request.metadata {
        body.insert("metadata".into(), Value::Object(metadata.clone()));
    }
    if let Some(tier) = &request.service_tier
        && (openai_source || OPENAI_SERVICE_TIERS.contains(&tier.as_str()))
    {
        body.insert("service_tier".into(), json!(tier));
    }
    if let Some(key) = &request.prompt_cache_key {
        body.insert("prompt_cache_key".into(), json!(key));
    }
    if let Some(store) = request.store {
        body.insert("store".into(), json!(store));
    }
    if openai_source {
        for key in PASSTHROUGH_EXTRAS {
            let Some(value) = request.extra.get(*key).filter(|v| !v.is_null()) else {
                continue;
            };
            // `top_logprobs` is its own switch in Responses but needs
            // `logprobs: true` in Chat.
            if *key == "top_logprobs" && !body.contains_key("logprobs") {
                body.insert("logprobs".into(), Value::Bool(true));
            }
            body.insert((*key).into(), value.clone());
        }
    }

    body.insert("stream".into(), json!(request.stream));
    if request.stream && ctx.quirks.stream_usage {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    Ok(Value::Object(body))
}

/// Builds a `system` message from the text parts of `parts`. Non-text parts
/// are dropped (Chat system messages are text-only), as are cache markers.
fn system_message(parts: &[Part], name: Option<&str>) -> Option<Value> {
    let texts: Vec<&str> = parts
        .iter()
        .filter_map(Part::as_text)
        .filter(|t| !t.is_empty())
        .collect();
    let content = match texts.as_slice() {
        [] => return None,
        [one] => json!(one),
        many => Value::Array(many.iter().map(|t| text_to_wire(t)).collect()),
    };
    let mut m = Map::new();
    m.insert("role".into(), json!("system"));
    m.insert("content".into(), content);
    if let Some(name) = name {
        m.insert("name".into(), json!(name));
    }
    Some(Value::Object(m))
}

/// A user message becomes: one `tool` message per tool result (in order),
/// then one user message per result that answers nothing (see below), then
/// a user message holding media the tools returned (Chat tool messages are
/// text-only) together with the message's own content.
///
/// A result is only a `tool` message when its call id is in `awaiting`,
/// i.e. an earlier assistant message made that call and nothing answered it
/// yet. Strict upstreams reject a `tool` message that answers no
/// `tool_calls` entry ("messages with role 'tool' must be a response to a
/// preceding message with 'tool_calls'"), so a result with an empty id, an id
/// that was never issued (trimmed history) or an id that was already answered
/// is shown to the model as user text instead of failing the whole request.
fn encode_user(
    msg: &Message,
    openai_source: bool,
    awaiting: &mut Vec<String>,
    out: &mut Vec<Value>,
) {
    let mut relayed: Vec<Value> = Vec::new();
    let mut orphans: Vec<Value> = Vec::new();
    let mut own: Vec<Value> = Vec::new();
    for part in &msg.parts {
        let Part::ToolResult(result) = part else {
            own.extend(user_part_to_wire(part, openai_source));
            continue;
        };
        let mut texts: Vec<String> = Vec::new();
        let mut media: Vec<Value> = Vec::new();
        for item in &result.content {
            match user_part_to_wire(item, openai_source) {
                Some(wire) if wire.get("type").and_then(Value::as_str) == Some("text") => {
                    if let Some(text) = str_of(&wire, "text") {
                        texts.push(text.to_string());
                    }
                }
                Some(wire) => media.push(wire),
                None => {}
            }
        }
        let text = if texts.is_empty() && !media.is_empty() {
            TOOL_MEDIA_PLACEHOLDER.to_string()
        } else {
            texts.join("\n\n")
        };
        let answers = (!result.call_id.is_empty())
            .then(|| awaiting.iter().position(|id| *id == result.call_id))
            .flatten();
        match answers {
            Some(position) => {
                awaiting.remove(position);
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": result.call_id,
                    "content": text
                }));
            }
            // An orphan: the output is still worth showing to the model.
            None if !text.trim().is_empty() => {
                orphans.push(json!({"role": "user", "content": text}));
            }
            None => {}
        }
        relayed.extend(media);
    }
    // After the tool messages, never between them: the answers to one
    // assistant turn have to stay together.
    out.extend(orphans);

    let content = if !relayed.is_empty() {
        let mut items = vec![text_to_wire(TOOL_MEDIA_NOTICE)];
        items.extend(relayed);
        items.extend(own);
        Value::Array(items)
    } else if own.is_empty() {
        // Nothing left to say, unless the message was empty to begin with
        // (which Chat can express and other parts of the turn may rely on).
        if !msg.parts.is_empty() {
            return;
        }
        json!("")
    } else if own.len() == 1 && own[0].get("type").and_then(Value::as_str) == Some("text") {
        own[0].get("text").cloned().unwrap_or_else(|| json!(""))
    } else {
        Value::Array(own)
    };
    let mut m = Map::new();
    m.insert("role".into(), json!("user"));
    m.insert("content".into(), content);
    if let Some(name) = &msg.name {
        m.insert("name".into(), json!(name));
    }
    out.push(Value::Object(m));
}

/// An assistant turn is always exactly one Chat message.
///
/// Reasoning: blobs issued by another vendor family are dropped together
/// with their text (it is that vendor's private state). Unsigned reasoning
/// and reasoning from the OpenAI family is replayed as `reasoning_content`,
/// which thinking-mode Chat servers (DeepSeek, Kimi) require next to tool
/// calls; blobs that came out of a Chat upstream go back in
/// `reasoning_details`, the slot they came from.
fn encode_assistant(msg: &Message) -> Option<Value> {
    let mut text = String::new();
    let mut refusal = String::new();
    let mut calls: Vec<Value> = Vec::new();
    let mut reasoning: Vec<&Reasoning> = Vec::new();
    for part in &msg.parts {
        match part {
            Part::Text(t) => text.push_str(&t.text),
            Part::Refusal(r) => refusal.push_str(&r.text),
            Part::ToolCall(call) => {
                let signature = call
                    .signature
                    .as_ref()
                    .filter(|s| s.valid_for(PROTOCOL) && s.origin == PROTOCOL)
                    .map(|s| s.data.clone());
                calls.push(tool_call_to_wire(call, signature));
            }
            Part::Reasoning(r) => {
                if r.signature.as_ref().is_none_or(|s| s.valid_for(PROTOCOL)) {
                    reasoning.push(r);
                }
            }
            // Generated media, tool results and foreign blocks have no place
            // in a Chat assistant message.
            Part::Image(_)
            | Part::Audio(_)
            | Part::Document(_)
            | Part::ToolResult(_)
            | Part::Opaque(_) => {}
        }
    }

    let mut m = Map::new();
    m.insert("role".into(), json!("assistant"));
    // An empty string rather than `null`: chat templates of several
    // compatible servers choke on a null content.
    m.insert("content".into(), json!(text));
    write_reasoning_fields(&mut m, reasoning.into_iter(), "\n\n", |s| {
        (s.origin == PROTOCOL).then(|| s.data.clone())
    });
    if !refusal.is_empty() {
        m.insert("refusal".into(), json!(refusal));
    }
    if !calls.is_empty() {
        m.insert("tool_calls".into(), Value::Array(calls));
    }
    // Only `role` and `content` so far means every part was dropped.
    let has_payload = !text.is_empty() || m.len() > 2;
    if let Some(name) = &msg.name {
        m.insert("name".into(), json!(name));
    }
    (has_payload || msg.parts.is_empty()).then_some(Value::Object(m))
}

fn encode_response_format(format: &ResponseFormat, foreign_schema: bool) -> Value {
    match format {
        ResponseFormat::Text => json!({"type": "text"}),
        ResponseFormat::JsonObject => json!({"type": "json_object"}),
        ResponseFormat::JsonSchema {
            name,
            description,
            schema,
            strict,
        } => {
            let mut spec = Map::new();
            // `name` is mandatory in Chat; other protocols have no such field.
            spec.insert("name".into(), json!(name.as_deref().unwrap_or("response")));
            if let Some(description) = description {
                spec.insert("description".into(), json!(description));
            }
            spec.insert(
                "schema".into(),
                if foreign_schema {
                    normalize_schema(schema)
                } else {
                    schema.clone()
                },
            );
            if let Some(strict) = strict {
                spec.insert("strict".into(), json!(strict));
            }
            json!({"type": "json_schema", "json_schema": Value::Object(spec)})
        }
    }
}

/// Returns the `tools` array and, separately, `web_search_options` (Chat's
/// only provider-executed tool is a top-level option, not a tool entry).
///
/// Built-in tools: declarations that came from a Chat client are replayed
/// verbatim. Everything else is dropped, with one exception: in Chat, web
/// search is not a tool the model may call but a property of dedicated
/// search models (`gpt-4o-search-preview`, …), and `web_search_options` is a
/// hard error on every other model. A Responses `web_search` tool therefore
/// becomes `web_search_options` only when the target is such a model (its
/// id contains `search`); a Responses client that always declares the tool
/// (Codex does) must not fail on an ordinary Chat model because of it.
///
/// Function parameter schemas that were written for another protocol
/// (`foreign_schemas`) are normalised, see [`crate::schema`]: one
/// argument-less MCP tool declared as `{"type":"object"}` would otherwise
/// fail the whole request with `invalid_function_parameters`.
fn encode_tools(request: &Request, foreign_schemas: bool) -> (Vec<Value>, Option<Value>) {
    let mut tools = Vec::new();
    let mut web_search = None;
    for tool in &request.tools {
        match tool {
            Tool::Function(f) => {
                let mut spec = Map::new();
                spec.insert("name".into(), json!(f.name));
                if let Some(description) = &f.description {
                    spec.insert("description".into(), json!(description));
                }
                spec.insert(
                    "parameters".into(),
                    if foreign_schemas {
                        normalize_parameters(&f.parameters)
                    } else {
                        f.parameters_or_empty()
                    },
                );
                if let Some(strict) = f.strict {
                    spec.insert("strict".into(), json!(strict));
                }
                tools.push(json!({"type": "function", "function": Value::Object(spec)}));
            }
            Tool::Custom(c) => {
                let mut spec = Map::new();
                spec.insert("name".into(), json!(c.name));
                if let Some(description) = &c.description {
                    spec.insert("description".into(), json!(description));
                }
                if let Some(format) = &c.format {
                    spec.insert("format".into(), format.clone());
                }
                tools.push(json!({"type": "custom", "custom": Value::Object(spec)}));
            }
            Tool::Builtin(b) => match (b.origin, &b.kind) {
                (Protocol::OpenaiChat, _) => match b.raw.get("web_search_options") {
                    Some(options) => web_search = Some(options.clone()),
                    None => tools.push(b.raw.clone()),
                },
                (Protocol::OpenaiResponses, BuiltinKind::WebSearch)
                    if is_search_model(&request.model) =>
                {
                    web_search = Some(web_search_options_from_responses(&b.raw));
                }
                _ => {}
            },
        }
    }
    (tools, web_search)
}

/// Whether a Chat model id names one of the dedicated web-search models,
/// the only ones that accept `web_search_options`.
fn is_search_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("search")
}

/// Responses `{"type":"web_search", search_context_size, user_location}` ->
/// Chat `web_search_options` (which nests the location under `approximate`).
fn web_search_options_from_responses(raw: &Value) -> Value {
    let mut options = Map::new();
    if let Some(size) = str_of(raw, "search_context_size") {
        options.insert("search_context_size".into(), json!(size));
    }
    if let Some(Value::Object(location)) = raw.get("user_location") {
        let mut approximate = location.clone();
        approximate.remove("type");
        if !approximate.is_empty() {
            options.insert(
                "user_location".into(),
                json!({"type": "approximate", "approximate": Value::Object(approximate)}),
            );
        }
    }
    Value::Object(options)
}

fn encode_tool_choice(choice: &ToolChoice, request: &Request) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => {
            let is_custom = request
                .tools
                .iter()
                .any(|t| matches!(t, Tool::Custom(c) if c.name == *name));
            if is_custom {
                json!({"type": "custom", "custom": {"name": name}})
            } else {
                json!({"type": "function", "function": {"name": name}})
            }
        }
    }
}

fn message_role(m: &Value) -> &str {
    m.get("role").and_then(Value::as_str).unwrap_or("")
}

fn call_ids(m: &Value) -> Vec<&str> {
    m.get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .map(|c| c.get("id").and_then(Value::as_str).unwrap_or(""))
                .collect()
        })
        .unwrap_or_default()
}

/// Chat requires the `tool` messages answering an assistant turn to follow
/// it directly, while other protocols allow other content in between. For
/// every assistant message whose calls are each answered exactly once by a
/// later tool message, those tool messages are moved up behind it (keeping
/// their relative order). Ambiguous or incomplete histories are left alone.
fn align_tool_messages(messages: Vec<Value>) -> Vec<Value> {
    let mut call_count: HashMap<&str, usize> = HashMap::new();
    let mut tool_positions: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        match message_role(m) {
            "assistant" => {
                for id in call_ids(m) {
                    *call_count.entry(id).or_default() += 1;
                }
            }
            "tool" => {
                let id = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                tool_positions.entry(id).or_default().push(i);
            }
            _ => {}
        }
    }

    let mut moved = vec![false; messages.len()];
    let mut followers: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        if message_role(m) != "assistant" {
            continue;
        }
        let ids = call_ids(m);
        if ids.is_empty() {
            continue;
        }
        let mut positions = Vec::with_capacity(ids.len());
        let eligible = ids.iter().all(|id| {
            if id.is_empty() || call_count.get(id) != Some(&1) {
                return false;
            }
            match tool_positions.get(id).map(Vec::as_slice) {
                Some([pos]) if *pos > i => {
                    positions.push(*pos);
                    true
                }
                _ => false,
            }
        });
        if !eligible {
            continue;
        }
        positions.sort_unstable();
        let adjacent = positions
            .iter()
            .enumerate()
            .all(|(k, pos)| *pos == i + 1 + k);
        if !adjacent {
            for pos in &positions {
                moved[*pos] = true;
            }
            followers.insert(i, positions);
        }
    }
    if followers.is_empty() {
        return messages;
    }

    let mut slots: Vec<Option<Value>> = messages.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(slots.len());
    for i in 0..slots.len() {
        if moved[i] {
            continue;
        }
        out.extend(slots[i].take());
        if let Some(positions) = followers.get(&i) {
            for pos in positions {
                out.extend(slots[*pos].take());
            }
        }
    }
    out
}
