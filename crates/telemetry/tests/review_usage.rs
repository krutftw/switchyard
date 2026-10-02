//! Regression tests from the adversarial review of the usage store, through
//! the crate's public API only.
//!
//! Each test failed against the first implementation; the comments describe
//! the defect that was fixed.

use switchyard_core::protocol::Protocol;
use switchyard_core::usage::Usage;
use switchyard_telemetry::{
    Range, RecordBuilder, RequestRecord, RequestStart, Totals, UsageStore, UsageStoreOptions,
};

/// 2026-10-02T00:00:00Z.
const T0: i64 = 1_790_899_200_000;
const MINUTE: i64 = 60_000;
const HOUR: i64 = 3_600_000;

fn record(id: &str, started_at: i64, finished_at: i64, usage: Usage) -> RequestRecord {
    let start = RequestStart::new(
        Protocol::OpenaiChat,
        "POST /v1/chat/completions",
        "gpt-5",
        started_at,
    )
    .with_id(id);
    let mut b = RecordBuilder::new(start);
    b.set_usage(usage);
    b.finish(200, finished_at)
}

// ---------------------------------------------------------------------------
// U1. Token counters are summed with plain `+`. Token counts come from the
// upstream's JSON; core's `u64_field` accepts floats and saturates, so an
// upstream answering `"prompt_tokens": 1e30` yields `u64::MAX`. The second
// such record overflows: a panic in debug/test builds (inside the request
// path, with the store mutex held), a silent wrap to garbage totals in
// release. DESIGN "Rules for everyone": panics are bugs, no trust in data
// from the network. Counters should saturate.
// ---------------------------------------------------------------------------

#[test]
fn u1_hostile_token_counts_do_not_overflow_the_store() {
    let huge = Usage {
        input_tokens: u64::MAX,
        ..Usage::default()
    };
    let outcome = std::panic::catch_unwind(|| {
        let store = UsageStore::in_memory();
        store.record(&record("a", T0, T0 + 500, huge));
        store.record(&record("b", T0 + 1, T0 + 501, huge));
        store.summary(Range::Hour, T0 + 1_000).totals
    });
    let totals = outcome.expect("recording a huge token count must not panic");
    assert_eq!(totals.requests, 2);
    assert_eq!(
        totals.input_tokens,
        u64::MAX,
        "the sum must saturate, not wrap"
    );
}

#[test]
fn u1_totals_add_record_saturates() {
    let huge = Usage {
        output_tokens: u64::MAX,
        ..Usage::default()
    };
    let outcome = std::panic::catch_unwind(|| {
        let mut totals = Totals::default();
        totals.add_record(&record("a", T0, T0 + 500, huge));
        totals.add_record(&record("b", T0, T0 + 500, huge));
        let mut merged = totals;
        merged.merge(&totals);
        merged
    });
    let merged = outcome.expect("Totals::add_record / merge must not panic on overflow");
    assert_eq!(merged.output_tokens, u64::MAX);
    assert_eq!(merged.requests, 4);
}

// ---------------------------------------------------------------------------
// U2. Acceptance and eviction are relative to the newest timestamp EVER
// seen (`State::latest`), not to the time of the query. One record stamped
// in the future (the clock was ahead and has been corrected, or a line in a
// usage file within the one-day load tolerance) makes the store silently
// drop every later record from the windows it is "too old" for, so the
// dashboard shows no traffic although requests are being served.
// store.rs says about loading: "one line with a bogus timestamp must not
// evict real history"; the same has to hold for live recording.
// ---------------------------------------------------------------------------

#[test]
fn u2_one_future_record_does_not_blind_the_live_statistics() {
    let store = UsageStore::in_memory();
    // A request that "finished" three days from now.
    store.record(&record("future", T0, T0 + 72 * HOUR, Usage::default()));
    // Normal traffic afterwards.
    for i in 0..5 {
        store.record(&record(
            &format!("r{i}"),
            T0 + i * 1_000,
            T0 + i * 1_000 + 400,
            Usage::default(),
        ));
    }
    let now = T0 + 10_000;
    assert_eq!(store.summary(Range::Hour, now).totals.requests, 5);
    assert_eq!(store.summary(Range::Day, now).totals.requests, 5);
    assert_eq!(store.stats_tick(now).rpm, 5);
    assert_eq!(store.summary(Range::Hour, now).latency.samples, 5);
}

#[test]
fn u2_a_usage_file_line_slightly_in_the_future_does_not_blind_rates_after_load() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("usage");
    let now = T0 + 12 * HOUR;
    {
        let writer = UsageStore::new(UsageStoreOptions {
            persist_dir: Some(dir.clone()),
            ..UsageStoreOptions::default()
        });
        // Written while the clock was 20 hours ahead: inside the one-day
        // tolerance `load` accepts.
        writer.record(&record("skewed", now, now + 20 * HOUR, Usage::default()));
        writer.flush_blocking().unwrap();
    }
    let store = UsageStore::new(UsageStoreOptions {
        persist_dir: Some(dir.clone()),
        ..UsageStoreOptions::default()
    });
    store.load(&dir, 30, now);
    // Traffic after the restart, at the real time.
    for i in 0..4 {
        let at = now + MINUTE + i * 1_000;
        store.record(&record(&format!("live{i}"), at, at + 300, Usage::default()));
    }
    let query_at = now + MINUTE + 10_000;
    let tick = store.stats_tick(query_at);
    assert_eq!(
        tick.rpm, 4,
        "requests of the last minute are missing: {tick:?}"
    );
    assert_eq!(
        store.summary(Range::Hour, query_at).latency.samples,
        4,
        "latency samples of the last hour are missing"
    );
}

// ---------------------------------------------------------------------------
// U3. The recent ring evicts by START time, not by arrival. A request that
// ran long (a slow reasoning stream, a Responses/Realtime WebSocket session;
// DESIGN §10 records "one request record per session") is recorded when it
// finishes; if `capacity` other requests started after it and finished
// first, it is the "oldest" entry the moment it is inserted and is evicted
// immediately. The request the gateway finished a millisecond ago is then
// missing from `GET /requests` and from `get(id)`, although the track asks
// for "a ring of the most recent N records ... and lookup by id".
// ---------------------------------------------------------------------------

#[test]
fn u3_a_long_running_request_is_listed_when_it_finishes() {
    let store = UsageStore::new(UsageStoreOptions {
        recent_capacity: 3,
        ..UsageStoreOptions::default()
    });
    // A session that started first and is still running.
    let long_started = T0;
    // Three short requests start and finish while it runs.
    for i in 0..3 {
        let at = T0 + MINUTE + i * 1_000;
        store.record(&record(
            &format!("short{i}"),
            at,
            at + 200,
            Usage::default(),
        ));
    }
    // The long one finishes now: the most recently recorded request.
    let finished = T0 + 10 * MINUTE;
    store.record(&record("long", long_started, finished, Usage::default()));

    assert!(
        store.get("long").is_some(),
        "the request that finished last is not in the recent ring"
    );
    let page = store.requests(&switchyard_telemetry::RequestQuery::default());
    let ids: Vec<&str> = page.items.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"long"), "request list: {ids:?}");
    assert_eq!(page.items.len(), 3);
}
