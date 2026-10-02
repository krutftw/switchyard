//! Conversions between Messages content blocks and canonical [`Part`]s that
//! are shared by the request, response and stream code.

use crate::util::{
    THIS, base64_decode_text, base64_encode, non_empty, sniff_image_type, str_field,
};
use serde_json::{Map, Value, json};
use switchyard_core::ir::{
    Citation, MediaPart, MediaSource, OpaquePart, Part, Reasoning, Signature, TextPart, ToolCall,
    ToolCallKind, ToolResult, parse_data_uri,
};
use switchyard_core::sig;

/// Where a signature string was read from, which decides how it is tagged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SigSource {
    /// A client request: the blob may be wrapped (`sy1.<tag>.…`).
    Client,
    /// An upstream response: the blob is native to this protocol.
    Upstream,
}

pub(crate) fn make_signature(raw: &str, source: SigSource) -> Signature {
    match source {
        SigSource::Client => sig::decode_from_client(raw, THIS),
        SigSource::Upstream => Signature::new(THIS, raw),
    }
}

/// The `cache_control` marker of a block, kept verbatim.
pub(crate) fn cache_control(block: &Value) -> Option<Value> {
    block
        .get("cache_control")
        .filter(|v| v.is_object())
        .cloned()
}

/// Wraps a block the IR does not model so it can be replayed verbatim to an
/// Anthropic upstream and is dropped for everyone else.
pub(crate) fn opaque(block: &Value) -> Part {
    Part::Opaque(OpaquePart {
        origin: THIS,
        raw: block.clone(),
    })
}

// ---------------------------------------------------------------------------
// Wire -> IR
// ---------------------------------------------------------------------------

/// Decodes one citation attached to a text block.
///
/// Anthropic's index fields (`start_char_index`, page numbers, block indices)
/// address the *source document*, whereas [`Citation::start`]/[`Citation::end`]
/// address the generated text, so they are not mapped.
pub(crate) fn decode_citation(value: &Value) -> Option<Citation> {
    if !value.is_object() {
        return None;
    }
    let url = non_empty(value, "url")
        .or_else(|| non_empty(value, "source"))
        .map(str::to_string);
    let title = non_empty(value, "title")
        .or_else(|| non_empty(value, "document_title"))
        .map(str::to_string);
    let cited_text = non_empty(value, "cited_text").map(str::to_string);
    if url.is_none() && title.is_none() && cited_text.is_none() {
        return None;
    }
    Some(Citation {
        url,
        title,
        cited_text,
        start: None,
        end: None,
    })
}

/// Renders a citation for a client. Web citations become
/// `web_search_result_location`; citations that only quote a source become
/// `char_location` over the quoted text. The gateway cannot mint the
/// `encrypted_index` the API issues, so it is left empty: such citations are
/// for display and cannot be replayed upstream (the request encoder drops
/// citations for that reason).
pub(crate) fn encode_citation(citation: &Citation) -> Option<Value> {
    let cited = citation.cited_text.as_deref().unwrap_or("");
    if let Some(url) = &citation.url {
        return Some(json!({
            "type": "web_search_result_location",
            "url": url,
            "title": citation.title,
            "encrypted_index": "",
            "cited_text": cited,
        }));
    }
    if citation.cited_text.is_none() && citation.title.is_none() {
        return None;
    }
    Some(json!({
        "type": "char_location",
        "cited_text": cited,
        "document_index": 0,
        "document_title": citation.title,
        "start_char_index": 0,
        "end_char_index": cited.chars().count(),
    }))
}

fn decode_text(block: &Value) -> Option<Part> {
    let text = str_field(block, "text").unwrap_or("");
    let citations: Vec<Citation> = block
        .get("citations")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(decode_citation).collect())
        .unwrap_or_default();
    // The API rejects empty text blocks, so nothing is lost by skipping them.
    if text.is_empty() && citations.is_empty() {
        return None;
    }
    Some(Part::Text(TextPart {
        text: text.to_string(),
        cache_control: cache_control(block),
        citations,
        signature: None,
    }))
}

fn media_from_source(source: &Value, default_type: Option<&str>) -> Option<MediaPart> {
    let media_type = non_empty(source, "media_type")
        .or(default_type)
        .map(str::to_string);
    let source = match str_field(source, "type").unwrap_or("") {
        "base64" => MediaSource::Base64 {
            data: non_empty(source, "data")?.to_string(),
        },
        "url" => {
            let url = non_empty(source, "url")?;
            // Some clients put a data URI in a url source.
            if let Some((media_type, data)) = parse_data_uri(url) {
                return Some(MediaPart::base64(media_type, data));
            }
            MediaSource::Url {
                url: url.to_string(),
            }
        }
        "file" => MediaSource::FileRef {
            id: non_empty(source, "file_id")?.to_string(),
        },
        _ => return None,
    };
    Some(MediaPart {
        source,
        media_type,
        filename: None,
        detail: None,
        cache_control: None,
    })
}

fn decode_image(block: &Value) -> Part {
    let media = block
        .get("source")
        .and_then(|source| media_from_source(source, None))
        // Non-standard top-level `url`, sent by some clients.
        .or_else(|| non_empty(block, "url").map(MediaPart::from_url));
    match media {
        Some(mut media) => {
            media.cache_control = cache_control(block);
            Part::Image(media)
        }
        None => opaque(block),
    }
}

fn decode_document(block: &Value) -> Part {
    let Some(source) = block.get("source") else {
        return opaque(block);
    };
    let media = match str_field(source, "type").unwrap_or("") {
        "base64" => media_from_source(source, Some("application/pdf")),
        // URL document sources are PDFs; saying so helps other encoders.
        "url" | "file" => media_from_source(source, None).map(|mut media| {
            if media.media_type.is_none() && matches!(media.source, MediaSource::Url { .. }) {
                media.media_type = Some("application/pdf".to_string());
            }
            media
        }),
        // Plain text documents carry the text itself; the IR stores bytes.
        "text" => str_field(source, "data").map(|text| {
            MediaPart::base64(
                non_empty(source, "media_type").unwrap_or("text/plain"),
                base64_encode(text),
            )
        }),
        _ => None,
    };
    match media {
        Some(mut media) => {
            media.filename = non_empty(block, "title").map(str::to_string);
            media.cache_control = cache_control(block);
            Part::Document(media)
        }
        // Custom-content documents and unknown source kinds stay verbatim.
        None => opaque(block),
    }
}

/// Serialises a `tool_use.input` value as canonical argument text.
pub(crate) fn tool_input_to_arguments(input: Option<&Value>) -> String {
    match input {
        None | Some(Value::Null) => "{}".to_string(),
        // Some compatible servers send the input as a JSON string.
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

fn decode_tool_use(block: &Value) -> Part {
    Part::ToolCall(ToolCall {
        id: str_field(block, "id").unwrap_or("").to_string(),
        name: str_field(block, "name").unwrap_or("").to_string(),
        arguments: tool_input_to_arguments(block.get("input")),
        kind: ToolCallKind::Function,
        signature: None,
        cache_control: cache_control(block),
    })
}

fn thinking_text(block: &Value) -> String {
    match block.get("thinking") {
        Some(Value::String(text)) => text.clone(),
        // `{"thinking": {"text": …}}` is produced by a few compatible servers.
        Some(Value::Object(inner)) => inner
            .get("text")
            .or_else(|| inner.get("thinking"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => str_field(block, "text").unwrap_or("").to_string(),
    }
}

fn decode_tool_result(block: &Value) -> Part {
    let mut hoisted_cache: Option<Value> = None;
    let mut content: Vec<Part> = Vec::new();
    let mut push_block = |item: &Value, content: &mut Vec<Part>| match item {
        Value::String(text) => {
            if !text.is_empty() {
                content.push(Part::text(text.clone()));
            }
        }
        Value::Object(_) => {
            // The API rejects cache_control inside tool_result content; a
            // marker found there is moved onto the tool_result itself.
            if hoisted_cache.is_none() {
                hoisted_cache = cache_control(item);
            }
            let part = match str_field(item, "type").unwrap_or("") {
                "text" | "" => decode_text(item),
                "image" => Some(decode_image(item)),
                "document" => Some(decode_document(item)),
                _ => Some(opaque(item)),
            };
            if let Some(mut part) = part {
                clear_cache_control(&mut part);
                content.push(part);
            }
        }
        Value::Null => {}
        other => content.push(Part::text(other.to_string())),
    };
    match block.get("content") {
        Some(Value::Array(items)) => {
            for item in items {
                push_block(item, &mut content);
            }
        }
        Some(single) => push_block(single, &mut content),
        None => {}
    }
    Part::ToolResult(ToolResult {
        call_id: str_field(block, "tool_use_id").unwrap_or("").to_string(),
        name: None,
        content,
        is_error: block.get("is_error").and_then(Value::as_bool) == Some(true),
        cache_control: cache_control(block).or(hoisted_cache),
    })
}

fn clear_cache_control(part: &mut Part) {
    match part {
        Part::Text(text) => text.cache_control = None,
        Part::Image(media) | Part::Audio(media) | Part::Document(media) => {
            media.cache_control = None;
        }
        _ => {}
    }
}

/// Decodes one block of a user turn. `None` means "drop it".
pub(crate) fn decode_user_block(block: &Value) -> Option<Part> {
    match block {
        Value::String(text) if !text.is_empty() => return Some(Part::text(text.clone())),
        Value::Object(_) => {}
        _ => return None,
    }
    match str_field(block, "type").unwrap_or("") {
        "text" => decode_text(block),
        "image" => Some(decode_image(block)),
        "document" => Some(decode_document(block)),
        "tool_result" => Some(decode_tool_result(block)),
        // Assistant-only blocks in a user turn are ignored: letting a user
        // message smuggle in tool calls or reasoning would be an injection
        // vector.
        "tool_use" | "thinking" | "redacted_thinking" => None,
        "" => decode_text(block),
        _ => Some(opaque(block)),
    }
}

/// Decodes one block of an assistant turn (request history or a response).
pub(crate) fn decode_assistant_block(block: &Value, source: SigSource) -> Option<Part> {
    match block {
        Value::String(text) if !text.is_empty() => return Some(Part::text(text.clone())),
        Value::Object(_) => {}
        _ => return None,
    }
    match str_field(block, "type").unwrap_or("") {
        "text" => decode_text(block),
        "thinking" => {
            let text = thinking_text(block);
            let signature = non_empty(block, "signature").map(|raw| make_signature(raw, source));
            if text.is_empty() && signature.is_none() {
                return None;
            }
            Some(Part::Reasoning(Reasoning {
                id: None,
                text,
                signature,
                redacted: false,
            }))
        }
        "redacted_thinking" => {
            let data = non_empty(block, "data")?;
            Some(Part::Reasoning(Reasoning {
                id: None,
                text: String::new(),
                signature: Some(make_signature(data, source)),
                redacted: true,
            }))
        }
        "tool_use" => Some(decode_tool_use(block)),
        "tool_result" => None,
        "image" => Some(decode_image(block)),
        "document" => Some(decode_document(block)),
        "" => decode_text(block),
        // server_tool_use, web_search_tool_result, code execution results,
        // MCP blocks, compaction blocks and anything newer.
        _ => Some(opaque(block)),
    }
}

// ---------------------------------------------------------------------------
// IR -> wire
// ---------------------------------------------------------------------------

fn with_cache_control(mut block: Map<String, Value>, cache: Option<&Value>) -> Value {
    if let Some(cache) = cache {
        block.insert("cache_control".to_string(), cache.clone());
    }
    Value::Object(block)
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Whether text may be sent as a `text` block. The API refuses blocks whose
/// text is empty or nothing but whitespace ("text content blocks must
/// contain non-whitespace text"), in messages and in `system` alike.
pub(crate) fn is_sendable_text(text: &str) -> bool {
    !text.trim().is_empty()
}

/// `{"type":"text","text":…}` for a request; `None` for text the API would
/// refuse (see [`is_sendable_text`]). Such parts are ordinary in histories
/// of other protocols — a Chat Completions assistant message carrying
/// `"\n\n"` next to its tool calls, for instance — and say nothing, so they
/// are left out rather than failing the request.
pub(crate) fn encode_text(text: &str, cache: Option<&Value>) -> Option<Value> {
    if !is_sendable_text(text) {
        return None;
    }
    Some(with_cache_control(
        object(json!({"type": "text", "text": text})),
        cache,
    ))
}

/// Encodes an image part. `allow_file_ref` is true only when the request came
/// from an Anthropic client, because file ids are private to the vendor that
/// issued them.
pub(crate) fn encode_image(
    media: &MediaPart,
    allow_file_ref: bool,
    with_cache: bool,
) -> Option<Value> {
    let source = match &media.source {
        // An image without bytes is refused by the API.
        MediaSource::Base64 { data } if data.trim().is_empty() => return None,
        MediaSource::Base64 { data } => json!({
            "type": "base64",
            "media_type": media.media_type.as_deref().unwrap_or_else(|| sniff_image_type(data)),
            "data": data,
        }),
        MediaSource::Url { url } => match parse_data_uri(url) {
            Some((media_type, data)) => json!({
                "type": "base64", "media_type": media_type, "data": data,
            }),
            None => json!({"type": "url", "url": url}),
        },
        MediaSource::FileRef { id } => {
            if !allow_file_ref {
                return None;
            }
            json!({"type": "file", "file_id": id})
        }
    };
    let cache = media.cache_control.as_ref().filter(|_| with_cache);
    Some(with_cache_control(
        object(json!({"type": "image", "source": source})),
        cache,
    ))
}

fn is_textual(media_type: &str) -> bool {
    let media_type = media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    media_type.starts_with("text/")
        || matches!(
            media_type.as_str(),
            "application/json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/javascript"
                | "application/x-ndjson"
        )
}

/// Encodes a document part. The Messages API takes PDFs (base64, URL or file
/// id) and plain text; images arriving as documents become image blocks and
/// every other media type is dropped (`None`).
pub(crate) fn encode_document(
    media: &MediaPart,
    allow_file_ref: bool,
    with_cache: bool,
) -> Option<Value> {
    let inline: Option<(String, String)> = match &media.source {
        MediaSource::Base64 { data } => Some((
            media
                .media_type
                .clone()
                .unwrap_or_else(|| "application/pdf".to_string()),
            data.clone(),
        )),
        MediaSource::Url { url } => parse_data_uri(url),
        MediaSource::FileRef { .. } => None,
    };
    let source = match (&media.source, inline) {
        // A document without content is refused by the API.
        (_, Some((_, data))) if data.trim().is_empty() => return None,
        (_, Some((media_type, data))) => {
            let lower = media_type.to_ascii_lowercase();
            if lower.starts_with("image/") {
                let image = MediaPart {
                    source: MediaSource::Base64 { data },
                    media_type: Some(media_type),
                    ..media.clone()
                };
                return encode_image(&image, allow_file_ref, with_cache);
            } else if lower.starts_with("application/pdf") {
                json!({"type": "base64", "media_type": "application/pdf", "data": data})
            } else if is_textual(&lower) {
                let text = base64_decode_text(&data).filter(|text| is_sendable_text(text))?;
                json!({"type": "text", "media_type": "text/plain", "data": text})
            } else {
                return None;
            }
        }
        (MediaSource::Url { url }, None) => {
            // A remote image that arrived labelled as a file.
            if media
                .media_type
                .as_deref()
                .is_some_and(|kind| kind.to_ascii_lowercase().starts_with("image/"))
            {
                return encode_image(media, allow_file_ref, with_cache);
            }
            json!({"type": "url", "url": url})
        }
        (MediaSource::FileRef { id }, None) => {
            if !allow_file_ref {
                return None;
            }
            json!({"type": "file", "file_id": id})
        }
        (MediaSource::Base64 { .. }, None) => return None,
    };
    let mut block = object(json!({"type": "document", "source": source}));
    if let Some(title) = media.filename.as_deref().filter(|t| !t.is_empty()) {
        block.insert("title".to_string(), Value::String(title.to_string()));
    }
    let cache = media.cache_control.as_ref().filter(|_| with_cache);
    Some(with_cache_control(block, cache))
}

/// The `input` object of a `tool_use` block for a canonical tool call.
///
/// * function calls: the parsed arguments; a JSON `null` (how some models
///   spell "no arguments") is the empty object, and arguments that are not
///   a JSON object are wrapped by [`ToolCall::arguments_value`] so nothing is
///   lost;
/// * free-form (custom) calls carry raw text, which is wrapped as
///   `{"input": "<text>"}` to satisfy the "input is an object" rule.
pub(crate) fn tool_call_input(call: &ToolCall) -> Value {
    tool_input(call.kind, &call.arguments)
}

/// [`tool_call_input`] for arguments that are not part of a [`ToolCall`]
/// (yet): the stream encoder collects them fragment by fragment.
pub(crate) fn tool_input(kind: ToolCallKind, arguments: &str) -> Value {
    match kind {
        ToolCallKind::Custom => json!({"input": arguments}),
        ToolCallKind::Function if arguments.trim() == "null" => Value::Object(Map::new()),
        ToolCallKind::Function => ToolCall {
            id: String::new(),
            name: String::new(),
            arguments: arguments.to_string(),
            kind,
            signature: None,
            cache_control: None,
        }
        .arguments_value(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn citation_shapes_decode() {
        let web = decode_citation(&json!({
            "type": "web_search_result_location", "url": "https://e.com", "title": "E",
            "encrypted_index": "abc", "cited_text": "quote"
        }))
        .unwrap();
        assert_eq!(web.url.as_deref(), Some("https://e.com"));
        assert_eq!(web.title.as_deref(), Some("E"));
        assert_eq!(web.cited_text.as_deref(), Some("quote"));
        let doc = decode_citation(&json!({
            "type": "char_location", "cited_text": "q", "document_index": 0,
            "document_title": "Doc", "start_char_index": 5, "end_char_index": 6
        }))
        .unwrap();
        assert_eq!(doc.title.as_deref(), Some("Doc"));
        assert_eq!((doc.start, doc.end), (None, None));
        let search = decode_citation(&json!({
            "type": "search_result_location", "source": "https://s.com", "title": "S",
            "cited_text": "x"
        }))
        .unwrap();
        assert_eq!(search.url.as_deref(), Some("https://s.com"));
        assert_eq!(decode_citation(&json!({"type": "char_location"})), None);
        assert_eq!(decode_citation(&json!("nope")), None);
    }

    #[test]
    fn citation_shapes_encode() {
        let web = Citation {
            url: Some("https://e.com".into()),
            title: Some("E".into()),
            cited_text: None,
            start: Some(1),
            end: Some(2),
        };
        assert_eq!(
            encode_citation(&web).unwrap(),
            json!({"type": "web_search_result_location", "url": "https://e.com", "title": "E",
                   "encrypted_index": "", "cited_text": ""})
        );
        let quote = Citation {
            cited_text: Some("héllo".into()),
            title: Some("Doc".into()),
            ..Citation::default()
        };
        assert_eq!(
            encode_citation(&quote).unwrap(),
            json!({"type": "char_location", "cited_text": "héllo", "document_index": 0,
                   "document_title": "Doc", "start_char_index": 0, "end_char_index": 5})
        );
        assert_eq!(encode_citation(&Citation::default()), None);
    }

    #[test]
    fn document_encoding_by_media_type() {
        let pdf = MediaPart::base64("application/pdf", "JVBERi0=");
        assert_eq!(
            encode_document(&pdf, false, true).unwrap(),
            json!({"type": "document", "source": {"type": "base64",
                   "media_type": "application/pdf", "data": "JVBERi0="}})
        );
        let text = MediaPart::base64("text/markdown", base64_encode("# hi"));
        assert_eq!(
            encode_document(&text, false, true).unwrap(),
            json!({"type": "document", "source": {"type": "text",
                   "media_type": "text/plain", "data": "# hi"}})
        );
        let image = MediaPart::base64("image/png", "iVBOR");
        assert_eq!(
            encode_document(&image, false, true).unwrap()["type"],
            json!("image")
        );
        let docx = MediaPart::base64("application/msword", "AAAA");
        assert_eq!(encode_document(&docx, false, true), None);
        let file = MediaPart {
            source: MediaSource::FileRef {
                id: "file_1".into(),
            },
            media_type: None,
            filename: Some("report".into()),
            detail: None,
            cache_control: None,
        };
        assert_eq!(encode_document(&file, false, true), None);
        assert_eq!(
            encode_document(&file, true, true).unwrap(),
            json!({"type": "document", "source": {"type": "file", "file_id": "file_1"},
                   "title": "report"})
        );
    }

    #[test]
    fn tool_input_is_always_an_object() {
        let function = ToolCallKind::Function;
        assert_eq!(tool_input(function, ""), json!({}));
        assert_eq!(tool_input(function, " null "), json!({}));
        assert_eq!(tool_input(function, "{\"a\": 1}"), json!({"a": 1}));
        assert_eq!(tool_input(function, "[1]"), json!({"input": [1]}));
        assert_eq!(tool_input(function, "{\"a\":"), json!({"input": "{\"a\":"}));
        assert_eq!(
            tool_input(ToolCallKind::Custom, "{}"),
            json!({"input": "{}"})
        );
        assert_eq!(tool_input(ToolCallKind::Custom, ""), json!({"input": ""}));
    }

    #[test]
    fn image_media_type_is_sniffed_when_missing() {
        let mut media = MediaPart::base64("image/png", "/9j/4AAQ");
        media.media_type = None;
        assert_eq!(
            encode_image(&media, false, true).unwrap()["source"]["media_type"],
            json!("image/jpeg")
        );
    }
}
