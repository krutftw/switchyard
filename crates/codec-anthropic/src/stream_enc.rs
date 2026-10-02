//! Client side: canonical [`StreamEvent`]s → a Messages SSE stream.

use crate::blocks::{call_signature_for_client, encode_citation, tool_call_input, tool_input};
use crate::error::encode_error;
use crate::response::{
    StopHints, ToolNames, encode_part, encode_usage, message_id, stop_reason, tool_use_id,
};
use crate::util::THIS;
use serde_json::{Value, json};
use switchyard_core::ir::{FinishReason, Part, Signature, ToolCallKind};
use switchyard_core::stream::{BlockStart, StreamEvent};
use switchyard_core::{ApiError, ClientCtx, SseEvent, StreamEncoder, Usage, sig};

/// How the arguments of the open tool call reach the client.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArgsMode {
    /// Nothing but whitespace seen so far.
    Undecided,
    /// The arguments are a JSON object: fragments are forwarded as they come.
    Streaming,
    /// The arguments are not a JSON object (free-form tool input, a bare
    /// JSON value, garbage): they are held back and sent wrapped in an
    /// object when the block closes, because `input` must be an object.
    Buffered,
}

/// The canonical block currently open. `wire` is its index on the wire once
/// its `content_block_start` has been sent.
enum Open {
    /// Text or a refusal. The start is deferred until there is something to
    /// put in the block: an empty text block is refused by the API when the
    /// client replays it.
    Text { wire: Option<u32> },
    /// Reasoning. The start is deferred until text or a signature arrives,
    /// so reasoning with neither never reaches the client as an empty,
    /// unsigned thinking block (the complete-response encoder drops it too).
    Thinking {
        wire: Option<u32>,
        /// Signature to send right before the block closes.
        signature: Option<String>,
    },
    /// Withheld reasoning. `redacted_thinking` blocks arrive complete, so
    /// the start is deferred until the payload is known.
    Redacted { wire: Option<u32> },
    Tool {
        wire: u32,
        kind: ToolCallKind,
        mode: ArgsMode,
        /// Every argument fragment received.
        buffer: String,
    },
    /// A canonical block with no wire counterpart; its events are ignored.
    Dropped,
}

/// Encoder state for one Messages stream.
///
/// * `message_start` is held back until the event after `Start`, so input
///   token counts that arrive right away (Anthropic and Gemini upstreams)
///   appear in it, as native clients expect. Later usage is reported in
///   `message_delta`, which always carries the complete running totals.
/// * Wire block indices are the canonical ones, except that blocks this
///   protocol cannot express and blocks that turn out to be empty (text
///   without text, reasoning with neither text nor signature) are skipped
///   without leaving a gap (clients index their content array by them).
/// * Tool arguments stream incrementally as `input_json_delta`; a block that
///   received none still gets one empty delta. Arguments that turn out not
///   to be a complete JSON object when the call closes downgrade a
///   `tool_use` / `end_turn` stop reason to `max_tokens`, so a client never
///   runs a half-written call.
/// * Tool names are handed back in the client's own spelling
///   ([`ToolNames`]).
/// * A tool call that carries a signature of its own is preceded by a
///   text-less `thinking` block holding it, as in a complete response.
/// * `Finish { reason: Error }` and `Error` end the stream with an `error`
///   event. A sequence that simply stops is closed with `message_delta`
///   (`stop_reason: null`) and `message_stop`.
pub(crate) struct Encoder {
    /// Model name to report (the client's), empty to use the upstream's.
    model: String,
    names: ToolNames,
    pending_start: Option<(String, String)>,
    message_started: bool,
    done: bool,
    saw_input: bool,
    usage: Usage,
    next_wire: u32,
    open: Option<Open>,
    hints: StopHints,
    truncated_tool_args: bool,
}

fn event(name: &str, payload: Value) -> SseEvent {
    SseEvent::json(Some(name), &payload)
}

fn block_start(wire: u32, block: Value) -> SseEvent {
    event(
        "content_block_start",
        json!({"type": "content_block_start", "index": wire, "content_block": block}),
    )
}

fn block_delta(wire: u32, delta: Value) -> SseEvent {
    event(
        "content_block_delta",
        json!({"type": "content_block_delta", "index": wire, "delta": delta}),
    )
}

fn block_stop(wire: u32) -> SseEvent {
    event(
        "content_block_stop",
        json!({"type": "content_block_stop", "index": wire}),
    )
}

/// Whether `text` is one complete JSON object.
fn is_json_object(text: &str) -> bool {
    matches!(serde_json::from_str::<Value>(text), Ok(Value::Object(_)))
}

impl Encoder {
    pub(crate) fn new(ctx: &ClientCtx) -> Self {
        Encoder {
            model: ctx.model.clone(),
            names: ToolNames::from_request(&ctx.request),
            pending_start: None,
            message_started: false,
            done: false,
            saw_input: false,
            usage: Usage::default(),
            next_wire: 0,
            open: None,
            hints: StopHints::default(),
            truncated_tool_args: false,
        }
    }

    fn allocate(&mut self) -> u32 {
        let wire = self.next_wire;
        self.next_wire += 1;
        wire
    }

    fn ensure_message_start(&mut self, out: &mut Vec<SseEvent>) {
        if self.message_started {
            return;
        }
        self.message_started = true;
        let (id, upstream_model) = self.pending_start.take().unwrap_or_default();
        let model = if self.model.is_empty() {
            upstream_model
        } else {
            self.model.clone()
        };
        // Initial usage: whatever is known now, without the thinking detail
        // (the API only reports it at the end).
        let usage = encode_usage(&Usage {
            reasoning_tokens: 0,
            ..self.usage
        });
        out.push(event(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": message_id(&id),
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": usage,
                },
            }),
        ));
    }

    /// The wire index of the open text block, announcing the block first if
    /// that has not happened yet. `None` when no text block is open.
    fn text_wire(&mut self, out: &mut Vec<SseEvent>) -> Option<u32> {
        let Some(Open::Text { wire }) = &mut self.open else {
            return None;
        };
        if wire.is_none() {
            *wire = Some(self.next_wire);
            out.push(block_start(
                self.next_wire,
                json!({"type": "text", "text": ""}),
            ));
            self.next_wire += 1;
        }
        *wire
    }

    /// The same for the open thinking block.
    fn thinking_wire(&mut self, out: &mut Vec<SseEvent>) -> Option<u32> {
        let Some(Open::Thinking { wire, .. }) = &mut self.open else {
            return None;
        };
        if wire.is_none() {
            *wire = Some(self.next_wire);
            out.push(block_start(
                self.next_wire,
                json!({"type": "thinking", "thinking": "", "signature": ""}),
            ));
            self.next_wire += 1;
        }
        *wire
    }

    /// The `thinking` block that carries a tool call's own signature to the
    /// client, sent right ahead of the call's `tool_use` block.
    fn call_signature(&mut self, signature: Option<&Signature>, out: &mut Vec<SseEvent>) {
        let Some(signature) = call_signature_for_client(signature) else {
            return;
        };
        let wire = self.allocate();
        out.push(block_start(
            wire,
            json!({"type": "thinking", "thinking": "", "signature": ""}),
        ));
        out.push(block_delta(
            wire,
            json!({"type": "signature_delta", "signature": signature}),
        ));
        out.push(block_stop(wire));
    }

    fn close_block(&mut self, out: &mut Vec<SseEvent>) {
        // A signature is content: a thinking block that has one is announced
        // now if no text did it earlier.
        if matches!(
            &self.open,
            Some(Open::Thinking {
                signature: Some(_),
                ..
            })
        ) {
            self.thinking_wire(out);
        }
        match self.open.take() {
            None
            | Some(Open::Dropped)
            | Some(Open::Text { wire: None })
            | Some(Open::Thinking { wire: None, .. })
            | Some(Open::Redacted { wire: None }) => {}
            Some(Open::Text { wire: Some(wire) }) | Some(Open::Redacted { wire: Some(wire) }) => {
                out.push(block_stop(wire));
            }
            Some(Open::Thinking {
                wire: Some(wire),
                signature,
            }) => {
                if let Some(signature) = signature {
                    out.push(block_delta(
                        wire,
                        json!({"type": "signature_delta", "signature": signature}),
                    ));
                }
                out.push(block_stop(wire));
            }
            Some(Open::Tool {
                wire,
                kind,
                mode,
                buffer,
            }) => {
                match mode {
                    ArgsMode::Streaming => {
                        if !is_json_object(&buffer) {
                            self.truncated_tool_args = true;
                        }
                    }
                    // No arguments at all: one empty delta, as the API does.
                    ArgsMode::Undecided if kind == ToolCallKind::Function => out.push(block_delta(
                        wire,
                        json!({"type": "input_json_delta", "partial_json": ""}),
                    )),
                    ArgsMode::Undecided | ArgsMode::Buffered => out.push(block_delta(
                        wire,
                        json!({
                            "type": "input_json_delta",
                            "partial_json": tool_input(kind, &buffer).to_string(),
                        }),
                    )),
                }
                out.push(block_stop(wire));
            }
        }
    }

    /// Emits a complete part as a start / delta / stop group.
    fn whole_part(&mut self, part: &Part, out: &mut Vec<SseEvent>) {
        match part {
            Part::Text(text) => {
                if text.text.is_empty() && text.citations.is_empty() {
                    return;
                }
                let wire = self.allocate();
                out.push(block_start(wire, json!({"type": "text", "text": ""})));
                if !text.text.is_empty() {
                    out.push(block_delta(
                        wire,
                        json!({"type": "text_delta", "text": text.text}),
                    ));
                }
                for citation in text.citations.iter().filter_map(encode_citation) {
                    out.push(block_delta(
                        wire,
                        json!({"type": "citations_delta", "citation": citation}),
                    ));
                }
                out.push(block_stop(wire));
            }
            Part::Refusal(refusal) => {
                self.hints.refusal = true;
                if refusal.text.is_empty() {
                    return;
                }
                let wire = self.allocate();
                out.push(block_start(wire, json!({"type": "text", "text": ""})));
                out.push(block_delta(
                    wire,
                    json!({"type": "text_delta", "text": refusal.text}),
                ));
                out.push(block_stop(wire));
            }
            Part::Reasoning(reasoning) => {
                let signature = reasoning
                    .signature
                    .as_ref()
                    .filter(|signature| !signature.data.is_empty())
                    .map(|signature| sig::encode_for_client(signature, THIS));
                if reasoning.redacted {
                    if let Some(data) = signature {
                        let wire = self.allocate();
                        out.push(block_start(
                            wire,
                            json!({"type": "redacted_thinking", "data": data}),
                        ));
                        out.push(block_stop(wire));
                    }
                    return;
                }
                if reasoning.text.is_empty() && signature.is_none() {
                    return;
                }
                let wire = self.allocate();
                out.push(block_start(
                    wire,
                    json!({"type": "thinking", "thinking": "", "signature": ""}),
                ));
                if !reasoning.text.is_empty() {
                    out.push(block_delta(
                        wire,
                        json!({"type": "thinking_delta", "thinking": reasoning.text}),
                    ));
                }
                if let Some(signature) = signature {
                    out.push(block_delta(
                        wire,
                        json!({"type": "signature_delta", "signature": signature}),
                    ));
                }
                out.push(block_stop(wire));
            }
            Part::ToolCall(call) => {
                self.hints.tool_use = true;
                self.call_signature(call.signature.as_ref(), out);
                let wire = self.allocate();
                out.push(block_start(
                    wire,
                    json!({
                        "type": "tool_use",
                        "id": tool_use_id(&call.id),
                        "name": self.names.restore(&call.name),
                        "input": {},
                    }),
                ));
                out.push(block_delta(
                    wire,
                    json!({
                        "type": "input_json_delta",
                        "partial_json": tool_call_input(call).to_string(),
                    }),
                ));
                out.push(block_stop(wire));
            }
            // Blocks of this vendor travel verbatim; media, tool results and
            // other vendors' blocks cannot appear in a Messages stream.
            other => {
                if let Some(block) = encode_part(other, &self.names) {
                    let wire = self.allocate();
                    out.push(block_start(wire, block));
                    out.push(block_stop(wire));
                }
            }
        }
    }

    fn on_block_start(&mut self, block: &BlockStart, out: &mut Vec<SseEvent>) {
        // The contract says the previous block is closed; do not rely on it.
        self.close_block(out);
        match block {
            BlockStart::Text => self.open = Some(Open::Text { wire: None }),
            BlockStart::Refusal => {
                self.hints.refusal = true;
                self.open = Some(Open::Text { wire: None });
            }
            BlockStart::Reasoning {
                redacted: false, ..
            } => {
                self.open = Some(Open::Thinking {
                    wire: None,
                    signature: None,
                });
            }
            BlockStart::Reasoning { redacted: true, .. } => {
                self.open = Some(Open::Redacted { wire: None });
            }
            BlockStart::ToolCall {
                id,
                name,
                kind,
                signature,
            } => {
                self.hints.tool_use = true;
                self.call_signature(signature.as_ref(), out);
                let wire = self.allocate();
                out.push(block_start(
                    wire,
                    json!({
                        "type": "tool_use",
                        "id": tool_use_id(id),
                        "name": self.names.restore(name),
                        "input": {},
                    }),
                ));
                self.open = Some(Open::Tool {
                    wire,
                    kind: *kind,
                    mode: if *kind == ToolCallKind::Custom {
                        ArgsMode::Buffered
                    } else {
                        ArgsMode::Undecided
                    },
                    buffer: String::new(),
                });
            }
            BlockStart::Whole { part } => {
                self.whole_part(part, out);
                self.open = Some(Open::Dropped);
            }
        }
    }

    fn on_tool_args(&mut self, fragment: &str, out: &mut Vec<SseEvent>) {
        let Some(Open::Tool {
            wire, mode, buffer, ..
        }) = &mut self.open
        else {
            return;
        };
        buffer.push_str(fragment);
        match *mode {
            ArgsMode::Buffered => {}
            ArgsMode::Streaming => {
                if !fragment.is_empty() {
                    out.push(block_delta(
                        *wire,
                        json!({"type": "input_json_delta", "partial_json": fragment}),
                    ));
                }
            }
            ArgsMode::Undecided => match buffer.trim_start().chars().next() {
                None => {}
                Some('{') => {
                    *mode = ArgsMode::Streaming;
                    out.push(block_delta(
                        *wire,
                        json!({"type": "input_json_delta", "partial_json": buffer.as_str()}),
                    ));
                }
                Some(_) => *mode = ArgsMode::Buffered,
            },
        }
    }

    fn error_event(&mut self, error: &ApiError, out: &mut Vec<SseEvent>) {
        out.push(event("error", encode_error(error)));
        self.done = true;
    }

    fn message_end(
        &mut self,
        reason: Option<&'static str>,
        stop_sequence: Option<&str>,
        out: &mut Vec<SseEvent>,
    ) {
        let stop_sequence = stop_sequence.filter(|_| reason == Some("stop_sequence"));
        out.push(event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": reason, "stop_sequence": stop_sequence},
                "usage": encode_usage(&self.usage),
            }),
        ));
        out.push(event("message_stop", json!({"type": "message_stop"})));
        self.done = true;
    }
}

impl StreamEncoder for Encoder {
    fn encode(&mut self, event: &StreamEvent) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        self.saw_input = true;
        match event {
            StreamEvent::Start { id, model, .. } => {
                if !self.message_started && self.pending_start.is_none() {
                    self.pending_start = Some((id.clone(), model.clone()));
                }
                return out;
            }
            StreamEvent::Usage(usage) => {
                self.usage.merge(usage);
                // Usage right after `Start` is the input side of the count:
                // it belongs in `message_start`.
                self.ensure_message_start(&mut out);
                return out;
            }
            // An error before anything was announced is just the error.
            StreamEvent::Error(_) if self.pending_start.is_none() => {}
            _ => self.ensure_message_start(&mut out),
        }
        match event {
            StreamEvent::Start { .. } | StreamEvent::Usage(_) => {}
            StreamEvent::BlockStart { block, .. } => self.on_block_start(block, &mut out),
            StreamEvent::TextDelta { text, .. } => {
                if !text.is_empty()
                    && let Some(wire) = self.text_wire(&mut out)
                {
                    out.push(block_delta(
                        wire,
                        json!({"type": "text_delta", "text": text}),
                    ));
                }
            }
            StreamEvent::ReasoningDelta { text, .. } => {
                if !text.is_empty()
                    && let Some(wire) = self.thinking_wire(&mut out)
                {
                    out.push(block_delta(
                        wire,
                        json!({"type": "thinking_delta", "thinking": text}),
                    ));
                }
            }
            // A signature without data signs nothing; it is ignored, as in a
            // complete response.
            StreamEvent::ReasoningSignature { signature, .. } if signature.data.is_empty() => {}
            StreamEvent::ReasoningSignature { signature, .. } => {
                let encoded = sig::encode_for_client(signature, THIS);
                match &mut self.open {
                    Some(Open::Thinking { signature, .. }) => *signature = Some(encoded),
                    Some(Open::Redacted { wire: None }) => {
                        let wire = self.allocate();
                        out.push(block_start(
                            wire,
                            json!({"type": "redacted_thinking", "data": encoded}),
                        ));
                        self.open = Some(Open::Redacted { wire: Some(wire) });
                    }
                    _ => {}
                }
            }
            StreamEvent::ToolArgsDelta { fragment, .. } => self.on_tool_args(fragment, &mut out),
            StreamEvent::Citation { citation, .. } => {
                if let Some(citation) = encode_citation(citation)
                    && let Some(wire) = self.text_wire(&mut out)
                {
                    out.push(block_delta(
                        wire,
                        json!({"type": "citations_delta", "citation": citation}),
                    ));
                }
            }
            StreamEvent::BlockStop { .. } => self.close_block(&mut out),
            StreamEvent::Finish {
                reason,
                stop_sequence,
            } => {
                self.close_block(&mut out);
                if *reason == FinishReason::Error {
                    self.error_event(
                        &ApiError::upstream("the upstream response ended before it was complete"),
                        &mut out,
                    );
                    return out;
                }
                let hints = StopHints {
                    stop_sequence: stop_sequence.is_some(),
                    ..self.hints
                };
                let mut wire_reason = stop_reason(reason, hints);
                if self.truncated_tool_args && matches!(wire_reason, Some("tool_use" | "end_turn"))
                {
                    wire_reason = Some("max_tokens");
                }
                self.message_end(wire_reason, stop_sequence.as_deref(), &mut out);
            }
            StreamEvent::Error(error) => self.error_event(error, &mut out),
        }
        out
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if self.done || !self.saw_input {
            return out;
        }
        self.ensure_message_start(&mut out);
        self.close_block(&mut out);
        // The sequence stopped without saying why.
        self.message_end(None, None, &mut out);
        out
    }
}
