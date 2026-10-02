//! Stream encoder: canonical [`StreamEvent`]s into the `chat.completion.chunk`
//! events a Chat client expects.
//!
//! Wire shape, in order:
//!
//! 1. a first chunk whose delta carries `role: "assistant"`;
//! 2. content chunks: `delta.content`, `delta.reasoning_content`,
//!    `delta.refusal`, `delta.tool_calls` (a header with id and name, then
//!    argument fragments addressed by `index`), plus the `annotations`,
//!    `images` and `reasoning_details` extensions;
//! 3. one chunk with an empty delta and the `finish_reason`;
//! 4. when the client sent `stream_options.include_usage: true`, one chunk
//!    with `choices: []` and the `usage` object (and every earlier chunk then
//!    carries `usage: null`, as OpenAI does);
//! 5. `data: [DONE]`.
//!
//! Reasoning text is sent twice in the same delta, as OpenRouter does: in
//! `reasoning_content` (what DeepSeek-style clients display) and mirrored in
//! a `reasoning_details` entry `{"type":"reasoning.text","text":…,"index":n}`.
//! The entry's `index` numbers the reasoning blocks of the response, which
//! is the only thing that keeps two consecutive blocks apart on the wire, and
//! a client that replays the accumulated `reasoning_details` (the documented
//! way to preserve reasoning across turns) sends back text and signature
//! together; a signature without its text is useless to the vendor that
//! issued it. Signatures and encrypted payloads follow in an entry with the
//! same `index`.
//!
//! A stream that fails is ended by a `data: {"error":{…}}` frame and **no**
//! `[DONE]`. The error frame is self-contained: an encoder that has not
//! started yet emits nothing but that frame (no role chunk with a freshly
//! minted id), because the gateway also uses a fresh encoder to append an
//! error to a passthrough stream whose chunks the client already received.
//! A sequence that simply stops without a terminal event is closed with
//! `finish_reason: "length"` (the answer is incomplete) and `[DONE]`.

use crate::common::{
    PROTOCOL, citation_to_wire, client_response_id, finish_to_wire, image_to_wire,
    reconcile_finish, usage_to_wire,
};
use crate::error::encode_error;
use serde_json::{Map, Value, json};
use switchyard_core::codec::{ClientCtx, StreamEncoder};
use switchyard_core::ir::{FinishReason, Part, Reasoning, Signature, ToolCallKind};
use switchyard_core::sse::SseEvent;
use switchyard_core::stream::{BlockStart, StreamEvent};
use switchyard_core::util::{new_call_id, now_unix};
use switchyard_core::{Usage, sig};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Block {
    /// No block is open, or the open one has no Chat representation.
    None,
    Text,
    Refusal,
    Reasoning {
        ordinal: u32,
        redacted: bool,
    },
    Tool {
        wire_index: u32,
        kind: ToolCallKind,
        /// Whether any argument text has been sent for this call.
        sent_args: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Terminal {
    Open,
    /// `Finish` was rendered; `[DONE]` is still owed.
    Finished,
    /// An error frame was sent; nothing may follow it.
    Errored,
    /// `finish()` already ran.
    Closed,
}

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct ChatStreamEncoder {
    model: String,
    include_usage: bool,
    id: String,
    created: i64,
    started: bool,
    terminal: Terminal,
    block: Block,
    /// Provider id of the open reasoning block, repeated on its details.
    reasoning_id: Option<String>,
    next_tool: u32,
    next_reasoning: u32,
    next_image: usize,
    /// Characters of `content` sent so far / before the open text block;
    /// citation offsets are relative to the whole message content.
    content_chars: u64,
    block_offset: u64,
    usage: Usage,
    saw_tool_call: bool,
}

impl ChatStreamEncoder {
    pub(crate) fn new(ctx: &ClientCtx) -> Self {
        let include_usage = ctx
            .request
            .get("stream_options")
            .and_then(|options| options.get("include_usage"))
            == Some(&Value::Bool(true));
        ChatStreamEncoder {
            model: ctx.model.clone(),
            include_usage,
            id: String::new(),
            created: 0,
            started: false,
            terminal: Terminal::Open,
            block: Block::None,
            reasoning_id: None,
            next_tool: 0,
            next_reasoning: 0,
            next_image: 0,
            content_chars: 0,
            block_offset: 0,
            usage: Usage::default(),
            saw_tool_call: false,
        }
    }

    fn chunk(&self, choices: Value, usage: Option<Value>) -> SseEvent {
        let mut chunk = Map::new();
        chunk.insert("id".into(), json!(self.id));
        chunk.insert("object".into(), json!("chat.completion.chunk"));
        chunk.insert("created".into(), json!(self.created));
        chunk.insert("model".into(), json!(self.model));
        chunk.insert("choices".into(), choices);
        if self.include_usage {
            chunk.insert("usage".into(), usage.unwrap_or(Value::Null));
        }
        SseEvent::json(None, &Value::Object(chunk))
    }

    fn delta_chunk(&self, delta: Value) -> SseEvent {
        self.chunk(
            json!([{"index": 0, "delta": delta, "logprobs": null, "finish_reason": null}]),
            None,
        )
    }

    fn start(&mut self, id: &str, model: &str, created: i64, out: &mut Vec<SseEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        self.id = client_response_id(id);
        self.created = if created > 0 { created } else { now_unix() };
        if self.model.is_empty() {
            self.model = model.to_string();
        }
        out.push(self.delta_chunk(json!({"role": "assistant", "content": ""})));
    }

    /// Tolerates a sequence that lacks its `Start`.
    fn ensure_started(&mut self, out: &mut Vec<SseEvent>) {
        if !self.started {
            self.start("", "", 0, out);
        }
    }

    fn tool_header(
        &mut self,
        id: &str,
        name: &str,
        kind: ToolCallKind,
        signature: Option<&Signature>,
        arguments: &str,
    ) -> (u32, SseEvent) {
        let wire_index = self.next_tool;
        self.next_tool += 1;
        self.saw_tool_call = true;
        let id = if id.is_empty() {
            new_call_id()
        } else {
            id.to_string()
        };
        let mut call = Map::new();
        call.insert("index".into(), json!(wire_index));
        call.insert("id".into(), json!(id));
        match kind {
            ToolCallKind::Function => {
                call.insert("type".into(), json!("function"));
                call.insert(
                    "function".into(),
                    json!({"name": name, "arguments": arguments}),
                );
            }
            ToolCallKind::Custom => {
                call.insert("type".into(), json!("custom"));
                call.insert("custom".into(), json!({"name": name, "input": arguments}));
            }
        }
        if let Some(signature) = signature {
            call.insert(
                "extra_content".into(),
                json!({"google": {"thought_signature": sig::encode_for_client(signature, PROTOCOL)}}),
            );
        }
        (
            wire_index,
            self.delta_chunk(json!({"tool_calls": [Value::Object(call)]})),
        )
    }

    fn signature_chunk(
        &self,
        ordinal: u32,
        redacted: bool,
        id: Option<&str>,
        signature: &Signature,
    ) -> SseEvent {
        let blob = sig::encode_for_client(signature, PROTOCOL);
        let mut detail = Map::new();
        if redacted {
            detail.insert("type".into(), json!("reasoning.encrypted"));
            detail.insert("data".into(), json!(blob));
        } else {
            detail.insert("type".into(), json!("reasoning.text"));
            detail.insert("signature".into(), json!(blob));
        }
        if let Some(id) = id {
            detail.insert("id".into(), json!(id));
        }
        detail.insert("index".into(), json!(ordinal));
        self.delta_chunk(json!({"reasoning_details": [Value::Object(detail)]}))
    }

    /// A piece of reasoning text of block `ordinal`: `reasoning_content`
    /// plus the mirroring `reasoning_details` entry (see the module
    /// documentation). A redacted block has no text of its own; text that
    /// arrives for one anyway is only shown, not mirrored, so the block
    /// stays a single encrypted entry.
    fn reasoning_text_chunk(
        &self,
        ordinal: u32,
        redacted: bool,
        id: Option<&str>,
        text: &str,
    ) -> SseEvent {
        if redacted {
            return self.delta_chunk(json!({"reasoning_content": text}));
        }
        let mut detail = Map::new();
        detail.insert("type".into(), json!("reasoning.text"));
        detail.insert("text".into(), json!(text));
        if let Some(id) = id {
            detail.insert("id".into(), json!(id));
        }
        detail.insert("index".into(), json!(ordinal));
        self.delta_chunk(json!({
            "reasoning_content": text,
            "reasoning_details": [Value::Object(detail)]
        }))
    }

    fn content_chunk(&mut self, text: &str) -> Option<SseEvent> {
        if text.is_empty() {
            return None;
        }
        self.content_chars += text.chars().count() as u64;
        Some(self.delta_chunk(json!({"content": text})))
    }

    fn annotation_chunk(&self, citation: &switchyard_core::ir::Citation) -> Option<SseEvent> {
        citation_to_wire(citation, self.block_offset)
            .map(|annotation| self.delta_chunk(json!({"annotations": [annotation]})))
    }

    fn image_chunk(&mut self, media: &switchyard_core::ir::MediaPart) -> Option<SseEvent> {
        let image = image_to_wire(media, self.next_image)?;
        self.next_image += 1;
        Some(self.delta_chunk(json!({"images": [image]})))
    }

    fn reasoning_whole(&mut self, r: &Reasoning, out: &mut Vec<SseEvent>) {
        let ordinal = self.next_reasoning;
        self.next_reasoning += 1;
        if !r.text.is_empty() && !r.redacted {
            out.push(self.reasoning_text_chunk(ordinal, false, r.id.as_deref(), &r.text));
        }
        if let Some(signature) = &r.signature {
            out.push(self.signature_chunk(ordinal, r.redacted, r.id.as_deref(), signature));
        }
    }

    fn block_start(&mut self, block: &BlockStart, out: &mut Vec<SseEvent>) {
        self.block = Block::None;
        self.reasoning_id = None;
        match block {
            BlockStart::Text => {
                self.block = Block::Text;
                self.block_offset = self.content_chars;
            }
            BlockStart::Refusal => self.block = Block::Refusal,
            BlockStart::Reasoning { id, redacted } => {
                self.block = Block::Reasoning {
                    ordinal: self.next_reasoning,
                    redacted: *redacted,
                };
                self.next_reasoning += 1;
                self.reasoning_id = id.clone();
            }
            BlockStart::ToolCall {
                id,
                name,
                kind,
                signature,
            } => {
                let (wire_index, event) = self.tool_header(id, name, *kind, signature.as_ref(), "");
                out.push(event);
                self.block = Block::Tool {
                    wire_index,
                    kind: *kind,
                    sent_args: false,
                };
            }
            // A complete part: rendered at once, no deltas follow.
            BlockStart::Whole { part } => match part {
                Part::Text(t) => {
                    self.block_offset = self.content_chars;
                    out.extend(self.content_chunk(&t.text));
                    out.extend(t.citations.iter().filter_map(|c| self.annotation_chunk(c)));
                }
                Part::Refusal(r) if !r.text.is_empty() => {
                    out.push(self.delta_chunk(json!({"refusal": r.text})));
                }
                Part::Reasoning(r) => self.reasoning_whole(r, out),
                Part::ToolCall(call) => {
                    let arguments = match call.kind {
                        ToolCallKind::Function if call.arguments.trim().is_empty() => "{}",
                        _ => call.arguments.as_str(),
                    };
                    let (_, event) = self.tool_header(
                        &call.id,
                        &call.name,
                        call.kind,
                        call.signature.as_ref(),
                        arguments,
                    );
                    out.push(event);
                }
                Part::Image(media) => out.extend(self.image_chunk(media)),
                // No Chat representation.
                Part::Refusal(_)
                | Part::Audio(_)
                | Part::Document(_)
                | Part::ToolResult(_)
                | Part::Opaque(_) => {}
            },
        }
    }

    /// The finish chunk and, if the client asked for it, the usage chunk.
    fn finish_chunks(&mut self, reason: &FinishReason, out: &mut Vec<SseEvent>) {
        let reason = reconcile_finish(reason.clone(), self.saw_tool_call);
        out.push(self.chunk(
            json!([{
                "index": 0,
                "delta": {},
                "logprobs": null,
                "finish_reason": finish_to_wire(&reason),
            }]),
            None,
        ));
        if self.include_usage {
            out.push(self.chunk(json!([]), Some(usage_to_wire(&self.usage))));
        }
        self.terminal = Terminal::Finished;
    }
}

impl StreamEncoder for ChatStreamEncoder {
    fn encode(&mut self, event: &StreamEvent) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if self.terminal != Terminal::Open {
            return out;
        }
        if let StreamEvent::Start { id, model, created } = event {
            self.start(id, model, *created, &mut out);
            return out;
        }
        if let StreamEvent::Error(error) = event {
            // Deliberately before `ensure_started`: the error frame stands
            // on its own (see the module documentation).
            out.push(SseEvent::json(None, &encode_error(error)));
            self.terminal = Terminal::Errored;
            return out;
        }
        self.ensure_started(&mut out);
        match event {
            StreamEvent::Start { .. } => {}
            StreamEvent::BlockStart { block, .. } => self.block_start(block, &mut out),
            StreamEvent::TextDelta { text, .. } => match self.block {
                Block::Text => out.extend(self.content_chunk(text)),
                Block::Refusal if !text.is_empty() => {
                    out.push(self.delta_chunk(json!({"refusal": text})));
                }
                _ => {}
            },
            StreamEvent::ReasoningDelta { text, .. } => {
                if let Block::Reasoning { ordinal, redacted } = self.block
                    && !text.is_empty()
                {
                    out.push(self.reasoning_text_chunk(
                        ordinal,
                        redacted,
                        self.reasoning_id.as_deref(),
                        text,
                    ));
                }
            }
            StreamEvent::ReasoningSignature { signature, .. } => {
                if let Block::Reasoning { ordinal, redacted } = self.block {
                    out.push(self.signature_chunk(
                        ordinal,
                        redacted,
                        self.reasoning_id.as_deref(),
                        signature,
                    ));
                }
            }
            StreamEvent::ToolArgsDelta { fragment, .. } => {
                if let Block::Tool {
                    wire_index,
                    kind,
                    sent_args,
                } = &mut self.block
                    && !fragment.is_empty()
                {
                    *sent_args = true;
                    let call = match kind {
                        ToolCallKind::Function => {
                            json!({"index": wire_index, "function": {"arguments": fragment}})
                        }
                        ToolCallKind::Custom => {
                            json!({"index": wire_index, "custom": {"input": fragment}})
                        }
                    };
                    out.push(self.delta_chunk(json!({"tool_calls": [call]})));
                }
            }
            StreamEvent::Citation { citation, .. } => {
                if self.block == Block::Text {
                    out.extend(self.annotation_chunk(citation));
                }
            }
            StreamEvent::BlockStop { .. } => {
                // A call without arguments is the empty string in the IR, but
                // clients parse what they accumulated as JSON.
                if let Block::Tool {
                    wire_index,
                    kind: ToolCallKind::Function,
                    sent_args: false,
                } = self.block
                {
                    out.push(self.delta_chunk(json!({
                        "tool_calls": [{"index": wire_index, "function": {"arguments": "{}"}}]
                    })));
                }
                self.block = Block::None;
                self.reasoning_id = None;
            }
            StreamEvent::Usage(usage) => self.usage.merge(usage),
            StreamEvent::Finish { reason, .. } => self.finish_chunks(reason, &mut out),
            // Handled above.
            StreamEvent::Error(_) => {}
        }
        out
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        match self.terminal {
            // An error frame ends the stream; `[DONE]` would tell the client
            // the response completed.
            Terminal::Errored | Terminal::Closed => return out,
            Terminal::Finished => {}
            Terminal::Open => {
                self.ensure_started(&mut out);
                self.finish_chunks(&FinishReason::Error, &mut out);
            }
        }
        // A second call must not emit a second terminator.
        self.terminal = Terminal::Closed;
        out.push(SseEvent::data("[DONE]"));
        out
    }
}
