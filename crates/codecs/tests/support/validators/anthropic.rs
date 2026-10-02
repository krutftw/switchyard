//! Anthropic Messages (`POST /v1/messages`), per notes 15 §5, 06 and 12 §7.3.

use super::{
    Errs, Report, event_json, is_ident, is_json_object_text, is_wrapped_blob, kind, object,
    only_keys, opt_bool, opt_number_in, opt_str, opt_uint, req_nonempty, req_str, req_uint,
};
use serde_json::{Map, Value};
use switchyard_core::SseEvent;

/// Request fields of the Messages API (notes 15 §5.2), including the newer
/// optional ones a native client may send.
const REQUEST_KEYS: &[&str] = &[
    "model",
    "max_tokens",
    "messages",
    "system",
    "tools",
    "tool_choice",
    "thinking",
    "output_config",
    "temperature",
    "top_p",
    "top_k",
    "stop_sequences",
    "metadata",
    "service_tier",
    "stream",
    "speed",
    "container",
    "context_management",
    "mcp_servers",
    "inference_geo",
    "cache_control",
    "fallbacks",
];

const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const STOP_REASONS: &[&str] = &[
    "end_turn",
    "max_tokens",
    "stop_sequence",
    "tool_use",
    "pause_turn",
    "refusal",
    "model_context_window_exceeded",
];
const IMAGE_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Longest tool name the API accepts. Notes 08 §4.5 record the reference's
/// 64; the API's own validation message is `^[a-zA-Z0-9_-]{1,128}$`, which is
/// what the codec is built for.
const MAX_TOOL_NAME: usize = 128;
/// Smallest thinking budget (notes 15 §5.6).
const MIN_BUDGET: u64 = 1024;
const MAX_USER_ID: usize = 256;

/// Model ids (as substrings) of the generations that take no assistant
/// prefill: "This model does not support assistant message prefill. The
/// conversation must end with a user message." Claude 4.6 and later (notes
/// 15 §5.2).
const NO_PREFILL: &[&str] = &[
    "opus-4-6",
    "sonnet-4-6",
    "opus-4-7",
    "sonnet-4-7",
    "opus-5",
    "sonnet-5",
    "haiku-5",
    "fable-5",
    "mythos-5",
];

/// Model ids (as substrings) of the models that take no forced tool use:
/// "tool_choice: type \"tool\" and \"any\" are not supported for this
/// model." (notes 15 §5.2).
const NO_FORCED_TOOL_CHOICE: &[&str] = &["opus-5-5", "sonnet-5-5", "fable-5-1", "mythos-5-1"];

fn model_in(root: &Map<String, Value>, ids: &[&str]) -> bool {
    root.get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| model.contains("claude") && ids.iter().any(|id| model.contains(id)))
}

fn sendable_text(text: &str) -> bool {
    // "text content blocks must contain non-whitespace text"
    !text.trim().is_empty()
}

fn check_cache_control(block: &Map<String, Value>, path: &str, errs: &mut Errs) {
    if let Some(cache) = block.get("cache_control")
        && cache.get("type").and_then(Value::as_str) != Some("ephemeral")
    {
        errs.push(path, "cache_control.type must be `ephemeral`");
    }
}

fn check_media_source(block: &Map<String, Value>, document: bool, path: &str, errs: &mut Errs) {
    let path = format!("{path}.source");
    let Some(source) = block.get("source").and_then(|s| object(s, &path, errs)) else {
        errs.push(&path, "missing");
        return;
    };
    match req_str(source, "type", &path, errs) {
        Some("base64") => {
            let media_type = req_str(source, "media_type", &path, errs).unwrap_or("");
            if document {
                if media_type != "application/pdf" {
                    errs.push(
                        &path,
                        format!("base64 documents must be application/pdf, not `{media_type}`"),
                    );
                }
            } else if !IMAGE_TYPES.contains(&media_type) {
                errs.push(
                    &path,
                    format!("unsupported image media type `{media_type}`"),
                );
            }
            req_nonempty(source, "data", &path, errs);
        }
        Some("url") => {
            if let Some(url) = req_nonempty(source, "url", &path, errs)
                && !url.starts_with("http://")
                && !url.starts_with("https://")
            {
                errs.push(&path, "url sources must be http(s) URLs");
            }
        }
        Some("file") => {
            req_nonempty(source, "file_id", &path, errs);
        }
        Some("text") if document => {
            if req_str(source, "media_type", &path, errs).is_some_and(|m| m != "text/plain") {
                errs.push(&path, "text documents must be text/plain");
            }
            if let Some(data) = req_str(source, "data", &path, errs)
                && !sendable_text(data)
            {
                errs.push(&path, "text document without text");
            }
        }
        Some("content") if document => {}
        Some(other) => errs.push(&path, format!("invalid source type `{other}`")),
        None => {}
    }
}

/// One conversation turn, reduced to what the pairing rules look at.
struct Turn {
    role: String,
    tool_use_ids: Vec<String>,
    tool_result_ids: Vec<String>,
    first_block: String,
    last_block: String,
}

fn check_message(
    message: &Value,
    _declared: &[String],
    path: &str,
    errs: &mut Errs,
) -> Option<Turn> {
    let message = object(message, path, errs)?;
    only_keys(message, &["role", "content"], path, errs);
    let role = req_str(message, "role", path, errs)?.to_string();
    if role != "user" && role != "assistant" {
        // Mid-conversation `system` turns only exist on the newest models
        // (notes 99 U3 lists the ones that answer them with a 400).
        errs.push(
            path,
            format!("role must be user or assistant, found `{role}`"),
        );
    }
    let assistant = role == "assistant";
    let mut turn = Turn {
        role,
        tool_use_ids: Vec::new(),
        tool_result_ids: Vec::new(),
        first_block: String::new(),
        last_block: String::new(),
    };
    let blocks = match message.get("content") {
        Some(Value::String(text)) => {
            if !sendable_text(text) {
                errs.push(path, "content must contain non-whitespace text");
            }
            turn.first_block = "text".into();
            turn.last_block = "text".into();
            return Some(turn);
        }
        Some(Value::Array(blocks)) => blocks,
        other => {
            errs.push(
                path,
                format!("content must be a string or an array, found {other:?}"),
            );
            return Some(turn);
        }
    };
    if blocks.is_empty() {
        errs.push(path, "content must not be empty");
    }
    let mut seen_non_result = false;
    for (i, block) in blocks.iter().enumerate() {
        let path = format!("{path}.content[{i}]");
        let Some(block) = object(block, &path, errs) else {
            continue;
        };
        let block_type = req_str(block, "type", &path, errs).unwrap_or("");
        if i == 0 {
            turn.first_block = block_type.to_string();
        }
        turn.last_block = block_type.to_string();
        check_cache_control(block, &path, errs);
        if block_type != "tool_result" {
            seen_non_result = true;
        }
        match block_type {
            "text" => {
                if let Some(text) = req_str(block, "text", &path, errs)
                    && !sendable_text(text)
                {
                    errs.push(
                        &path,
                        "text content blocks must contain non-whitespace text",
                    );
                }
            }
            "image" => check_media_source(block, false, &path, errs),
            "document" => check_media_source(block, true, &path, errs),
            "tool_use" => {
                if !assistant {
                    errs.push(&path, "tool_use blocks belong to assistant turns");
                }
                if let Some(id) = req_str(block, "id", &path, errs) {
                    if !is_ident(id, usize::MAX) {
                        errs.push(&path, format!("id `{id}` does not match ^[a-zA-Z0-9_-]+$"));
                    }
                    turn.tool_use_ids.push(id.to_string());
                }
                if let Some(name) = req_str(block, "name", &path, errs)
                    && !is_ident(name, MAX_TOOL_NAME)
                {
                    errs.push(&path, format!("invalid tool name `{name}`"));
                }
                if !block.get("input").is_some_and(Value::is_object) {
                    errs.push(&path, "input must be an object");
                }
                for stray in [
                    "signature",
                    "thoughtSignature",
                    "thought_signature",
                    "extra_content",
                ] {
                    if block.contains_key(stray) {
                        errs.push(&path, format!("stray `{stray}` on a tool_use block"));
                    }
                }
            }
            "tool_result" => {
                if assistant {
                    errs.push(&path, "tool_result blocks belong to user turns");
                }
                if seen_non_result {
                    errs.push(
                        &path,
                        "tool_result blocks must come first in the content array",
                    );
                }
                if let Some(id) = req_nonempty(block, "tool_use_id", &path, errs) {
                    turn.tool_result_ids.push(id.to_string());
                }
                opt_bool(block, "is_error", &path, errs);
                match block.get("content") {
                    None | Some(Value::String(_)) => {
                        if block.get("is_error") == Some(&Value::Bool(true))
                            && block
                                .get("content")
                                .and_then(Value::as_str)
                                .is_none_or(|t| !sendable_text(t))
                        {
                            errs.push(&path, "an error result needs content");
                        }
                    }
                    Some(Value::Array(inner)) => {
                        for (j, item) in inner.iter().enumerate() {
                            let path = format!("{path}.content[{j}]");
                            let Some(item) = object(item, &path, errs) else {
                                continue;
                            };
                            if item.contains_key("cache_control") {
                                errs.push(
                                    &path,
                                    "cache_control is not allowed inside tool_result content",
                                );
                            }
                            match item.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    if !item
                                        .get("text")
                                        .and_then(Value::as_str)
                                        .is_some_and(sendable_text)
                                    {
                                        errs.push(
                                            &path,
                                            "text content blocks must contain non-whitespace text",
                                        );
                                    }
                                }
                                Some("image") => check_media_source(item, false, &path, errs),
                                Some("document") => check_media_source(item, true, &path, errs),
                                Some("search_result" | "tool_reference") => {}
                                other => errs.push(
                                    &path,
                                    format!("invalid block type {other:?} in a tool result"),
                                ),
                            }
                        }
                    }
                    Some(other) => errs.push(
                        &path,
                        format!(
                            "content must be a string or an array, found {}",
                            kind(other)
                        ),
                    ),
                }
            }
            "thinking" => {
                if !assistant {
                    errs.push(&path, "thinking blocks belong to assistant turns");
                }
                req_str(block, "thinking", &path, errs);
                match req_str(block, "signature", &path, errs) {
                    Some(signature) if signature.trim().is_empty() => {
                        errs.push(&path, "thinking block without a signature");
                    }
                    Some(signature) if is_wrapped_blob(signature) => {
                        errs.push(&path, "a gateway-wrapped signature reached the upstream");
                    }
                    _ => {}
                }
            }
            "redacted_thinking" => {
                if !assistant {
                    errs.push(&path, "redacted_thinking blocks belong to assistant turns");
                }
                match req_nonempty(block, "data", &path, errs) {
                    Some(data) if is_wrapped_blob(data) => {
                        errs.push(&path, "a gateway-wrapped payload reached the upstream");
                    }
                    _ => {}
                }
            }
            // Server-tool activity and newer block types travel verbatim.
            "server_tool_use" | "web_search_tool_result" | "search_result" => {}
            "" => {}
            other => errs.push(&path, format!("unknown block type `{other}`")),
        }
    }
    Some(turn)
}

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
        if let Some(name) = req_str(tool, "name", &path, errs) {
            if !is_ident(name, MAX_TOOL_NAME) {
                errs.push(
                    &path,
                    format!("name `{name}` does not match ^[a-zA-Z0-9_-]{{1,128}}$"),
                );
            }
            if names.iter().any(|n| n == name) {
                errs.push(&path, format!("duplicate tool name `{name}`"));
            }
            names.push(name.to_string());
        }
        check_cache_control(tool, &path, errs);
        // Anthropic-defined tools carry a versioned `type` and no schema.
        if tool
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t != "custom")
        {
            continue;
        }
        opt_str(tool, "description", &path, errs);
        match tool.get("input_schema").and_then(Value::as_object) {
            Some(schema) => {
                if schema.get("type").and_then(Value::as_str) != Some("object") {
                    errs.push(&path, "input_schema.type must be `object`");
                }
                for union in ["anyOf", "oneOf", "allOf"] {
                    if schema.contains_key(union) {
                        errs.push(
                            &path,
                            format!("input_schema must not have `{union}` at the root"),
                        );
                    }
                }
            }
            None => errs.push(&path, "input_schema must be an object"),
        }
    }
    names
}

/// Validates a Messages request body.
pub fn validate_anthropic_request(body: &Value) -> Report {
    let mut errs = Errs::default();
    let Some(root) = object(body, "$", &mut errs) else {
        return errs.finish();
    };
    only_keys(root, REQUEST_KEYS, "$", &mut errs);
    req_nonempty(root, "model", "$", &mut errs);
    let max_tokens = req_uint(root, "max_tokens", "$", &mut errs);
    let declared = tool_names(root, &mut errs);

    match root.get("system") {
        None => {}
        Some(Value::String(text)) if !sendable_text(text) => {
            errs.push("system", "must contain non-whitespace text");
        }
        Some(Value::String(_)) => {}
        Some(Value::Array(blocks)) => {
            if blocks.is_empty() {
                errs.push("system", "must not be an empty array");
            }
            for (i, block) in blocks.iter().enumerate() {
                let path = format!("system[{i}]");
                let Some(block) = object(block, &path, &mut errs) else {
                    continue;
                };
                if block.get("type").and_then(Value::as_str) != Some("text") {
                    errs.push(&path, "system blocks must be text blocks");
                }
                if !block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(sendable_text)
                {
                    errs.push(
                        &path,
                        "text content blocks must contain non-whitespace text",
                    );
                }
                check_cache_control(block, &path, &mut errs);
            }
        }
        Some(other) => errs.push(
            "system",
            format!("must be a string or an array, found {}", kind(other)),
        ),
    }

    let mut turns: Vec<Turn> = Vec::new();
    match root.get("messages").and_then(Value::as_array) {
        Some(messages) => {
            if messages.is_empty() {
                errs.push("messages", "must not be empty");
            }
            for (i, message) in messages.iter().enumerate() {
                turns.extend(check_message(
                    message,
                    &declared,
                    &format!("messages[{i}]"),
                    &mut errs,
                ));
            }
        }
        None => errs.push("messages", "must be an array"),
    }
    if turns.first().is_some_and(|turn| turn.role != "user") {
        errs.push("messages[0]", "the first message must be a user message");
    }
    let mut seen_ids: Vec<&str> = Vec::new();
    for (i, turn) in turns.iter().enumerate() {
        let path = format!("messages[{i}]");
        if i > 0 && turns[i - 1].role == turn.role {
            errs.push(&path, format!("two consecutive `{}` messages", turn.role));
        }
        for id in &turn.tool_use_ids {
            if seen_ids.contains(&id.as_str()) {
                errs.push(&path, format!("tool_use id `{id}` is not unique"));
            }
            seen_ids.push(id);
        }
        // Each tool_result answers a tool_use of the turn right before it …
        let previous: &[String] = match i.checked_sub(1).map(|p| &turns[p]) {
            Some(previous) if previous.role == "assistant" => &previous.tool_use_ids,
            _ => &[],
        };
        let mut answered: Vec<&str> = Vec::new();
        for id in &turn.tool_result_ids {
            if !previous.contains(id) {
                errs.push(
                    &path,
                    format!("tool_result `{id}` has no tool_use in the previous message"),
                );
            }
            if answered.contains(&id.as_str()) {
                errs.push(&path, format!("tool_use `{id}` is answered twice"));
            }
            answered.push(id);
        }
        // … and each tool_use is answered by the turn right after it.
        if !turn.tool_use_ids.is_empty() {
            let results: &[String] = match turns.get(i + 1) {
                Some(next) if next.role == "user" => &next.tool_result_ids,
                _ => &[],
            };
            for id in &turn.tool_use_ids {
                if !results.contains(id) {
                    errs.push(
                        &path,
                        format!("tool_use `{id}` has no tool_result in the next message"),
                    );
                }
            }
        }
    }
    if let Some(last) = turns.last()
        && last.role == "assistant"
        && matches!(last.last_block.as_str(), "thinking" | "redacted_thinking")
    {
        errs.push(
            "messages",
            "a final assistant message must not end with a thinking block",
        );
    }
    if turns.last().is_some_and(|last| last.role == "assistant") && model_in(root, NO_PREFILL) {
        errs.push(
            "messages",
            "this model does not support assistant message prefill",
        );
    }

    let tool_choice = root.get("tool_choice");
    let mut forced = false;
    if let Some(choice) = tool_choice {
        if declared.is_empty() {
            errs.push("tool_choice", "is only allowed when tools are specified");
        }
        if let Some(choice) = object(choice, "tool_choice", &mut errs) {
            only_keys(
                choice,
                &["type", "name", "disable_parallel_tool_use"],
                "tool_choice",
                &mut errs,
            );
            opt_bool(
                choice,
                "disable_parallel_tool_use",
                "tool_choice",
                &mut errs,
            );
            match choice.get("type").and_then(Value::as_str) {
                Some("auto") => {}
                Some("none") => {
                    if choice.contains_key("disable_parallel_tool_use") {
                        errs.push("tool_choice", "`none` takes no disable_parallel_tool_use");
                    }
                }
                Some("any") => forced = true,
                Some("tool") => {
                    forced = true;
                    match choice.get("name").and_then(Value::as_str) {
                        Some(name) if declared.iter().any(|d| d == name) => {}
                        other => errs.push(
                            "tool_choice",
                            format!("names {other:?}, which is not a declared tool"),
                        ),
                    }
                }
                other => errs.push("tool_choice", format!("invalid type {other:?}")),
            }
        }
    }

    let mut thinking_active = false;
    if let Some(thinking) = root.get("thinking")
        && let Some(thinking) = object(thinking, "thinking", &mut errs)
    {
        only_keys(
            thinking,
            &["type", "budget_tokens", "display"],
            "thinking",
            &mut errs,
        );
        let kind = thinking.get("type").and_then(Value::as_str);
        match kind {
            Some("enabled") => {
                thinking_active = true;
                match thinking.get("budget_tokens").and_then(Value::as_u64) {
                    Some(budget) => {
                        if budget < MIN_BUDGET {
                            errs.push(
                                "thinking",
                                format!("budget_tokens {budget} is below {MIN_BUDGET}"),
                            );
                        }
                        if let Some(max) = max_tokens
                            && budget >= max
                        {
                            errs.push(
                                "thinking",
                                format!(
                                    "budget_tokens {budget} must be less than max_tokens {max}"
                                ),
                            );
                        }
                    }
                    None => errs.push("thinking", "`enabled` needs an integer budget_tokens"),
                }
                // "When thinking is enabled, a final assistant message must
                // start with a thinking block": the turn in progress is
                // everything after the last user message without tool results.
                let start = turns
                    .iter()
                    .rposition(|turn| turn.role == "user" && turn.tool_result_ids.is_empty())
                    .map_or(0, |at| at + 1);
                if let Some(first) = turns[start.min(turns.len())..]
                    .iter()
                    .find(|turn| turn.role == "assistant")
                    && !matches!(first.first_block.as_str(), "thinking" | "redacted_thinking")
                {
                    errs.push("thinking", "manual thinking, but the assistant turn in progress does not start with a thinking block");
                }
            }
            Some("adaptive") => {
                thinking_active = true;
                if thinking.contains_key("budget_tokens") {
                    errs.push("thinking", "`adaptive` takes no budget_tokens");
                }
            }
            Some("disabled") => {
                if thinking.len() != 1 {
                    errs.push("thinking", "`disabled` takes no other fields");
                }
            }
            other => errs.push("thinking", format!("invalid type {other:?}")),
        }
        if let Some(display) = opt_str(thinking, "display", "thinking", &mut errs)
            && !["summarized", "omitted", "updates"].contains(&display)
        {
            errs.push("thinking", format!("invalid display `{display}`"));
        }
        if max_tokens == Some(0) && kind == Some("enabled") {
            errs.push(
                "thinking",
                "manual thinking cannot be combined with max_tokens: 0",
            );
        }
    }
    if thinking_active && forced {
        errs.push(
            "tool_choice",
            "forced tool use cannot be combined with thinking",
        );
    }
    if forced && model_in(root, NO_FORCED_TOOL_CHOICE) {
        errs.push(
            "tool_choice",
            "type `tool` and `any` are not supported for this model",
        );
    }

    if let Some(config) = root.get("output_config")
        && let Some(config) = object(config, "output_config", &mut errs)
    {
        only_keys(config, &["effort", "format"], "output_config", &mut errs);
        if config.is_empty() {
            errs.push("output_config", "must not be an empty object");
        }
        if let Some(effort) = opt_str(config, "effort", "output_config", &mut errs)
            && !EFFORTS.contains(&effort)
        {
            errs.push("output_config.effort", format!("invalid value `{effort}`"));
        }
        if let Some(format) = config.get("format") {
            if format.get("type").and_then(Value::as_str) != Some("json_schema") {
                errs.push("output_config.format", "type must be `json_schema`");
            }
            if !format.get("schema").is_some_and(Value::is_object) {
                errs.push("output_config.format", "schema must be an object");
            }
        }
    }

    let temperature = opt_number_in(root, "temperature", 0.0, 1.0, "$", &mut errs);
    let top_p = opt_number_in(root, "top_p", 0.0, 1.0, "$", &mut errs);
    opt_uint(root, "top_k", "$", &mut errs);
    if thinking_active {
        // Notes 12 §7.3: with thinking, temperature must be 1, top_p at
        // least 0.95 and top_k unset.
        if temperature.is_some_and(|t| t != 1.0) {
            errs.push(
                "temperature",
                "must be 1 (or absent) while thinking is active",
            );
        }
        if top_p.is_some_and(|p| p < 0.95) {
            errs.push("top_p", "must be at least 0.95 while thinking is active");
        }
        if root.contains_key("top_k") {
            errs.push("top_k", "must not be set while thinking is active");
        }
    } else if root.contains_key("temperature") && root.contains_key("top_p") {
        errs.push("$", "temperature and top_p cannot both be specified");
    }
    if let Some(stops) = root.get("stop_sequences") {
        match stops.as_array() {
            Some(stops) => {
                if !stops.iter().all(|s| s.as_str().is_some_and(sendable_text)) {
                    errs.push(
                        "stop_sequences",
                        "each sequence must contain non-whitespace text",
                    );
                }
            }
            None => errs.push("stop_sequences", "must be an array"),
        }
    }
    if let Some(metadata) = root.get("metadata")
        && let Some(metadata) = object(metadata, "metadata", &mut errs)
    {
        only_keys(metadata, &["user_id"], "metadata", &mut errs);
        if let Some(user) = opt_str(metadata, "user_id", "metadata", &mut errs)
            && user.chars().count() > MAX_USER_ID
        {
            errs.push("metadata.user_id", "is longer than 256 characters");
        }
    }
    if let Some(tier) = opt_str(root, "service_tier", "$", &mut errs)
        && !["auto", "standard_only"].contains(&tier)
    {
        errs.push("service_tier", format!("invalid value `{tier}`"));
    }
    opt_bool(root, "stream", "$", &mut errs);
    errs.finish()
}

fn check_usage(usage: &Value, path: &str, errs: &mut Errs) {
    let Some(usage) = object(usage, path, errs) else {
        return;
    };
    req_uint(usage, "input_tokens", path, errs);
    let output = req_uint(usage, "output_tokens", path, errs).unwrap_or(0);
    opt_uint(usage, "cache_creation_input_tokens", path, errs);
    opt_uint(usage, "cache_read_input_tokens", path, errs);
    if let Some(details) = usage
        .get("output_tokens_details")
        .and_then(Value::as_object)
    {
        let thinking = opt_uint(details, "thinking_tokens", path, errs).unwrap_or(0);
        // `output_tokens` is the inclusive total (notes 15 §5.3).
        if thinking > output {
            errs.push(
                path,
                format!("thinking_tokens {thinking} exceed output_tokens {output}"),
            );
        }
    }
}

/// Checks a content block of a response. Returns its type.
fn check_response_block<'a>(block: &'a Value, path: &str, errs: &mut Errs) -> &'a str {
    let Some(block) = object(block, path, errs) else {
        return "";
    };
    let block_type = req_str(block, "type", path, errs).unwrap_or("");
    match block_type {
        "text" => {
            req_str(block, "text", path, errs);
        }
        "thinking" => {
            req_str(block, "thinking", path, errs);
            // Always present on the wire; empty until `signature_delta` in a
            // stream, and for reasoning no vendor signed.
            req_str(block, "signature", path, errs);
        }
        "redacted_thinking" => {
            req_nonempty(block, "data", path, errs);
        }
        "tool_use" => {
            // Any non-empty id: a client only echoes it. The id pattern the
            // API enforces (`^[a-zA-Z0-9_-]+$`) is a rule for *requests*,
            // checked by `validate_anthropic_request`; an id another vendor
            // minted keeps its spelling on the way to the client so that it
            // still pairs with what that vendor (and the gateway's reasoning
            // store) knows, and the codec's `prepare_passthrough` rewrites
            // it when such a history is later forwarded to Anthropic itself.
            req_nonempty(block, "id", path, errs);
            req_nonempty(block, "name", path, errs);
            if !block.get("input").is_some_and(Value::is_object) {
                errs.push(path, "input must be an object");
            }
        }
        _ => {}
    }
    block_type
}

/// Validates a complete Messages response.
pub fn validate_anthropic_response(body: &Value) -> Report {
    let mut errs = Errs::default();
    let Some(root) = object(body, "$", &mut errs) else {
        return errs.finish();
    };
    if let Some(id) = req_nonempty(root, "id", "$", &mut errs)
        && !id.starts_with("msg_")
    {
        errs.push("id", format!("`{id}` must begin with `msg_`"));
    }
    if req_str(root, "type", "$", &mut errs).is_some_and(|t| t != "message") {
        errs.push("type", "must be `message`");
    }
    if req_str(root, "role", "$", &mut errs).is_some_and(|r| r != "assistant") {
        errs.push("role", "must be `assistant`");
    }
    req_nonempty(root, "model", "$", &mut errs);
    let mut has_tool_use = false;
    match root.get("content").and_then(Value::as_array) {
        Some(blocks) => {
            for (i, block) in blocks.iter().enumerate() {
                if check_response_block(block, &format!("content[{i}]"), &mut errs) == "tool_use" {
                    has_tool_use = true;
                }
            }
        }
        None => errs.push("content", "must be an array"),
    }
    let stop_reason = match root.get("stop_reason") {
        Some(Value::String(reason)) => {
            if !STOP_REASONS.contains(&reason.as_str()) {
                errs.push("stop_reason", format!("invalid value `{reason}`"));
            }
            Some(reason.as_str())
        }
        Some(Value::Null) => None,
        other => {
            errs.push(
                "stop_reason",
                format!("must be a string or null, found {other:?}"),
            );
            None
        }
    };
    if stop_reason == Some("tool_use") && !has_tool_use {
        errs.push(
            "stop_reason",
            "is tool_use but the content has no tool_use block",
        );
    }
    if stop_reason == Some("end_turn") && has_tool_use {
        errs.push(
            "stop_reason",
            "is end_turn although a tool_use block is pending",
        );
    }
    match root.get("stop_sequence") {
        Some(Value::Null) => {}
        Some(Value::String(_)) => {
            if stop_reason != Some("stop_sequence") {
                errs.push(
                    "stop_sequence",
                    "is only reported with stop_reason `stop_sequence`",
                );
            }
        }
        other => errs.push(
            "stop_sequence",
            format!("must be a string or null, found {other:?}"),
        ),
    }
    match root.get("usage") {
        Some(usage) => check_usage(usage, "usage", &mut errs),
        None => errs.push("usage", "missing"),
    }
    errs.finish()
}

/// The content block that `content_block_start` opened.
struct OpenBlock {
    index: u64,
    kind: String,
    json: String,
    signature_seen: bool,
}

/// Validates a Messages SSE stream (notes 15 §5.7).
///
/// A stream may end with an `error` event instead of `message_stop`; nothing
/// may follow either.
pub fn validate_anthropic_stream(events: &[SseEvent]) -> Report {
    let mut errs = Errs::default();
    if events.is_empty() {
        errs.push("stream", "no events");
        return errs.finish();
    }
    let mut started = false;
    let mut stopped = false;
    let mut failed = false;
    let mut open: Option<OpenBlock> = None;
    let mut next_index = 0u64;
    let mut deltas = 0usize;
    let mut stop_reason: Option<String> = None;
    let mut tool_blocks = 0usize;
    let mut broken_tool_json = false;
    for (i, event) in events.iter().enumerate() {
        let path = format!("event[{i}]");
        if stopped || failed {
            errs.push(&path, "event after the end of the stream");
            continue;
        }
        if event.data.trim() == "[DONE]" {
            errs.push(&path, "Messages streams have no [DONE] sentinel");
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
        if !started && !matches!(kind, "message_start" | "error" | "ping") {
            errs.push(&path, format!("`{kind}` before message_start"));
        }
        match kind {
            "ping" => {}
            "message_start" => {
                if started {
                    errs.push(&path, "second message_start");
                }
                started = true;
                let path = format!("{path}.message");
                let Some(message) = map.get("message").and_then(|m| object(m, &path, &mut errs))
                else {
                    errs.push(&path, "missing");
                    continue;
                };
                if !message
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.starts_with("msg_"))
                {
                    errs.push(&path, "id must begin with `msg_`");
                }
                if message.get("type").and_then(Value::as_str) != Some("message") {
                    errs.push(&path, "type must be `message`");
                }
                if message.get("role").and_then(Value::as_str) != Some("assistant") {
                    errs.push(&path, "role must be `assistant`");
                }
                req_nonempty(message, "model", &path, &mut errs);
                if message
                    .get("content")
                    .and_then(Value::as_array)
                    .is_none_or(|c| !c.is_empty())
                {
                    errs.push(&path, "content must be an empty array");
                }
                if !message.get("stop_reason").is_some_and(Value::is_null) {
                    errs.push(&path, "stop_reason must be null in message_start");
                }
                match message.get("usage") {
                    Some(usage) => check_usage(usage, &format!("{path}.usage"), &mut errs),
                    None => errs.push(&path, "missing usage"),
                }
            }
            "content_block_start" => {
                let index = map.get("index").and_then(Value::as_u64);
                if let Some(block) = &open {
                    errs.push(
                        &path,
                        format!("block {index:?} starts while block {} is open", block.index),
                    );
                }
                if deltas > 0 {
                    errs.push(&path, "content block after message_delta");
                }
                if index != Some(next_index) {
                    errs.push(
                        &path,
                        format!("block index {index:?}, expected {next_index}"),
                    );
                }
                next_index += 1;
                let block = map.get("content_block").cloned().unwrap_or(Value::Null);
                let kind =
                    check_response_block(&block, &format!("{path}.content_block"), &mut errs)
                        .to_string();
                match kind.as_str() {
                    "text" if block.get("text").and_then(Value::as_str) != Some("") => {
                        errs.push(&path, "a text block starts empty");
                    }
                    "tool_use" => {
                        tool_blocks += 1;
                        if block
                            .get("input")
                            .and_then(Value::as_object)
                            .is_none_or(|input| !input.is_empty())
                        {
                            errs.push(&path, "a tool_use block starts with an empty input");
                        }
                    }
                    _ => {}
                }
                open = Some(OpenBlock {
                    index: index.unwrap_or(0),
                    kind,
                    json: String::new(),
                    signature_seen: false,
                });
            }
            "content_block_delta" => {
                let index = map.get("index").and_then(Value::as_u64);
                let Some(block) = &mut open else {
                    errs.push(&path, "delta without an open block");
                    continue;
                };
                if index != Some(block.index) {
                    errs.push(
                        &path,
                        format!(
                            "delta for block {index:?} while block {} is open",
                            block.index
                        ),
                    );
                }
                let delta = map.get("delta").cloned().unwrap_or(Value::Null);
                let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
                if block.signature_seen {
                    errs.push(&path, "delta after signature_delta");
                }
                let fits = match delta_type {
                    "text_delta" => {
                        if !delta.get("text").is_some_and(Value::is_string) {
                            errs.push(&path, "text_delta.text must be a string");
                        }
                        block.kind == "text"
                    }
                    "citations_delta" => block.kind == "text",
                    "thinking_delta" => {
                        if !delta.get("thinking").is_some_and(Value::is_string) {
                            errs.push(&path, "thinking_delta.thinking must be a string");
                        }
                        block.kind == "thinking"
                    }
                    "signature_delta" => {
                        match delta.get("signature").and_then(Value::as_str) {
                            Some(signature) if !signature.is_empty() => {}
                            _ => errs.push(&path, "signature_delta without a signature"),
                        }
                        block.signature_seen = true;
                        block.kind == "thinking"
                    }
                    "input_json_delta" => {
                        match delta.get("partial_json").and_then(Value::as_str) {
                            Some(fragment) => block.json.push_str(fragment),
                            None => {
                                errs.push(&path, "input_json_delta.partial_json must be a string")
                            }
                        }
                        block.kind == "tool_use" || block.kind == "server_tool_use"
                    }
                    other => {
                        errs.push(&path, format!("unknown delta type `{other}`"));
                        true
                    }
                };
                if !fits {
                    errs.push(&path, format!("`{delta_type}` in a `{}` block", block.kind));
                }
            }
            "content_block_stop" => {
                let index = map.get("index").and_then(Value::as_u64);
                match open.take() {
                    Some(block) => {
                        if index != Some(block.index) {
                            errs.push(
                                &path,
                                format!(
                                    "stop for block {index:?} while block {} is open",
                                    block.index
                                ),
                            );
                        }
                        if block.kind == "tool_use"
                            && !block.json.is_empty()
                            && !is_json_object_text(&block.json)
                        {
                            broken_tool_json = true;
                        }
                    }
                    None => errs.push(&path, "content_block_stop without an open block"),
                }
            }
            "message_delta" => {
                if let Some(block) = &open {
                    errs.push(
                        &path,
                        format!("message_delta while block {} is open", block.index),
                    );
                }
                deltas += 1;
                match map.get("delta").and_then(|d| d.get("stop_reason")) {
                    Some(Value::String(reason)) => {
                        if !STOP_REASONS.contains(&reason.as_str()) {
                            errs.push(&path, format!("invalid stop_reason `{reason}`"));
                        }
                        stop_reason = Some(reason.clone());
                    }
                    Some(Value::Null) => {}
                    other => errs.push(
                        &path,
                        format!("delta.stop_reason must be a string or null, found {other:?}"),
                    ),
                }
                match map.get("usage").and_then(Value::as_object) {
                    Some(usage) => {
                        req_uint(usage, "output_tokens", &path, &mut errs);
                    }
                    None => errs.push(&path, "message_delta without usage"),
                }
            }
            "message_stop" => {
                stopped = true;
                if let Some(block) = &open {
                    errs.push(
                        &path,
                        format!("message_stop while block {} is open", block.index),
                    );
                }
                if deltas == 0 {
                    errs.push(&path, "message_stop without a message_delta before it");
                }
            }
            "error" => {
                failed = true;
                let error = map.get("error");
                if error
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    errs.push(&path, "error event without error.type");
                }
                if error
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    errs.push(&path, "error event without error.message");
                }
            }
            other => errs.push(&path, format!("unexpected event type `{other}`")),
        }
    }
    if failed {
        return errs.finish();
    }
    if !stopped {
        errs.push("stream", "does not end with message_stop");
    }
    match stop_reason.as_deref() {
        Some("tool_use") => {
            if tool_blocks == 0 {
                errs.push(
                    "stream",
                    "stop_reason is tool_use but no tool_use block was streamed",
                );
            }
            if broken_tool_json {
                errs.push(
                    "stream",
                    "stop_reason is tool_use but a tool input is not a complete JSON object",
                );
            }
        }
        Some("end_turn") if tool_blocks > 0 => {
            errs.push(
                "stream",
                "tool_use blocks were streamed but stop_reason is end_turn",
            );
        }
        _ => {}
    }
    errs.finish()
}
