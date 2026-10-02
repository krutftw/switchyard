//! Application log capture: a `tracing` layer turns every event into a
//! [`LogLine`], keeps the most recent ones in a ring for `GET /logs`,
//! publishes them on the event bus for the live tail and, optionally, hands
//! them to a rotating [`FileSink`].
//!
//! Nothing in this module emits log events itself: a capture layer that
//! logged would feed on its own output.

use crate::bus::{Event, EventBus};
use crate::redact::{is_secret_key, redact_secret_value, redact_text};
use crate::time::{day_index, parse_day, rfc3339, utc_day};
use crate::usage::lenient;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use switchyard_core::util::{now_unix_ms, truncate_chars};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Log lines kept in memory.
pub const DEFAULT_LOG_CAPACITY: usize = 2_000;
/// Default number of lines returned by a log query.
pub const DEFAULT_LOG_PAGE: usize = 200;
/// Sub-directory of the data dir holding log files.
pub const LOGS_DIR: &str = "logs";

/// Longest message kept, in characters.
const MAX_MESSAGE_CHARS: usize = 4_000;
/// Longest string field value kept, in characters.
const MAX_FIELD_CHARS: usize = 1_000;
/// Most fields kept per line.
const MAX_FIELDS: usize = 48;

// ---------------------------------------------------------------------------
// Levels and lines
// ---------------------------------------------------------------------------

/// Severity of a log line, least severe first.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub const ALL: [LogLevel; 5] = [
        LogLevel::Trace,
        LogLevel::Debug,
        LogLevel::Info,
        LogLevel::Warn,
        LogLevel::Error,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }

    fn from_index(index: u8) -> LogLevel {
        LogLevel::ALL
            .get(usize::from(index))
            .copied()
            .unwrap_or(LogLevel::Error)
    }
}

impl From<Level> for LogLevel {
    fn from(level: Level) -> Self {
        match level {
            Level::TRACE => LogLevel::Trace,
            Level::DEBUG => LogLevel::Debug,
            Level::INFO => LogLevel::Info,
            Level::WARN => LogLevel::Warn,
            Level::ERROR => LogLevel::Error,
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "trace" => Ok(LogLevel::Trace),
            "debug" => Ok(LogLevel::Debug),
            "info" => Ok(LogLevel::Info),
            "warn" | "warning" => Ok(LogLevel::Warn),
            "error" | "err" | "fatal" => Ok(LogLevel::Error),
            other => Err(format!(
                "unknown log level `{other}` (expected trace, debug, info, warn or error)"
            )),
        }
    }
}

/// One application log event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    /// Position in the log buffer, increasing by one per line; `0` until the
    /// line has been pushed. The cursor for `GET /logs?before=`.
    #[serde(default)]
    pub seq: u64,
    /// Unix milliseconds.
    pub at: i64,
    /// `trace`, `debug`, `info`, `warn` or `error`.
    pub level: String,
    /// Module path of the code that logged, e.g. `switchyard_gateway::pipeline`.
    pub target: String,
    pub message: String,
    /// Structured fields of the event and of the spans it happened in.
    #[serde(default)]
    pub fields: Map<String, Value>,
}

impl LogLine {
    pub fn new(
        at: i64,
        level: impl Into<String>,
        target: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        LogLine {
            seq: 0,
            at,
            level: level.into(),
            target: target.into(),
            message: message.into(),
            fields: Map::new(),
        }
    }

    pub fn with_field(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.fields.insert(name.into(), value.into());
        self
    }

    /// The parsed level; unknown spellings count as `info`.
    pub fn severity(&self) -> LogLevel {
        self.level.parse().unwrap_or_default()
    }

    /// The line as written to log files:
    /// `2026-10-02T13:09:12.123Z  INFO target: message key=value`.
    /// Line breaks in the message are escaped so one event is one line.
    pub fn to_text(&self) -> String {
        let mut out = format!(
            "{} {:>5} {}: {}",
            rfc3339(self.at),
            self.level.to_ascii_uppercase(),
            self.target,
            self.message.replace('\r', "").replace('\n', "\\n"),
        );
        for (name, value) in &self.fields {
            out.push(' ');
            out.push_str(name);
            out.push('=');
            match value {
                Value::String(text) if is_bare(text) => out.push_str(text),
                other => out.push_str(&other.to_string()),
            }
        }
        out
    }

    fn matches_text(&self, needle: &str) -> bool {
        let hit = |text: &str| text.to_lowercase().contains(needle);
        hit(&self.message)
            || hit(&self.target)
            || self.fields.iter().any(|(name, value)| {
                hit(name)
                    || match value {
                        Value::String(text) => hit(text),
                        other => hit(&other.to_string()),
                    }
            })
    }
}

/// Whether a field value can be written without quotes.
fn is_bare(text: &str) -> bool {
    !text.is_empty()
        && !text
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '=')
}

// ---------------------------------------------------------------------------
// Ring buffer
// ---------------------------------------------------------------------------

/// Filters and paging of `GET /logs`. Deserialises straight from a query
/// string; empty values count as absent and garbage is ignored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LogQuery {
    /// Page size; defaults to [`DEFAULT_LOG_PAGE`].
    #[serde(deserialize_with = "lenient::opt_usize")]
    pub limit: Option<usize>,
    /// Least severe level to return.
    #[serde(deserialize_with = "opt_level")]
    pub level: Option<LogLevel>,
    /// Case-insensitive substring searched in the message, the target and
    /// the fields.
    #[serde(deserialize_with = "lenient::opt_string")]
    pub q: Option<String>,
    /// Cursor: only lines with a smaller `seq`.
    #[serde(deserialize_with = "lenient::opt_u64")]
    pub before: Option<u64>,
}

fn opt_level<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<LogLevel>, D::Error> {
    Ok(lenient::opt_string(d)?.and_then(|text| text.parse().ok()))
}

/// One page of log lines, oldest first (newest last).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LogPage {
    pub lines: Vec<Arc<LogLine>>,
    /// Pass as `before` to get the next older page; `null` when there is
    /// nothing older that matches.
    pub next_before: Option<u64>,
    pub has_more: bool,
}

struct Ring {
    lines: VecDeque<Arc<LogLine>>,
    capacity: usize,
    next_seq: u64,
}

struct BufferInner {
    ring: Mutex<Ring>,
    min_level: AtomicU8,
    sink: RwLock<Option<Arc<FileSink>>>,
}

/// The most recent log lines. Cloning is cheap; clones share the buffer.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<BufferInner>,
}

impl fmt::Debug for LogBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogBuffer")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .field("min_level", &self.min_level())
            .finish()
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        LogBuffer::new(DEFAULT_LOG_CAPACITY)
    }
}

impl LogBuffer {
    /// A buffer keeping the last `capacity` lines (at least one).
    pub fn new(capacity: usize) -> Self {
        LogBuffer {
            inner: Arc::new(BufferInner {
                ring: Mutex::new(Ring {
                    lines: VecDeque::new(),
                    capacity: capacity.max(1),
                    next_seq: 1,
                }),
                min_level: AtomicU8::new(LogLevel::Trace as u8),
                sink: RwLock::new(None),
            }),
        }
    }

    pub fn capacity(&self) -> usize {
        self.inner.ring.lock().capacity
    }

    pub fn len(&self) -> usize {
        self.inner.ring.lock().lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Least severe level the capture layer records. Defaults to `trace`,
    /// i.e. everything the subscriber lets through.
    pub fn min_level(&self) -> LogLevel {
        LogLevel::from_index(self.inner.min_level.load(Ordering::Relaxed))
    }

    pub fn set_min_level(&self, level: LogLevel) {
        self.inner.min_level.store(level as u8, Ordering::Relaxed);
    }

    /// Attaches (or detaches) the file sink every pushed line is also
    /// queued for.
    pub fn set_file_sink(&self, sink: Option<Arc<FileSink>>) {
        *self.inner.sink.write() = sink;
    }

    pub fn file_sink(&self) -> Option<Arc<FileSink>> {
        self.inner.sink.read().clone()
    }

    /// Appends a line, assigning its `seq`, and returns the stored line.
    /// The oldest line is dropped when the buffer is full.
    pub fn push(&self, mut line: LogLine) -> Arc<LogLine> {
        let line = {
            let mut ring = self.inner.ring.lock();
            line.seq = ring.next_seq;
            ring.next_seq += 1;
            let line = Arc::new(line);
            if ring.lines.len() >= ring.capacity {
                ring.lines.pop_front();
            }
            ring.lines.push_back(Arc::clone(&line));
            line
        };
        if let Some(sink) = self.file_sink() {
            sink.enqueue(Arc::clone(&line));
        }
        line
    }

    /// Forgets every buffered line. Sequence numbers keep increasing.
    pub fn clear(&self) {
        self.inner.ring.lock().lines.clear();
    }

    /// The last `limit` lines matching the filters, oldest first.
    ///
    /// * `min_level`: least severe level to include;
    /// * `q`: case-insensitive substring of the message, target or fields;
    /// * `before`: only lines with a smaller `seq`.
    pub fn query(
        &self,
        limit: usize,
        min_level: Option<LogLevel>,
        q: Option<&str>,
        before: Option<u64>,
    ) -> Vec<Arc<LogLine>> {
        self.select(limit, min_level, q, before).lines
    }

    /// [`query`](LogBuffer::query) driven by a parsed query string, with the
    /// cursor for the next older page.
    pub fn page(&self, query: &LogQuery) -> LogPage {
        self.select(
            query.limit.unwrap_or(DEFAULT_LOG_PAGE),
            query.level,
            query.q.as_deref(),
            query.before,
        )
    }

    fn select(
        &self,
        limit: usize,
        min_level: Option<LogLevel>,
        q: Option<&str>,
        before: Option<u64>,
    ) -> LogPage {
        let needle = q
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);
        let ring = self.inner.ring.lock();
        let limit = limit.clamp(1, ring.capacity);
        let mut lines = Vec::new();
        let mut has_more = false;
        for line in ring.lines.iter().rev() {
            if before.is_some_and(|before| line.seq >= before) {
                continue;
            }
            if min_level.is_some_and(|min| line.severity() < min) {
                continue;
            }
            if needle
                .as_deref()
                .is_some_and(|needle| !line.matches_text(needle))
            {
                continue;
            }
            if lines.len() == limit {
                has_more = true;
                break;
            }
            lines.push(Arc::clone(line));
        }
        lines.reverse();
        let next_before = match (has_more, lines.first()) {
            (true, Some(oldest)) => Some(oldest.seq),
            _ => None,
        };
        LogPage {
            lines,
            next_before,
            has_more,
        }
    }
}

// ---------------------------------------------------------------------------
// tracing layer
// ---------------------------------------------------------------------------

/// Stores a field value, redacting it when the name or the content looks
/// like a secret.
fn field_value(name: &str, value: Value) -> Value {
    match value {
        Value::String(text) => {
            let text = if is_secret_key(name) {
                redact_secret_value(name, &text)
            } else {
                redact_text(&text)
            };
            Value::String(truncate_chars(&text, MAX_FIELD_CHARS))
        }
        other => other,
    }
}

/// Collects the fields of an event or span.
#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    /// Set from the `log.target` field of events bridged from the `log`
    /// crate, whose own metadata target is just `log`.
    target: Option<String>,
    fields: Map<String, Value>,
}

impl FieldVisitor {
    fn put(&mut self, field: &Field, value: Value) {
        let name = field.name();
        match name {
            "message" => {
                self.message = Some(match value {
                    Value::String(text) => text,
                    other => other.to_string(),
                });
            }
            "log.target" => {
                if let Value::String(target) = value {
                    self.target = Some(target);
                }
            }
            // Source location of bridged `log` records: noise.
            _ if name.starts_with("log.") => {}
            _ => {
                if self.fields.len() < MAX_FIELDS || self.fields.contains_key(name) {
                    self.fields
                        .insert(name.to_string(), field_value(name, value));
                }
            }
        }
    }
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, Value::String(format!("{value:?}")));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, Value::String(value.to_string()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        match i64::try_from(value) {
            Ok(value) => self.put(field, Value::from(value)),
            Err(_) => self.put(field, Value::String(value.to_string())),
        }
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        match u64::try_from(value) {
            Ok(value) => self.put(field, Value::from(value)),
            Err(_) => self.put(field, Value::String(value.to_string())),
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        // NaN and infinities have no JSON number; keep them readable.
        let value = serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(value.to_string()));
        self.put(field, value);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::Bool(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, Value::String(value.to_string()));
    }
}

/// Fields of a span, kept in the span's extensions so events inside it can
/// carry them (a request id set on the request span ends up on every line).
struct SpanFields(Map<String, Value>);

thread_local! {
    /// Set while this thread is inside the capture layer.
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
}

/// Clears [`CAPTURING`] when the layer is left, also on unwind.
struct CaptureGuard;

impl CaptureGuard {
    /// `None` when this thread is already capturing an event.
    fn enter() -> Option<CaptureGuard> {
        CAPTURING.with(|flag| {
            if flag.get() {
                None
            } else {
                flag.set(true);
                Some(CaptureGuard)
            }
        })
    }
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        CAPTURING.with(|flag| flag.set(false));
    }
}

struct CaptureLayer {
    buffer: LogBuffer,
    bus: EventBus,
}

impl<S> Layer<S> for CaptureLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);
        // `replace`, not `insert`: inserting panics when the slot is taken,
        // which a second capture layer on the same registry would cause.
        span.extensions_mut().replace(SpanFields(visitor.fields));
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut visitor = FieldVisitor::default();
        values.record(&mut visitor);
        let mut extensions = span.extensions_mut();
        match extensions.get_mut::<SpanFields>() {
            Some(fields) => fields.0.extend(visitor.fields),
            None => {
                extensions.replace(SpanFields(visitor.fields));
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let level = LogLevel::from(*metadata.level());
        if level < self.buffer.min_level() {
            return;
        }
        // An event raised while this thread is already capturing one (from
        // a `Debug` impl that logs, say) is dropped rather than recursed
        // into.
        let Some(_guard) = CaptureGuard::enter() else {
            return;
        };

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let mut fields = Map::new();
        if let Some(scope) = ctx.event_scope(event) {
            // Outermost span first, so inner spans and finally the event
            // itself win when names collide.
            for span in scope.from_root() {
                if let Some(span_fields) = span.extensions().get::<SpanFields>() {
                    for (name, value) in &span_fields.0 {
                        fields.insert(name.clone(), value.clone());
                    }
                }
            }
        }
        fields.extend(visitor.fields);

        let message = visitor.message.unwrap_or_default();
        let line = LogLine {
            seq: 0,
            at: now_unix_ms(),
            level: level.as_str().to_string(),
            target: visitor
                .target
                .unwrap_or_else(|| metadata.target().to_string()),
            message: truncate_chars(&redact_text(&message), MAX_MESSAGE_CHARS),
            fields,
        };
        let line = self.buffer.push(line);
        self.bus.publish(Event::Log(line));
    }
}

/// A `tracing_subscriber` layer that records every event into `buffer` and
/// publishes it on `bus` as [`Event::Log`].
///
/// Secrets are redacted on the way in: the message and string fields go
/// through [`redact_text`], and fields named like a secret (`api_key`,
/// `authorization`, …) are masked whole. Fields of enclosing spans are
/// attached to each line.
///
/// The layer never logs and never blocks on I/O. Events below
/// [`LogBuffer::min_level`] are skipped, but the layer gives the subscriber
/// no level hint (the level can change at runtime), so on its own it keeps
/// every callsite enabled; combine it with a filtered layer or a global
/// level filter if trace-level callsites should stay disabled. Code that reacts to [`Event::Log`]
/// must not log at a level this layer records, or every line would produce
/// another.
pub fn capture_layer<S>(buffer: LogBuffer, bus: EventBus) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    CaptureLayer { buffer, bus }
}

// ---------------------------------------------------------------------------
// File sink
// ---------------------------------------------------------------------------

const LOG_PREFIX: &str = "switchyard-";
const LOG_SUFFIX: &str = ".log";
/// Lines queued beyond this are dropped (and counted) instead of growing
/// the queue while the disk is stuck.
const MAX_PENDING_LINES: usize = 50_000;
const MIN_PART_BYTES: u64 = 64 * 1024;
const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;

/// A log file on disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LogFile {
    pub name: String,
    pub size: u64,
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    order: (i64, u32),
}

/// File name of a day's log; parts after the first carry a number.
fn log_file_name(day: &str, part: u32) -> String {
    if part == 0 {
        format!("{LOG_PREFIX}{day}{LOG_SUFFIX}")
    } else {
        format!("{LOG_PREFIX}{day}.{part}{LOG_SUFFIX}")
    }
}

/// `(day index, part)` of a log file name; `None` for foreign files.
fn parse_log_file_name(name: &str) -> Option<(i64, u32)> {
    let stem = name.strip_prefix(LOG_PREFIX)?.strip_suffix(LOG_SUFFIX)?;
    match stem.split_once('.') {
        Some((day, part)) => Some((parse_day(day)?, part.parse().ok().filter(|p| *p > 0)?)),
        None => Some((parse_day(stem)?, 0)),
    }
}

#[derive(Default)]
struct SinkState {
    /// UTC day of the file being appended to.
    day: String,
    part: u32,
}

/// Writes log lines to `<data_dir>/logs/switchyard-YYYY-MM-DD.log` (UTC
/// day of the line).
///
/// Lines are queued by [`enqueue`](FileSink::enqueue) and written by
/// [`flush`](FileSink::flush), which the telemetry background task calls
/// about once a second. When the directory grows beyond the size cap the
/// oldest files are deleted; so that one busy day cannot outgrow the cap by
/// itself, a day's file is continued in numbered parts
/// (`switchyard-YYYY-MM-DD.1.log`, …) once it reaches a quarter of the cap.
pub struct FileSink {
    dir: PathBuf,
    max_total_bytes: AtomicU64,
    pending: Mutex<Vec<Arc<LogLine>>>,
    state: Mutex<SinkState>,
    dropped: AtomicU64,
    write_errors: AtomicU64,
}

impl fmt::Debug for FileSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileSink")
            .field("dir", &self.dir)
            .field("max_total_size_mb", &self.max_total_size_mb())
            .finish()
    }
}

impl FileSink {
    /// A sink writing under `<data_dir>/logs`. `max_total_size_mb = 0`
    /// keeps every file.
    pub fn new(data_dir: &Path, max_total_size_mb: u64) -> Self {
        FileSink {
            dir: data_dir.join(LOGS_DIR),
            max_total_bytes: AtomicU64::new(max_total_size_mb.saturating_mul(1024 * 1024)),
            pending: Mutex::new(Vec::new()),
            state: Mutex::new(SinkState::default()),
            dropped: AtomicU64::new(0),
            write_errors: AtomicU64::new(0),
        }
    }

    /// `<data_dir>/logs`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn max_total_size_mb(&self) -> u64 {
        self.max_total_bytes.load(Ordering::Relaxed) / (1024 * 1024)
    }

    pub fn set_max_total_size_mb(&self, max_total_size_mb: u64) {
        self.set_max_total_bytes(max_total_size_mb.saturating_mul(1024 * 1024));
    }

    /// The cap in bytes; `0` keeps every file.
    pub fn set_max_total_bytes(&self, bytes: u64) {
        self.max_total_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Lines dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Flushes that failed. The sink cannot report a broken disk through
    /// the log it is writing, so it counts instead.
    pub fn write_errors(&self) -> u64 {
        self.write_errors.load(Ordering::Relaxed)
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.lock().is_empty()
    }

    /// Queues a line for the next flush. Never touches the disk.
    pub fn enqueue(&self, line: Arc<LogLine>) {
        let mut pending = self.pending.lock();
        if pending.len() >= MAX_PENDING_LINES {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            pending.push(line);
        }
    }

    /// Size at which a day's file is continued in a new part.
    fn part_limit(&self) -> u64 {
        match self.max_total_bytes.load(Ordering::Relaxed) {
            0 => MAX_PART_BYTES,
            cap => (cap / 4).clamp(MIN_PART_BYTES, MAX_PART_BYTES),
        }
    }

    /// Log files in the directory, oldest first.
    pub fn files(&self) -> Vec<LogFile> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut files: Vec<LogFile> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_string();
                let order = parse_log_file_name(&name)?;
                let meta = entry.metadata().ok()?;
                meta.is_file().then(|| LogFile {
                    name,
                    size: meta.len(),
                    path: entry.path(),
                    order,
                })
            })
            .collect();
        files.sort_by_key(|file| file.order);
        files
    }

    /// Writes the queued lines (blocking file I/O), then enforces the size
    /// cap. Returns the number of lines written. On an I/O error the batch
    /// is discarded: retrying against a full disk would only grow the queue.
    pub fn flush(&self) -> io::Result<usize> {
        // Taken before the queue is drained so concurrent flushes cannot
        // write their batches out of order.
        let mut state = self.state.lock();
        let batch = std::mem::take(&mut *self.pending.lock());
        if batch.is_empty() {
            return Ok(0);
        }
        let result = self.write_batch(&mut state, &batch);
        if result.is_err() {
            self.write_errors.fetch_add(1, Ordering::Relaxed);
        }
        self.enforce_cap();
        result.map(|()| batch.len())
    }

    fn write_batch(&self, state: &mut SinkState, batch: &[Arc<LogLine>]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let part_limit = self.part_limit();
        let mut start = 0;
        while start < batch.len() {
            // A run of consecutive lines of the same UTC day goes out in one
            // write.
            let day = day_index(batch[start].at);
            let mut text = String::new();
            let mut end = start;
            while end < batch.len() && day_index(batch[end].at) == day {
                text.push_str(&batch[end].to_text());
                text.push('\n');
                end += 1;
            }
            self.append(state, &utc_day(batch[start].at), &text, part_limit)?;
            start = end;
        }
        Ok(())
    }

    fn append(
        &self,
        state: &mut SinkState,
        day: &str,
        text: &str,
        part_limit: u64,
    ) -> io::Result<()> {
        if state.day != day {
            // Continue the newest part already on disk for that day.
            let wanted = parse_day(day);
            state.part = self
                .files()
                .iter()
                .filter(|file| Some(file.order.0) == wanted)
                .map(|file| file.order.1)
                .max()
                .unwrap_or(0);
            state.day = day.to_string();
        }
        let mut path = self.dir.join(log_file_name(day, state.part));
        let size = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        if size > 0 && size.saturating_add(text.len() as u64) > part_limit {
            state.part += 1;
            path = self.dir.join(log_file_name(day, state.part));
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(text.as_bytes())?;
        file.flush()
    }

    /// Deletes the oldest log files while the directory exceeds the cap
    /// (blocking file I/O). The newest file — the one being written — is
    /// never deleted. Returns the number of files removed.
    pub fn enforce_cap(&self) -> usize {
        let cap = self.max_total_bytes.load(Ordering::Relaxed);
        if cap == 0 {
            return 0;
        }
        let files = self.files();
        let mut total: u64 = files.iter().map(|file| file.size).sum();
        let mut removed = 0;
        let deletable = files.len().saturating_sub(1);
        for file in files.iter().take(deletable) {
            if total <= cap {
                break;
            }
            if fs::remove_file(&file.path).is_ok() {
                total = total.saturating_sub(file.size);
                removed += 1;
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::broadcast::error::TryRecvError;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    /// 2026-10-02T00:00:00Z.
    const T0: i64 = 1_790_899_200_000;
    const DAY: i64 = 86_400_000;
    const KEY: &str = "sk-proj-abcdefghijklmnopqrstuvwxyz";

    fn line(at: i64, level: &str, message: &str) -> LogLine {
        LogLine::new(at, level, "switchyard::test", message)
    }

    /// Runs `body` with a subscriber made of the capture layer alone.
    fn capture(buffer: &LogBuffer, bus: &EventBus, body: impl FnOnce()) {
        let subscriber = Registry::default().with(capture_layer(buffer.clone(), bus.clone()));
        tracing::subscriber::with_default(subscriber, body);
    }

    fn messages(lines: &[Arc<LogLine>]) -> Vec<&str> {
        lines.iter().map(|l| l.message.as_str()).collect()
    }

    // -- levels and lines ---------------------------------------------------

    #[test]
    fn level_order_and_names() {
        assert!(LogLevel::Trace < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
        for level in LogLevel::ALL {
            assert_eq!(level.as_str().parse::<LogLevel>().unwrap(), level);
            assert_eq!(serde_json::to_value(level).unwrap(), level.as_str());
            assert_eq!(LogLevel::from_index(level as u8), level);
        }
        assert_eq!("WARNING".parse::<LogLevel>().unwrap(), LogLevel::Warn);
        assert_eq!(" Error ".parse::<LogLevel>().unwrap(), LogLevel::Error);
        assert!("loud".parse::<LogLevel>().is_err());
        assert_eq!(LogLevel::from(Level::WARN), LogLevel::Warn);
        assert_eq!(LogLevel::from(Level::TRACE), LogLevel::Trace);
    }

    #[test]
    fn line_json_shape() {
        let l = line(T0, "warn", "slow upstream")
            .with_field("provider", "openai")
            .with_field("elapsed_ms", 1200);
        assert_eq!(
            serde_json::to_value(&l).unwrap(),
            json!({
                "seq": 0,
                "at": T0,
                "level": "warn",
                "target": "switchyard::test",
                "message": "slow upstream",
                "fields": {"provider": "openai", "elapsed_ms": 1200}
            })
        );
        let back: LogLine = serde_json::from_value(serde_json::to_value(&l).unwrap()).unwrap();
        assert_eq!(back, l);
        assert_eq!(l.severity(), LogLevel::Warn);
        assert_eq!(line(0, "shouting", "x").severity(), LogLevel::Info);
    }

    #[test]
    fn line_text_format() {
        let l = line(T0 + 13 * 3_600_000 + 123, "info", "listening")
            .with_field("addr", "127.0.0.1:8317")
            .with_field("tls", false)
            .with_field("note", "two words")
            .with_field("empty", "");
        assert_eq!(
            l.to_text(),
            r#"2026-10-02T13:00:00.123Z  INFO switchyard::test: listening addr=127.0.0.1:8317 tls=false note="two words" empty="""#
        );
        let multi = line(T0, "error", "first\r\nsecond\nthird");
        assert_eq!(
            multi.to_text(),
            r"2026-10-02T00:00:00.000Z ERROR switchyard::test: first\nsecond\nthird"
        );
    }

    // -- ring ---------------------------------------------------------------

    #[test]
    fn ring_is_bounded_and_numbers_lines() {
        let buffer = LogBuffer::new(3);
        assert!(buffer.is_empty());
        for i in 0..5 {
            let stored = buffer.push(line(T0 + i, "info", &format!("m{i}")));
            assert_eq!(stored.seq, i as u64 + 1);
        }
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.capacity(), 3);
        let lines = buffer.query(10, None, None, None);
        assert_eq!(messages(&lines), ["m2", "m3", "m4"]);
        assert_eq!(lines.iter().map(|l| l.seq).collect::<Vec<_>>(), [3, 4, 5]);
        buffer.clear();
        assert!(buffer.is_empty());
        // Numbering continues after a clear, so cursors stay meaningful.
        assert_eq!(buffer.push(line(T0, "info", "again")).seq, 6);
        assert_eq!(LogBuffer::default().capacity(), DEFAULT_LOG_CAPACITY);
        assert_eq!(LogBuffer::new(0).capacity(), 1);
    }

    fn sample_buffer() -> LogBuffer {
        let buffer = LogBuffer::new(100);
        buffer.push(line(T0, "debug", "resolving model").with_field("model", "gpt-5"));
        buffer.push(line(T0 + 1, "info", "request finished").with_field("status", 200));
        buffer
            .push(line(T0 + 2, "warn", "credential cooling down").with_field("provider", "OpenAI"));
        buffer.push(line(T0 + 3, "error", "upstream failed").with_field("status", 502));
        buffer.push(line(T0 + 4, "info", "request finished").with_field("status", 200));
        buffer.push(LogLine::new(
            T0 + 5,
            "trace",
            "hyper::proto",
            "flushed 12 bytes",
        ));
        buffer
    }

    #[test]
    fn query_limit_returns_the_newest_lines_oldest_first() {
        let buffer = sample_buffer();
        let lines = buffer.query(2, None, None, None);
        assert_eq!(messages(&lines), ["request finished", "flushed 12 bytes"]);
        assert_eq!(buffer.query(100, None, None, None).len(), 6);
        // A zero limit still returns one line; a huge one is capped.
        assert_eq!(buffer.query(0, None, None, None).len(), 1);
        assert_eq!(buffer.query(usize::MAX, None, None, None).len(), 6);
    }

    #[test]
    fn query_filters_by_level_text_and_cursor() {
        let buffer = sample_buffer();
        let warn_up = buffer.query(10, Some(LogLevel::Warn), None, None);
        assert_eq!(
            messages(&warn_up),
            ["credential cooling down", "upstream failed"]
        );
        assert_eq!(buffer.query(10, Some(LogLevel::Info), None, None).len(), 4);
        assert_eq!(buffer.query(10, Some(LogLevel::Trace), None, None).len(), 6);

        // Message, target and fields are searched, case-insensitively.
        assert_eq!(
            messages(&buffer.query(10, None, Some("COOLING"), None)),
            ["credential cooling down"]
        );
        assert_eq!(
            messages(&buffer.query(10, None, Some("hyper"), None)),
            ["flushed 12 bytes"]
        );
        assert_eq!(
            messages(&buffer.query(10, None, Some("openai"), None)),
            ["credential cooling down"]
        );
        assert_eq!(
            messages(&buffer.query(10, None, Some("502"), None)),
            ["upstream failed"]
        );
        assert_eq!(buffer.query(10, None, Some("provider"), None).len(), 1);
        assert_eq!(buffer.query(10, None, Some("   "), None).len(), 6);
        assert!(
            buffer
                .query(10, None, Some("nothing like this"), None)
                .is_empty()
        );

        // Cursor: strictly older than the given sequence number.
        let older = buffer.query(10, None, None, Some(3));
        assert_eq!(older.iter().map(|l| l.seq).collect::<Vec<_>>(), [1, 2]);
        let combined = buffer.query(10, Some(LogLevel::Info), Some("finished"), Some(5));
        assert_eq!(combined.iter().map(|l| l.seq).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn pages_walk_backwards() {
        let buffer = LogBuffer::new(100);
        for i in 0..7 {
            buffer.push(line(T0 + i, "info", &format!("m{i}")));
        }
        let query =
            |value: serde_json::Value| -> LogQuery { serde_json::from_value(value).unwrap() };
        let first = buffer.page(&query(json!({"limit": "3"})));
        assert_eq!(messages(&first.lines), ["m4", "m5", "m6"]);
        assert_eq!((first.has_more, first.next_before), (true, Some(5)));
        let second = buffer.page(&query(json!({"limit": 3, "before": first.next_before})));
        assert_eq!(messages(&second.lines), ["m1", "m2", "m3"]);
        let third = buffer.page(&query(json!({"limit": 3, "before": "2"})));
        assert_eq!(messages(&third.lines), ["m0"]);
        assert_eq!((third.has_more, third.next_before), (false, None));

        // Lenient parsing: garbage is ignored, names are case-insensitive.
        let q = query(json!({"limit": "many", "level": "WARN", "q": "", "before": "x"}));
        assert_eq!(
            q,
            LogQuery {
                level: Some(LogLevel::Warn),
                ..LogQuery::default()
            }
        );
        assert_eq!(query(json!({"level": "loud"})).level, None);
        assert_eq!(buffer.page(&LogQuery::default()).lines.len(), 7);

        let value = serde_json::to_value(buffer.page(&query(json!({"limit": 1})))).unwrap();
        assert_eq!(value["lines"][0]["message"], "m6");
        assert_eq!(value["lines"][0]["seq"], 7);
        assert_eq!(value["next_before"], 7);
        assert_eq!(value["has_more"], true);
    }

    // -- layer --------------------------------------------------------------

    #[test]
    fn layer_captures_message_fields_and_levels() {
        let buffer = LogBuffer::default();
        let bus = EventBus::default();
        let before = now_unix_ms();
        capture(&buffer, &bus, || {
            tracing::trace!("very quiet");
            tracing::debug!(attempt = 2, "retrying");
            tracing::info!(
                provider = "openai",
                status = 200u16,
                ok = true,
                ratio = 0.5,
                "request finished"
            );
            tracing::warn!(target: "switchyard::custom", retry_after_ms = 1500i64, "rate limited");
            tracing::error!(error = %"connection reset", detail = ?Some(3), "upstream failed");
        });
        let after = now_unix_ms();
        let lines = buffer.query(10, None, None, None);
        assert_eq!(
            messages(&lines),
            [
                "very quiet",
                "retrying",
                "request finished",
                "rate limited",
                "upstream failed"
            ]
        );
        let levels: Vec<&str> = lines.iter().map(|l| l.level.as_str()).collect();
        assert_eq!(levels, ["trace", "debug", "info", "warn", "error"]);
        assert_eq!(
            lines[1].fields,
            json!({"attempt": 2}).as_object().unwrap().clone()
        );
        assert_eq!(
            Value::Object(lines[2].fields.clone()),
            json!({"provider": "openai", "status": 200, "ok": true, "ratio": 0.5})
        );
        assert_eq!(lines[3].target, "switchyard::custom");
        assert_eq!(lines[3].fields["retry_after_ms"], 1500);
        assert_eq!(
            Value::Object(lines[4].fields.clone()),
            json!({"error": "connection reset", "detail": "Some(3)"})
        );
        assert!(
            lines[0].target.ends_with("logs::tests"),
            "{}",
            lines[0].target
        );
        assert!(lines.iter().all(|l| (before..=after).contains(&l.at)));
        assert_eq!(
            lines.iter().map(|l| l.seq).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn layer_formats_message_arguments() {
        let buffer = LogBuffer::default();
        capture(&buffer, &EventBus::default(), || {
            let port = 8317;
            tracing::info!("listening on {}:{port}", "127.0.0.1");
            tracing::info!(only = "fields");
        });
        let lines = buffer.query(10, None, None, None);
        assert_eq!(lines[0].message, "listening on 127.0.0.1:8317");
        assert!(lines[0].fields.is_empty());
        assert_eq!(lines[1].message, "");
        assert_eq!(lines[1].fields["only"], "fields");
    }

    #[test]
    fn layer_publishes_each_line_on_the_bus() {
        let buffer = LogBuffer::default();
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        capture(&buffer, &bus, || {
            tracing::info!(n = 1, "one");
            tracing::warn!("two");
        });
        let first = rx.try_recv().unwrap();
        assert_eq!(first.topic(), "log");
        let frame = first.to_frame();
        assert_eq!(frame["type"], "log");
        assert_eq!(frame["data"]["message"], "one");
        assert_eq!(frame["data"]["level"], "info");
        assert_eq!(frame["data"]["fields"], json!({"n": 1}));
        assert_eq!(frame["data"]["seq"], 1);
        match rx.try_recv().unwrap() {
            Event::Log(line) => {
                assert_eq!(line.message, "two");
                // The very line that is in the buffer, not a copy.
                assert!(Arc::ptr_eq(&line, &buffer.query(1, None, None, None)[0]));
            }
            other => panic!("unexpected event {other:?}"),
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn layer_redacts_secrets() {
        let buffer = LogBuffer::default();
        capture(&buffer, &EventBus::default(), || {
            tracing::info!(
                api_key = KEY,
                authorization = %format!("Bearer {KEY}"),
                url = %format!("https://host/v1beta/models?key={KEY}"),
                max_tokens = 4096,
                input_tokens = 12u64,
                "calling upstream with {KEY}"
            );
        });
        let line = &buffer.query(1, None, None, None)[0];
        let text = serde_json::to_string(line.as_ref()).unwrap();
        assert!(!text.contains(KEY), "{text}");
        assert_eq!(line.message, "calling upstream with sk-pro…wxyz");
        assert_eq!(line.fields["api_key"], "sk-pro…wxyz");
        assert_eq!(line.fields["authorization"], "Bearer sk-pro…wxyz");
        assert_eq!(
            line.fields["url"],
            "https://host/v1beta/models?key=sk-pro…wxyz"
        );
        // Token counters are numbers and stay.
        assert_eq!(line.fields["max_tokens"], 4096);
        assert_eq!(line.fields["input_tokens"], 12);
    }

    #[test]
    fn layer_attaches_span_fields() {
        let buffer = LogBuffer::default();
        capture(&buffer, &EventBus::default(), || {
            let request = tracing::info_span!(
                "request",
                request_id = "req-1",
                model = tracing::field::Empty
            );
            let _in_request = request.enter();
            tracing::info!("started");
            request.record("model", "gpt-5");
            let attempt =
                tracing::info_span!("attempt", provider = "openai", model = "gpt-5-upstream");
            attempt.in_scope(|| tracing::warn!(status = 429, "attempt failed"));
            tracing::info!(request_id = "overridden", "finished");
        });
        let lines = buffer.query(10, None, None, None);
        assert_eq!(
            Value::Object(lines[0].fields.clone()),
            json!({"request_id": "req-1"})
        );
        assert_eq!(
            Value::Object(lines[1].fields.clone()),
            json!({"request_id": "req-1", "model": "gpt-5-upstream", "provider": "openai", "status": 429})
        );
        assert_eq!(
            Value::Object(lines[2].fields.clone()),
            json!({"request_id": "overridden", "model": "gpt-5"})
        );
    }

    #[test]
    fn layer_respects_the_buffer_level() {
        let buffer = LogBuffer::default();
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        buffer.set_min_level(LogLevel::Warn);
        assert_eq!(buffer.min_level(), LogLevel::Warn);
        capture(&buffer, &bus, || {
            tracing::debug!("dropped");
            tracing::info!("dropped");
            tracing::warn!("kept");
            tracing::error!("kept too");
        });
        assert_eq!(
            messages(&buffer.query(10, None, None, None)),
            ["kept", "kept too"]
        );
        assert_eq!(rx.try_recv().unwrap().topic(), "log");
        assert_eq!(rx.try_recv().unwrap().topic(), "log");
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn layer_bounds_message_and_field_sizes() {
        let buffer = LogBuffer::default();
        capture(&buffer, &EventBus::default(), || {
            let long = "x".repeat(10_000);
            tracing::info!(body = %long, "{long}");
        });
        let line = &buffer.query(1, None, None, None)[0];
        assert_eq!(line.message.chars().count(), MAX_MESSAGE_CHARS + 1);
        assert_eq!(
            line.fields["body"].as_str().unwrap().chars().count(),
            MAX_FIELD_CHARS + 1
        );
    }

    /// Logs from inside its own `Debug` impl, as a careless type might.
    struct Chatty;

    impl fmt::Debug for Chatty {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            tracing::error!("logged while being logged");
            f.write_str("Chatty")
        }
    }

    #[test]
    fn layer_does_not_recurse() {
        let buffer = LogBuffer::default();
        capture(&buffer, &EventBus::default(), || {
            tracing::info!(value = ?Chatty, "outer");
        });
        let lines = buffer.query(10, None, None, None);
        assert_eq!(messages(&lines), ["outer"]);
        assert_eq!(lines[0].fields["value"], "Chatty");
        // The guard is released afterwards.
        assert!(CaptureGuard::enter().is_some());
    }

    #[test]
    fn capture_guard_is_exclusive_per_thread() {
        let outer = CaptureGuard::enter();
        assert!(outer.is_some());
        assert!(CaptureGuard::enter().is_none());
        // Other threads are unaffected.
        assert!(
            std::thread::spawn(|| CaptureGuard::enter().is_some())
                .join()
                .unwrap()
        );
        drop(outer);
        assert!(CaptureGuard::enter().is_some());
    }

    #[test]
    fn lines_from_many_threads_are_all_kept_in_order() {
        let buffer = LogBuffer::new(10_000);
        let bus = EventBus::default();
        let subscriber: Arc<dyn Subscriber + Send + Sync> =
            Arc::new(Registry::default().with(capture_layer(buffer.clone(), bus)));
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let subscriber = Arc::clone(&subscriber);
                std::thread::spawn(move || {
                    tracing::subscriber::with_default(subscriber, || {
                        for i in 0..250 {
                            tracing::info!(thread = t, i, "tick");
                        }
                    });
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let lines = buffer.query(10_000, None, None, None);
        assert_eq!(lines.len(), 1_000);
        assert!(lines.windows(2).all(|w| w[0].seq + 1 == w[1].seq));
    }

    // -- file sink ----------------------------------------------------------

    fn names(sink: &FileSink) -> Vec<String> {
        sink.files().into_iter().map(|f| f.name).collect()
    }

    #[test]
    fn file_names_round_trip() {
        assert_eq!(log_file_name("2026-10-02", 0), "switchyard-2026-10-02.log");
        assert_eq!(
            log_file_name("2026-10-02", 3),
            "switchyard-2026-10-02.3.log"
        );
        let day = parse_day("2026-10-02").unwrap();
        assert_eq!(
            parse_log_file_name("switchyard-2026-10-02.log"),
            Some((day, 0))
        );
        assert_eq!(
            parse_log_file_name("switchyard-2026-10-02.3.log"),
            Some((day, 3))
        );
        for foreign in [
            "switchyard-2026-10-02.0.log",
            "switchyard-2026-10-02.x.log",
            "switchyard-yesterday.log",
            "other-2026-10-02.log",
            "switchyard-2026-10-02.log.gz",
            "main.log",
        ] {
            assert_eq!(parse_log_file_name(foreign), None, "{foreign}");
        }
    }

    #[test]
    fn sink_writes_one_file_per_utc_day() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = FileSink::new(tmp.path(), 0);
        assert_eq!(sink.dir(), tmp.path().join("logs"));
        assert_eq!(sink.flush().unwrap(), 0);
        assert!(!sink.dir().exists());
        sink.enqueue(Arc::new(
            line(T0 + DAY - 1, "info", "late").with_field("n", 1),
        ));
        sink.enqueue(Arc::new(line(T0 + DAY, "warn", "early")));
        assert!(sink.has_pending());
        assert_eq!(sink.flush().unwrap(), 2);
        assert!(!sink.has_pending());
        assert_eq!(
            names(&sink),
            ["switchyard-2026-10-02.log", "switchyard-2026-10-03.log"]
        );
        assert_eq!(
            fs::read_to_string(sink.dir().join("switchyard-2026-10-02.log")).unwrap(),
            "2026-10-02T23:59:59.999Z  INFO switchyard::test: late n=1\n"
        );
        // Later flushes append.
        sink.enqueue(Arc::new(line(T0 + DAY + 5, "info", "more")));
        sink.flush().unwrap();
        let text = fs::read_to_string(sink.dir().join("switchyard-2026-10-03.log")).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.ends_with("more\n"));
    }

    #[test]
    fn buffer_feeds_the_attached_sink() {
        let tmp = tempfile::tempdir().unwrap();
        let buffer = LogBuffer::default();
        buffer.push(line(T0, "info", "before the sink"));
        let sink = Arc::new(FileSink::new(tmp.path(), 0));
        buffer.set_file_sink(Some(Arc::clone(&sink)));
        capture(&buffer, &EventBus::default(), || {
            tracing::info!(n = 7, "through the layer")
        });
        buffer.push(line(T0, "warn", "pushed"));
        assert_eq!(sink.flush().unwrap(), 2);
        buffer.set_file_sink(None);
        assert!(buffer.file_sink().is_none());
        buffer.push(line(T0, "info", "after the sink"));
        assert_eq!(sink.flush().unwrap(), 0);
        let mut text = String::new();
        for file in sink.files() {
            text.push_str(&fs::read_to_string(sink.dir().join(file.name)).unwrap());
        }
        assert!(text.contains("through the layer n=7"));
        assert!(text.contains("WARN switchyard::test: pushed"));
        assert!(!text.contains("before the sink") && !text.contains("after the sink"));
    }

    /// A line whose text form is exactly 1,000 bytes with the newline.
    fn kilobyte_line(at: i64) -> Arc<LogLine> {
        let mut l = line(at, "info", "");
        let overhead = l.to_text().len() + 1;
        l.message = "x".repeat(1_000 - overhead);
        Arc::new(l)
    }

    #[test]
    fn a_busy_day_continues_in_numbered_parts() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = FileSink::new(tmp.path(), 0);
        // Cap 400 KiB → parts of 100 KiB.
        sink.set_max_total_bytes(400 * 1024);
        assert_eq!(sink.part_limit(), 100 * 1024);
        for _ in 0..25 {
            for i in 0..10 {
                sink.enqueue(kilobyte_line(T0 + i));
            }
            sink.flush().unwrap();
        }
        // 250 kB in batches of 10 kB: 100 kB, 100 kB and the rest.
        let files = sink.files();
        assert_eq!(
            files
                .iter()
                .map(|f| (f.name.as_str(), f.size))
                .collect::<Vec<_>>(),
            [
                ("switchyard-2026-10-02.log", 100_000),
                ("switchyard-2026-10-02.1.log", 100_000),
                ("switchyard-2026-10-02.2.log", 50_000),
            ]
        );
        // A new sink (after a restart) continues the newest part.
        let restarted = FileSink::new(tmp.path(), 0);
        restarted.set_max_total_bytes(400 * 1024);
        restarted.enqueue(kilobyte_line(T0 + 99));
        restarted.flush().unwrap();
        assert_eq!(restarted.files()[2].size, 51_000);
        assert_eq!(restarted.files().len(), 3);
    }

    #[test]
    fn oldest_files_are_deleted_beyond_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = FileSink::new(tmp.path(), 0);
        // Five days of 100 kB each, no cap yet.
        for day in 0..5 {
            for i in 0..100 {
                sink.enqueue(kilobyte_line(T0 + day * DAY + i));
            }
            sink.flush().unwrap();
        }
        assert_eq!(names(&sink).len(), 5);
        fs::write(sink.dir().join("notes.txt"), "not a log file").unwrap();

        // 300 kB allowed: the two oldest days go.
        sink.set_max_total_bytes(300_000);
        assert_eq!(sink.enforce_cap(), 2);
        assert_eq!(
            names(&sink),
            [
                "switchyard-2026-10-04.log",
                "switchyard-2026-10-05.log",
                "switchyard-2026-10-06.log"
            ]
        );
        assert_eq!(sink.enforce_cap(), 0);
        assert!(sink.dir().join("notes.txt").exists());

        // The cap is enforced on every flush too.
        sink.enqueue(kilobyte_line(T0 + 5 * DAY));
        sink.flush().unwrap();
        assert_eq!(
            names(&sink),
            [
                "switchyard-2026-10-05.log",
                "switchyard-2026-10-06.log",
                "switchyard-2026-10-07.log"
            ]
        );

        // The file being written survives even a cap smaller than itself.
        sink.set_max_total_bytes(10);
        assert_eq!(sink.enforce_cap(), 2);
        assert_eq!(names(&sink), ["switchyard-2026-10-07.log"]);
    }

    #[test]
    fn megabyte_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = FileSink::new(tmp.path(), 200);
        assert_eq!(sink.max_total_size_mb(), 200);
        assert_eq!(sink.part_limit(), 50 * 1024 * 1024);
        sink.set_max_total_size_mb(1);
        assert_eq!(sink.part_limit(), 256 * 1024);
        sink.set_max_total_size_mb(0);
        assert_eq!(sink.part_limit(), MAX_PART_BYTES);
        sink.set_max_total_bytes(1);
        assert_eq!(sink.part_limit(), MIN_PART_BYTES);
    }

    #[test]
    fn sink_queue_is_bounded_and_errors_are_counted() {
        let tmp = tempfile::tempdir().unwrap();
        // A file where the data directory's `logs` directory should be.
        fs::write(tmp.path().join("logs"), "in the way").unwrap();
        let sink = FileSink::new(tmp.path(), 0);
        let l = Arc::new(line(T0, "info", "x"));
        for _ in 0..MAX_PENDING_LINES + 2 {
            sink.enqueue(Arc::clone(&l));
        }
        assert_eq!(sink.dropped(), 2);
        assert!(sink.flush().is_err());
        assert_eq!(sink.write_errors(), 1);
        // The batch is dropped, not retried forever.
        assert!(!sink.has_pending());
        assert_eq!(sink.flush().unwrap(), 0);
    }
}
