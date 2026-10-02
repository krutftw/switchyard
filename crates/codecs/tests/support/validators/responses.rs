//! OpenAI Responses (`POST /v1/responses`), per notes 15 §4 and 08 §1–§3.

use super::{
    Errs, Report, event_json, is_ident, is_json_object_text, is_url_or_data_uri, is_wrapped_blob,
    kind, object, only_keys, opt_bool, opt_number_in, opt_str, opt_uint, req_nonempty, req_str,
    req_uint,
};
use serde_json::{Map, Value};
use switchyard_core::SseEvent;

/// Request fields of the Responses API (notes 15 §4.1 and 08 §1).
const REQUEST_KEYS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning",
    "text",
    "max_output_tokens",
    "max_tool_calls",
    "temperature",
    "top_p",
    "top_logprobs",
    "truncation",
    "store",
    "previous_response_id",
    "conversation",
    "include",
    "metadata",
    "user",
    "safety_identifier",
    "prompt_cache_key",
    "prompt_cache_retention",
    "prompt_cache_options",
    "service_tier",
    "stream",
    "stream_options",
    "background",
    "prompt",
];

const EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
const SUMMARIES: &[&str] = &["auto", "concise", "detailed"];
const SERVICE_TIERS: &[&str] = &["auto", "default", "flex", "scale", "priority", "ultrafast"];
/// `include[]` values (notes 15 §4.1).
const INCLUDES: &[&str] = &[
    "file_search_call.results",
    "web_search_call.results",
    "web_search_call.action.sources",
    "message.input_image.image_url",
    "computer_call_output.output.image_url",
    "code_interpreter_call.outputs",
    "reasoning.encrypted_content",
    "message.output_text.logprobs",
];
/// Hosted tool types a request may declare.
const HOSTED_TOOLS: &[&str] = &[
    "web_search",
    "web_search_preview",
    "web_search_2025_08_26",
    "web_search_preview_2025_03_11",
    "file_search",
    "code_interpreter",
    "image_generation",
    "computer_use_preview",
    "mcp",
    "local_shell",
    "shell",
    "apply_patch",
];

/// The vendor's limits on identifiers.
const MAX_CALL_ID: usize = 64;
const MIN_MAX_OUTPUT_TOKENS: u64 = 16;

fn tool_names(root: &Map<String, Value>, errs: &mut Errs) -> Vec<String> {
    let mut names = Vec::new();
    let Some(tools) = root.get("tools") else {
        return names;
    };
    let Some(tools) = tools.as_array() else {
        errs.push("tools", "must be an array");
        return names;
    };
    for (i, tool) in tools.iter().enumerate() {
        let path = format!("tools[{i}]");
        let Some(tool) = object(tool, &path, errs) else {
            continue;
        };
        match req_str(tool, "type", &path, errs) {
            Some("function") => {
                // The Responses spelling is flat: a nested `function` object
                // is the Chat spelling and is rejected.
                only_keys(
                    tool,
                    &["type", "name", "description", "parameters", "strict"],
                    &path,
                    errs,
                );
                if let Some(name) = req_str(tool, "name", &path, errs) {
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
                opt_bool(tool, "strict", &path, errs);
                match tool.get("parameters") {
                    Some(Value::Object(schema)) => {
                        if schema.get("type").and_then(Value::as_str) != Some("object") {
                            errs.push(&path, "parameters must be a schema of type `object`");
                        } else if !schema.get("properties").is_some_and(Value::is_object) {
                            errs.push(&path, "object schema is missing `properties`");
                        }
                    }
                    Some(Value::Null) | None => {}
                    Some(other) => errs.push(
                        &path,
                        format!("parameters must be an object, found {}", kind(other)),
                    ),
                }
            }
            Some("custom") => {
                if let Some(name) = req_str(tool, "name", &path, errs) {
                    if !is_ident(name, 64) {
                        errs.push(&path, format!("invalid tool name `{name}`"));
                    }
                    names.push(name.to_string());
                }
            }
            Some("namespace") => {}
            Some(other) if HOSTED_TOOLS.contains(&other) => {}
            Some(other) => errs.push(&path, format!("unknown tool type `{other}`")),
            None => {}
        }
    }
    names
}

fn check_input_part(part: &Value, assistant: bool, path: &str, errs: &mut Errs) {
    let Some(part) = object(part, path, errs) else {
        return;
    };
    let kind = req_str(part, "type", path, errs).unwrap_or("");
    match (assistant, kind) {
        (false, "input_text") | (true, "output_text") => {
            req_str(part, "text", path, errs);
        }
        (true, "refusal") => {
            req_str(part, "refusal", path, errs);
        }
        (false, "input_image") => {
            only_keys(
                part,
                &["type", "image_url", "file_id", "detail"],
                path,
                errs,
            );
            match (part.get("image_url"), part.get("file_id")) {
                (Some(Value::String(url)), None) => {
                    if !is_url_or_data_uri(url) {
                        errs.push(path, "image_url is neither http(s) nor a base64 data URI");
                    }
                }
                (None, Some(Value::String(id))) if !id.is_empty() => {}
                // The nested `{"url": …}` object is the Chat spelling.
                _ => errs.push(path, "needs image_url (a string) or file_id"),
            }
            if let Some(detail) = opt_str(part, "detail", path, errs)
                && !["auto", "low", "high", "original"].contains(&detail)
            {
                errs.push(path, format!("invalid detail `{detail}`"));
            }
        }
        (false, "input_file") => {
            only_keys(
                part,
                &["type", "file_data", "file_url", "file_id", "filename"],
                path,
                errs,
            );
            let sources = ["file_data", "file_url", "file_id"]
                .iter()
                .filter(|key| part.get(**key).is_some_and(Value::is_string))
                .count();
            if sources != 1 {
                errs.push(path, "needs exactly one of file_data / file_url / file_id");
            }
            if let Some(data) = opt_str(part, "file_data", path, errs) {
                if super::data_uri_media_type(data).is_none() {
                    errs.push(path, "file_data must be a base64 data URI");
                }
                req_nonempty(part, "filename", path, errs);
            }
        }
        (false, "input_audio") => {}
        (_, "") => {}
        (_, other) => errs.push(
            path,
            format!(
                "content part `{other}` is not valid in {} message",
                if assistant {
                    "an assistant"
                } else {
                    "an input"
                }
            ),
        ),
    }
}

/// What the input item list has to satisfy as a whole: tool outputs answer
/// calls, calls are answered, reasoning items are followed by output of the
/// same turn.
fn check_input_items(items: &[Value], store: Option<bool>, _declared: &[String], errs: &mut Errs) {
    // call id -> answered
    let mut calls: Vec<(String, bool)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let path = format!("input[{i}]");
        let Some(map) = object(item, &path, errs) else {
            continue;
        };
        let item_type = match map.get("type").and_then(Value::as_str) {
            Some(kind) => kind,
            None if map.contains_key("role") => "message",
            None => {
                errs.push(&path, "item has neither `type` nor `role`");
                continue;
            }
        };
        match item_type {
            "message" => {
                let role = req_str(map, "role", &path, errs).unwrap_or("");
                if !["user", "assistant", "system", "developer"].contains(&role) {
                    errs.push(&path, format!("invalid role `{role}`"));
                }
                let assistant = role == "assistant";
                match map.get("content") {
                    Some(Value::String(_)) => {}
                    Some(Value::Array(parts)) => {
                        if parts.is_empty() {
                            errs.push(&path, "content must not be empty");
                        }
                        for (j, part) in parts.iter().enumerate() {
                            check_input_part(
                                part,
                                assistant,
                                &format!("{path}.content[{j}]"),
                                errs,
                            );
                        }
                    }
                    other => errs.push(
                        &path,
                        format!("content must be a string or an array, found {other:?}"),
                    ),
                }
            }
            "function_call" | "custom_tool_call" => {
                if let Some(id) = req_nonempty(map, "call_id", &path, errs) {
                    if id.chars().count() > MAX_CALL_ID {
                        errs.push(&path, format!("call_id is longer than {MAX_CALL_ID}"));
                    }
                    if calls.iter().any(|(seen, _)| seen == id) {
                        errs.push(&path, format!("duplicate call_id `{id}`"));
                    }
                    calls.push((id.to_string(), false));
                }
                if let Some(name) = req_str(map, "name", &path, errs)
                    && !is_ident(name, 64)
                {
                    errs.push(&path, format!("invalid tool name `{name}`"));
                }
                if item_type == "function_call" {
                    if let Some(arguments) = req_str(map, "arguments", &path, errs)
                        && !is_json_object_text(arguments)
                    {
                        errs.push(
                            &path,
                            format!("arguments are not a JSON object: {arguments:.60}"),
                        );
                    }
                    if let Some(id) = opt_str(map, "id", &path, errs)
                        && !id.starts_with("fc")
                    {
                        errs.push(&path, format!("item id `{id}` must begin with `fc`"));
                    }
                } else {
                    req_str(map, "input", &path, errs);
                }
            }
            "function_call_output" | "custom_tool_call_output" => {
                if let Some(id) = req_nonempty(map, "call_id", &path, errs) {
                    match calls.iter_mut().find(|(call, _)| call == id) {
                        Some((_, answered)) if *answered => {
                            errs.push(&path, format!("call `{id}` is answered twice"));
                        }
                        Some((_, answered)) => *answered = true,
                        None => errs.push(
                            &path,
                            format!("call_id `{id}` matches no function_call before it"),
                        ),
                    }
                }
                match map.get("output") {
                    Some(Value::String(_)) => {}
                    Some(Value::Array(parts)) => {
                        if parts.is_empty() {
                            errs.push(&path, "output array must not be empty");
                        }
                        for (j, part) in parts.iter().enumerate() {
                            check_input_part(part, false, &format!("{path}.output[{j}]"), errs);
                        }
                    }
                    other => errs.push(
                        &path,
                        format!("output must be a string or an array of parts, found {other:?}"),
                    ),
                }
            }
            "reasoning" => {
                match map.get("summary").and_then(Value::as_array) {
                    Some(summary) => {
                        for (j, entry) in summary.iter().enumerate() {
                            let ok = entry.get("type").and_then(Value::as_str)
                                == Some("summary_text")
                                && entry.get("text").is_some_and(Value::is_string);
                            if !ok {
                                errs.push(
                                    &format!("{path}.summary[{j}]"),
                                    "must be a summary_text part",
                                );
                            }
                        }
                    }
                    None => errs.push(&path, "reasoning item needs a `summary` array"),
                }
                let id = opt_str(map, "id", &path, errs);
                if let Some(id) = id
                    && (!id.starts_with("rs_") || id.chars().count() > 64)
                {
                    errs.push(&path, format!("invalid reasoning item id `{id}`"));
                }
                match map.get("encrypted_content") {
                    Some(Value::String(blob)) if !blob.is_empty() => {
                        if is_wrapped_blob(blob) {
                            errs.push(
                                &path,
                                "a gateway-wrapped blob reached the upstream as encrypted_content",
                            );
                        }
                    }
                    Some(Value::Null) | None => {
                        // Without the blob the item only names state stored
                        // at the vendor, which `store: false` rules out.
                        if id.is_none() {
                            errs.push(
                                &path,
                                "reasoning item has neither encrypted_content nor an id",
                            );
                        } else if store == Some(false) {
                            errs.push(
                                &path,
                                "an id-only reasoning item needs stored state, but store is false",
                            );
                        }
                    }
                    Some(other) => errs.push(
                        &path,
                        format!("encrypted_content must be a non-empty string, found {other}"),
                    ),
                }
                // "Item of type 'reasoning' was provided without its required
                // following item."
                let follower = items[i + 1..]
                    .iter()
                    .find(|next| next.get("type").and_then(Value::as_str) != Some("reasoning"));
                let follows = follower.is_some_and(|next| {
                    let kind = next
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("message");
                    match kind {
                        "message" => next.get("role").and_then(Value::as_str) == Some("assistant"),
                        other => other.ends_with("_call"),
                    }
                });
                if !follows {
                    errs.push(
                        &path,
                        "reasoning item is not followed by output of the same turn",
                    );
                }
            }
            "item_reference" => {
                // A reference only resolves against state stored at the vendor.
                if store == Some(false) {
                    errs.push(
                        &path,
                        "item_reference needs stored state, but store is false",
                    );
                }
            }
            other if other.ends_with("_call") || other.ends_with("_output") => {}
            other => errs.push(&path, format!("unknown input item type `{other}`")),
        }
    }
    for (id, answered) in &calls {
        if !answered {
            errs.push("input", format!("no tool output found for call `{id}`"));
        }
    }
}

/// Validates a Responses request body.
pub fn validate_responses_request(body: &Value) -> Report {
    let mut errs = Errs::default();
    let Some(root) = object(body, "$", &mut errs) else {
        return errs.finish();
    };
    only_keys(root, REQUEST_KEYS, "$", &mut errs);
    req_nonempty(root, "model", "$", &mut errs);
    let declared = tool_names(root, &mut errs);
    let store = opt_bool(root, "store", "$", &mut errs);

    match root.get("input") {
        Some(Value::String(_)) => {}
        Some(Value::Array(items)) => check_input_items(items, store, &declared, &mut errs),
        Some(other) => errs.push(
            "input",
            format!("must be a string or an array, found {}", kind(other)),
        ),
        None => {
            if !root.contains_key("previous_response_id") && !root.contains_key("prompt") {
                errs.push("input", "missing required field");
            }
        }
    }
    opt_str(root, "instructions", "$", &mut errs);

    let has_tools = root
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    match root.get("tool_choice") {
        None => {}
        Some(_) if !has_tools => {
            errs.push("tool_choice", "is only allowed when tools are specified");
        }
        Some(Value::String(mode)) if !["none", "auto", "required"].contains(&mode.as_str()) => {
            errs.push("tool_choice", format!("invalid value `{mode}`"));
        }
        Some(Value::String(_)) => {}
        Some(Value::Object(choice)) => match choice.get("type").and_then(Value::as_str) {
            Some("function" | "custom") => match choice.get("name").and_then(Value::as_str) {
                Some(name) if declared.iter().any(|d| d == name) => {}
                Some(name) => errs.push(
                    "tool_choice",
                    format!("names `{name}`, which is not a declared tool"),
                ),
                // `{"function": {"name": …}}` is the Chat spelling.
                None => errs.push("tool_choice", "`name` is required at the top level"),
            },
            Some("allowed_tools") => {}
            // Forcing a hosted tool the request does not offer is refused
            // like forcing an undeclared function.
            Some(other) if HOSTED_TOOLS.contains(&other) => {
                let offered = root
                    .get("tools")
                    .and_then(Value::as_array)
                    .is_some_and(|tools| tools.iter().any(|tool| tool["type"] == other));
                if !offered {
                    errs.push(
                        "tool_choice",
                        format!("forces `{other}`, which is not among the tools"),
                    );
                }
            }
            other => errs.push("tool_choice", format!("invalid type {other:?}")),
        },
        Some(other) => errs.push("tool_choice", format!("invalid value {other}")),
    }
    if root.contains_key("parallel_tool_calls") {
        opt_bool(root, "parallel_tool_calls", "$", &mut errs);
        if !has_tools {
            errs.push(
                "parallel_tool_calls",
                "is only allowed when tools are specified",
            );
        }
    }

    if let Some(reasoning) = root.get("reasoning")
        && let Some(reasoning) = object(reasoning, "reasoning", &mut errs)
    {
        only_keys(
            reasoning,
            &["effort", "summary", "mode", "context", "generate_summary"],
            "reasoning",
            &mut errs,
        );
        if reasoning.is_empty() {
            errs.push("reasoning", "must not be an empty object");
        }
        if let Some(effort) = opt_str(reasoning, "effort", "reasoning", &mut errs)
            && !EFFORTS.contains(&effort)
        {
            errs.push("reasoning.effort", format!("invalid value `{effort}`"));
        }
        // `null` is not accepted by every backend: omission is "off".
        if let Some(summary) = opt_str(reasoning, "summary", "reasoning", &mut errs)
            && !SUMMARIES.contains(&summary)
        {
            errs.push("reasoning.summary", format!("invalid value `{summary}`"));
        }
    }
    if let Some(text) = root.get("text")
        && let Some(text) = object(text, "text", &mut errs)
    {
        only_keys(text, &["format", "verbosity"], "text", &mut errs);
        if let Some(format) = text.get("format")
            && let Some(format) = object(format, "text.format", &mut errs)
        {
            match format.get("type").and_then(Value::as_str) {
                Some("text") => {}
                // "Response input messages must contain the word 'json' in
                // some form to use 'text.format' of type 'json_object'."
                // Whether `instructions` count is not documented; they are
                // accepted here.
                Some("json_object") => {
                    let said = ["instructions", "input"].iter().any(|key| {
                        root.get(*key)
                            .is_some_and(|value| value.to_string().to_lowercase().contains("json"))
                    });
                    if !said {
                        errs.push(
                            "text.format",
                            "json_object needs the word `json` somewhere in the input",
                        );
                    }
                }
                Some("json_schema") => {
                    // Flat: `name` and `schema` sit directly under `format`.
                    if let Some(name) = req_str(format, "name", "text.format", &mut errs)
                        && !is_ident(name, 64)
                    {
                        errs.push("text.format", format!("invalid name `{name}`"));
                    }
                    match format.get("schema").and_then(Value::as_object) {
                        // "schema must be a JSON Schema of 'type:
                        // \"object\"', got 'type: \"array\"'."
                        Some(schema) => {
                            if schema.get("type").and_then(Value::as_str) != Some("object") {
                                errs.push(
                                    "text.format",
                                    format!(
                                        "schema must be of type `object`, found {}",
                                        schema.get("type").unwrap_or(&Value::Null)
                                    ),
                                );
                            }
                        }
                        None => errs.push("text.format", "schema must be an object"),
                    }
                }
                other => errs.push("text.format", format!("invalid type {other:?}")),
            }
        }
    }
    if let Some(limit) = opt_uint(root, "max_output_tokens", "$", &mut errs)
        && limit < MIN_MAX_OUTPUT_TOKENS
    {
        errs.push(
            "max_output_tokens",
            format!("{limit} is below the minimum of {MIN_MAX_OUTPUT_TOKENS}"),
        );
    }
    opt_number_in(root, "temperature", 0.0, 2.0, "$", &mut errs);
    opt_number_in(root, "top_p", 0.0, 1.0, "$", &mut errs);
    opt_bool(root, "stream", "$", &mut errs);
    if let Some(options) = root.get("stream_options").and_then(Value::as_object)
        && options.contains_key("include_usage")
    {
        // Usage is always in the terminal event on this API.
        errs.push(
            "stream_options",
            "`include_usage` does not exist on Responses",
        );
    }
    if let Some(include) = root.get("include") {
        match include.as_array() {
            Some(entries) => {
                for entry in entries {
                    match entry.as_str() {
                        Some(name) if INCLUDES.contains(&name) => {}
                        other => errs.push("include", format!("invalid entry {other:?}")),
                    }
                }
            }
            None => errs.push("include", "must be an array"),
        }
    }
    if let Some(metadata) = root.get("metadata") {
        match metadata.as_object() {
            Some(map) if map.len() <= 16 && map.values().all(Value::is_string) => {}
            _ => errs.push("metadata", "must be a map of at most 16 strings"),
        }
    }
    if let Some(tier) = opt_str(root, "service_tier", "$", &mut errs)
        && !SERVICE_TIERS.contains(&tier)
    {
        errs.push("service_tier", format!("invalid value `{tier}`"));
    }
    opt_str(root, "user", "$", &mut errs);
    opt_str(root, "previous_response_id", "$", &mut errs);
    errs.finish()
}

fn check_usage(usage: &Value, path: &str, errs: &mut Errs) {
    let Some(usage) = object(usage, path, errs) else {
        return;
    };
    let input = req_uint(usage, "input_tokens", path, errs).unwrap_or(0);
    let output = req_uint(usage, "output_tokens", path, errs).unwrap_or(0);
    let total = req_uint(usage, "total_tokens", path, errs).unwrap_or(0);
    if total != input + output {
        errs.push(path, format!("total_tokens {total} != {input} + {output}"));
    }
    match usage.get("input_tokens_details").and_then(Value::as_object) {
        Some(details) => {
            let cached = req_uint(details, "cached_tokens", path, errs).unwrap_or(0);
            if cached > input {
                errs.push(
                    path,
                    format!("cached_tokens {cached} exceed input_tokens {input}"),
                );
            }
        }
        None => errs.push(path, "missing input_tokens_details"),
    }
    match usage
        .get("output_tokens_details")
        .and_then(Value::as_object)
    {
        Some(details) => {
            let reasoning = req_uint(details, "reasoning_tokens", path, errs).unwrap_or(0);
            if reasoning > output {
                errs.push(
                    path,
                    format!("reasoning_tokens {reasoning} exceed output_tokens {output}"),
                );
            }
        }
        None => errs.push(path, "missing output_tokens_details"),
    }
}

/// Checks one output item. `finished` is false for the `in_progress` copy an
/// `output_item.added` event carries.
fn check_output_item(item: &Value, finished: bool, path: &str, errs: &mut Errs) {
    let Some(item) = object(item, path, errs) else {
        return;
    };
    let id = req_nonempty(item, "id", path, errs).unwrap_or("");
    let statuses: &[&str] = if finished {
        &["completed", "incomplete"]
    } else {
        &["in_progress"]
    };
    let check_status = |errs: &mut Errs| match item.get("status").and_then(Value::as_str) {
        Some(status) if statuses.contains(&status) => {}
        other => errs.push(path, format!("invalid status {other:?}")),
    };
    match req_str(item, "type", path, errs) {
        Some("message") => {
            check_status(errs);
            if !id.starts_with("msg_") {
                errs.push(
                    path,
                    format!("message item id `{id}` must begin with `msg_`"),
                );
            }
            if item.get("role").and_then(Value::as_str) != Some("assistant") {
                errs.push(path, "role must be `assistant`");
            }
            match item.get("content").and_then(Value::as_array) {
                Some(parts) => {
                    for (i, part) in parts.iter().enumerate() {
                        let path = format!("{path}.content[{i}]");
                        match part.get("type").and_then(Value::as_str) {
                            Some("output_text") => {
                                if !part.get("text").is_some_and(Value::is_string) {
                                    errs.push(&path, "text must be a string");
                                }
                                if !part.get("annotations").is_some_and(Value::is_array) {
                                    errs.push(&path, "annotations must be an array");
                                }
                            }
                            Some("refusal") => {
                                if !part.get("refusal").is_some_and(Value::is_string) {
                                    errs.push(&path, "refusal must be a string");
                                }
                            }
                            other => errs.push(&path, format!("invalid part type {other:?}")),
                        }
                    }
                }
                None => errs.push(path, "content must be an array"),
            }
        }
        Some(kind @ ("function_call" | "custom_tool_call")) => {
            check_status(errs);
            req_nonempty(item, "call_id", path, errs);
            req_nonempty(item, "name", path, errs);
            if kind == "function_call" {
                if !id.starts_with("fc") {
                    errs.push(
                        path,
                        format!("function_call item id `{id}` must begin with `fc`"),
                    );
                }
                let arguments = req_str(item, "arguments", path, errs);
                let complete = item.get("status").and_then(Value::as_str) == Some("completed");
                if let Some(arguments) = arguments
                    && complete
                    && !is_json_object_text(arguments)
                {
                    errs.push(
                        path,
                        format!(
                            "arguments of a completed call are not a JSON object: {arguments:.60}"
                        ),
                    );
                }
            } else {
                req_str(item, "input", path, errs);
            }
        }
        Some("reasoning") => {
            if !id.starts_with("rs_") {
                errs.push(
                    path,
                    format!("reasoning item id `{id}` must begin with `rs_`"),
                );
            }
            match item.get("summary").and_then(Value::as_array) {
                Some(summary) => {
                    for entry in summary {
                        let ok = entry.get("type").and_then(Value::as_str) == Some("summary_text")
                            && entry.get("text").is_some_and(Value::is_string);
                        if !ok {
                            errs.push(path, "summary entries must be summary_text parts");
                        }
                    }
                }
                None => errs.push(path, "summary must be an array"),
            }
            match item.get("encrypted_content") {
                None | Some(Value::Null) => {}
                Some(Value::String(blob)) if !blob.is_empty() => {}
                Some(other) => errs.push(
                    path,
                    format!("encrypted_content must be a non-empty string, found {other}"),
                ),
            }
        }
        Some(_) | None => {}
    }
}

fn check_response_object(response: &Value, path: &str, terminal: bool, errs: &mut Errs) {
    let Some(root) = object(response, path, errs) else {
        return;
    };
    if let Some(id) = req_nonempty(root, "id", path, errs)
        && !id.starts_with("resp_")
    {
        errs.push(path, format!("response id `{id}` must begin with `resp_`"));
    }
    if req_str(root, "object", path, errs).is_some_and(|o| o != "response") {
        errs.push(path, "object must be `response`");
    }
    req_uint(root, "created_at", path, errs);
    req_nonempty(root, "model", path, errs);
    if root.contains_key("output_text") {
        // An SDK convenience property, never part of the wire object.
        errs.push(path, "`output_text` is not a wire field");
    }
    let status = req_str(root, "status", path, errs).unwrap_or("");
    let allowed: &[&str] = if terminal {
        &["completed", "incomplete", "failed"]
    } else {
        &["in_progress", "queued"]
    };
    if !allowed.contains(&status) {
        errs.push(path, format!("invalid status `{status}`"));
    }
    let details = root.get("incomplete_details");
    match status {
        "incomplete" => {
            let reason = details
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str);
            if reason.is_none_or(str::is_empty) {
                errs.push(
                    path,
                    "incomplete response without incomplete_details.reason",
                );
            }
        }
        _ => {
            if details.is_some_and(|d| !d.is_null()) {
                errs.push(
                    path,
                    "incomplete_details must be null unless status is incomplete",
                );
            }
        }
    }
    let error = root.get("error");
    if status == "failed" {
        if !error.is_some_and(Value::is_object) {
            errs.push(path, "failed response without an error object");
        }
    } else if error.is_some_and(|e| !e.is_null()) {
        errs.push(path, "error must be null unless status is failed");
    }
    match root.get("output").and_then(Value::as_array) {
        Some(items) => {
            let mut ids: Vec<&str> = Vec::new();
            for (i, item) in items.iter().enumerate() {
                check_output_item(item, true, &format!("{path}.output[{i}]"), errs);
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    if ids.contains(&id) {
                        errs.push(path, format!("duplicate output item id `{id}`"));
                    }
                    ids.push(id);
                }
            }
        }
        None => errs.push(path, "output must be an array"),
    }
    match root.get("usage") {
        Some(Value::Null) | None => {
            if terminal && status != "failed" {
                errs.push(path, "terminal response without usage");
            }
        }
        Some(usage) => check_usage(usage, &format!("{path}.usage"), errs),
    }
}

/// Validates a complete Responses response object.
pub fn validate_responses_response(body: &Value) -> Report {
    let mut errs = Errs::default();
    check_response_object(body, "$", true, &mut errs);
    errs.finish()
}

/// The output item an `output_item.added` opened and that has not been
/// closed by `output_item.done` yet.
struct OpenItem {
    index: u64,
    id: String,
    kind: String,
    text: String,
    arguments: String,
    saw_arguments_done: bool,
    part_open: bool,
}

/// Validates a Responses SSE stream (notes 15 §4.3 and 08 §3).
pub fn validate_responses_stream(events: &[SseEvent]) -> Report {
    let mut errs = Errs::default();
    if events.is_empty() {
        errs.push("stream", "no events");
        return errs.finish();
    }
    let mut next_sequence = 0u64;
    let mut terminal = false;
    let mut open: Option<OpenItem> = None;
    let mut next_output_index = 0u64;
    let mut done_items: Vec<Value> = Vec::new();
    for (i, event) in events.iter().enumerate() {
        let path = format!("event[{i}]");
        if terminal {
            errs.push(&path, "event after the terminal event");
            continue;
        }
        if event.data.trim() == "[DONE]" {
            errs.push(&path, "Responses streams have no [DONE] sentinel");
            continue;
        }
        let Some(data) = event_json(event, i, &mut errs) else {
            continue;
        };
        let Some(map) = object(&data, &path, &mut errs) else {
            continue;
        };
        let kind = map.get("type").and_then(Value::as_str).unwrap_or("");
        if event.event.as_deref() != Some(kind) {
            errs.push(
                &path,
                format!("SSE event name {:?} != type `{kind}`", event.event),
            );
        }
        match map.get("sequence_number").and_then(Value::as_u64) {
            Some(n) if n == next_sequence => {}
            other => errs.push(
                &path,
                format!("sequence_number {other:?}, expected {next_sequence}"),
            ),
        }
        next_sequence += 1;
        if i == 0 && kind != "response.created" && kind != "error" {
            errs.push(
                &path,
                format!("the stream must open with response.created, not `{kind}`"),
            );
        }
        if i == 1 && kind != "response.in_progress" && kind != "error" {
            errs.push(
                &path,
                format!("response.in_progress must follow response.created, not `{kind}`"),
            );
        }
        let output_index = map.get("output_index").and_then(Value::as_u64);
        // Every per-item event names the open item.
        let per_item = kind.starts_with("response.")
            && !matches!(
                kind,
                "response.created"
                    | "response.in_progress"
                    | "response.output_item.added"
                    | "response.output_item.done"
                    | "response.completed"
                    | "response.incomplete"
                    | "response.failed"
            );
        if per_item {
            let item_id = map.get("item_id").and_then(Value::as_str);
            match &open {
                Some(item) => {
                    if output_index != Some(item.index) {
                        errs.push(
                            &path,
                            format!(
                                "output_index {output_index:?} is not the open item's ({})",
                                item.index
                            ),
                        );
                    }
                    if item_id != Some(item.id.as_str()) {
                        errs.push(
                            &path,
                            format!("item_id {item_id:?} is not the open item's (`{}`)", item.id),
                        );
                    }
                }
                None => {
                    errs.push(&path, format!("`{kind}` while no output item is open"));
                    continue;
                }
            }
        }
        match kind {
            "response.created" | "response.in_progress" => match map.get("response") {
                Some(response) => {
                    check_response_object(response, &format!("{path}.response"), false, &mut errs)
                }
                None => errs.push(&path, "missing response"),
            },
            "response.output_item.added" => {
                if let Some(item) = &open {
                    errs.push(
                        &path,
                        format!(
                            "item {} opens while item {} is still open",
                            output_index.unwrap_or(0),
                            item.index
                        ),
                    );
                }
                if output_index != Some(next_output_index) {
                    errs.push(
                        &path,
                        format!("output_index {output_index:?}, expected {next_output_index}"),
                    );
                }
                next_output_index += 1;
                let item = map.get("item").cloned().unwrap_or(Value::Null);
                let kind = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // Hosted-tool and image items arrive complete.
                let streamed = matches!(
                    kind.as_str(),
                    "message" | "function_call" | "custom_tool_call"
                );
                if streamed {
                    check_output_item(&item, false, &format!("{path}.item"), &mut errs);
                }
                open = Some(OpenItem {
                    index: output_index.unwrap_or(0),
                    id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    kind,
                    text: String::new(),
                    arguments: String::new(),
                    saw_arguments_done: false,
                    part_open: false,
                });
            }
            "response.output_item.done" => {
                let item = map.get("item").cloned().unwrap_or(Value::Null);
                check_output_item(&item, true, &format!("{path}.item"), &mut errs);
                match open.take() {
                    Some(opened) => {
                        if output_index != Some(opened.index) {
                            errs.push(&path, "output_index is not the open item's");
                        }
                        if item.get("id").and_then(Value::as_str) != Some(opened.id.as_str()) {
                            errs.push(
                                &path,
                                "the finished item has a different id than the one that opened",
                            );
                        }
                        if opened.part_open {
                            errs.push(&path, "item closed while a content part is still open");
                        }
                        if opened.kind == "function_call" {
                            if !opened.saw_arguments_done {
                                errs.push(
                                    &path,
                                    "function_call closed without function_call_arguments.done",
                                );
                            }
                            let arguments =
                                item.get("arguments").and_then(Value::as_str).unwrap_or("");
                            let streamed = opened.arguments.as_str();
                            if arguments != streamed && !(streamed.is_empty() && arguments == "{}")
                            {
                                errs.push(&path, format!("item arguments {arguments:.40} differ from the streamed deltas {streamed:.40}"));
                            }
                        }
                    }
                    None => errs.push(&path, "output_item.done without an open item"),
                }
                done_items.push(item);
            }
            "response.content_part.added" | "response.reasoning_summary_part.added" => {
                if let Some(item) = &mut open {
                    if item.part_open {
                        errs.push(&path, "a part opens while another is open");
                    }
                    item.part_open = true;
                    item.text.clear();
                }
            }
            "response.output_text.delta"
            | "response.refusal.delta"
            | "response.reasoning_summary_text.delta" => {
                if let Some(item) = &mut open {
                    if !item.part_open {
                        errs.push(&path, "text delta outside a content part");
                    }
                    match map.get("delta").and_then(Value::as_str) {
                        Some(delta) => item.text.push_str(delta),
                        None => errs.push(&path, "delta must be a string"),
                    }
                }
            }
            "response.output_text.done"
            | "response.refusal.done"
            | "response.reasoning_summary_text.done" => {
                if let Some(item) = &open {
                    let key = if kind == "response.refusal.done" {
                        "refusal"
                    } else {
                        "text"
                    };
                    if map.get(key).and_then(Value::as_str) != Some(item.text.as_str()) {
                        errs.push(
                            &path,
                            format!("`{key}` differs from the concatenated deltas"),
                        );
                    }
                }
            }
            "response.content_part.done" | "response.reasoning_summary_part.done" => {
                if let Some(item) = &mut open {
                    if !item.part_open {
                        errs.push(&path, "part closed that was not open");
                    }
                    item.part_open = false;
                }
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                if let Some(item) = &mut open {
                    let custom = kind.starts_with("response.custom");
                    if custom != (item.kind == "custom_tool_call") {
                        errs.push(&path, format!("`{kind}` on a `{}` item", item.kind));
                    }
                    match map.get("delta").and_then(Value::as_str) {
                        Some(delta) => item.arguments.push_str(delta),
                        None => errs.push(&path, "delta must be a string"),
                    }
                }
            }
            "response.function_call_arguments.done" | "response.custom_tool_call_input.done" => {
                if let Some(item) = &mut open {
                    item.saw_arguments_done = true;
                }
            }
            "response.output_text.annotation.added" => {}
            "response.completed" | "response.incomplete" | "response.failed" => {
                terminal = true;
                if let Some(item) = &open {
                    errs.push(
                        &path,
                        format!("terminal event while item {} is open", item.index),
                    );
                }
                let Some(response) = map.get("response") else {
                    errs.push(&path, "missing response");
                    continue;
                };
                check_response_object(response, &format!("{path}.response"), true, &mut errs);
                let status = response.get("status").and_then(Value::as_str).unwrap_or("");
                if kind != format!("response.{status}") {
                    errs.push(&path, format!("event `{kind}` carries status `{status}`"));
                }
                if response.get("output").and_then(Value::as_array) != Some(&done_items) {
                    errs.push(
                        &path,
                        "response.output is not the list of items finished in the stream",
                    );
                }
            }
            "error" => {
                terminal = true;
                // Notes 99 R19: the nested object, with the fields copied to
                // the top level for clients of the documented flat shape.
                let message = map
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .or_else(|| map.get("message"))
                    .and_then(Value::as_str);
                if message.is_none_or(str::is_empty) {
                    errs.push(&path, "error event without a message");
                }
            }
            other => errs.push(&path, format!("unexpected event type `{other}`")),
        }
    }
    if !terminal {
        errs.push("stream", "no terminal event");
    }
    errs.finish()
}
