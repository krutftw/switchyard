//! Upstream side: a Messages SSE stream → canonical [`StreamEvent`]s.

use crate::blocks::{decode_citation, opaque};
use crate::error::stream_error;
use crate::response::{MESSAGE_ID_PREFIX, TOOL_ID_PREFIX, decode_usage, finish_reason};
use crate::util::{THIS, non_empty, str_field};
use serde_json::Value;
use switchyard_core::ir::{FinishReason, Signature};
use switchyard_core::stream::{BlockStart, StreamEvent};
use switchyard_core::util::new_id;
use switchyard_core::{CodecError, SseEvent, StreamDecoder, Usage};

/// The content block currently open on the wire.
enum Open {
    Text {
        index: u32,
    },
    Thinking {
        index: u32,
        /// Signature received so far. The API sends it in one delta; a
        /// server that splits it still ends up with the whole value.
        signature: String,
    },
    Redacted {
        index: u32,
    },
    Tool {
        index: u32,
        /// A complete `input` object carried by `content_block_start`
        /// (compatible servers do this instead of streaming deltas).
        start_input: Option<String>,
        /// Whether any argument text has been forwarded.
        streamed: bool,
    },
    /// A block the IR does not model (server tools and their results, MCP
    /// blocks, anything newer). It is collected and forwarded whole when it
    /// closes.
    Other {
        raw: Value,
        /// Concatenated `input_json_delta` fragments.
        input_json: String,
    },
}

/// Decoder state for one Messages stream.
///
/// Sequence guarantees, whatever the upstream sends:
///
/// * `Start` is emitted first — synthesised when content arrives without a
///   `message_start`;
/// * a block still open when the next one starts, when the message ends or
///   when the stream dies is closed;
/// * `Finish` is emitted at `message_stop`. A stream that ends after a
///   `message_delta` carrying a stop reason but without `message_stop` is
///   treated as complete; any other truncated stream ends with
///   `Finish { reason: Error }`;
/// * nothing is emitted after a terminal event.
#[derive(Default)]
pub(crate) struct Decoder {
    started: bool,
    finished: bool,
    next_index: u32,
    open: Option<Open>,
    usage: Usage,
    stop_reason: Option<String>,
    stop_sequence: Option<String>,
    saw_tool_call: bool,
}

impl Decoder {
    pub(crate) fn new() -> Self {
        Decoder::default()
    }

    fn allocate(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn ensure_started(&mut self, out: &mut Vec<StreamEvent>) {
        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start {
                id: new_id(MESSAGE_ID_PREFIX),
                model: String::new(),
                created: 0,
            });
        }
    }

    fn close_open(&mut self, out: &mut Vec<StreamEvent>) {
        match self.open.take() {
            None => {}
            Some(Open::Text { index })
            | Some(Open::Thinking { index, .. })
            | Some(Open::Redacted { index }) => out.push(StreamEvent::BlockStop { index }),
            Some(Open::Tool {
                index,
                start_input,
                streamed,
            }) => {
                if let Some(fragment) = start_input.filter(|_| !streamed) {
                    out.push(StreamEvent::ToolArgsDelta { index, fragment });
                }
                out.push(StreamEvent::BlockStop { index });
            }
            Some(Open::Other {
                mut raw,
                input_json,
            }) => {
                if !input_json.trim().is_empty()
                    && let Ok(input) = serde_json::from_str::<Value>(&input_json)
                    && let Some(block) = raw.as_object_mut()
                {
                    block.insert("input".to_string(), input);
                }
                let index = self.allocate();
                out.push(StreamEvent::BlockStart {
                    index,
                    block: BlockStart::Whole { part: opaque(&raw) },
                });
                out.push(StreamEvent::BlockStop { index });
            }
        }
    }

    fn open_text(&mut self, out: &mut Vec<StreamEvent>) -> u32 {
        let index = self.allocate();
        out.push(StreamEvent::BlockStart {
            index,
            block: BlockStart::Text,
        });
        self.open = Some(Open::Text { index });
        index
    }

    fn open_thinking(&mut self, out: &mut Vec<StreamEvent>) -> u32 {
        let index = self.allocate();
        out.push(StreamEvent::BlockStart {
            index,
            block: BlockStart::Reasoning {
                id: None,
                redacted: false,
            },
        });
        self.open = Some(Open::Thinking {
            index,
            signature: String::new(),
        });
        index
    }

    fn merge_usage(&mut self, usage: Option<&Value>, out: &mut Vec<StreamEvent>) {
        let Some(usage) = usage.filter(|usage| usage.is_object()) else {
            return;
        };
        // Every usage object is a running total and Anthropic's buckets are
        // already disjoint, so merging field by field is the conversion.
        self.usage.merge(&decode_usage(usage));
        if !self.usage.is_empty() {
            out.push(StreamEvent::Usage(self.usage));
        }
    }

    fn on_message_start(&mut self, payload: &Value, out: &mut Vec<StreamEvent>) {
        let message = payload.get("message").unwrap_or(payload);
        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start {
                id: non_empty(message, "id")
                    .map(str::to_string)
                    .unwrap_or_else(|| new_id(MESSAGE_ID_PREFIX)),
                model: str_field(message, "model").unwrap_or("").to_string(),
                created: 0,
            });
        }
        self.merge_usage(message.get("usage"), out);
    }

    fn on_block_start(&mut self, payload: &Value, out: &mut Vec<StreamEvent>) {
        self.ensure_started(out);
        self.close_open(out);
        let Some(block) = payload.get("content_block").filter(|b| b.is_object()) else {
            return;
        };
        let kind = match str_field(block, "type") {
            Some(kind) if !kind.is_empty() => kind,
            // A typeless block with text can only be text.
            _ if block.get("text").is_some() => "text",
            _ => return,
        };
        match kind {
            "text" => {
                let index = self.open_text(out);
                if let Some(text) = non_empty(block, "text") {
                    out.push(StreamEvent::TextDelta {
                        index,
                        text: text.to_string(),
                    });
                }
                if let Some(citations) = block.get("citations").and_then(Value::as_array) {
                    for citation in citations.iter().filter_map(decode_citation) {
                        out.push(StreamEvent::Citation { index, citation });
                    }
                }
            }
            "thinking" => {
                let index = self.open_thinking(out);
                if let Some(text) = non_empty(block, "thinking") {
                    out.push(StreamEvent::ReasoningDelta {
                        index,
                        text: text.to_string(),
                    });
                }
                if let Some(signature) = non_empty(block, "signature") {
                    out.push(StreamEvent::ReasoningSignature {
                        index,
                        signature: Signature::new(THIS, signature),
                    });
                    self.open = Some(Open::Thinking {
                        index,
                        signature: signature.to_string(),
                    });
                }
            }
            "redacted_thinking" => {
                let index = self.allocate();
                out.push(StreamEvent::BlockStart {
                    index,
                    block: BlockStart::Reasoning {
                        id: None,
                        redacted: true,
                    },
                });
                if let Some(data) = non_empty(block, "data") {
                    out.push(StreamEvent::ReasoningSignature {
                        index,
                        signature: Signature::new(THIS, data),
                    });
                }
                self.open = Some(Open::Redacted { index });
            }
            "tool_use" => {
                let index = self.allocate();
                self.saw_tool_call = true;
                out.push(StreamEvent::BlockStart {
                    index,
                    block: BlockStart::ToolCall {
                        id: non_empty(block, "id")
                            .map(str::to_string)
                            .unwrap_or_else(|| new_id(TOOL_ID_PREFIX)),
                        name: str_field(block, "name").unwrap_or("").to_string(),
                        kind: Default::default(),
                        signature: None,
                    },
                });
                let start_input = match block.get("input") {
                    Some(Value::Object(input)) if !input.is_empty() => {
                        Some(Value::Object(input.clone()).to_string())
                    }
                    Some(Value::String(text)) if !text.trim().is_empty() => Some(text.clone()),
                    _ => None,
                };
                self.open = Some(Open::Tool {
                    index,
                    start_input,
                    streamed: false,
                });
            }
            _ => {
                self.open = Some(Open::Other {
                    raw: block.clone(),
                    input_json: String::new(),
                });
            }
        }
    }

    fn on_block_delta(&mut self, payload: &Value, out: &mut Vec<StreamEvent>) {
        self.ensure_started(out);
        let Some(delta) = payload.get("delta").filter(|d| d.is_object()) else {
            return;
        };
        let kind = match str_field(delta, "type") {
            Some(kind) if !kind.is_empty() => kind,
            _ if delta.get("partial_json").is_some() => "input_json_delta",
            _ if delta.get("thinking").is_some() => "thinking_delta",
            _ if delta.get("signature").is_some() => "signature_delta",
            _ if delta.get("text").is_some() => "text_delta",
            _ => return,
        };
        match kind {
            "text_delta" => {
                let text = str_field(delta, "text").unwrap_or("");
                match &mut self.open {
                    Some(Open::Text { index }) => {
                        if !text.is_empty() {
                            out.push(StreamEvent::TextDelta {
                                index: *index,
                                text: text.to_string(),
                            });
                        }
                    }
                    Some(Open::Other { raw, .. }) => {
                        if let Some(Value::String(existing)) = raw.get_mut("text") {
                            existing.push_str(text);
                        }
                    }
                    _ if text.is_empty() => {}
                    // Text without (or outside) a text block: open one.
                    _ => {
                        self.close_open(out);
                        let index = self.open_text(out);
                        out.push(StreamEvent::TextDelta {
                            index,
                            text: text.to_string(),
                        });
                    }
                }
            }
            "thinking_delta" => {
                let text = str_field(delta, "thinking")
                    .or_else(|| str_field(delta, "text"))
                    .unwrap_or("");
                match &self.open {
                    Some(Open::Thinking { index, .. }) => {
                        if !text.is_empty() {
                            out.push(StreamEvent::ReasoningDelta {
                                index: *index,
                                text: text.to_string(),
                            });
                        }
                    }
                    Some(Open::Redacted { .. } | Open::Other { .. }) => {}
                    _ if text.is_empty() => {}
                    _ => {
                        self.close_open(out);
                        let index = self.open_thinking(out);
                        out.push(StreamEvent::ReasoningDelta {
                            index,
                            text: text.to_string(),
                        });
                    }
                }
            }
            "signature_delta" => {
                let fragment = str_field(delta, "signature").unwrap_or("");
                if let Some(Open::Thinking { index, signature }) = &mut self.open
                    && !fragment.is_empty()
                {
                    signature.push_str(fragment);
                    out.push(StreamEvent::ReasoningSignature {
                        index: *index,
                        signature: Signature::new(THIS, signature.clone()),
                    });
                }
            }
            "input_json_delta" => {
                let fragment = str_field(delta, "partial_json").unwrap_or("");
                match &mut self.open {
                    // The first delta of a call is usually empty.
                    Some(Open::Tool {
                        index, streamed, ..
                    }) if !fragment.is_empty() => {
                        *streamed = true;
                        out.push(StreamEvent::ToolArgsDelta {
                            index: *index,
                            fragment: fragment.to_string(),
                        });
                    }
                    Some(Open::Other { input_json, .. }) => input_json.push_str(fragment),
                    _ => {}
                }
            }
            "citations_delta" => match &mut self.open {
                Some(Open::Text { index }) => {
                    if let Some(citation) = delta.get("citation").and_then(decode_citation) {
                        out.push(StreamEvent::Citation {
                            index: *index,
                            citation,
                        });
                    }
                }
                Some(Open::Other { raw, .. }) => {
                    if let (Some(Value::Array(citations)), Some(citation)) =
                        (raw.get_mut("citations"), delta.get("citation"))
                    {
                        citations.push(citation.clone());
                    }
                }
                _ => {}
            },
            // Delta kinds added after this was written.
            _ => {}
        }
    }

    fn on_message_delta(&mut self, payload: &Value, out: &mut Vec<StreamEvent>) {
        self.ensure_started(out);
        // A server that forgets `content_block_stop` must not leave a block
        // open across the end of the message.
        self.close_open(out);
        if let Some(delta) = payload.get("delta") {
            if let Some(reason) = non_empty(delta, "stop_reason") {
                self.stop_reason = Some(reason.to_string());
            }
            if let Some(sequence) = non_empty(delta, "stop_sequence") {
                self.stop_sequence = Some(sequence.to_string());
            }
        }
        self.merge_usage(payload.get("usage"), out);
    }

    fn finish_event(&self) -> StreamEvent {
        StreamEvent::Finish {
            reason: finish_reason(self.stop_reason.as_deref(), self.saw_tool_call),
            stop_sequence: self.stop_sequence.clone(),
        }
    }

    fn on_message_stop(&mut self, out: &mut Vec<StreamEvent>) {
        self.ensure_started(out);
        self.close_open(out);
        out.push(self.finish_event());
        self.finished = true;
    }

    fn on_error(&mut self, payload: &Value, out: &mut Vec<StreamEvent>) {
        self.ensure_started(out);
        self.close_open(out);
        out.push(StreamEvent::Error(stream_error(payload)));
        self.finished = true;
    }
}

impl StreamDecoder for Decoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, CodecError> {
        let mut out = Vec::new();
        if self.finished {
            return Ok(out);
        }
        let data = event.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(out);
        }
        // A payload that is not JSON cannot be an event of this protocol.
        let Ok(payload) = serde_json::from_str::<Value>(data) else {
            return Ok(out);
        };
        let kind = match str_field(&payload, "type").filter(|kind| !kind.is_empty()) {
            Some(kind) => kind,
            None => match event.event.as_deref() {
                Some(name) if !name.is_empty() => name,
                // OpenAI-shaped in-stream error of a compatible gateway.
                _ if payload.get("error").is_some() => "error",
                _ => return Ok(out),
            },
        };
        match kind {
            "message_start" => self.on_message_start(&payload, &mut out),
            "content_block_start" => self.on_block_start(&payload, &mut out),
            "content_block_delta" => self.on_block_delta(&payload, &mut out),
            "content_block_stop" => {
                self.ensure_started(&mut out);
                self.close_open(&mut out);
            }
            "message_delta" => self.on_message_delta(&payload, &mut out),
            "message_stop" => self.on_message_stop(&mut out),
            "error" => self.on_error(&payload, &mut out),
            // `ping` and event types this decoder does not know.
            _ => {}
        }
        Ok(out)
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.ensure_started(&mut out);
        self.close_open(&mut out);
        if self.stop_reason.is_some() {
            // The message was complete; only the closing event is missing.
            out.push(self.finish_event());
        } else {
            out.push(StreamEvent::Finish {
                reason: FinishReason::Error,
                stop_sequence: None,
            });
        }
        self.finished = true;
        out
    }
}
