//! `GenerateContentResponse`: decoding an upstream's answer and rendering a
//! canonical response for a Gemini client. The pieces shared with the stream
//! codecs (usage, finish reasons, part rendering, grounding) live here too.

use crate::error::clean_message;
use crate::names::ClientNames;
use crate::parts::{
    CANDIDATE_METADATA, call_args_text, call_name, candidate_metadata, decode_file_data,
    decode_inline_data, encode_media, explicit_id, is_metadata_only, opaque, response_signature,
    signature_for_client,
};
use crate::raw::unwrap_envelope;
use crate::util::{
    byte_to_char_offset, char_to_byte_offset, parse_rfc3339, pick, pick_in, pick_str, pick_u64,
};
use serde_json::{Map, Value, json};
use switchyard_core::ir::{
    Citation, FinishReason, OpaquePart, Part, Reasoning, RefusalPart, Response, Signature,
    TextPart, ToolCall, ToolCallKind,
};
use switchyard_core::util::{new_call_id, new_id};
use switchyard_core::{ClientCtx, CodecError, Family, Protocol, Usage};

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

/// `usageMetadata` of a response or stream chunk, in either spelling.
pub(crate) fn usage_metadata(body: &Value) -> Option<&Value> {
    pick(body, &["usageMetadata", "usage_metadata"])
}

/// Converts Gemini's token accounting into the canonical buckets.
///
/// Gemini's `promptTokenCount` includes cached tokens (and tool-use prompt
/// tokens are reported next to it), while `candidatesTokenCount` *excludes*
/// thought tokens. Canonically `output_tokens` is everything generated, so
/// the two are added.
pub(crate) fn decode_usage(meta: &Value) -> Usage {
    let count = |camel: &str, snake: &str| pick_u64(meta, &[camel, snake]).unwrap_or(0);
    let prompt = count("promptTokenCount", "prompt_token_count").saturating_add(count(
        "toolUsePromptTokenCount",
        "tool_use_prompt_token_count",
    ));
    let cached = count("cachedContentTokenCount", "cached_content_token_count");
    let candidates = count("candidatesTokenCount", "candidates_token_count");
    let thoughts = count("thoughtsTokenCount", "thoughts_token_count");
    let total = pick_u64(meta, &["totalTokenCount", "total_token_count"]);
    // Servers that imitate Gemini sometimes fold the thoughts into the
    // candidates count. The total gives them away; do not count twice.
    let folded =
        thoughts > 0 && thoughts <= candidates && total == Some(prompt.saturating_add(candidates));
    let output = if folded {
        candidates
    } else {
        candidates.saturating_add(thoughts)
    };
    Usage::from_inclusive(prompt, cached, 0, output, thoughts)
}

/// Renders canonical usage in Gemini's convention (see [`decode_usage`]).
pub(crate) fn encode_usage(usage: &Usage) -> Value {
    let mut meta = Map::new();
    meta.insert(
        "promptTokenCount".to_string(),
        Value::from(usage.prompt_tokens()),
    );
    if usage.cache_read_tokens > 0 {
        meta.insert(
            "cachedContentTokenCount".to_string(),
            Value::from(usage.cache_read_tokens),
        );
    }
    meta.insert(
        "candidatesTokenCount".to_string(),
        Value::from(usage.visible_output_tokens()),
    );
    if usage.reasoning_tokens > 0 {
        meta.insert(
            "thoughtsTokenCount".to_string(),
            Value::from(usage.reasoning_tokens),
        );
    }
    meta.insert(
        "totalTokenCount".to_string(),
        Value::from(usage.total_tokens()),
    );
    Value::Object(meta)
}

// ---------------------------------------------------------------------------
// Finish reasons
// ---------------------------------------------------------------------------

/// Finish reasons Gemini defines, for passing an unmapped one back through.
const KNOWN_REASONS: [&str; 22] = [
    "FINISH_REASON_UNSPECIFIED",
    "STOP",
    "MAX_TOKENS",
    "SAFETY",
    "RECITATION",
    "LANGUAGE",
    "OTHER",
    "BLOCKLIST",
    "PROHIBITED_CONTENT",
    "SPII",
    "MALFORMED_FUNCTION_CALL",
    "IMAGE_SAFETY",
    "IMAGE_PROHIBITED_CONTENT",
    "IMAGE_OTHER",
    "NO_IMAGE",
    "IMAGE_RECITATION",
    "UNEXPECTED_TOOL_CALL",
    "TOO_MANY_TOOL_CALLS",
    "MISSING_THOUGHT_SIGNATURE",
    "MALFORMED_RESPONSE",
    "ESCALATION",
    "PUP_LIMITED_DISABLED",
];

/// Gemini `finishReason` -> canonical. Gemini has no tool-call reason; the
/// caller turns `Stop` into `ToolCalls` when the candidate called a function.
pub(crate) fn decode_finish(reason: &str) -> FinishReason {
    let upper = reason.trim().to_ascii_uppercase();
    match upper.as_str() {
        "" | "STOP" | "FINISH_REASON_UNSPECIFIED" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY"
        | "RECITATION"
        | "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION" => FinishReason::ContentFilter,
        "MALFORMED_FUNCTION_CALL"
        | "UNEXPECTED_TOOL_CALL"
        | "TOO_MANY_TOOL_CALLS"
        | "MISSING_THOUGHT_SIGNATURE"
        | "MALFORMED_RESPONSE" => FinishReason::Error,
        _ => FinishReason::Other(upper),
    }
}

/// Canonical finish reason -> Gemini `finishReason`. A turn that ends in
/// function calls is a plain `STOP` for Gemini.
///
/// `refused`: the turn carries a refusal the model wrote out. The two OpenAI
/// protocols report one differently (Chat Completions as `stop` next to
/// `message.refusal`, Responses as a `completed` response with a refusal
/// part), which decode to `Stop` and `Refusal`. A Gemini client is told
/// `SAFETY` for both, so the same answer reads the same whichever upstream
/// gave it.
pub(crate) fn encode_finish(reason: &FinishReason, refused: bool) -> String {
    match reason {
        FinishReason::Stop if refused => "SAFETY".to_string(),
        FinishReason::Stop | FinishReason::ToolCalls | FinishReason::PauseTurn => {
            "STOP".to_string()
        }
        FinishReason::Length | FinishReason::ContextWindow => "MAX_TOKENS".to_string(),
        FinishReason::ContentFilter | FinishReason::Refusal => "SAFETY".to_string(),
        FinishReason::Error => "OTHER".to_string(),
        FinishReason::Other(other) => {
            let upper = other.trim().to_ascii_uppercase();
            if KNOWN_REASONS.contains(&upper.as_str()) && upper != "FINISH_REASON_UNSPECIFIED" {
                upper
            } else {
                "OTHER".to_string()
            }
        }
    }
}

/// The finish reason of a candidate that produced function calls: Gemini says
/// `STOP` (or runs into the token limit after emitting complete calls), the
/// canonical model says `ToolCalls`. Safety blocks and errors stay.
pub(crate) fn finish_with_calls(reason: FinishReason, saw_call: bool) -> FinishReason {
    match reason {
        FinishReason::Stop | FinishReason::Length | FinishReason::Other(_) if saw_call => {
            FinishReason::ToolCalls
        }
        other => other,
    }
}

/// Text shown for a prompt Gemini refused to process.
pub(crate) fn block_message(feedback: &Value) -> Option<String> {
    let reason = pick_str(feedback, &["blockReason", "block_reason"]).filter(|r| !r.is_empty())?;
    let message = pick_str(feedback, &["blockReasonMessage", "block_reason_message"])
        .filter(|m| !m.is_empty());
    Some(match message {
        Some(message) => message.to_string(),
        None => format!("The prompt was blocked by Gemini ({reason})."),
    })
}

// ---------------------------------------------------------------------------
// Grounding
// ---------------------------------------------------------------------------

/// A grounding source: its URL and its title.
pub(crate) type Source = (Option<String>, Option<String>);

/// One `groundingSupports` entry resolved against `groundingChunks`: the
/// sources, and the UTF-8 **byte** range of the part text they support.
pub(crate) struct Support {
    pub(crate) part_index: usize,
    pub(crate) start: Option<usize>,
    pub(crate) end: Option<usize>,
    pub(crate) text: Option<String>,
    pub(crate) sources: Vec<Source>,
}

fn grounding_sources(meta: &Value) -> Vec<Source> {
    pick(meta, &["groundingChunks", "grounding_chunks"])
        .and_then(Value::as_array)
        .map(|chunks| {
            chunks
                .iter()
                .map(|chunk| {
                    let source = pick(
                        chunk,
                        &["web", "retrievedContext", "retrieved_context", "maps"],
                    );
                    let field = |name: &str| {
                        source
                            .and_then(|s| s.get(name))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    };
                    (field("uri"), field("title"))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Reads the supports of a `groundingMetadata` object. When it lists sources
/// but no supports, the second value holds the bare sources.
pub(crate) fn grounding_supports(meta: &Value) -> (Vec<Support>, Vec<Source>) {
    let sources = grounding_sources(meta);
    let mut supports = Vec::new();
    for support in pick(meta, &["groundingSupports", "grounding_supports"])
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let segment = pick(support, &["segment"]);
        let index = |camel: &str, snake: &str| {
            segment
                .and_then(|s| pick_u64(s, &[camel, snake]))
                .and_then(|n| usize::try_from(n).ok())
        };
        let cited: Vec<Source> = pick(
            support,
            &["groundingChunkIndices", "grounding_chunk_indices"],
        )
        .and_then(Value::as_array)
        .map(|indices| {
            indices
                .iter()
                .filter_map(|i| i.as_u64().and_then(|i| sources.get(i as usize)).cloned())
                .collect()
        })
        .unwrap_or_default();
        if cited.is_empty() {
            continue;
        }
        let end = index("endIndex", "end_index");
        supports.push(Support {
            part_index: index("partIndex", "part_index").unwrap_or(0),
            // proto3 JSON omits a zero start.
            start: end.map(|_| index("startIndex", "start_index").unwrap_or(0)),
            end,
            text: segment
                .and_then(|s| s.get("text"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            sources: cited,
        });
    }
    (supports, sources)
}

/// Builds the citations of one support for a text whose bytes
/// `[base, base + len)` came from the supported Gemini part. Gemini's offsets
/// are bytes; canonical citation offsets are characters.
pub(crate) fn support_citations(support: &Support, text: &str, base: usize) -> Vec<Citation> {
    let offset =
        |byte: Option<usize>| byte.and_then(|b| byte_to_char_offset(text, base.saturating_add(b)));
    let (start, end) = match (offset(support.start), offset(support.end)) {
        (Some(start), Some(end)) => (Some(start), Some(end)),
        _ => (None, None),
    };
    support
        .sources
        .iter()
        .map(|(url, title)| Citation {
            url: url.clone(),
            title: title.clone(),
            cited_text: support.text.clone(),
            start,
            end,
        })
        .collect()
}

/// Collects citations while a response is rendered and turns them into a
/// `groundingMetadata` object, for responses whose citations did not come
/// from Gemini.
#[derive(Default)]
pub(crate) struct GroundingBuilder {
    chunks: Vec<Value>,
    supports: Vec<Value>,
}

impl GroundingBuilder {
    /// Records a citation on the text of the part at `part_index`.
    pub(crate) fn add(&mut self, citation: &Citation, text: &str, part_index: usize) {
        if citation.url.is_none() && citation.title.is_none() {
            return;
        }
        let mut web = Map::new();
        if let Some(url) = &citation.url {
            web.insert("uri".to_string(), Value::String(url.clone()));
        }
        if let Some(title) = &citation.title {
            web.insert("title".to_string(), Value::String(title.clone()));
        }
        let chunk = json!({"web": web});
        let chunk_index = match self.chunks.iter().position(|existing| *existing == chunk) {
            Some(index) => index,
            None => {
                self.chunks.push(chunk);
                self.chunks.len() - 1
            }
        };
        let (Some(start), Some(end)) = (citation.start, citation.end) else {
            return;
        };
        let mut segment = Map::new();
        if part_index > 0 {
            segment.insert("partIndex".to_string(), Value::from(part_index));
        }
        segment.insert(
            "startIndex".to_string(),
            Value::from(char_to_byte_offset(text, start)),
        );
        segment.insert(
            "endIndex".to_string(),
            Value::from(char_to_byte_offset(text, end)),
        );
        if let Some(cited) = &citation.cited_text {
            segment.insert("text".to_string(), Value::String(cited.clone()));
        }
        self.supports
            .push(json!({"segment": segment, "groundingChunkIndices": [chunk_index]}));
    }

    pub(crate) fn build(self) -> Option<Value> {
        if self.chunks.is_empty() {
            return None;
        }
        let mut meta = Map::new();
        meta.insert("groundingChunks".to_string(), Value::Array(self.chunks));
        if !self.supports.is_empty() {
            meta.insert("groundingSupports".to_string(), Value::Array(self.supports));
        }
        Some(Value::Object(meta))
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Folds the parts of a complete candidate into canonical parts.
#[derive(Default)]
struct Folder {
    parts: Vec<Part>,
    /// For every Gemini part: the canonical text part its text went into and
    /// the byte offset it starts at there. Grounding offsets are per part.
    text_map: Vec<Option<(usize, usize)>>,
    saw_call: bool,
}

impl Folder {
    fn push(&mut self, part: &Value) {
        let mapped = self.fold(part);
        self.text_map.push(mapped);
    }

    fn reasoning(&mut self, text: &str, signature: Option<Signature>) {
        // Consecutive thought parts are one reasoning block, up to and
        // including the part that carries the signature.
        if let Some(Part::Reasoning(open)) = self.parts.last_mut()
            && open.signature.is_none()
        {
            open.text.push_str(text);
            open.signature = signature;
            return;
        }
        if text.is_empty() && signature.is_none() {
            return;
        }
        self.parts.push(Part::Reasoning(Reasoning {
            id: None,
            text: text.to_string(),
            signature,
            redacted: false,
        }));
    }

    fn fold(&mut self, part: &Value) -> Option<(usize, usize)> {
        let map = part.as_object()?;
        let signature = response_signature(part);
        let thought = map.get("thought").and_then(Value::as_bool).unwrap_or(false);

        if let Some(call) = pick_in(map, &["functionCall", "function_call"]) {
            self.saw_call = true;
            self.parts.push(Part::ToolCall(ToolCall {
                id: explicit_id(call)
                    .map(str::to_owned)
                    .unwrap_or_else(new_call_id),
                name: call_name(call).to_string(),
                arguments: call_args_text(call),
                kind: ToolCallKind::Function,
                signature,
                cache_control: None,
            }));
            return None;
        }
        if let Some(text) = map.get("text").and_then(Value::as_str) {
            if thought {
                self.reasoning(text, signature);
                return None;
            }
            if text.is_empty() {
                // `{"text": "", "thoughtSignature": …}`: reasoning state with
                // no text of its own.
                if signature.is_some() {
                    self.reasoning("", signature);
                }
                return None;
            }
            if signature.is_none()
                && let Some(Part::Text(open)) = self.parts.last_mut()
                && open.signature.is_none()
            {
                let base = open.text.len();
                open.text.push_str(text);
                return Some((self.parts.len() - 1, base));
            }
            // A signed part is never merged with its neighbours: the
            // signature has to go back on exactly this part.
            self.parts.push(Part::Text(TextPart {
                text: text.to_string(),
                signature,
                ..TextPart::default()
            }));
            return Some((self.parts.len() - 1, 0));
        }
        if let Some(inline) = pick_in(map, &["inlineData", "inline_data"]) {
            self.parts.extend(decode_inline_data(inline));
            return None;
        }
        if let Some(file) = pick_in(map, &["fileData", "file_data"]) {
            self.parts.extend(decode_file_data(file));
            return None;
        }
        if is_metadata_only(map) {
            if signature.is_some() {
                self.reasoning("", signature);
            }
            return None;
        }
        self.parts.push(opaque(part));
        None
    }

    fn attach_grounding(&mut self, meta: &Value) {
        let (supports, sources) = grounding_supports(meta);
        let mut attached = false;
        for support in &supports {
            let Some(Some((part, base))) = self.text_map.get(support.part_index).copied() else {
                continue;
            };
            if let Some(Part::Text(text)) = self.parts.get_mut(part) {
                let citations = support_citations(support, &text.text, base);
                attached |= !citations.is_empty();
                text.citations.extend(citations);
            }
        }
        if attached || sources.is_empty() {
            return;
        }
        // Sources without supports: cite them on the answer as a whole.
        if let Some(Part::Text(text)) = self
            .parts
            .iter_mut()
            .rev()
            .find(|p| matches!(p, Part::Text(_)))
        {
            text.citations
                .extend(sources.into_iter().map(|(url, title)| Citation {
                    url,
                    title,
                    ..Citation::default()
                }));
        }
    }
}

fn metadata_spellings(key: &str) -> [&'static str; 2] {
    match key {
        "groundingMetadata" => ["groundingMetadata", "grounding_metadata"],
        "citationMetadata" => ["citationMetadata", "citation_metadata"],
        _ => ["urlContextMetadata", "url_context_metadata"],
    }
}

/// Candidate-level metadata (`groundingMetadata`, …) found on a candidate, by
/// canonical key.
pub(crate) fn candidate_metadata_of(candidate: &Value) -> Vec<(&'static str, &Value)> {
    CANDIDATE_METADATA
        .iter()
        .filter_map(|key| pick(candidate, &metadata_spellings(key)).map(|value| (*key, value)))
        .collect()
}

/// Wraps candidate-level metadata as an opaque part.
pub(crate) fn metadata_part(key: &str, value: &Value) -> Part {
    let mut raw = Map::new();
    raw.insert(key.to_string(), value.clone());
    Part::Opaque(OpaquePart {
        origin: Protocol::Gemini,
        raw: Value::Object(raw),
    })
}

/// Response id, model and creation time of a response or stream chunk.
pub(crate) fn response_head(root: &Value) -> (String, String, i64) {
    let id = pick_str(root, &["responseId", "response_id"])
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        // Gemini response ids carry no prefix.
        .unwrap_or_else(|| new_id(""));
    let model = pick_str(root, &["modelVersion", "model_version", "model"])
        .unwrap_or("")
        .to_string();
    let created = pick_str(root, &["createTime", "create_time"])
        .and_then(parse_rfc3339)
        .unwrap_or(0);
    (id, model, created)
}

/// Decodes a complete `GenerateContentResponse`. Only the first candidate is
/// used: the canonical model has one answer per response.
pub(crate) fn decode_response(body: &Value) -> Result<Response, CodecError> {
    let root = unwrap_envelope(body);
    let Some(map) = root.as_object() else {
        return Err(CodecError::upstream("expected a JSON object"));
    };
    if let Some(error) = map.get("error").filter(|e| !e.is_null()) {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(CodecError::upstream(format!(
            "error payload in a success response: {}",
            clean_message(message)
        )));
    }
    let candidates = pick_in(map, &["candidates"]);
    let feedback = pick_in(map, &["promptFeedback", "prompt_feedback"]);
    if candidates.is_none() && feedback.is_none() && usage_metadata(root).is_none() {
        return Err(CodecError::upstream(
            "not a GenerateContentResponse: no `candidates`, `promptFeedback` or `usageMetadata`",
        ));
    }

    let (id, model, created) = response_head(root);
    let mut response = Response::new(id, model);
    response.created = created;
    if let Some(meta) = usage_metadata(root) {
        response.usage = decode_usage(meta);
    }

    let candidate = candidates
        .and_then(Value::as_array)
        .and_then(|list| list.first());
    let mut folder = Folder::default();
    if let Some(candidate) = candidate {
        let parts =
            match pick(candidate, &["content"]).and_then(|content| pick(content, &["parts"])) {
                Some(Value::Array(parts)) => parts.as_slice(),
                Some(single @ Value::Object(_)) => std::slice::from_ref(single),
                _ => &[],
            };
        for part in parts {
            folder.push(part);
        }
        let metadata = candidate_metadata_of(candidate);
        for (key, value) in &metadata {
            if *key == "groundingMetadata" {
                folder.attach_grounding(value);
            }
        }
        let reason = pick_str(candidate, &["finishReason", "finish_reason"]).unwrap_or("");
        response.finish = finish_with_calls(decode_finish(reason), folder.saw_call);
        if folder.parts.is_empty()
            && response.finish == FinishReason::ContentFilter
            && let Some(message) =
                pick_str(candidate, &["finishMessage", "finish_message"]).filter(|m| !m.is_empty())
        {
            folder.parts.push(Part::Refusal(RefusalPart {
                text: message.to_string(),
            }));
        }
        for (key, value) in metadata {
            folder.parts.push(metadata_part(key, value));
        }
    } else if let Some(message) = feedback.and_then(block_message) {
        // A blocked prompt is a 200 without candidates.
        folder
            .parts
            .push(Part::Refusal(RefusalPart { text: message }));
        response.finish = FinishReason::ContentFilter;
    }
    response.parts = folder.parts;
    Ok(response)
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// The `thoughtSignature` value handed to a Gemini client: native blobs as
/// they are, foreign ones tagged with their origin (and armoured as base64,
/// which the field must be) so they are recognised when they come back.
/// `redacted` marks the payload of withheld reasoning.
fn client_signature(signature: Option<&Signature>, redacted: bool) -> Option<String> {
    signature
        .filter(|s| !s.data.is_empty())
        .map(|s| signature_for_client(s, redacted))
}

/// Renders a function call for a client: `args` is always an object and the
/// call id is passed on, so the client's `functionResponse.id` pairs the
/// result with the call exactly. The function is named the way the client
/// declared it, whatever spelling the upstream had to be given.
pub(crate) fn client_function_call(call: &ToolCall, names: &ClientNames) -> Value {
    let args = match call.kind {
        ToolCallKind::Custom => json!({"input": call.arguments}),
        ToolCallKind::Function => call.arguments_value(),
    };
    let mut function_call = Map::new();
    function_call.insert(
        "name".to_string(),
        Value::String(names.restore(&call.name).to_string()),
    );
    function_call.insert("args".to_string(), args);
    if !call.id.is_empty() {
        function_call.insert("id".to_string(), Value::String(call.id.clone()));
    }
    let mut part = Map::new();
    part.insert("functionCall".to_string(), Value::Object(function_call));
    if let Some(signature) = client_signature(call.signature.as_ref(), false) {
        part.insert("thoughtSignature".to_string(), Value::String(signature));
    }
    Value::Object(part)
}

/// Renders the thought text / signature of a reasoning part. `redacted`
/// says the provider withheld the text and the signature is its payload.
pub(crate) fn client_reasoning(
    text: &str,
    signature: Option<&Signature>,
    thought: bool,
    redacted: bool,
) -> Option<Value> {
    let signature = client_signature(signature, redacted);
    if text.is_empty() && signature.is_none() {
        return None;
    }
    let mut part = Map::new();
    part.insert("text".to_string(), Value::String(text.to_string()));
    if thought {
        part.insert("thought".to_string(), Value::Bool(true));
    }
    if let Some(signature) = signature {
        part.insert("thoughtSignature".to_string(), Value::String(signature));
    }
    Some(Value::Object(part))
}

/// Renders one canonical part as a Gemini part for a client. `None` for parts
/// Gemini cannot express (tool results, other vendors' opaque blocks) and for
/// candidate-level metadata, which does not live in `parts`.
pub(crate) fn client_part(part: &Part, names: &ClientNames) -> Option<Value> {
    match part {
        Part::Text(text) => {
            let signature = client_signature(text.signature.as_ref(), false);
            if text.text.is_empty() && signature.is_none() {
                return None;
            }
            let mut out = Map::new();
            out.insert("text".to_string(), Value::String(text.text.clone()));
            if let Some(signature) = signature {
                out.insert("thoughtSignature".to_string(), Value::String(signature));
            }
            Some(Value::Object(out))
        }
        // Signed reasoning without text is rendered as Gemini's own signature
        // carrier: an empty text part that is not flagged as a thought.
        Part::Reasoning(reasoning) => client_reasoning(
            &reasoning.text,
            reasoning.signature.as_ref(),
            !reasoning.text.is_empty(),
            reasoning.redacted,
        ),
        Part::ToolCall(call) => Some(client_function_call(call, names)),
        Part::Image(_) | Part::Audio(_) | Part::Document(_) => encode_media(part, true),
        Part::Refusal(refusal) => (!refusal.text.is_empty()).then(|| json!({"text": refusal.text})),
        Part::Opaque(opaque) => (opaque.origin.family() == Family::Google
            && opaque.raw.is_object()
            && candidate_metadata(&opaque.raw).is_none())
        .then(|| opaque.raw.clone()),
        Part::ToolResult(_) => None,
    }
}

/// Renders a complete response for a Gemini client.
pub(crate) fn encode_response(response: &Response, ctx: &ClientCtx) -> Result<Value, CodecError> {
    let names = ClientNames::from_request(&ctx.request);
    let mut parts = Vec::with_capacity(response.parts.len());
    let mut metadata: Vec<(&'static str, Value)> = Vec::new();
    let mut grounding = GroundingBuilder::default();
    for part in &response.parts {
        if let Part::Opaque(opaque) = part
            && opaque.origin.family() == Family::Google
            && let Some((key, value)) = candidate_metadata(&opaque.raw)
        {
            metadata.retain(|(existing, _)| *existing != key);
            metadata.push((key, value.clone()));
            continue;
        }
        let Some(rendered) = client_part(part, &names) else {
            continue;
        };
        if let Part::Text(text) = part {
            for citation in &text.citations {
                grounding.add(citation, &text.text, parts.len());
            }
        }
        parts.push(rendered);
    }
    // Citations that came from another vendor become grounding metadata;
    // Gemini's own metadata, when present, already says it all.
    if !metadata.iter().any(|(key, _)| *key == "groundingMetadata")
        && let Some(built) = grounding.build()
    {
        metadata.push(("groundingMetadata", built));
    }

    let mut candidate = Map::new();
    candidate.insert(
        "content".to_string(),
        json!({"parts": parts, "role": "model"}),
    );
    candidate.insert(
        "finishReason".to_string(),
        Value::String(encode_finish(
            &response.finish,
            response
                .parts
                .iter()
                .any(|part| matches!(part, Part::Refusal(refusal) if !refusal.text.is_empty())),
        )),
    );
    candidate.insert("index".to_string(), Value::from(0));
    for (key, value) in metadata {
        candidate.insert(key.to_string(), value);
    }
    let model = if ctx.model.is_empty() {
        &response.model
    } else {
        &ctx.model
    };
    let id = if response.id.is_empty() {
        new_id("")
    } else {
        response.id.clone()
    };
    Ok(json!({
        "candidates": [candidate],
        "usageMetadata": encode_usage(&response.usage),
        "modelVersion": model,
        "responseId": id,
    }))
}
