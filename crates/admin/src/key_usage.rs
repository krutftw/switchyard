//! Usage per client key, by the key's *id*.
//!
//! The usage store breaks its statistics down by the *name* a key had when
//! each request was made. That is the right label for a chart, but it is
//! not the key: a key keeps its id when it is renamed, and a name that was
//! given up can be handed to another key. Read by name, a renamed key
//! would show no usage at all, and a new key would show the traffic of
//! whichever key carried its name before.
//!
//! The request records themselves say which key made them
//! (`client.key_id`), so `GET /keys` counts those instead:
//!
//! * with usage persistence on, from the usage files
//!   (`<data_dir>/usage/YYYY-MM-DD.jsonl`), which hold every record of the
//!   retention window. A file is read once; afterwards only what was
//!   appended to it since. [`KeyLedger`] keeps the per-file sums;
//! * from the requests still in memory, which is all there is when nothing
//!   is persisted, and which covers records that have not reached a file.
//!
//! A key is shown the larger of the two counts: each can only miss
//! requests (the files while the disk refuses writes, the memory once the
//! request list has turned over), never invent one.
//!
//! The range is the usage store's `30d`: the 720 hour buckets ending with
//! the current hour, a request counting at the time it finished, and
//! nothing older than `usage.retention_days`.

use crate::views::KeyUsage;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use switchyard_telemetry::{
    MAX_PAGE_SIZE, RequestQuery, RequestRecord, Telemetry, Totals, UsageStore, utc_day,
};

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;
/// Hours of the reported range (30 days).
const RANGE_HOURS: i64 = 30 * 24;
/// Day files looked at: a file holds the requests that *started* on its
/// day, so the range's 30 days touch 31 files, and one more allows for a
/// request that started before midnight and finished after.
const DAYS_READ: i64 = RANGE_HOURS / 24 + 2;
/// Extension of the usage files.
const EXTENSION: &str = "jsonl";
/// Leading bytes of a file remembered to recognise it. A record begins
/// with its id, a UUID, so two files do not begin alike.
const HEAD_BYTES: usize = 96;
/// Pages of the in-memory request list read at most; far more than the
/// list can hold, there only so that the loop provably ends.
const MAX_PAGES: usize = 10_000;

/// The hour buckets a query reads: `first..=last`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Hours {
    first: i64,
    last: i64,
}

impl Hours {
    /// The 30-day range ending at `now`, cut to the retention window — the
    /// arithmetic of the usage store's own `30d` range.
    fn ending(now: i64, retention_days: u32) -> Hours {
        let last = now.div_euclid(HOUR_MS);
        let kept = i64::from(retention_days.max(1)) * 24;
        Hours {
            first: last - RANGE_HOURS.min(kept) + 1,
            last,
        }
    }

    fn contains(self, hour: i64) -> bool {
        (self.first..=self.last).contains(&hour)
    }
}

/// The hour a record counts in: the one it finished in (records of an
/// older format without a finish time: the one it started in).
fn hour_of(record: &RequestRecord) -> i64 {
    record
        .finished_at
        .max(record.started_at)
        .div_euclid(HOUR_MS)
}

/// The id of the client key that made a request, if one did.
fn key_of(record: &RequestRecord) -> Option<&str> {
    record.client.key_id.as_deref().filter(|id| !id.is_empty())
}

/// What one usage file says about each key.
#[derive(Debug, Default)]
struct DayFile {
    /// Bytes read so far: up to the end of the last complete line.
    consumed: u64,
    /// The first bytes of the file as it was when they were read.
    head: Vec<u8>,
    /// Hour → key id → counters.
    hours: BTreeMap<i64, HashMap<String, Totals>>,
    /// Key id → start of its most recent request in this file.
    last_used: HashMap<String, i64>,
}

impl DayFile {
    fn add(&mut self, record: &RequestRecord) {
        let Some(key) = key_of(record) else {
            return;
        };
        self.hours
            .entry(hour_of(record))
            .or_default()
            .entry(key.to_string())
            .or_default()
            .add_record(record);
        let last = self.last_used.entry(key.to_string()).or_insert(i64::MIN);
        *last = (*last).max(record.started_at);
    }

    /// Reads what was appended to the file since the last call. A file
    /// that is no longer the one that was read (it shrank, or begins
    /// differently: the statistics were cleared and the day's file started
    /// anew) is read from its beginning.
    fn catch_up(&mut self, path: &Path) -> std::io::Result<()> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        if !self.is_still(&mut file, length)? {
            *self = DayFile::default();
        }

        if length > self.consumed {
            file.seek(SeekFrom::Start(self.consumed))?;
            let mut reader = BufReader::new(&mut file);
            let mut line = Vec::new();
            loop {
                line.clear();
                // Bytes, not `lines()`: a damaged line must cost that line,
                // not the rest of the file.
                let read = reader.read_until(b'\n', &mut line)?;
                if read == 0 || line.last() != Some(&b'\n') {
                    // The end, or a line still being written: next time.
                    break;
                }
                self.consumed += read as u64;
                if let Ok(record) = serde_json::from_slice::<RequestRecord>(line.trim_ascii()) {
                    self.add(&record);
                }
            }
        }

        // Remember how the file begins — as far as it has been read: the
        // rest of a line still being written may not stay as it is.
        let certain = self.consumed.min(HEAD_BYTES as u64);
        if (self.head.len() as u64) < certain {
            file.seek(SeekFrom::Start(0))?;
            let mut head = Vec::with_capacity(HEAD_BYTES);
            (&mut file).take(certain).read_to_end(&mut head)?;
            self.head = head;
        }
        Ok(())
    }

    /// Whether `file` is the file this was read from: at least as long as
    /// what was read, and beginning the same way.
    fn is_still(&self, file: &mut File, length: u64) -> std::io::Result<bool> {
        if length < self.consumed {
            return Ok(false);
        }
        let mut head = vec![0u8; self.head.len()];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut head)?;
        Ok(head == self.head)
    }
}

#[derive(Debug, Default)]
struct Files {
    /// The directory the sums below were read from.
    dir: Option<PathBuf>,
    /// Day (`YYYY-MM-DD`) → what its file says.
    days: HashMap<String, DayFile>,
}

impl Files {
    /// Brings the sums up to date with the files in `dir` and adds up the
    /// range.
    fn usage(&mut self, dir: &Path, now: i64, hours: Hours) -> HashMap<String, KeyUsage> {
        if self.dir.as_deref() != Some(dir) {
            self.dir = Some(dir.to_path_buf());
            self.days.clear();
        }
        let wanted: HashSet<String> = (0..DAYS_READ)
            .map(|days_ago| utc_day(now.saturating_sub(days_ago * DAY_MS)))
            .collect();
        self.days.retain(|day, _| wanted.contains(day));
        for day in wanted {
            let path = dir.join(format!("{day}.{EXTENSION}"));
            if !path.is_file() {
                // Pruned, cleared, or a day without requests.
                self.days.remove(&day);
                continue;
            }
            let entry = self.days.entry(day).or_default();
            if let Err(error) = entry.catch_up(&path) {
                // What was read before the failure stays counted; the rest
                // is picked up by the next call.
                tracing::debug!(path = %path.display(), %error, "a usage file could not be read");
            }
        }

        let mut usage: HashMap<String, KeyUsage> = HashMap::new();
        for file in self.days.values() {
            for by_key in file.hours.range(hours.first..=hours.last).map(|(_, v)| v) {
                for (key, totals) in by_key {
                    usage.entry(key.clone()).or_default().totals.merge(totals);
                }
            }
            for (key, started_at) in &file.last_used {
                let entry = usage.entry(key.clone()).or_default();
                entry.last_used_at = entry.last_used_at.max(Some(*started_at));
            }
        }
        usage
    }
}

/// Usage of every key that made one of the requests still in memory.
fn in_memory(store: &UsageStore, hours: Hours) -> HashMap<String, KeyUsage> {
    let mut usage: HashMap<String, KeyUsage> = HashMap::new();
    let mut before = None;
    for _ in 0..MAX_PAGES {
        let page = store.requests(&RequestQuery {
            limit: Some(MAX_PAGE_SIZE),
            before: before.take(),
            ..RequestQuery::default()
        });
        for record in &page.items {
            let Some(key) = key_of(record) else {
                continue;
            };
            let entry = usage.entry(key.to_string()).or_default();
            entry.last_used_at = entry.last_used_at.max(Some(record.started_at));
            if hours.contains(hour_of(record)) {
                entry.totals.add_record(record);
            }
        }
        match page.next_before {
            Some(cursor) => before = Some(cursor),
            None => break,
        }
    }
    usage
}

/// Per-key usage read from the usage files, kept between calls so that a
/// file is not read twice.
#[derive(Debug, Default)]
pub(crate) struct KeyLedger {
    files: Mutex<Files>,
}

impl KeyLedger {
    /// Usage of every key id seen in the range ending at `now` (unix ms).
    /// Keys without requests are absent.
    ///
    /// Blocking: writes out the records the usage store still has queued,
    /// then reads what the usage files gained since the last call (all of
    /// them on the first).
    pub fn usage(&self, telemetry: &Telemetry, now: i64) -> HashMap<String, KeyUsage> {
        let store = telemetry.usage();
        let hours = Hours::ending(now, store.retention_days());
        let mut usage = in_memory(store, hours);
        let Some(dir) = store.persist_dir() else {
            return usage;
        };
        if let Err(error) = store.flush_blocking() {
            // The store itself warns about a disk that refuses writes; the
            // requests in memory are counted above.
            tracing::debug!(%error, "queued usage records could not be written before counting");
        }
        let on_disk = self.files.lock().usage(&dir, now, hours);
        for (key, disk) in on_disk {
            let entry = usage.entry(key).or_default();
            if disk.totals.requests >= entry.totals.requests {
                entry.totals = disk.totals;
            }
            entry.last_used_at = entry.last_used_at.max(disk.last_used_at);
        }
        usage
    }

    /// Forgets what was read: the statistics were cleared.
    pub fn reset(&self) {
        *self.files.lock() = Files::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::io::Write;
    use switchyard_telemetry::new_request_id;

    /// 2026-10-02T12:00:00Z.
    const NOON: i64 = 1_790_942_400_000;

    fn record(key: Option<(&str, &str)>, started_at: i64, tokens: u64, ok: bool) -> RequestRecord {
        let (key_id, key_name) = key.unzip();
        serde_json::from_value(json!({
            "id": new_request_id(),
            "started_at": started_at,
            "finished_at": started_at + 20,
            "duration_ms": 20,
            "client": {"key_id": key_id, "key_name": key_name},
            "client_protocol": "openai-chat",
            "endpoint": "POST /v1/chat/completions",
            "requested_model": "m",
            "status": if ok { 200 } else { 500 },
            "ok": ok,
            "usage": {"input_tokens": tokens},
        }))
        .unwrap()
    }

    fn append(dir: &Path, record: &RequestRecord) {
        let path = dir.join(format!("{}.{EXTENSION}", utc_day(record.started_at)));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        let mut line = serde_json::to_vec(record).unwrap();
        line.push(b'\n');
        file.write_all(&line).unwrap();
    }

    fn hours() -> Hours {
        Hours::ending(NOON, 30)
    }

    #[test]
    fn the_range_is_the_usage_stores_thirty_days() {
        let last = NOON / HOUR_MS;
        assert_eq!(
            Hours::ending(NOON, 30),
            Hours {
                first: last - 719,
                last
            }
        );
        // A longer retention does not widen the range; a shorter one cuts
        // it; none at all still keeps a day.
        assert_eq!(Hours::ending(NOON, 90).first, last - 719);
        assert_eq!(Hours::ending(NOON, 7).first, last - 167);
        assert_eq!(Hours::ending(NOON, 0).first, last - 23);
    }

    #[test]
    fn usage_follows_the_key_id_whatever_the_name_was() {
        let dir = tempfile::tempdir().unwrap();
        // One key under two names, and another key that later took over
        // the first name.
        append(
            dir.path(),
            &record(Some(("key_a", "tester")), NOON - 5_000, 10, true),
        );
        append(
            dir.path(),
            &record(Some(("key_a", "laptop")), NOON - 4_000, 5, false),
        );
        append(
            dir.path(),
            &record(Some(("key_b", "tester")), NOON - 3_000, 1, true),
        );
        append(dir.path(), &record(None, NOON - 2_000, 100, true));

        let mut files = Files::default();
        let usage = files.usage(dir.path(), NOON, hours());
        assert_eq!(usage.len(), 2);
        let a = usage["key_a"];
        assert_eq!((a.totals.requests, a.totals.errors), (2, 1));
        assert_eq!(a.totals.input_tokens, 15);
        assert_eq!(a.last_used_at, Some(NOON - 4_000));
        let b = usage["key_b"];
        assert_eq!((b.totals.requests, b.totals.input_tokens), (1, 1));
        assert_eq!(b.last_used_at, Some(NOON - 3_000));
    }

    #[test]
    fn a_file_is_read_once_and_then_only_its_new_lines() {
        let dir = tempfile::tempdir().unwrap();
        let first = record(Some(("key_a", "a")), NOON - 5_000, 1, true);
        append(dir.path(), &first);
        let mut files = Files::default();
        assert_eq!(
            files.usage(dir.path(), NOON, hours())["key_a"]
                .totals
                .requests,
            1
        );
        let path = dir.path().join(format!("{}.{EXTENSION}", utc_day(NOON)));
        let length = std::fs::metadata(&path).unwrap().len();
        assert_eq!(files.days[&utc_day(NOON)].consumed, length);

        // Nothing new: nothing is counted twice.
        assert_eq!(
            files.usage(dir.path(), NOON, hours())["key_a"]
                .totals
                .requests,
            1
        );

        // A line still being written is left for the next call, and a line
        // that is not a record costs only itself.
        let second =
            serde_json::to_vec(&record(Some(("key_a", "a")), NOON - 1_000, 1, true)).unwrap();
        let (begun, rest) = second.split_at(40);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"this is not a record\n").unwrap();
        file.write_all(begun).unwrap();
        assert_eq!(
            files.usage(dir.path(), NOON, hours())["key_a"]
                .totals
                .requests,
            1
        );
        file.write_all(rest).unwrap();
        file.write_all(b"\n").unwrap();
        let usage = files.usage(dir.path(), NOON, hours());
        assert_eq!(usage["key_a"].totals.requests, 2);
        assert_eq!(usage["key_a"].last_used_at, Some(NOON - 1_000));
    }

    #[test]
    fn a_file_that_was_replaced_or_removed_is_not_remembered() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..3 {
            append(
                dir.path(),
                &record(Some(("key_a", "a")), NOON - 9_000 + i, 1, true),
            );
        }
        let mut files = Files::default();
        assert_eq!(
            files.usage(dir.path(), NOON, hours())["key_a"]
                .totals
                .requests,
            3
        );

        // Cleared and grown past its old length before anyone looked: it
        // begins differently, so it is read afresh.
        let path = dir.path().join(format!("{}.{EXTENSION}", utc_day(NOON)));
        std::fs::remove_file(&path).unwrap();
        for i in 0..5 {
            append(
                dir.path(),
                &record(Some(("key_b", "b")), NOON - 900 + i, 1, true),
            );
        }
        let usage = files.usage(dir.path(), NOON, hours());
        assert!(!usage.contains_key("key_a"), "{usage:?}");
        assert_eq!(usage["key_b"].totals.requests, 5);

        // Shorter than what was read: likewise.
        std::fs::remove_file(&path).unwrap();
        append(
            dir.path(),
            &record(Some(("key_c", "c")), NOON - 100, 1, true),
        );
        let usage = files.usage(dir.path(), NOON, hours());
        assert_eq!(usage.keys().collect::<Vec<_>>(), ["key_c"]);

        // Gone.
        std::fs::remove_file(&path).unwrap();
        assert!(files.usage(dir.path(), NOON, hours()).is_empty());

        // Another directory starts from nothing.
        let other = tempfile::tempdir().unwrap();
        append(
            dir.path(),
            &record(Some(("key_c", "c")), NOON - 100, 1, true),
        );
        assert_eq!(files.usage(dir.path(), NOON, hours()).len(), 1);
        assert!(files.usage(other.path(), NOON, hours()).is_empty());
    }

    #[test]
    fn requests_outside_the_range_are_not_counted() {
        let dir = tempfile::tempdir().unwrap();
        // Finished in the last hour of the range's first day, 30 days ago.
        let old = NOON - 30 * DAY_MS + HOUR_MS;
        append(dir.path(), &record(Some(("key_a", "a")), old, 1, true));
        // An hour earlier: outside.
        append(
            dir.path(),
            &record(Some(("key_a", "a")), old - HOUR_MS, 1, true),
        );
        // Started the day before the range's first day's file and finished
        // inside the range: its file is read too.
        let mut straddling = record(Some(("key_b", "b")), NOON - 31 * DAY_MS, 1, true);
        straddling.finished_at = old;
        append(dir.path(), &straddling);
        // Far outside: the file is not even opened.
        append(
            dir.path(),
            &record(Some(("key_c", "c")), NOON - 40 * DAY_MS, 1, true),
        );
        // Stamped in a future hour: not yet.
        append(
            dir.path(),
            &record(Some(("key_d", "d")), NOON + 2 * HOUR_MS, 1, true),
        );

        let mut files = Files::default();
        let usage = files.usage(dir.path(), NOON, hours());
        assert_eq!(usage["key_a"].totals.requests, 1);
        assert_eq!(usage["key_b"].totals.requests, 1);
        assert!(!usage.contains_key("key_c"));
        assert_eq!(usage["key_d"].totals.requests, 0);

        // A week of retention: the old requests are past it.
        let week = Hours::ending(NOON, 7);
        let usage = files.usage(dir.path(), NOON, week);
        assert_eq!(usage["key_a"].totals.requests, 0);
        assert_eq!(usage["key_b"].totals.requests, 0);
    }

    #[test]
    fn the_requests_in_memory_are_counted_by_key_id() {
        let store = UsageStore::in_memory();
        let now = switchyard_core::util::now_unix_ms();
        for i in 0..(MAX_PAGE_SIZE as i64 + 20) {
            store.record(&record(
                Some(("key_a", "tester")),
                now - 10_000 + i,
                2,
                true,
            ));
        }
        store.record(&record(Some(("key_b", "tester")), now - 500, 7, false));
        store.record(&record(None, now - 400, 1, true));
        // In the list, but past the range.
        store.record(&record(Some(("key_c", "old")), now - 31 * DAY_MS, 1, true));

        let usage = in_memory(&store, Hours::ending(now, 30));
        assert_eq!(usage["key_a"].totals.requests, MAX_PAGE_SIZE as u64 + 20);
        assert_eq!(
            usage["key_a"].totals.input_tokens,
            2 * (MAX_PAGE_SIZE as u64 + 20)
        );
        assert_eq!(
            usage["key_a"].last_used_at,
            Some(now - 10_000 + MAX_PAGE_SIZE as i64 + 19)
        );
        assert_eq!(
            (usage["key_b"].totals.requests, usage["key_b"].totals.errors),
            (1, 1)
        );
        assert_eq!(usage["key_c"].totals.requests, 0);
        assert_eq!(usage["key_c"].last_used_at, Some(now - 31 * DAY_MS));
        assert_eq!(usage.len(), 3);
    }
}
