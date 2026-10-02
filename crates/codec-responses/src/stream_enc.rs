//! Canonical stream events → Responses streaming events.
//!
//! The emitted sequence is the documented one:
//!
//! ```text
//! response.created, response.in_progress
//! per output item:
//!   response.output_item.added
//!     message:   (content_part.added, output_text.delta*, output_text.done, content_part.done)+
//!                with refusal.delta / refusal.done for refusal parts
//!     reasoning: reasoning_summary_part.added, reasoning_summary_text.delta*,
//!                reasoning_summary_text.done, reasoning_summary_part.done   (only when there is text)
//!     function:  function_call_arguments.delta*, function_call_arguments.done
//!     custom:    custom_tool_call_input.delta*, custom_tool_call_input.done
//!   response.output_item.done
//! response.completed | response.incomplete | response.failed
//! ```
//!
//! Every event has an SSE event name equal to its `type` and a
//! `sequence_number` that starts at 0 and increases by one. `output_index`
//! counts items in the order they open; an item is always closed before the
//! next one opens. Adjacent text / refusal blocks share one `message` item
//! (one content part each, `content_index` counting up), so the message's
//! `output_item.done` is only sent once something else follows or the
//! response ends. A tool call's closing events wait for the next canonical
//! event in the same way, because only that tells whether the call was
//! finished (`completed`) or is where generation was cut off (`incomplete`,
//! arguments left as they are). There is no `[DONE]` sentinel on this
//! protocol.

use crate::common::{
    ToolIndex, annotation_from_citation, call_signature_for_client, id_base, response_id,
    signature_for_client,
};
use crate::error::stream_error_event;
use crate::response::{
    Shell, ToolView, cuts_output, message_item, presentable_finish, reasoning_item,
    reasoning_item_id, refusal_part, status_for, text_part, whole_item,
};
use serde_json::{Map, Value, json};
use switchyard_core::ir::{Citation, FinishReason, Part};
use switchyard_core::stream::{BlockStart, StreamEvent};
use switchyard_core::util::now_unix;
use switchyard_core::{ApiError, ClientCtx, SseEvent, StreamEncoder, Usage};

/// The message item currently accepting content parts.
struct OpenMessage {
    id: String,
    output_index: usize,
    content: Vec<Value>,
}

/// The canonical block being rendered.
enum Block {
    None,
    /// A text or refusal content part of the open message.
    Text {
        content_index: usize,
        text: String,
        annotations: Vec<Value>,
        refusal: bool,
    },
    Reasoning {
        id: String,
        output_index: usize,
        text: String,
        blob: Option<String>,
        redacted: bool,
        part_open: bool,
    },
    Tool {
        view: ToolView,
        output_index: usize,
        arguments: String,
    },
    /// A block that was rendered in one go or cannot be expressed; its
    /// remaining events are ignored.
    Skipped,
}

/// A tool call whose block has stopped but whose closing events are held
/// back until the next canonical event shows whether it was completed or
/// was the point where generation got cut off.
struct StoppedTool {
    view: ToolView,
    output_index: usize,
    arguments: String,
}

/// Stateful Responses stream encoder.
pub(crate) struct Encoder {
    ctx: ClientCtx,
    tools: ToolIndex,
    sequence: u64,
    started: bool,
    finished: bool,
    id: String,
    base: String,
    created: i64,
    upstream_model: String,
    output: Vec<Value>,
    usage: Usage,
    saw_refusal: bool,
    message: Option<OpenMessage>,
    block: Block,
    stopped_tool: Option<StoppedTool>,
}

impl Encoder {
    pub(crate) fn new(ctx: &ClientCtx) -> Self {
        Encoder {
            tools: ToolIndex::from_request(&ctx.request),
            ctx: ctx.clone(),
            sequence: 0,
            started: false,
            finished: false,
            id: String::new(),
            base: String::new(),
            created: 0,
            upstream_model: String::new(),
            output: Vec::new(),
            usage: Usage::default(),
            saw_refusal: false,
            message: None,
            block: Block::None,
            stopped_tool: None,
        }
    }

    /// Appends one wire event: `type` and `sequence_number` first, then the
    /// event's own fields.
    fn emit(&mut self, out: &mut Vec<SseEvent>, kind: &str, fields: Value) {
        let mut event = Map::new();
        event.insert("type".into(), json!(kind));
        event.insert("sequence_number".into(), json!(self.sequence));
        self.sequence += 1;
        if let Value::Object(fields) = fields {
            event.extend(fields);
        }
        out.push(SseEvent::json(Some(kind), &Value::Object(event)));
    }

    fn response_object(&self, finish: Option<&FinishReason>, output: Vec<Value>) -> Value {
        let shell = Shell {
            ctx: &self.ctx,
            id: &self.id,
            created: self.created,
            upstream_model: &self.upstream_model,
            service_tier: None,
        };
        shell.render(finish, output, finish.map(|_| &self.usage))
    }

    fn start(&mut self, out: &mut Vec<SseEvent>, id: &str, model: &str, created: i64) {
        if self.started {
            return;
        }
        self.started = true;
        self.id = response_id(id);
        self.base = id_base(&self.id).to_string();
        self.created = if created > 0 { created } else { now_unix() };
        self.upstream_model = model.to_string();
        let response = self.response_object(None, Vec::new());
        self.emit(
            out,
            "response.created",
            json!({"response": response.clone()}),
        );
        self.emit(out, "response.in_progress", json!({"response": response}));
    }

    fn close_message(&mut self, out: &mut Vec<SseEvent>, status: &str) {
        let Some(message) = self.message.take() else {
            return;
        };
        let item = message_item(&message.id, status, message.content);
        self.emit(
            out,
            "response.output_item.done",
            json!({"output_index": message.output_index, "item": item.clone()}),
        );
        self.output.push(item);
    }

    fn open_part(&mut self, out: &mut Vec<SseEvent>, refusal: bool) {
        if self.message.is_none() {
            let output_index = self.output.len();
            let id = format!("msg_{}_{output_index}", self.base);
            self.emit(
                out,
                "response.output_item.added",
                json!({"output_index": output_index, "item": message_item(&id, "in_progress", Vec::new())}),
            );
            self.message = Some(OpenMessage {
                id,
                output_index,
                content: Vec::new(),
            });
        }
        let Some(message) = &self.message else {
            return;
        };
        let content_index = message.content.len();
        let part = if refusal {
            refusal_part("")
        } else {
            text_part("", Vec::new())
        };
        let fields = json!({
            "item_id": message.id,
            "output_index": message.output_index,
            "content_index": content_index,
            "part": part,
        });
        self.emit(out, "response.content_part.added", fields);
        self.saw_refusal |= refusal;
        self.block = Block::Text {
            content_index,
            text: String::new(),
            annotations: Vec::new(),
            refusal,
        };
    }

    fn start_block(&mut self, out: &mut Vec<SseEvent>, block: &BlockStart) {
        // The contract closes a block before the next one starts; tolerate a
        // producer that does not.
        self.stop_block(out);
        // Something follows the last tool call, so it ran to completion.
        self.finish_tool(out, true);
        match block {
            BlockStart::Text => self.open_part(out, false),
            BlockStart::Refusal => self.open_part(out, true),
            BlockStart::Reasoning { id, redacted } => {
                self.close_message(out, "completed");
                let output_index = self.output.len();
                let id = reasoning_item_id(id.as_deref(), &self.base, output_index);
                self.emit(
                    out,
                    "response.output_item.added",
                    json!({"output_index": output_index, "item": reasoning_item(&id, "", None)}),
                );
                self.block = Block::Reasoning {
                    id,
                    output_index,
                    text: String::new(),
                    blob: None,
                    redacted: *redacted,
                    part_open: false,
                };
            }
            BlockStart::ToolCall {
                id,
                name,
                kind,
                signature,
            } => {
                self.close_message(out, "completed");
                // The call's own signature travels on a reasoning item
                // directly ahead of it, as in a complete response.
                if let Some(blob) = call_signature_for_client(signature.as_ref()) {
                    let output_index = self.output.len();
                    let id = reasoning_item_id(None, &self.base, output_index);
                    self.emit(
                        out,
                        "response.output_item.added",
                        json!({"output_index": output_index, "item": reasoning_item(&id, "", None)}),
                    );
                    let item = reasoning_item(&id, "", Some(&blob));
                    self.emit(
                        out,
                        "response.output_item.done",
                        json!({"output_index": output_index, "item": item.clone()}),
                    );
                    self.output.push(item);
                }
                let output_index = self.output.len();
                let view = ToolView::new(&self.tools, id, name, *kind);
                self.emit(
                    out,
                    "response.output_item.added",
                    json!({"output_index": output_index, "item": view.item("in_progress", "")}),
                );
                self.block = Block::Tool {
                    view,
                    output_index,
                    arguments: String::new(),
                };
            }
            BlockStart::Whole { part } => self.whole(out, part),
        }
    }

    /// A complete part delivered in one event. Parts that have a streamed
    /// form are replayed through it (their `BlockStop` follows as usual).
    fn whole(&mut self, out: &mut Vec<SseEvent>, part: &Part) {
        match part {
            Part::Text(text) => {
                self.open_part(out, false);
                self.text_delta(out, &text.text);
                for citation in &text.citations {
                    self.citation(out, citation);
                }
            }
            Part::Refusal(refusal) => {
                self.open_part(out, true);
                self.text_delta(out, &refusal.text);
            }
            Part::Reasoning(reasoning) => {
                self.start_block(
                    out,
                    &BlockStart::Reasoning {
                        id: reasoning.id.clone(),
                        redacted: reasoning.redacted,
                    },
                );
                self.reasoning_delta(out, &reasoning.text);
                if let (Block::Reasoning { blob, .. }, Some(signature)) =
                    (&mut self.block, &reasoning.signature)
                {
                    *blob = Some(signature_for_client(signature, reasoning.redacted));
                }
            }
            Part::ToolCall(call) => {
                self.start_block(
                    out,
                    &BlockStart::ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        kind: call.kind,
                        signature: call.signature.clone(),
                    },
                );
                self.args_delta(out, &call.arguments);
            }
            other => {
                self.block = Block::Skipped;
                // Probe first: a part with no wire form must not close the
                // open message.
                if whole_item(other, &self.base, 0).is_none() {
                    return;
                }
                self.close_message(out, "completed");
                let output_index = self.output.len();
                let Some(item) = whole_item(other, &self.base, output_index) else {
                    return;
                };
                let fields = json!({"output_index": output_index, "item": item.clone()});
                self.emit(out, "response.output_item.added", fields.clone());
                self.emit(out, "response.output_item.done", fields);
                self.output.push(item);
            }
        }
    }

    fn text_delta(&mut self, out: &mut Vec<SseEvent>, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let (
            Block::Text {
                content_index,
                text,
                refusal,
                ..
            },
            Some(message),
        ) = (&mut self.block, &self.message)
        else {
            return;
        };
        text.push_str(delta);
        let mut fields = json!({
            "item_id": message.id,
            "output_index": message.output_index,
            "content_index": *content_index,
            "delta": delta,
        });
        let kind = if *refusal {
            "response.refusal.delta"
        } else {
            fields["logprobs"] = json!([]);
            "response.output_text.delta"
        };
        self.emit(out, kind, fields);
    }

    fn citation(&mut self, out: &mut Vec<SseEvent>, citation: &Citation) {
        let (
            Block::Text {
                content_index,
                annotations,
                refusal: false,
                ..
            },
            Some(message),
        ) = (&mut self.block, &self.message)
        else {
            return;
        };
        let Some(annotation) = annotation_from_citation(citation) else {
            return;
        };
        let fields = json!({
            "item_id": message.id,
            "output_index": message.output_index,
            "content_index": *content_index,
            "annotation_index": annotations.len(),
            "annotation": annotation.clone(),
        });
        annotations.push(annotation);
        self.emit(out, "response.output_text.annotation.added", fields);
    }

    fn reasoning_delta(&mut self, out: &mut Vec<SseEvent>, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let Block::Reasoning {
            id,
            output_index,
            text,
            part_open,
            ..
        } = &mut self.block
        else {
            return;
        };
        text.push_str(delta);
        let opening = !*part_open;
        *part_open = true;
        let position = json!({"item_id": id, "output_index": *output_index, "summary_index": 0});
        if opening {
            let mut fields = position.clone();
            fields["part"] = json!({"type": "summary_text", "text": ""});
            self.emit(out, "response.reasoning_summary_part.added", fields);
        }
        let mut fields = position;
        fields["delta"] = json!(delta);
        self.emit(out, "response.reasoning_summary_text.delta", fields);
    }

    fn args_delta(&mut self, out: &mut Vec<SseEvent>, fragment: &str) {
        if fragment.is_empty() {
            return;
        }
        let Block::Tool {
            view,
            output_index,
            arguments,
        } = &mut self.block
        else {
            return;
        };
        arguments.push_str(fragment);
        // Function-shaped JSON for a custom tool cannot be streamed: the raw
        // input only exists once the wrapper is complete.
        if view.wrapped {
            return;
        }
        let kind = if view.custom {
            "response.custom_tool_call_input.delta"
        } else {
            "response.function_call_arguments.delta"
        };
        let fields =
            json!({"item_id": view.item_id, "output_index": *output_index, "delta": fragment});
        self.emit(out, kind, fields);
    }

    fn stop_block(&mut self, out: &mut Vec<SseEvent>) {
        match std::mem::replace(&mut self.block, Block::None) {
            Block::None | Block::Skipped => {}
            Block::Text {
                content_index,
                text,
                annotations,
                refusal,
            } => {
                let Some(message) = &self.message else {
                    return;
                };
                let position = json!({
                    "item_id": message.id,
                    "output_index": message.output_index,
                    "content_index": content_index,
                });
                let (done_kind, done_key, part) = if refusal {
                    ("response.refusal.done", "refusal", refusal_part(&text))
                } else {
                    (
                        "response.output_text.done",
                        "text",
                        text_part(&text, annotations),
                    )
                };
                let mut done = position.clone();
                done[done_key] = json!(text);
                if !refusal {
                    done["logprobs"] = json!([]);
                }
                let mut part_done = position;
                part_done["part"] = part.clone();
                self.emit(out, done_kind, done);
                self.emit(out, "response.content_part.done", part_done);
                if let Some(message) = self.message.as_mut() {
                    message.content.push(part);
                }
            }
            Block::Reasoning {
                id,
                output_index,
                text,
                blob,
                part_open,
                ..
            } => {
                if part_open {
                    let position =
                        json!({"item_id": id, "output_index": output_index, "summary_index": 0});
                    let mut done = position.clone();
                    done["text"] = json!(text);
                    let mut part_done = position;
                    part_done["part"] = json!({"type": "summary_text", "text": text});
                    self.emit(out, "response.reasoning_summary_text.done", done);
                    self.emit(out, "response.reasoning_summary_part.done", part_done);
                }
                let item = reasoning_item(&id, &text, blob.as_deref());
                self.emit(
                    out,
                    "response.output_item.done",
                    json!({"output_index": output_index, "item": item.clone()}),
                );
                self.output.push(item);
            }
            Block::Tool {
                view,
                output_index,
                arguments,
            } => {
                // Whether the call is `completed` or `incomplete` depends on
                // what comes next (see `finish_tool`).
                self.stopped_tool = Some(StoppedTool {
                    view,
                    output_index,
                    arguments,
                });
            }
        }
    }

    /// Emits the closing events of the tool call whose block has stopped:
    /// `…arguments.done` / `…input.done` and `response.output_item.done`.
    ///
    /// They are held back until the next canonical event because only that
    /// tells a finished call from one generation was cut off in: a call
    /// followed by another block or by a normal finish is `completed` (empty
    /// function arguments become `{}`), a call followed by a cut-off
    /// (`Finish` with a length / filter / error reason) is `incomplete` and
    /// keeps the arguments exactly as far as they got. No wire event can come
    /// between the block's last delta and these, so the order on the wire is
    /// the documented one.
    fn finish_tool(&mut self, out: &mut Vec<SseEvent>, completed: bool) {
        let Some(StoppedTool {
            view,
            output_index,
            arguments,
        }) = self.stopped_tool.take()
        else {
            return;
        };
        let payload = view.payload(&arguments, completed);
        if view.custom {
            self.emit(
                out,
                "response.custom_tool_call_input.done",
                json!({"item_id": view.item_id, "output_index": output_index, "input": payload}),
            );
        } else {
            self.emit(
                out,
                "response.function_call_arguments.done",
                json!({
                    "item_id": view.item_id,
                    "output_index": output_index,
                    "name": view.name,
                    "arguments": payload,
                }),
            );
        }
        let item = view.finished_item(&arguments, completed);
        self.emit(
            out,
            "response.output_item.done",
            json!({"output_index": output_index, "item": item.clone()}),
        );
        self.output.push(item);
    }

    fn fail(&mut self, out: &mut Vec<SseEvent>, error: &ApiError) {
        // The call's block had stopped before the failure: it is whole.
        self.finish_tool(out, true);
        let event = stream_error_event(error, self.sequence);
        self.sequence += 1;
        out.push(SseEvent::json(Some("error"), &event));
        self.finished = true;
    }
}

impl StreamEncoder for Encoder {
    fn encode(&mut self, event: &StreamEvent) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        match event {
            StreamEvent::Start { id, model, created } => self.start(&mut out, id, model, *created),
            // A failure before anything was produced has no response to
            // attach to; the bare error event is the whole stream.
            StreamEvent::Error(error) => self.fail(&mut out, error),
            other => {
                self.start(&mut out, "", "", 0);
                match other {
                    StreamEvent::BlockStart { block, .. } => self.start_block(&mut out, block),
                    StreamEvent::TextDelta { text, .. } => self.text_delta(&mut out, text),
                    StreamEvent::Citation { citation, .. } => self.citation(&mut out, citation),
                    StreamEvent::ReasoningDelta { text, .. } => {
                        self.reasoning_delta(&mut out, text);
                    }
                    StreamEvent::ReasoningSignature { signature, .. } => {
                        if let Block::Reasoning { blob, redacted, .. } = &mut self.block {
                            *blob = Some(signature_for_client(signature, *redacted));
                        }
                    }
                    StreamEvent::ToolArgsDelta { fragment, .. } => {
                        self.args_delta(&mut out, fragment);
                    }
                    StreamEvent::BlockStop { .. } => self.stop_block(&mut out),
                    StreamEvent::Usage(usage) => self.usage.merge(usage),
                    StreamEvent::Finish { reason, .. } => {
                        self.stop_block(&mut out);
                        let finish = presentable_finish(reason, self.saw_refusal);
                        // Whatever was being produced last — a message or a
                        // tool call — is where a cut-off happened.
                        let cut = cuts_output(&finish);
                        self.finish_tool(&mut out, !cut);
                        let status = if cut { "incomplete" } else { "completed" };
                        self.close_message(&mut out, status);
                        let response = self.response_object(Some(&finish), self.output.clone());
                        let kind = match status_for(&finish).0 {
                            "incomplete" => "response.incomplete",
                            "failed" => "response.failed",
                            _ => "response.completed",
                        };
                        self.emit(&mut out, kind, json!({"response": response}));
                        self.finished = true;
                    }
                    StreamEvent::Start { .. } | StreamEvent::Error(_) => {}
                }
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.finished {
            // A Responses client waits for a terminal event; a sequence that
            // just stops is reported as the stream failure it is.
            let error = ApiError::upstream("upstream stream closed before a terminal event");
            self.fail(&mut out, &error);
        }
        out
    }
}
