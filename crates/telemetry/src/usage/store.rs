//! The usage store: recent request records, time buckets and latency
//! histograms, with optional JSONL persistence.
//!
//! A record is counted in the bucket of the moment it **finished**, so new
//! data always lands in the current bucket and a chart never rewrites its
//! past. (The JSONL file and the request list go by the start time.)
//!
//! Resolution: one bucket per minute for the last 24 hours and one per hour
//! for the retention window. `1h` and `24h` queries read the minute buckets,
//! `7d` and `30d` the hour buckets. A range of N source buckets ends with the
//! current, still filling one, so a summary and a time series over the same
//! range always add up to the same totals.
//!
//! What a query returns depends only on the time passed to it, never on the
//! newest timestamp the store happens to have seen: a record stamped in the
//! future (a clock that was ahead, a bogus line in a usage file) occupies
//! its own bucket and does not push real traffic out of any window. Memory
//! is bounded by the number of buckets kept, and [`UsageStore::prune`] drops
//! what has slid out of every window.

use super::buckets::{Bucket, OTHER, SecondSlot, merge_groups};
use super::latency::LatencySlot;
use super::persist::{LoadReport, Persister, find_in_day, first_kept_day, load_dir};
use super::types::{
    BucketSize, GroupBy, Latency, NamedTotals, Range, RequestPage, RequestQuery, StatsTick,
    StatusFilter, TimePoint, Timeseries, Totals, UsageSummary,
};
use crate::gauges::Gauges;
use crate::record::{RequestRecord, request_id_time_ms};
use crate::time::{DAY_MS, HOUR_MS, MINUTE_MS, SECOND_MS, utc_day};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

/// Request records kept in memory for the request list.
pub const DEFAULT_RECENT_CAPACITY: usize = 2_000;

/// How often the background writer appends queued records to disk.
const FLUSH_INTERVAL: Duration = Duration::from_millis(1_000);

const SECONDS_KEPT: i64 = 120;
const MINUTES_KEPT: i64 = 24 * 60;
const LATENCY_MINUTES_KEPT: i64 = 60;
const LATENCY_HOURS_KEPT: i64 = 24;

/// Buckets held per window length. Twice the window, so buckets stamped in
/// the future (which are never the oldest, hence never the ones dropped)
/// cannot crowd the real ones out of a window.
const BUCKET_SLACK: usize = 2;

/// Most buckets a map covering `kept` time units may hold.
fn bucket_cap(kept: i64) -> usize {
    usize::try_from(kept)
        .unwrap_or(0)
        .saturating_mul(BUCKET_SLACK)
        .max(1)
}

/// The bucket `key` of a map that keeps its `cap` newest buckets, created
/// if need be. `None` when the map is full of newer buckets: the record is
/// older than everything the map can still answer for.
fn bucket_mut<V: Default>(map: &mut BTreeMap<i64, V>, key: i64, cap: usize) -> Option<&mut V> {
    if !map.contains_key(&key) && map.len() >= cap {
        if map
            .first_key_value()
            .is_some_and(|(oldest, _)| key < *oldest)
        {
            return None;
        }
        map.pop_first();
    }
    Some(map.entry(key).or_default())
}

/// Series returned by a grouped time series; the rest is folded into
/// [`OTHER`] so a chart stays readable and the payload bounded.
const MAX_SERIES: usize = 20;

/// Records on disk dated further than this past "now" are ignored when
/// loading: a line with a bogus timestamp is not history. (A record inside
/// the tolerance is kept; it sits in its own future bucket and shows up in
/// the statistics once time gets there.)
const FUTURE_TOLERANCE_MS: i64 = DAY_MS;

/// Construction parameters of a [`UsageStore`].
#[derive(Clone, Debug)]
pub struct UsageStoreOptions {
    /// Size of the recent-requests ring.
    pub recent_capacity: usize,
    /// Days of hourly statistics (and usage files) to keep; at least one.
    pub retention_days: u32,
    /// When false, [`UsageStore::record`] does nothing.
    pub enabled: bool,
    /// Directory of the usage files (normally `<data_dir>/usage`); `None`
    /// keeps everything in memory only.
    pub persist_dir: Option<PathBuf>,
    /// Live gauges reported by [`UsageStore::stats_tick`].
    pub gauges: Option<Gauges>,
}

impl Default for UsageStoreOptions {
    fn default() -> Self {
        UsageStoreOptions {
            recent_capacity: DEFAULT_RECENT_CAPACITY,
            retention_days: 30,
            enabled: true,
            persist_dir: None,
            gauges: None,
        }
    }
}

/// Sizes of the store's parts, for diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageStoreInfo {
    /// Records in the recent-requests ring.
    pub recent: usize,
    /// Per-minute buckets held in memory. Buckets older than 24 hours stay
    /// (unread) until the next [`UsageStore::prune`] or until the cap of
    /// twice the window pushes them out.
    pub minute_buckets: usize,
    /// Per-hour buckets held in memory; see `minute_buckets`.
    pub hour_buckets: usize,
    /// Records waiting to be written to disk.
    pub pending_writes: usize,
    /// Records that were not persisted because the write queue was full.
    pub dropped_writes: u64,
}

/// Which bucket map a query reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Minutes,
    Hours,
}

/// The source buckets a range covers: `first..=last`, each `src_ms` wide.
#[derive(Clone, Copy, Debug)]
struct Window {
    source: Source,
    src_ms: i64,
    first: i64,
    last: i64,
    /// First bucket that may hold data: `first`, or later when the range
    /// reaches back beyond the retention window. Reading from here makes
    /// retention hold at query time, whether or not the buckets past it
    /// have been dropped yet.
    data_first: i64,
}

fn window(range: Range, now: i64, hours_kept: i64) -> Window {
    let (source, src_ms, count) = match range {
        Range::Hour => (Source::Minutes, MINUTE_MS, 60),
        Range::Day => (Source::Minutes, MINUTE_MS, 24 * 60),
        Range::Week => (Source::Hours, HOUR_MS, 7 * 24),
        Range::Month => (Source::Hours, HOUR_MS, 30 * 24),
    };
    let last = now.div_euclid(src_ms);
    let first = last - count + 1;
    Window {
        source,
        src_ms,
        first,
        last,
        data_first: match source {
            Source::Minutes => first,
            Source::Hours => first.max(last - hours_kept + 1),
        },
    }
}

/// The moment a record is counted at: when it finished. Records restored
/// from an older file format without `finished_at` fall back to the start.
fn event_time(record: &RequestRecord) -> i64 {
    record.finished_at.max(record.started_at)
}

fn drop_through<V>(map: &mut BTreeMap<i64, V>, threshold: i64) {
    while let Some(entry) = map.first_entry() {
        if *entry.key() > threshold {
            break;
        }
        entry.remove();
    }
}

type RecentKey = (i64, String);

struct State {
    capacity: usize,
    /// Recent records ordered by `(started_at, id)`, the order of the
    /// request list.
    recent: BTreeMap<RecentKey, Arc<RequestRecord>>,
    /// id → `started_at`, to find a record's key in `recent`.
    index: HashMap<String, i64>,
    /// The same records in the order they were recorded, oldest first: the
    /// order the ring evicts in. A request is recorded when it finishes, so
    /// a long-running one may have started before everything else in the
    /// ring; evicting by start time would drop it the moment it arrives.
    arrivals: VecDeque<Arc<RequestRecord>>,
    seconds: BTreeMap<i64, SecondSlot>,
    minutes: BTreeMap<i64, Bucket>,
    hours: BTreeMap<i64, Bucket>,
    latency_minutes: BTreeMap<i64, LatencySlot>,
    latency_hours: BTreeMap<i64, LatencySlot>,
}

impl State {
    fn new(capacity: usize) -> Self {
        State {
            capacity,
            recent: BTreeMap::new(),
            index: HashMap::new(),
            arrivals: VecDeque::new(),
            seconds: BTreeMap::new(),
            minutes: BTreeMap::new(),
            hours: BTreeMap::new(),
            latency_minutes: BTreeMap::new(),
            latency_hours: BTreeMap::new(),
        }
    }

    /// Counts a record in every aggregate.
    ///
    /// `reference` is the current time when the caller knows it (loading
    /// history): buckets that have already slid out of their window are then
    /// not created at all. Live recording passes `None` and every record
    /// gets its buckets; which of them a query reads is decided by the time
    /// given to the query. Deliberately nothing here is relative to the
    /// newest timestamp seen so far, so one record stamped in the future
    /// cannot make later, correctly stamped records look too old to count.
    fn apply(&mut self, record: &RequestRecord, hours_kept: i64, reference: Option<i64>) {
        let at = event_time(record);
        let in_window = |index: i64, width: i64, kept: i64| {
            reference.is_none_or(|now| index > now.div_euclid(width) - kept)
        };

        let second = at.div_euclid(SECOND_MS);
        if in_window(second, SECOND_MS, SECONDS_KEPT)
            && let Some(slot) = bucket_mut(&mut self.seconds, second, bucket_cap(SECONDS_KEPT))
        {
            slot.add(record);
        }
        let minute = at.div_euclid(MINUTE_MS);
        if in_window(minute, MINUTE_MS, MINUTES_KEPT)
            && let Some(bucket) = bucket_mut(&mut self.minutes, minute, bucket_cap(MINUTES_KEPT))
        {
            bucket.add(record);
        }
        if in_window(minute, MINUTE_MS, LATENCY_MINUTES_KEPT)
            && let Some(slot) = bucket_mut(
                &mut self.latency_minutes,
                minute,
                bucket_cap(LATENCY_MINUTES_KEPT),
            )
        {
            slot.add(record);
        }
        let hour = at.div_euclid(HOUR_MS);
        if in_window(hour, HOUR_MS, hours_kept)
            && let Some(bucket) = bucket_mut(&mut self.hours, hour, bucket_cap(hours_kept))
        {
            bucket.add(record);
        }
        if in_window(hour, HOUR_MS, LATENCY_HOURS_KEPT)
            && let Some(slot) = bucket_mut(
                &mut self.latency_hours,
                hour,
                bucket_cap(LATENCY_HOURS_KEPT),
            )
        {
            slot.add(record);
        }
        // A shortened retention shrinks the cap; let go of the surplus.
        while self.hours.len() > bucket_cap(hours_kept) {
            self.hours.pop_first();
        }
    }

    /// Drops whatever has slid out of its window as of `now`.
    fn evict(&mut self, now: i64, hours_kept: i64) {
        let minute = now.div_euclid(MINUTE_MS);
        let hour = now.div_euclid(HOUR_MS);
        drop_through(&mut self.seconds, now.div_euclid(SECOND_MS) - SECONDS_KEPT);
        drop_through(&mut self.minutes, minute - MINUTES_KEPT);
        drop_through(&mut self.latency_minutes, minute - LATENCY_MINUTES_KEPT);
        drop_through(&mut self.hours, hour - hours_kept);
        drop_through(&mut self.latency_hours, hour - LATENCY_HOURS_KEPT);
    }

    /// Puts a record in the recent ring. Beyond capacity the record that
    /// was recorded longest ago is dropped, whatever its start time.
    fn remember(&mut self, record: Arc<RequestRecord>) {
        if self.capacity == 0 {
            return;
        }
        if let Some(previous) = self.index.insert(record.id.clone(), record.started_at) {
            // The same id recorded again replaces the earlier entry.
            self.recent.remove(&(previous, record.id.clone()));
            if let Some(position) = self.arrivals.iter().rposition(|r| r.id == record.id) {
                self.arrivals.remove(position);
            }
        }
        self.recent
            .insert((record.started_at, record.id.clone()), Arc::clone(&record));
        self.arrivals.push_back(record);
        while self.arrivals.len() > self.capacity {
            if let Some(oldest) = self.arrivals.pop_front() {
                self.index.remove(&oldest.id);
                self.recent.remove(&(oldest.started_at, oldest.id.clone()));
            }
        }
    }

    fn buckets(&self, source: Source) -> &BTreeMap<i64, Bucket> {
        match source {
            Source::Minutes => &self.minutes,
            Source::Hours => &self.hours,
        }
    }

    /// Counters of the 60 seconds ending with the current one.
    fn last_minute(&self, now: i64) -> SecondSlot {
        let last = now.div_euclid(SECOND_MS);
        let mut sum = SecondSlot::default();
        for slot in self.seconds.range(last - 59..=last).map(|(_, slot)| slot) {
            sum.merge(slot);
        }
        sum
    }

    /// Percentiles over the last hour for [`Range::Hour`], over the last 24
    /// hours for every longer range.
    fn latency(&self, range: Range, now: i64) -> Latency {
        let mut merged = LatencySlot::default();
        let window_ms = if range == Range::Hour {
            let last = now.div_euclid(MINUTE_MS);
            let first = last - LATENCY_MINUTES_KEPT + 1;
            for slot in self
                .latency_minutes
                .range(first..=last)
                .map(|(_, slot)| slot)
            {
                merged.merge(slot);
            }
            HOUR_MS
        } else {
            let last = now.div_euclid(HOUR_MS);
            let first = last - LATENCY_HOURS_KEPT + 1;
            for slot in self.latency_hours.range(first..=last).map(|(_, slot)| slot) {
                merged.merge(slot);
            }
            DAY_MS
        };
        merged.percentiles(window_ms)
    }

    /// Resolves a `before` cursor to a position in `recent`.
    fn cursor(&self, text: &str) -> Option<RecentKey> {
        if let Some((at, id)) = text.split_once(':')
            && let Ok(at) = at.parse::<i64>()
        {
            return Some((at, id.to_string()));
        }
        if let Some(at) = self.index.get(text) {
            return Some((*at, text.to_string()));
        }
        if let Ok(at) = text.parse::<i64>() {
            // Everything that started before this instant.
            return Some((at, String::new()));
        }
        // An id that has left the ring: its embedded creation time still
        // says where the page boundary was.
        request_id_time_ms(text).map(|at| (at, text.to_string()))
    }
}

fn eq_opt(value: &Option<String>, wanted: &str) -> bool {
    value
        .as_deref()
        .is_some_and(|v| v.eq_ignore_ascii_case(wanted))
}

/// A [`RequestQuery`] prepared for matching many records.
struct Filter<'a> {
    model: Option<&'a str>,
    provider: Option<&'a str>,
    key: Option<&'a str>,
    status: Option<StatusFilter>,
    /// Lower-cased free-text needle.
    needle: Option<String>,
}

impl<'a> Filter<'a> {
    fn new(query: &'a RequestQuery) -> Self {
        Filter {
            model: query.model.as_deref(),
            provider: query.provider.as_deref(),
            key: query.key.as_deref(),
            status: query.status,
            needle: query.q.as_deref().map(str::to_lowercase),
        }
    }

    fn matches(&self, r: &RequestRecord) -> bool {
        // The aggregation name is compared as well: it is `unknown` for a
        // request without a model, the row the summaries list it under.
        if let Some(model) = self.model
            && !(r.requested_model.eq_ignore_ascii_case(model)
                || eq_opt(&r.client_model, model)
                || eq_opt(&r.upstream_model, model)
                || r.model_name().eq_ignore_ascii_case(model))
        {
            return false;
        }
        if let Some(provider) = self.provider
            && !r.provider_name().eq_ignore_ascii_case(provider)
        {
            return false;
        }
        if let Some(key) = self.key
            && !(r.key_name().eq_ignore_ascii_case(key)
                || eq_opt(&r.client.key_id, key)
                || eq_opt(&r.client.key_name, key))
        {
            return false;
        }
        if let Some(status) = self.status
            && !status.matches(r)
        {
            return false;
        }
        match &self.needle {
            Some(needle) => Self::text_matches(r, needle),
            None => true,
        }
    }

    fn text_matches(r: &RequestRecord, needle: &str) -> bool {
        let hit = |text: &str| text.to_lowercase().contains(needle);
        let hit_opt = |text: &Option<String>| text.as_deref().is_some_and(hit);
        hit(&r.id)
            || hit(&r.requested_model)
            || hit_opt(&r.client_model)
            || hit_opt(&r.upstream_model)
            || hit_opt(&r.provider)
            || hit_opt(&r.credential_label)
            || hit_opt(&r.client.key_name)
            || hit(&r.endpoint)
            || r.error
                .as_ref()
                .is_some_and(|e| hit(&e.kind) || hit(&e.message))
    }
}

/// Sorts a breakdown: most requests first, ties by name.
fn ranked(groups: HashMap<String, Totals>) -> Vec<NamedTotals> {
    let mut out: Vec<NamedTotals> = groups
        .into_iter()
        .map(|(name, totals)| NamedTotals { name, totals })
        .collect();
    out.sort_by(|a, b| {
        b.totals
            .requests
            .cmp(&a.totals.requests)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// The bucket width a time series is served at: the requested one, or the
/// range's natural one for `auto`, but never finer than the data.
fn effective_bucket(range: Range, requested: BucketSize, src_ms: i64) -> BucketSize {
    let chosen = match requested {
        BucketSize::Auto => match range {
            Range::Hour => BucketSize::Minute,
            Range::Day | Range::Week => BucketSize::Hour,
            Range::Month => BucketSize::Day,
        },
        other => other,
    };
    match chosen.width_ms() {
        Some(width) if width >= src_ms => chosen,
        _ => BucketSize::Hour,
    }
}

struct Inner {
    state: Mutex<State>,
    persister: RwLock<Option<Arc<Persister>>>,
    retention_days: AtomicU32,
    enabled: AtomicBool,
    /// Whether the last write to disk failed, to report a broken disk once
    /// instead of every second.
    write_failing: AtomicBool,
    gauges: Option<Gauges>,
}

impl Inner {
    fn hours_kept(&self) -> i64 {
        i64::from(self.retention_days.load(Ordering::Relaxed).max(1)) * 24
    }

    fn persister(&self) -> Option<Arc<Persister>> {
        self.persister.read().clone()
    }

    /// Writes the queue, reporting a failure the first time it happens and
    /// the recovery when writes work again.
    fn write_pending(&self, persister: &Persister) -> io::Result<usize> {
        let result = persister.write_pending();
        match &result {
            Ok(_) => {
                if self.write_failing.swap(false, Ordering::Relaxed) {
                    tracing::info!(dir = %persister.dir().display(), "usage records are being written again");
                }
            }
            Err(error) => {
                if !self.write_failing.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        dir = %persister.dir().display(),
                        %error,
                        "could not write usage records; statistics stay in memory only until this recovers"
                    );
                }
            }
        }
        result
    }
}

/// Usage statistics and the recent-requests ring. Cloning is cheap; clones
/// share the same data.
#[derive(Clone)]
pub struct UsageStore {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for UsageStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageStore")
            .field("info", &self.info())
            .field("persist_dir", &self.persist_dir())
            .finish()
    }
}

impl Default for UsageStore {
    fn default() -> Self {
        UsageStore::new(UsageStoreOptions::default())
    }
}

impl UsageStore {
    pub fn new(options: UsageStoreOptions) -> Self {
        UsageStore {
            inner: Arc::new(Inner {
                state: Mutex::new(State::new(options.recent_capacity)),
                persister: RwLock::new(
                    options.persist_dir.map(|dir| Arc::new(Persister::new(dir))),
                ),
                retention_days: AtomicU32::new(options.retention_days),
                enabled: AtomicBool::new(options.enabled),
                write_failing: AtomicBool::new(false),
                gauges: options.gauges,
            }),
        }
    }

    /// An in-memory store with default sizes.
    pub fn in_memory() -> Self {
        UsageStore::default()
    }

    // ------------------------------------------------------------------
    // Settings
    // ------------------------------------------------------------------

    pub fn enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Relaxed)
    }

    /// Turns recording on or off. What was recorded so far is kept.
    pub fn set_enabled(&self, enabled: bool) {
        self.inner.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn retention_days(&self) -> u32 {
        self.inner.retention_days.load(Ordering::Relaxed)
    }

    /// Changes the retention window. Queries honour it at once; the memory
    /// of buckets past a shorter window is released by the next
    /// [`prune`](UsageStore::prune).
    pub fn set_retention_days(&self, days: u32) {
        self.inner.retention_days.store(days, Ordering::Relaxed);
    }

    /// Directory records are persisted to, if any.
    pub fn persist_dir(&self) -> Option<PathBuf> {
        self.inner.persister().map(|p| p.dir().to_path_buf())
    }

    /// Starts, moves or stops persistence. Records still queued for the
    /// previous directory are written there first (blocking file I/O, only
    /// when the directory actually changes).
    pub fn set_persist_dir(&self, dir: Option<PathBuf>) {
        let previous = {
            let mut slot = self.inner.persister.write();
            if slot.as_ref().map(|p| p.dir()) == dir.as_deref() {
                return;
            }
            std::mem::replace(&mut *slot, dir.map(|dir| Arc::new(Persister::new(dir))))
        };
        if let Some(previous) = previous {
            let _ = self.inner.write_pending(&previous);
        }
    }

    // ------------------------------------------------------------------
    // Recording
    // ------------------------------------------------------------------

    /// Counts a finished request and queues it for persistence. Never
    /// touches the disk.
    pub fn record(&self, record: &RequestRecord) {
        self.record_arc(Arc::new(record.clone()));
    }

    /// [`record`](UsageStore::record) for a record that is already shared.
    pub fn record_arc(&self, record: Arc<RequestRecord>) {
        if !self.enabled() {
            return;
        }
        let hours_kept = self.inner.hours_kept();
        {
            let mut state = self.inner.state.lock();
            state.apply(&record, hours_kept, None);
            state.remember(Arc::clone(&record));
        }
        if let Some(persister) = self.inner.persister() {
            persister.enqueue(record);
        }
    }

    /// Forgets everything: the ring, every bucket, the write queue and the
    /// usage files on disk (blocking file I/O).
    pub fn clear(&self) {
        {
            let mut state = self.inner.state.lock();
            *state = State::new(state.capacity);
        }
        if let Some(persister) = self.inner.persister() {
            persister.clear();
        }
    }

    // ------------------------------------------------------------------
    // Request list
    // ------------------------------------------------------------------

    /// One page of recent requests, newest first, ordered by start time and
    /// then id. Paging with [`RequestPage::next_before`] is stable while new
    /// requests keep arriving: a page never repeats or skips a record that
    /// is still in memory.
    ///
    /// Only finished requests are listed (a record is made when a request
    /// ends), and of those the most recent [`RequestPage::capacity`].
    ///
    /// The ring holds the most recently *recorded* requests, i.e. the ones
    /// that finished last. A request that ran for a long time is listed at
    /// the position of its start time, which can be below records that were
    /// already on an earlier page.
    pub fn requests(&self, query: &RequestQuery) -> RequestPage {
        let limit = query.page_size();
        let filter = Filter::new(query);
        let state = self.inner.state.lock();
        let cursor = match query.before.as_deref() {
            Some(text) => match state.cursor(text) {
                Some(cursor) => Some(cursor),
                // An unusable cursor yields an empty page rather than the
                // first page again, which a paging client would loop on.
                None => {
                    return RequestPage {
                        items: Vec::new(),
                        next_before: None,
                        has_more: false,
                        total: 0,
                        capacity: state.capacity,
                    };
                }
            },
            None => None,
        };
        let mut items = Vec::with_capacity(limit.min(state.recent.len()));
        let mut total = 0;
        let mut has_more = false;
        for (key, record) in state.recent.iter().rev() {
            if !filter.matches(record) {
                continue;
            }
            total += 1;
            if cursor.as_ref().is_some_and(|cursor| key >= cursor) {
                continue;
            }
            if items.len() < limit {
                items.push(Arc::clone(record));
            } else {
                has_more = true;
            }
        }
        let next_before = match (has_more, items.last()) {
            (true, Some(last)) => Some(format!("{}:{}", last.started_at, last.id)),
            _ => None,
        };
        RequestPage {
            items,
            next_before,
            has_more,
            total,
            capacity: state.capacity,
        }
    }

    /// A record still in memory.
    pub fn get(&self, id: &str) -> Option<Arc<RequestRecord>> {
        let state = self.inner.state.lock();
        let started_at = *state.index.get(id)?;
        state.recent.get(&(started_at, id.to_string())).cloned()
    }

    /// A record by id: from memory, else from the usage files (blocking
    /// file I/O). The file lookup needs a UUIDv7 id, whose timestamp says
    /// which day's file to read.
    pub fn find(&self, id: &str) -> Option<Arc<RequestRecord>> {
        if let Some(record) = self.get(id) {
            return Some(record);
        }
        let persister = self.inner.persister()?;
        let minted_at = request_id_time_ms(id)?;
        // The id is minted next to, not at, the recorded start time, so
        // around midnight the record may sit in a neighbouring day's file.
        [0, -DAY_MS, DAY_MS]
            .into_iter()
            .find_map(|shift| find_in_day(persister.dir(), &utc_day(minted_at + shift), id))
            .map(Arc::new)
    }

    // ------------------------------------------------------------------
    // Statistics
    // ------------------------------------------------------------------

    /// Totals, breakdowns and latency of a range ending at `now`.
    pub fn summary(&self, range: Range, now: i64) -> UsageSummary {
        let state = self.inner.state.lock();
        let w = window(range, now, self.inner.hours_kept());
        let mut totals = Totals::default();
        let mut by_model = HashMap::new();
        let mut by_provider = HashMap::new();
        let mut by_key = HashMap::new();
        for bucket in state
            .buckets(w.source)
            .range(w.data_first..=w.last)
            .map(|(_, b)| b)
        {
            totals.merge(&bucket.totals);
            merge_groups(&mut by_model, &bucket.by_model);
            merge_groups(&mut by_provider, &bucket.by_provider);
            merge_groups(&mut by_key, &bucket.by_key);
        }
        let last_minute = state.last_minute(now);
        UsageSummary {
            range,
            from: w.first * w.src_ms,
            to: now,
            totals,
            latency: state.latency(range, now),
            error_rate: totals.error_rate(),
            requests_per_minute: last_minute.requests,
            tokens_per_minute: last_minute.tokens,
            by_model: ranked(by_model),
            by_provider: ranked(by_provider),
            by_key: ranked(by_key),
        }
    }

    /// Totals of a range ending at `now` per model, provider or client key,
    /// most requests first. With [`GroupBy::None`] the result is one entry
    /// named `total`.
    pub fn breakdown(&self, range: Range, group_by: GroupBy, now: i64) -> Vec<NamedTotals> {
        let state = self.inner.state.lock();
        let w = window(range, now, self.inner.hours_kept());
        let mut totals = Totals::default();
        let mut groups = HashMap::new();
        for bucket in state
            .buckets(w.source)
            .range(w.data_first..=w.last)
            .map(|(_, b)| b)
        {
            match bucket.groups(group_by) {
                Some(by_name) => merge_groups(&mut groups, by_name),
                None => totals.merge(&bucket.totals),
            }
        }
        if group_by == GroupBy::None {
            groups.insert("total".to_string(), totals);
        }
        ranked(groups)
    }

    /// A contiguous time series over a range ending at `now`. Buckets
    /// without traffic are present with zero counters.
    ///
    /// The bucket width is never finer than the stored data: a `minute`
    /// series of `7d` or `30d` is served per hour, and the response says so.
    /// With a `group_by`, the busiest series are returned by name and the
    /// rest is folded into [`OTHER`], so the groups of a point always add up
    /// to the point.
    pub fn timeseries(
        &self,
        range: Range,
        bucket: BucketSize,
        group_by: GroupBy,
        now: i64,
    ) -> Timeseries {
        let state = self.inner.state.lock();
        let w = window(range, now, self.inner.hours_kept());
        let bucket = effective_bucket(range, bucket, w.src_ms);
        let width = bucket.width_ms().unwrap_or(w.src_ms).max(w.src_ms);
        let align = |index: i64| (index * w.src_ms).div_euclid(width) * width;
        let from = align(w.first);
        let count = usize::try_from((align(w.last) - from) / width + 1).unwrap_or(0);
        let position = |index: i64| usize::try_from((align(index) - from) / width).ok();

        let mut points: Vec<TimePoint> = (0..count)
            .map(|i| TimePoint {
                t: from + i as i64 * width,
                totals: Totals::default(),
                groups: BTreeMap::new(),
            })
            .collect();
        let source = state.buckets(w.source);
        let mut per_series: HashMap<String, Totals> = HashMap::new();
        for (index, b) in source.range(w.data_first..=w.last) {
            if let Some(point) = position(*index).and_then(|p| points.get_mut(p)) {
                point.totals.merge(&b.totals);
            }
            if let Some(groups) = b.groups(group_by) {
                merge_groups(&mut per_series, groups);
            }
        }

        let ranking = ranked(per_series);
        let folded = ranking.len() > MAX_SERIES;
        let mut series: Vec<String> = ranking
            .into_iter()
            .take(MAX_SERIES)
            .map(|named| named.name)
            .collect();
        if group_by != GroupBy::None {
            let kept: HashSet<&str> = series.iter().map(String::as_str).collect();
            for (index, b) in source.range(w.data_first..=w.last) {
                let (Some(groups), Some(point)) = (
                    b.groups(group_by),
                    position(*index).and_then(|p| points.get_mut(p)),
                ) else {
                    continue;
                };
                for (name, totals) in groups {
                    let name = if kept.contains(name.as_str()) {
                        name.as_str()
                    } else {
                        OTHER
                    };
                    point
                        .groups
                        .entry(name.to_string())
                        .or_default()
                        .add(totals);
                }
            }
        }
        if folded && !series.iter().any(|name| name == OTHER) {
            series.push(OTHER.to_string());
        }

        Timeseries {
            range,
            bucket,
            bucket_ms: width,
            group_by,
            from,
            to: now,
            series,
            points,
        }
    }

    /// The numbers of the once-a-second dashboard `stats` frame. Gauges are
    /// zero unless the store was built with [`UsageStoreOptions::gauges`].
    pub fn stats_tick(&self, now: i64) -> StatsTick {
        let (last_minute, latency) = {
            let state = self.inner.state.lock();
            (state.last_minute(now), state.latency(Range::Hour, now))
        };
        let gauges = self.inner.gauges.as_ref();
        StatsTick {
            at: now,
            in_flight: gauges.map_or(0, Gauges::in_flight),
            active_streams: gauges.map_or(0, Gauges::active_streams),
            ws_connections: gauges.map_or(0, Gauges::ws_connections),
            rpm: last_minute.requests,
            tpm: last_minute.tokens,
            error_rate_1m: if last_minute.requests == 0 {
                0.0
            } else {
                last_minute.errors as f64 / last_minute.requests as f64
            },
            p50_ms: latency.p50,
            p95_ms: latency.p95,
            latency_samples: latency.samples,
        }
    }

    pub fn info(&self) -> UsageStoreInfo {
        let (recent, minute_buckets, hour_buckets) = {
            let state = self.inner.state.lock();
            (state.arrivals.len(), state.minutes.len(), state.hours.len())
        };
        let persister = self.inner.persister();
        UsageStoreInfo {
            recent,
            minute_buckets,
            hour_buckets,
            pending_writes: persister.as_ref().map_or(0, |p| p.pending_len()),
            dropped_writes: persister.as_ref().map_or(0, |p| p.dropped()),
        }
    }

    // ------------------------------------------------------------------
    // Persistence
    // ------------------------------------------------------------------

    /// Rebuilds the statistics and the recent ring from the usage files in
    /// `dir` (blocking file I/O; call once at start, before serving).
    ///
    /// Files older than `retention_days` are not read. Corrupt or truncated
    /// lines are skipped and counted in the report. Loaded records are not
    /// queued for writing again.
    pub fn load(&self, dir: &Path, retention_days: u32, now: i64) -> LoadReport {
        let hours_kept = i64::from(retention_days.max(1)) * 24;
        let mut state = self.inner.state.lock();
        let mut implausible = 0;
        let mut report = load_dir(dir, first_kept_day(retention_days, now), |record| {
            if event_time(&record) > now.saturating_add(FUTURE_TOLERANCE_MS) {
                implausible += 1;
                return;
            }
            state.apply(&record, hours_kept, Some(now));
            state.remember(Arc::new(record));
        });
        report.records -= implausible;
        report.skipped_lines += implausible;
        report
    }

    /// Writes queued records now (blocking file I/O) and returns how many
    /// were written.
    pub fn flush_blocking(&self) -> io::Result<usize> {
        match self.inner.persister() {
            Some(persister) => self.inner.write_pending(&persister),
            None => Ok(0),
        }
    }

    /// Writes queued records now without blocking the async runtime. Call
    /// on shutdown so the last second of records is not lost.
    ///
    /// Also waits for a write that is already in progress, so when this
    /// returns everything recorded before the call is on disk.
    pub async fn flush(&self) -> io::Result<usize> {
        let Some(persister) = self.inner.persister() else {
            return Ok(0);
        };
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || inner.write_pending(&persister))
            .await
            .unwrap_or_else(|join_error| Err(io::Error::other(join_error)))
    }

    /// Deletes usage files older than the retention window and releases the
    /// buckets and histograms that have slid out of every window as of
    /// `now` (blocking file I/O). Returns the number of files removed.
    pub fn prune(&self, now: i64) -> usize {
        let hours_kept = self.inner.hours_kept();
        self.inner.state.lock().evict(now, hours_kept);
        match self.inner.persister() {
            Some(persister) => persister.prune(first_kept_day(self.retention_days(), now)),
            None => 0,
        }
    }

    /// Starts the background writer: every second (sooner when the queue
    /// grows long) it appends the queued records to the day's file. The task
    /// ends on its own once every handle to the store is gone; abort it and
    /// call [`flush`](UsageStore::flush) for an orderly shutdown.
    pub fn spawn_writer(&self, handle: &Handle) -> JoinHandle<()> {
        let weak = Arc::downgrade(&self.inner);
        handle.spawn(async move {
            let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            let mut held: Option<Arc<Persister>> = None;
            while let Some(inner) = weak.upgrade() {
                held = inner.persister();
                // Only the persister is held across the wait; keeping the
                // store itself would stop it from ever being dropped.
                drop(inner);
                match &held {
                    Some(persister) => {
                        tokio::select! {
                            _ = ticker.tick() => {}
                            _ = persister.wake.notified() => {}
                        }
                    }
                    None => {
                        ticker.tick().await;
                    }
                }
                match weak.upgrade() {
                    // An idle gateway does not visit the blocking pool.
                    Some(inner) if inner.persister().is_some_and(|p| p.has_pending()) => {
                        let _ = UsageStore { inner }.flush().await;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            // The store is gone; save what it left behind.
            if let Some(persister) = held {
                let _ = tokio::task::spawn_blocking(move || persister.write_pending()).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Attempt, ClientInfo, RecordBuilder, RecordError, RequestStart};
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::fs;
    use switchyard_core::protocol::Protocol;
    use switchyard_core::usage::Usage;

    /// 2026-10-02T00:00:00Z.
    const T0: i64 = 1_790_899_200_000;
    const MODELS: [&str; 3] = ["gpt-5", "sonnet", "gemini-pro"];
    const PROVIDERS: [&str; 2] = ["openai", "anthropic"];
    const KEYS: [Option<&str>; 2] = [Some("laptop"), None];

    struct Spec<'a> {
        id: String,
        model: &'a str,
        provider: Option<&'a str>,
        key: Option<&'a str>,
        started_at: i64,
        duration_ms: i64,
        status: u16,
    }

    impl Spec<'_> {
        fn build(&self) -> RequestRecord {
            let start = RequestStart::new(
                Protocol::OpenaiChat,
                "POST /v1/chat/completions",
                self.model,
                self.started_at,
            )
            .with_id(self.id.clone())
            .with_client(ClientInfo {
                key_id: self.key.map(|k| format!("id-{k}")),
                key_name: self.key.map(str::to_string),
                ..ClientInfo::default()
            });
            let mut b = RecordBuilder::new(start);
            b.set_client_model(self.model);
            if let Some(provider) = self.provider {
                b.push_attempt(Attempt::new(
                    provider,
                    format!("up-{}", self.model),
                    Protocol::OpenaiChat,
                ));
            }
            b.set_usage(Usage {
                input_tokens: 100,
                cache_read_tokens: 20,
                cache_write_tokens: 4,
                output_tokens: 50,
                reasoning_tokens: 10,
            })
            // A power-of-two fraction, so sums are exact in any order.
            .set_cost(Some(0.125));
            b.mark_first_byte(self.started_at + self.duration_ms / 4);
            if self.status >= 400 {
                b.set_error(
                    RecordError::new("upstream", "upstream exploded")
                        .with_upstream_status(self.status),
                );
            }
            b.finish(self.status, self.started_at + self.duration_ms)
        }
    }

    fn simple(id: &str, started_at: i64, status: u16) -> RequestRecord {
        Spec {
            id: id.to_string(),
            model: "gpt-5",
            provider: Some("openai"),
            key: Some("laptop"),
            started_at,
            duration_ms: 500,
            status,
        }
        .build()
    }

    /// One request every ten minutes for three days starting at [`T0`];
    /// every seventh fails. Returns the records in arrival order.
    fn three_days() -> Vec<RequestRecord> {
        (0..432)
            .map(|i: i64| {
                Spec {
                    id: format!("req-{i:04}"),
                    model: MODELS[(i % 3) as usize],
                    provider: Some(PROVIDERS[(i % 2) as usize]),
                    key: KEYS[(i % 2) as usize],
                    started_at: T0 + i * 10 * MINUTE_MS,
                    duration_ms: 1_500,
                    status: if i % 7 == 0 { 502 } else { 200 },
                }
                .build()
            })
            .collect()
    }

    fn filled(retention_days: u32) -> (UsageStore, Vec<RequestRecord>) {
        let store = UsageStore::new(UsageStoreOptions {
            retention_days,
            ..UsageStoreOptions::default()
        });
        let records = three_days();
        for record in &records {
            store.record(record);
        }
        (store, records)
    }

    fn expected(records: &[RequestRecord]) -> Totals {
        let mut totals = Totals::default();
        for record in records {
            totals.add_record(record);
        }
        totals
    }

    fn sum_named(groups: &[NamedTotals]) -> Totals {
        let mut totals = Totals::default();
        for group in groups {
            totals.merge(&group.totals);
        }
        totals
    }

    const NOW: i64 = T0 + 3 * DAY_MS;

    #[test]
    fn summary_of_the_last_hour_reads_minute_buckets() {
        let (store, records) = filled(30);
        let s = store.summary(Range::Hour, NOW);
        // The request that started exactly an hour ago finished in the
        // minute just outside the 60-minute window.
        assert_eq!(s.totals, expected(&records[427..]));
        assert_eq!(s.totals.requests, 5);
        assert_eq!(s.range, Range::Hour);
        assert_eq!(s.from, NOW - 59 * MINUTE_MS);
        assert_eq!(s.to, NOW);
        assert_eq!(s.error_rate, s.totals.errors as f64 / 5.0);
    }

    #[test]
    fn summary_of_the_last_day() {
        let (store, records) = filled(30);
        let s = store.summary(Range::Day, NOW);
        assert_eq!(s.totals.requests, 143);
        assert_eq!(s.totals, expected(&records[432 - 143..]));
        assert_eq!(s.totals.input_tokens, 143 * 100);
        assert_eq!(s.totals.cache_read_tokens, 143 * 20);
        assert_eq!(s.totals.cache_write_tokens, 143 * 4);
        assert_eq!(s.totals.output_tokens, 143 * 50);
        assert_eq!(s.totals.reasoning_tokens, 143 * 10);
        assert_eq!(s.totals.cost, 143.0 * 0.125);
        assert_eq!(s.totals.duration_ms_sum, 143 * 1_500);
        assert_eq!(s.totals.ttfb_ms_sum, 143 * 375);
        assert_eq!(s.totals.ttfb_count, 143);
    }

    #[test]
    fn week_and_month_read_hour_buckets_and_see_everything() {
        let (store, records) = filled(30);
        let all = expected(&records);
        assert_eq!(all.requests, 432);
        assert_eq!(all.errors, 62);
        for range in [Range::Week, Range::Month] {
            let s = store.summary(range, NOW);
            assert_eq!(s.totals, all, "{range}");
        }
        let week = store.summary(Range::Week, NOW);
        assert_eq!(week.from, NOW - 167 * HOUR_MS);
    }

    #[test]
    fn breakdowns_add_up_to_the_totals_and_are_ranked() {
        let (store, _) = filled(30);
        for range in Range::ALL {
            let s = store.summary(range, NOW);
            assert_eq!(sum_named(&s.by_model), s.totals, "{range} by model");
            assert_eq!(sum_named(&s.by_provider), s.totals, "{range} by provider");
            assert_eq!(sum_named(&s.by_key), s.totals, "{range} by key");
            for groups in [&s.by_model, &s.by_provider, &s.by_key] {
                assert!(
                    groups
                        .windows(2)
                        .all(|w| w[0].totals.requests >= w[1].totals.requests),
                    "{range}: not sorted by requests"
                );
            }
        }
        let s = store.summary(Range::Week, NOW);
        let names = |groups: &[NamedTotals]| -> Vec<(String, u64)> {
            groups
                .iter()
                .map(|g| (g.name.clone(), g.totals.requests))
                .collect()
        };
        assert_eq!(
            names(&s.by_model),
            [
                ("gemini-pro".to_string(), 144),
                ("gpt-5".to_string(), 144),
                ("sonnet".to_string(), 144)
            ]
        );
        assert_eq!(
            names(&s.by_provider),
            [("anthropic".to_string(), 216), ("openai".to_string(), 216)]
        );
        assert_eq!(
            names(&s.by_key),
            [("anonymous".to_string(), 216), ("laptop".to_string(), 216)]
        );
    }

    #[test]
    fn breakdown_is_one_dimension_of_the_summary() {
        let (store, _) = filled(30);
        for range in Range::ALL {
            let s = store.summary(range, NOW);
            assert_eq!(store.breakdown(range, GroupBy::Model, NOW), s.by_model);
            assert_eq!(
                store.breakdown(range, GroupBy::Provider, NOW),
                s.by_provider
            );
            assert_eq!(store.breakdown(range, GroupBy::Key, NOW), s.by_key);
            assert_eq!(
                store.breakdown(range, GroupBy::None, NOW),
                [NamedTotals {
                    name: "total".to_string(),
                    totals: s.totals
                }]
            );
        }
    }

    #[test]
    fn timeseries_matches_the_summary_for_every_range_and_bucket() {
        let (store, _) = filled(30);
        for range in Range::ALL {
            let summary = store.summary(range, NOW);
            for bucket in [
                BucketSize::Auto,
                BucketSize::Minute,
                BucketSize::Hour,
                BucketSize::Day,
            ] {
                for group_by in [
                    GroupBy::None,
                    GroupBy::Model,
                    GroupBy::Provider,
                    GroupBy::Key,
                ] {
                    let ts = store.timeseries(range, bucket, group_by, NOW);
                    let label = format!("{range} {bucket:?} {group_by:?}");
                    let mut sum = Totals::default();
                    for point in &ts.points {
                        sum.merge(&point.totals);
                    }
                    assert_eq!(sum, summary.totals, "{label}");
                    // Contiguous, aligned buckets.
                    assert_eq!(ts.bucket_ms, ts.bucket.width_ms().unwrap(), "{label}");
                    assert_eq!(ts.from.rem_euclid(ts.bucket_ms), 0, "{label}");
                    for (i, point) in ts.points.iter().enumerate() {
                        assert_eq!(point.t, ts.from + i as i64 * ts.bucket_ms, "{label}");
                    }
                    assert!(ts.points.last().unwrap().t <= NOW, "{label}");
                    assert!(ts.points.last().unwrap().t + ts.bucket_ms > NOW, "{label}");
                    // Groups of a point add up to the point.
                    for point in &ts.points {
                        if group_by == GroupBy::None {
                            assert!(point.groups.is_empty(), "{label}");
                            continue;
                        }
                        let requests: u64 = point.groups.values().map(|g| g.requests).sum();
                        let errors: u64 = point.groups.values().map(|g| g.errors).sum();
                        let tokens: u64 = point.groups.values().map(|g| g.tokens).sum();
                        let cost: f64 = point.groups.values().map(|g| g.cost).sum();
                        assert_eq!(requests, point.totals.requests, "{label}");
                        assert_eq!(errors, point.totals.errors, "{label}");
                        assert_eq!(tokens, point.totals.total_tokens(), "{label}");
                        assert_eq!(cost, point.totals.cost, "{label}");
                        assert!(
                            point.groups.keys().all(|name| ts.series.contains(name)),
                            "{label}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn timeseries_bucket_selection() {
        let (store, _) = filled(30);
        let shape = |range, bucket| {
            let ts = store.timeseries(range, bucket, GroupBy::None, NOW);
            (ts.bucket, ts.bucket_ms, ts.points.len())
        };
        assert_eq!(
            shape(Range::Hour, BucketSize::Auto),
            (BucketSize::Minute, MINUTE_MS, 60)
        );
        // The window starts one minute into the hour 24 hours ago, so that
        // hour is a partial first bucket.
        assert_eq!(
            shape(Range::Day, BucketSize::Auto),
            (BucketSize::Hour, HOUR_MS, 25)
        );
        assert_eq!(
            shape(Range::Day, BucketSize::Minute),
            (BucketSize::Minute, MINUTE_MS, 1_440)
        );
        assert_eq!(
            shape(Range::Week, BucketSize::Auto),
            (BucketSize::Hour, HOUR_MS, 168)
        );
        assert_eq!(
            shape(Range::Week, BucketSize::Day),
            (BucketSize::Day, DAY_MS, 8)
        );
        assert_eq!(
            shape(Range::Month, BucketSize::Auto),
            (BucketSize::Day, DAY_MS, 31)
        );
        // Minute data does not exist that far back: served per hour.
        assert_eq!(
            shape(Range::Week, BucketSize::Minute),
            (BucketSize::Hour, HOUR_MS, 168)
        );
        assert_eq!(
            shape(Range::Month, BucketSize::Minute),
            (BucketSize::Hour, HOUR_MS, 720)
        );
        assert_eq!(
            shape(Range::Hour, BucketSize::Day),
            (BucketSize::Day, DAY_MS, 2)
        );
    }

    #[test]
    fn timeseries_fills_gaps_with_zeros() {
        let store = UsageStore::in_memory();
        // Traffic in two hours, four hours apart.
        store.record(&simple("a", T0 + 10 * MINUTE_MS, 200));
        store.record(&simple("b", T0 + 20 * MINUTE_MS, 500));
        store.record(&simple("c", T0 + 4 * HOUR_MS + 5 * MINUTE_MS, 200));
        let now = T0 + 6 * HOUR_MS + 30 * MINUTE_MS;
        let ts = store.timeseries(Range::Week, BucketSize::Hour, GroupBy::Model, now);
        assert_eq!(ts.points.len(), 168);
        assert_eq!(ts.series, ["gpt-5"]);
        let busy: Vec<(i64, u64, u64)> = ts
            .points
            .iter()
            .filter(|p| p.totals.requests > 0)
            .map(|p| (p.t, p.totals.requests, p.totals.errors))
            .collect();
        assert_eq!(busy, [(T0, 2, 1), (T0 + 4 * HOUR_MS, 1, 0)]);
        let quiet = &ts.points[ts.points.len() - 2];
        assert_eq!(quiet.t, T0 + 5 * HOUR_MS);
        assert_eq!(quiet.totals, Totals::default());
        assert!(quiet.groups.is_empty());

        // An empty store still yields the full, zeroed axis.
        let empty =
            UsageStore::in_memory().timeseries(Range::Day, BucketSize::Auto, GroupBy::Key, now);
        assert_eq!(empty.points.len(), 25);
        assert!(empty.points.iter().all(|p| p.totals == Totals::default()));
        assert!(empty.series.is_empty());
    }

    #[test]
    fn timeseries_json_shape() {
        let store = UsageStore::in_memory();
        store.record(&simple("a", T0 + 10 * MINUTE_MS, 200));
        let ts = store.timeseries(
            Range::Hour,
            BucketSize::Auto,
            GroupBy::Provider,
            T0 + 11 * MINUTE_MS,
        );
        let value = serde_json::to_value(&ts).unwrap();
        assert_eq!(value["range"], "1h");
        assert_eq!(value["bucket"], "minute");
        assert_eq!(value["bucket_ms"], 60_000);
        assert_eq!(value["group_by"], "provider");
        assert_eq!(value["series"], json!(["openai"]));
        assert_eq!(value["points"].as_array().unwrap().len(), 60);
        assert_eq!(
            value["points"][58],
            json!({
                "t": T0 + 10 * MINUTE_MS,
                "requests": 1, "errors": 0, "input_tokens": 100, "cache_read_tokens": 20,
                "cache_write_tokens": 4, "output_tokens": 50, "reasoning_tokens": 10,
                "cost": 0.125, "duration_ms_sum": 500, "ttfb_ms_sum": 125, "ttfb_count": 1,
                "groups": {"openai": {"requests": 1, "errors": 0, "tokens": 174, "cost": 0.125}}
            })
        );
        assert_eq!(value["points"][59]["requests"], 0);
        assert_eq!(value["points"][59]["groups"], json!({}));
    }

    #[test]
    fn many_series_are_folded_into_other() {
        let store = UsageStore::in_memory();
        for i in 0..30 {
            // Model i gets i + 1 requests.
            for n in 0..=i {
                let mut r = simple(&format!("r-{i}-{n}"), T0 + i * SECOND_MS, 200);
                r.client_model = Some(format!("model-{i:02}"));
                store.record(&r);
            }
        }
        let now = T0 + MINUTE_MS;
        let ts = store.timeseries(Range::Hour, BucketSize::Auto, GroupBy::Model, now);
        assert_eq!(ts.series.len(), MAX_SERIES + 1);
        assert_eq!(ts.series[0], "model-29");
        assert_eq!(ts.series[MAX_SERIES], OTHER);
        let point = ts.points.iter().find(|p| p.totals.requests > 0).unwrap();
        assert_eq!(point.totals.requests, 465);
        assert_eq!(point.groups.len(), MAX_SERIES + 1);
        // Models 0..=9 have 1 + 2 + … + 10 requests between them.
        assert_eq!(point.groups[OTHER].requests, 55);
        assert_eq!(point.groups.values().map(|g| g.requests).sum::<u64>(), 465);
        // The summary keeps every name.
        assert_eq!(store.summary(Range::Hour, now).by_model.len(), 30);
    }

    #[test]
    fn minute_buckets_roll_off_after_a_day_but_hours_remain() {
        let (store, records) = filled(30);
        assert_eq!(store.info().hour_buckets, 72);
        // Pruning releases the minute buckets older than 24 hours.
        assert_eq!(store.prune(NOW), 0);
        assert_eq!(store.info().minute_buckets, 143);
        assert_eq!(store.info().hour_buckets, 72);
        // A day later nothing is left in minute resolution, while the hourly
        // history still answers.
        let later = NOW + 25 * HOUR_MS;
        assert_eq!(store.summary(Range::Day, later).totals, Totals::default());
        assert_eq!(store.summary(Range::Hour, later).totals, Totals::default());
        assert_eq!(store.summary(Range::Week, later).totals, expected(&records));
        store.prune(later);
        assert_eq!(store.info().minute_buckets, 0);
        store.record(&simple("late", later, 200));
        assert_eq!(store.info().minute_buckets, 1);
        assert_eq!(store.info().hour_buckets, 73);
        assert_eq!(store.summary(Range::Day, later + 1_000).totals.requests, 1);
        assert_eq!(
            store.summary(Range::Week, later + 1_000).totals.requests,
            433
        );
    }

    #[test]
    fn hour_buckets_are_evicted_beyond_retention() {
        let (store, records) = filled(2);
        // Queries never reach back beyond the retention window (the 48
        // hours ending with the current one; that one is still empty),
        // whether or not the older buckets have been released yet.
        let s = store.summary(Range::Month, NOW);
        assert_eq!(s.totals, expected(&records[432 - 47 * 6..]));
        let ts = store.timeseries(Range::Month, BucketSize::Hour, GroupBy::None, NOW);
        assert_eq!(ts.points.len(), 720);
        let busy = ts.points.iter().filter(|p| p.totals.requests > 0).count();
        assert_eq!(busy, 47);
        assert_eq!(store.prune(NOW), 0);
        assert_eq!(store.info().hour_buckets, 47);
        assert_eq!(store.summary(Range::Month, NOW), s);
        // Shortening the retention takes effect at once for queries, and on
        // prune for the memory.
        store.set_retention_days(1);
        assert_eq!(store.summary(Range::Month, NOW).totals.requests, 23 * 6);
        assert_eq!(store.info().hour_buckets, 47);
        assert_eq!(store.prune(NOW), 0);
        assert_eq!(store.info().hour_buckets, 23);
        assert_eq!(store.summary(Range::Month, NOW).totals.requests, 23 * 6);
    }

    #[test]
    fn out_of_order_and_stale_records() {
        let store = UsageStore::in_memory();
        store.record(&simple("new", T0 + 2 * DAY_MS, 200));
        // Finished a moment earlier but reported later: still counted.
        store.record(&simple("slightly-older", T0 + 2 * DAY_MS - 5_000, 200));
        // Older than the minute window: only the hourly history shows it.
        store.record(&simple("old", T0, 200));
        let now = T0 + 2 * DAY_MS + 1_000;
        assert_eq!(store.summary(Range::Hour, now).totals.requests, 2);
        assert_eq!(store.summary(Range::Day, now).totals.requests, 2);
        assert_eq!(store.summary(Range::Week, now).totals.requests, 3);
        store.prune(now);
        assert_eq!(store.info().minute_buckets, 2);
        assert_eq!(store.summary(Range::Week, now).totals.requests, 3);
    }

    #[test]
    fn a_record_from_the_future_does_not_hide_real_traffic() {
        let store = UsageStore::in_memory();
        for i in 0..3 {
            store.record(&simple(&format!("before{i}"), T0 + i * 1_000, 200));
        }
        // The clock was three days ahead for one request.
        store.record(&simple("future", T0 + 3 * DAY_MS, 200));
        for i in 0..4 {
            store.record(&simple(&format!("after{i}"), T0 + 5_000 + i * 1_000, 500));
        }
        let now = T0 + 20_000;
        for range in Range::ALL {
            let s = store.summary(range, now);
            assert_eq!((s.totals.requests, s.totals.errors), (7, 4), "{range}");
            assert_eq!(s.latency.samples, 7, "{range}");
            assert_eq!(s.requests_per_minute, 7, "{range}");
        }
        let tick = store.stats_tick(now);
        assert_eq!(tick.rpm, 7);
        assert!((tick.error_rate_1m - 4.0 / 7.0).abs() < 1e-12);
        // Pruning at the real time keeps everything, the future bucket too:
        // it is counted once time gets there.
        store.prune(now);
        assert_eq!(store.summary(Range::Hour, now).totals.requests, 7);
        let then = T0 + 3 * DAY_MS + 1_000;
        assert_eq!(store.summary(Range::Hour, then).totals.requests, 1);
        assert_eq!(store.summary(Range::Week, then).totals.requests, 8);
    }

    #[test]
    fn bucket_maps_are_bounded_without_pruning() {
        let mut map: BTreeMap<i64, u32> = BTreeMap::new();
        for key in 0..10 {
            *bucket_mut(&mut map, key, 4).unwrap() += 1;
        }
        // The newest four are kept.
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), [6, 7, 8, 9]);
        // Older than everything kept: not created.
        assert!(bucket_mut(&mut map, 2, 4).is_none());
        // An existing bucket keeps counting; one inside the span replaces
        // the oldest.
        *bucket_mut(&mut map, 7, 4).unwrap() += 1;
        assert_eq!(map[&7], 2);
        assert!(bucket_mut(&mut map, 100, 4).is_some());
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), [7, 8, 9, 100]);
        assert_eq!(bucket_cap(60), 120);
        assert_eq!(bucket_cap(-5), 1);

        // Three days of one request per minute, never pruned: every map
        // stops at its cap.
        let store = UsageStore::in_memory();
        for i in 0..(3 * MINUTES_KEPT) {
            store.record(&simple(&format!("r{i}"), T0 + i * MINUTE_MS, 200));
        }
        let state = store.inner.state.lock();
        assert_eq!(state.minutes.len(), bucket_cap(MINUTES_KEPT));
        assert_eq!(state.seconds.len(), bucket_cap(SECONDS_KEPT));
        assert_eq!(
            state.latency_minutes.len(),
            bucket_cap(LATENCY_MINUTES_KEPT)
        );
        assert_eq!(state.latency_hours.len(), bucket_cap(LATENCY_HOURS_KEPT));
        assert_eq!(state.hours.len(), 72);
    }

    #[test]
    fn rates_cover_the_last_sixty_seconds() {
        let store = UsageStore::in_memory();
        let base = T0 + HOUR_MS;
        // Finishing at base-90s, base-59s, base-30s (failed) and base.
        for (id, offset, status) in [
            ("a", -90, 200),
            ("b", -59, 200),
            ("c", -30, 500),
            ("d", 0, 200),
        ] {
            store.record(&simple(id, base + offset * SECOND_MS - 500, status));
        }
        let s = store.summary(Range::Hour, base);
        assert_eq!(s.requests_per_minute, 3);
        assert_eq!(s.tokens_per_minute, 3 * 174);
        let tick = store.stats_tick(base);
        assert_eq!(tick.at, base);
        assert_eq!((tick.rpm, tick.tpm), (3, 3 * 174));
        assert!((tick.error_rate_1m - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!((tick.p50_ms, tick.p95_ms), (500, 500));
        // The percentiles cover the last hour: all four requests.
        assert_eq!(tick.latency_samples, 4);
        // Half a minute later two of them have slid out.
        let tick = store.stats_tick(base + 31 * SECOND_MS);
        assert_eq!(tick.rpm, 1);
        assert_eq!(tick.error_rate_1m, 0.0);
        // After two idle minutes the rates are zero.
        let tick = store.stats_tick(base + 2 * MINUTE_MS);
        assert_eq!((tick.rpm, tick.tpm, tick.error_rate_1m), (0, 0, 0.0));
        // …while the percentiles still have their samples.
        assert_eq!((tick.p50_ms, tick.latency_samples), (500, 4));
    }

    /// Regression: without a sample count a dashboard could not tell "no
    /// request in the last hour" (percentiles 0) from requests that really
    /// took 0 ms, and showed "0ms".
    #[test]
    fn stats_tick_says_how_many_samples_the_percentiles_have() {
        let store = UsageStore::in_memory();
        let fresh = store.stats_tick(T0);
        assert_eq!(
            (fresh.p50_ms, fresh.p95_ms, fresh.latency_samples),
            (0, 0, 0)
        );
        let base = T0 + 3 * HOUR_MS;
        store.record(&simple("a", base - 500, 200));
        store.record(&simple("b", base - 500, 502));
        let tick = store.stats_tick(base);
        assert_eq!(tick.latency_samples, 2);
        assert_eq!(
            tick.latency_samples,
            store.summary(Range::Hour, base).latency.samples
        );
        // An hour later nothing is left to measure.
        let later = store.stats_tick(base + HOUR_MS + MINUTE_MS);
        assert_eq!(
            (later.p50_ms, later.p95_ms, later.latency_samples),
            (0, 0, 0)
        );
        let value = serde_json::to_value(tick).unwrap();
        assert_eq!(value["latency_samples"], json!(2));
        // Frames written before the field existed still read.
        let old: StatsTick = serde_json::from_value(json!({
            "at": 1, "in_flight": 0, "active_streams": 0, "ws_connections": 0,
            "rpm": 0, "tpm": 0, "error_rate_1m": 0.0, "p50_ms": 7, "p95_ms": 9
        }))
        .unwrap();
        assert_eq!(old.latency_samples, 0);
    }

    #[test]
    fn stats_tick_reports_attached_gauges() {
        let gauges = Gauges::new(T0);
        let store = UsageStore::new(UsageStoreOptions {
            gauges: Some(gauges.clone()),
            ..UsageStoreOptions::default()
        });
        let _a = gauges.track_in_flight();
        let _b = gauges.track_in_flight();
        let _s = gauges.track_stream();
        let _w = gauges.track_ws();
        let tick = store.stats_tick(T0);
        assert_eq!(
            (tick.in_flight, tick.active_streams, tick.ws_connections),
            (2, 1, 1)
        );
        let detached = UsageStore::in_memory().stats_tick(T0);
        assert_eq!(
            (
                detached.in_flight,
                detached.active_streams,
                detached.ws_connections
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn latency_percentiles_slide() {
        let store = UsageStore::in_memory();
        let base = T0 + 5 * HOUR_MS;
        // 100 requests of 10..=1000 ms finishing within one minute.
        for i in 1..=100i64 {
            let mut spec = Spec {
                id: format!("r{i}"),
                model: "gpt-5",
                provider: Some("openai"),
                key: None,
                started_at: base,
                duration_ms: i * 10,
                status: 200,
            };
            spec.started_at = base - spec.duration_ms;
            store.record(&spec.build());
        }
        let close = |actual: u64, expected: u64| actual.abs_diff(expected) <= expected / 50 + 1;
        let l = store.summary(Range::Hour, base).latency;
        assert_eq!(l.window_ms, HOUR_MS);
        assert_eq!((l.samples, l.ttfb_samples), (100, 100));
        assert!(close(l.p50, 500), "{l:?}");
        assert!(close(l.p90, 900), "{l:?}");
        assert!(close(l.p95, 950), "{l:?}");
        assert!(close(l.p99, 990), "{l:?}");
        assert!(close(l.ttfb_p50, 125), "{l:?}");
        assert!(close(l.ttfb_p95, 237), "{l:?}");
        // Longer ranges use the hourly histograms.
        let day = store.summary(Range::Day, base).latency;
        assert_eq!(day.window_ms, DAY_MS);
        assert_eq!(day.samples, 100);
        assert!(close(day.p95, 950), "{day:?}");
        // Two hours later the hour window is empty, the day window is not.
        let later = base + 2 * HOUR_MS;
        assert_eq!(store.summary(Range::Hour, later).latency.samples, 0);
        assert_eq!(store.summary(Range::Hour, later).latency.p99, 0);
        assert_eq!(store.summary(Range::Week, later).latency.samples, 100);
        assert_eq!(store.stats_tick(later).p95_ms, 0);
    }

    #[test]
    fn summary_json_shape() {
        let store = UsageStore::in_memory();
        store.record(&simple("a", T0, 200));
        let value = serde_json::to_value(store.summary(Range::Day, T0 + 1_000)).unwrap();
        assert_eq!(value["range"], "24h");
        assert_eq!(value["to"], T0 + 1_000);
        assert_eq!(value["totals"]["requests"], 1);
        assert_eq!(value["error_rate"], 0.0);
        assert_eq!(value["requests_per_minute"], 1);
        assert_eq!(value["tokens_per_minute"], 174);
        assert_eq!(
            value["latency"],
            json!({
                "window_ms": DAY_MS, "p50": 500, "p90": 500, "p95": 500, "p99": 500,
                "ttfb_p50": 125, "ttfb_p95": 125, "samples": 1, "ttfb_samples": 1
            })
        );
        assert_eq!(
            value["by_model"],
            json!([{
                "name": "gpt-5", "requests": 1, "errors": 0, "input_tokens": 100,
                "cache_read_tokens": 20, "cache_write_tokens": 4, "output_tokens": 50,
                "reasoning_tokens": 10, "cost": 0.125, "duration_ms_sum": 500,
                "ttfb_ms_sum": 125, "ttfb_count": 1
            }])
        );
        assert_eq!(value["by_provider"][0]["name"], "openai");
        assert_eq!(value["by_key"][0]["name"], "laptop");
    }

    // ------------------------------------------------------------------
    // Recent ring
    // ------------------------------------------------------------------

    fn ids(page: &RequestPage) -> Vec<&str> {
        page.items.iter().map(|r| r.id.as_str()).collect()
    }

    fn query(value: serde_json::Value) -> RequestQuery {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn ring_is_bounded_and_keeps_the_newest() {
        let store = UsageStore::new(UsageStoreOptions {
            recent_capacity: 5,
            ..UsageStoreOptions::default()
        });
        for i in 0..12 {
            store.record(&simple(&format!("r{i:02}"), T0 + i * 1_000, 200));
        }
        let page = store.requests(&RequestQuery::default());
        assert_eq!(ids(&page), ["r11", "r10", "r09", "r08", "r07"]);
        assert_eq!(page.total, 5);
        assert!(!page.has_more);
        assert_eq!(page.next_before, None);
        assert!(store.get("r06").is_none());
        assert_eq!(store.get("r07").unwrap().started_at, T0 + 7_000);
        assert_eq!(store.info().recent, 5);
        // The statistics are not bounded by the ring.
        assert_eq!(store.summary(Range::Hour, T0 + 12_000).totals.requests, 12);
    }

    #[test]
    fn ring_evicts_by_arrival_not_by_start_time() {
        let store = UsageStore::new(UsageStoreOptions {
            recent_capacity: 3,
            ..UsageStoreOptions::default()
        });
        for i in 0..3 {
            store.record(&simple(
                &format!("short{i}"),
                T0 + MINUTE_MS + i * 1_000,
                200,
            ));
        }
        // Started before all of them, finished (and recorded) last.
        let mut long = simple("long", T0, 200);
        long.finished_at = T0 + 10 * MINUTE_MS;
        long.duration_ms = 10 * 60_000;
        store.record(&long);
        assert_eq!(store.get("long").unwrap().duration_ms, 600_000);
        // The record that arrived first made room; the list is still in
        // start order, so the long request comes last.
        assert!(store.get("short0").is_none());
        let page = store.requests(&RequestQuery::default());
        assert_eq!(ids(&page), ["short2", "short1", "long"]);
        assert_eq!(store.info().recent, 3);
        // The next arrival evicts the next oldest arrival, not "long".
        store.record(&simple("short3", T0 + 11 * MINUTE_MS, 200));
        let page = store.requests(&RequestQuery::default());
        assert_eq!(ids(&page), ["short3", "short2", "long"]);
        // Paging through it stays consistent.
        let first = store.requests(&query(json!({"limit": 2})));
        let second = store.requests(&query(json!({"limit": 2, "before": first.next_before})));
        assert_eq!(ids(&second), ["long"]);
        assert!(!second.has_more);
    }

    #[test]
    fn re_recording_an_id_does_not_grow_the_ring() {
        let store = UsageStore::new(UsageStoreOptions {
            recent_capacity: 3,
            ..UsageStoreOptions::default()
        });
        store.record(&simple("a", T0, 200));
        store.record(&simple("b", T0 + 1, 200));
        for i in 0..50 {
            store.record(&simple("dup", T0 + 10 + i, 200));
        }
        {
            let state = store.inner.state.lock();
            assert_eq!(state.arrivals.len(), 3);
            assert_eq!(state.recent.len(), 3);
            assert_eq!(state.index.len(), 3);
        }
        assert_eq!(
            ids(&store.requests(&RequestQuery::default())),
            ["dup", "b", "a"]
        );
        // Re-recording refreshes the arrival position: "a" goes first.
        store.record(&simple("a", T0, 500));
        store.record(&simple("c", T0 + 2, 200));
        assert_eq!(
            ids(&store.requests(&RequestQuery::default())),
            ["dup", "c", "a"]
        );
        assert_eq!(store.get("a").unwrap().status, 500);
    }

    #[test]
    fn list_is_ordered_by_start_time_then_id() {
        let store = UsageStore::in_memory();
        // Arrival order differs from start order; two share a start time.
        store.record(&simple("c", T0 + 3_000, 200));
        store.record(&simple("a", T0 + 1_000, 200));
        store.record(&simple("b2", T0 + 2_000, 200));
        store.record(&simple("b1", T0 + 2_000, 200));
        let page = store.requests(&RequestQuery::default());
        assert_eq!(ids(&page), ["c", "b2", "b1", "a"]);
    }

    #[test]
    fn filters() {
        let store = UsageStore::in_memory();
        let mut specs = Vec::new();
        for i in 0..12i64 {
            specs.push(Spec {
                id: format!("req-{i:02}"),
                model: MODELS[(i % 3) as usize],
                provider: if i == 11 {
                    None
                } else {
                    Some(PROVIDERS[(i % 2) as usize])
                },
                key: KEYS[(i % 2) as usize],
                started_at: T0 + i * 1_000,
                duration_ms: 100,
                status: if i % 4 == 0 { 429 } else { 200 },
            });
        }
        for spec in &specs {
            store.record(&spec.build());
        }
        let run = |value: serde_json::Value| {
            let page = store.requests(&query(value));
            let mut found: Vec<String> = page.items.iter().map(|r| r.id.clone()).collect();
            found.sort();
            assert_eq!(page.total, found.len());
            found
        };
        assert_eq!(
            run(json!({"model": "SONNET"})),
            ["req-01", "req-04", "req-07", "req-10"]
        );
        // The upstream model name works too.
        assert_eq!(run(json!({"model": "up-sonnet"})).len(), 4);
        assert_eq!(
            run(json!({"provider": "anthropic"})),
            ["req-01", "req-03", "req-05", "req-07", "req-09"]
        );
        assert_eq!(run(json!({"provider": "unknown"})), ["req-11"]);
        assert_eq!(run(json!({"key": "laptop"})).len(), 6);
        assert_eq!(run(json!({"key": "id-laptop"})).len(), 6);
        assert_eq!(run(json!({"key": "anonymous"})).len(), 6);
        assert_eq!(
            run(json!({"status": "error"})),
            ["req-00", "req-04", "req-08"]
        );
        assert_eq!(run(json!({"status": "ok"})).len(), 9);
        assert_eq!(run(json!({"status": "429"})).len(), 3);
        assert_eq!(run(json!({"status": "4xx"})).len(), 3);
        assert_eq!(run(json!({"status": "5xx"})).len(), 0);
        // Free text: id, model, provider, error message.
        assert_eq!(run(json!({"q": "REQ-07"})), ["req-07"]);
        assert_eq!(run(json!({"q": "gemini"})).len(), 4);
        assert_eq!(run(json!({"q": "openai"})).len(), 6);
        assert_eq!(
            run(json!({"q": "exploded"})),
            ["req-00", "req-04", "req-08"]
        );
        assert_eq!(run(json!({"q": "no such thing"})).len(), 0);
        // Filters combine.
        assert_eq!(
            run(json!({"model": "gpt-5", "status": "error", "key": "laptop"})),
            ["req-00"]
        );
    }

    /// Regression: the summaries list requests without a model under
    /// `unknown`, but `model=unknown` selected nothing, so that row of the
    /// usage page opened an empty list. (`provider=unknown` and
    /// `key=anonymous` already matched their rows.)
    #[test]
    fn the_unknown_model_row_of_the_summary_selects_its_requests() {
        let store = UsageStore::in_memory();
        store.record(&simple("with-model", T0, 200));
        // Refused before a model could be read from the body.
        let start = RequestStart::new(
            Protocol::OpenaiChat,
            "POST /v1/chat/completions",
            "",
            T0 + 1_000,
        )
        .with_id("no-model");
        let mut builder = RecordBuilder::new(start);
        builder.set_error(RecordError::new("invalid_request", "the body is not JSON"));
        let record = builder.finish(400, T0 + 1_001);
        assert_eq!(record.requested_model, "");
        assert_eq!(record.model_name(), crate::UNKNOWN);
        store.record(&record);

        let by_model = store.summary(Range::Hour, T0 + 2_000).by_model;
        let names: Vec<&str> = by_model.iter().map(|g| g.name.as_str()).collect();
        assert!(names.contains(&"unknown"), "{names:?}");

        let run = |value: serde_json::Value| {
            let page = store.requests(&query(value));
            page.items
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<String>>()
        };
        assert_eq!(run(json!({"model": "unknown"})), ["no-model"]);
        assert_eq!(run(json!({"model": "UNKNOWN"})), ["no-model"]);
        assert_eq!(
            run(json!({"model": "unknown", "provider": "unknown", "key": "anonymous"})),
            ["no-model"]
        );
        assert_eq!(run(json!({"model": "gpt-5"})), ["with-model"]);
        assert_eq!(run(json!({"model": "unknown", "status": "ok"})).len(), 0);

        // A model that is really called "unknown" is selected by its name
        // as any other model is.
        let mut named = Spec {
            id: "named-unknown".to_string(),
            model: "unknown",
            provider: Some("openai"),
            key: None,
            started_at: T0 + 2_000,
            duration_ms: 10,
            status: 200,
        }
        .build();
        named.client_model = None;
        store.record(&named);
        assert_eq!(
            run(json!({"model": "unknown"})),
            ["named-unknown", "no-model"]
        );
    }

    /// Regression: the list holds the newest N finished requests and
    /// `total` stops there, but nothing in the answer said what N is.
    #[test]
    fn the_page_says_how_many_requests_the_list_can_hold() {
        let store = UsageStore::new(UsageStoreOptions {
            recent_capacity: 4,
            ..UsageStoreOptions::default()
        });
        let empty = store.requests(&RequestQuery::default());
        assert_eq!((empty.total, empty.capacity), (0, 4));
        for i in 0..9 {
            store.record(&simple(&format!("r{i}"), T0 + i * 1_000, 200));
        }
        let page = store.requests(&RequestQuery::default());
        assert_eq!((page.total, page.capacity), (4, 4));
        // Whatever the filter or the cursor, an unusable cursor included.
        for value in [
            json!({"model": "nope"}),
            json!({"limit": 1}),
            json!({"before": "not-a-cursor"}),
        ] {
            assert_eq!(store.requests(&query(value)).capacity, 4);
        }
        assert_eq!(serde_json::to_value(&page).unwrap()["capacity"], json!(4));
        assert_eq!(
            UsageStore::in_memory()
                .requests(&RequestQuery::default())
                .capacity,
            DEFAULT_RECENT_CAPACITY
        );
        // Clearing the statistics does not change what the list can hold.
        store.clear();
        assert_eq!(store.requests(&RequestQuery::default()).capacity, 4);
    }

    #[test]
    fn pagination_walks_everything_exactly_once() {
        let store = UsageStore::in_memory();
        for i in 0..23 {
            // Pairs share a start time, so the id has to break ties.
            store.record(&simple(&format!("r{i:02}"), T0 + (i / 2) * 1_000, 200));
        }
        let mut seen = Vec::new();
        let mut before: Option<String> = None;
        let mut pages = 0;
        loop {
            let page = store.requests(&RequestQuery {
                limit: Some(5),
                before: before.clone(),
                ..RequestQuery::default()
            });
            assert_eq!(page.total, 23);
            seen.extend(page.items.iter().map(|r| r.id.clone()));
            pages += 1;
            assert_eq!(page.has_more, page.next_before.is_some());
            match page.next_before {
                Some(next) => before = Some(next),
                None => break,
            }
        }
        assert_eq!(pages, 5);
        let expected: Vec<String> = (0..23).rev().map(|i| format!("r{i:02}")).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn pagination_is_stable_while_requests_arrive() {
        let store = UsageStore::in_memory();
        for i in 0..10 {
            store.record(&simple(&format!("r{i:02}"), T0 + i * 1_000, 200));
        }
        let first = store.requests(&query(json!({"limit": 4})));
        assert_eq!(ids(&first), ["r09", "r08", "r07", "r06"]);
        assert_eq!(
            first.next_before.as_deref(),
            Some(&*format!("{}:r06", T0 + 6_000))
        );
        // New requests arrive between the two page loads.
        for i in 10..15 {
            store.record(&simple(&format!("r{i:02}"), T0 + i * 1_000, 200));
        }
        let second = store.requests(&query(json!({"limit": 4, "before": first.next_before})));
        assert_eq!(ids(&second), ["r05", "r04", "r03", "r02"]);
        let third = store.requests(&query(json!({"limit": 4, "before": second.next_before})));
        assert_eq!(ids(&third), ["r01", "r00"]);
        assert!(!third.has_more);
    }

    #[test]
    fn cursor_forms() {
        let store = UsageStore::in_memory();
        for i in 0..6 {
            store.record(&simple(&format!("r{i}"), T0 + i * 1_000, 200));
        }
        // A bare id still in memory.
        assert_eq!(
            ids(&store.requests(&query(json!({"before": "r3"})))),
            ["r2", "r1", "r0"]
        );
        // A bare timestamp: everything that started before it.
        let page = store.requests(&query(json!({"before": (T0 + 2_000).to_string()})));
        assert_eq!(ids(&page), ["r1", "r0"]);
        // A cursor pointing at a record that has since been evicted.
        let page = store.requests(&query(json!({"before": format!("{}:gone", T0 + 4_500)})));
        assert_eq!(ids(&page), ["r4", "r3", "r2", "r1", "r0"]);
        // Garbage yields nothing instead of the first page.
        let page = store.requests(&query(json!({"before": "garbage"})));
        assert!(page.items.is_empty() && !page.has_more && page.next_before.is_none());
        // Filters and cursors combine; `total` ignores the cursor.
        store.record(&simple("bad", T0 + 2_500, 503));
        let page = store.requests(&query(json!({"status": "ok", "before": "r3", "limit": 2})));
        assert_eq!(ids(&page), ["r2", "r1"]);
        assert_eq!(page.total, 6);
        assert!(page.has_more);
    }

    #[test]
    fn recording_the_same_id_again_replaces_the_ring_entry() {
        let store = UsageStore::in_memory();
        store.record(&simple("dup", T0, 200));
        store.record(&simple("dup", T0 + 5, 500));
        let page = store.requests(&RequestQuery::default());
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].status, 500);
        assert_eq!(store.get("dup").unwrap().started_at, T0 + 5);
    }

    #[test]
    fn page_json_shape() {
        let store = UsageStore::in_memory();
        store.record(&simple("a", T0, 200));
        store.record(&simple("b", T0 + 1, 200));
        let value = serde_json::to_value(store.requests(&query(json!({"limit": 1})))).unwrap();
        assert_eq!(value["items"].as_array().unwrap().len(), 1);
        assert_eq!(value["items"][0]["id"], "b");
        assert_eq!(value["next_before"], format!("{}:b", T0 + 1));
        assert_eq!(value["has_more"], true);
        assert_eq!(value["total"], 2);
    }

    #[test]
    fn disabled_store_records_nothing() {
        let store = UsageStore::new(UsageStoreOptions {
            enabled: false,
            ..UsageStoreOptions::default()
        });
        store.record(&simple("a", T0, 200));
        assert_eq!(store.info(), UsageStoreInfo::default());
        store.set_enabled(true);
        store.record(&simple("b", T0, 200));
        assert_eq!(store.info().recent, 1);
    }

    #[test]
    fn clear_forgets_everything() {
        let (store, _) = filled(30);
        store.clear();
        assert_eq!(store.info(), UsageStoreInfo::default());
        assert_eq!(store.summary(Range::Month, NOW).totals, Totals::default());
        assert!(store.requests(&RequestQuery::default()).items.is_empty());
        // Still usable, with the same ring size.
        store.record(&simple("again", NOW, 200));
        assert_eq!(store.summary(Range::Hour, NOW + 1_000).totals.requests, 1);
    }

    // ------------------------------------------------------------------
    // Persistence
    // ------------------------------------------------------------------

    fn persistent(dir: &Path, retention_days: u32) -> UsageStore {
        UsageStore::new(UsageStoreOptions {
            retention_days,
            persist_dir: Some(dir.to_path_buf()),
            ..UsageStoreOptions::default()
        })
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn records_survive_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("usage");
        let store = persistent(&dir, 30);
        let records = three_days();
        for record in &records {
            store.record(record);
        }
        // Nothing touches the disk until the writer runs.
        assert!(!dir.exists());
        assert_eq!(store.info().pending_writes, 432);
        assert_eq!(store.flush_blocking().unwrap(), 432);
        assert_eq!(store.info().pending_writes, 0);
        assert_eq!(
            file_names(&dir),
            ["2026-10-02.jsonl", "2026-10-03.jsonl", "2026-10-04.jsonl"]
        );

        let restarted = persistent(&dir, 30);
        let report = restarted.load(&dir, 30, NOW);
        assert_eq!(
            report,
            LoadReport {
                files: 3,
                records: 432,
                skipped_lines: 0
            }
        );
        for range in Range::ALL {
            assert_eq!(
                restarted.summary(range, NOW),
                store.summary(range, NOW),
                "{range}"
            );
        }
        assert_eq!(
            restarted.timeseries(Range::Week, BucketSize::Auto, GroupBy::Model, NOW),
            store.timeseries(Range::Week, BucketSize::Auto, GroupBy::Model, NOW)
        );
        let page = restarted.requests(&query(json!({"limit": 3})));
        assert_eq!(ids(&page), ["req-0431", "req-0430", "req-0429"]);
        assert_eq!(page.total, 432);
        assert_eq!(*restarted.get("req-0007").unwrap(), records[7]);
        // Loading does not queue the records for writing again.
        assert_eq!(restarted.info().pending_writes, 0);
        assert_eq!(restarted.flush_blocking().unwrap(), 0);
    }

    #[test]
    fn load_tolerates_corrupt_and_truncated_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("usage");
        fs::create_dir_all(&dir).unwrap();
        let line = |id: &str, at: i64| serde_json::to_string(&simple(id, at, 200)).unwrap();
        let cut = line("cut", T0 + 3_000);
        let text = format!(
            "{}\n{{\"id\": \"broken\n\n[1,2,3]\n{}\n{}",
            line("a", T0 + 1_000),
            line("b", T0 + 2_000),
            &cut[..cut.len() - 20]
        );
        fs::write(dir.join("2026-10-02.jsonl"), text).unwrap();
        // A record dated far in the future must not wipe the real history.
        fs::write(
            dir.join("2026-10-03.jsonl"),
            line("future", T0 + 400 * DAY_MS),
        )
        .unwrap();

        let store = persistent(&dir, 30);
        let report = store.load(&dir, 30, T0 + DAY_MS);
        assert_eq!(
            report,
            LoadReport {
                files: 2,
                records: 2,
                skipped_lines: 4
            }
        );
        assert_eq!(store.summary(Range::Week, T0 + DAY_MS).totals.requests, 2);
        assert_eq!(ids(&store.requests(&RequestQuery::default())), ["b", "a"]);

        // New records are appended on a fresh line after the truncated one.
        store.record(&simple("c", T0 + 4_000, 200));
        store.flush_blocking().unwrap();
        let again = persistent(&dir, 30);
        assert_eq!(again.load(&dir, 30, T0 + DAY_MS).records, 3);
        assert_eq!(
            ids(&again.requests(&RequestQuery::default())),
            ["c", "b", "a"]
        );
    }

    #[test]
    fn load_skips_files_beyond_retention_and_prune_deletes_them() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("usage");
        let store = persistent(&dir, 30);
        for day in 0..10 {
            store.record(&simple(
                &format!("d{day}"),
                T0 + day * DAY_MS + HOUR_MS,
                200,
            ));
        }
        store.flush_blocking().unwrap();
        assert_eq!(file_names(&dir).len(), 10);
        let now = T0 + 9 * DAY_MS + 2 * HOUR_MS;

        // A restart with a three-day retention reads today and the three
        // days before it.
        let short = persistent(&dir, 3);
        let report = short.load(&dir, 3, now);
        assert_eq!((report.files, report.records), (4, 4));
        assert_eq!(
            ids(&short.requests(&RequestQuery::default())),
            ["d9", "d8", "d7", "d6"]
        );
        // Hourly statistics cover exactly 72 hours back from the newest.
        assert_eq!(short.summary(Range::Month, now).totals.requests, 3);

        assert_eq!(short.prune(now), 6);
        assert_eq!(
            file_names(&dir),
            [
                "2026-10-08.jsonl",
                "2026-10-09.jsonl",
                "2026-10-10.jsonl",
                "2026-10-11.jsonl"
            ]
        );
        assert_eq!(short.prune(now), 0);
    }

    #[test]
    fn find_falls_back_to_the_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("usage");
        let store = UsageStore::new(UsageStoreOptions {
            recent_capacity: 2,
            persist_dir: Some(dir.clone()),
            ..UsageStoreOptions::default()
        });
        let ids: Vec<String> = (0..4).map(|_| crate::record::new_request_id()).collect();
        let now = switchyard_core::util::now_unix_ms();
        for (i, id) in ids.iter().enumerate() {
            store.record(&simple(id, now + i as i64, 200));
        }
        store.flush_blocking().unwrap();
        assert!(store.get(&ids[0]).is_none());
        assert_eq!(store.find(&ids[0]).unwrap().id, ids[0]);
        assert_eq!(store.find(&ids[3]).unwrap().id, ids[3]);
        assert!(store.find(&crate::record::new_request_id()).is_none());
        assert!(store.find("not-a-uuid").is_none());
        // Without persistence only memory is searched.
        store.set_persist_dir(None);
        assert!(store.find(&ids[0]).is_none());
    }

    #[test]
    fn clear_deletes_the_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("usage");
        let store = persistent(&dir, 30);
        store.record(&simple("a", T0, 200));
        store.flush_blocking().unwrap();
        store.record(&simple("b", T0 + DAY_MS, 200));
        store.clear();
        assert!(file_names(&dir).is_empty());
        assert_eq!(store.flush_blocking().unwrap(), 0);
        assert_eq!(persistent(&dir, 30).load(&dir, 30, T0 + DAY_MS).records, 0);
    }

    #[test]
    fn switching_persistence_off_writes_what_was_queued() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("usage");
        let store = persistent(&dir, 30);
        assert_eq!(store.persist_dir().as_deref(), Some(dir.as_path()));
        store.record(&simple("a", T0, 200));
        // Same directory: nothing changes, the queue stays.
        store.set_persist_dir(Some(dir.clone()));
        assert_eq!(store.info().pending_writes, 1);
        store.set_persist_dir(None);
        assert_eq!(store.persist_dir(), None);
        assert_eq!(file_names(&dir), ["2026-10-02.jsonl"]);
        // Memory only from here on.
        store.record(&simple("b", T0 + 1, 200));
        assert_eq!(store.flush_blocking().unwrap(), 0);
        assert_eq!(store.info().recent, 2);
        let other = tmp.path().join("elsewhere");
        store.set_persist_dir(Some(other.clone()));
        store.record(&simple("c", T0 + 2, 200));
        assert_eq!(store.flush_blocking().unwrap(), 1);
        assert_eq!(file_names(&other), ["2026-10-02.jsonl"]);
    }

    #[test]
    fn a_broken_directory_does_not_break_recording() {
        let tmp = tempfile::tempdir().unwrap();
        // A file where the directory should be.
        let blocker = tmp.path().join("usage");
        fs::write(&blocker, "not a directory").unwrap();
        let store = persistent(&blocker, 30);
        store.record(&simple("a", T0, 200));
        assert!(store.flush_blocking().is_err());
        // The batch is dropped rather than retried forever.
        assert_eq!(store.info().pending_writes, 0);
        assert_eq!(store.summary(Range::Hour, T0 + 1_000).totals.requests, 1);
        assert_eq!(store.load(&blocker, 30, T0), LoadReport::default());
    }

    #[tokio::test]
    async fn async_flush_writes_the_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("usage");
        let store = persistent(&dir, 30);
        assert_eq!(store.flush().await.unwrap(), 0);
        store.record(&simple("a", T0, 200));
        store.record(&simple("b", T0 + 1, 200));
        assert_eq!(store.flush().await.unwrap(), 2);
        assert_eq!(file_names(&dir), ["2026-10-02.jsonl"]);
        assert_eq!(UsageStore::in_memory().flush().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn background_writer_flushes_about_every_second() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("usage");
        let store = persistent(&dir, 30);
        let writer = store.spawn_writer(&Handle::current());
        // Let the interval's immediate first tick pass, so the record below
        // is written by a timed flush rather than by that first one.
        tokio::time::sleep(Duration::from_millis(50)).await;
        store.record(&simple("a", T0, 200));
        // No explicit flush: the file appears once the interval has fired.
        for _ in 0..400 {
            if store.info().pending_writes == 0 && dir.join("2026-10-02.jsonl").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(file_names(&dir), ["2026-10-02.jsonl"]);
        assert_eq!(persistent(&dir, 30).load(&dir, 30, T0 + 1_000).records, 1);

        // Dropping the last handle ends the task after saving the rest.
        store.record(&simple("b", T0 + 1, 200));
        drop(store);
        tokio::time::timeout(Duration::from_secs(30), writer)
            .await
            .expect("writer task ends when the store is dropped")
            .unwrap();
        assert_eq!(persistent(&dir, 30).load(&dir, 30, T0 + 1_000).records, 2);
    }
}
