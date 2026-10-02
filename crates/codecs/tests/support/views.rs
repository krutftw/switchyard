//! Reading what a client was sent: per protocol, the pieces of a wire
//! response the matrix compares (text, reasoning, tool calls, finish reason,
//! usage), and the tables saying what they must be.
//!
//! Like the validators, this reads the vendors' formats directly and uses no
//! codec code.

use super::scenarios::Expect;
use base64::Engine as _;
use serde_json::Value;
use std::collections::BTreeSet;
use switchyard_core::ir::{FinishReason, Part, Response};
use switchyard_core::{Protocol, Usage};

/// A wire response as a client sees it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    /// Visible answer text, concatenated.
    pub text: String,
    pub reasoning: String,
    /// Refusal text, on the protocols that have a slot for it.
    pub refusal: String,
    /// `(name, arguments)` of each tool call, in order.
    pub calls: Vec<(String, Value)>,
    /// The finish reason in the protocol's own spelling. For Responses:
    /// `status`, or `incomplete:<reason>`.
    pub finish: String,
    /// The stop sequence that matched, where the protocol reports one.
    pub stop_sequence: Option<String>,
}

fn arguments(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

/// Reads a complete response body of `protocol`.
pub fn view(protocol: Protocol, body: &Value) -> View {
    let mut view = View::default();
    match protocol {
        Protocol::OpenaiChat => {
            let choice = &body["choices"][0];
            let message = &choice["message"];
            view.text = message["content"].as_str().unwrap_or("").to_string();
            view.reasoning = message["reasoning_content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            view.refusal = message["refusal"].as_str().unwrap_or("").to_string();
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                view.calls.push((
                    call["function"]["name"].as_str().unwrap_or("").to_string(),
                    arguments(call["function"]["arguments"].as_str().unwrap_or("")),
                ));
            }
            view.finish = choice["finish_reason"].as_str().unwrap_or("").to_string();
        }
        Protocol::OpenaiResponses => {
            for item in body["output"].as_array().into_iter().flatten() {
                match item["type"].as_str() {
                    Some("message") => {
                        for part in item["content"].as_array().into_iter().flatten() {
                            match part["type"].as_str() {
                                Some("output_text") => {
                                    view.text.push_str(part["text"].as_str().unwrap_or(""));
                                }
                                Some("refusal") => {
                                    view.refusal
                                        .push_str(part["refusal"].as_str().unwrap_or(""));
                                }
                                _ => {}
                            }
                        }
                    }
                    Some("reasoning") => {
                        for part in item["summary"].as_array().into_iter().flatten() {
                            view.reasoning.push_str(part["text"].as_str().unwrap_or(""));
                        }
                    }
                    Some("function_call") => view.calls.push((
                        item["name"].as_str().unwrap_or("").to_string(),
                        arguments(item["arguments"].as_str().unwrap_or("")),
                    )),
                    _ => {}
                }
            }
            let status = body["status"].as_str().unwrap_or("");
            view.finish = match body["incomplete_details"]["reason"].as_str() {
                Some(reason) => format!("{status}:{reason}"),
                None => status.to_string(),
            };
        }
        Protocol::Anthropic => {
            for block in body["content"].as_array().into_iter().flatten() {
                match block["type"].as_str() {
                    Some("text") => view.text.push_str(block["text"].as_str().unwrap_or("")),
                    Some("thinking") => {
                        view.reasoning
                            .push_str(block["thinking"].as_str().unwrap_or(""));
                    }
                    Some("tool_use") => view.calls.push((
                        block["name"].as_str().unwrap_or("").to_string(),
                        block["input"].clone(),
                    )),
                    _ => {}
                }
            }
            view.finish = body["stop_reason"].as_str().unwrap_or("").to_string();
            view.stop_sequence = body["stop_sequence"].as_str().map(str::to_string);
        }
        Protocol::Gemini => {
            let candidate = &body["candidates"][0];
            for part in candidate["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if let Some(call) = part.get("functionCall") {
                    view.calls.push((
                        call["name"].as_str().unwrap_or("").to_string(),
                        call["args"].clone(),
                    ));
                } else if part["thought"] == true {
                    view.reasoning.push_str(part["text"].as_str().unwrap_or(""));
                } else {
                    view.text.push_str(part["text"].as_str().unwrap_or(""));
                }
            }
            view.finish = candidate["finishReason"].as_str().unwrap_or("").to_string();
        }
    }
    view
}

/// The finish reason a client of `protocol` must be shown for a canonical
/// outcome. Written out from the mapping tables:
///
/// * Chat — notes 06 §4.1 (`end_turn`/`stop_sequence` → `stop`, `tool_use` →
///   `tool_calls`, `max_tokens` → `length`, `refusal` → `content_filter`),
///   notes 07 §3 REC (`MAX_TOKENS` → `length`, safety family →
///   `content_filter`) and notes 08 §8.2 (a `completed` Responses answer
///   without tool calls is `stop`: a refusal the model wrote out is a
///   completed answer, reported as `message.refusal` with `stop`);
/// * Anthropic — notes 06 §8.1 (`stop` → `end_turn`, `length` →
///   `max_tokens`, `tool_calls` → `tool_use`) with the vendor hint of notes
///   15 §5.4 for filtered output (`refusal`), which is also what notes 09
///   §2.1 recommends for `SAFETY`;
/// * Gemini — notes 07 §6 (`stop`/`tool_calls` → `STOP`, `length` →
///   `MAX_TOKENS`, `content_filter` → `SAFETY`) and notes 09 §4.1 REC;
/// * Responses — notes 08 §2 (`completed`, or `incomplete` with
///   `max_output_tokens` / `content_filter`).
pub fn expected_finish(protocol: Protocol, expect: &Expect, stop_sequence: bool) -> String {
    let has_refusal = !expect.refusal.is_empty();
    let has_calls = !expect.calls.is_empty();
    let finish = &expect.finish;
    match protocol {
        Protocol::OpenaiChat => match finish {
            FinishReason::Stop => "stop",
            FinishReason::ToolCalls => "tool_calls",
            FinishReason::Length => "length",
            // A refusal that was written out is a completed answer; one
            // without refusal text is a withheld answer.
            FinishReason::Refusal if has_refusal && !has_calls => "stop",
            FinishReason::ContentFilter | FinishReason::Refusal => "content_filter",
            other => panic!("no Chat expectation for {other:?}"),
        }
        .to_string(),
        Protocol::Anthropic => match finish {
            // The Messages API has no refusal block: a refusal is text plus
            // `stop_reason: "refusal"`.
            FinishReason::Stop if has_refusal => "refusal",
            FinishReason::Stop if stop_sequence => "stop_sequence",
            FinishReason::Stop => "end_turn",
            FinishReason::ToolCalls => "tool_use",
            FinishReason::Length => "max_tokens",
            FinishReason::ContentFilter | FinishReason::Refusal => "refusal",
            other => panic!("no Anthropic expectation for {other:?}"),
        }
        .to_string(),
        Protocol::Gemini => match finish {
            // Gemini has no refusal part either: a refusal is text, and the
            // finish reason says the same whichever OpenAI protocol reported
            // it (Chat: `stop` + `message.refusal`; Responses: `completed`
            // + a refusal part).
            FinishReason::Stop if has_refusal => "SAFETY",
            FinishReason::Stop | FinishReason::ToolCalls => "STOP",
            FinishReason::Length => "MAX_TOKENS",
            FinishReason::ContentFilter | FinishReason::Refusal => "SAFETY",
            other => panic!("no Gemini expectation for {other:?}"),
        }
        .to_string(),
        Protocol::OpenaiResponses => match finish {
            FinishReason::Stop | FinishReason::ToolCalls => "completed",
            // A refusal is a completed response whose content is a refusal
            // part; a refusal without one is a filtered answer.
            FinishReason::Refusal if has_refusal && !has_calls => "completed",
            FinishReason::Refusal | FinishReason::ContentFilter => "incomplete:content_filter",
            FinishReason::Length => "incomplete:max_output_tokens",
            other => panic!("no Responses expectation for {other:?}"),
        }
        .to_string(),
    }
}

/// Checks the usage object of a wire response against canonical usage, under
/// the protocol's own convention. Returns the mismatches.
///
/// * OpenAI (notes 06 §4.2, 15 §3.2): the prompt count *includes* cached and
///   cache-written tokens; the completion count includes reasoning tokens.
/// * Anthropic (notes 06 §8.2, 15 §5.3): `input_tokens` *excludes* cache
///   reads and writes, which are reported next to it; `output_tokens`
///   includes thinking.
/// * Gemini (notes 07 §6.2 REC, 09 §4.1 REC, 15 §6.3): `promptTokenCount`
///   includes cached tokens; `candidatesTokenCount` *excludes* thoughts,
///   which are counted next to it; the total is the sum of all three.
pub fn usage_mismatches(protocol: Protocol, body: &Value, usage: &Usage) -> Vec<String> {
    let prompt_total = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
    let mut expected: Vec<(&str, u64)> = Vec::new();
    let holder = match protocol {
        Protocol::OpenaiChat => {
            expected.push(("/prompt_tokens", prompt_total));
            expected.push(("/completion_tokens", usage.output_tokens));
            expected.push(("/total_tokens", prompt_total + usage.output_tokens));
            expected.push((
                "/prompt_tokens_details/cached_tokens",
                usage.cache_read_tokens,
            ));
            expected.push((
                "/completion_tokens_details/reasoning_tokens",
                usage.reasoning_tokens,
            ));
            &body["usage"]
        }
        Protocol::OpenaiResponses => {
            expected.push(("/input_tokens", prompt_total));
            expected.push(("/output_tokens", usage.output_tokens));
            expected.push(("/total_tokens", prompt_total + usage.output_tokens));
            expected.push((
                "/input_tokens_details/cached_tokens",
                usage.cache_read_tokens,
            ));
            expected.push((
                "/output_tokens_details/reasoning_tokens",
                usage.reasoning_tokens,
            ));
            &body["usage"]
        }
        Protocol::Anthropic => {
            expected.push(("/input_tokens", usage.input_tokens));
            expected.push(("/cache_read_input_tokens", usage.cache_read_tokens));
            expected.push(("/cache_creation_input_tokens", usage.cache_write_tokens));
            expected.push(("/output_tokens", usage.output_tokens));
            &body["usage"]
        }
        Protocol::Gemini => {
            expected.push(("/promptTokenCount", prompt_total));
            expected.push(("/cachedContentTokenCount", usage.cache_read_tokens));
            expected.push((
                "/candidatesTokenCount",
                usage.output_tokens - usage.reasoning_tokens,
            ));
            expected.push(("/thoughtsTokenCount", usage.reasoning_tokens));
            expected.push(("/totalTokenCount", prompt_total + usage.output_tokens));
            &body["usageMetadata"]
        }
    };
    expected
        .into_iter()
        .filter_map(|(pointer, want)| {
            // A zero count may simply be left out.
            let got = holder.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
            (got != want).then(|| format!("usage{pointer} is {got}, expected {want}"))
        })
        .collect()
}

/// How a blob issued by `origin` looks in a payload for a client of
/// `client`: verbatim on the issuer's own protocol, tagged with its origin
/// (`sy1.<tag>.<blob>`, `docs/DESIGN.md` §2) everywhere else — and, on
/// Gemini, base64-armoured on top because the field is protobuf `bytes`.
pub fn client_blob(client: Protocol, origin: Protocol, blob: &str) -> String {
    if client == origin {
        return blob.to_string();
    }
    let tagged = format!("sy1.{}.{blob}", origin.tag());
    if client == Protocol::Gemini {
        base64::engine::general_purpose::STANDARD.encode(tagged)
    } else {
        tagged
    }
}

/// Whether a client protocol has a field of its own for the signature of a
/// tool call: Chat (`extra_content.google.thought_signature`) and Gemini
/// (`thoughtSignature` on the `functionCall` part).
pub fn has_call_slot(client: Protocol) -> bool {
    matches!(client, Protocol::OpenaiChat | Protocol::Gemini)
}

/// How the signature `blob` that `origin` put on a *tool call* looks in a
/// payload for a client of `client`.
///
/// Chat and Gemini clients have a field for it, so it looks like any other
/// blob ([`client_blob`]). A `tool_use` block and a `function_call` item
/// have none; for Anthropic and Responses clients the codecs carry the
/// signature on a text-less `thinking` block / summary-less `reasoning` item
/// directly ahead of the call, marked `call:` inside the origin tag so it is
/// told apart from signed reasoning (the rustdoc of
/// `switchyard_codec_anthropic` `blocks::CALL_MARKER` and
/// `switchyard_codec_responses` `common::CALL_MARKER`).
pub fn client_call_blob(client: Protocol, origin: Protocol, blob: &str) -> String {
    if has_call_slot(client) {
        client_blob(client, origin, blob)
    } else {
        format!("sy1.{}.call:{blob}", origin.tag())
    }
}

/// For an Anthropic or a Responses client: checks that `body` (a response,
/// or the assistant turn of a request) carries `carrier` on a text-less
/// reasoning block directly ahead of a tool call.
pub fn carrier_ahead_of_call(client: Protocol, body: &Value, carrier: &str) -> Result<(), String> {
    let blocks = match client {
        Protocol::Anthropic => &body["content"],
        Protocol::OpenaiResponses => &body["output"],
        other => return Err(format!("{other} has a field for call signatures")),
    };
    let is_carrier = |block: &Value| match client {
        Protocol::Anthropic => {
            block["type"] == "thinking" && block["signature"] == carrier && block["thinking"] == ""
        }
        _ => {
            block["type"] == "reasoning"
                && block["encrypted_content"] == carrier
                && block["summary"].as_array().is_some_and(Vec::is_empty)
        }
    };
    let is_call = |block: &Value| match client {
        Protocol::Anthropic => block["type"] == "tool_use",
        _ => block["type"] == "function_call",
    };
    let blocks = blocks.as_array().ok_or("no content blocks")?;
    let at = blocks
        .iter()
        .position(is_carrier)
        .ok_or_else(|| format!("no block carries `{carrier}`: {blocks:?}"))?;
    if blocks.get(at + 1).is_some_and(is_call) {
        Ok(())
    } else {
        Err(format!(
            "the carrier of `{carrier}` is not directly ahead of a tool call: {blocks:?}"
        ))
    }
}

/// A canonical response reduced to what must survive a trip through any
/// client protocol: ids, timestamps, model names and block boundaries are
/// each protocol's own business.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub text: String,
    pub reasoning: String,
    pub refusal: String,
    pub calls: Vec<(String, String, Value)>,
    pub finish: FinishReason,
    pub usage: Usage,
    /// `(origin tag, blob)` of every signature in the response.
    pub blobs: BTreeSet<(char, String)>,
}

pub fn summarize(response: &Response) -> Summary {
    let mut summary = Summary {
        text: String::new(),
        reasoning: String::new(),
        refusal: String::new(),
        calls: Vec::new(),
        finish: response.finish.clone(),
        usage: response.usage,
        blobs: BTreeSet::new(),
    };
    for part in &response.parts {
        match part {
            Part::Text(text) => {
                summary.text.push_str(&text.text);
                if let Some(signature) = &text.signature {
                    summary
                        .blobs
                        .insert((signature.origin.tag(), signature.data.clone()));
                }
            }
            Part::Reasoning(reasoning) => {
                summary.reasoning.push_str(&reasoning.text);
                if let Some(signature) = &reasoning.signature {
                    summary
                        .blobs
                        .insert((signature.origin.tag(), signature.data.clone()));
                }
            }
            Part::Refusal(refusal) => summary.refusal.push_str(&refusal.text),
            Part::ToolCall(call) => {
                summary
                    .calls
                    .push((call.id.clone(), call.name.clone(), call.arguments_value()));
                if let Some(signature) = &call.signature {
                    summary
                        .blobs
                        .insert((signature.origin.tag(), signature.data.clone()));
                }
            }
            _ => {}
        }
    }
    summary
}
