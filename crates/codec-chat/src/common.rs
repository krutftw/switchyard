//! Wire-level helpers shared by the request, response and stream halves of
//! the codec: finish reasons, usage, reasoning fields, tool calls and content
//! parts.

use serde_json::{Map, Value, json};
use switchyard_core::ir::{
    Citation, FinishReason, MediaPart, MediaSource, OpaquePart, Part, Reasoning, RefusalPart,
    Signature, TextPart, ToolCall, ToolCallKind, parse_data_uri,
};
use switchyard_core::util::{new_call_id, new_id, u64_field};
use switchyard_core::{Protocol, Usage, sig};

/// The protocol this crate implements.
pub(crate) const PROTOCOL: Protocol = Protocol::OpenaiChat;

/// Customary prefix of a Chat Completions response id.
pub(crate) const ID_PREFIX: &str = "chatcmpl-";

/// Which side of the gateway a payload came from. Decides how opaque blobs
/// are interpreted: blobs in a *client* request may be wrapped foreign
/// signatures, blobs in an *upstream* response are always native.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Client,
    Upstream,
}

/// Turns a raw blob string into a [`Signature`] according to where it was read.
pub(crate) fn blob(raw: &str, side: Side) -> Signature {
    match side {
        Side::Client => sig::decode_from_client(raw, PROTOCOL),
        Side::Upstream => Signature::new(PROTOCOL, raw),
    }
}

// ---------------------------------------------------------------------------
// Tolerant field access
// ---------------------------------------------------------------------------

/// A string field that is present and not empty.
pub(crate) fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// A numeric field as `f64`. JSON `null` and non-numbers are "absent".
pub(crate) fn f64_of(value: &Value, key: &str) -> Option<f64> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .filter(|f| f.is_finite())
}

/// A numeric field as `i64`, accepting integral floats (`3.0`).
pub(crate) fn i64_of(value: &Value, key: &str) -> Option<i64> {
    as_i64(value.get(key)?)
}

/// A JSON number as `i64`, accepting integral floats.
pub(crate) fn as_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| {
        v.as_f64()
            .filter(|f| f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e15)
            .map(|f| f as i64)
    })
}

/// A boolean field; anything that is not a JSON boolean is "absent".
pub(crate) fn bool_of(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

/// An Anthropic-style cache breakpoint carried on a Chat payload (an
/// extension understood by OpenRouter-like gateways). Only well-formed
/// markers are kept so garbage never reaches an Anthropic upstream.
pub(crate) fn cache_control_of(value: &Value) -> Option<Value> {
    let cc = value.get("cache_control")?;
    (cc.get("type").and_then(Value::as_str) == Some("ephemeral")).then(|| cc.clone())
}

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

/// Shapes a response id for a Chat client: ids that already look like
/// `chatcmpl-…` pass through untouched, an empty id is minted, and anything
/// else (an Anthropic `msg_…`, a Responses `resp_…`, a Gemini response id)
/// keeps its unique tail behind the customary prefix.
pub(crate) fn client_response_id(id: &str) -> String {
    if id.is_empty() {
        return new_id(ID_PREFIX);
    }
    if id.starts_with(ID_PREFIX) {
        return id.to_string();
    }
    let tail = id
        .strip_prefix("msg_")
        .or_else(|| id.strip_prefix("resp_"))
        .filter(|t| !t.is_empty())
        .unwrap_or(id);
    format!("{ID_PREFIX}{tail}")
}

// ---------------------------------------------------------------------------
// Finish reasons
// ---------------------------------------------------------------------------

/// Maps a canonical finish reason to a Chat `finish_reason`.
///
/// | IR | Chat |
/// |---|---|
/// | `Stop` | `stop` |
/// | `Length` | `length` |
/// | `ToolCalls` | `tool_calls` |
/// | `ContentFilter` | `content_filter` |
/// | `Refusal` | `content_filter` (the answer was withheld) |
/// | `PauseTurn` | `stop` (the turn ended without an error; Chat has no "continue" signal) |
/// | `ContextWindow` | `length` (the model ran out of room) |
/// | `Error` | `length` (the output is incomplete; `stop` would claim a finished answer) |
/// | `Other(_)` | `stop` |
pub(crate) fn finish_to_wire(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop | FinishReason::PauseTurn | FinishReason::Other(_) => "stop",
        FinishReason::Length | FinishReason::ContextWindow | FinishReason::Error => "length",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::ContentFilter | FinishReason::Refusal => "content_filter",
    }
}

/// Maps a Chat `finish_reason` (including the spellings other vendors'
/// "compatible" endpoints leak through) to the canonical reason.
///
/// | Chat | IR |
/// |---|---|
/// | `stop`, `eos`, `end_turn`, `stop_sequence`, empty | `Stop` |
/// | `length`, `max_tokens`, `max_output_tokens` | `Length` |
/// | `tool_calls`, `function_call`, `tool_use` | `ToolCalls` |
/// | `content_filter`, `safety`, `recitation`, `sensitive` | `ContentFilter` |
/// | `refusal` | `Refusal` |
/// | `pause_turn` | `PauseTurn` |
/// | `model_context_window_exceeded` | `ContextWindow` |
/// | `error` | `Error` |
/// | anything else | `Other(raw)` |
pub(crate) fn finish_from_wire(raw: &str) -> FinishReason {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "stop" | "eos" | "end_turn" | "stop_sequence" => FinishReason::Stop,
        "length" | "max_tokens" | "max_output_tokens" => FinishReason::Length,
        "tool_calls" | "function_call" | "tool_use" => FinishReason::ToolCalls,
        "content_filter" | "safety" | "recitation" | "sensitive" => FinishReason::ContentFilter,
        "refusal" => FinishReason::Refusal,
        "pause_turn" => FinishReason::PauseTurn,
        "model_context_window_exceeded" => FinishReason::ContextWindow,
        "error" => FinishReason::Error,
        _ => FinishReason::Other(raw.to_string()),
    }
}

/// Reconciles an upstream finish reason with what was actually generated.
/// Many compatible servers report `stop` for a turn that ends in tool calls,
/// and a few report `tool_calls` without sending any.
pub(crate) fn reconcile_finish(reason: FinishReason, has_tool_calls: bool) -> FinishReason {
    match reason {
        FinishReason::Stop if has_tool_calls => FinishReason::ToolCalls,
        FinishReason::ToolCalls if !has_tool_calls => FinishReason::Stop,
        other => other,
    }
}

/// Whether the argument text of a function call is a finished document: no
/// text at all (a call without arguments) or complete JSON. Whitespace only,
/// or JSON that stops in the middle, is what an upstream leaves behind when
/// it runs out of tokens while writing the call.
pub(crate) fn function_arguments_complete(arguments: &str) -> bool {
    arguments.is_empty() || serde_json::from_str::<serde::de::IgnoredAny>(arguments).is_ok()
}

/// The finish reason of a turn decoded from an upstream, given the upstream's
/// own reason (if it sent one) and the tool calls of the turn as
/// `(kind, arguments)`.
///
/// On top of [`reconcile_finish`]: a turn that would be reported as
/// `ToolCalls` is reported as `Length` when any function call's arguments
/// are a cut-off document (see [`function_arguments_complete`]). Compatible
/// servers label a max-token cut `stop`, and relays append `[DONE]` to
/// streams they cut short; a client must not execute a half-written call.
/// Custom (free-text) calls cannot be judged and are exempt; `length`,
/// `content_filter` and every other explicit reason are kept.
pub(crate) fn upstream_finish<'a>(
    reason: Option<FinishReason>,
    calls: impl Iterator<Item = (ToolCallKind, &'a str)>,
) -> FinishReason {
    let mut has_calls = false;
    let mut cut_off = false;
    for (kind, arguments) in calls {
        has_calls = true;
        cut_off |= kind == ToolCallKind::Function && !function_arguments_complete(arguments);
    }
    match reconcile_finish(reason.unwrap_or(FinishReason::Stop), has_calls) {
        FinishReason::ToolCalls if cut_off => FinishReason::Length,
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

/// The first of the candidate fields that holds a non-zero count. Servers
/// that report a figure under two names sometimes leave one of them at zero.
fn first_u64(candidates: &[(Option<&Value>, &str)]) -> u64 {
    candidates
        .iter()
        .filter_map(|(holder, key)| holder.and_then(|h| u64_field(h, key)))
        .find(|n| *n > 0)
        .unwrap_or(0)
}

/// Reads a Chat `usage` object. `prompt_tokens` includes cached tokens and
/// `completion_tokens` includes reasoning tokens, so the totals are converted
/// to the disjoint canonical buckets with [`Usage::from_inclusive`].
///
/// Besides the OpenAI field names it accepts what compatible vendors add:
/// DeepSeek's `prompt_cache_hit_tokens`, Anthropic-style
/// `cache_read_input_tokens` / `cache_creation_input_tokens`, and
/// Responses-style `input_tokens` / `output_tokens`.
///
/// Some compatible reasoning endpoints (Google's among them) report the
/// thinking tokens *next to* `completion_tokens` instead of inside it. The
/// body itself shows which convention is in use: `total_tokens ==
/// prompt + completion + reasoning` cannot hold when a non-zero reasoning
/// count is part of the completion count, and neither can
/// `reasoning > completion`. In that case the reasoning tokens are added to
/// the output total so they are not lost from accounting.
pub(crate) fn usage_from_wire(usage: &Value) -> Option<Usage> {
    if !usage.is_object() {
        return None;
    }
    let u = Some(usage);
    let prompt_details = usage
        .get("prompt_tokens_details")
        .or_else(|| usage.get("input_tokens_details"));
    let completion_details = usage
        .get("completion_tokens_details")
        .or_else(|| usage.get("output_tokens_details"));

    let prompt = first_u64(&[(u, "prompt_tokens"), (u, "input_tokens")]);
    let completion = first_u64(&[(u, "completion_tokens"), (u, "output_tokens")]);
    let cache_read = first_u64(&[
        (prompt_details, "cached_tokens"),
        (u, "prompt_cache_hit_tokens"),
        (u, "cache_read_input_tokens"),
    ]);
    let cache_write = first_u64(&[
        (prompt_details, "cache_write_tokens"),
        (prompt_details, "cache_creation_tokens"),
        (prompt_details, "cached_creation_tokens"),
        (u, "cache_creation_input_tokens"),
    ]);
    let reasoning = first_u64(&[
        (completion_details, "reasoning_tokens"),
        (u, "reasoning_tokens"),
    ]);
    let total = first_u64(&[(u, "total_tokens")]);
    let separate_sum = prompt.saturating_add(completion).saturating_add(reasoning);
    let reasoning_is_separate = reasoning > 0
        && (total == separate_sum
            || (reasoning > completion && (total == 0 || total > separate_sum)));
    let output = if reasoning_is_separate {
        completion.saturating_add(reasoning)
    } else {
        completion
    };
    Some(Usage::from_inclusive(
        prompt,
        cache_read,
        cache_write,
        output,
        reasoning,
    ))
}

/// Renders canonical usage as a Chat `usage` object (inclusive totals).
pub(crate) fn usage_to_wire(usage: &Usage) -> Value {
    let mut prompt_details = Map::new();
    prompt_details.insert("cached_tokens".into(), json!(usage.cache_read_tokens));
    if usage.cache_write_tokens > 0 {
        prompt_details.insert("cache_write_tokens".into(), json!(usage.cache_write_tokens));
    }
    json!({
        "prompt_tokens": usage.prompt_tokens(),
        "completion_tokens": usage.output_tokens,
        "total_tokens": usage.total_tokens(),
        "prompt_tokens_details": Value::Object(prompt_details),
        "completion_tokens_details": {
            "reasoning_tokens": usage.reasoning_tokens.min(usage.output_tokens)
        }
    })
}

// ---------------------------------------------------------------------------
// Reasoning
// ---------------------------------------------------------------------------

/// Flattens the loosely specified reasoning text fields of compatible
/// servers: a string is itself, an object contributes its `text`, an array is
/// flattened in order.
pub(crate) fn reasoning_text(value: Option<&Value>) -> String {
    fn walk(v: &Value, out: &mut String) {
        match v {
            Value::String(s) => out.push_str(s),
            Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            Value::Object(o) => {
                if let Some(Value::String(s)) = o.get("text").or_else(|| o.get("summary")) {
                    out.push_str(s);
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    if let Some(v) = value {
        walk(v, &mut out);
    }
    out
}

/// One entry of an OpenRouter-style `reasoning_details` array.
pub(crate) struct ReasoningDetail {
    pub index: Option<i64>,
    pub id: Option<String>,
    pub text: String,
    /// Signature of a text entry or payload of an encrypted entry.
    pub blob: Option<String>,
    /// `reasoning.encrypted`: the text was withheld.
    pub encrypted: bool,
}

/// Parses a `reasoning_details` value into its usable entries.
pub(crate) fn reasoning_details(value: Option<&Value>) -> Vec<ReasoningDetail> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        if !item.is_object() {
            continue;
        }
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        let index = i64_of(item, "index");
        let id = str_of(item, "id").map(str::to_string);
        let data = str_of(item, "data");
        if kind == "reasoning.encrypted" || (data.is_some() && item.get("text").is_none()) {
            if let Some(data) = data {
                out.push(ReasoningDetail {
                    index,
                    id,
                    text: String::new(),
                    blob: Some(data.to_string()),
                    encrypted: true,
                });
            }
            continue;
        }
        let text = str_of(item, "text")
            .or_else(|| str_of(item, "summary"))
            .unwrap_or("")
            .to_string();
        let blob = str_of(item, "signature").map(str::to_string);
        if text.is_empty() && blob.is_none() {
            continue;
        }
        out.push(ReasoningDetail {
            index,
            id,
            text,
            blob,
            encrypted: false,
        });
    }
    out
}

/// Extracts the reasoning of a complete message (a response message or an
/// assistant turn in request history).
///
/// `reasoning_details` gives the block structure and the signatures; the
/// plain-text spellings (`reasoning_content`, then `reasoning`) supply the
/// text when the details carry none. Entries sharing an `index` are fragments
/// of one block (clients that replay concatenated stream deltas) and are
/// merged.
pub(crate) fn reasoning_from_message(msg: &Value, side: Side) -> Vec<Reasoning> {
    let mut parts: Vec<(Option<i64>, Reasoning)> = Vec::new();
    for d in reasoning_details(msg.get("reasoning_details")) {
        if d.encrypted {
            parts.push((
                None,
                Reasoning {
                    id: d.id,
                    text: String::new(),
                    signature: d.blob.as_deref().map(|b| blob(b, side)),
                    redacted: true,
                },
            ));
            continue;
        }
        match parts.last_mut() {
            Some((idx, last)) if !last.redacted && d.index.is_some() && *idx == d.index => {
                last.text.push_str(&d.text);
                if let Some(b) = &d.blob {
                    last.signature = Some(blob(b, side));
                }
                if last.id.is_none() {
                    last.id = d.id;
                }
            }
            _ => parts.push((
                d.index,
                Reasoning {
                    id: d.id,
                    text: d.text,
                    signature: d.blob.as_deref().map(|b| blob(b, side)),
                    redacted: false,
                },
            )),
        }
    }
    let mut parts: Vec<Reasoning> = parts.into_iter().map(|(_, r)| r).collect();

    if parts.iter().all(|p| p.text.is_empty()) {
        let mut text = reasoning_text(msg.get("reasoning_content"));
        if text.is_empty() {
            text = reasoning_text(msg.get("reasoning"));
        }
        if !text.is_empty() {
            match parts.iter_mut().find(|p| !p.redacted) {
                Some(p) => p.text = text,
                None => parts.insert(
                    0,
                    Reasoning {
                        text,
                        ..Reasoning::default()
                    },
                ),
            }
        }
    }
    parts.retain(|p| !p.text.is_empty() || p.signature.is_some());
    parts
}

/// Writes reasoning parts onto an assistant message object:
/// `reasoning_content` holds the visible text (blocks joined with `sep`) and,
/// only when at least one blob survives `render_blob`, `reasoning_details`
/// carries the block structure with the blobs.
pub(crate) fn write_reasoning_fields<'a>(
    message: &mut Map<String, Value>,
    parts: impl Iterator<Item = &'a Reasoning>,
    sep: &str,
    render_blob: impl Fn(&Signature) -> Option<String>,
) {
    let mut text = String::new();
    let mut details: Vec<Value> = Vec::new();
    let mut any_blob = false;
    for (index, part) in parts.enumerate() {
        let rendered = part.signature.as_ref().and_then(&render_blob);
        if part.redacted {
            // A redacted block is nothing but its payload.
            if let Some(data) = rendered {
                any_blob = true;
                let mut d = Map::new();
                d.insert("type".into(), json!("reasoning.encrypted"));
                d.insert("data".into(), json!(data));
                if let Some(id) = &part.id {
                    d.insert("id".into(), json!(id));
                }
                d.insert("index".into(), json!(index));
                details.push(Value::Object(d));
            }
            continue;
        }
        if !part.text.is_empty() {
            if !text.is_empty() {
                text.push_str(sep);
            }
            text.push_str(&part.text);
        }
        let mut d = Map::new();
        d.insert("type".into(), json!("reasoning.text"));
        d.insert("text".into(), json!(part.text));
        if let Some(signature) = rendered {
            any_blob = true;
            d.insert("signature".into(), json!(signature));
        }
        if let Some(id) = &part.id {
            d.insert("id".into(), json!(id));
        }
        d.insert("index".into(), json!(index));
        details.push(Value::Object(d));
    }
    if !text.is_empty() {
        message.insert("reasoning_content".into(), json!(text));
    }
    if any_blob {
        message.insert("reasoning_details".into(), Value::Array(details));
    }
}

// ---------------------------------------------------------------------------
// Tool calls
// ---------------------------------------------------------------------------

/// Where a Gemini thought signature may sit on a Chat tool call. Google's
/// OpenAI-compatible endpoint uses `extra_content.google.thought_signature`.
pub(crate) fn thought_signature(tool_call: &Value) -> Option<&str> {
    fn nested(holder: &Value) -> Option<&Value> {
        holder
            .get("extra_content")?
            .get("google")?
            .get("thought_signature")
    }
    nested(tool_call)
        .or_else(|| tool_call.get("function").and_then(nested))
        .or_else(|| tool_call.get("thought_signature"))
        .or_else(|| tool_call.get("thoughtSignature"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Argument text of a function call: a string is taken verbatim, an object
/// (some servers skip the string encoding) is serialised, anything else is
/// empty.
pub(crate) fn arguments_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Decodes one complete `tool_calls[]` entry (function or custom). Entries
/// without a name are unusable and yield `None`.
pub(crate) fn tool_call_from_wire(tc: &Value, side: Side) -> Option<ToolCall> {
    if !tc.is_object() {
        return None;
    }
    let kind_str = tc.get("type").and_then(Value::as_str).unwrap_or("");
    let custom = tc.get("custom").filter(|c| c.is_object());
    let function = tc.get("function").filter(|f| f.is_object());
    let (kind, name, arguments) =
        if let Some(custom) = custom.filter(|_| function.is_none() || kind_str == "custom") {
            (
                ToolCallKind::Custom,
                str_of(custom, "name")?,
                arguments_text(custom.get("input")),
            )
        } else {
            // A flat `{name, arguments}` entry is tolerated as a function call.
            let f = function.unwrap_or(tc);
            (
                ToolCallKind::Function,
                str_of(f, "name")?,
                arguments_text(f.get("arguments")),
            )
        };
    Some(ToolCall {
        id: str_of(tc, "id")
            .map(str::to_string)
            .unwrap_or_else(new_call_id),
        name: name.to_string(),
        arguments,
        kind,
        signature: thought_signature(tc).map(|s| blob(s, side)),
        cache_control: None,
    })
}

/// Renders a tool call for a `tool_calls` array. `signature` is the already
/// rendered thought signature, if one may be sent.
pub(crate) fn tool_call_to_wire(call: &ToolCall, signature: Option<String>) -> Value {
    let id = if call.id.is_empty() {
        new_call_id()
    } else {
        call.id.clone()
    };
    let mut out = Map::new();
    out.insert("id".into(), json!(id));
    match call.kind {
        ToolCallKind::Function => {
            // An empty argument string means "no arguments"; strict servers
            // insist on a JSON document.
            let arguments = if call.arguments.trim().is_empty() {
                "{}"
            } else {
                call.arguments.as_str()
            };
            out.insert("type".into(), json!("function"));
            out.insert(
                "function".into(),
                json!({"name": call.name, "arguments": arguments}),
            );
        }
        ToolCallKind::Custom => {
            out.insert("type".into(), json!("custom"));
            out.insert(
                "custom".into(),
                json!({"name": call.name, "input": call.arguments}),
            );
        }
    }
    if let Some(signature) = signature {
        out.insert(
            "extra_content".into(),
            json!({"google": {"thought_signature": signature}}),
        );
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// Media
// ---------------------------------------------------------------------------

/// `input_audio.format` -> media type.
pub(crate) fn audio_mime(format: &str) -> String {
    match format.trim().to_ascii_lowercase().as_str() {
        "" | "wav" => "audio/wav".to_string(),
        "mp3" => "audio/mpeg".to_string(),
        "pcm16" => "audio/pcm".to_string(),
        "g711_ulaw" | "g711_alaw" => "audio/basic".to_string(),
        other => format!("audio/{other}"),
    }
}

/// Media type -> `input_audio.format`.
pub(crate) fn audio_format(mime: Option<&str>) -> String {
    let mime = mime.unwrap_or("audio/wav").trim().to_ascii_lowercase();
    let mime = mime.split(';').next().unwrap_or("").trim();
    match mime {
        "audio/wav" | "audio/wave" | "audio/x-wav" => "wav".to_string(),
        "audio/mpeg" | "audio/mp3" => "mp3".to_string(),
        "audio/pcm" | "audio/l16" => "pcm16".to_string(),
        other => other
            .strip_prefix("audio/")
            .filter(|s| !s.is_empty())
            .unwrap_or("wav")
            .to_string(),
    }
}

/// Media type guessed from a file name, for `file_data` that is bare base64
/// instead of a data URI.
pub(crate) fn mime_from_filename(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mpeg" | "mpg" => "video/mpeg",
        "avi" => "video/x-msvideo",
        "mkv" => "video/x-matroska",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => return None,
    })
}

/// A file name for media that has none (`file` parts want one).
pub(crate) fn default_filename(mime: Option<&str>) -> &'static str {
    match mime.unwrap_or("").split(';').next().unwrap_or("").trim() {
        "application/pdf" => "document.pdf",
        "text/plain" => "document.txt",
        "text/markdown" => "document.md",
        "text/csv" => "document.csv",
        "application/json" => "document.json",
        "application/xml" | "text/xml" => "document.xml",
        "text/html" => "document.html",
        _ => "document",
    }
}

/// Files are classified by media type so that an image uploaded through a
/// `file` part reaches other protocols as an image, not as a document.
/// Video has no variant of its own in the IR and is filed under
/// [`Part::Document`] with its `video/*` media type.
fn media_part(media: MediaPart) -> Part {
    let mt = media
        .media_type
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    if mt.starts_with("image/") {
        Part::Image(media)
    } else if mt.starts_with("audio/") {
        Part::Audio(media)
    } else {
        Part::Document(media)
    }
}

// ---------------------------------------------------------------------------
// Content parts
// ---------------------------------------------------------------------------

fn text_part(text: &str, holder: &Value) -> Option<Part> {
    // Empty text blocks are rejected by stricter protocols; they carry nothing.
    if text.is_empty() {
        return None;
    }
    Some(Part::Text(TextPart {
        text: text.to_string(),
        cache_control: cache_control_of(holder),
        citations: Vec::new(),
        signature: None,
    }))
}

/// Whether a media type names a video.
pub(crate) fn is_video(media_type: Option<&str>) -> bool {
    media_type.is_some_and(|mt| {
        mt.get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("video/"))
    })
}

/// `image_url.detail` as Chat defines it: `auto`, `low` or `high`.
/// Responses clients may also send `original` (full resolution, i.e. `high`
/// here); any other level has no Chat equivalent and is left out so the
/// upstream applies its default instead of rejecting the enum value.
fn image_detail(detail: &str) -> Option<&'static str> {
    match detail.trim().to_ascii_lowercase().as_str() {
        "auto" => Some("auto"),
        "low" => Some("low"),
        "high" | "original" => Some("high"),
        _ => None,
    }
}

/// A `video_url` part: `{"type":"video_url","video_url":{"url":…}}` (the
/// URL may also be given directly as a string). Understood by Chat-compatible
/// vision servers (Qwen, GLM, vLLM, OpenRouter), not by OpenAI itself.
///
/// The IR has no video variant; like the Gemini codec this files the clip
/// under [`Part::Document`] with a `video/*` media type, which is what lets
/// other codecs forward it and [`user_part_to_wire`] turn it back into a
/// `video_url` part. A remote URL does not say what it points to, so its
/// type is taken from the file extension, defaulting to `video/mp4`.
fn video_from_wire(item: &Value) -> Option<Part> {
    let url = part_url(item, "video_url")?;
    let mut media = MediaPart::from_url(url);
    if !is_video(media.media_type.as_deref()) && matches!(media.source, MediaSource::Url { .. }) {
        let guessed = mime_from_url(url).filter(|mt| is_video(Some(mt)));
        media.media_type = Some(guessed.unwrap_or("video/mp4").to_string());
    }
    media.cache_control = cache_control_of(item);
    Some(media_part(media))
}

/// An `audio_url` part (vLLM, Qwen): audio given as a data URI or a remote
/// URL instead of OpenAI's base64-only `input_audio`.
fn audio_url_from_wire(item: &Value) -> Option<Part> {
    let url = part_url(item, "audio_url")?;
    let mut media = MediaPart::from_url(url);
    if matches!(media.source, MediaSource::Url { .. }) {
        media.media_type = mime_from_url(url)
            .filter(|mt| mt.starts_with("audio/"))
            .map(str::to_string);
    }
    media.cache_control = cache_control_of(item);
    Some(Part::Audio(media))
}

/// The URL of a `{"type": "<key>", "<key>": {"url": …}}` part; the URL may
/// also be given directly as a string.
fn part_url<'a>(item: &'a Value, key: &str) -> Option<&'a str> {
    let spec = item.get(key)?;
    match spec {
        Value::String(url) => Some(url.as_str()).filter(|url| !url.is_empty()),
        Value::Object(_) => str_of(spec, "url"),
        _ => None,
    }
}

/// Media type suggested by the file extension of a URL's path.
fn mime_from_url(url: &str) -> Option<&'static str> {
    mime_from_filename(url.split(['?', '#']).next().unwrap_or(url))
}

fn image_from_wire(item: &Value) -> Option<Part> {
    let spec = item.get("image_url")?;
    let (url, detail) = match spec {
        Value::String(url) => (url.as_str(), str_of(item, "detail")),
        Value::Object(_) => (str_of(spec, "url")?, str_of(spec, "detail")),
        _ => return None,
    };
    if url.is_empty() {
        return None;
    }
    let mut media = MediaPart::from_url(url);
    media.detail = detail.map(str::to_string);
    media.cache_control = cache_control_of(item);
    Some(Part::Image(media))
}

fn audio_from_wire(item: &Value) -> Option<Part> {
    let spec = item
        .get("input_audio")
        .filter(|s| s.is_object())
        .unwrap_or(item);
    let data = str_of(spec, "data")?;
    let format = spec.get("format").and_then(Value::as_str).unwrap_or("");
    let mut media = MediaPart::base64(audio_mime(format), data);
    media.cache_control = cache_control_of(item);
    Some(Part::Audio(media))
}

fn file_from_wire(item: &Value) -> Option<Part> {
    // Chat nests the fields under `file`; Responses-style `input_file` parts
    // (sent by clients that mix the two APIs) are flat.
    let spec = item.get("file").filter(|s| s.is_object()).unwrap_or(item);
    let filename = str_of(spec, "filename").map(str::to_string);
    let cache_control = cache_control_of(item);
    let media = if let Some(data) = str_of(spec, "file_data") {
        match parse_data_uri(data) {
            Some((media_type, payload)) => MediaPart {
                source: MediaSource::Base64 { data: payload },
                media_type: Some(media_type),
                filename,
                detail: None,
                cache_control,
            },
            None => MediaPart {
                source: MediaSource::Base64 {
                    data: data.to_string(),
                },
                media_type: filename
                    .as_deref()
                    .and_then(mime_from_filename)
                    .map(str::to_string),
                filename,
                detail: None,
                cache_control,
            },
        }
    } else if let Some(id) = str_of(spec, "file_id") {
        MediaPart {
            source: MediaSource::FileRef { id: id.to_string() },
            media_type: filename
                .as_deref()
                .and_then(mime_from_filename)
                .map(str::to_string),
            filename,
            detail: None,
            cache_control,
        }
    } else if let Some(url) = str_of(spec, "file_url") {
        MediaPart {
            source: MediaSource::Url {
                url: url.to_string(),
            },
            media_type: filename
                .as_deref()
                .and_then(mime_from_filename)
                .map(str::to_string),
            filename,
            detail: None,
            cache_control,
        }
    } else {
        return None;
    };
    Some(media_part(media))
}

/// Decodes one element of a `content` array.
fn part_from_wire(item: &Value) -> Option<Part> {
    match item {
        Value::String(s) => return text_part(s, &Value::Null),
        Value::Object(_) => {}
        _ => return None,
    }
    let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "text" | "input_text" | "output_text" => {
            text_part(item.get("text").and_then(Value::as_str).unwrap_or(""), item)
        }
        "image_url" | "input_image" => image_from_wire(item),
        "input_audio" => audio_from_wire(item),
        "video_url" | "input_video" => video_from_wire(item),
        "audio_url" => audio_url_from_wire(item),
        "file" | "input_file" => file_from_wire(item),
        "refusal" => str_of(item, "refusal").map(|text| {
            Part::Refusal(RefusalPart {
                text: text.to_string(),
            })
        }),
        "" => match item.get("text") {
            // `{"text": "…"}` without a type is unambiguous enough.
            Some(Value::String(text)) => text_part(text, item),
            _ => None,
        },
        // Part types this codec does not model (vendor extensions) survive
        // a Chat-to-Chat translation verbatim.
        _ => Some(Part::Opaque(OpaquePart {
            origin: PROTOCOL,
            raw: item.clone(),
        })),
    }
}

/// Decodes a message `content` value: `null`, a string, an array of parts,
/// or (leniently) a single part object or a scalar.
pub(crate) fn parts_from_content(content: Option<&Value>) -> Vec<Part> {
    match content {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => text_part(s, &Value::Null).into_iter().collect(),
        Some(Value::Array(items)) => items.iter().filter_map(part_from_wire).collect(),
        Some(obj @ Value::Object(o)) => {
            if o.contains_key("type") || o.get("text").is_some_and(Value::is_string) {
                part_from_wire(obj).into_iter().collect()
            } else {
                // Structured tool output sent without string encoding.
                vec![Part::text(obj.to_string())]
            }
        }
        Some(scalar) => vec![Part::text(scalar.to_string())],
    }
}

/// Gives a message-level `cache_control` marker to the last part that can
/// hold one and has none of its own (part-level markers win).
pub(crate) fn apply_message_cache_control(message: &Value, parts: &mut [Part]) {
    let Some(cc) = cache_control_of(message) else {
        return;
    };
    for part in parts.iter_mut().rev() {
        let slot = match part {
            Part::Text(t) => &mut t.cache_control,
            Part::Image(m) | Part::Audio(m) | Part::Document(m) => &mut m.cache_control,
            Part::ToolCall(c) => &mut c.cache_control,
            Part::ToolResult(r) => &mut r.cache_control,
            Part::Reasoning(_) | Part::Refusal(_) | Part::Opaque(_) => continue,
        };
        if slot.is_none() {
            *slot = Some(cc);
        }
        return;
    }
}

/// `{"type":"text","text":…}`
pub(crate) fn text_to_wire(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// Renders one part of a user message. `openai_source` says whether provider
/// file handles in the request were issued by OpenAI (they are meaningless to
/// it otherwise). Parts Chat cannot express yield `None`.
///
/// `cache_control` markers are dropped: OpenAI rejects unknown fields on
/// content parts and caches automatically.
///
/// Media with a `video/*` type (whatever variant another codec filed it
/// under) is sent as a `video_url` part, the only form Chat-compatible
/// servers read a clip from; a `file` part named "document" would not be
/// recognised as a video by any of them.
pub(crate) fn user_part_to_wire(part: &Part, openai_source: bool) -> Option<Value> {
    if let Part::Image(m) | Part::Audio(m) | Part::Document(m) = part
        && is_video(m.media_type.as_deref())
        && !matches!(m.source, MediaSource::FileRef { .. })
    {
        return Some(json!({"type": "video_url", "video_url": {"url": m.as_url()?}}));
    }
    match part {
        Part::Text(t) => Some(text_to_wire(&t.text)),
        Part::Image(m) => match &m.source {
            MediaSource::FileRef { id } => {
                openai_source.then(|| json!({"type": "file", "file": {"file_id": id}}))
            }
            _ => {
                let mut image = Map::new();
                image.insert("url".into(), json!(m.as_url()?));
                if let Some(detail) = m.detail.as_deref().and_then(image_detail) {
                    image.insert("detail".into(), json!(detail));
                }
                Some(json!({"type": "image_url", "image_url": Value::Object(image)}))
            }
        },
        Part::Audio(m) => match &m.source {
            MediaSource::Base64 { data } => Some(json!({
                "type": "input_audio",
                "input_audio": {"data": data, "format": audio_format(m.media_type.as_deref())}
            })),
            // `input_audio` is base64-only. Servers that fetch audio
            // themselves read `audio_url`; the others reject the part,
            // which beats answering as if no audio had been attached.
            MediaSource::Url { url } => {
                Some(json!({"type": "audio_url", "audio_url": {"url": url}}))
            }
            MediaSource::FileRef { .. } => None,
        },
        Part::Document(m) => match &m.source {
            MediaSource::Base64 { .. } => {
                let filename = m
                    .filename
                    .clone()
                    .unwrap_or_else(|| default_filename(m.media_type.as_deref()).to_string());
                Some(json!({
                    "type": "file",
                    "file": {"filename": filename, "file_data": m.as_url()?}
                }))
            }
            MediaSource::FileRef { id } => {
                openai_source.then(|| json!({"type": "file", "file": {"file_id": id}}))
            }
            // Chat cannot fetch documents by URL. Naming the file lets the
            // model say it cannot read it instead of answering as if nothing
            // had been attached.
            MediaSource::Url { url } => Some(text_to_wire(&format!("[File: {url}]"))),
        },
        Part::Opaque(o) if o.origin == PROTOCOL => Some(o.raw.clone()),
        Part::Opaque(_)
        | Part::ToolCall(_)
        | Part::ToolResult(_)
        | Part::Reasoning(_)
        | Part::Refusal(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Citations and generated images
// ---------------------------------------------------------------------------

/// `message.annotations` (`url_citation` entries) -> citations.
pub(crate) fn citations_from_wire(annotations: Option<&Value>) -> Vec<Citation> {
    let Some(Value::Array(items)) = annotations else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|a| {
            let c = a.get("url_citation").filter(|c| c.is_object()).unwrap_or(a);
            let url = str_of(c, "url")?;
            Some(Citation {
                url: Some(url.to_string()),
                title: str_of(c, "title").map(str::to_string),
                cited_text: None,
                start: u64_field(c, "start_index"),
                end: u64_field(c, "end_index"),
            })
        })
        .collect()
}

/// A citation as a `url_citation` annotation. `offset` is the number of
/// characters of message content preceding the text part the citation
/// belongs to. Citations without a URL cannot be expressed.
pub(crate) fn citation_to_wire(citation: &Citation, offset: u64) -> Option<Value> {
    let url = citation.url.as_deref()?;
    let mut c = Map::new();
    c.insert("url".into(), json!(url));
    c.insert(
        "title".into(),
        json!(citation.title.as_deref().unwrap_or("")),
    );
    if let Some(start) = citation.start {
        c.insert("start_index".into(), json!(start + offset));
    }
    if let Some(end) = citation.end {
        c.insert("end_index".into(), json!(end + offset));
    }
    Some(json!({"type": "url_citation", "url_citation": Value::Object(c)}))
}

/// `message.images` / `delta.images` (an extension used by OpenRouter and
/// Google's compatible endpoint for generated images) -> image parts.
pub(crate) fn images_from_wire(images: Option<&Value>) -> Vec<Part> {
    let Some(Value::Array(items)) = images else {
        return Vec::new();
    };
    items.iter().filter_map(image_from_wire).collect()
}

/// A generated image as an `images[]` entry.
pub(crate) fn image_to_wire(media: &MediaPart, index: usize) -> Option<Value> {
    Some(json!({
        "type": "image_url",
        "image_url": {"url": media.as_url()?},
        "index": index
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn response_ids_keep_native_shape_and_reprefix_foreign_ones() {
        assert_eq!(client_response_id("chatcmpl-abc"), "chatcmpl-abc");
        assert_eq!(client_response_id("msg_01XYZ"), "chatcmpl-01XYZ");
        assert_eq!(client_response_id("resp_68af"), "chatcmpl-68af");
        assert_eq!(client_response_id("gen-17"), "chatcmpl-gen-17");
        let minted = client_response_id("");
        assert!(minted.starts_with("chatcmpl-") && minted.len() == 9 + 24);
    }

    #[test]
    fn audio_formats_round_trip() {
        for format in ["wav", "mp3", "pcm16", "flac", "ogg"] {
            assert_eq!(audio_format(Some(&audio_mime(format))), format);
        }
        assert_eq!(audio_mime(""), "audio/wav");
        assert_eq!(audio_format(None), "wav");
        assert_eq!(audio_format(Some("audio/x-wav")), "wav");
    }

    #[test]
    fn reasoning_text_flattens_every_shape() {
        assert_eq!(reasoning_text(Some(&json!("abc"))), "abc");
        assert_eq!(reasoning_text(Some(&json!({"text": "abc"}))), "abc");
        assert_eq!(
            reasoning_text(Some(&json!(["a", {"text": "b"}, [{"text": "c"}], 4]))),
            "abc"
        );
        assert_eq!(reasoning_text(Some(&Value::Null)), "");
        assert_eq!(reasoning_text(None), "");
    }

    #[test]
    fn filename_media_types() {
        assert_eq!(mime_from_filename("Report.PDF"), Some("application/pdf"));
        assert_eq!(mime_from_filename("noext"), None);
        assert_eq!(default_filename(Some("application/pdf")), "document.pdf");
        assert_eq!(default_filename(None), "document");
    }
}
