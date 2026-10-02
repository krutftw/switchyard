//! OpenAI Chat Completions (`POST /v1/chat/completions`), per notes 15 §3.

use super::{
    Errs, Report, event_json, is_ident, is_json_object_text, is_url_or_data_uri, is_wrapped_blob,
    kind, object, only_keys, opt_bool, opt_number_in, opt_str, opt_uint, req_nonempty, req_str,
    req_uint,
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use switchyard_core::SseEvent;

/// Request fields of the Chat Completions API (notes 15 §3.1), plus `top_k`,
/// which only compatible servers know and the gateway only replays for Chat
/// clients.
const REQUEST_KEYS: &[&str] = &[
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
    "reasoning_effort",
    "user",
    "metadata",
    "service_tier",
    "prompt_cache_key",
    "prompt_cache_retention",
    "prompt_cache_options",
    "store",
    "web_search_options",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "modalities",
    "audio",
    "prediction",
    "verbosity",
    "safety_identifier",
];

const EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
const SERVICE_TIERS: &[&str] = &["auto", "default", "flex", "scale", "priority", "fast"];

/// OpenAI rejects tool-call ids longer than this ("string too long. Expected
/// a string with maximum length 40").
const MAX_TOOL_CALL_ID: usize = 40;

fn tool_names(root: &Map<String, Value>, errs: &mut Errs) -> Vec<String> {
    let mut names = Vec::new();
    let Some(tools) = root.get("tools") else {
        return names;
    };
    let Some(tools) = tools.as_array() else {
        errs.push("tools", "must be an array");
        return names;
    };
    if tools.is_empty() {
        errs.push("tools", "must not be empty when present");
    }
    for (i, tool) in tools.iter().enumerate() {
        let path = format!("tools[{i}]");
        let Some(tool) = object(tool, &path, errs) else {
            continue;
        };
        match req_str(tool, "type", &path, errs) {
            Some("function") => {
                only_keys(tool, &["type", "function"], &path, errs);
                let path = format!("{path}.function");
                let Some(function) = tool.get("function").and_then(|f| object(f, &path, errs))
                else {
                    errs.push(&path, "missing");
                    continue;
                };
                only_keys(
                    function,
                    &["name", "description", "parameters", "strict"],
                    &path,
                    errs,
                );
                if let Some(name) = req_str(function, "name", &path, errs) {
                    if !is_ident(name, 64) {
                        errs.push(
                            &path,
                            format!("name `{name}` does not match ^[a-zA-Z0-9_-]{{1,64}}$"),
                        );
                    }
                    if names.iter().any(|n| n == name) {
                        errs.push(&path, format!("duplicate tool name `{name}`"));
                    }
                    names.push(name.to_string());
                }
                opt_str(function, "description", &path, errs);
                opt_bool(function, "strict", &path, errs);
                if let Some(parameters) = function.get("parameters") {
                    match parameters.as_object() {
                        Some(schema) => {
                            if schema.get("type").and_then(Value::as_str) != Some("object") {
                                errs.push(&path, "parameters must be a schema of type `object`");
                            } else if !schema.get("properties").is_some_and(Value::is_object) {
                                // "object schema missing properties" is what
                                // the API answers to a bare `{"type":"object"}`.
                                errs.push(&path, "object schema is missing `properties`");
                            }
                        }
                        None => errs.push(&path, "parameters must be an object"),
                    }
                }
            }
            Some("custom") => {
                let path = format!("{path}.custom");
                if let Some(custom) = tool.get("custom").and_then(|c| object(c, &path, errs))
                    && let Some(name) = req_str(custom, "name", &path, errs)
                {
                    if !is_ident(name, 64) {
                        errs.push(&path, format!("invalid tool name `{name}`"));
                    }
                    names.push(name.to_string());
                }
            }
            Some(other) => errs.push(&path, format!("unknown tool type `{other}`")),
            None => {}
        }
    }
    names
}

fn check_user_part(part: &Value, path: &str, errs: &mut Errs) {
    let Some(part) = object(part, path, errs) else {
        return;
    };
    match req_str(part, "type", path, errs) {
        Some("text") => {
            req_str(part, "text", path, errs);
        }
        Some("image_url") => {
            let path = format!("{path}.image_url");
            let Some(image) = part.get("image_url").and_then(|i| object(i, &path, errs)) else {
                errs.push(&path, "missing");
                return;
            };
            only_keys(image, &["url", "detail"], &path, errs);
            if let Some(url) = req_nonempty(image, "url", &path, errs)
                && !is_url_or_data_uri(url)
            {
                errs.push(&path, "url is neither http(s) nor a base64 data URI");
            }
            if let Some(detail) = opt_str(image, "detail", &path, errs)
                && !["auto", "low", "high"].contains(&detail)
            {
                errs.push(&path, format!("invalid detail `{detail}`"));
            }
        }
        Some("input_audio") => {
            let path = format!("{path}.input_audio");
            if let Some(audio) = part.get("input_audio").and_then(|a| object(a, &path, errs)) {
                req_nonempty(audio, "data", &path, errs);
                req_nonempty(audio, "format", &path, errs);
            } else {
                errs.push(&path, "missing");
            }
        }
        Some("file") => {
            let path = format!("{path}.file");
            let Some(file) = part.get("file").and_then(|f| object(f, &path, errs)) else {
                errs.push(&path, "missing");
                return;
            };
            only_keys(file, &["file_data", "file_id", "filename"], &path, errs);
            match (file.get("file_data"), file.get("file_id")) {
                (Some(Value::String(data)), None) => {
                    if super::data_uri_media_type(data).is_none() {
                        errs.push(&path, "file_data must be a base64 data URI");
                    }
                    req_nonempty(file, "filename", &path, errs);
                }
                (None, Some(Value::String(id))) if !id.is_empty() => {}
                _ => errs.push(&path, "needs exactly one of file_data / file_id"),
            }
        }
        // Understood by Chat-compatible vision / audio servers only; the
        // gateway emits them for media no OpenAI part can carry.
        Some("video_url" | "audio_url") => {}
        Some(other) => errs.push(path, format!("unknown content part type `{other}`")),
        None => {}
    }
}

fn check_text_content(content: Option<&Value>, path: &str, errs: &mut Errs) {
    match content {
        Some(Value::String(_)) => {}
        Some(Value::Array(parts)) => {
            for (i, part) in parts.iter().enumerate() {
                let path = format!("{path}[{i}]");
                if let Some(part) = object(part, &path, errs) {
                    if part.get("type").and_then(Value::as_str) != Some("text") {
                        errs.push(&path, "only text parts are allowed here");
                    }
                    req_str(part, "text", &path, errs);
                }
            }
        }
        Some(other) => errs.push(
            path,
            format!(
                "must be a string or an array of text parts, found {}",
                kind(other)
            ),
        ),
        None => errs.push(path, "missing required field `content`"),
    }
}

/// Checks the tool calls of an assistant message and returns their ids.
fn check_tool_calls(
    calls: &Value,
    _declared: &[String],
    path: &str,
    errs: &mut Errs,
) -> Vec<String> {
    let mut ids = Vec::new();
    let Some(calls) = calls.as_array() else {
        errs.push(path, "tool_calls must be an array");
        return ids;
    };
    if calls.is_empty() {
        errs.push(path, "tool_calls must not be empty when present");
    }
    for (i, call) in calls.iter().enumerate() {
        let path = format!("{path}[{i}]");
        let Some(call) = object(call, &path, errs) else {
            continue;
        };
        if let Some(id) = req_nonempty(call, "id", &path, errs) {
            if id.len() > MAX_TOOL_CALL_ID {
                errs.push(
                    &path,
                    format!(
                        "id is {} characters, the maximum is {MAX_TOOL_CALL_ID}",
                        id.len()
                    ),
                );
            }
            if ids.iter().any(|seen| seen == id) {
                errs.push(&path, format!("duplicate tool call id `{id}`"));
            }
            ids.push(id.to_string());
        }
        match req_str(call, "type", &path, errs) {
            Some("function") => {
                let path = format!("{path}.function");
                let Some(function) = call.get("function").and_then(|f| object(f, &path, errs))
                else {
                    errs.push(&path, "missing");
                    continue;
                };
                if let Some(name) = req_str(function, "name", &path, errs)
                    && !is_ident(name, 64)
                {
                    errs.push(&path, format!("invalid function name `{name}`"));
                }
                if let Some(arguments) = req_str(function, "arguments", &path, errs)
                    && !is_json_object_text(arguments)
                {
                    errs.push(
                        &path,
                        format!("arguments are not a JSON object: {arguments:.60}"),
                    );
                }
            }
            Some("custom") => {
                let path = format!("{path}.custom");
                if let Some(custom) = call.get("custom").and_then(|c| object(c, &path, errs)) {
                    req_nonempty(custom, "name", &path, errs);
                    req_str(custom, "input", &path, errs);
                }
            }
            Some(other) => errs.push(&path, format!("unknown tool call type `{other}`")),
            None => {}
        }
        if let Some(signature) = call
            .get("extra_content")
            .and_then(|e| e.get("google"))
            .and_then(|g| g.get("thought_signature"))
            .and_then(Value::as_str)
            && is_wrapped_blob(signature)
        {
            errs.push(
                &path,
                "a gateway-wrapped thought signature reached the upstream",
            );
        }
    }
    ids
}

fn check_messages(root: &Map<String, Value>, declared: &[String], errs: &mut Errs) {
    let Some(messages) = root.get("messages") else {
        errs.push("messages", "missing required field");
        return;
    };
    let Some(messages) = messages.as_array() else {
        errs.push("messages", "must be an array");
        return;
    };
    if messages.is_empty() {
        errs.push("messages", "must not be empty");
    }
    // Ids of the calls of the latest assistant turn that are still waiting
    // for their `tool` message.
    let mut open_calls: Vec<String> = Vec::new();
    let mut open_at = String::new();
    for (i, message) in messages.iter().enumerate() {
        let path = format!("messages[{i}]");
        let Some(message) = object(message, &path, errs) else {
            continue;
        };
        let role = req_str(message, "role", &path, errs).unwrap_or("");
        if role != "tool" && !open_calls.is_empty() {
            errs.push(
                &open_at,
                format!(
                    "tool_calls {open_calls:?} are not answered by tool messages directly after it"
                ),
            );
            open_calls.clear();
        }
        match role {
            "system" | "developer" => {
                only_keys(message, &["role", "content", "name"], &path, errs);
                check_text_content(message.get("content"), &format!("{path}.content"), errs);
            }
            "user" => {
                only_keys(message, &["role", "content", "name"], &path, errs);
                match message.get("content") {
                    Some(Value::String(_)) => {}
                    Some(Value::Array(parts)) => {
                        if parts.is_empty() {
                            errs.push(&path, "content array must not be empty");
                        }
                        for (j, part) in parts.iter().enumerate() {
                            check_user_part(part, &format!("{path}.content[{j}]"), errs);
                        }
                    }
                    Some(other) => errs.push(
                        &path,
                        format!(
                            "content must be a string or an array, found {}",
                            kind(other)
                        ),
                    ),
                    None => errs.push(&path, "missing required field `content`"),
                }
            }
            "assistant" => {
                only_keys(
                    message,
                    &[
                        "role",
                        "content",
                        "name",
                        "refusal",
                        "tool_calls",
                        "audio",
                        // Read by thinking-mode compatible servers; OpenAI
                        // itself ignores them.
                        "reasoning_content",
                        "reasoning_details",
                    ],
                    &path,
                    errs,
                );
                let has_calls = message.contains_key("tool_calls");
                let has_refusal = message.get("refusal").is_some_and(Value::is_string);
                match message.get("content") {
                    Some(Value::String(_)) => {}
                    Some(Value::Null) | None => {
                        if !has_calls && !has_refusal {
                            errs.push(
                                &path,
                                "content is required unless tool_calls or refusal is given",
                            );
                        }
                    }
                    Some(Value::Array(parts)) => {
                        for (j, part) in parts.iter().enumerate() {
                            let path = format!("{path}.content[{j}]");
                            if let Some(part) = object(part, &path, errs)
                                && !matches!(
                                    part.get("type").and_then(Value::as_str),
                                    Some("text" | "refusal")
                                )
                            {
                                errs.push(&path, "assistant content parts are text or refusal");
                            }
                        }
                    }
                    Some(other) => errs.push(
                        &path,
                        format!(
                            "content must be a string, null or an array, found {}",
                            kind(other)
                        ),
                    ),
                }
                if let Some(details) = message.get("reasoning_details") {
                    let text = details.to_string();
                    if text.contains("\"sy1.") {
                        errs.push(
                            &path,
                            "a gateway-wrapped signature reached the upstream in reasoning_details",
                        );
                    }
                }
                if let Some(calls) = message.get("tool_calls") {
                    open_calls =
                        check_tool_calls(calls, declared, &format!("{path}.tool_calls"), errs);
                    open_at = path.clone();
                }
            }
            "tool" => {
                only_keys(message, &["role", "content", "tool_call_id"], &path, errs);
                check_text_content(message.get("content"), &format!("{path}.content"), errs);
                if let Some(id) = req_nonempty(message, "tool_call_id", &path, errs) {
                    match open_calls.iter().position(|open| open == id) {
                        Some(position) => {
                            open_calls.remove(position);
                        }
                        None => errs.push(
                            &path,
                            format!("tool_call_id `{id}` answers no pending tool call of the assistant message before it"),
                        ),
                    }
                }
            }
            "" => {}
            other => errs.push(&path, format!("unknown role `{other}`")),
        }
    }
    if !open_calls.is_empty() {
        errs.push(
            &open_at,
            format!("tool_calls {open_calls:?} are never answered"),
        );
    }
}

/// Validates a Chat Completions request body.
pub fn validate_chat_request(body: &Value) -> Report {
    let mut errs = Errs::default();
    let Some(root) = object(body, "$", &mut errs) else {
        return errs.finish();
    };
    only_keys(root, REQUEST_KEYS, "$", &mut errs);
    req_nonempty(root, "model", "$", &mut errs);
    let declared = tool_names(root, &mut errs);
    check_messages(root, &declared, &mut errs);

    match root.get("tool_choice") {
        None => {}
        Some(_) if declared.is_empty() => {
            errs.push("tool_choice", "is only allowed when tools are specified");
        }
        Some(Value::String(mode)) if !["none", "auto", "required"].contains(&mode.as_str()) => {
            errs.push("tool_choice", format!("invalid value `{mode}`"));
        }
        Some(Value::String(_)) => {}
        Some(Value::Object(choice)) => match choice.get("type").and_then(Value::as_str) {
            Some("function") => {
                let name = choice
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str);
                match name {
                    Some(name) if declared.iter().any(|d| d == name) => {}
                    Some(name) => errs.push(
                        "tool_choice",
                        format!("names `{name}`, which is not a declared tool"),
                    ),
                    None => errs.push("tool_choice", "function.name is required"),
                }
            }
            Some("custom" | "allowed_tools") => {}
            other => errs.push("tool_choice", format!("invalid type {other:?}")),
        },
        Some(other) => errs.push("tool_choice", format!("invalid value {other}")),
    }
    if root.contains_key("parallel_tool_calls") {
        opt_bool(root, "parallel_tool_calls", "$", &mut errs);
        if declared.is_empty() {
            errs.push(
                "parallel_tool_calls",
                "is only allowed when tools are specified",
            );
        }
    }

    for key in ["max_tokens", "max_completion_tokens"] {
        if let Some(limit) = opt_uint(root, key, "$", &mut errs)
            && limit == 0
        {
            errs.push(key, "must be at least 1");
        }
    }
    if root.contains_key("max_tokens") && root.contains_key("max_completion_tokens") {
        errs.push("$", "max_tokens and max_completion_tokens are both set");
    }
    opt_number_in(root, "temperature", 0.0, 2.0, "$", &mut errs);
    opt_number_in(root, "top_p", 0.0, 1.0, "$", &mut errs);
    opt_number_in(root, "presence_penalty", -2.0, 2.0, "$", &mut errs);
    opt_number_in(root, "frequency_penalty", -2.0, 2.0, "$", &mut errs);
    if let Some(n) = opt_uint(root, "n", "$", &mut errs)
        && n == 0
    {
        errs.push("n", "must be at least 1");
    }
    if let Some(seed) = root.get("seed")
        && !seed.is_i64()
        && !seed.is_u64()
    {
        errs.push("seed", "must be an integer");
    }
    match root.get("stop") {
        None | Some(Value::String(_)) => {}
        Some(Value::Array(stops)) => {
            if stops.is_empty() || stops.len() > 4 {
                errs.push("stop", "must hold between 1 and 4 sequences");
            }
            if !stops
                .iter()
                .all(|s| s.as_str().is_some_and(|s| !s.is_empty()))
            {
                errs.push("stop", "sequences must be non-empty strings");
            }
        }
        Some(other) => errs.push(
            "stop",
            format!("must be a string or an array, found {}", kind(other)),
        ),
    }
    if let Some(effort) = opt_str(root, "reasoning_effort", "$", &mut errs)
        && !EFFORTS.contains(&effort)
    {
        errs.push("reasoning_effort", format!("invalid value `{effort}`"));
    }
    if let Some(format) = root.get("response_format")
        && let Some(format) = object(format, "response_format", &mut errs)
    {
        match format.get("type").and_then(Value::as_str) {
            Some("text") => {}
            // "'messages' must contain the word 'json' in some form, to use
            // 'response_format' of type 'json_object'."
            Some("json_object") => {
                let said = root
                    .get("messages")
                    .is_some_and(|messages| messages.to_string().to_lowercase().contains("json"));
                if !said {
                    errs.push(
                        "response_format",
                        "json_object needs the word `json` somewhere in the messages",
                    );
                }
            }
            Some("json_schema") => {
                let path = "response_format.json_schema";
                match format.get("json_schema").and_then(Value::as_object) {
                    Some(spec) => {
                        if let Some(name) = req_str(spec, "name", path, &mut errs)
                            && !is_ident(name, 64)
                        {
                            errs.push(path, format!("invalid name `{name}`"));
                        }
                        match spec.get("schema").and_then(Value::as_object) {
                            // "schema must be a JSON Schema of 'type:
                            // \"object\"', got 'type: \"array\"'."
                            Some(schema) => {
                                if schema.get("type").and_then(Value::as_str) != Some("object") {
                                    errs.push(
                                        path,
                                        format!(
                                            "schema must be of type `object`, found {}",
                                            schema.get("type").unwrap_or(&Value::Null)
                                        ),
                                    );
                                }
                            }
                            None => errs.push(path, "schema must be an object"),
                        }
                        opt_bool(spec, "strict", path, &mut errs);
                    }
                    None => errs.push(path, "missing"),
                }
            }
            other => errs.push("response_format", format!("invalid type {other:?}")),
        }
    }
    let stream = opt_bool(root, "stream", "$", &mut errs).unwrap_or(false);
    if let Some(options) = root.get("stream_options") {
        if !stream {
            errs.push("stream_options", "is only allowed when stream is true");
        }
        if let Some(options) = object(options, "stream_options", &mut errs) {
            only_keys(
                options,
                &["include_usage", "include_obfuscation"],
                "stream_options",
                &mut errs,
            );
            opt_bool(options, "include_usage", "stream_options", &mut errs);
        }
    }
    let store = opt_bool(root, "store", "$", &mut errs);
    if let Some(metadata) = root.get("metadata") {
        if store != Some(true) {
            errs.push("metadata", "is only allowed when store is enabled");
        }
        match metadata.as_object() {
            Some(map) if map.values().all(Value::is_string) => {}
            _ => errs.push("metadata", "must be a map of strings"),
        }
    }
    if let Some(tier) = opt_str(root, "service_tier", "$", &mut errs)
        && !SERVICE_TIERS.contains(&tier)
    {
        errs.push("service_tier", format!("invalid value `{tier}`"));
    }
    opt_str(root, "user", "$", &mut errs);
    errs.finish()
}

/// Fields that exist on OpenAI's own platform only (stored completions,
/// cache routing, service tiers, safety identifiers).
const PLATFORM_KEYS: &[&str] = &[
    "store",
    "metadata",
    "service_tier",
    "prompt_cache_key",
    "prompt_cache_retention",
    "prompt_cache_options",
    "safety_identifier",
];

/// Validates a Chat Completions request for an OpenAI-*compatible* server:
/// everything [`validate_chat_request`] checks, and nothing that only
/// api.openai.com knows.
///
/// This is the profile a request translated from another protocol has to
/// meet: it is only translated to Chat when the provider speaks nothing
/// else (DeepSeek, Mistral, Groq, vLLM, Ollama, Google's compatible
/// endpoint, …). Those servers know `function` tools only, and the ones that
/// validate their request schema refuse OpenAI's platform fields (Mistral
/// answers 422 `extra_forbidden`). Notes 08 §1.3 (a custom tool becomes a
/// function tool), §5.1 ("everything else: dropped"), §5.2 (a
/// `custom_tool_call` becomes a function call).
pub fn validate_chat_compatible_request(body: &Value) -> Report {
    let mut errs = Errs::default();
    if let Err(violations) = validate_chat_request(body) {
        for violation in violations {
            errs.push("$", violation);
        }
    }
    let Some(root) = body.as_object() else {
        return errs.finish();
    };
    for key in PLATFORM_KEYS {
        if root.contains_key(*key) {
            errs.push(key, "exists on OpenAI's own platform only");
        }
    }
    for (i, tool) in root
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        if tool["type"] != "function" {
            errs.push(
                &format!("tools[{i}]"),
                format!(
                    "type {} (compatible servers know `function` only)",
                    tool["type"]
                ),
            );
        }
    }
    if let Some(choice) = root.get("tool_choice").filter(|choice| choice.is_object())
        && choice["type"] != "function"
    {
        errs.push(
            "tool_choice",
            format!(
                "type {} (compatible servers know `function` only)",
                choice["type"]
            ),
        );
    }
    for (i, message) in root
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        for (j, call) in message
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if call["type"] != "function" {
                errs.push(
                    &format!("messages[{i}].tool_calls[{j}]"),
                    format!(
                        "type {} (compatible servers know `function` only)",
                        call["type"]
                    ),
                );
            }
        }
    }
    errs.finish()
}

const FINISH_REASONS: &[&str] = &[
    "stop",
    "length",
    "tool_calls",
    "content_filter",
    "function_call",
];

fn check_usage(usage: &Value, path: &str, errs: &mut Errs) {
    let Some(usage) = object(usage, path, errs) else {
        return;
    };
    let prompt = req_uint(usage, "prompt_tokens", path, errs).unwrap_or(0);
    let completion = req_uint(usage, "completion_tokens", path, errs).unwrap_or(0);
    let total = req_uint(usage, "total_tokens", path, errs).unwrap_or(0);
    if errs.is_empty() && total != prompt + completion {
        errs.push(
            path,
            format!("total_tokens {total} != {prompt} + {completion}"),
        );
    }
    if let Some(details) = usage
        .get("prompt_tokens_details")
        .and_then(Value::as_object)
    {
        let cached = opt_uint(details, "cached_tokens", path, errs).unwrap_or(0);
        let written = opt_uint(details, "cache_write_tokens", path, errs).unwrap_or(0);
        // Cached tokens are a subset of the prompt tokens (notes 15 §3.2).
        if cached + written > prompt {
            errs.push(
                path,
                format!("cache tokens {cached}+{written} exceed prompt_tokens {prompt}"),
            );
        }
    }
    if let Some(details) = usage
        .get("completion_tokens_details")
        .and_then(Value::as_object)
    {
        let reasoning = opt_uint(details, "reasoning_tokens", path, errs).unwrap_or(0);
        if reasoning > completion {
            errs.push(
                path,
                format!("reasoning_tokens {reasoning} exceed completion_tokens {completion}"),
            );
        }
    }
}

/// Validates a complete `chat.completion` object.
pub fn validate_chat_response(body: &Value) -> Report {
    let mut errs = Errs::default();
    let Some(root) = object(body, "$", &mut errs) else {
        return errs.finish();
    };
    req_nonempty(root, "id", "$", &mut errs);
    if req_str(root, "object", "$", &mut errs).is_some_and(|o| o != "chat.completion") {
        errs.push("object", "must be `chat.completion`");
    }
    req_uint(root, "created", "$", &mut errs);
    req_nonempty(root, "model", "$", &mut errs);
    match root.get("choices").and_then(Value::as_array) {
        Some(choices) if !choices.is_empty() => {
            for (i, choice) in choices.iter().enumerate() {
                let path = format!("choices[{i}]");
                let Some(choice) = object(choice, &path, &mut errs) else {
                    continue;
                };
                if req_uint(choice, "index", &path, &mut errs) != Some(i as u64) {
                    errs.push(&path, "index does not match its position");
                }
                let finish = match choice.get("finish_reason") {
                    Some(Value::String(reason)) if FINISH_REASONS.contains(&reason.as_str()) => {
                        reason.as_str()
                    }
                    other => {
                        errs.push(&path, format!("invalid finish_reason {other:?}"));
                        ""
                    }
                };
                let path = format!("{path}.message");
                let Some(message) = choice
                    .get("message")
                    .and_then(|m| object(m, &path, &mut errs))
                else {
                    errs.push(&path, "missing");
                    continue;
                };
                if message.get("role").and_then(Value::as_str) != Some("assistant") {
                    errs.push(&path, "role must be `assistant`");
                }
                match message.get("content") {
                    Some(Value::String(_)) | Some(Value::Null) => {}
                    other => errs.push(
                        &path,
                        format!("content must be a string or null, found {other:?}"),
                    ),
                }
                match message.get("refusal") {
                    None | Some(Value::String(_)) | Some(Value::Null) => {}
                    Some(other) => errs.push(
                        &path,
                        format!("refusal must be a string or null, found {}", kind(other)),
                    ),
                }
                let mut has_calls = false;
                if let Some(calls) = message.get("tool_calls") {
                    has_calls = true;
                    match calls.as_array() {
                        Some(calls) if !calls.is_empty() => {
                            for (j, call) in calls.iter().enumerate() {
                                let path = format!("{path}.tool_calls[{j}]");
                                let Some(call) = object(call, &path, &mut errs) else {
                                    continue;
                                };
                                req_nonempty(call, "id", &path, &mut errs);
                                if call.get("type").and_then(Value::as_str) != Some("function") {
                                    errs.push(&path, "type must be `function`");
                                }
                                if call.contains_key("index") {
                                    errs.push(&path, "`index` only exists in stream deltas");
                                }
                                match call.get("function").and_then(Value::as_object) {
                                    Some(function) => {
                                        req_nonempty(function, "name", &path, &mut errs);
                                        let arguments =
                                            req_str(function, "arguments", &path, &mut errs);
                                        // A cut-off call is only legitimate on
                                        // a `length` stop.
                                        if let Some(arguments) = arguments
                                            && finish != "length"
                                            && !is_json_object_text(arguments)
                                        {
                                            errs.push(&path, format!("arguments are not a JSON object: {arguments:.60}"));
                                        }
                                    }
                                    None => errs.push(&path, "function is missing"),
                                }
                            }
                        }
                        _ => errs.push(&path, "tool_calls must be a non-empty array when present"),
                    }
                }
                if finish == "tool_calls" && !has_calls {
                    errs.push(&path, "finish_reason is tool_calls but there are none");
                }
                if finish == "stop" && has_calls {
                    errs.push(&path, "tool calls are reported with finish_reason `stop`");
                }
            }
        }
        _ => errs.push("choices", "must be a non-empty array"),
    }
    match root.get("usage") {
        Some(usage) => check_usage(usage, "usage", &mut errs),
        None => errs.push("usage", "missing"),
    }
    errs.finish()
}

#[derive(Default)]
struct StreamCall {
    has_id: bool,
    has_name: bool,
    arguments: String,
}

/// Validates a Chat Completions SSE stream (notes 15 §3.3).
///
/// A stream that ends with an error frame (`data: {"error":{…}}`) is valid
/// without `[DONE]`: that is how the API reports a failure after HTTP 200.
pub fn validate_chat_stream(events: &[SseEvent]) -> Report {
    let mut errs = Errs::default();
    if events.is_empty() {
        errs.push("stream", "no events");
        return errs.finish();
    }
    let mut id: Option<String> = None;
    let mut saw_role = false;
    let mut finish: Option<String> = None;
    let mut done = false;
    let mut failed = false;
    let mut usage_chunks = 0usize;
    let mut calls: BTreeMap<u64, StreamCall> = BTreeMap::new();
    let mut content_after_finish = false;
    for (i, event) in events.iter().enumerate() {
        let path = format!("event[{i}]");
        if event.event.is_some() {
            errs.push(&path, "Chat streams use data-only events");
        }
        if done {
            errs.push(&path, "event after [DONE]");
            continue;
        }
        if failed {
            errs.push(&path, "event after the error frame");
            continue;
        }
        if event.data.trim() == "[DONE]" {
            done = true;
            continue;
        }
        let Some(chunk) = event_json(event, i, &mut errs) else {
            continue;
        };
        let Some(chunk) = object(&chunk, &path, &mut errs) else {
            continue;
        };
        if chunk.contains_key("error") && !chunk.contains_key("choices") {
            let message = chunk
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str);
            if message.is_none_or(str::is_empty) {
                errs.push(&path, "error frame without a message");
            }
            failed = true;
            continue;
        }
        if chunk.get("object").and_then(Value::as_str) != Some("chat.completion.chunk") {
            errs.push(&path, "object must be `chat.completion.chunk`");
        }
        match chunk.get("id").and_then(Value::as_str) {
            Some(chunk_id) if !chunk_id.is_empty() => match &id {
                Some(first) if first != chunk_id => {
                    errs.push(&path, format!("id changed from `{first}` to `{chunk_id}`"));
                }
                Some(_) => {}
                None => id = Some(chunk_id.to_string()),
            },
            _ => errs.push(&path, "missing id"),
        }
        req_uint(chunk, "created", &path, &mut errs);
        req_nonempty(chunk, "model", &path, &mut errs);
        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            errs.push(&path, "choices must be an array");
            continue;
        };
        if choices.is_empty() {
            match chunk.get("usage") {
                Some(usage) if usage.is_object() => {
                    usage_chunks += 1;
                    check_usage(usage, &format!("{path}.usage"), &mut errs);
                    if finish.is_none() {
                        errs.push(&path, "usage chunk before the finish chunk");
                    }
                }
                _ => errs.push(&path, "a chunk without choices must carry usage"),
            }
            continue;
        }
        if choices.len() != 1 {
            errs.push(&path, "expected exactly one choice per chunk");
        }
        let Some(choice) = choices.first().and_then(Value::as_object) else {
            errs.push(&path, "choice must be an object");
            continue;
        };
        if choice.get("index").and_then(Value::as_u64) != Some(0) {
            errs.push(&path, "choice index must be 0");
        }
        let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
            errs.push(&path, "choice.delta must be an object");
            continue;
        };
        if delta.get("role").is_some_and(|r| !r.is_null()) {
            if delta.get("role").and_then(Value::as_str) != Some("assistant") {
                errs.push(&path, "delta.role must be `assistant`");
            }
            saw_role = true;
        }
        let has_payload = ["content", "refusal", "tool_calls", "reasoning_content"]
            .iter()
            .any(|key| delta.get(*key).is_some_and(|v| !v.is_null()));
        if has_payload && !saw_role {
            errs.push(&path, "content arrives before the role chunk");
        }
        if has_payload && finish.is_some() {
            content_after_finish = true;
        }
        for key in ["content", "refusal", "reasoning_content"] {
            if let Some(value) = delta.get(key)
                && !value.is_string()
                && !value.is_null()
            {
                errs.push(&path, format!("delta.{key} must be a string or null"));
            }
        }
        if let Some(tool_calls) = delta.get("tool_calls").filter(|v| !v.is_null()) {
            let Some(tool_calls) = tool_calls.as_array() else {
                errs.push(&path, "delta.tool_calls must be an array");
                continue;
            };
            for call in tool_calls {
                let Some(index) = call.get("index").and_then(Value::as_u64) else {
                    errs.push(&path, "tool call delta without an integer index");
                    continue;
                };
                let known = calls.contains_key(&index);
                if !known && index != calls.len() as u64 {
                    errs.push(
                        &path,
                        format!(
                            "tool call index {index} skips ahead (next is {})",
                            calls.len()
                        ),
                    );
                }
                let entry = calls.entry(index).or_default();
                let function = call.get("function").and_then(Value::as_object);
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                let name = function
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                if !known {
                    if id.is_none() {
                        errs.push(
                            &path,
                            format!("first fragment of tool call {index} has no id"),
                        );
                    }
                    if name.is_none() {
                        errs.push(
                            &path,
                            format!("first fragment of tool call {index} has no function.name"),
                        );
                    }
                    if call.get("type").and_then(Value::as_str) != Some("function") {
                        errs.push(
                            &path,
                            format!("first fragment of tool call {index} has no type"),
                        );
                    }
                } else if id.is_some() || name.is_some() {
                    errs.push(
                        &path,
                        format!("tool call {index} repeats its id / name in a later fragment"),
                    );
                }
                entry.has_id |= id.is_some();
                entry.has_name |= name.is_some();
                if let Some(arguments) = function.and_then(|f| f.get("arguments")) {
                    match arguments.as_str() {
                        Some(fragment) => entry.arguments.push_str(fragment),
                        None => errs.push(&path, "function.arguments must be a string"),
                    }
                }
            }
        }
        match choice.get("finish_reason") {
            Some(Value::Null) | None => {}
            Some(Value::String(reason)) => {
                if !FINISH_REASONS.contains(&reason.as_str()) {
                    errs.push(&path, format!("invalid finish_reason `{reason}`"));
                }
                if finish.is_some() {
                    errs.push(&path, "second finish_reason");
                }
                finish = Some(reason.clone());
            }
            Some(other) => errs.push(
                &path,
                format!(
                    "finish_reason must be a string or null, found {}",
                    kind(other)
                ),
            ),
        }
    }
    if failed {
        return errs.finish();
    }
    if !done {
        errs.push("stream", "does not end with `data: [DONE]`");
    }
    if content_after_finish {
        errs.push("stream", "content delta after the finish chunk");
    }
    match finish.as_deref() {
        None => errs.push("stream", "no chunk carries a finish_reason"),
        Some("tool_calls") => {
            if calls.is_empty() {
                errs.push(
                    "stream",
                    "finish_reason is tool_calls but no tool call was streamed",
                );
            }
            for (index, call) in &calls {
                if !call.arguments.is_empty() && !is_json_object_text(&call.arguments) {
                    errs.push(
                        "stream",
                        format!(
                            "arguments of tool call {index} are not a JSON object: {:.60}",
                            call.arguments
                        ),
                    );
                }
            }
        }
        Some("stop") if !calls.is_empty() => {
            errs.push(
                "stream",
                "tool calls were streamed but finish_reason is `stop`",
            );
        }
        Some(_) => {}
    }
    if usage_chunks > 1 {
        errs.push("stream", "more than one usage chunk");
    }
    errs.finish()
}
