//! Helpers shared by the request, response and stream halves of the codec:
//! content-part conversion, usage conversion, identifier shaping and the
//! tool index used to restore namespaces and custom tools on the way back to
//! a client.

use serde_json::{Map, Value, json};
use std::collections::HashMap;
use switchyard_core::ir::{
    Citation, MediaPart, MediaSource, OpaquePart, Part, Reasoning, RefusalPart, Signature,
    TextPart, ToolCall, ToolCallKind, parse_data_uri,
};
use switchyard_core::protocol::Family;
use switchyard_core::util::{new_call_id, new_id, str_field, u64_field};
use switchyard_core::{Protocol, Usage, sig};

/// The protocol this crate implements.
pub(crate) const P: Protocol = Protocol::OpenaiResponses;

/// Marker put in front of a foreign `redacted_thinking` payload before it is
/// wrapped for a client. Responses has a single opaque slot per reasoning
/// item (`encrypted_content`), so "this blob *is* the reasoning" has to ride
/// inside the blob. Base64 never contains `:`, so the marker cannot collide.
const REDACTED_MARKER: &str = "redacted:";

// ---------------------------------------------------------------------------
// Small JSON accessors
// ---------------------------------------------------------------------------

/// A trimmed, non-empty string field.
pub(crate) fn non_empty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    str_field(value, key)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// A JSON number as `i64`, accepting integral floats.
pub(crate) fn i64_field(value: &Value, key: &str) -> Option<i64> {
    let v = value.get(key)?;
    v.as_i64()
        .or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
}

/// The wire `type` of an item or part (empty when absent).
pub(crate) fn type_of(value: &Value) -> &str {
    str_field(value, "type").map(str::trim).unwrap_or("")
}

/// Renders a JSON value as the string a tool-argument slot expects: strings
/// verbatim, `null`/absent as empty, anything else as compact JSON.
pub(crate) fn stringish(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

/// Converts a Responses `usage` object into disjoint IR buckets.
///
/// OpenAI reports `input_tokens` *including* cached (and cache-written)
/// tokens and `output_tokens` *including* reasoning tokens. Chat-style key
/// names are accepted too because "compatible" servers mix the two.
pub(crate) fn usage_from_wire(usage: &Value) -> Option<Usage> {
    if !usage.is_object() {
        return None;
    }
    let input = u64_field(usage, "input_tokens").or_else(|| u64_field(usage, "prompt_tokens"));
    let output =
        u64_field(usage, "output_tokens").or_else(|| u64_field(usage, "completion_tokens"));
    if input.is_none() && output.is_none() && u64_field(usage, "total_tokens").is_none() {
        return None;
    }
    let in_details = usage
        .get("input_tokens_details")
        .or_else(|| usage.get("prompt_tokens_details"));
    let out_details = usage
        .get("output_tokens_details")
        .or_else(|| usage.get("completion_tokens_details"));
    let cached = in_details
        .and_then(|d| u64_field(d, "cached_tokens"))
        .unwrap_or(0);
    let cache_write = in_details
        .and_then(|d| {
            u64_field(d, "cache_write_tokens").or_else(|| u64_field(d, "cache_creation_tokens"))
        })
        .unwrap_or(0);
    let reasoning = out_details
        .and_then(|d| u64_field(d, "reasoning_tokens"))
        .unwrap_or(0);
    Some(Usage::from_inclusive(
        input.unwrap_or(0),
        cached,
        cache_write,
        output.unwrap_or(0),
        reasoning,
    ))
}

/// Renders IR usage in the Responses convention (inclusive totals).
pub(crate) fn usage_to_wire(usage: &Usage) -> Value {
    let mut in_details = Map::new();
    in_details.insert("cached_tokens".into(), json!(usage.cache_read_tokens));
    if usage.cache_write_tokens > 0 {
        in_details.insert("cache_write_tokens".into(), json!(usage.cache_write_tokens));
    }
    // Saturating sums: the counts come from upstreams, and an absurd value
    // must not be able to overflow.
    let input = usage
        .input_tokens
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens);
    json!({
        "input_tokens": input,
        "input_tokens_details": Value::Object(in_details),
        "output_tokens": usage.output_tokens,
        "output_tokens_details": {"reasoning_tokens": usage.reasoning_tokens.min(usage.output_tokens)},
        "total_tokens": input.saturating_add(usage.output_tokens),
    })
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// 64-bit FNV-1a rendered as 16 hex characters. Used wherever an identifier
/// has to be shortened deterministically; no cryptographic strength needed.
pub(crate) fn fnv64_hex(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// OpenAI limits `call_id` to 64 characters. Longer ids (other vendors mint
/// long ones) become `<first 47 chars>_<16 hex>`, deterministically, so a
/// call and its output still pair up.
pub(crate) fn fit_call_id(id: &str) -> String {
    if id.chars().count() <= 64 {
        return id.to_string();
    }
    let head: String = id.chars().take(47).collect();
    format!("{head}_{}", fnv64_hex(id))
}

/// A response id in the `resp_…` shape. Ids that already have it are kept
/// verbatim; an empty id is minted; anything else is prefixed.
pub(crate) fn response_id(id: &str) -> String {
    let id = id.trim();
    if id.is_empty() {
        return new_id("resp_");
    }
    if id.starts_with("resp_") {
        return id.to_string();
    }
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("resp_{safe}")
}

/// The part of a response id that item ids are derived from.
pub(crate) fn id_base(response_id: &str) -> &str {
    response_id.strip_prefix("resp_").unwrap_or(response_id)
}

/// Item id of a tool call: `fc_<call_id>` / `ctc_<call_id>`. A call id that
/// already looks like an item id is reused as is.
pub(crate) fn tool_item_id(call_id: &str, custom: bool) -> String {
    let prefix = if custom { "ctc_" } else { "fc_" };
    if call_id.starts_with(prefix) {
        call_id.to_string()
    } else {
        format!("{prefix}{call_id}")
    }
}

/// The pairing id a tool call or tool output item states outright, in any of
/// the spellings in use.
pub(crate) fn stated_call_id(item: &Value) -> Option<&str> {
    ["call_id", "tool_call_id", "callId"]
        .iter()
        .find_map(|key| non_empty(item, key))
}

/// Extracts the pairing id of a tool call or tool output item. Clients spell
/// it several ways; the item's own `id` is the last resort, and the item id
/// of an *output* (`fco_…`, `ctco_…`) is never a call id.
pub(crate) fn call_id_of(item: &Value, is_output: bool) -> String {
    if let Some(id) = stated_call_id(item) {
        return id.to_string();
    }
    match non_empty(item, "id") {
        Some(id) if is_output && (id.starts_with("fco_") || id.starts_with("ctco_")) => {
            String::new()
        }
        Some(id) => id.to_string(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Namespaced tools
// ---------------------------------------------------------------------------

/// Joins a namespace and a child tool name into the flat name used in the IR.
pub(crate) fn qualify(namespace: &str, child: &str) -> String {
    let child = child.trim();
    let namespace = namespace.trim();
    if child.is_empty() || namespace.is_empty() || child.starts_with("mcp__") {
        return child.to_string();
    }
    if child == namespace || child.starts_with(&format!("{namespace}__")) {
        return child.to_string();
    }
    if namespace.ends_with("__") {
        return format!("{namespace}{child}");
    }
    format!("{namespace}__{child}")
}

/// How a flat IR tool name maps back onto what the client declared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolIdentity {
    /// Name as the client declared it (local to its namespace).
    pub name: String,
    pub namespace: Option<String>,
    /// Declared as a free-form (`custom`) tool.
    pub custom: bool,
}

/// Tool declarations of a client's Responses request, indexed by the flat
/// name the IR uses. Lets responses report `name` + `namespace` the way the
/// client declared them and turn a function-shaped call back into a
/// `custom_tool_call` when the upstream had no notion of custom tools.
#[derive(Clone, Debug, Default)]
pub(crate) struct ToolIndex {
    by_qualified: HashMap<String, ToolIdentity>,
    /// Local name → qualified name; `None` once two tools share a local name.
    by_local: HashMap<String, Option<String>>,
}

impl ToolIndex {
    pub(crate) fn from_request(request: &Value) -> Self {
        let mut index = ToolIndex::default();
        if let Some(tools) = request.get("tools").and_then(Value::as_array) {
            for tool in tools {
                index.add(tool, None);
            }
        }
        if let Some(items) = request.get("input").and_then(Value::as_array) {
            for item in items {
                if type_of(item) == "additional_tools"
                    && let Some(tools) = item.get("tools").and_then(Value::as_array)
                {
                    for tool in tools {
                        index.add(tool, None);
                    }
                }
            }
        }
        index
    }

    fn add(&mut self, tool: &Value, namespace: Option<&str>) {
        let kind = match type_of(tool) {
            "" => "function",
            other => other,
        };
        match kind {
            "namespace" => {
                let Some(ns) = non_empty(tool, "name") else {
                    return;
                };
                if let Some(children) = tool.get("tools").and_then(Value::as_array) {
                    for child in children {
                        self.add(child, Some(ns));
                    }
                }
            }
            "function" | "custom" => {
                let name = non_empty(tool, "name")
                    .or_else(|| tool.get(kind).and_then(|inner| non_empty(inner, "name")));
                let Some(name) = name else {
                    return;
                };
                let qualified = match namespace {
                    Some(ns) => qualify(ns, name),
                    None => name.to_string(),
                };
                if self.by_qualified.contains_key(&qualified) {
                    return;
                }
                self.by_local
                    .entry(name.to_string())
                    .and_modify(|slot| *slot = None)
                    .or_insert_with(|| Some(qualified.clone()));
                self.by_qualified.insert(
                    qualified,
                    ToolIdentity {
                        name: name.to_string(),
                        namespace: namespace.map(str::to_string),
                        custom: kind == "custom",
                    },
                );
            }
            _ => {}
        }
    }

    /// Finds the declaration a tool name refers to: the exact flat name, or a
    /// local name that only one declared tool carries (models sometimes
    /// answer with the bare child name of a namespaced tool).
    pub(crate) fn resolve(&self, name: &str) -> Option<&ToolIdentity> {
        self.by_qualified.get(self.flat_name(name)?)
    }

    /// The flat IR name of the declaration a tool name refers to, by the
    /// same rule as [`ToolIndex::resolve`]: an exact flat name is itself,
    /// a local name that exactly one declared tool carries is that tool, and
    /// anything else (unknown, or ambiguous between two namespaces) is not
    /// guessed at.
    pub(crate) fn flat_name<'a>(&'a self, name: &'a str) -> Option<&'a str> {
        if self.by_qualified.contains_key(name) {
            return Some(name);
        }
        match self.by_local.get(name) {
            Some(Some(qualified)) => Some(qualified.as_str()),
            _ => None,
        }
    }
}

/// Recovers the raw input of a custom tool call that travelled through a
/// function-only protocol as `{"input": "<text>"}`.
///
/// When generation was cut short inside the wrapper (`{"input":"par`) the
/// part of the input that did arrive is recovered, so a truncated call looks
/// the same whether or not the upstream knew custom tools.
pub(crate) fn unwrap_custom_input(arguments: &str) -> String {
    let wrapped = |value: Value| match value {
        Value::Object(map) => match map.get("input") {
            Some(Value::String(text)) => Some(text.clone()),
            _ => None,
        },
        _ => None,
    };
    let trimmed = arguments.trim();
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::String(text)) => return text,
        Ok(value) => return wrapped(value).unwrap_or_else(|| arguments.to_string()),
        Err(_) => {}
    }
    if !trimmed.starts_with('{') {
        return arguments.to_string();
    }
    // Close the document, dropping up to one unfinished escape sequence
    // (`\`, `\u00`, half a surrogate pair: at most 11 characters).
    let mut end = trimmed.len();
    for _ in 0..12 {
        for closer in ["\"}", "}"] {
            let candidate = format!("{}{closer}", &trimmed[..end]);
            if let Some(text) = serde_json::from_str::<Value>(&candidate)
                .ok()
                .and_then(wrapped)
            {
                return text;
            }
        }
        match trimmed[..end].char_indices().next_back() {
            Some((index, _)) if index > 0 => end = index,
            _ => break,
        }
    }
    arguments.to_string()
}

// ---------------------------------------------------------------------------
// Signatures
// ---------------------------------------------------------------------------

/// Reads `encrypted_content` sent by a client. Returns the signature and
/// whether it stands for a redacted (text-less) reasoning block.
pub(crate) fn signature_from_client(raw: &str) -> (Signature, bool) {
    let mut signature = sig::decode_from_client(raw, P);
    if signature.origin.family() != Family::Openai
        && let Some(rest) = signature.data.strip_prefix(REDACTED_MARKER)
    {
        signature.data = rest.to_string();
        return (signature, true);
    }
    (signature, false)
}

/// Renders a reasoning signature for a Responses client's
/// `encrypted_content` slot.
pub(crate) fn signature_for_client(signature: &Signature, redacted: bool) -> String {
    if redacted && signature.origin.family() != Family::Openai {
        let marked = Signature::new(
            signature.origin,
            format!("{REDACTED_MARKER}{}", signature.data),
        );
        return sig::encode_for_client(&marked, P);
    }
    sig::encode_for_client(signature, P)
}

// ---------------------------------------------------------------------------
// Content parts
// ---------------------------------------------------------------------------

/// Content-part `type`s. Anything else found at item level is an item.
pub(crate) fn is_content_part_type(kind: &str) -> bool {
    kind.starts_with("input_")
        || kind.starts_with("output_")
        || matches!(
            kind,
            "text"
                | "refusal"
                | "summary_text"
                | "reasoning_text"
                | "image_url"
                | "image"
                | "audio"
                | "video_url"
                | "file"
        )
}

fn media_type_for_filename(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "xml" => "application/xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => return None,
    })
}

fn filename_for_media_type(media_type: Option<&str>) -> &'static str {
    match media_type.unwrap_or("") {
        "application/pdf" => "document.pdf",
        "text/plain" => "document.txt",
        "text/markdown" => "document.md",
        "text/csv" => "document.csv",
        "application/json" => "document.json",
        "text/html" => "document.html",
        _ => "document",
    }
}

fn audio_media_type(format: &str) -> String {
    match format.trim().to_ascii_lowercase().as_str() {
        "" => "audio/wav".to_string(),
        "mp3" => "audio/mpeg".to_string(),
        other if other.contains('/') => other.to_string(),
        other => format!("audio/{other}"),
    }
}

fn audio_format(media_type: Option<&str>) -> String {
    match media_type.unwrap_or("audio/wav") {
        "audio/mpeg" | "audio/mp3" => "mp3".to_string(),
        "audio/x-wav" | "audio/wave" => "wav".to_string(),
        other => other.strip_prefix("audio/").unwrap_or(other).to_string(),
    }
}

fn with_cache_control(part: &Value, media: &mut MediaPart) {
    media.cache_control = part.get("cache_control").filter(|v| !v.is_null()).cloned();
}

/// Decodes one content part of a message or of a structured tool output.
/// Returns `None` for parts that carry nothing (no type and no text).
pub(crate) fn decode_content_part(part: &Value) -> Option<Part> {
    if let Value::String(text) = part {
        return Some(Part::text(text.clone()));
    }
    if !part.is_object() {
        return None;
    }
    match type_of(part) {
        "" | "input_text" | "output_text" | "text" | "summary_text" | "reasoning_text" => {
            let text = str_field(part, "text")?;
            let citations = part
                .get("annotations")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(citation_from_annotation).collect())
                .unwrap_or_default();
            Some(Part::Text(TextPart {
                text: text.to_string(),
                cache_control: part.get("cache_control").filter(|v| !v.is_null()).cloned(),
                citations,
                signature: None,
            }))
        }
        "refusal" => Some(Part::Refusal(RefusalPart {
            text: str_field(part, "refusal")
                .or_else(|| str_field(part, "text"))
                .unwrap_or("")
                .to_string(),
        })),
        "input_image" | "image_url" | "image" => {
            let url = part
                .get("image_url")
                .and_then(|v| v.as_str().or_else(|| str_field(v, "url")))
                .or_else(|| str_field(part, "url"));
            let mut media = if let Some(url) = url.filter(|u| !u.is_empty()) {
                MediaPart::from_url(url)
            } else if let Some(id) = non_empty(part, "file_id") {
                MediaPart {
                    source: MediaSource::FileRef { id: id.to_string() },
                    media_type: None,
                    filename: None,
                    detail: None,
                    cache_control: None,
                }
            } else {
                return None;
            };
            media.detail = non_empty(part, "detail")
                .or_else(|| part.get("image_url").and_then(|v| non_empty(v, "detail")))
                .map(str::to_string);
            with_cache_control(part, &mut media);
            Some(Part::Image(media))
        }
        "input_file" | "file" => {
            // Chat-style parts nest the payload under `file`.
            let inner = part.get("file").filter(|v| v.is_object()).unwrap_or(part);
            let filename = non_empty(inner, "filename").map(str::to_string);
            let mut media = if let Some(data) = non_empty(inner, "file_data") {
                match parse_data_uri(data) {
                    Some((media_type, payload)) => MediaPart::base64(media_type, payload),
                    None => MediaPart::base64(
                        filename
                            .as_deref()
                            .and_then(media_type_for_filename)
                            .unwrap_or("application/octet-stream"),
                        data,
                    ),
                }
            } else if let Some(url) = non_empty(inner, "file_url") {
                let mut media = MediaPart::url(url);
                media.media_type = filename
                    .as_deref()
                    .and_then(media_type_for_filename)
                    .map(str::to_string);
                media
            } else if let Some(id) = non_empty(inner, "file_id") {
                MediaPart {
                    source: MediaSource::FileRef { id: id.to_string() },
                    media_type: None,
                    filename: None,
                    detail: None,
                    cache_control: None,
                }
            } else {
                return None;
            };
            media.filename = filename;
            with_cache_control(part, &mut media);
            Some(Part::Document(media))
        }
        "input_audio" | "audio" => {
            let inner = part
                .get("input_audio")
                .filter(|v| v.is_object())
                .unwrap_or(part);
            let data = non_empty(inner, "data")?;
            let format = str_field(inner, "format").unwrap_or("");
            let mut media = MediaPart::base64(audio_media_type(format), data);
            with_cache_control(part, &mut media);
            Some(Part::Audio(media))
        }
        _ => Some(Part::Opaque(OpaquePart {
            origin: P,
            raw: part.clone(),
        })),
    }
}

/// Encodes a media part as an *input* content part. `openai_source` says
/// whether provider file handles in the request were issued by OpenAI (the
/// IR does not record the issuer of a [`MediaSource::FileRef`]; the protocol
/// the request arrived in is the best evidence).
pub(crate) fn encode_media_part(part: &Part, openai_source: bool) -> Option<Value> {
    match part {
        Part::Image(media) => {
            let mut out = Map::new();
            out.insert("type".into(), json!("input_image"));
            match &media.source {
                MediaSource::FileRef { id } => {
                    if !openai_source {
                        return None;
                    }
                    out.insert("file_id".into(), json!(id));
                }
                _ => {
                    out.insert("image_url".into(), json!(media.as_url()?));
                }
            }
            if let Some(detail) = &media.detail {
                out.insert("detail".into(), json!(detail));
            }
            Some(Value::Object(out))
        }
        Part::Document(media) => {
            let mut out = Map::new();
            out.insert("type".into(), json!("input_file"));
            match &media.source {
                MediaSource::Base64 { .. } => {
                    let filename = media.filename.clone().unwrap_or_else(|| {
                        filename_for_media_type(media.media_type.as_deref()).to_string()
                    });
                    out.insert("filename".into(), json!(filename));
                    out.insert("file_data".into(), json!(media.as_url()?));
                }
                MediaSource::Url { url } => {
                    out.insert("file_url".into(), json!(url));
                }
                MediaSource::FileRef { id } => {
                    if !openai_source {
                        return None;
                    }
                    out.insert("file_id".into(), json!(id));
                }
            }
            Some(Value::Object(out))
        }
        Part::Audio(media) => match &media.source {
            MediaSource::Base64 { data } => Some(json!({
                "type": "input_audio",
                "input_audio": {"data": data, "format": audio_format(media.media_type.as_deref())},
            })),
            _ => None,
        },
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Citations
// ---------------------------------------------------------------------------

/// `url_citation` annotation → IR citation. Other annotation kinds (file
/// citations, container file paths) have no cross-protocol meaning.
pub(crate) fn citation_from_annotation(annotation: &Value) -> Option<Citation> {
    if type_of(annotation) != "url_citation" {
        return None;
    }
    // Chat Completions nests the fields under `url_citation`.
    let inner = annotation
        .get("url_citation")
        .filter(|v| v.is_object())
        .unwrap_or(annotation);
    Some(Citation {
        url: non_empty(inner, "url").map(str::to_string),
        title: non_empty(inner, "title").map(str::to_string),
        cited_text: None,
        start: u64_field(inner, "start_index"),
        end: u64_field(inner, "end_index"),
    })
}

/// IR citation → `url_citation` annotation. Citations without a URL cannot
/// be expressed.
pub(crate) fn annotation_from_citation(citation: &Citation) -> Option<Value> {
    let url = citation.url.as_deref()?;
    Some(json!({
        "type": "url_citation",
        "start_index": citation.start.unwrap_or(0),
        "end_index": citation.end.unwrap_or(0),
        "url": url,
        "title": citation.title.as_deref().unwrap_or(""),
    }))
}

// ---------------------------------------------------------------------------
// Output items → IR parts (shared by the response and stream decoders)
// ---------------------------------------------------------------------------

/// Concatenates the `text` of every entry of a summary / content array.
/// A bare string is accepted because some compatible servers flatten it.
pub(crate) fn joined_texts(value: Option<&Value>, separator: &str) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(entries)) => {
            let mut out = String::new();
            for entry in entries {
                let text = match entry {
                    Value::String(text) => text.as_str(),
                    other => str_field(other, "text").unwrap_or(""),
                };
                if text.is_empty() {
                    continue;
                }
                if !out.is_empty() {
                    out.push_str(separator);
                }
                out.push_str(text);
            }
            out
        }
        _ => String::new(),
    }
}

/// Visible text of a reasoning item: the summary, or the raw reasoning
/// content when there is no summary (never both, to avoid double replay).
pub(crate) fn reasoning_item_text(item: &Value) -> String {
    let summary = joined_texts(item.get("summary"), "\n\n");
    if summary.is_empty() {
        joined_texts(item.get("content"), "\n\n")
    } else {
        summary
    }
}

/// Decodes a reasoning *output* item produced by an upstream. Returns `None`
/// when the item carries neither text nor an encrypted payload.
pub(crate) fn reasoning_from_output(item: &Value) -> Option<Reasoning> {
    let text = reasoning_item_text(item);
    let signature = non_empty(item, "encrypted_content").map(|enc| Signature::new(P, enc));
    if text.is_empty() && signature.is_none() {
        return None;
    }
    Some(Reasoning {
        id: non_empty(item, "id").map(str::to_string),
        text,
        signature,
        redacted: false,
    })
}

/// Decodes a `function_call` / `custom_tool_call` output item.
pub(crate) fn tool_call_from_output(item: &Value) -> ToolCall {
    let custom = type_of(item) == "custom_tool_call";
    let mut id = call_id_of(item, false);
    if id.is_empty() {
        id = new_call_id();
    }
    let name = str_field(item, "name").unwrap_or("").trim();
    let name = match non_empty(item, "namespace") {
        Some(ns) => qualify(ns, name),
        None => name.to_string(),
    };
    ToolCall {
        id,
        name,
        arguments: stringish(item.get(if custom { "input" } else { "arguments" })),
        kind: if custom {
            ToolCallKind::Custom
        } else {
            ToolCallKind::Function
        },
        signature: None,
        cache_control: None,
    }
}

/// Decodes one upstream output item into IR parts.
pub(crate) fn parts_from_output_item(item: &Value, out: &mut Vec<Part>) {
    match type_of(item) {
        "message" | "" if item.get("content").is_some() || type_of(item) == "message" => {
            match item.get("content") {
                Some(Value::String(text)) => out.push(Part::text(text.clone())),
                Some(Value::Array(parts)) => {
                    for part in parts {
                        // A response never contains media or foreign blocks
                        // inside a message; skip what we cannot place rather
                        // than invent an opaque part.
                        if let Some(p @ (Part::Text(_) | Part::Refusal(_))) =
                            decode_content_part(part)
                        {
                            out.push(p);
                        }
                    }
                }
                _ => {}
            }
        }
        "function_call" | "custom_tool_call" => {
            out.push(Part::ToolCall(tool_call_from_output(item)));
        }
        "reasoning" => {
            if let Some(reasoning) = reasoning_from_output(item) {
                out.push(Part::Reasoning(reasoning));
            }
        }
        "image_generation_call" if non_empty(item, "result").is_some() => {
            let format = non_empty(item, "output_format").unwrap_or("png");
            out.push(Part::Image(MediaPart::base64(
                format!("image/{format}"),
                non_empty(item, "result").unwrap_or_default(),
            )));
        }
        _ => {
            if item.is_object() {
                out.push(Part::Opaque(OpaquePart {
                    origin: P,
                    raw: item.clone(),
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualify_follows_the_namespace_rules() {
        assert_eq!(qualify("fs", "fs_read"), "fs__fs_read");
        assert_eq!(qualify("fs", "fs__read"), "fs__read");
        assert_eq!(qualify("fs", "fs"), "fs");
        assert_eq!(qualify("fs__", "read"), "fs__read");
        assert_eq!(
            qualify("mcp__node_repl", "mcp__node_repl__js"),
            "mcp__node_repl__js"
        );
        assert_eq!(
            qualify("mcp__test_mcp__", "add_numbers"),
            "mcp__test_mcp__add_numbers"
        );
        assert_eq!(qualify("", "x"), "x");
    }

    #[test]
    fn response_ids_keep_native_shape() {
        assert_eq!(response_id("resp_abc"), "resp_abc");
        assert_eq!(response_id("chatcmpl-1"), "resp_chatcmpl-1");
        assert_eq!(response_id("a b/c"), "resp_a_b_c");
        let minted = response_id("");
        assert!(minted.starts_with("resp_") && minted.len() == 29);
    }

    #[test]
    fn long_call_ids_are_shortened_deterministically() {
        let long = "x".repeat(80);
        let fitted = fit_call_id(&long);
        assert_eq!(fitted.len(), 64);
        assert_eq!(fitted, fit_call_id(&long));
        assert_eq!(fit_call_id("call_1"), "call_1");
    }

    #[test]
    fn tool_index_restores_namespace_and_custom() {
        let request = json!({
            "tools": [
                {"type": "namespace", "name": "mcp__gh", "tools": [
                    {"type": "function", "name": "get_me"},
                    {"type": "custom", "name": "exec"}
                ]},
                {"type": "function", "name": "plain"}
            ],
            "input": [{"type": "additional_tools", "tools": [{"type": "function", "name": "late"}]}]
        });
        let index = ToolIndex::from_request(&request);
        let id = index.resolve("mcp__gh__get_me").unwrap();
        assert_eq!(
            (id.name.as_str(), id.namespace.as_deref()),
            ("get_me", Some("mcp__gh"))
        );
        assert!(index.resolve("mcp__gh__exec").unwrap().custom);
        // Bare local name of a namespaced tool still resolves when unique.
        assert_eq!(
            index.resolve("exec").unwrap().namespace.as_deref(),
            Some("mcp__gh")
        );
        assert_eq!(index.resolve("plain").unwrap().namespace, None);
        assert!(index.resolve("late").is_some());
        assert!(index.resolve("missing").is_none());
    }

    #[test]
    fn custom_input_unwrapping() {
        assert_eq!(unwrap_custom_input(r#"{"input":"ls -la"}"#), "ls -la");
        assert_eq!(unwrap_custom_input("raw text"), "raw text");
        assert_eq!(unwrap_custom_input(r#"{"other":1}"#), r#"{"other":1}"#);
    }

    #[test]
    fn truncated_custom_input_is_still_unwrapped() {
        assert_eq!(unwrap_custom_input(r#"{"input":"ls -"#), "ls -");
        assert_eq!(unwrap_custom_input(r#"{"input":"ls -la""#), "ls -la");
        assert_eq!(unwrap_custom_input(r#"{ "input" : "a\nb\"#), "a\nb");
        assert_eq!(unwrap_custom_input(r#"{"input":"caf\u00"#), "caf");
        assert_eq!(unwrap_custom_input(r#"{"input":"x \ud83d\ude"#), "x ");
        assert_eq!(unwrap_custom_input(r#"{"input":"é√"#), "é√");
        // Nothing of the input arrived, or it is not the wrapper at all.
        assert_eq!(unwrap_custom_input(r#"{"input":"#), r#"{"input":"#);
        assert_eq!(unwrap_custom_input(r#"{"inp"#), r#"{"inp"#);
        assert_eq!(unwrap_custom_input(r#"{"other":"x"#), r#"{"other":"x"#);
        assert_eq!(unwrap_custom_input(""), "");
    }

    #[test]
    fn redacted_marker_round_trips_only_for_foreign_blobs() {
        let foreign = Signature::new(Protocol::Anthropic, "EqRedacted==");
        let wire = signature_for_client(&foreign, true);
        assert_eq!(wire, "sy1.a.redacted:EqRedacted==");
        assert_eq!(signature_from_client(&wire), (foreign, true));

        let native = Signature::new(P, "gAAAAnative");
        assert_eq!(signature_for_client(&native, false), "gAAAAnative");
        assert_eq!(signature_from_client("gAAAAnative"), (native, false));
    }
}
