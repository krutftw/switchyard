//! Append-only persistence of request records: one JSON object per line in
//! `<data_dir>/usage/YYYY-MM-DD.jsonl`, where the day is the UTC day the
//! request started.
//!
//! Records are queued in memory by the request path and written in batches
//! by a background task (or by an explicit flush), so recording never waits
//! for the disk.

use crate::record::RequestRecord;
use crate::time::{day_index, parse_day, utc_day};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Notify;

/// Sub-directory of the data dir holding usage files.
pub const USAGE_DIR: &str = "usage";
const EXTENSION: &str = "jsonl";

/// Records queued beyond this are dropped (and counted) instead of growing
/// the queue without bound while the disk is stuck.
const MAX_PENDING: usize = 100_000;
/// Queue length at which the writer is woken without waiting for its timer.
const WAKE_THRESHOLD: usize = 512;

/// What [`crate::UsageStore::load`] found on disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadReport {
    /// Usage files read.
    pub files: usize,
    /// Records restored.
    pub records: usize,
    /// Lines that could not be parsed (corrupt or truncated) and were
    /// skipped.
    pub skipped_lines: usize,
}

/// `<data_dir>/usage`.
pub fn usage_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(USAGE_DIR)
}

fn file_for_day(dir: &Path, day: &str) -> PathBuf {
    dir.join(format!("{day}.{EXTENSION}"))
}

/// Usage files in `dir` as `(day index, path)`, oldest first. Files whose
/// name is not `YYYY-MM-DD.jsonl` are ignored.
fn list_files(dir: &Path) -> Vec<(i64, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(i64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
                return None;
            }
            let day = parse_day(path.file_stem()?.to_str()?)?;
            path.is_file().then_some((day, path))
        })
        .collect();
    files.sort();
    files
}

/// Reads every record of the files whose day is `first_day` or later, oldest
/// file first, calling `sink` for each. Unparseable lines are counted and
/// skipped; an unreadable file ends that file only.
pub(crate) fn load_dir(
    dir: &Path,
    first_day: i64,
    mut sink: impl FnMut(RequestRecord),
) -> LoadReport {
    let mut report = LoadReport::default();
    for (day, path) in list_files(dir) {
        if day < first_day {
            continue;
        }
        let Ok(file) = File::open(&path) else {
            continue;
        };
        report.files += 1;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            // Bytes, not `lines()`: a file cut in the middle of a multi-byte
            // character must cost one line, not the rest of the file.
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            match serde_json::from_str::<RequestRecord>(text) {
                Ok(record) => {
                    report.records += 1;
                    sink(record);
                }
                Err(_) => report.skipped_lines += 1,
            }
        }
    }
    report
}

/// Deletes usage files older than `first_day`; returns how many were
/// removed.
pub(crate) fn prune_dir(dir: &Path, first_day: i64) -> usize {
    list_files(dir)
        .into_iter()
        .filter(|(day, _)| *day < first_day)
        .filter(|(_, path)| fs::remove_file(path).is_ok())
        .count()
}

/// Finds one record by id in the file of the given UTC day.
pub(crate) fn find_in_day(dir: &Path, day: &str, id: &str) -> Option<RequestRecord> {
    let file = File::open(file_for_day(dir, day)).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        // Cheap pre-filter before paying for a JSON parse.
        if !text.contains(id) {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<RequestRecord>(text.trim())
            && record.id == id
        {
            return Some(record);
        }
    }
}

#[derive(Default)]
struct IoState {
    /// Days whose file has been checked for a clean (newline-terminated)
    /// end during this process.
    checked: HashSet<String>,
}

/// Queue + writer for usage files.
pub(crate) struct Persister {
    dir: PathBuf,
    pending: Mutex<Vec<Arc<RequestRecord>>>,
    io: Mutex<IoState>,
    /// Wakes the background writer early when the queue is long.
    pub(crate) wake: Notify,
    dropped: AtomicU64,
}

impl Persister {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Persister {
            dir,
            pending: Mutex::new(Vec::new()),
            io: Mutex::new(IoState::default()),
            wake: Notify::new(),
            dropped: AtomicU64::new(0),
        }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Queues a record for the next batch. Never touches the disk.
    pub(crate) fn enqueue(&self, record: Arc<RequestRecord>) {
        let queued = {
            let mut pending = self.pending.lock();
            if pending.len() >= MAX_PENDING {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            pending.push(record);
            pending.len()
        };
        if queued >= WAKE_THRESHOLD {
            self.wake.notify_one();
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.lock().is_empty()
    }

    pub(crate) fn pending_len(&self) -> usize {
        self.pending.lock().len()
    }

    /// Records dropped because the queue was full.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Writes everything queued so far and returns the number of records
    /// written. Blocking file I/O: call from a blocking context.
    ///
    /// On an I/O error the batch is discarded: retrying forever against a
    /// full or read-only disk would only grow the queue.
    pub(crate) fn write_pending(&self) -> io::Result<usize> {
        // The I/O lock is taken before the queue is drained so concurrent
        // callers cannot write their batches out of order.
        let mut io_state = self.io.lock();
        let batch = std::mem::take(&mut *self.pending.lock());
        if batch.is_empty() {
            return Ok(0);
        }
        fs::create_dir_all(&self.dir)?;
        let mut by_day: BTreeMap<String, String> = BTreeMap::new();
        let mut written = 0;
        for record in &batch {
            let Ok(line) = serde_json::to_string(record.as_ref()) else {
                continue;
            };
            let text = by_day.entry(utc_day(record.started_at)).or_default();
            text.push_str(&line);
            text.push('\n');
            written += 1;
        }
        for (day, text) in by_day {
            let path = file_for_day(&self.dir, &day);
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&path)?;
            if io_state.checked.insert(day) && !ends_with_newline(&mut file)? {
                // A crash left half a line behind; terminate it so the next
                // record does not get glued onto the garbage.
                file.write_all(b"\n")?;
            }
            file.write_all(text.as_bytes())?;
            file.flush()?;
        }
        Ok(written)
    }

    /// Drops the queue and deletes every usage file.
    pub(crate) fn clear(&self) {
        let mut io_state = self.io.lock();
        self.pending.lock().clear();
        io_state.checked.clear();
        for (_, path) in list_files(&self.dir) {
            let _ = fs::remove_file(path);
        }
    }

    /// Deletes files of days before `first_day`.
    pub(crate) fn prune(&self, first_day: i64) -> usize {
        let _io_state = self.io.lock();
        prune_dir(&self.dir, first_day)
    }
}

/// Whether the file is empty or its last byte is a line feed.
fn ends_with_newline(file: &mut File) -> io::Result<bool> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// First day index kept for a retention of `retention_days` days ending at
/// `now`: today and the `retention_days` days before it.
pub(crate) fn first_kept_day(retention_days: u32, now: i64) -> i64 {
    day_index(now) - i64::from(retention_days.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{RecordBuilder, RequestStart};
    use pretty_assertions::assert_eq;
    use switchyard_core::protocol::Protocol;

    const DAY: i64 = 86_400_000;
    /// 2026-10-02T00:00:00Z.
    const T0: i64 = 1_790_899_200_000;

    fn record(id: &str, started_at: i64) -> Arc<RequestRecord> {
        let start = RequestStart::new(
            Protocol::Anthropic,
            "POST /v1/messages",
            "sonnet",
            started_at,
        )
        .with_id(id);
        Arc::new(RecordBuilder::new(start).finish(200, started_at + 100))
    }

    fn read_ids(dir: &Path, first_day: i64) -> (Vec<String>, LoadReport) {
        let mut ids = Vec::new();
        let report = load_dir(dir, first_day, |r| ids.push(r.id));
        (ids, report)
    }

    #[test]
    fn writes_one_file_per_utc_day_of_start() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persister::new(usage_dir(tmp.path()));
        p.enqueue(record("a", T0 + 5));
        p.enqueue(record("b", T0 + DAY - 1));
        p.enqueue(record("c", T0 + DAY));
        assert!(p.has_pending());
        assert_eq!(p.write_pending().unwrap(), 3);
        assert!(!p.has_pending());
        assert_eq!(p.write_pending().unwrap(), 0);

        let first = fs::read_to_string(tmp.path().join("usage/2026-10-02.jsonl")).unwrap();
        assert_eq!(first.lines().count(), 2);
        assert!(first.ends_with('\n'));
        let second = fs::read_to_string(tmp.path().join("usage/2026-10-03.jsonl")).unwrap();
        assert_eq!(second.lines().count(), 1);

        let (ids, report) = read_ids(p.dir(), i64::MIN);
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(
            report,
            LoadReport {
                files: 2,
                records: 3,
                skipped_lines: 0
            }
        );
    }

    #[test]
    fn appends_across_batches_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persister::new(usage_dir(tmp.path()));
        p.enqueue(record("a", T0));
        p.write_pending().unwrap();
        p.enqueue(record("b", T0 + 1));
        p.enqueue(record("c", T0 + 2));
        p.write_pending().unwrap();
        let (ids, _) = read_ids(p.dir(), i64::MIN);
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn corrupt_and_truncated_lines_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = usage_dir(tmp.path());
        fs::create_dir_all(&dir).unwrap();
        let good = |id: &str| serde_json::to_string(record(id, T0).as_ref()).unwrap();
        let mut content = Vec::new();
        content.extend_from_slice(good("a").as_bytes());
        content.extend_from_slice(b"\n\n{not json}\n");
        content.extend_from_slice(b"{\"id\":\"no-protocol\",\"started_at\":1}\n");
        content.extend_from_slice(b"\xff\xfe broken utf8 \xc3\n");
        content.extend_from_slice(good("b").as_bytes());
        content.extend_from_slice(b"\r\n");
        // The process died while writing the last record.
        let half = good("c");
        content.extend_from_slice(&half.as_bytes()[..half.len() / 2]);
        fs::write(dir.join("2026-10-02.jsonl"), content).unwrap();
        fs::write(dir.join("notes.txt"), "ignored").unwrap();
        fs::write(dir.join("not-a-date.jsonl"), "ignored").unwrap();

        let (ids, report) = read_ids(&dir, i64::MIN);
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(
            report,
            LoadReport {
                files: 1,
                records: 2,
                skipped_lines: 4
            }
        );
    }

    #[test]
    fn appending_after_a_truncated_line_starts_on_a_fresh_line() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = usage_dir(tmp.path());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("2026-10-02.jsonl"), "{\"id\":\"cut").unwrap();
        let p = Persister::new(dir.clone());
        p.enqueue(record("a", T0));
        p.write_pending().unwrap();
        p.enqueue(record("b", T0));
        p.write_pending().unwrap();
        let text = fs::read_to_string(dir.join("2026-10-02.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 3);
        let (ids, report) = read_ids(&dir, i64::MIN);
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(report.skipped_lines, 1);
    }

    #[test]
    fn load_skips_files_before_the_first_day() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persister::new(usage_dir(tmp.path()));
        for (i, id) in ["a", "b", "c", "d"].iter().enumerate() {
            p.enqueue(record(id, T0 + i as i64 * DAY));
        }
        p.write_pending().unwrap();
        let (ids, report) = read_ids(p.dir(), day_index(T0) + 2);
        assert_eq!(ids, ["c", "d"]);
        assert_eq!(report.files, 2);
    }

    #[test]
    fn prune_removes_only_old_usage_files() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persister::new(usage_dir(tmp.path()));
        for i in 0..5 {
            p.enqueue(record(&format!("r{i}"), T0 + i * DAY));
        }
        p.write_pending().unwrap();
        fs::write(p.dir().join("keep.txt"), "x").unwrap();
        // Keep today (day 4) and the two days before it.
        let now = T0 + 4 * DAY + 1_000;
        assert_eq!(first_kept_day(2, now), day_index(T0) + 2);
        assert_eq!(p.prune(first_kept_day(2, now)), 2);
        let mut names: Vec<String> = fs::read_dir(p.dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "2026-10-04.jsonl",
                "2026-10-05.jsonl",
                "2026-10-06.jsonl",
                "keep.txt"
            ]
        );
        assert_eq!(p.prune(first_kept_day(2, now)), 0);
    }

    #[test]
    fn zero_retention_keeps_at_least_a_day() {
        assert_eq!(first_kept_day(0, T0), day_index(T0) - 1);
    }

    #[test]
    fn clear_drops_queue_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persister::new(usage_dir(tmp.path()));
        p.enqueue(record("a", T0));
        p.write_pending().unwrap();
        p.enqueue(record("b", T0));
        p.clear();
        assert!(!p.has_pending());
        assert!(list_files(p.dir()).is_empty());
        // Usable again afterwards.
        p.enqueue(record("c", T0));
        p.write_pending().unwrap();
        let (ids, _) = read_ids(p.dir(), i64::MIN);
        assert_eq!(ids, ["c"]);
    }

    #[test]
    fn find_by_id_in_a_day_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persister::new(usage_dir(tmp.path()));
        p.enqueue(record("needle-1", T0));
        p.enqueue(record("needle-10", T0));
        p.write_pending().unwrap();
        assert_eq!(
            find_in_day(p.dir(), "2026-10-02", "needle-10").unwrap().id,
            "needle-10"
        );
        assert_eq!(
            find_in_day(p.dir(), "2026-10-02", "needle-1").unwrap().id,
            "needle-1"
        );
        assert!(find_in_day(p.dir(), "2026-10-02", "needle").is_none());
        assert!(find_in_day(p.dir(), "2026-10-03", "needle-1").is_none());
    }

    #[test]
    fn queue_is_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persister::new(usage_dir(tmp.path()));
        let r = record("a", T0);
        for _ in 0..MAX_PENDING + 3 {
            p.enqueue(Arc::clone(&r));
        }
        assert_eq!(p.pending.lock().len(), MAX_PENDING);
        assert_eq!(p.dropped(), 3);
    }

    #[test]
    fn missing_directory_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let (ids, report) = read_ids(&tmp.path().join("nope"), i64::MIN);
        assert!(ids.is_empty());
        assert_eq!(report, LoadReport::default());
        assert_eq!(prune_dir(&tmp.path().join("nope"), 0), 0);
    }
}
