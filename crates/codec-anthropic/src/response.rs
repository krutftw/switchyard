//! Complete (non-streamed) Messages responses: upstream body → canonical
//! [`Response`] and canonical [`Response`] → client body. The stop-reason,
//! usage and block helpers are shared with the stream code.

use crate::blocks::{
    SigSource, call_signature_block, decode_assistant_block, encode_citation, tool_call_input,
};
use crate::error::parse_error_value;
use crate::util::{THIS, non_empty, str_field, u64_field};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use switchyard_core::ir::{FinishReason, Part, Reasoning, Response, TextPart, ToolCall};
use switchyard_core::util::new_id;
use switchyard_core::{ClientCtx, CodecError, Usage, sig};

/// Prefix of Anthropic message ids.
pub(crate) const MESSAGE_ID_PREFIX: &str = "msg_";
/// Prefix of Anthropic tool-use ids.
pub(crate) const TOOL_ID_PREFIX: &str = "toolu_";

/// `service_tier` values the Messages API reports in `usage`.
const SERVICE_TIERS: &[&str] = &["standard", "priority", "batch"];

// ---------------------------------------------------------------------------
// Stop reasons
// ---------------------------------------------------------------------------

/// Maps a wire `stop_reason` onto the canonical finish reason. A missing
/// reason is inferred from the content: a pending tool call means the model
/// wants tools run.
pub(crate) fn finish_reason(stop_reason: Option<&str>, has_tool_calls: bool) -> FinishReason {
    match stop_reason
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
    {
        None if has_tool_calls => FinishReason::ToolCalls,
        None => FinishReason::Stop,
        Some("end_turn" | "stop_sequence") => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
        Some("pause_turn") => FinishReason::PauseTurn,
        Some("refusal") => FinishReason::Refusal,
        Some("model_context_window_exceeded") => FinishReason::ContextWindow,
        // Spellings leaked by "Anthropic-compatible" servers that front
        // other vendors.
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls" | "function_call") => FinishReason::ToolCalls,
        Some("content_filter" | "sensitive") => FinishReason::ContentFilter,
        Some(other) => FinishReason::Other(other.to_string()),
    }
}

/// What the content of a response says about how it ended, used to pick the
/// wire `stop_reason` for a canonical [`FinishReason::Stop`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StopHints {
    /// A stop sequence was reported.
    pub(crate) stop_sequence: bool,
    /// A client `tool_use` block was emitted.
    pub(crate) tool_use: bool,
    /// A refusal part was emitted.
    pub(crate) refusal: bool,
}

/// Maps a canonical finish reason onto the wire `stop_reason`.
///
/// * `Stop` becomes `refusal` when the model refused, `tool_use` when a tool
///   call is pending (Messages clients key their tool loop on it and some
///   upstreams report a plain stop next to tool calls), `stop_sequence` when
///   one matched, `end_turn` otherwise;
/// * `ContentFilter` has no Messages spelling and is reported as `refusal`;
/// * reasons without an equivalent become `end_turn`, or `tool_use` when a
///   tool call is pending (an upstream that ends a tool turn with a reason
///   of its own must not make the client skip the call);
/// * `Error` yields `None`: the caller decides how to report a failed
///   generation (`null` in a complete response, an `error` event in a stream).
pub(crate) fn stop_reason(finish: &FinishReason, hints: StopHints) -> Option<&'static str> {
    Some(match finish {
        FinishReason::Stop if hints.refusal => "refusal",
        FinishReason::Stop if hints.tool_use => "tool_use",
        FinishReason::Stop if hints.stop_sequence => "stop_sequence",
        FinishReason::Stop => "end_turn",
        FinishReason::Length => "max_tokens",
        FinishReason::ToolCalls => "tool_use",
        FinishReason::ContentFilter | FinishReason::Refusal => "refusal",
        FinishReason::PauseTurn => "pause_turn",
        FinishReason::ContextWindow => "model_context_window_exceeded",
        FinishReason::Error => return None,
        FinishReason::Other(_) if hints.tool_use => "tool_use",
        FinishReason::Other(_) => "end_turn",
    })
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

/// Reads a Messages `usage` object into the canonical disjoint buckets.
///
/// Anthropic's own convention already is disjoint (`input_tokens` excludes
/// cache reads and writes), so the fields map one to one. A body that only
/// has OpenAI's `prompt_tokens` / `completion_tokens` (compatible gateways)
/// is converted from the inclusive convention instead.
pub(crate) fn decode_usage(usage: &Value) -> Usage {
    if !usage.is_object() {
        return Usage::default();
    }
    let input = u64_field(usage, "input_tokens");
    let output = u64_field(usage, "output_tokens");
    if input.is_none() && output.is_none() {
        let prompt = u64_field(usage, "prompt_tokens");
        let completion = u64_field(usage, "completion_tokens");
        if prompt.is_some() || completion.is_some() {
            let cached = usage
                .get("prompt_tokens_details")
                .and_then(|details| u64_field(details, "cached_tokens"))
                .unwrap_or(0);
            let reasoning = usage
                .get("completion_tokens_details")
                .and_then(|details| u64_field(details, "reasoning_tokens"))
                .unwrap_or(0);
            return Usage::from_inclusive(
                prompt.unwrap_or(0),
                cached,
                0,
                completion.unwrap_or(0),
                reasoning,
            );
        }
    }
    let cache_write = u64_field(usage, "cache_creation_input_tokens").or_else(|| {
        // Only the per-TTL breakdown is present on some responses.
        let breakdown = usage.get("cache_creation").filter(|v| v.is_object())?;
        Some(
            u64_field(breakdown, "ephemeral_5m_input_tokens").unwrap_or(0)
                + u64_field(breakdown, "ephemeral_1h_input_tokens").unwrap_or(0),
        )
    });
    let output = output.unwrap_or(0);
    let reasoning = usage
        .get("output_tokens_details")
        .and_then(|details| u64_field(details, "thinking_tokens"))
        .unwrap_or(0);
    Usage {
        input_tokens: input.unwrap_or(0),
        cache_read_tokens: u64_field(usage, "cache_read_input_tokens").unwrap_or(0),
        cache_write_tokens: cache_write.unwrap_or(0),
        output_tokens: output,
        reasoning_tokens: reasoning.min(output),
    }
}

/// Renders canonical usage as a Messages `usage` object. The cache fields are
/// always present, as in the vendor's responses; the thinking-token detail
/// only when there is something to report.
pub(crate) fn encode_usage(usage: &Usage) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("input_tokens".to_string(), json!(usage.input_tokens));
    out.insert(
        "cache_creation_input_tokens".to_string(),
        json!(usage.cache_write_tokens),
    );
    out.insert(
        "cache_read_input_tokens".to_string(),
        json!(usage.cache_read_tokens),
    );
    out.insert("output_tokens".to_string(), json!(usage.output_tokens));
    if usage.reasoning_tokens > 0 {
        out.insert(
            "output_tokens_details".to_string(),
            json!({"thinking_tokens": usage.reasoning_tokens.min(usage.output_tokens)}),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

/// A message id in the vendor's shape: ids that already are `msg_…` pass
/// through, an empty id is minted, and ids of other vendors are re-prefixed
/// (their own prefix removed when it is a known one).
pub(crate) fn message_id(id: &str) -> String {
    let id = id.trim();
    if id.is_empty() {
        return new_id(MESSAGE_ID_PREFIX);
    }
    if id.starts_with(MESSAGE_ID_PREFIX) {
        return id.to_string();
    }
    let bare = ["chatcmpl-", "chatcmpl_", "resp_"]
        .iter()
        .find_map(|prefix| id.strip_prefix(prefix))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(id);
    format!("{MESSAGE_ID_PREFIX}{bare}")
}

/// The id of a `tool_use` block shown to a client. A call id issued by
/// another vendor is kept verbatim: the client sends it back in the matching
/// `tool_result` and the upstream must recognise it. Only a missing id is
/// minted.
pub(crate) fn tool_use_id(id: &str) -> String {
    if id.is_empty() {
        new_id(TOOL_ID_PREFIX)
    } else {
        id.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tool names
// ---------------------------------------------------------------------------

/// The tool names the client declared in its request, used to hand tool
/// calls back under exactly those names.
///
/// An upstream of another protocol may not accept a name as the client wrote
/// it: its encoder changes the spelling (a `_` prepended to a name that
/// starts with a digit, for instance) and some models change the case. A
/// client only recognises its own spelling, so a returned name that is not
/// one of the declared ones is matched loosely — ignoring case, punctuation
/// and leading underscores — and replaced when exactly one declared tool
/// fits.
///
/// Punctuation and length matter because Anthropic's names are the most
/// permissive of the four protocols in one respect: 128 characters, where
/// OpenAI and Gemini stop at 64. An OpenAI upstream is given
/// `mcp__server__a_very_long_…` cut to 64 characters, and a name with a
/// character it refuses with a `_` in its place; the call comes back under
/// that spelling. A returned name of 63 characters or more that is the
/// beginning of exactly one declared name is therefore that tool.
#[derive(Debug, Default)]
pub(crate) struct ToolNames {
    exact: HashSet<String>,
    /// Loose spelling → declared name; `None` when two tools share it.
    loose: HashMap<String, Option<String>>,
}

/// Shortest returned name that may be a declared one cut short (Gemini cuts
/// at 64 and may spend one character on a leading `_`).
const MIN_TRUNCATED_NAME: usize = 63;

fn loose_name(name: &str) -> String {
    let key: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    key.trim_start_matches('_').to_string()
}

impl ToolNames {
    /// Reads `tools[].name` from the client's original Messages request
    /// ([`ClientCtx::request`]); empty when that is unavailable.
    pub(crate) fn from_request(request: &Value) -> Self {
        let mut names = ToolNames::default();
        let Some(tools) = request.get("tools").and_then(Value::as_array) else {
            return names;
        };
        for name in tools.iter().filter_map(|tool| non_empty(tool, "name")) {
            if !names.exact.insert(name.to_string()) {
                continue;
            }
            names
                .loose
                .entry(loose_name(name))
                .and_modify(|slot| *slot = None)
                .or_insert_with(|| Some(name.to_string()));
        }
        names
    }

    /// The client's spelling of `name`, or `name` itself when it is already
    /// one of the declared names or matches none of them.
    pub(crate) fn restore<'a>(&'a self, name: &'a str) -> &'a str {
        if self.exact.contains(name) {
            return name;
        }
        let key = loose_name(name);
        match self.loose.get(&key) {
            Some(Some(declared)) => return declared,
            Some(None) => return name,
            None => {}
        }
        if name.len() < MIN_TRUNCATED_NAME || key.is_empty() {
            return name;
        }
        let mut cut_short = self
            .loose
            .iter()
            .filter(|(loose, _)| loose.starts_with(&key))
            .map(|(_, declared)| declared);
        match (cut_short.next(), cut_short.next()) {
            (Some(Some(declared)), None) => declared,
            _ => name,
        }
    }
}

// ---------------------------------------------------------------------------
// IR -> wire blocks (client side)
// ---------------------------------------------------------------------------

/// A `text` block for a client. Citations are best effort (see
/// [`encode_citation`]); `None` for an empty block.
pub(crate) fn encode_text_part(text: &TextPart) -> Option<Value> {
    let citations: Vec<Value> = text.citations.iter().filter_map(encode_citation).collect();
    if text.text.is_empty() && citations.is_empty() {
        return None;
    }
    let mut block = Map::new();
    block.insert("type".to_string(), json!("text"));
    block.insert("text".to_string(), Value::String(text.text.clone()));
    if !citations.is_empty() {
        block.insert("citations".to_string(), Value::Array(citations));
    }
    Some(Value::Object(block))
}

/// A `thinking` / `redacted_thinking` block for a client. The signature is
/// wrapped when another vendor issued it, so it can never be replayed to
/// Anthropic by mistake; reasoning without a signature is still shown, with
/// an empty signature string.
pub(crate) fn encode_reasoning_part(reasoning: &Reasoning) -> Option<Value> {
    let signature = reasoning
        .signature
        .as_ref()
        .filter(|signature| !signature.data.is_empty())
        .map(|signature| sig::encode_for_client(signature, THIS));
    if reasoning.redacted {
        // Withheld reasoning is nothing but its payload.
        let data = signature?;
        return Some(json!({"type": "redacted_thinking", "data": data}));
    }
    if reasoning.text.is_empty() && signature.is_none() {
        return None;
    }
    Some(json!({
        "type": "thinking",
        "thinking": reasoning.text,
        "signature": signature.unwrap_or_default(),
    }))
}

/// A `tool_use` block for a client.
pub(crate) fn encode_tool_call_part(call: &ToolCall, names: &ToolNames) -> Value {
    json!({
        "type": "tool_use",
        "id": tool_use_id(&call.id),
        "name": names.restore(&call.name),
        "input": tool_call_input(call),
    })
}

/// Renders one canonical part as a response content block. Parts the
/// protocol cannot show in an assistant message (media, tool results, blocks
/// of other vendors) yield `None`.
pub(crate) fn encode_part(part: &Part, names: &ToolNames) -> Option<Value> {
    match part {
        Part::Text(text) => encode_text_part(text),
        Part::Reasoning(reasoning) => encode_reasoning_part(reasoning),
        Part::ToolCall(call) => Some(encode_tool_call_part(call, names)),
        // The Messages API has no refusal block: a refusal is ordinary text
        // plus `stop_reason: "refusal"`.
        Part::Refusal(refusal) if !refusal.text.is_empty() => {
            Some(json!({"type": "text", "text": refusal.text}))
        }
        Part::Opaque(opaque)
            if opaque.origin.family() == THIS.family() && opaque.raw.is_object() =>
        {
            Some(opaque.raw.clone())
        }
        _ => None,
    }
}

/// Renders a complete response for a Messages client.
///
/// * `model` is the name the client asked for ([`ClientCtx::model`]), falling
///   back to the upstream's when the context has none.
/// * `stop_reason` follows [`stop_reason`]; a failed generation
///   ([`FinishReason::Error`]) is reported as `null`.
/// * `stop_sequence` is only reported together with `stop_reason:
///   "stop_sequence"`, as the API does.
/// * `usage.service_tier` is written only for the vendor's own tier names.
/// * Tool names are handed back in the client's own spelling ([`ToolNames`]).
/// * A tool call that carries a signature of its own (Gemini's
///   `thoughtSignature` on a `functionCall`) is preceded by a text-less
///   `thinking` block holding it, so a client that echoes the turn returns
///   it (see [`call_signature_block`]).
pub(crate) fn encode_response(response: &Response, ctx: &ClientCtx) -> Result<Value, CodecError> {
    let names = ToolNames::from_request(&ctx.request);
    let mut content: Vec<Value> = Vec::with_capacity(response.parts.len());
    for part in &response.parts {
        if let Part::ToolCall(call) = part {
            content.extend(call_signature_block(call));
        }
        content.extend(encode_part(part, &names));
    }
    let hints = StopHints {
        stop_sequence: response.stop_sequence.is_some(),
        tool_use: response
            .parts
            .iter()
            .any(|part| matches!(part, Part::ToolCall(_))),
        refusal: response
            .parts
            .iter()
            .any(|part| matches!(part, Part::Refusal(_))),
    };
    let reason = stop_reason(&response.finish, hints);
    let stop_sequence = response
        .stop_sequence
        .as_deref()
        .filter(|_| reason == Some("stop_sequence"));
    let model = if ctx.model.is_empty() {
        response.model.as_str()
    } else {
        ctx.model.as_str()
    };
    let mut usage = encode_usage(&response.usage);
    if let Some(tier) = response
        .service_tier
        .as_deref()
        .filter(|tier| SERVICE_TIERS.contains(tier))
    {
        usage.insert("service_tier".to_string(), json!(tier));
    }
    Ok(json!({
        "id": message_id(&response.id),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": reason,
        "stop_sequence": stop_sequence,
        "usage": usage,
    }))
}

// ---------------------------------------------------------------------------
// Wire -> IR (upstream side)
// ---------------------------------------------------------------------------

/// Decodes a complete Messages response.
///
/// Fails only when the body is not a message at all: not an object, an error
/// envelope, or an object with none of the fields a message has.
pub(crate) fn decode_response(body: &Value) -> Result<Response, CodecError> {
    let Some(object) = body.as_object() else {
        return Err(CodecError::upstream("response body is not a JSON object"));
    };
    let is_error = str_field(body, "type") == Some("error")
        || (object.contains_key("error") && !object.contains_key("content"));
    if is_error {
        let message = parse_error_value(body)
            .map(|info| info.message)
            .unwrap_or_else(|| "unknown error".to_string());
        return Err(CodecError::upstream(format!(
            "upstream answered with an error body: {message}"
        )));
    }
    if !object.contains_key("content")
        && !object.contains_key("stop_reason")
        && str_field(body, "type") != Some("message")
    {
        return Err(CodecError::upstream(
            "response body is not a Messages response (no `content`)",
        ));
    }

    let parts: Vec<Part> = match body.get("content") {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| decode_assistant_block(block, SigSource::Upstream))
            .collect(),
        // Not valid Messages output, but unambiguous.
        Some(Value::String(text)) if !text.is_empty() => vec![Part::text(text.clone())],
        _ => Vec::new(),
    };
    let has_tool_calls = parts.iter().any(|part| matches!(part, Part::ToolCall(_)));
    let parts = parts
        .into_iter()
        .map(|part| match part {
            Part::ToolCall(mut call) if call.id.is_empty() => {
                call.id = new_id(TOOL_ID_PREFIX);
                Part::ToolCall(call)
            }
            other => other,
        })
        .collect();

    let usage_value = body.get("usage");
    Ok(Response {
        id: non_empty(body, "id")
            .map(str::to_string)
            .unwrap_or_else(|| new_id(MESSAGE_ID_PREFIX)),
        model: str_field(body, "model").unwrap_or("").to_string(),
        // The Messages API reports no creation time.
        created: 0,
        parts,
        finish: finish_reason(str_field(body, "stop_reason"), has_tool_calls),
        stop_sequence: non_empty(body, "stop_sequence").map(str::to_string),
        usage: usage_value.map(decode_usage).unwrap_or_default(),
        service_tier: usage_value
            .and_then(|usage| non_empty(usage, "service_tier"))
            .or_else(|| non_empty(body, "service_tier"))
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn message_ids_keep_native_shape_and_reprefix_foreign_ones() {
        assert_eq!(
            message_id("msg_01XFDUDYJgAACzvnptvVoYEL"),
            "msg_01XFDUDYJgAACzvnptvVoYEL"
        );
        assert_eq!(message_id("chatcmpl-abc123"), "msg_abc123");
        assert_eq!(message_id("resp_68af"), "msg_68af");
        assert_eq!(message_id("Xy-9"), "msg_Xy-9");
        let minted = message_id("");
        assert!(minted.starts_with("msg_") && minted.len() == 28);
    }

    #[test]
    fn tool_names_are_restored_to_the_clients_spelling() {
        let names = ToolNames::from_request(&json!({"tools": [
            {"name": "Read", "input_schema": {}},
            {"name": "9lives", "input_schema": {}},
            {"name": "dup", "input_schema": {}},
            {"name": "_dup", "input_schema": {}},
            {"type": "web_search_20250305", "name": "web_search"},
            "garbage"
        ]}));
        assert_eq!(names.restore("Read"), "Read");
        assert_eq!(names.restore("read"), "Read");
        assert_eq!(names.restore("_9lives"), "9lives");
        assert_eq!(names.restore("WEB_SEARCH"), "web_search");
        // Declared names are never rewritten, ambiguous ones never guessed.
        assert_eq!(names.restore("_dup"), "_dup");
        assert_eq!(names.restore("DUP"), "DUP");
        assert_eq!(names.restore("unknown_tool"), "unknown_tool");
        // No request available: everything passes through.
        let names = ToolNames::from_request(&Value::Null);
        assert_eq!(names.restore("read"), "read");
    }

    #[test]
    fn tool_ids_are_only_minted_when_missing() {
        assert_eq!(tool_use_id("call_abc"), "call_abc");
        assert_eq!(tool_use_id("toolu_01A"), "toolu_01A");
        assert!(tool_use_id("").starts_with("toolu_"));
    }

    #[test]
    fn usage_accepts_the_cache_creation_breakdown_alone() {
        let usage = decode_usage(&json!({
            "input_tokens": 5, "output_tokens": 7,
            "cache_creation": {"ephemeral_5m_input_tokens": 10, "ephemeral_1h_input_tokens": 3}
        }));
        assert_eq!(usage.cache_write_tokens, 13);
    }

    #[test]
    fn usage_in_openai_shape_is_converted_from_the_inclusive_convention() {
        let usage = decode_usage(&json!({
            "prompt_tokens": 100, "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 60},
            "completion_tokens_details": {"reasoning_tokens": 8}
        }));
        assert_eq!(
            usage,
            Usage {
                input_tokens: 40,
                cache_read_tokens: 60,
                cache_write_tokens: 0,
                output_tokens: 20,
                reasoning_tokens: 8,
            }
        );
    }

    #[test]
    fn stop_reason_for_plain_stop_depends_on_content() {
        let plain = StopHints::default();
        assert_eq!(stop_reason(&FinishReason::Stop, plain), Some("end_turn"));
        let hints = StopHints {
            stop_sequence: true,
            ..plain
        };
        assert_eq!(
            stop_reason(&FinishReason::Stop, hints),
            Some("stop_sequence")
        );
        let hints = StopHints {
            tool_use: true,
            stop_sequence: true,
            ..plain
        };
        assert_eq!(stop_reason(&FinishReason::Stop, hints), Some("tool_use"));
        let hints = StopHints {
            refusal: true,
            tool_use: true,
            ..plain
        };
        assert_eq!(stop_reason(&FinishReason::Stop, hints), Some("refusal"));
        assert_eq!(stop_reason(&FinishReason::Error, plain), None);
    }
}
