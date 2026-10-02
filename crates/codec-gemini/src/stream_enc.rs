//! Encoder for a Gemini client stream.
//!
//! Every canonical delta becomes one `GenerateContentResponse` chunk holding
//! a single part (`candidates[0].content.parts[0]`, role `model`, index 0).
//! Tool-call arguments are buffered and emitted as one complete
//! `functionCall` part when the block closes, because Gemini clients expect
//! whole calls with object arguments. The last chunk carries `finishReason`
//! and `usageMetadata`. Every chunk carries `modelVersion` and `responseId`.
//! There is no terminator; an error is a `{"error": {...}}` payload.

use crate::error::encode_error;
use crate::parts::candidate_metadata;
use crate::response::{
    GroundingBuilder, client_function_call, client_part, client_reasoning, encode_finish,
    encode_usage,
};
use serde_json::{Map, Value, json};
use switchyard_core::ir::{FinishReason, Part, Signature, ToolCall};
use switchyard_core::stream::{BlockStart, StreamEvent};
use switchyard_core::util::new_id;
use switchyard_core::{ClientCtx, Family, SseEvent, StreamEncoder, Usage};

enum Block {
    None,
    Text,
    Reasoning {
        has_text: bool,
    },
    Tool(ToolCall),
    /// A block with nothing more to say (a whole part, already emitted).
    Done,
}

/// Stateful encoder for one Gemini client stream.
pub(crate) struct GeminiStreamEncoder {
    model: String,
    id: String,
    started: bool,
    done: bool,
    block: Block,
    /// Text of the open text block, for citation offsets.
    block_text: String,
    usage: Usage,
    metadata: Vec<(&'static str, Value)>,
    grounding: GroundingBuilder,
}

impl GeminiStreamEncoder {
    pub(crate) fn new(ctx: &ClientCtx) -> Self {
        GeminiStreamEncoder {
            model: ctx.model.clone(),
            id: String::new(),
            started: false,
            done: false,
            block: Block::None,
            block_text: String::new(),
            usage: Usage::default(),
            metadata: Vec::new(),
            grounding: GroundingBuilder::default(),
        }
    }

    fn ensure_started(&mut self) {
        self.started = true;
        if self.id.is_empty() {
            // Gemini response ids carry no prefix.
            self.id = new_id("");
        }
    }

    fn chunk(&self, part: Value) -> SseEvent {
        SseEvent::json(
            None,
            &json!({
                "candidates": [{"content": {"parts": [part], "role": "model"}, "index": 0}],
                "modelVersion": self.model,
                "responseId": self.id,
            }),
        )
    }

    /// The closing chunk: finish reason, candidate-level metadata and usage.
    fn last_chunk(&mut self, reason: &FinishReason) -> SseEvent {
        let mut candidate = Map::new();
        candidate.insert(
            "content".to_string(),
            json!({"parts": [{"text": ""}], "role": "model"}),
        );
        candidate.insert(
            "finishReason".to_string(),
            Value::String(encode_finish(reason)),
        );
        candidate.insert("index".to_string(), Value::from(0));
        let mut metadata = std::mem::take(&mut self.metadata);
        if !metadata.iter().any(|(key, _)| *key == "groundingMetadata")
            && let Some(built) = std::mem::take(&mut self.grounding).build()
        {
            metadata.push(("groundingMetadata", built));
        }
        for (key, value) in metadata {
            candidate.insert(key.to_string(), value);
        }
        SseEvent::json(
            None,
            &json!({
                "candidates": [candidate],
                "usageMetadata": encode_usage(&self.usage),
                "modelVersion": self.model,
                "responseId": self.id,
            }),
        )
    }

    /// Emits the buffered function call, if one is open.
    fn flush_tool(&mut self, out: &mut Vec<SseEvent>) {
        if let Block::Tool(call) = std::mem::replace(&mut self.block, Block::None) {
            out.push(self.chunk(client_function_call(&call)));
        }
    }

    fn signature_chunk(&self, signature: &Signature, thought: bool) -> Option<SseEvent> {
        client_reasoning("", Some(signature), thought).map(|part| self.chunk(part))
    }
}

impl StreamEncoder for GeminiStreamEncoder {
    fn encode(&mut self, event: &StreamEvent) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        match event {
            StreamEvent::Start { id, model, .. } => {
                self.id = id.clone();
                if self.model.is_empty() {
                    self.model = model.clone();
                }
                self.ensure_started();
            }
            StreamEvent::BlockStart { block, .. } => {
                self.ensure_started();
                // A block that was never closed still gets its call out.
                self.flush_tool(&mut out);
                self.block_text.clear();
                self.block = match block {
                    BlockStart::Text | BlockStart::Refusal => Block::Text,
                    BlockStart::Reasoning { .. } => Block::Reasoning { has_text: false },
                    BlockStart::ToolCall {
                        id,
                        name,
                        kind,
                        signature,
                    } => Block::Tool(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                        kind: *kind,
                        signature: signature.clone(),
                        cache_control: None,
                    }),
                    BlockStart::Whole { part } => {
                        match part {
                            Part::Opaque(opaque) if opaque.origin.family() == Family::Google => {
                                if let Some((key, value)) = candidate_metadata(&opaque.raw) {
                                    // Lives on the candidate, not in `parts`.
                                    self.metadata.retain(|(existing, _)| *existing != key);
                                    self.metadata.push((key, value.clone()));
                                } else if let Some(rendered) = client_part(part) {
                                    out.push(self.chunk(rendered));
                                }
                            }
                            other => {
                                if let Some(rendered) = client_part(other) {
                                    out.push(self.chunk(rendered));
                                }
                            }
                        }
                        Block::Done
                    }
                };
            }
            StreamEvent::TextDelta { text, .. } => {
                self.ensure_started();
                if !text.is_empty() && matches!(self.block, Block::Text) {
                    self.block_text.push_str(text);
                    out.push(self.chunk(json!({"text": text})));
                }
            }
            StreamEvent::ReasoningDelta { text, .. } => {
                self.ensure_started();
                if !text.is_empty()
                    && let Block::Reasoning { has_text } = &mut self.block
                {
                    *has_text = true;
                    out.push(self.chunk(json!({"text": text, "thought": true})));
                }
            }
            StreamEvent::ReasoningSignature { signature, .. } => {
                self.ensure_started();
                if let Block::Reasoning { has_text } = &self.block {
                    // After thought text the signature closes that thought;
                    // on its own it is Gemini's plain signature carrier part.
                    out.extend(self.signature_chunk(signature, *has_text));
                }
            }
            StreamEvent::ToolArgsDelta { fragment, .. } => {
                if let Block::Tool(call) = &mut self.block {
                    call.arguments.push_str(fragment);
                }
            }
            StreamEvent::Citation { citation, .. } => {
                if matches!(self.block, Block::Text) {
                    self.grounding.add(citation, &self.block_text, 0);
                }
            }
            StreamEvent::BlockStop { .. } => {
                self.flush_tool(&mut out);
                self.block = Block::None;
            }
            StreamEvent::Usage(usage) => self.usage.merge(usage),
            StreamEvent::Finish { reason, .. } => {
                self.ensure_started();
                self.flush_tool(&mut out);
                out.push(self.last_chunk(reason));
                self.done = true;
            }
            StreamEvent::Error(error) => {
                out.push(SseEvent::json(None, &encode_error(error)));
                self.done = true;
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        // Gemini streams have no terminator. A sequence that stopped without
        // a terminal event is closed with an abnormal finish reason so the
        // client does not mistake the cut-off answer for a complete one; a
        // half-received function call is not delivered.
        if self.done || !self.started {
            return Vec::new();
        }
        self.done = true;
        self.block = Block::None;
        vec![self.last_chunk(&FinishReason::Other("OTHER".to_string()))]
    }
}
