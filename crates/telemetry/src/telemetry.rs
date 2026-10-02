//! The facade the gateway holds: one cheap-to-clone handle bundling the
//! event bus, the live gauges, the usage store, the body store and the log
//! buffer, wired together and driven by the configuration.

use crate::bodies::{BodyStore, CapturedBodies};
use crate::bus::{Event, EventBus};
use crate::gauges::{GaugeGuard, GaugeSnapshot, Gauges};
use crate::logs::{FileSink, LogBuffer, LogLevel, capture_layer};
use crate::record::{RequestRecord, RequestStart};
use crate::usage::{LoadReport, StatsTick, UsageStore, UsageStoreOptions, usage_dir};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use switchyard_core::config::{LoggingConfig, UsageConfig};
use switchyard_core::util::now_unix_ms;
use tokio::runtime::Handle;
use tokio::sync::{Notify, broadcast};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::registry::LookupSpan;

/// How often queued log lines are written to the log file.
const LOG_FLUSH_INTERVAL: Duration = Duration::from_millis(1_000);
/// How often old usage files, captured bodies and log files are pruned.
const PRUNE_INTERVAL: Duration = Duration::from_secs(3_600);

/// What [`Telemetry::new`] needs to know.
#[derive(Clone, Debug, Default)]
pub struct TelemetryOptions {
    /// The gateway's data directory (`server.data_dir`, resolved). `None`
    /// keeps everything in memory: no usage files, no captured bodies, no
    /// log files.
    pub data_dir: Option<PathBuf>,
    pub usage: UsageConfig,
    pub logging: LoggingConfig,
}

/// What [`Telemetry::prune`] removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneReport {
    pub usage_files: usize,
    pub body_files: usize,
    pub log_files: usize,
}

/// Body files handed to the blocking pool and not written yet, so a flush
/// can wait for them.
#[derive(Default)]
struct BodyWrites {
    pending: AtomicUsize,
    idle: Notify,
}

impl BodyWrites {
    fn begin(self: &Arc<Self>) -> BodyWriteGuard {
        self.pending.fetch_add(1, Ordering::AcqRel);
        BodyWriteGuard(Arc::clone(self))
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            // Registered before the counter is read, so a write finishing
            // in between still wakes this task.
            notified.as_mut().enable();
            if self.pending.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// Counts one body write as pending until dropped: when the write is done,
/// and also when the runtime discards the task without running it.
struct BodyWriteGuard(Arc<BodyWrites>);

impl Drop for BodyWriteGuard {
    fn drop(&mut self) {
        if self.0.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

struct Inner {
    data_dir: Option<PathBuf>,
    bus: EventBus,
    gauges: Gauges,
    usage: UsageStore,
    bodies: BodyStore,
    body_writes: Arc<BodyWrites>,
    logs: LogBuffer,
    /// `logging.max_total_size_mb` in bytes; caps the log directory and,
    /// separately, the captured-bodies directory.
    max_total_bytes: AtomicU64,
    background_started: AtomicBool,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// Everything the gateway records about itself. Cloning is cheap; clones
/// share all state.
///
/// ```
/// use switchyard_core::Protocol;
/// use switchyard_telemetry::{RecordBuilder, RequestStart, Telemetry, TelemetryOptions};
///
/// let telemetry = Telemetry::new(TelemetryOptions::default());
/// let mut events = telemetry.subscribe();
///
/// let _in_flight = telemetry.track_in_flight();
/// let start = RequestStart::new(Protocol::OpenaiChat, "POST /v1/chat/completions", "gpt-5", 1_000);
/// telemetry.request_started(start.clone());
/// let record = RecordBuilder::new(start).finish(200, 1_250);
/// telemetry.finish_request(record);
///
/// assert_eq!(events.try_recv().unwrap().topic(), "request.started");
/// assert_eq!(events.try_recv().unwrap().topic(), "request.finished");
/// assert_eq!(telemetry.gauges().totals().requests, 1);
/// ```
#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telemetry")
            .field("data_dir", &self.inner.data_dir)
            .field("usage", &self.inner.usage)
            .field("bodies", &self.inner.bodies)
            .field("logs", &self.inner.logs)
            .finish()
    }
}

impl Default for Telemetry {
    /// In-memory telemetry with default settings.
    fn default() -> Self {
        Telemetry::new(TelemetryOptions::default())
    }
}

fn megabytes(mb: u64) -> u64 {
    mb.saturating_mul(1024 * 1024)
}

fn kilobytes(kb: u64) -> usize {
    usize::try_from(kb.saturating_mul(1024)).unwrap_or(usize::MAX)
}

impl Telemetry {
    /// Builds the telemetry of a gateway starting now. Does no I/O and
    /// needs no async runtime; call [`load_history`](Telemetry::load_history)
    /// and [`spawn_background`](Telemetry::spawn_background) afterwards.
    pub fn new(options: TelemetryOptions) -> Self {
        Telemetry::with_start_time(options, now_unix_ms())
    }

    /// [`new`](Telemetry::new) with an explicit start time (unix
    /// milliseconds) for the uptime gauge.
    pub fn with_start_time(options: TelemetryOptions, started_at: i64) -> Self {
        let TelemetryOptions {
            data_dir,
            usage,
            logging,
        } = options;
        let gauges = Gauges::new(started_at);
        let store = UsageStore::new(UsageStoreOptions {
            retention_days: usage.retention_days,
            enabled: usage.enabled,
            persist_dir: None,
            gauges: Some(gauges.clone()),
            ..UsageStoreOptions::default()
        });
        let bodies = BodyStore::new(
            data_dir.as_deref(),
            logging.request_log,
            kilobytes(logging.request_log_max_body_kb),
        );
        let telemetry = Telemetry {
            inner: Arc::new(Inner {
                data_dir,
                bus: EventBus::default(),
                gauges,
                usage: store,
                bodies,
                body_writes: Arc::new(BodyWrites::default()),
                logs: LogBuffer::default(),
                max_total_bytes: AtomicU64::new(0),
                background_started: AtomicBool::new(false),
                tasks: Mutex::new(Vec::new()),
            }),
        };
        telemetry.reconfigure(&usage, &logging);
        telemetry
    }

    // ------------------------------------------------------------------
    // Parts
    // ------------------------------------------------------------------

    /// The data directory given at construction.
    pub fn data_dir(&self) -> Option<&Path> {
        self.inner.data_dir.as_deref()
    }

    pub fn bus(&self) -> &EventBus {
        &self.inner.bus
    }

    pub fn gauges(&self) -> &Gauges {
        &self.inner.gauges
    }

    pub fn usage(&self) -> &UsageStore {
        &self.inner.usage
    }

    pub fn bodies(&self) -> &BodyStore {
        &self.inner.bodies
    }

    pub fn logs(&self) -> &LogBuffer {
        &self.inner.logs
    }

    // ------------------------------------------------------------------
    // Events and gauges
    // ------------------------------------------------------------------

    /// Publishes an event; see [`EventBus::publish`].
    pub fn publish(&self, event: Event) -> usize {
        self.inner.bus.publish(event)
    }

    /// Subscribes to the event bus.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.bus.subscribe()
    }

    /// Counts a request as in flight until the guard is dropped.
    pub fn track_in_flight(&self) -> GaugeGuard {
        self.inner.gauges.track_in_flight()
    }

    /// Counts an open streaming response until the guard is dropped.
    pub fn track_stream(&self) -> GaugeGuard {
        self.inner.gauges.track_stream()
    }

    /// Counts an open client WebSocket until the guard is dropped.
    pub fn track_ws(&self) -> GaugeGuard {
        self.inner.gauges.track_ws()
    }

    /// Gauges and totals since start, for `GET /status`.
    pub fn status(&self, now: i64) -> GaugeSnapshot {
        self.inner.gauges.snapshot(now)
    }

    /// The once-a-second dashboard `stats` frame.
    pub fn stats_tick(&self, now: i64) -> StatsTick {
        self.inner.usage.stats_tick(now)
    }

    // ------------------------------------------------------------------
    // Requests
    // ------------------------------------------------------------------

    /// Announces a request on the bus (`request.started`).
    pub fn request_started(&self, start: RequestStart) {
        self.inner.bus.publish(Event::RequestStarted(start));
    }

    /// Records a finished request: counts it in the totals since start and
    /// in the usage store (unless `usage.enabled` is off), queues it for
    /// persistence and publishes `request.finished`.
    ///
    /// The caller fills in `cost` and `has_bodies` beforehand. Never blocks.
    pub fn finish_request(&self, record: RequestRecord) -> Arc<RequestRecord> {
        let record = Arc::new(record);
        self.inner.gauges.count_finished(&record);
        self.inner.usage.record_arc(Arc::clone(&record));
        self.inner
            .bus
            .publish(Event::RequestFinished(Arc::clone(&record)));
        record
    }

    /// Stores the bodies of a request if `logging.request_log` asks for it
    /// and returns the value for the record's `has_bodies`. Call it with
    /// the finished record, then pass the record on:
    ///
    /// ```
    /// # use switchyard_core::Protocol;
    /// # use switchyard_telemetry::{CapturedBodies, RecordBuilder, RequestStart, Telemetry};
    /// # let telemetry = Telemetry::default();
    /// # let start = RequestStart::new(Protocol::Gemini, "POST /v1beta/models/x:generateContent", "x", 0);
    /// # let (builder, bodies) = (RecordBuilder::new(start), CapturedBodies::default());
    /// let mut record = builder.finish(200, 1_000);
    /// record.has_bodies = telemetry.capture_bodies(&record, bodies);
    /// telemetry.finish_request(record);
    /// ```
    ///
    /// Use [`BodyStore::wants`] (`telemetry.bodies().wants(true)`) to decide
    /// whether bodies need to be collected at all.
    ///
    /// Inside a tokio runtime the file is written on the blocking pool and
    /// this returns at once (so a failed write still reports `true`);
    /// [`flush`](Telemetry::flush) and [`shutdown`](Telemetry::shutdown)
    /// wait for such writes. Outside a runtime the file is written before
    /// returning.
    pub fn capture_bodies(&self, record: &RequestRecord, bodies: CapturedBodies) -> bool {
        let store = &self.inner.bodies;
        if !store.accepts(&record.id, !record.ok) || bodies.is_empty() {
            return false;
        }
        match Handle::try_current() {
            Ok(handle) => {
                let store = store.clone();
                let (id, started_at, failed) = (record.id.clone(), record.started_at, !record.ok);
                let pending = self.inner.body_writes.begin();
                handle.spawn_blocking(move || {
                    let _pending = pending;
                    store.capture_at(&id, started_at, failed, bodies)
                });
                true
            }
            Err(_) => store.capture(record, bodies).unwrap_or(false),
        }
    }

    // ------------------------------------------------------------------
    // Logs
    // ------------------------------------------------------------------

    /// The `tracing` layer that feeds the log buffer and the bus; add it to
    /// the subscriber the binary installs.
    pub fn log_layer<S>(&self) -> impl Layer<S> + use<S>
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        capture_layer(self.inner.logs.clone(), self.inner.bus.clone())
    }

    // ------------------------------------------------------------------
    // Configuration
    // ------------------------------------------------------------------

    /// Applies the `[usage]` and `[logging]` sections, at start and on
    /// every hot reload:
    ///
    /// * `usage.enabled`, `usage.persist`, `usage.retention_days`;
    /// * `logging.request_log`, `logging.request_log_max_body_kb`;
    /// * `logging.level` becomes the least severe level the log buffer
    ///   records (an unknown level leaves it unchanged);
    /// * `logging.file`, `logging.max_total_size_mb`.
    ///
    /// The data directory itself is fixed at construction.
    pub fn reconfigure(&self, usage: &UsageConfig, logging: &LoggingConfig) {
        let inner = &self.inner;
        inner.usage.set_enabled(usage.enabled);
        inner.usage.set_retention_days(usage.retention_days);
        let persist_dir = match (&inner.data_dir, usage.persist) {
            (Some(data_dir), true) => Some(usage_dir(data_dir)),
            _ => None,
        };
        inner.usage.set_persist_dir(persist_dir);

        inner.bodies.configure(
            logging.request_log,
            kilobytes(logging.request_log_max_body_kb),
        );
        inner
            .max_total_bytes
            .store(megabytes(logging.max_total_size_mb), Ordering::Relaxed);

        if let Ok(level) = logging.level.parse::<LogLevel>() {
            inner.logs.set_min_level(level);
        }
        match (&inner.data_dir, logging.file, inner.logs.file_sink()) {
            (Some(_), true, Some(sink)) => sink.set_max_total_size_mb(logging.max_total_size_mb),
            (Some(data_dir), true, None) => {
                let sink = FileSink::new(data_dir, logging.max_total_size_mb);
                inner.logs.set_file_sink(Some(Arc::new(sink)));
            }
            (_, _, Some(sink)) => {
                // File logging was switched off: stop feeding the sink and
                // write out what it still holds.
                inner.logs.set_file_sink(None);
                flush_sink_detached(sink);
            }
            (_, _, None) => {}
        }
    }

    // ------------------------------------------------------------------
    // Persistence and background work
    // ------------------------------------------------------------------

    /// Rebuilds the statistics and the recent-requests ring from the usage
    /// files (blocking file I/O; call once at start). Does nothing when
    /// persistence is off.
    pub fn load_history(&self, now: i64) -> LoadReport {
        match self.inner.usage.persist_dir() {
            Some(dir) => self
                .inner
                .usage
                .load(&dir, self.inner.usage.retention_days(), now),
            None => LoadReport::default(),
        }
    }

    /// Starts the background tasks on `handle`: the usage writer, the log
    /// file writer and the hourly pruning. Calling it again does nothing.
    /// The tasks end on their own when the last `Telemetry` handle is
    /// dropped; for an orderly stop use [`shutdown`](Telemetry::shutdown).
    pub fn spawn_background(&self, handle: &Handle) {
        if self.inner.background_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let writer = self.inner.usage.spawn_writer(handle);
        let maintenance = handle.spawn(maintenance_loop(Arc::downgrade(&self.inner)));
        self.inner.tasks.lock().extend([writer, maintenance]);
    }

    /// Writes everything queued — usage records and log lines — without
    /// blocking the async runtime, and waits for writes already in
    /// progress, including captured bodies on their way to disk.
    pub async fn flush(&self) -> io::Result<()> {
        self.inner.body_writes.wait_idle().await;
        let usage = self.inner.usage.flush().await.map(|_| ());
        let logs = match self.inner.logs.file_sink() {
            Some(sink) => tokio::task::spawn_blocking(move || sink.flush().map(|_| ()))
                .await
                .unwrap_or_else(|join_error| Err(io::Error::other(join_error))),
            None => Ok(()),
        };
        usage.and(logs)
    }

    /// Stops the background tasks and writes everything queued. Call once
    /// the server has stopped accepting requests.
    pub async fn shutdown(&self) -> io::Result<()> {
        for task in self.inner.tasks.lock().drain(..) {
            task.abort();
        }
        self.flush().await
    }

    /// Deletes what is past its retention (blocking file I/O): usage files
    /// and captured bodies older than `usage.retention_days`, captured
    /// bodies and log files beyond `logging.max_total_size_mb` (each
    /// directory is capped separately). Runs hourly once
    /// [`spawn_background`](Telemetry::spawn_background) was called.
    pub fn prune(&self, now: i64) -> PruneReport {
        prune(&self.inner, now)
    }
}

fn prune(inner: &Inner, now: i64) -> PruneReport {
    let max_total_bytes = inner.max_total_bytes.load(Ordering::Relaxed);
    PruneReport {
        usage_files: inner.usage.prune(now),
        body_files: inner
            .bodies
            .prune(now, inner.usage.retention_days(), max_total_bytes),
        log_files: inner.logs.file_sink().map_or(0, |sink| sink.enforce_cap()),
    }
}

/// Writes a detached sink's queue: on the blocking pool when called from a
/// runtime, inline otherwise.
fn flush_sink_detached(sink: Arc<FileSink>) {
    if !sink.has_pending() {
        return;
    }
    match Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(move || sink.flush());
        }
        Err(_) => {
            let _ = sink.flush();
        }
    }
}

/// Writes queued log lines every second and prunes every hour, until every
/// `Telemetry` handle is gone.
async fn maintenance_loop(weak: Weak<Inner>) {
    let mut ticker = tokio::time::interval(LOG_FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut next_prune = tokio::time::Instant::now();
    // The sink seen last, so its queue can still be written once the
    // telemetry itself is gone.
    let mut last_sink: Option<Arc<FileSink>> = None;
    loop {
        ticker.tick().await;
        let Some(inner) = weak.upgrade() else {
            break;
        };
        last_sink = inner.logs.file_sink();
        if let Some(sink) = last_sink.clone()
            && sink.has_pending()
        {
            // The sink counts its own failures; nothing to report here.
            let _ = tokio::task::spawn_blocking(move || sink.flush()).await;
        }
        if tokio::time::Instant::now() >= next_prune {
            next_prune = tokio::time::Instant::now() + PRUNE_INTERVAL;
            let _ = tokio::task::spawn_blocking(move || prune(&inner, now_unix_ms())).await;
        }
    }
    if let Some(sink) = last_sink {
        let _ = tokio::task::spawn_blocking(move || sink.flush()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{RecordBuilder, RecordError};
    use crate::usage::{Range, RequestQuery};
    use pretty_assertions::assert_eq;
    use std::fs;
    use switchyard_core::config::RequestLogMode;
    use switchyard_core::protocol::Protocol;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    /// 2026-10-02T00:00:00Z.
    const T0: i64 = 1_790_899_200_000;

    fn record(id: &str, started_at: i64, status: u16) -> RequestRecord {
        let start = RequestStart::new(
            Protocol::Anthropic,
            "POST /v1/messages",
            "sonnet",
            started_at,
        )
        .with_id(id);
        let mut b = RecordBuilder::new(start);
        if status >= 400 {
            b.set_error(RecordError::new("upstream", "boom"));
        }
        b.finish(status, started_at + 200)
    }

    fn options(dir: &Path) -> TelemetryOptions {
        TelemetryOptions {
            data_dir: Some(dir.to_path_buf()),
            ..TelemetryOptions::default()
        }
    }

    fn bodies() -> CapturedBodies {
        CapturedBodies {
            client_request: Some(r#"{"model":"sonnet"}"#.to_string()),
            ..CapturedBodies::default()
        }
    }

    #[test]
    fn finish_request_records_counts_and_publishes() {
        let telemetry = Telemetry::with_start_time(TelemetryOptions::default(), T0);
        let mut rx = telemetry.subscribe();
        let shared = telemetry.finish_request(record("a", T0 + 1_000, 200));
        telemetry
            .clone()
            .finish_request(record("b", T0 + 2_000, 502));

        match rx.try_recv().unwrap() {
            Event::RequestFinished(published) => assert!(Arc::ptr_eq(&published, &shared)),
            other => panic!("unexpected event {other:?}"),
        }
        assert_eq!(rx.try_recv().unwrap().topic(), "request.finished");

        let status = telemetry.status(T0 + 5_000);
        assert_eq!(status.uptime_ms, 5_000);
        assert_eq!((status.totals.requests, status.totals.errors), (2, 1));
        assert_eq!(
            telemetry
                .usage()
                .summary(Range::Hour, T0 + 5_000)
                .totals
                .requests,
            2
        );
        assert_eq!(
            telemetry.usage().requests(&RequestQuery::default()).total,
            2
        );
        assert!(Arc::ptr_eq(&telemetry.usage().get("a").unwrap(), &shared));
        // Memory only: nothing to persist.
        assert_eq!(telemetry.data_dir(), None);
        assert_eq!(telemetry.usage().persist_dir(), None);
        assert_eq!(telemetry.load_history(T0), LoadReport::default());
    }

    #[test]
    fn stats_tick_combines_gauges_and_rates() {
        let telemetry = Telemetry::with_start_time(TelemetryOptions::default(), T0);
        let in_flight = telemetry.track_in_flight();
        let _stream = telemetry.track_stream();
        let _ws = telemetry.track_ws();
        telemetry.finish_request(record("a", T0, 200));
        let tick = telemetry.stats_tick(T0 + 1_000);
        assert_eq!(
            (tick.in_flight, tick.active_streams, tick.ws_connections),
            (1, 1, 1)
        );
        assert_eq!(tick.rpm, 1);
        assert_eq!(tick.p50_ms, 200);
        drop(in_flight);
        assert_eq!(telemetry.stats_tick(T0 + 1_000).in_flight, 0);
        assert_eq!(telemetry.status(T0).in_flight, 0);
    }

    #[test]
    fn usage_can_be_disabled_without_silencing_the_live_view() {
        let telemetry = Telemetry::new(TelemetryOptions {
            usage: UsageConfig {
                enabled: false,
                ..UsageConfig::default()
            },
            ..TelemetryOptions::default()
        });
        let mut rx = telemetry.subscribe();
        telemetry.request_started(record("a", T0, 200).start());
        telemetry.finish_request(record("a", T0, 200));
        assert_eq!(rx.try_recv().unwrap().topic(), "request.started");
        assert_eq!(rx.try_recv().unwrap().topic(), "request.finished");
        assert_eq!(telemetry.gauges().totals().requests, 1);
        assert_eq!(telemetry.usage().info().recent, 0);
        assert_eq!(
            telemetry
                .usage()
                .summary(Range::Hour, T0 + 1_000)
                .totals
                .requests,
            0
        );
    }

    #[test]
    fn persistence_follows_the_configuration() {
        let tmp = tempfile::tempdir().unwrap();
        let telemetry = Telemetry::new(options(tmp.path()));
        assert_eq!(
            telemetry.usage().persist_dir(),
            Some(tmp.path().join("usage"))
        );
        telemetry.finish_request(record("a", T0, 200));
        telemetry.usage().flush_blocking().unwrap();
        assert!(tmp.path().join("usage/2026-10-02.jsonl").is_file());

        // A restart sees the record again.
        let restarted = Telemetry::new(options(tmp.path()));
        assert_eq!(restarted.load_history(T0 + 1_000).records, 1);
        assert_eq!(restarted.usage().get("a").unwrap().status, 200);

        // persist = false: memory only.
        let logging = LoggingConfig::default();
        let memory_only = UsageConfig {
            persist: false,
            ..UsageConfig::default()
        };
        restarted.reconfigure(&memory_only, &logging);
        assert_eq!(restarted.usage().persist_dir(), None);
        assert_eq!(restarted.load_history(T0 + 1_000), LoadReport::default());
        restarted.finish_request(record("b", T0 + 5, 200));
        assert_eq!(restarted.usage().flush_blocking().unwrap(), 0);
        // And back on.
        restarted.reconfigure(
            &UsageConfig {
                retention_days: 7,
                ..UsageConfig::default()
            },
            &logging,
        );
        assert_eq!(restarted.usage().retention_days(), 7);
        restarted.finish_request(record("c", T0 + 9, 200));
        assert_eq!(restarted.usage().flush_blocking().unwrap(), 1);
    }

    #[test]
    fn body_capture_follows_the_configuration() {
        let tmp = tempfile::tempdir().unwrap();
        let telemetry = Telemetry::new(options(tmp.path()));
        // Default: off.
        assert_eq!(telemetry.bodies().mode(), RequestLogMode::Off);
        assert!(!telemetry.capture_bodies(&record("a", T0, 502), bodies()));

        let errors_only = LoggingConfig {
            request_log: RequestLogMode::Errors,
            request_log_max_body_kb: 4,
            ..LoggingConfig::default()
        };
        telemetry.reconfigure(&UsageConfig::default(), &errors_only);
        assert_eq!(telemetry.bodies().max_body_bytes(), 4096);
        assert!(!telemetry.capture_bodies(&record("ok", T0, 200), bodies()));
        // Outside a runtime the file is written before returning.
        assert!(telemetry.capture_bodies(&record("bad", T0, 502), bodies()));
        assert_eq!(telemetry.bodies().read("bad").unwrap(), bodies());
        assert!(tmp.path().join("requests/2026-10-02/bad.json").is_file());
        // Nothing to store: nothing claimed.
        assert!(!telemetry.capture_bodies(&record("bad2", T0, 502), CapturedBodies::default()));
        assert_eq!(telemetry.bodies().read("ok"), None);
    }

    #[tokio::test]
    async fn body_capture_inside_a_runtime_writes_in_the_background() {
        let tmp = tempfile::tempdir().unwrap();
        let telemetry = Telemetry::new(TelemetryOptions {
            logging: LoggingConfig {
                request_log: RequestLogMode::All,
                ..LoggingConfig::default()
            },
            ..options(tmp.path())
        });
        let mut r = record("r1", T0, 200);
        r.has_bodies = telemetry.capture_bodies(&r, bodies());
        assert!(r.has_bodies);
        let stored = telemetry.finish_request(r);
        assert!(stored.has_bodies);
        for _ in 0..400 {
            if telemetry.bodies().read("r1").is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(telemetry.bodies().read("r1").unwrap(), bodies());
    }

    #[tokio::test]
    async fn flush_waits_for_body_writes_in_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let telemetry = Telemetry::new(TelemetryOptions {
            logging: LoggingConfig {
                request_log: RequestLogMode::All,
                ..LoggingConfig::default()
            },
            ..options(tmp.path())
        });
        // Nothing pending: returns at once.
        telemetry.flush().await.unwrap();
        for i in 0..40 {
            let r = record(&format!("r{i}"), T0, 200);
            assert!(telemetry.capture_bodies(&r, bodies()));
        }
        telemetry.shutdown().await.unwrap();
        // No polling: every file is there when shutdown returns.
        assert_eq!(
            telemetry.inner.body_writes.pending.load(Ordering::Acquire),
            0
        );
        for i in 0..40 {
            assert!(
                tmp.path()
                    .join(format!("requests/2026-10-02/r{i}.json"))
                    .is_file(),
                "r{i} was not written before shutdown returned"
            );
        }
    }

    #[test]
    fn a_discarded_body_write_does_not_block_flush_forever() {
        let writes = Arc::new(BodyWrites::default());
        let guard = writes.begin();
        assert_eq!(writes.pending.load(Ordering::Acquire), 1);
        // The runtime dropping the task without running it drops the guard.
        drop(guard);
        assert_eq!(writes.pending.load(Ordering::Acquire), 0);
    }

    #[test]
    fn log_layer_feeds_buffer_and_bus_and_honours_the_level() {
        let telemetry = Telemetry::default();
        let mut rx = telemetry.subscribe();
        let subscriber = Registry::default().with(telemetry.log_layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!("below the default level");
            tracing::info!(port = 8317, "listening");
        });
        let lines = telemetry.logs().query(10, None, None, None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].message, "listening");
        assert_eq!(
            rx.try_recv().unwrap().to_frame()["data"]["fields"]["port"],
            8317
        );

        // Hot reload to debug.
        let debug = LoggingConfig {
            level: "debug".to_string(),
            ..LoggingConfig::default()
        };
        telemetry.reconfigure(&UsageConfig::default(), &debug);
        assert_eq!(telemetry.logs().min_level(), LogLevel::Debug);
        // An unknown level keeps the current one.
        let bogus = LoggingConfig {
            level: "loud".to_string(),
            ..LoggingConfig::default()
        };
        telemetry.reconfigure(&UsageConfig::default(), &bogus);
        assert_eq!(telemetry.logs().min_level(), LogLevel::Debug);
    }

    #[test]
    fn file_logging_follows_the_configuration() {
        let tmp = tempfile::tempdir().unwrap();
        let telemetry = Telemetry::new(options(tmp.path()));
        assert!(telemetry.logs().file_sink().is_none());

        let to_file = LoggingConfig {
            file: true,
            max_total_size_mb: 5,
            ..LoggingConfig::default()
        };
        telemetry.reconfigure(&UsageConfig::default(), &to_file);
        let sink = telemetry.logs().file_sink().unwrap();
        assert_eq!(sink.dir(), tmp.path().join("logs"));
        assert_eq!(sink.max_total_size_mb(), 5);

        // Changing the cap keeps the same sink.
        let bigger = LoggingConfig {
            max_total_size_mb: 9,
            ..to_file.clone()
        };
        telemetry.reconfigure(&UsageConfig::default(), &bigger);
        assert!(Arc::ptr_eq(&telemetry.logs().file_sink().unwrap(), &sink));
        assert_eq!(sink.max_total_size_mb(), 9);

        telemetry
            .logs()
            .push(crate::logs::LogLine::new(T0, "info", "t", "to the file"));
        // Switching file logging off writes what was queued.
        telemetry.reconfigure(&UsageConfig::default(), &LoggingConfig::default());
        assert!(telemetry.logs().file_sink().is_none());
        let text = fs::read_to_string(tmp.path().join("logs/switchyard-2026-10-02.log")).unwrap();
        assert!(text.contains("to the file"));

        // Without a data dir there is nowhere to write.
        let homeless = Telemetry::default();
        homeless.reconfigure(&UsageConfig::default(), &to_file);
        assert!(homeless.logs().file_sink().is_none());
        assert_eq!(homeless.bodies().mode(), RequestLogMode::Off);
    }

    #[test]
    fn prune_covers_usage_bodies_and_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let telemetry = Telemetry::new(TelemetryOptions {
            usage: UsageConfig {
                retention_days: 2,
                ..UsageConfig::default()
            },
            logging: LoggingConfig {
                request_log: RequestLogMode::All,
                file: true,
                max_total_size_mb: 0,
                ..LoggingConfig::default()
            },
            ..options(tmp.path())
        });
        const DAY: i64 = 86_400_000;
        for day in 0..6 {
            let r = record(&format!("d{day}"), T0 + day * DAY, 200);
            assert!(telemetry.capture_bodies(&r, bodies()));
            telemetry.finish_request(r);
            telemetry.logs().push(crate::logs::LogLine::new(
                T0 + day * DAY,
                "info",
                "t",
                "x".repeat(2_000),
            ));
        }
        telemetry.usage().flush_blocking().unwrap();
        let sink = telemetry.logs().file_sink().unwrap();
        sink.flush().unwrap();
        assert_eq!(sink.files().len(), 6);

        let now = T0 + 5 * DAY + 1_000;
        // No size cap: only age counts, and log files have no age limit.
        assert_eq!(
            telemetry.prune(now),
            PruneReport {
                usage_files: 3,
                body_files: 3,
                log_files: 0
            }
        );
        assert!(!tmp.path().join("usage/2026-10-04.jsonl").exists());
        assert!(tmp.path().join("usage/2026-10-05.jsonl").exists());
        assert!(!tmp.path().join("requests/2026-10-04").exists());
        assert!(tmp.path().join("requests/2026-10-05/d3.json").exists());

        // A cap (in bytes, set directly for the test) trims the log files.
        sink.set_max_total_bytes(5_000);
        assert_eq!(telemetry.prune(now).log_files, 4);
        assert_eq!(sink.files().len(), 2);
    }

    #[tokio::test]
    async fn background_tasks_persist_and_shutdown_flushes() {
        let tmp = tempfile::tempdir().unwrap();
        let telemetry = Telemetry::new(TelemetryOptions {
            logging: LoggingConfig {
                file: true,
                ..LoggingConfig::default()
            },
            ..options(tmp.path())
        });
        telemetry.spawn_background(&Handle::current());
        // Idempotent.
        telemetry.spawn_background(&Handle::current());
        assert_eq!(telemetry.inner.tasks.lock().len(), 2);

        // The hourly pruning runs against the real clock, so this test uses
        // current timestamps: nothing it writes is ever past retention.
        let now = now_unix_ms();
        let day = crate::time::utc_day(now);
        telemetry.finish_request(record("a", now, 200));
        telemetry
            .logs()
            .push(crate::logs::LogLine::new(now, "info", "t", "hello file"));
        let usage_file = tmp.path().join(format!("usage/{day}.jsonl"));
        let log_file = tmp.path().join(format!("logs/switchyard-{day}.log"));
        for _ in 0..400 {
            if usage_file.exists() && log_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(usage_file.exists(), "the usage writer never ran");
        assert!(log_file.exists(), "the log writer never ran");

        // Whatever is still queued at shutdown is written.
        telemetry.finish_request(record("b", now, 200));
        telemetry
            .logs()
            .push(crate::logs::LogLine::new(now, "warn", "t", "last words"));
        telemetry.shutdown().await.unwrap();
        assert!(telemetry.inner.tasks.lock().is_empty());
        assert_eq!(fs::read_to_string(&usage_file).unwrap().lines().count(), 2);
        assert!(
            fs::read_to_string(&log_file)
                .unwrap()
                .contains("last words")
        );
        assert_eq!(telemetry.flush().await.ok(), Some(()));
    }

    #[test]
    fn handles_are_shareable_across_threads() {
        fn assert_shareable<T: Send + Sync + Clone + 'static>() {}
        assert_shareable::<Telemetry>();
        assert_shareable::<EventBus>();
        assert_shareable::<Event>();
        assert_shareable::<Gauges>();
        assert_shareable::<UsageStore>();
        assert_shareable::<BodyStore>();
        assert_shareable::<LogBuffer>();
        fn assert_send<T: Send + 'static>(_: &T) {}
        assert_send(&Telemetry::default().track_in_flight());
        // The layer can be installed in a global subscriber.
        fn assert_layer<L: Layer<Registry> + Send + Sync + 'static>(_: &L) {}
        assert_layer(&Telemetry::default().log_layer::<Registry>());
    }

    #[test]
    fn unusable_ids_never_claim_captured_bodies() {
        let tmp = tempfile::tempdir().unwrap();
        let telemetry = Telemetry::new(TelemetryOptions {
            logging: LoggingConfig {
                request_log: RequestLogMode::All,
                ..LoggingConfig::default()
            },
            ..options(tmp.path())
        });
        assert!(!telemetry.capture_bodies(&record("../escape", T0, 200), bodies()));
        assert!(telemetry.capture_bodies(&record("fine-id", T0, 200), bodies()));
    }

    #[test]
    fn prune_report_shape() {
        assert_eq!(
            serde_json::to_value(PruneReport::default()).unwrap(),
            serde_json::json!({"usage_files": 0, "body_files": 0, "log_files": 0})
        );
    }
}
