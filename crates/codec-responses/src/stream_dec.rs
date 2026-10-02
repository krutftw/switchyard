//! Responses streaming events → canonical stream events.
//!
//! The wire protocol is item-oriented and, in principle, lets items
//! interleave (deltas are addressed by `item_id` / `output_index`). The
//! canonical stream is strictly sequential, so the decoder keeps a queue of
//! logical blocks in order of first appearance: the head block streams
//! through as its events arrive, later blocks buffer until every block ahead
//! of them has closed. With a well-behaved upstream the queue never holds
//! more than one block and nothing is delayed.
//!
//! Block mapping: every content part of a `message` item is a text (or
//! refusal) block; a `reasoning` item is one reasoning block (summary parts
//! joined by a blank line); a `function_call` / `custom_tool_call` item is a
//! tool-call block; any other item is a whole block carrying the item
//! verbatim.
//!
//! Upstreams differ in how much they stream. Every piece of content is taken
//! from the first place it shows up — a delta, the matching `…done` event,
//! the item in `response.output_item.done`, or the `output` array of the
//! terminal event — and never twice.
//!
//! Upstreams also differ in how they address items. The vendor gives every
//! item its own id and output index; bridges in front of other APIs reuse an
//! id for several items, put every item at index 0, or leave both out. No
//! single field is therefore trusted as an item's identity: an id counts
//! only while the output index does not contradict it, an index only while
//! ids, call ids and the kind of content do not, and an event without any
//! address continues the latest open item of its kind (see `find`,
//! `locate_item` and `match_final_output`).
//!
//! Whatever arrives, the output obeys the canonical sequence contract: an
//! event that addresses an item of another kind is dropped, and nothing is
//! ever queued on a block it does not fit (`push`).

use crate::common::{
    P, call_id_of, citation_from_annotation, i64_field, non_empty, parts_from_output_item, qualify,
    reasoning_item_text, stated_call_id, stringish, type_of, usage_from_wire,
};
use crate::error::api_error_from_stream;
use crate::response::{ContentKinds, finish_from};
use serde_json::Value;
use std::collections::VecDeque;
use switchyard_core::ir::{Citation, FinishReason, Signature, ToolCallKind};
use switchyard_core::stream::{BlockStart, StreamEvent};
use switchyard_core::util::{new_call_id, new_id, str_field, u64_field};
use switchyard_core::{CodecError, SseEvent, StreamDecoder};

/// A buffered canonical event of one block.
enum Queued {
    Text(String),
    Reasoning(String),
    Signature(Signature),
    Args(String),
    Citation(Citation),
}

/// One canonical block, from creation until its `BlockStop` is emitted.
struct Block {
    uid: u64,
    start: BlockStart,
    /// `BlockStart` has been emitted.
    started: bool,
    closed: bool,
    queue: Vec<Queued>,
    /// Characters of text / arguments received so far.
    len: usize,
    citations: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Kind {
    #[default]
    Message,
    Reasoning,
    Tool,
    Other,
}

fn kind_of(item_type: &str) -> Kind {
    match item_type {
        "message" | "" => Kind::Message,
        "reasoning" => Kind::Reasoning,
        "function_call" | "custom_tool_call" => Kind::Tool,
        _ => Kind::Other,
    }
}

/// Which of the two reasoning text channels feeds the block. Only one is
/// used so a summary and the raw text it summarises are not both replayed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    Summary,
    Content,
}

/// What is known about one output item of the upstream.
#[derive(Default)]
struct Item {
    id: String,
    output_index: Option<u64>,
    kind: Kind,
    /// The kind was stated by the upstream (an item with a `type`) rather
    /// than inferred from the first event that mentioned the item.
    typed: bool,
    /// `output_item.done` (or the terminal output array) has been applied.
    finished: bool,
    /// Message content parts: `(content_index, block uid)`.
    parts: Vec<(u64, u64)>,
    /// The block of a reasoning or tool-call item.
    block: Option<u64>,
    channel: Option<Channel>,
    /// The block has received reasoning text.
    any_text: bool,
    /// The current summary / content part has produced text.
    part_text: bool,
    /// A blank line is due before the next summary part's text.
    separator: bool,
    call_id: String,
    /// The call id as the upstream stated it (`call_id` may be a stand-in
    /// derived from the item id).
    stated_call: String,
    name: String,
    custom: bool,
    /// Argument text received before the call's block could be opened.
    pending_args: String,
    /// Argument characters received through delta events.
    args_len: usize,
}

impl Item {
    /// The item has opened at least one canonical block.
    fn has_blocks(&self) -> bool {
        !self.parts.is_empty() || self.block.is_some()
    }
}

/// Stateful Responses stream decoder.
#[derive(Default)]
pub(crate) struct Decoder {
    started: bool,
    finished: bool,
    id: String,
    model: String,
    created: i64,
    next_index: u32,
    next_uid: u64,
    blocks: VecDeque<Block>,
    items: Vec<Item>,
    kinds: ContentKinds,
}

impl Decoder {
    pub(crate) fn new() -> Self {
        Decoder::default()
    }

    // -- response-level state ------------------------------------------------

    fn capture(&mut self, response: &Value) {
        if self.started || !response.is_object() {
            return;
        }
        if let Some(id) = non_empty(response, "id") {
            self.id = id.to_string();
        }
        if let Some(model) = non_empty(response, "model") {
            self.model = model.to_string();
        }
        if let Some(created) = i64_field(response, "created_at") {
            self.created = created;
        }
    }

    fn ensure_started(&mut self, out: &mut Vec<StreamEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        if self.id.is_empty() {
            self.id = new_id("resp_");
        }
        out.push(StreamEvent::Start {
            id: self.id.clone(),
            model: self.model.clone(),
            created: self.created,
        });
    }

    // -- block queue -----------------------------------------------------------

    fn new_block(&mut self, start: BlockStart) -> u64 {
        match &start {
            BlockStart::ToolCall { .. } => self.kinds.tool_calls = true,
            BlockStart::Refusal => self.kinds.refusal = true,
            BlockStart::Whole { part } => self.kinds.note(part),
            BlockStart::Text | BlockStart::Reasoning { .. } => {}
        }
        let uid = self.next_uid;
        self.next_uid += 1;
        self.blocks.push_back(Block {
            uid,
            start,
            started: false,
            closed: false,
            queue: Vec::new(),
            len: 0,
            citations: 0,
        });
        uid
    }

    fn block_mut(&mut self, uid: u64) -> Option<&mut Block> {
        self.blocks.iter_mut().find(|b| b.uid == uid && !b.closed)
    }

    /// Characters received by a block; `None` once it has closed.
    fn block_len(&self, uid: u64) -> Option<usize> {
        self.blocks
            .iter()
            .find(|b| b.uid == uid && !b.closed)
            .map(|b| b.len)
    }

    /// Queues an event on a block. An event that does not fit the kind of
    /// block it was routed to is dropped here: whatever the upstream sends,
    /// a delta never comes out inside a block of another kind.
    fn push(&mut self, uid: u64, event: Queued) {
        let text_len = match &event {
            Queued::Text(text) | Queued::Reasoning(text) | Queued::Args(text) => text.len(),
            Queued::Signature(_) | Queued::Citation(_) => 0,
        };
        let Some(block) = self.block_mut(uid) else {
            return;
        };
        let fits = matches!(
            (&block.start, &event),
            (BlockStart::Text | BlockStart::Refusal, Queued::Text(_))
                | (BlockStart::Text, Queued::Citation(_))
                | (
                    BlockStart::Reasoning { .. },
                    Queued::Reasoning(_) | Queued::Signature(_)
                )
                | (BlockStart::ToolCall { .. }, Queued::Args(_))
        );
        if !fits {
            return;
        }
        if matches!(event, Queued::Citation(_)) {
            block.citations += 1;
        }
        block.len += text_len;
        let visible = matches!((&block.start, &event), (BlockStart::Text, Queued::Text(_)));
        block.queue.push(event);
        if visible && text_len > 0 {
            self.kinds.text = true;
        }
    }

    fn close(&mut self, uid: u64) {
        if let Some(block) = self.block_mut(uid) {
            block.closed = true;
        }
    }

    /// Emits everything the sequence contract allows right now.
    fn drain(&mut self, out: &mut Vec<StreamEvent>) {
        if self.blocks.is_empty() {
            return;
        }
        self.ensure_started(out);
        while let Some(head) = self.blocks.front_mut() {
            let index = self.next_index;
            if !head.started {
                // A reasoning block is reserved when its item is announced,
                // to keep its place in the output order, but the item may
                // turn out to carry neither text nor an encrypted payload.
                // The block only materialises once it has something to say.
                let reserved =
                    matches!(head.start, BlockStart::Reasoning { .. }) && head.queue.is_empty();
                if reserved {
                    if !head.closed {
                        break;
                    }
                    self.blocks.pop_front();
                    continue;
                }
                head.started = true;
                out.push(StreamEvent::BlockStart {
                    index,
                    block: head.start.clone(),
                });
            }
            for event in head.queue.drain(..) {
                out.push(match event {
                    Queued::Text(text) => StreamEvent::TextDelta { index, text },
                    Queued::Reasoning(text) => StreamEvent::ReasoningDelta { index, text },
                    Queued::Signature(signature) => {
                        StreamEvent::ReasoningSignature { index, signature }
                    }
                    Queued::Args(fragment) => StreamEvent::ToolArgsDelta { index, fragment },
                    Queued::Citation(citation) => StreamEvent::Citation { index, citation },
                });
            }
            if !head.closed {
                break;
            }
            out.push(StreamEvent::BlockStop { index });
            self.next_index += 1;
            self.blocks.pop_front();
        }
    }

    // -- item tracking -----------------------------------------------------------

    /// Finds the known item an address (item id and/or output index) points
    /// at.
    ///
    /// The id is tried first, but an id alone is not proof of identity:
    /// bridges in front of other APIs reuse one item id for several output
    /// items (one id per choice, a new output index per reasoning burst). An
    /// id match whose known output index differs from the event's is
    /// therefore a different item. Without an id match the output index
    /// decides. Among several candidates an open item wins over a finished
    /// one and a later one over an earlier one.
    fn find(&self, id: Option<&str>, output_index: Option<u64>) -> Option<usize> {
        let newest_open = |candidates: &mut dyn Iterator<Item = usize>| {
            candidates.max_by_key(|&i| (!self.items[i].finished, i))
        };
        if let Some(id) = id {
            let same_id = || {
                self.items
                    .iter()
                    .enumerate()
                    .filter(move |(_, item)| item.id == id)
            };
            let exact = newest_open(
                &mut same_id()
                    .filter(|(_, item)| output_index.is_some() && item.output_index == output_index)
                    .map(|(i, _)| i),
            );
            let loose = || {
                newest_open(
                    &mut same_id()
                        .filter(|(_, item)| output_index.is_none() || item.output_index.is_none())
                        .map(|(i, _)| i),
                )
            };
            if let Some(found) = exact.or_else(loose) {
                return Some(found);
            }
        }
        let index = output_index?;
        newest_open(
            &mut self
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.output_index == Some(index))
                .map(|(i, _)| i),
        )
    }

    /// Fills in the parts of an address an open item did not have yet.
    fn adopt(&mut self, item: usize, id: Option<&str>, output_index: Option<u64>) {
        let entry = &mut self.items[item];
        if entry.finished {
            return;
        }
        if entry.id.is_empty()
            && let Some(id) = id
        {
            entry.id = id.to_string();
        }
        if entry.output_index.is_none() {
            entry.output_index = output_index;
        }
    }

    /// The open item an event belongs to when its address leads nowhere: an
    /// event without any address continues the latest open item of its kind;
    /// an addressed one may be the first addressed mention of an item that
    /// has so far only been fed by unaddressed events (minimal servers send
    /// bare deltas and then a complete item).
    fn fallback(&self, id: Option<&str>, output_index: Option<u64>, kind: Kind) -> Option<usize> {
        let unaddressed = id.is_none() && output_index.is_none();
        self.items.iter().rposition(|item| {
            !item.finished
                && item.kind == kind
                && (unaddressed || (item.id.is_empty() && item.output_index.is_none()))
        })
    }

    fn register(&mut self, id: Option<&str>, output_index: Option<u64>, kind: Kind) -> usize {
        self.items.push(Item {
            id: id.unwrap_or("").to_string(),
            output_index,
            kind,
            ..Item::default()
        });
        self.items.len() - 1
    }

    /// Ends an item that turned out to be over without an `output_item.done`
    /// of its own (or already was over): closes its blocks and releases its
    /// output index, so the address can stand for the item that follows it.
    fn retire(&mut self, item: usize) {
        let mut open: Vec<u64> = self.items[item].parts.iter().map(|(_, uid)| *uid).collect();
        open.extend(self.items[item].block);
        for uid in open {
            self.close(uid);
        }
        self.items[item].finished = true;
        self.items[item].output_index = None;
    }

    /// Finds the item a delta-style event refers to (see [`Decoder::find`]
    /// and [`Decoder::fallback`]) and registers a new one when there is none.
    ///
    /// Some upstreams reuse an output index (typically 0) for every item.
    /// When the index leads to an item with another id that is complete, or
    /// that has produced content of another kind than the event carries, the
    /// event is about a new item and the older one is over.
    fn locate(&mut self, id: Option<&str>, output_index: Option<u64>, kind: Kind) -> usize {
        let id = id.map(str::trim).filter(|id| !id.is_empty());
        let found = self
            .find(id, output_index)
            .or_else(|| self.fallback(id, output_index, kind));
        let Some(found) = found else {
            return self.register(id, output_index, kind);
        };
        let entry = &self.items[found];
        let other_id = id.is_some_and(|id| !entry.id.is_empty() && entry.id != id);
        let other_kind = entry.kind != kind && entry.has_blocks();
        if other_id && (entry.finished || other_kind) {
            self.retire(found);
            return self.register(id, output_index, kind);
        }
        self.adopt(found, id, output_index);
        found
    }

    /// The item an `output_item.added` (`announces`) / `.done` event is about.
    ///
    /// The address may lead to an item that is demonstrably another one, in
    /// which case a fresh item is registered instead of mixing the two up:
    ///
    /// * the item is already complete and the event announces a new one, or
    ///   completes one with a different id (some upstreams reuse an output
    ///   index for every item, others an item id);
    /// * the item is open but has produced content of another kind, or is
    ///   announced again under another id. The older item is then over.
    fn locate_item(&mut self, wire: &Value, output_index: Option<u64>, announces: bool) -> usize {
        let kind = kind_of(type_of(wire));
        let id = non_empty(wire, "id");
        let found = self.find(id, output_index).or_else(|| {
            // A new item is never the continuation of an older one.
            if announces {
                None
            } else {
                self.fallback(id, output_index, kind)
            }
        });
        let Some(found) = found else {
            return self.register(id, output_index, kind);
        };
        let entry = &self.items[found];
        let other_id = id.is_some_and(|id| !entry.id.is_empty() && entry.id != id);
        // Two calls with different call ids are never the same item, whatever
        // their address. Only ids both sides state outright are compared.
        let other_call = stated_call_id(wire)
            .is_some_and(|call_id| !entry.stated_call.is_empty() && entry.stated_call != call_id);
        let another = if entry.finished {
            announces || other_id || other_call
        } else {
            (announces && other_id) || other_call || (entry.kind != kind && entry.has_blocks())
        };
        if another {
            self.retire(found);
            return self.register(id, output_index, kind);
        }
        self.adopt(found, id, output_index);
        found
    }

    /// The item a delta-style event (`item_id` + `output_index`) addresses,
    /// provided it is an item of the kind the event is about. An event that
    /// addresses an item of another kind (a reasoning delta for a tool call,
    /// say) has no meaning and is dropped by the caller.
    fn target(&mut self, payload: &Value, kind: Kind) -> Option<usize> {
        let item = self.locate(
            str_field(payload, "item_id"),
            u64_field(payload, "output_index"),
            kind,
        );
        let entry = &mut self.items[item];
        if entry.kind == kind {
            return Some(item);
        }
        // An item announced without a `type` takes the kind of the first
        // event that feeds it.
        if !entry.typed && !entry.finished && !entry.has_blocks() {
            entry.kind = kind;
            return Some(item);
        }
        None
    }

    /// The block of content part `content_index` of a message item.
    fn part_block(&mut self, item: usize, content_index: Option<u64>, refusal: bool) -> u64 {
        let content_index = content_index
            .or_else(|| self.items[item].parts.last().map(|(ci, _)| *ci))
            .unwrap_or(0);
        let known = self.items[item]
            .parts
            .iter()
            .find(|(ci, _)| *ci == content_index)
            .map(|(_, uid)| *uid);
        if let Some(uid) = known {
            return uid;
        }
        let uid = self.new_block(if refusal {
            BlockStart::Refusal
        } else {
            BlockStart::Text
        });
        self.items[item].parts.push((content_index, uid));
        uid
    }

    fn reasoning_block(&mut self, item: usize) -> u64 {
        if let Some(uid) = self.items[item].block {
            return uid;
        }
        let id = Some(self.items[item].id.clone()).filter(|id| !id.is_empty());
        let uid = self.new_block(BlockStart::Reasoning {
            id,
            redacted: false,
        });
        self.items[item].block = Some(uid);
        uid
    }

    fn reasoning_text(&mut self, item: usize, text: &str, channel: Channel) {
        if text.is_empty() || self.items[item].finished {
            return;
        }
        match self.items[item].channel {
            Some(active) if active != channel => return,
            _ => self.items[item].channel = Some(channel),
        }
        let uid = self.reasoning_block(item);
        if std::mem::take(&mut self.items[item].separator) {
            self.push(uid, Queued::Reasoning("\n\n".to_string()));
        }
        self.push(uid, Queued::Reasoning(text.to_string()));
        self.items[item].any_text = true;
        self.items[item].part_text = true;
    }

    /// Records what a tool-call item says about itself (fields that are
    /// already known are kept).
    fn note_tool(&mut self, item: usize, wire: &Value) {
        let entry = &mut self.items[item];
        entry.kind = Kind::Tool;
        if type_of(wire) == "custom_tool_call" {
            entry.custom = true;
        }
        if entry.call_id.is_empty() {
            entry.call_id = call_id_of(wire, false);
        }
        if entry.stated_call.is_empty()
            && let Some(call_id) = stated_call_id(wire)
        {
            entry.stated_call = call_id.to_string();
        }
        if entry.name.is_empty() {
            let name = str_field(wire, "name").unwrap_or("").trim();
            entry.name = match non_empty(wire, "namespace") {
                Some(namespace) => qualify(namespace, name),
                None => name.to_string(),
            };
        }
    }

    /// Opens the tool-call block of an item. Until the name is known the
    /// block cannot start (`force` overrides that when the item is complete).
    fn tool_block(&mut self, item: usize, force: bool) -> Option<u64> {
        if let Some(uid) = self.items[item].block {
            return Some(uid);
        }
        if self.items[item].name.is_empty() && !force {
            return None;
        }
        if self.items[item].call_id.is_empty() {
            self.items[item].call_id = new_call_id();
        }
        let entry = &self.items[item];
        let start = BlockStart::ToolCall {
            id: entry.call_id.clone(),
            name: entry.name.clone(),
            kind: if entry.custom {
                ToolCallKind::Custom
            } else {
                ToolCallKind::Function
            },
            signature: None,
        };
        let uid = self.new_block(start);
        self.items[item].block = Some(uid);
        let buffered = std::mem::take(&mut self.items[item].pending_args);
        if !buffered.is_empty() {
            self.push(uid, Queued::Args(buffered));
        }
        Some(uid)
    }

    fn tool_args(&mut self, item: usize, fragment: &str) {
        if fragment.is_empty() || self.items[item].finished {
            return;
        }
        self.items[item].args_len += fragment.len();
        match self.tool_block(item, false) {
            Some(uid) => self.push(uid, Queued::Args(fragment.to_string())),
            None => self.items[item].pending_args.push_str(fragment),
        }
    }

    /// Applies the final form of an output item: fills in whatever was not
    /// streamed and closes the item's blocks.
    fn finish_item(&mut self, item: usize, wire: &Value) {
        if self.items[item].finished {
            return;
        }
        let kind = kind_of(type_of(wire));
        if self.items[item].kind != kind && self.items[item].has_blocks() {
            // Callers pair a final form only with an item of its own kind
            // (`locate_item`, `match_final_output`). Should one slip through,
            // what was streamed stands and nothing is mixed into its blocks.
            self.retire(item);
            return;
        }
        self.items[item].kind = kind;
        match kind {
            Kind::Message => {
                let content: Vec<Value> = match wire.get("content") {
                    Some(Value::Array(parts)) => parts.clone(),
                    Some(text @ Value::String(_)) => vec![text.clone()],
                    _ => Vec::new(),
                };
                for (position, part) in content.iter().enumerate() {
                    let content_index = position as u64;
                    let (refusal, text) = match part {
                        Value::String(text) => (false, text.as_str()),
                        other => match type_of(other) {
                            "refusal" => (
                                true,
                                str_field(other, "refusal")
                                    .or_else(|| str_field(other, "text"))
                                    .unwrap_or(""),
                            ),
                            "output_text" | "text" | "" => {
                                (false, str_field(other, "text").unwrap_or(""))
                            }
                            _ => continue,
                        },
                    };
                    let existed = self.items[item]
                        .parts
                        .iter()
                        .any(|(ci, _)| *ci == content_index);
                    let uid = self.part_block(item, Some(content_index), refusal);
                    self.fill_part(uid, text, part, !existed);
                }
                let open: Vec<u64> = self.items[item].parts.iter().map(|(_, uid)| *uid).collect();
                for uid in open {
                    self.close(uid);
                }
            }
            Kind::Reasoning => {
                let text = reasoning_item_text(wire);
                if !self.items[item].any_text && !text.is_empty() {
                    // Bypass the channel bookkeeping: this is the item's own
                    // final text and nothing was streamed.
                    let uid = self.reasoning_block(item);
                    self.push(uid, Queued::Reasoning(text));
                    self.items[item].any_text = true;
                }
                // The copy in `output_item.done` is the only one that is safe
                // to replay; whatever `added` carried may be incomplete.
                if let Some(blob) = non_empty(wire, "encrypted_content") {
                    let uid = self.reasoning_block(item);
                    self.push(uid, Queued::Signature(Signature::new(P, blob)));
                }
                if let Some(uid) = self.items[item].block {
                    self.close(uid);
                }
            }
            Kind::Tool => {
                self.note_tool(item, wire);
                let custom = self.items[item].custom;
                let full = stringish(wire.get(if custom { "input" } else { "arguments" }));
                if let Some(uid) = self.tool_block(item, true) {
                    if self.items[item].args_len == 0 && !full.is_empty() {
                        self.items[item].args_len = full.len();
                        self.push(uid, Queued::Args(full));
                    }
                    self.close(uid);
                }
            }
            Kind::Other => {
                let mut parts = Vec::new();
                parts_from_output_item(wire, &mut parts);
                for part in parts {
                    let uid = self.new_block(BlockStart::Whole { part });
                    self.close(uid);
                }
            }
        }
        self.items[item].finished = true;
    }

    /// Completes a content part from its final form: the text when no delta
    /// carried it and the annotations when no event announced them.
    fn fill_part(&mut self, uid: u64, text: &str, part: &Value, fresh: bool) {
        let Some(len) = self.block_len(uid) else {
            return;
        };
        if len == 0 && !text.is_empty() {
            self.push(uid, Queued::Text(text.to_string()));
        }
        let announced = self.block_mut(uid).map_or(0, |b| b.citations);
        if fresh || announced == 0 {
            let citations: Vec<Citation> = part
                .get("annotations")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(citation_from_annotation).collect())
                .unwrap_or_default();
            for citation in citations {
                self.push(uid, Queued::Citation(citation));
            }
        }
    }

    // -- events ----------------------------------------------------------------

    fn handle(&mut self, kind: &str, payload: &Value, out: &mut Vec<StreamEvent>) {
        match kind {
            "response.created" | "response.in_progress" | "response.queued" => {
                if let Some(response) = payload.get("response") {
                    self.capture(response);
                }
                self.ensure_started(out);
            }
            "response.output_item.added" => {
                let Some(wire) = payload.get("item").filter(|v| v.is_object()) else {
                    return;
                };
                let kind = kind_of(type_of(wire));
                let item = self.locate_item(wire, u64_field(payload, "output_index"), true);
                self.items[item].kind = kind;
                self.items[item].typed = !type_of(wire).is_empty();
                if kind == Kind::Reasoning {
                    // Reserve the item's place: its text may be empty and its
                    // encrypted payload only arrives when the item completes,
                    // by which time later items may have started.
                    self.reasoning_block(item);
                }
                if kind == Kind::Tool {
                    self.note_tool(item, wire);
                    let custom = self.items[item].custom;
                    let early = stringish(wire.get(if custom { "input" } else { "arguments" }));
                    self.tool_block(item, false);
                    self.tool_args(item, &early);
                }
            }
            "response.content_part.added" => {
                let part = payload.get("part").unwrap_or(&Value::Null);
                let content_index = u64_field(payload, "content_index");
                match type_of(part) {
                    "reasoning_text" => {
                        let Some(item) = self.target(payload, Kind::Reasoning) else {
                            return;
                        };
                        self.items[item].part_text = false;
                        self.items[item].separator = self.items[item].any_text;
                        let text = str_field(part, "text").unwrap_or("").to_string();
                        self.reasoning_text(item, &text, Channel::Content);
                    }
                    other => {
                        let Some(item) = self.target(payload, Kind::Message) else {
                            return;
                        };
                        let uid = self.part_block(item, content_index, other == "refusal");
                        let text = str_field(part, "text")
                            .or_else(|| str_field(part, "refusal"))
                            .unwrap_or("");
                        if !text.is_empty() {
                            self.push(uid, Queued::Text(text.to_string()));
                        }
                    }
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let Some(item) = self.target(payload, Kind::Message) else {
                    return;
                };
                let refusal = kind == "response.refusal.delta";
                let uid = self.part_block(item, u64_field(payload, "content_index"), refusal);
                let delta = str_field(payload, "delta").unwrap_or("");
                if !delta.is_empty() {
                    self.push(uid, Queued::Text(delta.to_string()));
                }
            }
            "response.output_text.done" | "response.refusal.done" => {
                let Some(item) = self.target(payload, Kind::Message) else {
                    return;
                };
                let refusal = kind == "response.refusal.done";
                let uid = self.part_block(item, u64_field(payload, "content_index"), refusal);
                let text =
                    str_field(payload, if refusal { "refusal" } else { "text" }).unwrap_or("");
                if self.block_len(uid) == Some(0) && !text.is_empty() {
                    self.push(uid, Queued::Text(text.to_string()));
                }
            }
            "response.output_text.annotation.added" => {
                let Some(item) = self.target(payload, Kind::Message) else {
                    return;
                };
                let uid = self.part_block(item, u64_field(payload, "content_index"), false);
                let citation = payload.get("annotation").and_then(citation_from_annotation);
                if let Some(citation) = citation {
                    self.push(uid, Queued::Citation(citation));
                }
            }
            "response.content_part.done" => {
                let part = payload.get("part").unwrap_or(&Value::Null);
                match type_of(part) {
                    "reasoning_text" => {
                        let Some(item) = self.target(payload, Kind::Reasoning) else {
                            return;
                        };
                        if !self.items[item].part_text {
                            let text = str_field(part, "text").unwrap_or("").to_string();
                            self.reasoning_text(item, &text, Channel::Content);
                        }
                    }
                    other => {
                        let Some(item) = self.target(payload, Kind::Message) else {
                            return;
                        };
                        let refusal = other == "refusal";
                        let uid =
                            self.part_block(item, u64_field(payload, "content_index"), refusal);
                        let text = str_field(part, "text")
                            .or_else(|| str_field(part, "refusal"))
                            .unwrap_or("");
                        self.fill_part(uid, text, part, false);
                        self.close(uid);
                    }
                }
            }
            "response.reasoning_summary_part.added" => {
                let Some(item) = self.target(payload, Kind::Reasoning) else {
                    return;
                };
                self.items[item].part_text = false;
                self.items[item].separator = self.items[item].any_text;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let Some(item) = self.target(payload, Kind::Reasoning) else {
                    return;
                };
                let channel = if kind == "response.reasoning_text.delta" {
                    Channel::Content
                } else {
                    Channel::Summary
                };
                let delta = str_field(payload, "delta").unwrap_or("").to_string();
                self.reasoning_text(item, &delta, channel);
            }
            "response.reasoning_summary_text.done"
            | "response.reasoning_text.done"
            | "response.reasoning_summary_part.done" => {
                let Some(item) = self.target(payload, Kind::Reasoning) else {
                    return;
                };
                if self.items[item].part_text {
                    return;
                }
                let channel = if kind == "response.reasoning_text.done" {
                    Channel::Content
                } else {
                    Channel::Summary
                };
                let text = str_field(payload, "text")
                    .or_else(|| payload.get("part").and_then(|p| str_field(p, "text")))
                    .unwrap_or("")
                    .to_string();
                self.reasoning_text(item, &text, channel);
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                let Some(item) = self.target(payload, Kind::Tool) else {
                    return;
                };
                if kind == "response.custom_tool_call_input.delta" {
                    self.items[item].custom = true;
                }
                let delta = str_field(payload, "delta").unwrap_or("").to_string();
                self.tool_args(item, &delta);
            }
            "response.function_call_arguments.done" | "response.custom_tool_call_input.done" => {
                let Some(item) = self.target(payload, Kind::Tool) else {
                    return;
                };
                let custom = kind == "response.custom_tool_call_input.done";
                if custom {
                    self.items[item].custom = true;
                }
                if self.items[item].name.is_empty()
                    && let Some(name) = non_empty(payload, "name")
                {
                    self.items[item].name = name.to_string();
                }
                if self.items[item].args_len == 0 {
                    let full = stringish(payload.get(if custom { "input" } else { "arguments" }));
                    self.tool_args(item, &full);
                }
            }
            "response.output_item.done" => {
                let Some(wire) = payload.get("item").filter(|v| v.is_object()) else {
                    return;
                };
                let item = self.locate_item(wire, u64_field(payload, "output_index"), false);
                self.finish_item(item, wire);
            }
            // Everything else (`response.web_search_call.*`, audio, MCP
            // progress, keep-alives, future additions) has no canonical form.
            _ => {}
        }
    }

    /// Pairs every entry of the terminal `output` array with the streamed
    /// item it describes (`None`: the item was never mentioned before). Each
    /// streamed item is paired at most once, so content that was streamed is
    /// never applied a second time and items that share an id stay apart.
    ///
    /// Addresses come first: the item id (the one at the same position when
    /// several items share it, else the earliest still unpaired), then the
    /// array position as output index as long as kind and ids do not say it
    /// is another item. The entries left over belong to items that were
    /// streamed without a usable address; they are paired by call id and
    /// then, in order of appearance, with unpaired items of the same kind
    /// whose ids do not contradict.
    fn match_final_output(&self, output: &[Value]) -> Vec<Option<usize>> {
        let mut paired = vec![false; self.items.len()];
        let mut matches: Vec<Option<usize>> = vec![None; output.len()];
        for (position, wire) in output.iter().enumerate() {
            if !wire.is_object() {
                continue;
            }
            let index = Some(position as u64);
            let kind = kind_of(type_of(wire));
            let id = non_empty(wire, "id");
            let call_id = stated_call_id(wire);
            // An item that streamed content of another kind, or a call with
            // another call id, is another item whatever its address says.
            let unpaired = || {
                self.items.iter().enumerate().filter(|(i, item)| {
                    !paired[*i]
                        && (item.kind == kind || !item.has_blocks())
                        && !call_id
                            .is_some_and(|c| !item.stated_call.is_empty() && item.stated_call != c)
                })
            };
            let by_id = id.and_then(|id| {
                unpaired()
                    .find(|(_, item)| item.id == id && item.output_index == index)
                    .or_else(|| unpaired().find(|(_, item)| item.id == id))
            });
            let found = by_id.or_else(|| {
                unpaired().find(|(_, item)| {
                    item.output_index == index
                        && item.kind == kind
                        && (id.is_none() || item.id.is_empty())
                })
            });
            if let Some((found, _)) = found {
                paired[found] = true;
                matches[position] = Some(found);
            }
        }
        for (position, wire) in output.iter().enumerate() {
            if matches[position].is_some() || !wire.is_object() {
                continue;
            }
            let kind = kind_of(type_of(wire));
            let id = non_empty(wire, "id");
            let call_id = stated_call_id(wire);
            let unpaired = || {
                self.items
                    .iter()
                    .enumerate()
                    .filter(|(i, item)| !paired[*i] && item.kind == kind)
            };
            let found = unpaired()
                .find(|(_, item)| call_id.is_some_and(|c| item.stated_call == c))
                .or_else(|| {
                    unpaired().find(|(_, item)| {
                        (id.is_none() || item.id.is_empty())
                            && !call_id.is_some_and(|c| {
                                !item.stated_call.is_empty() && item.stated_call != c
                            })
                    })
                });
            if let Some((found, _)) = found {
                paired[found] = true;
                matches[position] = Some(found);
            }
        }
        matches
    }

    /// `response.completed` / `response.incomplete` / `response.done`.
    fn terminal(&mut self, kind: &str, payload: &Value, out: &mut Vec<StreamEvent>) {
        let response = payload
            .get("response")
            .filter(|v| v.is_object())
            .unwrap_or(payload);
        self.capture(response);
        self.ensure_started(out);

        // Items that never got an `output_item.done` of their own are taken
        // from the final output array. An empty array (some upstreams send
        // one) simply contributes nothing: the items were already applied.
        if let Some(Value::Array(output)) = response.get("output") {
            let matches = self.match_final_output(output);
            for (wire, known) in output.iter().zip(matches) {
                if !wire.is_object() {
                    continue;
                }
                let item = known.unwrap_or_else(|| {
                    self.register(non_empty(wire, "id"), None, kind_of(type_of(wire)))
                });
                self.finish_item(item, wire);
            }
        }
        self.close_all();
        self.drain(out);

        let usage = response
            .get("usage")
            .and_then(usage_from_wire)
            .or_else(|| payload.get("usage").and_then(usage_from_wire));
        if let Some(usage) = usage {
            out.push(StreamEvent::Usage(usage));
        }
        let status = match kind {
            "response.incomplete" => "incomplete",
            _ => str_field(response, "status").unwrap_or("completed"),
        };
        let reason = response
            .get("incomplete_details")
            .and_then(|d| str_field(d, "reason"));
        out.push(StreamEvent::Finish {
            reason: finish_from(status, reason, self.kinds),
            stop_sequence: None,
        });
        self.finished = true;
    }

    /// Closes every block that exists. Calls whose name never arrived have no
    /// block and are dropped.
    fn close_all(&mut self) {
        for block in &mut self.blocks {
            block.closed = true;
        }
    }
}

/// Event types that end a Responses stream.
fn is_terminal(kind: &str) -> bool {
    matches!(
        kind,
        "response.completed"
            | "response.incomplete"
            | "response.failed"
            | "response.done"
            | "response.error"
            | "error"
    )
}

/// Whether a frame reports a failure: an error event, or any frame that
/// carries a non-null error object.
fn is_failure(kind: &str, payload: &Value) -> bool {
    if matches!(kind, "error" | "response.failed" | "response.error") {
        return true;
    }
    let has_error = |value: &Value| {
        value
            .get("error")
            .is_some_and(|e| e.is_object() || e.is_string())
    };
    if has_error(payload) || payload.get("response").is_some_and(has_error) {
        return true;
    }
    kind.is_empty() && payload.get("code").is_some() && payload.get("message").is_some()
}

impl StreamDecoder for Decoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, CodecError> {
        let mut out = Vec::new();
        if self.finished {
            return Ok(out);
        }
        let data = event.data.trim();
        if data.is_empty() || event.is_done_marker() {
            return Ok(out);
        }
        let Ok(payload) = serde_json::from_str::<Value>(data) else {
            return Ok(out);
        };
        if !payload.is_object() {
            return Ok(out);
        }
        let named = event.event.as_deref().map(str::trim).unwrap_or("");
        // The JSON `type` is authoritative, except that a terminal SSE event
        // name is trusted over a payload that forgot to say so.
        let kind = match non_empty(&payload, "type") {
            Some(kind) if !is_terminal(named) || is_terminal(kind) => kind,
            _ => named,
        };

        if is_failure(kind, &payload) {
            self.finished = true;
            // The sequence contract wants a `Start` even when the failure is
            // the first thing the upstream says.
            if let Some(response) = payload.get("response") {
                self.capture(response);
            }
            self.ensure_started(&mut out);
            out.push(StreamEvent::Error(api_error_from_stream(&payload)));
            return Ok(out);
        }
        if is_terminal(kind) {
            self.terminal(kind, &payload, &mut out);
            return Ok(out);
        }
        self.handle(kind, &payload, &mut out);
        self.drain(&mut out);
        Ok(out)
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.finished = true;
        self.ensure_started(&mut out);
        self.close_all();
        self.drain(&mut out);
        out.push(StreamEvent::Finish {
            reason: FinishReason::Error,
            stop_sequence: None,
        });
        out
    }
}
