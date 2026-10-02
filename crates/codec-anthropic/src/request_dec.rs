//! Client side: Messages request body → canonical [`Request`].

use crate::blocks::{SigSource, cache_control, decode_assistant_block, decode_user_block};
use crate::reasoning::read_reasoning;
use crate::util::{THIS, TOOL_EXTRAS_KEY, f64_field, non_empty, str_field, u64_field};
use serde_json::{Map, Value};
use switchyard_core::ir::{
    BuiltinKind, BuiltinTool, FunctionTool, Message, Part, Request, ResponseFormat, Role, TextPart,
    Tool, ToolChoice,
};
use switchyard_core::{CodecError, RequestMeta, RequestPath};

/// Top-level keys that have a slot in the IR; everything else goes to
/// [`Request::extra`].
const KNOWN_KEYS: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "tool_choice",
    "thinking",
    "output_config",
    "output_format",
    "max_tokens",
    "stop_sequences",
    "temperature",
    "top_p",
    "top_k",
    "metadata",
    "service_tier",
    "stream",
];

/// Model name and stream flag of a Messages request. `stream` is true only
/// for the JSON literal `true`.
pub(crate) fn request_meta(
    body: &Value,
    path: &RequestPath<'_>,
) -> Result<RequestMeta, CodecError> {
    if !body.is_object() {
        return Err(CodecError::invalid("request body must be a JSON object"));
    }
    let model = non_empty(body, "model")
        .or(path.model.filter(|m| !m.is_empty()))
        .ok_or_else(|| CodecError::invalid_param("model", "`model` is required"))?;
    let stream = path
        .stream
        .unwrap_or_else(|| body.get("stream") == Some(&Value::Bool(true)));
    Ok(RequestMeta {
        model: model.to_string(),
        stream,
    })
}

fn decode_system(system: Option<&Value>) -> Vec<Part> {
    match system {
        Some(Value::String(text)) if !text.is_empty() => vec![Part::text(text.clone())],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| match block {
                Value::String(text) if !text.is_empty() => Some(Part::text(text.clone())),
                Value::Object(_) => {
                    let text = non_empty(block, "text")?;
                    Some(Part::Text(TextPart {
                        text: text.to_string(),
                        cache_control: cache_control(block),
                        citations: Vec::new(),
                        signature: None,
                    }))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn decode_role(role: Option<&str>) -> Role {
    match role.map(str::to_ascii_lowercase).as_deref() {
        Some("assistant" | "model") => Role::Assistant,
        Some("system" | "developer") => Role::System,
        _ => Role::User,
    }
}

fn decode_message(message: &Value) -> Option<Message> {
    if !message.is_object() {
        return None;
    }
    let role = decode_role(str_field(message, "role"));
    let decode_block = |block: &Value| match role {
        Role::Assistant => decode_assistant_block(block, SigSource::Client),
        Role::User => decode_user_block(block),
        // A mid-conversation system message is instructions: text only.
        Role::System => decode_user_block(block).filter(|part| matches!(part, Part::Text(_))),
    };
    let parts: Vec<Part> = match message.get("content") {
        Some(Value::String(text)) if !text.is_empty() => vec![Part::text(text.clone())],
        Some(Value::Array(blocks)) => blocks.iter().filter_map(decode_block).collect(),
        Some(single @ Value::Object(_)) => decode_block(single).into_iter().collect(),
        _ => Vec::new(),
    };
    // A turn without content says nothing; the API itself rejects it.
    if parts.is_empty() {
        return None;
    }
    Some(Message {
        role,
        parts,
        name: None,
    })
}

fn builtin_kind(tool_type: &str) -> BuiltinKind {
    if tool_type.starts_with("web_search") {
        BuiltinKind::WebSearch
    } else if tool_type.starts_with("web_fetch") {
        BuiltinKind::WebFetch
    } else if tool_type.starts_with("code_execution") {
        BuiltinKind::CodeExecution
    } else {
        BuiltinKind::Other(tool_type.to_string())
    }
}

/// Fields of a client tool definition that have a slot in the IR.
const TOOL_KEYS: &[&str] = &[
    "type",
    "name",
    "description",
    "input_schema",
    "strict",
    "cache_control",
];

/// The fields of a client tool the IR has no slot for (`defer_loading`,
/// `input_examples`, `eager_input_streaming`, …). They are remembered in
/// [`Request::extra`] so that re-encoding for an Anthropic upstream does not
/// lose them.
fn tool_extras(tool: &Value) -> Option<Map<String, Value>> {
    let extras: Map<String, Value> = tool
        .as_object()?
        .iter()
        .filter(|(key, value)| !TOOL_KEYS.contains(&key.as_str()) && !value.is_null())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    (!extras.is_empty()).then_some(extras)
}

fn decode_tool(tool: &Value) -> Option<Tool> {
    if !tool.is_object() {
        return None;
    }
    // Anthropic-defined tools (server tools, bash, text editor, computer use,
    // memory, MCP toolsets …) are told apart by a versioned `type`; a client
    // tool has no type or the literal "custom".
    if let Some(tool_type) = non_empty(tool, "type").filter(|t| *t != "custom") {
        return Some(Tool::Builtin(BuiltinTool {
            kind: builtin_kind(tool_type),
            origin: THIS,
            raw: tool.clone(),
        }));
    }
    let name = non_empty(tool, "name")?;
    Some(Tool::Function(FunctionTool {
        name: name.to_string(),
        description: str_field(tool, "description").map(str::to_string),
        parameters: tool.get("input_schema").cloned().unwrap_or(Value::Null),
        strict: tool.get("strict").and_then(Value::as_bool),
        cache_control: cache_control(tool),
    }))
}

/// Decodes `tool_choice`. Returns the choice and whether parallel tool use
/// was disabled.
fn decode_tool_choice(choice: Option<&Value>) -> (Option<ToolChoice>, bool) {
    let (kind, name, serial) = match choice {
        Some(Value::String(kind)) => (kind.as_str(), None, false),
        Some(object @ Value::Object(_)) => (
            str_field(object, "type").unwrap_or(""),
            non_empty(object, "name"),
            object.get("disable_parallel_tool_use") == Some(&Value::Bool(true)),
        ),
        _ => return (None, false),
    };
    let choice = match kind {
        "auto" => ToolChoice::Auto,
        // "required" is the OpenAI spelling; harmless to accept.
        "any" | "required" => ToolChoice::Required,
        "none" => ToolChoice::None,
        "tool" => match name {
            Some(name) => ToolChoice::Tool {
                name: name.to_string(),
            },
            // A named choice without a name fails closed.
            None => ToolChoice::None,
        },
        // An unknown restriction must not turn into permission.
        _ => ToolChoice::None,
    };
    (Some(choice), serial)
}

fn decode_response_format(body: &Value) -> Option<ResponseFormat> {
    let format = body
        .get("output_config")
        .and_then(|config| config.get("format"))
        // `output_format` is the deprecated top-level spelling.
        .or_else(|| body.get("output_format"))
        .filter(|format| format.is_object())?;
    match str_field(format, "type") {
        Some("json_schema") => Some(ResponseFormat::JsonSchema {
            name: non_empty(format, "name").map(str::to_string),
            description: None,
            schema: format.get("schema").cloned().unwrap_or(Value::Null),
            strict: None,
        }),
        Some("json_object") => Some(ResponseFormat::JsonObject),
        Some("text") => Some(ResponseFormat::Text),
        _ => None,
    }
}

/// Decodes a Messages request.
///
/// Errors only when the body is not an object or `messages` is missing or
/// not an array. `role: "system"` / `"developer"` messages at the head of
/// `messages` are appended to [`Request::system`]; later ones stay in place
/// as [`Role::System`] messages. Lost in the IR (and therefore in translation, never in
/// passthrough): `citations` settings and `context` of document blocks and
/// `thinking` fields other than type / budget / display. Tool fields without
/// an IR slot survive a round trip to an Anthropic upstream through
/// [`Request::extra`] and are lost for every other target.
pub(crate) fn decode_request(body: &Value, path: &RequestPath<'_>) -> Result<Request, CodecError> {
    let Some(object) = body.as_object() else {
        return Err(CodecError::invalid("request body must be a JSON object"));
    };
    let messages = match body.get("messages") {
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

    let model = non_empty(body, "model").or(path.model).unwrap_or("");
    let mut request = Request::new(model, THIS);
    request.stream = path
        .stream
        .unwrap_or_else(|| body.get("stream") == Some(&Value::Bool(true)));
    request.system = decode_system(body.get("system"));
    request.messages = messages.iter().filter_map(decode_message).collect();
    // `system` / `developer` messages ahead of the first user or assistant
    // message precede the conversation: by the IR contract they are leading
    // instructions and live in `Request::system`, after the top-level ones.
    // Only later ones are mid-conversation messages.
    let leading = request
        .messages
        .iter()
        .take_while(|message| message.role == Role::System)
        .count();
    for message in request.messages.drain(..leading) {
        request.system.extend(message.parts);
    }

    if let Some(Value::Array(tools)) = body.get("tools") {
        let mut extras = Map::new();
        for tool in tools {
            let Some(decoded) = decode_tool(tool) else {
                continue;
            };
            if let Tool::Function(function) = &decoded
                && let Some(fields) = tool_extras(tool)
            {
                extras.insert(function.name.clone(), Value::Object(fields));
            }
            request.tools.push(decoded);
        }
        if !extras.is_empty() {
            request
                .extra
                .insert(TOOL_EXTRAS_KEY.to_string(), Value::Object(extras));
        }
    }
    let (tool_choice, serial) = decode_tool_choice(body.get("tool_choice"));
    request.tool_choice = tool_choice;
    if serial {
        request.parallel_tool_calls = Some(false);
    }

    request.max_output_tokens = u64_field(body, "max_tokens");
    request.temperature = f64_field(body, "temperature");
    request.top_p = f64_field(body, "top_p");
    request.top_k = u64_field(body, "top_k");
    request.stop = match body.get("stop_sequences") {
        Some(Value::Array(stops)) => stops
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(stop)) => vec![stop.clone()],
        _ => Vec::new(),
    };

    let reasoning = read_reasoning(body);
    if !reasoning.is_empty() {
        request.reasoning = Some(reasoning);
    }
    request.response_format = decode_response_format(body);

    if let Some(Value::Object(metadata)) = body.get("metadata") {
        request.user = metadata
            .get("user_id")
            .and_then(Value::as_str)
            .filter(|user| !user.trim().is_empty())
            .map(str::to_string);
        let rest: Map<String, Value> = metadata
            .iter()
            .filter(|(key, value)| key.as_str() != "user_id" && !value.is_null())
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if !rest.is_empty() {
            request.metadata = Some(rest);
        }
    }
    request.service_tier = non_empty(body, "service_tier").map(str::to_string);

    for (key, value) in object {
        // The private extras key is the gateway's own; a client cannot set it.
        if key == TOOL_EXTRAS_KEY {
            continue;
        }
        if !KNOWN_KEYS.contains(&key.as_str()) && !value.is_null() {
            request.extra.insert(key.clone(), value.clone());
        }
    }
    Ok(request)
}
