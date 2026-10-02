//! Live gauges: what is happening right now, plus totals since the process
//! started. Counters are atomics; holding a [`GaugeGuard`] keeps one unit
//! counted and dropping it releases the unit, so a gauge cannot leak on an
//! early return, an error path or a cancelled future.

use crate::record::RequestRecord;
use crate::usage::Totals;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
struct Inner {
    started_at: i64,
    in_flight: AtomicU64,
    active_streams: AtomicU64,
    ws_connections: AtomicU64,
    totals: Mutex<Totals>,
}

/// Shared live counters. Cloning is cheap; clones share the counters.
#[derive(Clone, Debug)]
pub struct Gauges {
    inner: Arc<Inner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    InFlight,
    Stream,
    Websocket,
}

/// Keeps one unit of a gauge counted until dropped.
#[must_use = "the gauge is decremented as soon as the guard is dropped"]
#[derive(Debug)]
pub struct GaugeGuard {
    inner: Arc<Inner>,
    kind: Kind,
}

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        let counter = self.inner.counter(self.kind);
        // Saturating: a gauge must never wrap, whatever happens.
        let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
            Some(v.saturating_sub(1))
        });
    }
}

impl Inner {
    fn counter(&self, kind: Kind) -> &AtomicU64 {
        match kind {
            Kind::InFlight => &self.in_flight,
            Kind::Stream => &self.active_streams,
            Kind::Websocket => &self.ws_connections,
        }
    }
}

/// Point-in-time view of the gauges, served by `GET /status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GaugeSnapshot {
    /// When the gateway started, unix milliseconds.
    pub started_at: i64,
    pub uptime_ms: u64,
    /// Client requests currently being served.
    pub in_flight: u64,
    /// Streaming responses currently open.
    pub active_streams: u64,
    /// Client WebSocket connections currently open.
    pub ws_connections: u64,
    /// Everything served since the process started.
    pub totals: Totals,
}

impl Gauges {
    /// Gauges for a process that started at `started_at` (unix
    /// milliseconds).
    pub fn new(started_at: i64) -> Self {
        Gauges {
            inner: Arc::new(Inner {
                started_at,
                in_flight: AtomicU64::new(0),
                active_streams: AtomicU64::new(0),
                ws_connections: AtomicU64::new(0),
                totals: Mutex::new(Totals::default()),
            }),
        }
    }

    fn track(&self, kind: Kind) -> GaugeGuard {
        self.inner.counter(kind).fetch_add(1, Ordering::AcqRel);
        GaugeGuard {
            inner: Arc::clone(&self.inner),
            kind,
        }
    }

    /// Counts a request as in flight until the guard is dropped.
    pub fn track_in_flight(&self) -> GaugeGuard {
        self.track(Kind::InFlight)
    }

    /// Counts an open streaming response until the guard is dropped.
    pub fn track_stream(&self) -> GaugeGuard {
        self.track(Kind::Stream)
    }

    /// Counts an open client WebSocket until the guard is dropped.
    pub fn track_ws(&self) -> GaugeGuard {
        self.track(Kind::Websocket)
    }

    pub fn in_flight(&self) -> u64 {
        self.inner.in_flight.load(Ordering::Acquire)
    }

    pub fn active_streams(&self) -> u64 {
        self.inner.active_streams.load(Ordering::Acquire)
    }

    pub fn ws_connections(&self) -> u64 {
        self.inner.ws_connections.load(Ordering::Acquire)
    }

    pub fn started_at(&self) -> i64 {
        self.inner.started_at
    }

    /// Milliseconds since start; zero if `now` is before the start time.
    pub fn uptime_ms(&self, now: i64) -> u64 {
        u64::try_from(now.saturating_sub(self.inner.started_at)).unwrap_or(0)
    }

    /// Adds a finished request to the totals since start.
    pub fn count_finished(&self, record: &RequestRecord) {
        self.inner.totals.lock().add_record(record);
    }

    /// Totals since start.
    pub fn totals(&self) -> Totals {
        *self.inner.totals.lock()
    }

    pub fn snapshot(&self, now: i64) -> GaugeSnapshot {
        GaugeSnapshot {
            started_at: self.inner.started_at,
            uptime_ms: self.uptime_ms(now),
            in_flight: self.in_flight(),
            active_streams: self.active_streams(),
            ws_connections: self.ws_connections(),
            totals: self.totals(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{RecordBuilder, RequestStart};
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use switchyard_core::protocol::Protocol;

    #[test]
    fn guards_count_while_alive() {
        let gauges = Gauges::new(0);
        let a = gauges.track_in_flight();
        let b = gauges.track_in_flight();
        let s = gauges.track_stream();
        let w = gauges.track_ws();
        assert_eq!(
            (
                gauges.in_flight(),
                gauges.active_streams(),
                gauges.ws_connections()
            ),
            (2, 1, 1)
        );
        drop(a);
        assert_eq!(gauges.in_flight(), 1);
        drop(s);
        drop(w);
        drop(b);
        assert_eq!(
            (
                gauges.in_flight(),
                gauges.active_streams(),
                gauges.ws_connections()
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn guard_is_released_on_early_return_and_panic() {
        fn early(gauges: &Gauges, fail: bool) -> Result<u64, ()> {
            let _guard = gauges.track_in_flight();
            if fail {
                return Err(());
            }
            Ok(gauges.in_flight())
        }
        let gauges = Gauges::new(0);
        assert_eq!(early(&gauges, false), Ok(1));
        assert_eq!(early(&gauges, true), Err(()));
        assert_eq!(gauges.in_flight(), 0);

        let cloned = gauges.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = cloned.track_stream();
            panic!("boom");
        }));
        assert!(result.is_err());
        assert_eq!(gauges.active_streams(), 0);
    }

    #[test]
    fn guard_outlives_the_gauges_handle_and_moves_across_threads() {
        let gauges = Gauges::new(0);
        let guard = gauges.track_ws();
        let observer = gauges.clone();
        drop(gauges);
        assert_eq!(observer.ws_connections(), 1);
        std::thread::spawn(move || drop(guard)).join().unwrap();
        assert_eq!(observer.ws_connections(), 0);
    }

    #[test]
    fn concurrent_tracking_balances() {
        let gauges = Gauges::new(0);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let gauges = gauges.clone();
                std::thread::spawn(move || {
                    for _ in 0..1_000 {
                        let _g = gauges.track_in_flight();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(gauges.in_flight(), 0);
    }

    #[test]
    fn uptime_and_totals() {
        let gauges = Gauges::new(10_000);
        assert_eq!(gauges.uptime_ms(12_500), 2_500);
        assert_eq!(gauges.uptime_ms(5), 0);
        let start = RequestStart::new(
            Protocol::OpenaiChat,
            "POST /v1/chat/completions",
            "m",
            10_000,
        );
        gauges.count_finished(&RecordBuilder::new(start.clone()).finish(200, 10_250));
        gauges.count_finished(&RecordBuilder::new(start).finish(500, 10_100));
        let _held = gauges.track_in_flight();
        let snap = gauges.snapshot(13_000);
        assert_eq!(snap.totals.requests, 2);
        assert_eq!(snap.totals.errors, 1);
        assert_eq!(snap.totals.duration_ms_sum, 350);
        let value = serde_json::to_value(snap).unwrap();
        assert_eq!(value["started_at"], json!(10_000));
        assert_eq!(value["uptime_ms"], json!(3_000));
        assert_eq!(value["in_flight"], json!(1));
        assert_eq!(value["active_streams"], json!(0));
        assert_eq!(value["ws_connections"], json!(0));
        assert_eq!(value["totals"]["requests"], json!(2));
    }
}
