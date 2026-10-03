//! Replay memory for reasoning blobs that clients cannot carry.
//!
//! Reasoning models bind tool use to opaque, provider-signed state: Anthropic
//! `thinking` blocks carry a `signature`, OpenAI Responses reasoning items
//! carry `encrypted_content`, Gemini attaches a `thoughtSignature` to the
//! function call itself. On the next turn the provider expects that state
//! back next to the tool call it belongs to — and rejects the request or
//! silently loses its train of thought when it is missing.
//!
//! A client that speaks a protocol with no slot for such blobs (Chat
//! Completions being the common case) cannot send them back, and many clients
//! of the richer protocols drop them too. The [`ReasoningStore`] closes that
//! gap for translated requests:
//!
//! * after every upstream response the gateway calls
//!   [`ReasoningStore::remember`] (or one of the stream variants). For each
//!   tool call in the response the store keeps the signed reasoning parts
//!   that preceded it and the call's own signature, keyed by
//!   `(scope, tool call id)`;
//! * before encoding a later request the gateway calls
//!   [`ReasoningStore::restore`], which puts those parts back into assistant
//!   turns that lost them.
//!
//! Blobs are vendor-bound: only reasoning and signatures whose
//! [`Signature::valid_for`] the target protocol are ever restored, so one
//! vendor's state is never replayed to another.
//!
//! `scope` isolates callers. Hidden reasoning must never leak from one client
//! to another, so the gateway passes a value that identifies the client key
//! (and may add the provider or a session id to it); the store treats it as
//! an opaque string.
//!
//! The store is bounded in entries and, optionally, in bytes
//! ([`ReasoningStore::with_max_bytes`]; encrypted reasoning can be large).
//! Least recently used entries are evicted first, entries expire `ttl` after
//! they were last stored or used, and all operations are `O(1)` apart from
//! the work proportional to the request or response being processed.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use switchyard_core::ir::{Message, Part, Reasoning, Request, Response, Role, Signature, ToolCall};
use switchyard_core::protocol::Protocol;
use switchyard_core::stream::{Accumulator, StreamEvent};

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// A source of monotonic time, injectable so expiry can be tested without
/// sleeping.
pub trait Clock: Send + Sync + 'static {
    /// The current instant. Must never go backwards.
    fn now(&self) -> Instant;
}

/// The real monotonic clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A clock that only moves when told to. For tests.
#[derive(Debug)]
pub struct ManualClock {
    start: Instant,
    elapsed: Mutex<Duration>,
}

impl ManualClock {
    /// A clock frozen at the moment it was created.
    pub fn new() -> Self {
        ManualClock {
            start: Instant::now(),
            elapsed: Mutex::new(Duration::ZERO),
        }
    }

    /// Moves the clock forward.
    pub fn advance(&self, by: Duration) {
        let mut elapsed = self.elapsed.lock();
        *elapsed = elapsed.saturating_add(by);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        ManualClock::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        let elapsed = *self.elapsed.lock();
        self.start.checked_add(elapsed).unwrap_or(self.start)
    }
}

// ---------------------------------------------------------------------------
// Stored data
// ---------------------------------------------------------------------------

/// A reasoning part that preceded a tool call, with enough positional
/// information to put it back where it was.
///
/// Positions are *places*: the number of non-reasoning parts the response had
/// in front of something. Reasoning parts do not count because they are what
/// the client lost; the parts that do count are the ones the client can be
/// expected to send back.
#[derive(Clone, Debug)]
struct Preceding {
    /// Shared between the entries of all tool calls of one response.
    part: Arc<Reasoning>,
    /// Place of this part in the response.
    place: usize,
    /// Number of text-like parts before this reasoning part.
    texts_before: usize,
}

/// What is remembered for one tool call.
#[derive(Clone, Debug)]
struct Remembered {
    /// Shared once per response, with each call retaining only its prefix.
    reasoning: Arc<[Preceding]>,
    reasoning_len: usize,
    reasoning_cost: usize,
    texts_before: usize,
    signature: Option<Signature>,
    /// Place of the call itself in the response.
    place: usize,
    /// Place of every tool call of the response, by id. Tells which calls of
    /// a later request sat between a reasoning part and this call. Shared
    /// between the entries of one response.
    calls: Arc<HashMap<String, usize>>,
    calls_cost: usize,
}

impl Remembered {
    /// Whether the response had `call` between `preceding` and this entry's
    /// own call, i.e. *after* the reasoning part.
    fn had_between(&self, preceding: &Preceding, call: &ToolCall) -> bool {
        self.calls
            .get(&call.id)
            .is_some_and(|&place| place >= preceding.place && place < self.place)
    }

    /// Approximate heap size of the entry stored under `key`, for the byte
    /// budget. Reasoning parts shared with other entries are counted in
    /// full, so the sum over all entries is an upper bound.
    fn cost(&self, key: &str) -> usize {
        // Struct, map slot and allocator overhead per entry and per part.
        const OVERHEAD: usize = 96;
        let blob = |signature: &Option<Signature>| signature.as_ref().map_or(0, |s| s.data.len());
        // The key is held twice: by the map and by the slot.
        OVERHEAD + 2 * key.len() + blob(&self.signature) + self.reasoning_cost + self.calls_cost
    }
}

// ---------------------------------------------------------------------------
// LRU
// ---------------------------------------------------------------------------

const NIL: usize = usize::MAX;

struct Slot {
    key: String,
    entry: Remembered,
    /// `entry.cost(key)`, kept so removal does not have to recompute it.
    cost: usize,
    touched: Instant,
    /// Towards the most recently used end.
    prev: usize,
    /// Towards the least recently used end.
    next: usize,
}

/// Hash map plus an index-linked recency list: every operation is `O(1)`.
/// Because expiry is measured from the last touch, the list is also ordered
/// by expiry time, so expired entries are always found at the tail.
struct Lru {
    map: HashMap<String, usize>,
    slots: Vec<Option<Slot>>,
    free: Vec<usize>,
    /// Most recently used.
    head: usize,
    /// Least recently used.
    tail: usize,
    /// Sum of the costs of all slots.
    bytes: usize,
}

impl Lru {
    fn new() -> Self {
        Lru {
            map: HashMap::new(),
            slots: Vec::new(),
            free: Vec::new(),
            head: NIL,
            tail: NIL,
            bytes: 0,
        }
    }

    fn slot(&self, index: usize) -> Option<&Slot> {
        self.slots.get(index).and_then(Option::as_ref)
    }

    fn slot_mut(&mut self, index: usize) -> Option<&mut Slot> {
        self.slots.get_mut(index).and_then(Option::as_mut)
    }

    fn detach(&mut self, index: usize) {
        let Some((prev, next)) = self.slot(index).map(|s| (s.prev, s.next)) else {
            return;
        };
        match self.slot_mut(prev) {
            Some(p) => p.next = next,
            None => self.head = next,
        }
        match self.slot_mut(next) {
            Some(n) => n.prev = prev,
            None => self.tail = prev,
        }
        if let Some(slot) = self.slot_mut(index) {
            slot.prev = NIL;
            slot.next = NIL;
        }
    }

    fn attach_front(&mut self, index: usize) {
        let old_head = self.head;
        if let Some(slot) = self.slot_mut(index) {
            slot.prev = NIL;
            slot.next = old_head;
        }
        match self.slot_mut(old_head) {
            Some(head) => head.prev = index,
            None => self.tail = index,
        }
        self.head = index;
    }

    fn remove_index(&mut self, index: usize) -> bool {
        self.detach(index);
        match self.slots.get_mut(index).and_then(Option::take) {
            Some(slot) => {
                self.map.remove(&slot.key);
                self.free.push(index);
                self.bytes = self.bytes.saturating_sub(slot.cost);
                true
            }
            None => false,
        }
    }

    fn remove(&mut self, key: &str) -> bool {
        match self.map.get(key).copied() {
            Some(index) => self.remove_index(index),
            None => false,
        }
    }

    /// Stores `entry` as the most recently used one, replacing an entry with
    /// the same key, then evicts from the least recently used end until the
    /// store is within `capacity` entries and `max_bytes` bytes. An entry
    /// that is larger than the whole byte budget is not kept.
    fn insert(
        &mut self,
        key: String,
        entry: Remembered,
        now: Instant,
        capacity: usize,
        max_bytes: usize,
    ) {
        let cost = entry.cost(&key);
        if cost > max_bytes {
            // Whatever was stored for this call is stale now.
            self.remove(&key);
            return;
        }
        if let Some(index) = self.map.get(&key).copied() {
            let mut replaced = 0;
            if let Some(slot) = self.slot_mut(index) {
                replaced = std::mem::replace(&mut slot.cost, cost);
                slot.entry = entry;
                slot.touched = now;
            }
            self.bytes = self.bytes.saturating_sub(replaced).saturating_add(cost);
            self.detach(index);
            self.attach_front(index);
        } else {
            let slot = Slot {
                key: key.clone(),
                entry,
                cost,
                touched: now,
                prev: NIL,
                next: NIL,
            };
            let index = match self.free.pop() {
                Some(index) if index < self.slots.len() => {
                    self.slots[index] = Some(slot);
                    index
                }
                _ => {
                    self.slots.push(Some(slot));
                    self.slots.len() - 1
                }
            };
            self.map.insert(key, index);
            self.attach_front(index);
            self.bytes = self.bytes.saturating_add(cost);
        }
        while self.map.len() > capacity || self.bytes > max_bytes {
            let tail = self.tail;
            if !self.remove_index(tail) {
                break;
            }
        }
    }

    /// Looks an entry up, refreshing its recency and expiry. An expired entry
    /// is dropped and reported as absent.
    fn touch(&mut self, key: &str, now: Instant, ttl: Duration) -> Option<Remembered> {
        let index = self.map.get(key).copied()?;
        let expired = self
            .slot(index)
            .is_none_or(|slot| is_expired(slot.touched, now, ttl));
        if expired {
            self.remove_index(index);
            return None;
        }
        self.detach(index);
        self.attach_front(index);
        let slot = self.slot_mut(index)?;
        slot.touched = now;
        Some(slot.entry.clone())
    }

    fn evict_expired(&mut self, now: Instant, ttl: Duration) {
        loop {
            let tail = self.tail;
            match self.slot(tail) {
                Some(slot) if is_expired(slot.touched, now, ttl) => {
                    self.remove_index(tail);
                }
                _ => break,
            }
        }
    }

    fn clear(&mut self) {
        *self = Lru::new();
    }
}

fn is_expired(touched: Instant, now: Instant, ttl: Duration) -> bool {
    now.saturating_duration_since(touched) >= ttl
}

/// `(scope, call id)` as one unambiguous string: the scope's length prefix
/// keeps `("ab", "c")` and `("a", "bc")` apart.
fn entry_key(scope: &str, call_id: &str) -> String {
    format!("{}:{scope}{call_id}", scope.len())
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// What a [`ReasoningStore::restore`] call put back into a request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Restored {
    /// Reasoning parts inserted into assistant messages.
    pub reasoning_parts: usize,
    /// Tool-call signatures filled in.
    pub signatures: usize,
}

impl Restored {
    /// True when the request was not changed.
    pub fn is_empty(&self) -> bool {
        self.reasoning_parts == 0 && self.signatures == 0
    }
}

/// Bounded, expiring memory of reasoning parts and tool-call signatures. See
/// the module docs.
pub struct ReasoningStore {
    capacity: usize,
    max_bytes: usize,
    ttl: Duration,
    clock: Arc<dyn Clock>,
    inner: Mutex<Lru>,
}

impl std::fmt::Debug for ReasoningStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the contents: reasoning blobs do not belong in logs.
        f.debug_struct("ReasoningStore")
            .field("capacity", &self.capacity)
            .field("max_bytes", &self.max_bytes)
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl ReasoningStore {
    /// A store holding at most `capacity` tool calls, each for `ttl` after it
    /// was last stored or used. A capacity of zero (or a zero `ttl`) disables
    /// the store: nothing is kept and nothing is restored.
    ///
    /// The number of bytes held is not limited unless
    /// [`ReasoningStore::with_max_bytes`] is applied.
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        ReasoningStore::with_clock(capacity, ttl, Arc::new(SystemClock))
    }

    /// Like [`ReasoningStore::new`] with an explicit time source.
    pub fn with_clock(capacity: usize, ttl: Duration, clock: Arc<dyn Clock>) -> Self {
        ReasoningStore {
            capacity,
            max_bytes: usize::MAX,
            ttl,
            clock,
            inner: Mutex::new(Lru::new()),
        }
    }

    /// Additionally limits the store to roughly `max_bytes` bytes of
    /// remembered data (reasoning text, signatures, ids, plus a fixed
    /// overhead per entry). Least recently used entries are evicted when the
    /// budget is exceeded, and a single tool call whose state is larger than
    /// the whole budget is not remembered at all. Zero disables the store.
    ///
    /// The accounting is an upper bound: a reasoning part that precedes
    /// several parallel tool calls is stored once but counted once per call.
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    fn disabled(&self) -> bool {
        self.capacity == 0 || self.max_bytes == 0 || self.ttl.is_zero()
    }

    /// Number of live (unexpired) entries, one per remembered tool call.
    pub fn len(&self) -> usize {
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        inner.evict_expired(now, self.ttl);
        inner.map.len()
    }

    /// The size of the live entries as counted against
    /// [`ReasoningStore::with_max_bytes`]: an upper bound of the memory the
    /// remembered data occupies.
    pub fn approx_bytes(&self) -> usize {
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        inner.evict_expired(now, self.ttl);
        inner.bytes
    }

    /// True when no live entry is stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drops everything.
    pub fn clear(&self) {
        self.inner.lock().clear();
    }

    /// Remembers the replayable state of every tool call in `response`:
    /// the reasoning parts that came before the call in the same response
    /// (only parts that carry a signature — unsigned reasoning cannot be
    /// replayed) and the call's own signature. Returns the number of tool
    /// calls that had such state; calls with neither are skipped, as are
    /// calls without an id. (With a byte budget, a call whose state exceeds
    /// the whole budget is counted here but not kept.)
    pub fn remember(&self, response: &Response, scope: &str) -> usize {
        if self.disabled() {
            return 0;
        }
        struct Seen<'a> {
            part: &'a Reasoning,
            place: usize,
            texts_before: usize,
            cost: usize,
        }
        struct Pending<'a> {
            call: &'a ToolCall,
            place: usize,
            reasoning_len: usize,
            texts_before: usize,
        }

        let mut pending: Vec<Pending<'_>> = Vec::new();
        let mut seen: Vec<Seen<'_>> = Vec::new();
        let mut places: HashMap<String, usize> = HashMap::new();
        let mut reasoning_cost = 0usize;
        // Non-reasoning parts so far, and how many of them were not calls.
        let mut place = 0usize;
        let mut texts = 0usize;
        for part in &response.parts {
            match part {
                Part::Reasoning(reasoning) => {
                    if reasoning.signature.is_some() {
                        reasoning_cost = reasoning_cost.saturating_add(
                            96 + reasoning.text.len()
                                + reasoning.id.as_ref().map_or(0, String::len)
                                + reasoning.signature.as_ref().map_or(0, |s| s.data.len()),
                        );
                        seen.push(Seen {
                            part: reasoning,
                            place,
                            texts_before: texts,
                            cost: reasoning_cost,
                        });
                    }
                }
                Part::ToolCall(call) => {
                    if !call.id.is_empty() {
                        places.insert(call.id.clone(), place);
                        if !seen.is_empty() || call.signature.is_some() {
                            pending.push(Pending {
                                call,
                                place,
                                reasoning_len: seen.len(),
                                texts_before: texts,
                            });
                        }
                    }
                    place += 1;
                }
                _ => {
                    place += 1;
                    texts += 1;
                }
            }
        }
        if pending.is_empty() {
            return 0;
        }
        let stored = pending.len();
        let calls_cost: usize = places.keys().map(|id| id.len() + 32).sum();
        // Refuse entries before cloning signed payloads. All calls share one
        // prefix array, so pending metadata and construction are linear in
        // response parts instead of calls times reasoning parts.
        let key_prefix_bytes = scope.len().to_string().len() + 1 + scope.len();
        let entry_cost = |item: &Pending<'_>, parts_cost: usize| {
            96usize
                .saturating_add(2usize.saturating_mul(key_prefix_bytes + item.call.id.len()))
                .saturating_add(item.call.signature.as_ref().map_or(0, |s| s.data.len()))
                .saturating_add(parts_cost)
                .saturating_add(calls_cost)
        };
        // An unaffordable later prefix must not prevent an earlier,
        // affordable call from being retained.
        let kept = pending
            .iter()
            .filter(|item| {
                let cost = item
                    .reasoning_len
                    .checked_sub(1)
                    .map_or(0, |last| seen[last].cost);
                entry_cost(item, cost) <= self.max_bytes
            })
            .map(|item| item.reasoning_len)
            .max()
            .unwrap_or(0);
        let shared_reasoning_cost = kept.checked_sub(1).map_or(0, |last| seen[last].cost);
        let reasoning: Arc<[Preceding]> = seen
            .into_iter()
            .take(kept)
            .map(|seen| Preceding {
                part: Arc::new(seen.part.clone()),
                place: seen.place,
                texts_before: seen.texts_before,
            })
            .collect();
        let places = Arc::new(places);
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        inner.evict_expired(now, self.ttl);
        for item in pending {
            let key = entry_key(scope, &item.call.id);
            if item.reasoning_len > reasoning.len() {
                inner.remove(&key);
                continue;
            }
            let parts_cost = if item.reasoning_len == 0 {
                0
            } else {
                shared_reasoning_cost
            };
            if entry_cost(&item, parts_cost) > self.max_bytes {
                inner.remove(&key);
                continue;
            }
            let entry = Remembered {
                reasoning: if item.reasoning_len == 0 {
                    Arc::from([])
                } else {
                    Arc::clone(&reasoning)
                },
                reasoning_len: item.reasoning_len,
                // Every entry holds the whole shared allocation alive.
                reasoning_cost: parts_cost,
                texts_before: item.texts_before,
                signature: item.call.signature.clone(),
                place: item.place,
                calls: Arc::clone(&places),
                calls_cost,
            };
            inner.insert(key, entry, now, self.capacity, self.max_bytes);
        }
        stored
    }

    /// [`remember`](Self::remember) for a streamed response, given the
    /// accumulator the stream was folded into (for example
    /// `Transcoder::accumulator`).
    pub fn remember_accumulator(&self, accumulator: &Accumulator, scope: &str) -> usize {
        if self.disabled() {
            return 0;
        }
        self.remember(&accumulator.snapshot(), scope)
    }

    /// [`remember`](Self::remember) for a streamed response, given its
    /// canonical events.
    pub fn remember_events(&self, events: &[StreamEvent], scope: &str) -> usize {
        if self.disabled() {
            return 0;
        }
        let mut accumulator = Accumulator::new();
        for event in events {
            accumulator.push(event);
        }
        self.remember(&accumulator.into_response(), scope)
    }

    /// Puts remembered state back into the assistant turns of `request`,
    /// which is about to be encoded for an upstream speaking `target`.
    ///
    /// For every tool call in an assistant message whose id is known in
    /// `scope`:
    ///
    /// * a missing signature on the call (or one that is not valid for
    ///   `target`) is replaced by the remembered one, if that one is valid
    ///   for `target`;
    /// * if no reasoning part with a signature valid for `target` precedes
    ///   the call in its message, the remembered reasoning parts whose
    ///   signature is valid for `target` are inserted again, in their
    ///   original order and at their original position relative to the other
    ///   parts: directly before the call when nothing separated them in the
    ///   response — for a turn with several calls that is before the first
    ///   call — and before the text that separated them otherwise (providers
    ///   insist that such a turn *starts* with its reasoning). A part that is
    ///   already present is not inserted twice.
    ///
    /// The client may have rearranged the turn (a Chat Completions client
    /// sends all text first and merges it into one string, clients drop
    /// calls). As long as it kept the calls themselves in order, restored
    /// parts keep the order they had in the response, both among themselves
    /// and relative to the tool calls: a part is never placed in front of a
    /// call it was generated after, nor behind one it was generated before.
    /// Where that leaves a choice, the response's layout decides (the part
    /// goes back in front of as much text as followed it); a call the
    /// response did not have between the part and its own call — an earlier
    /// call, or one from another response — is never crossed.
    ///
    /// Unsigned reasoning text that merely duplicates a restored part (a
    /// client echoing `reasoning_content` back) is removed in favour of the
    /// signed original.
    ///
    /// Messages of other roles and unknown call ids are left alone. Entries
    /// that are used have their expiry refreshed.
    pub fn restore(&self, request: &mut Request, target: Protocol, scope: &str) -> Restored {
        let mut restored = Restored::default();
        if self.disabled() {
            return restored;
        }

        // Look everything up under one short lock, mutate afterwards.
        let mut lookups: Vec<(usize, Vec<Option<Remembered>>)> = Vec::new();
        {
            let mut guard: Option<(parking_lot::MutexGuard<'_, Lru>, Instant)> = None;
            for (index, message) in request.messages.iter().enumerate() {
                if message.role != Role::Assistant {
                    continue;
                }
                let mut entries = Vec::new();
                for call in message.tool_calls() {
                    let (inner, now) =
                        guard.get_or_insert_with(|| (self.inner.lock(), self.clock.now()));
                    entries.push(inner.touch(&entry_key(scope, &call.id), *now, self.ttl));
                }
                if entries.iter().any(Option::is_some) {
                    lookups.push((index, entries));
                }
            }
        }

        for (index, entries) in lookups {
            if let Some(message) = request.messages.get_mut(index) {
                restore_message(message, &entries, target, &mut restored);
            }
        }
        restored
    }

    /// Forgets the tool calls that appear in the assistant turns of
    /// `request`. For use when an upstream rejects replayed state (an
    /// "invalid signature" error): the entries are evidently no good, and
    /// dropping them lets the next attempt go out without them. Returns the
    /// number of entries removed.
    pub fn forget(&self, request: &Request, scope: &str) -> usize {
        let mut removed = 0;
        let mut inner = self.inner.lock();
        for message in &request.messages {
            if message.role != Role::Assistant {
                continue;
            }
            for call in message.tool_calls() {
                if inner.remove(&entry_key(scope, &call.id)) {
                    removed += 1;
                }
            }
        }
        removed
    }

    /// Forgets one tool call. Returns whether it was known.
    pub fn forget_call(&self, scope: &str, call_id: &str) -> bool {
        self.inner.lock().remove(&entry_key(scope, call_id))
    }
}

fn valid_for(signature: &Option<Signature>, target: Protocol) -> bool {
    signature.as_ref().is_some_and(|s| s.valid_for(target))
}

/// Position of the `ordinal`-th tool call part of a message.
fn nth_call(parts: &[Part], ordinal: usize) -> Option<usize> {
    parts
        .iter()
        .enumerate()
        .filter(|(_, part)| matches!(part, Part::ToolCall(_)))
        .nth(ordinal)
        .map(|(index, _)| index)
}

/// Where a remembered reasoning part goes in `parts`, the client's version of
/// the turn, given the position `call_at` of the call it is restored for.
///
/// In the response the part was followed by `texts_between` text-like parts
/// and by the calls for which `had_between` holds, then by its call. The
/// walk goes back from the call and moves the insertion point in front of
///
/// * every call that `had_between` — the reasoning came before them, so it
///   must stay before them however the client arranged the rest — and
/// * up to `texts_between` text-like parts,
///
/// and stops for good at any other call: that one was generated *before*
/// the reasoning (or belongs to another response), and a reasoning part that
/// jumped over it would also overtake the reasoning restored for it. Text
/// beyond the budget is not passed, but does not end the walk either: a call
/// that must follow the reasoning may still sit further back.
///
/// Reasoning parts are stepped over without moving the insertion point, so
/// the part lands directly in front of the furthest part it had to pass,
/// behind reasoning that was placed there earlier. That is what keeps the
/// parts of one entry, which are restored first to last, in order.
fn insertion_point(
    parts: &[Part],
    call_at: usize,
    texts_between: usize,
    had_between: impl Fn(&ToolCall) -> bool,
) -> usize {
    let mut at = call_at.min(parts.len());
    let mut texts_left = texts_between;
    let mut index = at;
    while index > 0 {
        index -= 1;
        match &parts[index] {
            Part::Reasoning(_) => {}
            Part::ToolCall(call) => {
                if !had_between(call) {
                    break;
                }
                at = index;
            }
            _ => {
                if texts_left > 0 {
                    texts_left -= 1;
                    at = index;
                }
            }
        }
    }
    at
}

fn restore_message(
    message: &mut Message,
    entries: &[Option<Remembered>],
    target: Protocol,
    restored: &mut Restored,
) {
    // Eligibility is judged on the message as the client sent it, not on the
    // message as it looks after earlier insertions.
    let first_signed_reasoning = message
        .parts
        .iter()
        .position(|part| matches!(part, Part::Reasoning(r) if valid_for(&r.signature, target)));
    let eligible: Vec<bool> = message
        .parts
        .iter()
        .enumerate()
        .filter(|(_, part)| matches!(part, Part::ToolCall(_)))
        .map(|(index, _)| first_signed_reasoning.is_none_or(|r| r > index))
        .collect();

    let mut inserted: Vec<Arc<Reasoning>> = Vec::new();
    for (ordinal, entry) in entries.iter().enumerate() {
        let Some(entry) = entry else {
            continue;
        };

        if let Some(at) = nth_call(&message.parts, ordinal)
            && let Some(Part::ToolCall(call)) = message.parts.get_mut(at)
            && !valid_for(&call.signature, target)
            && valid_for(&entry.signature, target)
        {
            call.signature = entry.signature.clone();
            restored.signatures += 1;
        }

        if !eligible.get(ordinal).copied().unwrap_or(false) {
            continue;
        }
        for preceding in entry.reasoning.iter().take(entry.reasoning_len) {
            if !valid_for(&preceding.part.signature, target) {
                continue;
            }
            let already_there = message
                .parts
                .iter()
                .any(|part| matches!(part, Part::Reasoning(r) if r == preceding.part.as_ref()));
            if already_there {
                continue;
            }
            let Some(call_at) = nth_call(&message.parts, ordinal) else {
                break;
            };
            let at = insertion_point(
                &message.parts,
                call_at,
                entry.texts_before - preceding.texts_before,
                |call| entry.had_between(preceding, call),
            );
            message
                .parts
                .insert(at, Part::Reasoning(preceding.part.as_ref().clone()));
            inserted.push(Arc::clone(&preceding.part));
            restored.reasoning_parts += 1;
        }
    }

    if !inserted.is_empty() {
        message.parts.retain(|part| match part {
            Part::Reasoning(r) if r.signature.is_none() && !r.redacted && !r.text.is_empty() => {
                !inserted.iter().any(|kept| kept.text == r.text)
            }
            _ => true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use switchyard_core::ir::{FinishReason, ToolCall};
    use switchyard_core::stream::response_to_events;

    const HOUR: Duration = Duration::from_secs(3600);

    #[test]
    fn oversized_later_reasoning_keeps_affordable_earlier_calls() {
        let (store, _) = store(10);
        let store = store.with_max_bytes(1000);
        let result = response(vec![
            call_signed("signature-only", Protocol::Anthropic, "call-sig"),
            signed("first", Protocol::Anthropic, "sig-1"),
            call("a"),
            signed(&"x".repeat(700), Protocol::Anthropic, "sig-2"),
            call("b"),
        ]);
        assert_eq!(store.remember(&result, "key"), 3);
        let inner = store.inner.lock();
        assert!(inner.map.contains_key(&entry_key("key", "signature-only")));
        assert!(inner.map.contains_key(&entry_key("key", "a")));
        assert!(!inner.map.contains_key(&entry_key("key", "b")));
        let signature_only = &inner
            .slot(inner.map[&entry_key("key", "signature-only")])
            .unwrap()
            .entry;
        assert!(signature_only.reasoning.is_empty());
        assert!(inner.bytes <= 1000);
    }

    #[test]
    fn calls_share_reasoning_storage_but_keep_their_own_prefix() {
        let (store, _) = store(10);
        let result = response(vec![
            signed("first", Protocol::Anthropic, "sig-1"),
            call("a"),
            Part::text("between"),
            signed("second", Protocol::Anthropic, "sig-2"),
            call("b"),
        ]);
        assert_eq!(store.remember(&result, "key"), 2);
        let inner = store.inner.lock();
        let a = &inner.slot(inner.map[&entry_key("key", "a")]).unwrap().entry;
        let b = &inner.slot(inner.map[&entry_key("key", "b")]).unwrap().entry;
        assert!(Arc::ptr_eq(&a.reasoning, &b.reasoning));
        assert_eq!((a.reasoning_len, b.reasoning_len), (1, 2));
        assert_eq!((a.texts_before, b.texts_before), (0, 1));
        // A prefix still retains the complete shared allocation.
        assert_eq!(a.reasoning_cost, b.reasoning_cost);
    }

    fn store(capacity: usize) -> (ReasoningStore, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new());
        (
            ReasoningStore::with_clock(capacity, HOUR, clock.clone()),
            clock,
        )
    }

    fn signed(text: &str, origin: Protocol, sig: &str) -> Part {
        Part::Reasoning(Reasoning {
            id: None,
            text: text.into(),
            signature: Some(Signature::new(origin, sig)),
            redacted: false,
        })
    }

    fn call(id: &str) -> Part {
        Part::tool_call(id, "lookup", "{}")
    }

    fn call_signed(id: &str, origin: Protocol, sig: &str) -> Part {
        Part::ToolCall(ToolCall {
            signature: Some(Signature::new(origin, sig)),
            ..match call(id) {
                Part::ToolCall(c) => c,
                _ => unreachable!(),
            }
        })
    }

    fn response(parts: Vec<Part>) -> Response {
        let mut r = Response::new("resp_1", "upstream-model");
        r.parts = parts;
        r.finish = FinishReason::ToolCalls;
        r
    }

    /// A follow-up request: user, assistant turn, tool results.
    fn request(assistant: Vec<Part>) -> Request {
        let results: Vec<Part> = assistant
            .iter()
            .filter_map(|p| match p {
                Part::ToolCall(c) => Some(Part::tool_result_text(c.id.clone(), "ok")),
                _ => None,
            })
            .collect();
        let mut req = Request::new("m", Protocol::OpenaiChat);
        req.messages = vec![
            Message::user_text("question"),
            Message::new(Role::Assistant, assistant),
            Message::new(Role::User, results),
        ];
        req
    }

    fn assistant(req: &Request) -> &[Part] {
        &req.messages[1].parts
    }

    fn kinds(parts: &[Part]) -> Vec<String> {
        parts
            .iter()
            .map(|p| match p {
                Part::Reasoning(r) => format!("R:{}", r.text),
                Part::ToolCall(c) => format!("C:{}", c.id),
                Part::Text(t) => format!("T:{}", t.text),
                Part::ToolResult(r) => format!("TR:{}", r.call_id),
                _ => "other".to_string(),
            })
            .collect()
    }

    // ----- basic round trip ------------------------------------------------

    #[test]
    fn restores_reasoning_before_the_tool_call() {
        let (store, _) = store(16);
        let thinking = signed("let me look", Protocol::Anthropic, "sig-1");
        assert_eq!(
            store.remember(&response(vec![thinking.clone(), call("c1")]), "key"),
            1
        );

        let mut req = request(vec![call("c1")]);
        let restored = store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(
            restored,
            Restored {
                reasoning_parts: 1,
                signatures: 0
            }
        );
        assert_eq!(assistant(&req), &[thinking, call("c1")]);
    }

    #[test]
    fn restores_before_the_first_of_several_calls() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![
                signed("plan", Protocol::Anthropic, "s"),
                call("c1"),
                call("c2"),
                call("c3"),
            ]),
            "key",
        );
        assert_eq!(store.len(), 3);
        let mut req = request(vec![call("c1"), call("c2"), call("c3")]);
        let restored = store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(restored.reasoning_parts, 1);
        assert_eq!(kinds(assistant(&req)), ["R:plan", "C:c1", "C:c2", "C:c3"]);
    }

    #[test]
    fn restores_even_when_the_client_dropped_the_first_call() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![
                signed("plan", Protocol::Anthropic, "s"),
                call("c1"),
                call("c2"),
            ]),
            "key",
        );
        let mut req = request(vec![call("c2")]);
        store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(kinds(assistant(&req)), ["R:plan", "C:c2"]);
    }

    #[test]
    fn several_reasoning_parts_keep_their_order() {
        let (store, _) = store(16);
        let redacted = Part::Reasoning(Reasoning {
            id: None,
            text: String::new(),
            signature: Some(Signature::new(Protocol::Anthropic, "encrypted-payload")),
            redacted: true,
        });
        let parts = vec![
            signed("first", Protocol::Anthropic, "s1"),
            redacted.clone(),
            signed("third", Protocol::Anthropic, "s3"),
            call("c1"),
        ];
        store.remember(&response(parts.clone()), "key");
        let mut req = request(vec![call("c1")]);
        let restored = store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(restored.reasoning_parts, 3);
        assert_eq!(assistant(&req), parts.as_slice());
    }

    #[test]
    fn reasoning_item_ids_survive() {
        let (store, _) = store(16);
        let item = Part::Reasoning(Reasoning {
            id: Some("rs_123".into()),
            text: "summary".into(),
            signature: Some(Signature::new(Protocol::OpenaiResponses, "gAAAA")),
            redacted: false,
        });
        store.remember(&response(vec![item.clone(), call("call_1")]), "key");
        let mut req = request(vec![call("call_1")]);
        store.restore(&mut req, Protocol::OpenaiResponses, "key");
        assert_eq!(assistant(&req)[0], item);
    }

    #[test]
    fn restore_is_idempotent() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![signed("t", Protocol::Anthropic, "s"), call("c1")]),
            "key",
        );
        let mut req = request(vec![call("c1")]);
        store.restore(&mut req, Protocol::Anthropic, "key");
        let once = req.clone();
        let again = store.restore(&mut req, Protocol::Anthropic, "key");
        assert!(again.is_empty());
        assert_eq!(req, once);
    }

    #[test]
    fn every_assistant_turn_of_the_conversation_is_restored() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![signed("one", Protocol::Anthropic, "s1"), call("c1")]),
            "key",
        );
        store.remember(
            &response(vec![signed("two", Protocol::Anthropic, "s2"), call("c2")]),
            "key",
        );
        let mut req = Request::new("m", Protocol::OpenaiChat);
        req.messages = vec![
            Message::user_text("q"),
            Message::new(Role::Assistant, vec![call("c1")]),
            Message::new(Role::User, vec![Part::tool_result_text("c1", "r1")]),
            Message::new(Role::Assistant, vec![call("c2")]),
            Message::new(Role::User, vec![Part::tool_result_text("c2", "r2")]),
        ];
        let restored = store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(restored.reasoning_parts, 2);
        assert_eq!(kinds(&req.messages[1].parts), ["R:one", "C:c1"]);
        assert_eq!(kinds(&req.messages[3].parts), ["R:two", "C:c2"]);
        // User turns are never touched.
        assert_eq!(kinds(&req.messages[2].parts), ["TR:c1"]);
    }

    // ----- placement -------------------------------------------------------

    #[test]
    fn reasoning_goes_back_in_front_of_the_text_that_followed_it() {
        // Providers require the turn to *start* with its thinking block.
        let (store, _) = store(16);
        store.remember(
            &response(vec![
                signed("think", Protocol::Anthropic, "s"),
                Part::text("I'll check."),
                call("c1"),
                call("c2"),
            ]),
            "key",
        );
        let mut req = request(vec![Part::text("I'll check."), call("c1"), call("c2")]);
        store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(
            kinds(assistant(&req)),
            ["R:think", "T:I'll check.", "C:c1", "C:c2"]
        );
    }

    #[test]
    fn placement_clamps_when_the_client_dropped_the_text() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![
                signed("a", Protocol::Anthropic, "s1"),
                signed("b", Protocol::Anthropic, "s2"),
                Part::text("preamble"),
                call("c1"),
            ]),
            "key",
        );
        let mut req = request(vec![call("c1")]);
        store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(kinds(assistant(&req)), ["R:a", "R:b", "C:c1"]);
    }

    #[test]
    fn interleaved_reasoning_returns_to_its_own_call() {
        let (store, _) = store(16);
        let original = vec![
            signed("r1", Protocol::OpenaiResponses, "e1"),
            call("c1"),
            signed("r2", Protocol::OpenaiResponses, "e2"),
            call("c2"),
        ];
        store.remember(&response(original.clone()), "key");
        let mut req = request(vec![call("c1"), call("c2")]);
        let restored = store.restore(&mut req, Protocol::OpenaiResponses, "key");
        assert_eq!(restored.reasoning_parts, 2);
        assert_eq!(assistant(&req), original.as_slice());
    }

    #[test]
    fn reasoning_between_text_and_call_stays_there() {
        let (store, _) = store(16);
        let original = vec![
            Part::text("intro"),
            signed("r", Protocol::OpenaiResponses, "e"),
            call("c1"),
        ];
        store.remember(&response(original.clone()), "key");
        let mut req = request(vec![Part::text("intro"), call("c1")]);
        store.restore(&mut req, Protocol::OpenaiResponses, "key");
        assert_eq!(assistant(&req), original.as_slice());
    }

    #[test]
    fn insertion_point_walks_back_over_non_reasoning_parts_only() {
        let parts = vec![
            Part::reasoning("x"),
            Part::text("a"),
            Part::reasoning("y"),
            Part::text("b"),
            call("c"),
        ];
        let no_calls = |_: &ToolCall| false;
        assert_eq!(insertion_point(&parts, 4, 0, no_calls), 4);
        assert_eq!(insertion_point(&parts, 4, 1, no_calls), 3);
        assert_eq!(insertion_point(&parts, 4, 2, no_calls), 1);
        assert_eq!(insertion_point(&parts, 4, 9, no_calls), 1);
        assert_eq!(insertion_point(&parts, 0, 3, no_calls), 0);
        // Out-of-range positions are clamped, never a panic.
        assert_eq!(insertion_point(&parts, 99, 0, no_calls), 5);
        assert_eq!(insertion_point(&[], 0, 2, no_calls), 0);
    }

    #[test]
    fn insertion_point_passes_only_the_calls_that_followed_the_reasoning() {
        let parts = vec![
            call("a"),
            Part::text("t1"),
            call("b"),
            Part::text("t2"),
            call("c"),
        ];
        let only = |ids: &'static [&'static str]| move |c: &ToolCall| ids.contains(&c.id.as_str());
        // No call may be passed: the walk ends at `b`, whatever the budget.
        assert_eq!(insertion_point(&parts, 4, 0, only(&[])), 4);
        assert_eq!(insertion_point(&parts, 4, 1, only(&[])), 3);
        assert_eq!(insertion_point(&parts, 4, 5, only(&[])), 3);
        // `b` followed the reasoning: it is passed, `a` still is not.
        assert_eq!(insertion_point(&parts, 4, 1, only(&["b"])), 2);
        assert_eq!(insertion_point(&parts, 4, 2, only(&["b"])), 1);
        // A call that followed the reasoning is passed even when the text
        // budget is used up: the reasoning must stay in front of it.
        assert_eq!(insertion_point(&parts, 4, 0, only(&["b"])), 2);
        assert_eq!(insertion_point(&parts, 4, 0, only(&["a", "b"])), 0);
        // Text beyond the budget is not passed on its own.
        assert_eq!(insertion_point(&parts, 2, 0, only(&[])), 2);
        assert_eq!(insertion_point(&parts, 2, 0, only(&["a"])), 0);
    }

    // ----- order under client rearrangement (R-S1) -------------------------------

    #[test]
    fn restored_reasoning_stays_behind_the_call_it_followed() {
        // Responses output `reasoning, call, reasoning, message, message,
        // call` as a Chat client sends it back: one text, in front.
        let (store, _) = store(16);
        store.remember(
            &response(vec![
                signed("first", Protocol::OpenaiResponses, "e1"),
                call("c1"),
                signed("second", Protocol::OpenaiResponses, "e2"),
                Part::text("Checking "),
                Part::text("one more thing."),
                call("c2"),
            ]),
            "key",
        );
        let mut req = request(vec![
            Part::text("Checking one more thing."),
            call("c1"),
            call("c2"),
        ]);
        let restored = store.restore(&mut req, Protocol::OpenaiResponses, "key");
        assert_eq!(restored.reasoning_parts, 2);
        assert_eq!(
            kinds(assistant(&req)),
            [
                "T:Checking one more thing.",
                "R:first",
                "C:c1",
                "R:second",
                "C:c2"
            ]
        );
    }

    #[test]
    fn restored_reasoning_keeps_its_order_when_calls_were_dropped() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![
                call("c0"),
                signed("r1", Protocol::Anthropic, "s1"),
                call("c2"),
                signed("r3", Protocol::Anthropic, "s3"),
                call("c4"),
                call("c5"),
                call("c6"),
            ]),
            "key",
        );
        let mut req = request(vec![call("c0"), call("c2"), call("c6")]);
        store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(
            kinds(assistant(&req)),
            ["C:c0", "R:r1", "C:c2", "R:r3", "C:c6"]
        );
    }

    #[test]
    fn reasoning_restored_through_a_later_call_still_precedes_the_earlier_ones() {
        // The entry of `c1` is gone (evicted, expired, forgotten): the plan
        // comes back through `c2` and must still open the turn.
        let (store, _) = store(16);
        let original = vec![
            signed("plan", Protocol::Anthropic, "s"),
            Part::text("Two lookups."),
            call("c1"),
            call("c2"),
        ];
        store.remember(&response(original.clone()), "key");
        assert!(store.forget_call("key", "c1"));
        let mut req = request(vec![Part::text("Two lookups."), call("c1"), call("c2")]);
        store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(assistant(&req), original.as_slice());

        // Even when the client moved the text behind the first call.
        let mut req = request(vec![call("c1"), Part::text("Two lookups."), call("c2")]);
        store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(
            kinds(assistant(&req)),
            ["R:plan", "C:c1", "T:Two lookups.", "C:c2"]
        );
    }

    #[test]
    fn reasoning_does_not_jump_over_a_call_from_another_response() {
        // A client that merged two assistant turns into one message.
        let (store, _) = store(16);
        store.remember(
            &response(vec![signed("one", Protocol::Anthropic, "s1"), call("c1")]),
            "key",
        );
        store.remember(
            &response(vec![
                signed("two", Protocol::Anthropic, "s2"),
                Part::text("next"),
                call("c2"),
            ]),
            "key",
        );
        let mut req = request(vec![call("c1"), call("c2")]);
        store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(kinds(assistant(&req)), ["R:one", "C:c1", "R:two", "C:c2"]);
    }

    #[test]
    fn extra_client_text_does_not_push_reasoning_behind_a_call_it_preceded() {
        // The entry of `c1` is gone and the client split the text in two:
        // the text budget runs out before `c1` is reached, and the reasoning
        // still has to end up in front of it.
        let (store, _) = store(16);
        store.remember(
            &response(vec![
                signed("plan", Protocol::Anthropic, "s"),
                call("c1"),
                Part::text("and then"),
                call("c2"),
            ]),
            "key",
        );
        store.forget_call("key", "c1");
        let mut req = request(vec![
            call("c1"),
            Part::text("and"),
            Part::text("then"),
            call("c2"),
        ]);
        store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(
            kinds(assistant(&req)),
            ["R:plan", "C:c1", "T:and", "T:then", "C:c2"]
        );
    }

    // ----- byte budget -------------------------------------------------------------

    fn remember_blob(store: &ReasoningStore, id: &str, bytes: usize) {
        store.remember(
            &response(vec![
                signed("", Protocol::Anthropic, &"x".repeat(bytes)),
                call(id),
            ]),
            "key",
        );
    }

    #[test]
    fn byte_budget_evicts_the_least_recently_used_entries() {
        let clock = Arc::new(ManualClock::new());
        let store = ReasoningStore::with_clock(100, HOUR, clock).with_max_bytes(10_000);
        for id in ["a", "b", "c"] {
            remember_blob(&store, id, 3_000);
        }
        assert_eq!(store.len(), 3);
        assert!(store.approx_bytes() >= 9_000 && store.approx_bytes() <= 10_000);
        // Using `a` makes `b` the oldest; the next blob does not fit with it.
        assert!(known(&store, "a"));
        remember_blob(&store, "d", 3_000);
        assert_eq!(store.len(), 3);
        assert!(!known(&store, "b"));
        assert!(known(&store, "a") && known(&store, "c") && known(&store, "d"));
        assert!(store.approx_bytes() <= 10_000);
    }

    #[test]
    fn a_blob_larger_than_the_whole_budget_is_not_kept() {
        let store = ReasoningStore::new(100, HOUR).with_max_bytes(2_000);
        remember_blob(&store, "small", 500);
        remember_blob(&store, "huge", 5_000);
        // The oversized entry is refused without throwing the others out.
        assert!(!known(&store, "huge"));
        assert!(known(&store, "small"));
        // An oversized replacement removes the stale entry it would replace.
        remember_blob(&store, "small", 5_000);
        assert!(!known(&store, "small"));
        assert!(store.is_empty());
        assert_eq!(store.approx_bytes(), 0);
    }

    #[test]
    fn byte_accounting_follows_replacement_removal_and_expiry() {
        let clock = Arc::new(ManualClock::new());
        let store = ReasoningStore::with_clock(100, HOUR, clock.clone()).with_max_bytes(1 << 20);
        assert_eq!(store.approx_bytes(), 0);
        remember_blob(&store, "a", 1_000);
        let one = store.approx_bytes();
        assert!((1_000..1_500).contains(&one), "{one}");
        // Replacing an entry replaces its cost.
        remember_blob(&store, "a", 4_000);
        assert_eq!(store.approx_bytes(), one + 3_000);
        remember_blob(&store, "b", 1_000);
        assert_eq!(store.approx_bytes(), 2 * one + 3_000);
        assert!(store.forget_call("key", "a"));
        assert_eq!(store.approx_bytes(), one);
        clock.advance(HOUR);
        assert_eq!(store.approx_bytes(), 0);
        remember_blob(&store, "c", 1_000);
        store.clear();
        assert_eq!(store.approx_bytes(), 0);
    }

    #[test]
    fn without_a_byte_budget_only_the_entry_count_limits() {
        let store = ReasoningStore::new(2, HOUR);
        remember_blob(&store, "a", 1 << 20);
        remember_blob(&store, "b", 1 << 20);
        assert_eq!(store.len(), 2);
        assert!(store.approx_bytes() >= 2 << 20);
    }

    #[test]
    fn zero_byte_budget_disables_the_store() {
        let store = ReasoningStore::new(8, HOUR).with_max_bytes(0);
        remember_one(&store, "a");
        assert!(store.is_empty());
        assert!(!known(&store, "a"));
    }

    // ----- when not to restore ---------------------------------------------

    #[test]
    fn a_turn_that_kept_its_reasoning_is_left_alone() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![signed("stored", Protocol::Anthropic, "s"), call("c1")]),
            "key",
        );
        let kept = vec![signed("client copy", Protocol::Anthropic, "s"), call("c1")];
        let mut req = request(kept.clone());
        let restored = store.restore(&mut req, Protocol::Anthropic, "key");
        assert!(restored.is_empty());
        assert_eq!(assistant(&req), kept.as_slice());
    }

    #[test]
    fn reasoning_after_the_call_does_not_count_as_preceding_it() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![signed("r", Protocol::Anthropic, "s"), call("c1")]),
            "key",
        );
        let mut req = request(vec![
            call("c1"),
            signed("later", Protocol::Anthropic, "other"),
        ]);
        store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(kinds(assistant(&req)), ["R:r", "C:c1", "R:later"]);
    }

    #[test]
    fn unsigned_client_reasoning_does_not_block_and_is_superseded() {
        // A Chat client echoing `reasoning_content`: text, no signature.
        let (store, _) = store(16);
        let original = signed("the real thought", Protocol::Anthropic, "s");
        store.remember(&response(vec![original.clone(), call("c1")]), "key");
        let mut req = request(vec![Part::reasoning("the real thought"), call("c1")]);
        let restored = store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(restored.reasoning_parts, 1);
        assert_eq!(assistant(&req), &[original, call("c1")]);
    }

    #[test]
    fn unsigned_client_reasoning_with_other_text_is_kept() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![signed("stored", Protocol::Anthropic, "s"), call("c1")]),
            "key",
        );
        let mut req = request(vec![Part::reasoning("something else"), call("c1")]);
        store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(
            kinds(assistant(&req)),
            ["R:something else", "R:stored", "C:c1"]
        );
    }

    #[test]
    fn foreign_signed_client_reasoning_does_not_block() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![signed("mine", Protocol::Anthropic, "s"), call("c1")]),
            "key",
        );
        let mut req = request(vec![signed("theirs", Protocol::Gemini, "g"), call("c1")]);
        let restored = store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(restored.reasoning_parts, 1);
        assert_eq!(kinds(assistant(&req)), ["R:theirs", "R:mine", "C:c1"]);
    }

    #[test]
    fn unknown_call_ids_are_left_alone() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![signed("r", Protocol::Anthropic, "s"), call("c1")]),
            "key",
        );
        let mut req = request(vec![call("other")]);
        let before = req.clone();
        assert!(
            store
                .restore(&mut req, Protocol::Anthropic, "key")
                .is_empty()
        );
        assert_eq!(req, before);
    }

    #[test]
    fn tool_calls_outside_assistant_turns_are_ignored() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![signed("r", Protocol::Anthropic, "s"), call("c1")]),
            "key",
        );
        let mut req = Request::new("m", Protocol::OpenaiChat);
        req.messages = vec![
            Message::new(Role::User, vec![call("c1")]),
            Message::new(Role::System, vec![call("c1")]),
        ];
        let before = req.clone();
        assert!(
            store
                .restore(&mut req, Protocol::Anthropic, "key")
                .is_empty()
        );
        assert_eq!(req, before);
    }

    #[test]
    fn request_without_tool_calls_is_untouched() {
        let (store, _) = store(16);
        let mut req = Request::new("m", Protocol::OpenaiChat);
        req.messages = vec![Message::user_text("hi"), Message::assistant_text("hello")];
        let before = req.clone();
        assert!(
            store
                .restore(&mut req, Protocol::Anthropic, "key")
                .is_empty()
        );
        assert_eq!(req, before);
    }

    // ----- what is stored --------------------------------------------------

    #[test]
    fn unsigned_reasoning_is_not_stored() {
        let (store, _) = store(16);
        assert_eq!(
            store.remember(&response(vec![Part::reasoning("plain"), call("c1")]), "key"),
            0
        );
        assert!(store.is_empty());
    }

    #[test]
    fn responses_without_tool_calls_store_nothing() {
        let (store, _) = store(16);
        assert_eq!(
            store.remember(
                &response(vec![
                    signed("r", Protocol::Anthropic, "s"),
                    Part::text("answer")
                ]),
                "key"
            ),
            0
        );
        assert!(store.is_empty());
    }

    #[test]
    fn calls_without_an_id_are_skipped() {
        let (store, _) = store(16);
        assert_eq!(
            store.remember(
                &response(vec![signed("r", Protocol::Anthropic, "s"), call("")]),
                "key"
            ),
            0
        );
    }

    #[test]
    fn reasoning_after_a_call_is_not_attributed_to_it() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![call("c1"), signed("late", Protocol::Anthropic, "s")]),
            "key",
        );
        assert!(store.is_empty());
    }

    // ----- vendor families -------------------------------------------------

    #[test]
    fn reasoning_is_not_replayed_to_another_vendor() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![signed("claude", Protocol::Anthropic, "s"), call("c1")]),
            "key",
        );
        for target in [
            Protocol::Gemini,
            Protocol::OpenaiChat,
            Protocol::OpenaiResponses,
        ] {
            let mut req = request(vec![call("c1")]);
            let before = req.clone();
            assert!(
                store.restore(&mut req, target, "key").is_empty(),
                "{target}"
            );
            assert_eq!(req, before);
        }
    }

    #[test]
    fn same_family_protocols_share_blobs() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![
                signed("openai", Protocol::OpenaiResponses, "enc"),
                call("c1"),
            ]),
            "key",
        );
        let mut req = request(vec![call("c1")]);
        let restored = store.restore(&mut req, Protocol::OpenaiChat, "key");
        assert_eq!(restored.reasoning_parts, 1);
    }

    #[test]
    fn mixed_origins_restore_only_the_matching_ones() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![
                signed("a", Protocol::Anthropic, "sa"),
                signed("g", Protocol::Gemini, "sg"),
                call("c1"),
            ]),
            "key",
        );
        let mut req = request(vec![call("c1")]);
        store.restore(&mut req, Protocol::Gemini, "key");
        assert_eq!(kinds(assistant(&req)), ["R:g", "C:c1"]);
    }

    // ----- tool-call signatures --------------------------------------------

    #[test]
    fn restores_a_missing_tool_call_signature() {
        let (store, _) = store(16);
        let original = call_signed("c1", Protocol::Gemini, "thought-sig");
        assert_eq!(store.remember(&response(vec![original.clone()]), "key"), 1);
        let mut req = request(vec![call("c1")]);
        let restored = store.restore(&mut req, Protocol::Gemini, "key");
        assert_eq!(
            restored,
            Restored {
                reasoning_parts: 0,
                signatures: 1
            }
        );
        assert_eq!(assistant(&req), &[original]);
    }

    #[test]
    fn tool_call_signature_is_not_replayed_to_another_vendor() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![call_signed("c1", Protocol::Gemini, "sig")]),
            "key",
        );
        let mut req = request(vec![call("c1")]);
        let before = req.clone();
        assert!(
            store
                .restore(&mut req, Protocol::Anthropic, "key")
                .is_empty()
        );
        assert_eq!(req, before);
    }

    #[test]
    fn a_valid_client_signature_is_kept() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![call_signed("c1", Protocol::Gemini, "stored")]),
            "key",
        );
        let mut req = request(vec![call_signed("c1", Protocol::Gemini, "from-client")]);
        let before = req.clone();
        assert!(store.restore(&mut req, Protocol::Gemini, "key").is_empty());
        assert_eq!(req, before);
    }

    #[test]
    fn a_foreign_client_signature_is_replaced() {
        let (store, _) = store(16);
        let original = call_signed("c1", Protocol::Gemini, "stored");
        store.remember(&response(vec![original.clone()]), "key");
        let mut req = request(vec![call_signed("c1", Protocol::Anthropic, "wrong")]);
        let restored = store.restore(&mut req, Protocol::Gemini, "key");
        assert_eq!(restored.signatures, 1);
        assert_eq!(assistant(&req), &[original]);
    }

    #[test]
    fn only_the_signed_call_of_a_parallel_set_gets_a_signature() {
        // Gemini signs the first function call of a turn only.
        let (store, _) = store(16);
        store.remember(
            &response(vec![call_signed("c1", Protocol::Gemini, "sig"), call("c2")]),
            "key",
        );
        assert_eq!(store.len(), 1);
        let mut req = request(vec![call("c1"), call("c2")]);
        let restored = store.restore(&mut req, Protocol::Gemini, "key");
        assert_eq!(restored.signatures, 1);
        assert_eq!(
            assistant(&req),
            &[call_signed("c1", Protocol::Gemini, "sig"), call("c2")]
        );
    }

    #[test]
    fn reasoning_and_signature_are_restored_together() {
        let (store, _) = store(16);
        let original = vec![
            signed("thought", Protocol::Gemini, "ts"),
            call_signed("c1", Protocol::Gemini, "cs"),
        ];
        store.remember(&response(original.clone()), "key");
        let mut req = request(vec![call("c1")]);
        let restored = store.restore(&mut req, Protocol::Gemini, "key");
        assert_eq!(
            restored,
            Restored {
                reasoning_parts: 1,
                signatures: 1
            }
        );
        assert_eq!(assistant(&req), original.as_slice());
    }

    #[test]
    fn signature_is_restored_even_when_reasoning_is_already_present() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![
                signed("thought", Protocol::Gemini, "ts"),
                call_signed("c1", Protocol::Gemini, "cs"),
            ]),
            "key",
        );
        let mut req = request(vec![signed("thought", Protocol::Gemini, "ts"), call("c1")]);
        let restored = store.restore(&mut req, Protocol::Gemini, "key");
        assert_eq!(
            restored,
            Restored {
                reasoning_parts: 0,
                signatures: 1
            }
        );
    }

    // ----- scope -----------------------------------------------------------

    #[test]
    fn scopes_are_isolated() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![signed("secret", Protocol::Anthropic, "s"), call("c1")]),
            "client-a",
        );
        let mut req = request(vec![call("c1")]);
        assert!(
            store
                .restore(&mut req, Protocol::Anthropic, "client-b")
                .is_empty()
        );
        assert!(store.restore(&mut req, Protocol::Anthropic, "").is_empty());
        assert_eq!(
            store
                .restore(&mut req, Protocol::Anthropic, "client-a")
                .reasoning_parts,
            1
        );
    }

    #[test]
    fn scope_and_id_cannot_be_confused() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![signed("r", Protocol::Anthropic, "s"), call("bc")]),
            "a",
        );
        let mut req = request(vec![call("c")]);
        assert!(
            store
                .restore(&mut req, Protocol::Anthropic, "ab")
                .is_empty()
        );
        assert_ne!(entry_key("a", "bc"), entry_key("ab", "c"));
        assert_ne!(entry_key("1", "1:x"), entry_key("11", ":x"));
    }

    #[test]
    fn same_call_id_in_two_scopes_keeps_two_entries() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![signed("for a", Protocol::Anthropic, "sa"), call("c1")]),
            "a",
        );
        store.remember(
            &response(vec![signed("for b", Protocol::Anthropic, "sb"), call("c1")]),
            "b",
        );
        assert_eq!(store.len(), 2);
        let mut req = request(vec![call("c1")]);
        store.restore(&mut req, Protocol::Anthropic, "b");
        assert_eq!(kinds(assistant(&req)), ["R:for b", "C:c1"]);
    }

    // ----- capacity --------------------------------------------------------

    fn remember_one(store: &ReasoningStore, id: &str) {
        store.remember(
            &response(vec![signed(id, Protocol::Anthropic, id), call(id)]),
            "key",
        );
    }

    fn known(store: &ReasoningStore, id: &str) -> bool {
        let mut req = request(vec![call(id)]);
        !store
            .restore(&mut req, Protocol::Anthropic, "key")
            .is_empty()
    }

    #[test]
    fn capacity_evicts_the_oldest_entry() {
        let (store, _) = store(3);
        for id in ["a", "b", "c", "d"] {
            remember_one(&store, id);
        }
        assert_eq!(store.len(), 3);
        assert!(!known(&store, "a"));
        assert!(known(&store, "b") && known(&store, "c") && known(&store, "d"));
    }

    #[test]
    fn using_an_entry_protects_it_from_eviction() {
        let (store, _) = store(3);
        for id in ["a", "b", "c"] {
            remember_one(&store, id);
        }
        assert!(known(&store, "a"));
        remember_one(&store, "d");
        assert!(known(&store, "a"));
        assert!(!known(&store, "b"));
        assert!(known(&store, "c") && known(&store, "d"));
    }

    #[test]
    fn remembering_again_replaces_and_refreshes() {
        let (store, _) = store(2);
        remember_one(&store, "a");
        remember_one(&store, "b");
        store.remember(
            &response(vec![signed("newer", Protocol::Anthropic, "s2"), call("a")]),
            "key",
        );
        assert_eq!(store.len(), 2);
        remember_one(&store, "c");
        assert!(!known(&store, "b"));
        let mut req = request(vec![call("a")]);
        store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(kinds(assistant(&req)), ["R:newer", "C:a"]);
    }

    #[test]
    fn capacity_one() {
        let (store, _) = store(1);
        remember_one(&store, "a");
        remember_one(&store, "b");
        assert_eq!(store.len(), 1);
        assert!(!known(&store, "a"));
        assert!(known(&store, "b"));
    }

    #[test]
    fn a_response_larger_than_the_capacity_keeps_its_last_calls() {
        let (store, _) = store(2);
        store.remember(
            &response(vec![
                signed("r", Protocol::Anthropic, "s"),
                call("a"),
                call("b"),
                call("c"),
            ]),
            "key",
        );
        assert_eq!(store.len(), 2);
        assert!(!known(&store, "a"));
        assert!(known(&store, "c"));
    }

    #[test]
    fn slots_are_reused_after_eviction() {
        let (store, _) = store(4);
        for round in 0..50 {
            for n in 0..4 {
                remember_one(&store, &format!("id-{round}-{n}"));
            }
        }
        assert_eq!(store.len(), 4);
        let inner = store.inner.lock();
        assert!(inner.slots.len() <= 5, "slab grew to {}", inner.slots.len());
    }

    #[test]
    fn zero_capacity_disables_the_store() {
        let (store, _) = store(0);
        assert_eq!(
            store.remember(
                &response(vec![signed("r", Protocol::Anthropic, "s"), call("c1")]),
                "key"
            ),
            0
        );
        assert!(store.is_empty());
        assert!(!known(&store, "c1"));
    }

    #[test]
    fn zero_ttl_disables_the_store() {
        let store = ReasoningStore::new(8, Duration::ZERO);
        remember_one(&store, "a");
        assert!(store.is_empty());
        assert!(!known(&store, "a"));
    }

    // ----- expiry ----------------------------------------------------------

    #[test]
    fn entries_expire_after_the_ttl() {
        let (store, clock) = store(16);
        remember_one(&store, "a");
        clock.advance(HOUR - Duration::from_secs(1));
        assert_eq!(store.len(), 1);
        clock.advance(Duration::from_secs(1));
        assert_eq!(store.len(), 0);
        assert!(!known(&store, "a"));
    }

    #[test]
    fn an_expired_entry_is_not_restored_even_before_it_is_purged() {
        let (store, clock) = store(16);
        remember_one(&store, "a");
        clock.advance(HOUR * 2);
        // No len() call in between: expiry is enforced on the lookup itself.
        assert!(!known(&store, "a"));
        assert!(store.inner.lock().map.is_empty());
    }

    #[test]
    fn using_an_entry_extends_its_life() {
        let (store, clock) = store(16);
        remember_one(&store, "a");
        clock.advance(Duration::from_secs(3000));
        assert!(known(&store, "a"));
        clock.advance(Duration::from_secs(3000));
        assert!(known(&store, "a"));
        clock.advance(HOUR);
        assert!(!known(&store, "a"));
    }

    #[test]
    fn only_the_stale_entries_expire() {
        let (store, clock) = store(16);
        remember_one(&store, "old");
        clock.advance(Duration::from_secs(2400));
        remember_one(&store, "new");
        clock.advance(Duration::from_secs(1800));
        assert_eq!(store.len(), 1);
        assert!(!known(&store, "old"));
        assert!(known(&store, "new"));
    }

    #[test]
    fn remembering_purges_expired_entries() {
        let (store, clock) = store(16);
        remember_one(&store, "a");
        remember_one(&store, "b");
        clock.advance(HOUR);
        remember_one(&store, "c");
        assert_eq!(store.inner.lock().map.len(), 1);
    }

    // ----- streams ---------------------------------------------------------

    #[test]
    fn remember_events_accumulates_a_stream() {
        let (store, _) = store(16);
        let original = vec![
            signed("streamed thought", Protocol::Anthropic, "sig"),
            Part::tool_call("c1", "lookup", "{\"q\":1}"),
        ];
        let events = response_to_events(&response(original.clone()));
        assert_eq!(store.remember_events(&events, "key"), 1);
        let mut req = request(vec![Part::tool_call("c1", "lookup", "{\"q\":1}")]);
        store.restore(&mut req, Protocol::Anthropic, "key");
        assert_eq!(assistant(&req), original.as_slice());
    }

    #[test]
    fn remember_accumulator_includes_the_open_block() {
        let (store, _) = store(16);
        let events = response_to_events(&response(vec![
            signed("t", Protocol::Anthropic, "sig"),
            call("c1"),
        ]));
        let mut acc = Accumulator::new();
        // Stop before the tool call block is closed: a truncated stream.
        for event in &events[..events.len() - 2] {
            acc.push(event);
        }
        assert_eq!(store.remember_accumulator(&acc, "key"), 1);
        assert!(known(&store, "c1"));
    }

    #[test]
    fn remember_events_of_an_empty_stream() {
        let (store, _) = store(16);
        assert_eq!(store.remember_events(&[], "key"), 0);
    }

    // ----- forgetting ------------------------------------------------------

    #[test]
    fn forget_drops_the_calls_of_a_request() {
        let (store, _) = store(16);
        for id in ["a", "b", "c"] {
            remember_one(&store, id);
        }
        let req = request(vec![call("a"), call("b"), call("unknown")]);
        assert_eq!(store.forget(&req, "other-scope"), 0);
        assert_eq!(store.forget(&req, "key"), 2);
        assert_eq!(store.len(), 1);
        assert!(known(&store, "c"));
    }

    #[test]
    fn forget_call_and_clear() {
        let (store, _) = store(16);
        remember_one(&store, "a");
        remember_one(&store, "b");
        assert!(store.forget_call("key", "a"));
        assert!(!store.forget_call("key", "a"));
        assert_eq!(store.len(), 1);
        store.clear();
        assert!(store.is_empty());
        remember_one(&store, "c");
        assert!(known(&store, "c"));
    }

    // ----- misc ------------------------------------------------------------

    #[test]
    fn debug_output_does_not_leak_blobs() {
        let (store, _) = store(16);
        store.remember(
            &response(vec![
                signed("private thought", Protocol::Anthropic, "SECRET-SIG"),
                call("c1"),
            ]),
            "key",
        );
        let debug = format!("{store:?}");
        assert!(!debug.contains("SECRET-SIG") && !debug.contains("private thought"));
    }

    #[test]
    fn system_clock_store_works() {
        let store = ReasoningStore::new(8, HOUR);
        remember_one(&store, "a");
        assert!(known(&store, "a"));
    }

    #[test]
    fn concurrent_use_is_safe_and_stays_bounded() {
        let store = Arc::new(ReasoningStore::new(64, HOUR));
        let handles: Vec<_> = (0..8)
            .map(|thread| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    for n in 0..200 {
                        let id = format!("t{thread}-{n}");
                        store.remember(
                            &response(vec![signed("r", Protocol::Anthropic, &id), call(&id)]),
                            "key",
                        );
                        let mut req = request(vec![call(&id)]);
                        store.restore(&mut req, Protocol::Anthropic, "key");
                        if n % 7 == 0 {
                            store.forget(&req, "key");
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("worker thread");
        }
        assert!(store.len() <= 64);
        let inner = store.inner.lock();
        // The recency list and the map agree.
        let mut walked = 0;
        let mut cursor = inner.head;
        while let Some(slot) = inner.slot(cursor) {
            walked += 1;
            cursor = slot.next;
        }
        assert_eq!(walked, inner.map.len());
    }

    #[test]
    fn store_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ReasoningStore>();
    }
}
