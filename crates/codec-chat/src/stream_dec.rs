//! Stream decoder: `chat.completion.chunk` events from an upstream into
//! canonical [`StreamEvent`]s.
//!
//! Chat streams are loosely structured: text, reasoning and several tool
//! calls may interleave, tool-call fragments are keyed by an index, and the
//! finish reason, the usage and the `[DONE]` marker each arrive in their own
//! chunk (or not at all). The canonical stream is strictly sequential, so the
//! decoder keeps exactly one block open and buffers whatever cannot be
//! emitted yet:
//!
//! * **Tool calls.** A call is announced once its name and id are known.
//!   The announced call streams its argument fragments live. A second call is
//!   announced as soon as the first one's arguments form a complete JSON
//!   document (the normal, sequential case); if they do not (fragments of
//!   parallel calls genuinely interleave) the later calls are buffered and
//!   emitted whole, in index order, when the turn ends.
//! * **Text and reasoning that arrive while a call is unfinished** are
//!   buffered and emitted as blocks of their own after the tool calls.
//! * **`Finish` is deferred** until `[DONE]` (or the end of the stream) so a
//!   usage-only chunk that follows the finish reason is still reported
//!   before it.
//!
//! A stream that ends without `[DONE]` is complete if a finish reason or a
//! trailing usage-only chunk was seen (many compatible servers omit the
//! marker) and truncated otherwise (`Finish { reason: Error }`). A usage-only
//! chunk is only "trailing" when it follows output: usage reported up front,
//! or followed by more output, does not complete anything.
//!
//! A `[DONE]` that arrives before any chunk produces **no** event: nothing
//! was generated, and staying silent lets the gateway see that the upstream
//! never started answering (so the attempt can be retried elsewhere).
//! `finish()` then ends such a stream like any other empty one, with
//! `Start` and `Finish { reason: Error }`.
//!
//! A turn whose tool-call arguments are a cut-off JSON document is reported
//! as `Length`, not `ToolCalls`, whatever the upstream called it (see
//! [`upstream_finish`]).

use crate::common::{
    ID_PREFIX, PROTOCOL, arguments_text, citations_from_wire, finish_from_wire, i64_of,
    images_from_wire, reasoning_details, reasoning_text, str_of, thought_signature,
    upstream_finish, usage_from_wire,
};
use crate::error::{api_error_from_stream, clean_message};
use serde_json::Value;
use std::collections::HashMap;
use switchyard_core::Usage;
use switchyard_core::codec::StreamDecoder;
use switchyard_core::error::{ApiError, CodecError};
use switchyard_core::ir::{FinishReason, Part, Reasoning, Signature, ToolCallKind};
use switchyard_core::sse::SseEvent;
use switchyard_core::stream::{BlockStart, StreamEvent};
use switchyard_core::util::{new_call_id, new_id};

/// Wire key of the deprecated single `function_call`, which has no index.
const LEGACY_CALL_KEY: i64 = -1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotState {
    /// Not announced yet: waiting for its name/id or for its turn.
    Pending,
    /// Announced and currently the open block.
    Live,
    /// Announced and closed, or discarded.
    Closed,
}

#[derive(Debug)]
struct ToolSlot {
    /// `tool_calls[].index` on the wire.
    key: i64,
    id: String,
    name: String,
    kind: ToolCallKind,
    signature: Option<Signature>,
    /// Every argument fragment received so far.
    args: String,
    state: SlotState,
    /// The call was announced downstream (a `BlockStart` was emitted).
    announced: bool,
}

impl ToolSlot {
    fn startable(&self) -> bool {
        !self.name.is_empty() && !self.id.is_empty()
    }

    /// Whether the arguments received so far are a finished JSON document.
    /// Only delimited documents count: nothing can follow a closed object,
    /// whereas `12` may still become `123`. Custom tool input is free text
    /// and never "complete".
    fn args_complete(&self) -> bool {
        self.kind == ToolCallKind::Function
            && serde_json::from_str::<Value>(self.args.trim())
                .is_ok_and(|v| v.is_object() || v.is_array())
    }

    /// Whether a delta on this slot's wire index opens another call instead
    /// of continuing this one. Some servers send every call of a turn with
    /// index 0, with or without ids.
    ///
    /// Only a header (a delta with a name) can open a call. When both ids
    /// are known they decide. Otherwise it is a new call when the name
    /// differs from this call's name, or when argument text arrives although
    /// this call's arguments are already a complete document (nothing can be
    /// appended to one). A header that merely repeats the name, or supplies
    /// the id late, continues the call.
    fn superseded_by(&self, id: Option<&str>, name: Option<&str>, fragment: &str) -> bool {
        let Some(name) = name else {
            return false;
        };
        if let Some(id) = id
            && !self.id.is_empty()
        {
            return self.id != id;
        }
        if self.name.is_empty() {
            return false;
        }
        self.name != name || (self.args_complete() && !fragment.trim().is_empty())
    }
}

/// Whether two `reasoning_details[].index` values may name the same block.
/// An unknown index is compatible with anything.
fn same_detail(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// What one `tool_calls[]` delta element (or a deprecated `function_call`
/// delta) contributes to a call.
struct ToolDelta<'a> {
    key: i64,
    id: Option<&'a str>,
    name: Option<&'a str>,
    fragment: String,
    kind: ToolCallKind,
    signature: Option<Signature>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Open {
    None,
    Text,
    Refusal,
    Reasoning {
        /// `reasoning_details[].index` this block was opened for.
        detail: Option<i64>,
        /// A signature has been delivered: the block is complete and further
        /// reasoning text belongs to a new one.
        signed: bool,
    },
    Tool(usize),
}

/// Output that arrived while a tool call was unfinished.
#[derive(Debug)]
enum Trailing {
    Text(String),
    Reasoning {
        /// `reasoning_details[].index` of the block, when known.
        detail: Option<i64>,
        reasoning: Reasoning,
    },
    Refusal(String),
    Whole(Part),
}

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct ChatStreamDecoder {
    started: bool,
    terminal: bool,
    id: String,
    model: String,
    created: i64,
    next_index: u32,
    current: u32,
    open: Open,
    tools: Vec<ToolSlot>,
    /// Wire index -> the slot currently collecting fragments for it.
    slot_by_key: HashMap<i64, usize>,
    trailing: Vec<Trailing>,
    finish: Option<FinishReason>,
    stop_sequence: Option<String>,
    saw_terminal_usage: bool,
    last_usage: Option<Usage>,
}

impl ChatStreamDecoder {
    pub(crate) fn new() -> Self {
        ChatStreamDecoder {
            started: false,
            terminal: false,
            id: String::new(),
            model: String::new(),
            created: 0,
            next_index: 0,
            current: 0,
            open: Open::None,
            tools: Vec::new(),
            slot_by_key: HashMap::new(),
            trailing: Vec::new(),
            finish: None,
            stop_sequence: None,
            saw_terminal_usage: false,
            last_usage: None,
        }
    }

    // -- block plumbing ------------------------------------------------------

    fn ensure_started(&mut self, out: &mut Vec<StreamEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        if self.id.is_empty() {
            self.id = new_id(ID_PREFIX);
        }
        out.push(StreamEvent::Start {
            id: self.id.clone(),
            model: self.model.clone(),
            created: self.created,
        });
    }

    fn open_block(&mut self, block: BlockStart, out: &mut Vec<StreamEvent>) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        self.current = index;
        out.push(StreamEvent::BlockStart { index, block });
        index
    }

    fn close_open(&mut self, out: &mut Vec<StreamEvent>) {
        match std::mem::replace(&mut self.open, Open::None) {
            Open::None => return,
            Open::Tool(slot) => self.tools[slot].state = SlotState::Closed,
            Open::Text | Open::Refusal | Open::Reasoning { .. } => {}
        }
        out.push(StreamEvent::BlockStop {
            index: self.current,
        });
    }

    /// Emits a complete block: start, an optional single delta, stop.
    fn whole_block(
        &mut self,
        block: BlockStart,
        delta: impl FnOnce(u32) -> Vec<StreamEvent>,
        out: &mut Vec<StreamEvent>,
    ) {
        self.close_open(out);
        let index = self.open_block(block, out);
        out.extend(delta(index));
        out.push(StreamEvent::BlockStop { index });
    }

    // -- tool calls ----------------------------------------------------------

    fn next_pending(&self) -> Option<usize> {
        self.tools
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.state == SlotState::Pending)
            .min_by_key(|(position, slot)| (slot.key, *position))
            .map(|(position, _)| position)
    }

    /// Announces a pending call as the open block and replays the argument
    /// text buffered for it. Nothing may be open.
    fn start_tool(&mut self, position: usize, out: &mut Vec<StreamEvent>) {
        let slot = &mut self.tools[position];
        if slot.id.is_empty() {
            slot.id = new_call_id();
        }
        slot.state = SlotState::Live;
        slot.announced = true;
        let block = BlockStart::ToolCall {
            id: slot.id.clone(),
            name: slot.name.clone(),
            kind: slot.kind,
            signature: slot.signature.clone(),
        };
        let buffered = slot.args.clone();
        let index = self.open_block(block, out);
        self.open = Open::Tool(position);
        if !buffered.is_empty() {
            out.push(StreamEvent::ToolArgsDelta {
                index,
                fragment: buffered,
            });
        }
    }

    /// Announces pending calls for as long as that is safe: the lowest
    /// pending call must know its name and id, and the call that is open (if
    /// any) must be finished.
    fn pump_tools(&mut self, out: &mut Vec<StreamEvent>) {
        while let Some(next) = self.next_pending() {
            if !self.tools[next].startable() {
                return;
            }
            if let Open::Tool(live) = self.open
                && !self.tools[live].args_complete()
            {
                return;
            }
            self.close_open(out);
            self.start_tool(next, out);
        }
    }

    /// Makes room for a non-tool block. Returns `false` when a tool call is
    /// still unfinished, in which case the caller must buffer its output.
    fn settle_tools(&mut self, out: &mut Vec<StreamEvent>) -> bool {
        self.pump_tools(out);
        if self.next_pending().is_some() {
            return false;
        }
        match self.open {
            Open::Tool(live) if self.tools[live].args_complete() => {
                self.close_open(out);
                true
            }
            Open::Tool(_) => false,
            _ => true,
        }
    }

    fn apply_tool(&mut self, delta: ToolDelta<'_>, out: &mut Vec<StreamEvent>) {
        let ToolDelta {
            key,
            id,
            name,
            fragment,
            kind,
            signature,
        } = delta;
        let existing = self.slot_by_key.get(&key).copied().filter(|&position| {
            key == LEGACY_CALL_KEY || !self.tools[position].superseded_by(id, name, &fragment)
        });
        let position = match existing {
            Some(position) => position,
            None => {
                if id.is_none() && name.is_none() && fragment.is_empty() {
                    return;
                }
                self.tools.push(ToolSlot {
                    key,
                    // The deprecated form never carries an id.
                    id: if key == LEGACY_CALL_KEY {
                        new_call_id()
                    } else {
                        String::new()
                    },
                    name: String::new(),
                    kind,
                    signature: None,
                    args: String::new(),
                    state: SlotState::Pending,
                    announced: false,
                });
                let position = self.tools.len() - 1;
                self.slot_by_key.insert(key, position);
                position
            }
        };
        let slot = &mut self.tools[position];
        match slot.state {
            // A fragment for a call that was already closed cannot be
            // appended to it; a sane upstream never sends one.
            SlotState::Closed => {}
            SlotState::Live => {
                if !fragment.is_empty() {
                    slot.args.push_str(&fragment);
                    out.push(StreamEvent::ToolArgsDelta {
                        index: self.current,
                        fragment,
                    });
                }
            }
            SlotState::Pending => {
                if let Some(id) = id
                    && key != LEGACY_CALL_KEY
                {
                    slot.id = id.to_string();
                }
                if slot.name.is_empty()
                    && let Some(name) = name
                {
                    slot.name = name.to_string();
                    slot.kind = kind;
                }
                if signature.is_some() {
                    slot.signature = signature;
                }
                slot.args.push_str(&fragment);
                self.pump_tools(out);
            }
        }
    }

    fn tool_delta(&mut self, position: usize, tc: &Value, out: &mut Vec<StreamEvent>) {
        if !tc.is_object() {
            return;
        }
        let key = i64_of(tc, "index")
            .filter(|k| *k >= 0)
            .unwrap_or(position as i64);
        let custom = tc.get("custom").filter(|c| c.is_object());
        let function = tc.get("function").filter(|f| f.is_object());
        let is_custom = custom.is_some()
            && (function.is_none() || tc.get("type").and_then(Value::as_str) == Some("custom"));
        let (spec, args_key, kind) = if is_custom {
            (custom.unwrap_or(tc), "input", ToolCallKind::Custom)
        } else {
            // A flat `{name, arguments}` entry is tolerated.
            (function.unwrap_or(tc), "arguments", ToolCallKind::Function)
        };
        self.apply_tool(
            ToolDelta {
                key,
                id: str_of(tc, "id"),
                name: str_of(spec, "name"),
                fragment: arguments_text(spec.get(args_key)),
                kind,
                signature: thought_signature(tc).map(|s| Signature::new(PROTOCOL, s)),
            },
            out,
        );
    }

    // -- text, reasoning, refusal -------------------------------------------

    fn text_delta(&mut self, text: &str, out: &mut Vec<StreamEvent>) {
        if text.is_empty() {
            return;
        }
        if !self.settle_tools(out) {
            match self.trailing.last_mut() {
                Some(Trailing::Text(buffered)) => buffered.push_str(text),
                _ => self.trailing.push(Trailing::Text(text.to_string())),
            }
            return;
        }
        if self.open != Open::Text {
            self.close_open(out);
            self.open_block(BlockStart::Text, out);
            self.open = Open::Text;
        }
        out.push(StreamEvent::TextDelta {
            index: self.current,
            text: text.to_string(),
        });
    }

    fn refusal_delta(&mut self, text: &str, out: &mut Vec<StreamEvent>) {
        if text.is_empty() {
            return;
        }
        if !self.settle_tools(out) {
            match self.trailing.last_mut() {
                Some(Trailing::Refusal(buffered)) => buffered.push_str(text),
                _ => self.trailing.push(Trailing::Refusal(text.to_string())),
            }
            return;
        }
        if self.open != Open::Refusal {
            self.close_open(out);
            self.open_block(BlockStart::Refusal, out);
            self.open = Open::Refusal;
        }
        out.push(StreamEvent::TextDelta {
            index: self.current,
            text: text.to_string(),
        });
    }

    fn reasoning_delta(
        &mut self,
        detail: Option<i64>,
        id: Option<String>,
        text: &str,
        out: &mut Vec<StreamEvent>,
    ) {
        if text.is_empty() {
            return;
        }
        if !self.settle_tools(out) {
            match self.trailing.last_mut() {
                Some(Trailing::Reasoning {
                    detail: buffered,
                    reasoning,
                }) if reasoning.signature.is_none() && same_detail(*buffered, detail) => {
                    reasoning.text.push_str(text);
                    *buffered = buffered.or(detail);
                }
                _ => self.trailing.push(Trailing::Reasoning {
                    detail,
                    reasoning: Reasoning {
                        id,
                        text: text.to_string(),
                        ..Reasoning::default()
                    },
                }),
            }
            return;
        }
        let continues = match &mut self.open {
            Open::Reasoning {
                detail: open_detail,
                signed: false,
            } if same_detail(*open_detail, detail) => {
                *open_detail = open_detail.or(detail);
                true
            }
            _ => false,
        };
        if !continues {
            self.close_open(out);
            self.open_block(
                BlockStart::Reasoning {
                    id,
                    redacted: false,
                },
                out,
            );
            self.open = Open::Reasoning {
                detail,
                signed: false,
            };
        }
        out.push(StreamEvent::ReasoningDelta {
            index: self.current,
            text: text.to_string(),
        });
    }

    /// Attaches a signature to the reasoning block it closes: the open one,
    /// provided that block has no signature yet and the detail `index` the
    /// signature came with does not name another block.
    ///
    /// Any other signature belongs to reasoning whose text was never sent (a
    /// Responses item with an empty summary, Anthropic thinking with omitted
    /// display) and becomes a block of its own, so that neither blob is lost
    /// and an earlier signature is never overwritten.
    fn reasoning_signature(
        &mut self,
        detail: Option<i64>,
        id: Option<String>,
        signature: Signature,
        out: &mut Vec<StreamEvent>,
    ) {
        if let Open::Reasoning {
            detail: open_detail,
            signed,
        } = &mut self.open
            && !*signed
            && same_detail(*open_detail, detail)
        {
            *open_detail = open_detail.or(detail);
            *signed = true;
            out.push(StreamEvent::ReasoningSignature {
                index: self.current,
                signature,
            });
            return;
        }
        if !self.settle_tools(out) {
            match self.trailing.last_mut() {
                Some(Trailing::Reasoning {
                    detail: buffered,
                    reasoning,
                }) if reasoning.signature.is_none() && same_detail(*buffered, detail) => {
                    reasoning.signature = Some(signature);
                    *buffered = buffered.or(detail);
                }
                _ => self.trailing.push(Trailing::Reasoning {
                    detail,
                    reasoning: Reasoning {
                        id,
                        signature: Some(signature),
                        ..Reasoning::default()
                    },
                }),
            }
            return;
        }
        self.whole_block(
            BlockStart::Reasoning {
                id,
                redacted: false,
            },
            |index| vec![StreamEvent::ReasoningSignature { index, signature }],
            out,
        );
    }

    /// A `reasoning.encrypted` detail: reasoning whose text was withheld.
    fn redacted_reasoning(
        &mut self,
        id: Option<String>,
        signature: Signature,
        out: &mut Vec<StreamEvent>,
    ) {
        if !self.settle_tools(out) {
            self.trailing.push(Trailing::Reasoning {
                detail: None,
                reasoning: Reasoning {
                    id,
                    text: String::new(),
                    signature: Some(signature),
                    redacted: true,
                },
            });
            return;
        }
        self.whole_block(
            BlockStart::Reasoning { id, redacted: true },
            |index| vec![StreamEvent::ReasoningSignature { index, signature }],
            out,
        );
    }

    /// Reasoning comes in three spellings. The plain strings
    /// (`reasoning_content`, then `reasoning`) win for the text because
    /// servers that send `reasoning_details` repeat the same text there;
    /// the details then contribute the block structure (the `index` and
    /// `id` of the entry that mirrors the text), signatures and encrypted
    /// payloads.
    fn reasoning(&mut self, delta: &Value, out: &mut Vec<StreamEvent>) {
        let mut text = reasoning_text(delta.get("reasoning_content"));
        if text.is_empty() {
            text = reasoning_text(delta.get("reasoning"));
        }
        let details = reasoning_details(delta.get("reasoning_details"));
        let from_details = text.is_empty();
        if !from_details {
            let mirror = details.iter().find(|d| !d.encrypted && !d.text.is_empty());
            self.reasoning_delta(
                mirror.and_then(|d| d.index),
                mirror.and_then(|d| d.id.clone()),
                &text,
                out,
            );
        }
        for detail in details {
            let signature = detail.blob.as_deref().map(|b| Signature::new(PROTOCOL, b));
            if detail.encrypted {
                if let Some(signature) = signature {
                    self.redacted_reasoning(detail.id, signature, out);
                }
                continue;
            }
            if from_details {
                self.reasoning_delta(detail.index, detail.id.clone(), &detail.text, out);
            }
            if let Some(signature) = signature {
                self.reasoning_signature(detail.index, detail.id, signature, out);
            }
        }
    }

    fn content(&mut self, content: &Value, out: &mut Vec<StreamEvent>) {
        match content {
            Value::String(text) => self.text_delta(text, out),
            // Non-standard: typed fragments (Mistral sends `thinking` items).
            Value::Array(items) => {
                for item in items {
                    if let Value::String(text) = item {
                        self.text_delta(text, out);
                        continue;
                    }
                    match item.get("type").and_then(Value::as_str) {
                        Some("thinking") | Some("reasoning") => {
                            let text = reasoning_text(
                                item.get("thinking")
                                    .or_else(|| item.get("reasoning"))
                                    .or_else(|| item.get("text")),
                            );
                            self.reasoning_delta(None, None, &text, out);
                        }
                        Some("text") | Some("output_text") | None => {
                            self.text_delta(str_of(item, "text").unwrap_or_default(), out);
                        }
                        Some("refusal") => {
                            self.refusal_delta(str_of(item, "refusal").unwrap_or_default(), out);
                        }
                        Some(_) => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn delta(&mut self, delta: &Value, out: &mut Vec<StreamEvent>) {
        self.reasoning(delta, out);
        if let Some(content) = delta.get("content") {
            self.content(content, out);
        }
        if let Some(transcript) = delta.get("audio").and_then(|a| str_of(a, "transcript")) {
            // Audio-output models stream the transcript instead of `content`.
            self.text_delta(transcript, out);
        }
        if let Some(refusal) = str_of(delta, "refusal") {
            self.refusal_delta(refusal, out);
        }
        if self.open == Open::Text {
            for citation in citations_from_wire(delta.get("annotations")) {
                out.push(StreamEvent::Citation {
                    index: self.current,
                    citation,
                });
            }
        }
        for image in images_from_wire(delta.get("images")) {
            if self.settle_tools(out) {
                self.whole_block(BlockStart::Whole { part: image }, |_| Vec::new(), out);
            } else {
                self.trailing.push(Trailing::Whole(image));
            }
        }
        if let Some(Value::Array(calls)) = delta.get("tool_calls") {
            for (position, tc) in calls.iter().enumerate() {
                self.tool_delta(position, tc, out);
            }
        }
        if let Some(call) = delta.get("function_call").filter(|c| c.is_object()) {
            self.apply_tool(
                ToolDelta {
                    key: LEGACY_CALL_KEY,
                    id: None,
                    name: str_of(call, "name"),
                    fragment: arguments_text(call.get("arguments")),
                    kind: ToolCallKind::Function,
                    signature: None,
                },
                out,
            );
        }
    }

    // -- chunk level ---------------------------------------------------------

    /// Closes whatever is open and emits everything that was buffered: the
    /// remaining tool calls in index order, then trailing output.
    fn finalize_blocks(&mut self, out: &mut Vec<StreamEvent>) {
        self.close_open(out);
        while let Some(next) = self.next_pending() {
            if self.tools[next].name.is_empty() {
                // A call without a name cannot be executed by anyone.
                self.tools[next].state = SlotState::Closed;
                continue;
            }
            self.start_tool(next, out);
            self.close_open(out);
        }
        for item in std::mem::take(&mut self.trailing) {
            match item {
                Trailing::Text(text) => self.whole_block(
                    BlockStart::Text,
                    |index| vec![StreamEvent::TextDelta { index, text }],
                    out,
                ),
                Trailing::Refusal(text) => self.whole_block(
                    BlockStart::Refusal,
                    |index| vec![StreamEvent::TextDelta { index, text }],
                    out,
                ),
                Trailing::Reasoning { reasoning: r, .. } => self.whole_block(
                    BlockStart::Reasoning {
                        id: r.id,
                        redacted: r.redacted,
                    },
                    |index| {
                        let mut events = Vec::new();
                        if !r.text.is_empty() {
                            events.push(StreamEvent::ReasoningDelta {
                                index,
                                text: r.text,
                            });
                        }
                        if let Some(signature) = r.signature {
                            events.push(StreamEvent::ReasoningSignature { index, signature });
                        }
                        events
                    },
                    out,
                ),
                Trailing::Whole(part) => {
                    self.whole_block(BlockStart::Whole { part }, |_| Vec::new(), out);
                }
            }
        }
    }

    /// Whether anything was generated so far (emitted or still buffered).
    fn has_output(&self) -> bool {
        self.next_index > 0 || !self.tools.is_empty() || !self.trailing.is_empty()
    }

    fn terminate(&mut self, truncated: bool, out: &mut Vec<StreamEvent>) {
        self.ensure_started(out);
        self.finalize_blocks(out);
        let reason = match self.finish.take() {
            None if truncated => FinishReason::Error,
            // `stop` (or no reason at all) after tool calls is a tool turn,
            // unless a call's arguments were cut short.
            reason => upstream_finish(
                reason,
                self.tools
                    .iter()
                    .filter(|slot| slot.announced)
                    .map(|slot| (slot.kind, slot.args.as_str())),
            ),
        };
        let stop_sequence = self
            .stop_sequence
            .take()
            .filter(|_| reason == FinishReason::Stop);
        out.push(StreamEvent::Finish {
            reason,
            stop_sequence,
        });
        self.terminal = true;
    }

    fn fail(&mut self, error: ApiError, out: &mut Vec<StreamEvent>) {
        self.ensure_started(out);
        self.close_open(out);
        out.push(StreamEvent::Error(error));
        self.terminal = true;
    }

    fn chunk(&mut self, chunk: &Value, out: &mut Vec<StreamEvent>) {
        if !self.started {
            if let Some(id) = str_of(chunk, "id") {
                self.id = id.to_string();
            }
            if let Some(model) = str_of(chunk, "model") {
                self.model = model.to_string();
            }
            if let Some(created) = i64_of(chunk, "created").filter(|c| *c > 0) {
                self.created = created;
            }
        }
        let choices = chunk.get("choices").and_then(Value::as_array);
        // The IR models a single candidate.
        let choice = choices.and_then(|choices| {
            choices
                .iter()
                .find(|c| c.is_object() && i64_of(c, "index").unwrap_or(0) == 0)
        });
        let usage = chunk
            .get("usage")
            .and_then(usage_from_wire)
            // Moonshot reports usage inside the final choice.
            .or_else(|| {
                choice
                    .and_then(|c| c.get("usage"))
                    .and_then(usage_from_wire)
            })
            .filter(|u| !u.is_empty());
        if choice.is_none() && usage.is_none() {
            // Nothing this decoder understands (keep-alive objects, content
            // filter preambles, other choices of an `n > 1` request).
            return;
        }
        self.ensure_started(out);
        if let Some(choice) = choice {
            if let Some(delta) = choice.get("delta").filter(|d| d.is_object()) {
                if carries_output(delta) {
                    // Output after a usage-only chunk: that chunk was not
                    // the end of the stream.
                    self.saw_terminal_usage = false;
                }
                self.delta(delta, out);
            } else if let Some(text) = str_of(choice, "text") {
                // Legacy completion chunk.
                self.saw_terminal_usage = false;
                self.text_delta(text, out);
            }
            if let Some(raw) = str_of(choice, "finish_reason") {
                self.finalize_blocks(out);
                self.finish = Some(finish_from_wire(raw));
                if let Some(stop) = str_of(choice, "stop_reason") {
                    self.stop_sequence = Some(stop.to_string());
                }
            }
        }
        if let Some(usage) = usage {
            if self.last_usage != Some(usage) {
                self.last_usage = Some(usage);
                out.push(StreamEvent::Usage(usage));
            }
            if choices.is_none_or(|c| c.is_empty()) {
                // A usage-only chunk ends the stream (for servers that send
                // neither a finish reason nor `[DONE]`) only when it trails
                // output. Usage reported before anything was generated says
                // nothing about completion.
                self.saw_terminal_usage = self.finish.is_some() || self.has_output();
            }
        }
    }
}

/// Whether a delta carries anything besides the role: some field that is not
/// `null`, an empty string, an empty array or an empty object.
fn carries_output(delta: &Value) -> bool {
    delta.as_object().is_some_and(|fields| {
        fields.iter().any(|(key, value)| {
            key != "role"
                && match value {
                    Value::Null => false,
                    Value::String(text) => !text.is_empty(),
                    Value::Array(items) => !items.is_empty(),
                    Value::Object(map) => !map.is_empty(),
                    Value::Bool(_) | Value::Number(_) => true,
                }
        })
    })
}

/// Whether a chunk's `error` member actually reports an error.
fn is_error_value(error: &Value) -> bool {
    match error {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Object(map) => !map.is_empty(),
        _ => true,
    }
}

impl StreamDecoder for ChatStreamDecoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, CodecError> {
        let mut out = Vec::new();
        if self.terminal {
            return Ok(out);
        }
        if event.is_done_marker() {
            // A terminator with nothing before it terminates nothing: the
            // upstream generated no response. No event is produced, so the
            // gateway still sees an upstream that has not started answering,
            // and `finish()` reports the empty stream as failed.
            if self.started {
                self.terminate(false, &mut out);
            }
            return Ok(out);
        }
        let data = event.data.trim();
        let named_error = event
            .event
            .as_deref()
            .is_some_and(|name| name.trim().eq_ignore_ascii_case("error"));
        let chunk: Value = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(_) => {
                if named_error && !data.is_empty() {
                    self.fail(ApiError::upstream(clean_message(data)), &mut out);
                }
                // Anything else that is not JSON is noise.
                return Ok(out);
            }
        };
        if named_error || chunk.get("error").is_some_and(is_error_value) {
            self.fail(api_error_from_stream(&chunk), &mut out);
            return Ok(out);
        }
        if chunk.is_object() {
            self.chunk(&chunk, &mut out);
        }
        Ok(out)
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.terminal {
            return out;
        }
        let truncated = self.finish.is_none() && !self.saw_terminal_usage;
        self.terminate(truncated, &mut out);
        out
    }
}
