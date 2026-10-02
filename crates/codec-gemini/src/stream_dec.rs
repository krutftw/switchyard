//! Decoder for `streamGenerateContent?alt=sse`.
//!
//! Every SSE `data:` payload is a complete `GenerateContentResponse` chunk.
//! There are no event names and no terminator: text parts are deltas, thought
//! parts are reasoning deltas, `functionCall` parts arrive whole, the chunk
//! that carries `finishReason` is the last one, and the stream then simply
//! ends. An error in the middle of a stream is a payload with a top-level
//! `error` object.
//!
//! Because the end of the connection is the only terminator, `Finish` is
//! emitted from [`StreamDecoder::finish`], never from `decode`. That keeps
//! the decoder correct for every placement of the finish reason and the usage
//! seen in the wild: usage in a chunk of its own after the finish reason,
//! servers that put `"finishReason": "STOP"` on every chunk (old Gemini
//! models and several imitations), and relays that attach it to each
//! function call. A stream that ends without any finish reason was cut off
//! and finishes with `FinishReason::Error`.

use crate::error::api_error_from_payload;
use crate::parts::{
    call_args_text, call_name, decode_file_data, decode_inline_data, explicit_id, is_metadata_only,
    opaque, response_signature,
};
use crate::raw::unwrap_envelope;
use crate::response::{
    block_message, candidate_metadata_of, decode_finish, decode_usage, finish_with_calls,
    grounding_supports, metadata_part, response_head, support_citations, usage_metadata,
};
use crate::util::{pick, pick_in, pick_str};
use serde_json::Value;
use switchyard_core::ir::{FinishReason, Part, Signature, ToolCallKind};
use switchyard_core::stream::{BlockStart, StreamEvent};
use switchyard_core::util::{new_call_id, new_id};
use switchyard_core::{CodecError, SseEvent, StreamDecoder, Usage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Open {
    None,
    Text(u32),
    Reasoning(u32),
}

/// Stateful decoder for one Gemini stream.
pub(crate) struct GeminiStreamDecoder {
    started: bool,
    /// A terminal event (`Finish` or `Error`) has been emitted.
    done: bool,
    next_index: u32,
    open: Open,
    /// Text of the open text block, for converting grounding byte offsets.
    block_text: String,
    saw_call: bool,
    /// The latest finish reason seen on a chunk; reported when the stream ends.
    pending_finish: Option<FinishReason>,
    last_usage: Usage,
    /// Latest candidate-level metadata, emitted as opaque parts at the end.
    metadata: Vec<(&'static str, Value)>,
    /// Citations already emitted, so cumulative grounding is not repeated.
    cited: Vec<(Option<String>, Option<u64>, Option<u64>)>,
}

impl GeminiStreamDecoder {
    pub(crate) fn new() -> Self {
        GeminiStreamDecoder {
            started: false,
            done: false,
            next_index: 0,
            open: Open::None,
            block_text: String::new(),
            saw_call: false,
            pending_finish: None,
            last_usage: Usage::default(),
            metadata: Vec::new(),
            cited: Vec::new(),
        }
    }

    fn take_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn close_block(&mut self, out: &mut Vec<StreamEvent>) {
        match self.open {
            Open::Text(index) | Open::Reasoning(index) => {
                out.push(StreamEvent::BlockStop { index })
            }
            Open::None => {}
        }
        self.open = Open::None;
        self.block_text.clear();
    }

    fn start(&mut self, root: &Value, out: &mut Vec<StreamEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        let (id, model, created) = response_head(root);
        out.push(StreamEvent::Start { id, model, created });
    }

    fn whole(&mut self, part: Part, out: &mut Vec<StreamEvent>) {
        self.close_block(out);
        let index = self.take_index();
        out.push(StreamEvent::BlockStart {
            index,
            block: BlockStart::Whole { part },
        });
        out.push(StreamEvent::BlockStop { index });
    }

    fn reasoning_index(&mut self, out: &mut Vec<StreamEvent>) -> u32 {
        if let Open::Reasoning(index) = self.open {
            return index;
        }
        self.close_block(out);
        let index = self.take_index();
        out.push(StreamEvent::BlockStart {
            index,
            block: BlockStart::Reasoning {
                id: None,
                redacted: false,
            },
        });
        self.open = Open::Reasoning(index);
        index
    }

    /// A signature that arrived without text: it belongs to the reasoning
    /// block that is open, or forms a text-less reasoning block of its own.
    fn signature_only(&mut self, signature: Signature, out: &mut Vec<StreamEvent>) {
        let index = self.reasoning_index(out);
        out.push(StreamEvent::ReasoningSignature { index, signature });
    }

    fn part(&mut self, part: &Value, out: &mut Vec<StreamEvent>) {
        let Some(map) = part.as_object() else {
            return;
        };
        let signature = response_signature(part);
        let thought = map.get("thought").and_then(Value::as_bool).unwrap_or(false);

        if let Some(call) = pick_in(map, &["functionCall", "function_call"]) {
            // Function calls are never fragmented: one block, one delta.
            self.close_block(out);
            let index = self.take_index();
            out.push(StreamEvent::BlockStart {
                index,
                block: BlockStart::ToolCall {
                    id: explicit_id(call)
                        .map(str::to_owned)
                        .unwrap_or_else(new_call_id),
                    name: call_name(call).to_string(),
                    kind: ToolCallKind::Function,
                    signature,
                },
            });
            out.push(StreamEvent::ToolArgsDelta {
                index,
                fragment: call_args_text(call),
            });
            out.push(StreamEvent::BlockStop { index });
            self.saw_call = true;
            return;
        }

        if let Some(text) = map.get("text").and_then(Value::as_str) {
            if thought {
                if text.is_empty() && signature.is_none() {
                    return;
                }
                let index = self.reasoning_index(out);
                if !text.is_empty() {
                    out.push(StreamEvent::ReasoningDelta {
                        index,
                        text: text.to_string(),
                    });
                }
                if let Some(signature) = signature {
                    out.push(StreamEvent::ReasoningSignature { index, signature });
                }
            } else if text.is_empty() {
                // Gemini 3 delivers the signature of a turn without function
                // calls on an empty text part, often in the last chunk.
                if let Some(signature) = signature {
                    self.signature_only(signature, out);
                }
            } else {
                // The stream model has no signature slot on text blocks, so a
                // signature on non-empty text is not carried (Gemini does not
                // validate those).
                let index = match self.open {
                    Open::Text(index) => index,
                    _ => {
                        self.close_block(out);
                        let index = self.take_index();
                        out.push(StreamEvent::BlockStart {
                            index,
                            block: BlockStart::Text,
                        });
                        self.open = Open::Text(index);
                        index
                    }
                };
                self.block_text.push_str(text);
                out.push(StreamEvent::TextDelta {
                    index,
                    text: text.to_string(),
                });
            }
            return;
        }

        if let Some(media) =
            pick_in(map, &["inlineData", "inline_data"]).and_then(decode_inline_data)
        {
            self.whole(media, out);
            return;
        }
        if let Some(media) = pick_in(map, &["fileData", "file_data"]).and_then(decode_file_data) {
            self.whole(media, out);
            return;
        }
        if map.contains_key("inlineData")
            || map.contains_key("inline_data")
            || map.contains_key("fileData")
        {
            // Media part without data: nothing to deliver.
            return;
        }
        if is_metadata_only(map) {
            if let Some(signature) = signature {
                self.signature_only(signature, out);
            }
            return;
        }
        self.whole(opaque(part), out);
    }

    /// Emits the citations of a grounding update on the open text block.
    /// Offsets are relative to the supported part; in a stream the best
    /// available approximation of "the part" is the open block.
    fn citations(&mut self, meta: &Value, out: &mut Vec<StreamEvent>) {
        let Open::Text(index) = self.open else {
            return;
        };
        let (supports, _) = grounding_supports(meta);
        for support in &supports {
            for citation in support_citations(support, &self.block_text, 0) {
                let key = (citation.url.clone(), citation.start, citation.end);
                if self.cited.contains(&key) {
                    continue;
                }
                self.cited.push(key);
                out.push(StreamEvent::Citation { index, citation });
            }
        }
    }

    fn terminate(&mut self, reason: FinishReason, out: &mut Vec<StreamEvent>) {
        self.close_block(out);
        for (key, value) in std::mem::take(&mut self.metadata) {
            self.whole(metadata_part(key, &value), out);
        }
        out.push(StreamEvent::Finish {
            reason: finish_with_calls(reason, self.saw_call),
            stop_sequence: None,
        });
        self.done = true;
    }

    fn chunk(&mut self, value: &Value, error_event: bool, out: &mut Vec<StreamEvent>) {
        if self.done {
            return;
        }
        let root = unwrap_envelope(value);
        let Some(map) = root.as_object() else {
            return;
        };
        if error_event || map.get("error").is_some_and(|e| !e.is_null()) {
            // The sequence contract wants a `Start` even when the very first
            // payload is the error.
            self.start(root, out);
            self.close_block(out);
            out.push(StreamEvent::Error(api_error_from_payload(root)));
            self.done = true;
            return;
        }
        let candidates = pick_in(map, &["candidates"]).and_then(Value::as_array);
        let usage = usage_metadata(root);
        let feedback = pick_in(map, &["promptFeedback", "prompt_feedback"]);
        let recognised = candidates.is_some()
            || usage.is_some()
            || feedback.is_some()
            || map.contains_key("responseId")
            || map.contains_key("modelVersion");
        if !recognised {
            return;
        }
        self.start(root, out);

        if let Some(candidate) = candidates.and_then(|list| list.first()) {
            let parts =
                match pick(candidate, &["content"]).and_then(|content| pick(content, &["parts"])) {
                    Some(Value::Array(parts)) => parts.as_slice(),
                    Some(single @ Value::Object(_)) => std::slice::from_ref(single),
                    _ => &[],
                };
            for part in parts {
                self.part(part, out);
            }
            for (key, value) in candidate_metadata_of(candidate) {
                if key == "groundingMetadata" {
                    self.citations(value, out);
                }
                // Metadata is cumulative: the latest object replaces the last.
                self.metadata.retain(|(existing, _)| *existing != key);
                self.metadata.push((key, value.clone()));
            }
            let reason = pick_str(candidate, &["finishReason", "finish_reason"])
                .map(str::trim)
                .filter(|r| !r.is_empty() && !r.eq_ignore_ascii_case("FINISH_REASON_UNSPECIFIED"));
            if let Some(reason) = reason {
                self.pending_finish = Some(decode_finish(reason));
            }
        } else if let Some(message) = feedback.and_then(block_message) {
            // The prompt was blocked: no candidate will ever come.
            self.close_block(out);
            let index = self.take_index();
            out.push(StreamEvent::BlockStart {
                index,
                block: BlockStart::Refusal,
            });
            out.push(StreamEvent::TextDelta {
                index,
                text: message,
            });
            out.push(StreamEvent::BlockStop { index });
            self.pending_finish = Some(FinishReason::ContentFilter);
        }

        if let Some(meta) = usage {
            let usage = decode_usage(meta);
            if !usage.is_empty() && usage != self.last_usage {
                self.last_usage = usage;
                out.push(StreamEvent::Usage(usage));
            }
        }
    }
}

impl StreamDecoder for GeminiStreamDecoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, CodecError> {
        let mut out = Vec::new();
        if self.done {
            return Ok(out);
        }
        let data = event.data.trim();
        // Some relays hand over the raw line, prefix included.
        let data = data.strip_prefix("data:").map(str::trim).unwrap_or(data);
        if data.is_empty() || data == "[DONE]" {
            return Ok(out);
        }
        // Anything that is not JSON (keep-alives, HTML fragments) is skipped.
        let Ok(payload) = serde_json::from_str::<Value>(data) else {
            return Ok(out);
        };
        let error_event = event
            .event
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("error"));
        match &payload {
            // Without `alt=sse` Gemini streams one JSON array of chunks.
            Value::Array(chunks) => {
                for chunk in chunks {
                    self.chunk(chunk, error_event, &mut out);
                }
            }
            chunk => self.chunk(chunk, error_event, &mut out),
        }
        Ok(out)
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start {
                id: new_id(""),
                model: String::new(),
                created: 0,
            });
        }
        // No chunk carried a finish reason: the stream was cut off.
        let reason = self.pending_finish.take().unwrap_or(FinishReason::Error);
        self.terminate(reason, &mut out);
        out
    }
}
