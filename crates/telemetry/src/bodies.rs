//! Body capture: the client request, the upstream request, the upstream
//! response and the client response of a request, stored per request id
//! under `<data_dir>/requests/YYYY-MM-DD/<id>.json` for the dashboard's
//! request inspector.
//!
//! Bodies are redacted (see [`crate::redact`]) and truncated before they
//! reach the disk. Capture is opt-in through `logging.request_log`: `errors`
//! keeps failed requests only, `all` keeps everything.

use crate::record::{RequestRecord, request_id_time_ms};
use crate::redact::{redact_body, redact_header_value};
use crate::time::{DAY_MS, day_index, parse_day, utc_day};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use switchyard_core::config::RequestLogMode;

/// Sub-directory of the data dir holding captured bodies.
pub const REQUESTS_DIR: &str = "requests";

/// Default per-body size limit (`logging.request_log_max_body_kb = 256`).
pub const DEFAULT_MAX_BODY_BYTES: usize = 256 * 1024;

/// Bodies larger than this are cut before redaction, so the cost of scanning
/// a body is bounded whatever a client sends.
const REDACTION_SCAN_LIMIT: usize = 8 * 1024 * 1024;

/// The bodies and headers of one request.
///
/// Fill the bodies with the raw text; [`BodyStore`] redacts and truncates
/// them. Build the header maps with [`crate::redact_headers`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CapturedBodies {
    /// What the client sent.
    pub client_request: Option<String>,
    /// What was sent upstream on the last attempt (after translation and
    /// payload rules).
    pub upstream_request: Option<String>,
    /// What the upstream answered on the last attempt; for a stream, the
    /// raw event text.
    pub upstream_response: Option<String>,
    /// What the client received.
    pub client_response: Option<String>,
    /// Request headers of the client, redacted.
    pub client_headers: BTreeMap<String, String>,
    /// Request headers sent upstream, redacted.
    pub upstream_headers: BTreeMap<String, String>,
}

impl CapturedBodies {
    pub fn is_empty(&self) -> bool {
        self.client_request.is_none()
            && self.upstream_request.is_none()
            && self.upstream_response.is_none()
            && self.client_response.is_none()
            && self.client_headers.is_empty()
            && self.upstream_headers.is_empty()
    }
}

/// Cuts `body` to at most `max_bytes` bytes (on a character boundary) and
/// appends `…[truncated N bytes]`, N being the number of bytes removed.
/// Shorter bodies are returned unchanged.
pub fn truncate_body(body: &str, max_bytes: usize) -> String {
    truncate_with_extra(body, max_bytes, 0)
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// [`truncate_body`] for a body of which `already_cut` bytes were removed
/// earlier.
fn truncate_with_extra(body: &str, max_bytes: usize, already_cut: usize) -> String {
    if body.len() <= max_bytes && already_cut == 0 {
        return body.to_string();
    }
    let keep = floor_char_boundary(body, max_bytes);
    let removed = body.len() - keep + already_cut;
    format!("{}…[truncated {removed} bytes]", &body[..keep])
}

/// Redacts secrets in a body, then truncates it to `max_bytes`.
///
/// Redaction comes first so a secret can never survive by straddling the
/// cut. Only a body beyond 8 MiB is cut before it is scanned, to bound the
/// cost of redaction.
pub fn prepare_body(body: &str, max_bytes: usize) -> String {
    let scan_limit = max_bytes.max(REDACTION_SCAN_LIMIT);
    let scanned = &body[..floor_char_boundary(body, scan_limit)];
    truncate_with_extra(&redact_body(scanned), max_bytes, body.len() - scanned.len())
}

/// Whether an id is safe to use as a file name: short, and made of letters,
/// digits, `-` and `_` only (no separators, no dots).
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[derive(Clone, Copy, Debug)]
struct Settings {
    mode: RequestLogMode,
    max_body_bytes: usize,
}

struct Inner {
    /// `<data_dir>/requests`; `None` when the gateway has no data dir.
    dir: Option<PathBuf>,
    settings: RwLock<Settings>,
    /// Bodies accepted for storing whose file is not written yet, by
    /// request id, as they were handed in (not redacted yet). The record of
    /// such a request already says `has_bodies`, so [`BodyStore::read`]
    /// answers from here until the file exists.
    held: Mutex<HashMap<String, Arc<CapturedBodies>>>,
}

impl std::fmt::Debug for Inner {
    // The held bodies are raw: never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BodyStore")
            .field("dir", &self.dir)
            .field("settings", &self.settings)
            .field("held", &self.held.lock().len())
            .finish()
    }
}

/// Bodies on their way to disk (see [`BodyStore::hold`]). While this value
/// lives, [`BodyStore::read`] serves them from memory; dropping it — after
/// the file was written, or because the write never ran — ends that.
pub(crate) struct HeldBodies {
    store: BodyStore,
    id: String,
    bodies: Arc<CapturedBodies>,
}

impl HeldBodies {
    /// Writes the held bodies to their file (blocking file I/O). Returns
    /// whether a file was written.
    pub(crate) fn write(self, started_at: i64, failed: bool) -> io::Result<bool> {
        // `self` is dropped when this returns: the file is in place before
        // the copy in memory goes away, so a reader never finds neither.
        self.store
            .write_file(&self.id, started_at, failed, &self.bodies)
    }
}

impl Drop for HeldBodies {
    fn drop(&mut self) {
        let mut held = self.store.inner.held.lock();
        // Only this capture's own entry: a later capture under the same id
        // has replaced it and is released by its own guard.
        if held
            .get(&self.id)
            .is_some_and(|current| Arc::ptr_eq(current, &self.bodies))
        {
            held.remove(&self.id);
        }
    }
}

/// Stores and retrieves captured bodies. Cloning is cheap; clones share the
/// settings.
#[derive(Clone)]
pub struct BodyStore {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for BodyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

impl BodyStore {
    /// A store writing under `<data_dir>/requests`. Without a data dir
    /// nothing is ever captured.
    ///
    /// `max_body_bytes` is the limit for each of the four bodies; values
    /// below 1 KiB are raised to 1 KiB.
    pub fn new(data_dir: Option<&Path>, mode: RequestLogMode, max_body_bytes: usize) -> Self {
        BodyStore {
            inner: Arc::new(Inner {
                dir: data_dir.map(|dir| dir.join(REQUESTS_DIR)),
                settings: RwLock::new(Settings {
                    mode,
                    max_body_bytes: max_body_bytes.max(1024),
                }),
                held: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// A store that never captures.
    pub fn disabled() -> Self {
        BodyStore::new(None, RequestLogMode::Off, DEFAULT_MAX_BODY_BYTES)
    }

    /// `<data_dir>/requests`.
    pub fn dir(&self) -> Option<&Path> {
        self.inner.dir.as_deref()
    }

    /// The capture mode in effect; always `Off` without a data dir.
    pub fn mode(&self) -> RequestLogMode {
        if self.inner.dir.is_some() {
            self.inner.settings.read().mode
        } else {
            RequestLogMode::Off
        }
    }

    pub fn max_body_bytes(&self) -> usize {
        self.inner.settings.read().max_body_bytes
    }

    /// Applies new settings (hot reload).
    pub fn configure(&self, mode: RequestLogMode, max_body_bytes: usize) {
        *self.inner.settings.write() = Settings {
            mode,
            max_body_bytes: max_body_bytes.max(1024),
        };
    }

    /// Whether the bodies of a request with this outcome would be kept.
    /// With mode `errors` the outcome is only known at the end, so collect
    /// bodies whenever `wants(true)` holds and let [`capture`] decide.
    ///
    /// [`capture`]: BodyStore::capture
    pub fn wants(&self, failed: bool) -> bool {
        match self.mode() {
            RequestLogMode::Off => false,
            RequestLogMode::Errors => failed,
            RequestLogMode::All => true,
        }
    }

    /// [`wants`](BodyStore::wants) for a specific request: also false when
    /// the id cannot serve as a file name (anything but letters, digits,
    /// `-` and `_`, or longer than 128 characters).
    pub fn accepts(&self, id: &str, failed: bool) -> bool {
        self.wants(failed) && is_safe_id(id)
    }

    /// Redacts and truncates bodies and re-checks the header maps, exactly
    /// as [`capture`](BodyStore::capture) does before writing.
    pub fn prepare(&self, bodies: CapturedBodies) -> CapturedBodies {
        self.prepared(&bodies)
    }

    /// [`prepare`](BodyStore::prepare) from a reference.
    fn prepared(&self, bodies: &CapturedBodies) -> CapturedBodies {
        let max = self.max_body_bytes();
        let body = |text: &Option<String>| text.as_deref().map(|text| prepare_body(text, max));
        let headers = |map: &BTreeMap<String, String>| -> BTreeMap<String, String> {
            map.iter()
                .map(|(name, value)| {
                    // Already redacted values pass through unchanged; a raw
                    // secret that slipped in is caught here.
                    (name.clone(), redact_header_value(name, value))
                })
                .collect()
        };
        CapturedBodies {
            client_request: body(&bodies.client_request),
            upstream_request: body(&bodies.upstream_request),
            upstream_response: body(&bodies.upstream_response),
            client_response: body(&bodies.client_response),
            client_headers: headers(&bodies.client_headers),
            upstream_headers: headers(&bodies.upstream_headers),
        }
    }

    fn path_for(dir: &Path, started_at: i64, id: &str) -> PathBuf {
        dir.join(utc_day(started_at)).join(format!("{id}.json"))
    }

    /// Stores the bodies of a finished request when the mode asks for it
    /// (`all`, or `errors` and the request was not ok). Returns whether a
    /// file was written, which is the record's `has_bodies`.
    ///
    /// Blocking file I/O: call from a blocking context
    /// ([`crate::Telemetry::capture_bodies`] takes care of that).
    pub fn capture(&self, record: &RequestRecord, bodies: CapturedBodies) -> io::Result<bool> {
        self.capture_at(&record.id, record.started_at, !record.ok, bodies)
    }

    /// [`capture`](BodyStore::capture) without a record: `started_at`
    /// selects the day directory, `failed` is the request outcome.
    pub fn capture_at(
        &self,
        id: &str,
        started_at: i64,
        failed: bool,
        bodies: CapturedBodies,
    ) -> io::Result<bool> {
        self.write_file(id, started_at, failed, &bodies)
    }

    /// Takes bodies that are about to be written by another thread and
    /// makes them readable at once: until the returned guard is dropped,
    /// [`read`](BodyStore::read) answers for `id` from memory (redacted and
    /// truncated like the file will be). The caller writes the file with
    /// [`HeldBodies::write`].
    ///
    /// This is what lets a record say `has_bodies` the moment it is
    /// published although the file is still being written.
    pub(crate) fn hold(&self, id: &str, bodies: CapturedBodies) -> HeldBodies {
        let bodies = Arc::new(bodies);
        self.inner
            .held
            .lock()
            .insert(id.to_string(), Arc::clone(&bodies));
        HeldBodies {
            store: self.clone(),
            id: id.to_string(),
            bodies,
        }
    }

    fn write_file(
        &self,
        id: &str,
        started_at: i64,
        failed: bool,
        bodies: &CapturedBodies,
    ) -> io::Result<bool> {
        let Some(dir) = self.dir() else {
            return Ok(false);
        };
        if !self.accepts(id, failed) {
            return Ok(false);
        }
        let path = Self::path_for(dir, started_at, id);
        let text = serde_json::to_vec(&self.prepared(bodies)).map_err(io::Error::other)?;
        if let Some(parent) = path.parent() {
            crate::private_files::create_dir_all(parent)?;
        }
        // Written under a temporary name first, so a reader never sees half
        // a file.
        let tmp = path.with_extension("json.tmp");
        let mut file =
            crate::private_files::open(fs::OpenOptions::new().create_new(true).write(true), &tmp)?;
        if let Err(error) = file.write_all(&text) {
            drop(file);
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        drop(file);
        match fs::rename(&tmp, &path) {
            Ok(()) => Ok(true),
            Err(error) => {
                let _ = fs::remove_file(&tmp);
                Err(error)
            }
        }
    }

    fn read_file(path: &Path) -> Option<CapturedBodies> {
        let bytes = fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// The captured bodies of a request, if any (blocking file I/O).
    ///
    /// A UUIDv7 id says which day to look in; for any other id the day
    /// directories are searched newest first.
    ///
    /// Bodies whose file is still being written (see
    /// [`crate::Telemetry::capture_bodies`]) are answered from memory, in
    /// the form the file will have: a request whose record says
    /// `has_bodies` can be read from the moment the record exists.
    pub fn read(&self, id: &str) -> Option<CapturedBodies> {
        let dir = self.dir()?;
        if !is_safe_id(id) {
            return None;
        }
        // Looked up before the directory: the write puts the file in place
        // first and lets go of the copy in memory afterwards, so whichever
        // is found is complete.
        let held = self.inner.held.lock().get(id).cloned();
        if let Some(bodies) = held {
            return Some(self.prepared(&bodies));
        }
        let name = format!("{id}.json");
        if let Some(minted_at) = request_id_time_ms(id) {
            // The id is minted next to, not at, the recorded start time.
            for shift in [0, -DAY_MS, DAY_MS] {
                let path = dir.join(utc_day(minted_at + shift)).join(&name);
                if path.is_file() {
                    return Self::read_file(&path);
                }
            }
        }
        day_dirs(dir)
            .into_iter()
            .rev()
            .map(|(_, day_dir)| day_dir.join(&name))
            .find(|path| path.is_file())
            .and_then(|path| Self::read_file(&path))
    }

    /// Deletes captured bodies that are too old or too many (blocking file
    /// I/O) and returns the number of files removed.
    ///
    /// * day directories older than `retention_days` (counting back from
    ///   `now`) are removed entirely;
    /// * when `max_total_bytes` is not zero and the remaining files exceed
    ///   it, the oldest files are removed until they fit.
    ///
    /// Day directories left empty are removed, except those of today and
    /// yesterday: [`capture_at`](BodyStore::capture_at) creates the day
    /// directory and then writes into it, and removing the still-empty
    /// directory in between would lose that capture.
    pub fn prune(&self, now: i64, retention_days: u32, max_total_bytes: u64) -> usize {
        let Some(dir) = self.dir() else {
            return 0;
        };
        let today = day_index(now);
        let first_kept = today - i64::from(retention_days.max(1));
        let mut removed = 0;
        let mut kept_days = Vec::new();
        for (day, day_dir) in day_dirs(dir) {
            if day < first_kept {
                let files = body_files(&day_dir).len();
                if fs::remove_dir_all(&day_dir).is_ok() {
                    removed += files;
                }
            } else {
                kept_days.push((day, day_dir));
            }
        }
        if max_total_bytes == 0 {
            return removed;
        }
        // Oldest first: by day, then by modification time, then by name.
        let mut files: Vec<BodyFile> = kept_days
            .iter()
            .flat_map(|(_, day_dir)| body_files(day_dir))
            .collect();
        let mut total: u64 = files.iter().map(|f| f.size).sum();
        files.sort_by(|a, b| {
            (a.path.parent(), a.modified, &a.path).cmp(&(b.path.parent(), b.modified, &b.path))
        });
        for file in &files {
            if total <= max_total_bytes {
                break;
            }
            if fs::remove_file(&file.path).is_ok() {
                total = total.saturating_sub(file.size);
                removed += 1;
            }
        }
        for (day, day_dir) in &kept_days {
            // A request is filed under the day it started on, so a capture
            // in progress can only be creating the directory of today or
            // (for a request that began before midnight) yesterday.
            if *day < today - 1 {
                // Only succeeds when the directory is empty.
                let _ = fs::remove_dir(day_dir);
            }
        }
        removed
    }
}

struct BodyFile {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

/// Day directories (`YYYY-MM-DD`) under `dir` as `(day index, path)`,
/// oldest first. Anything else in the directory is ignored.
fn day_dirs(dir: &Path) -> Vec<(i64, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut days: Vec<(i64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let day = parse_day(path.file_name()?.to_str()?)?;
            path.is_dir().then_some((day, path))
        })
        .collect();
    days.sort();
    days
}

/// The `*.json` files of one day directory.
fn body_files(day_dir: &Path) -> Vec<BodyFile> {
    let Ok(entries) = fs::read_dir(day_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| BodyFile {
                path,
                size: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{RecordBuilder, RecordError, RequestStart, new_request_id};
    use crate::redact::redact_headers;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use switchyard_core::protocol::Protocol;

    /// 2026-10-02T00:00:00Z.
    const T0: i64 = 1_790_899_200_000;
    const KEY: &str = "sk-proj-abcdefghijklmnopqrstuvwxyz";

    fn bodies() -> CapturedBodies {
        CapturedBodies {
            client_request: Some(r#"{"model":"sonnet","max_tokens":64,"messages":[]}"#.to_string()),
            upstream_request: Some(r#"{"model":"claude-sonnet-4-5","max_tokens":64}"#.to_string()),
            upstream_response: Some("event: message_start\ndata: {}\n\n".to_string()),
            client_response: Some(r#"{"id":"msg_1"}"#.to_string()),
            client_headers: redact_headers([("Authorization", format!("Bearer {KEY}"))]),
            upstream_headers: redact_headers([
                ("x-api-key", KEY),
                ("anthropic-version", "2023-06-01"),
            ]),
        }
    }

    fn record(id: &str, started_at: i64, ok: bool) -> RequestRecord {
        let start = RequestStart::new(
            Protocol::Anthropic,
            "POST /v1/messages",
            "sonnet",
            started_at,
        )
        .with_id(id);
        let mut b = RecordBuilder::new(start);
        if !ok {
            b.set_error(RecordError::new("upstream", "boom"));
        }
        b.finish(if ok { 200 } else { 502 }, started_at + 10)
    }

    fn store(dir: &Path, mode: RequestLogMode) -> BodyStore {
        BodyStore::new(Some(dir), mode, DEFAULT_MAX_BODY_BYTES)
    }

    fn all_files(dir: &Path) -> Vec<String> {
        let mut names = Vec::new();
        for (_, day_dir) in day_dirs(&dir.join(REQUESTS_DIR)) {
            for file in body_files(&day_dir) {
                let day = day_dir.file_name().unwrap().to_string_lossy().into_owned();
                let name = file
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                names.push(format!("{day}/{name}"));
            }
        }
        names.sort();
        names
    }

    #[test]
    fn mode_all_captures_every_request() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), RequestLogMode::All);
        assert!(store.wants(false) && store.wants(true));
        assert!(store.capture(&record("ok-1", T0, true), bodies()).unwrap());
        assert!(
            store
                .capture(&record("bad-1", T0 + DAY_MS, false), bodies())
                .unwrap()
        );
        assert_eq!(
            all_files(tmp.path()),
            ["2026-10-02/ok-1.json", "2026-10-03/bad-1.json"]
        );
        assert_eq!(store.read("ok-1").unwrap(), bodies());
        assert_eq!(store.read("bad-1").unwrap(), bodies());
        assert_eq!(store.read("missing"), None);
    }

    #[test]
    fn mode_errors_captures_failures_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), RequestLogMode::Errors);
        assert!(!store.wants(false) && store.wants(true));
        assert!(!store.capture(&record("ok-1", T0, true), bodies()).unwrap());
        assert!(
            store
                .capture(&record("bad-1", T0, false), bodies())
                .unwrap()
        );
        assert_eq!(all_files(tmp.path()), ["2026-10-02/bad-1.json"]);
        assert_eq!(store.read("ok-1"), None);
    }

    #[test]
    fn mode_off_and_missing_data_dir_capture_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let off = store(tmp.path(), RequestLogMode::Off);
        assert!(!off.wants(true));
        assert!(!off.capture(&record("bad-1", T0, false), bodies()).unwrap());
        assert!(!tmp.path().join(REQUESTS_DIR).exists());

        let homeless = BodyStore::new(None, RequestLogMode::All, DEFAULT_MAX_BODY_BYTES);
        assert_eq!(homeless.mode(), RequestLogMode::Off);
        assert!(!homeless.wants(true));
        assert!(
            !homeless
                .capture(&record("bad-1", T0, false), bodies())
                .unwrap()
        );
        assert_eq!(homeless.read("bad-1"), None);
        assert_eq!(homeless.prune(T0, 1, 1), 0);
        assert_eq!(BodyStore::disabled().dir(), None);
    }

    #[test]
    fn settings_can_change_at_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), RequestLogMode::Off);
        let shared = store.clone();
        shared.configure(RequestLogMode::All, 4096);
        assert_eq!(store.mode(), RequestLogMode::All);
        assert_eq!(store.max_body_bytes(), 4096);
        assert!(store.capture(&record("a", T0, true), bodies()).unwrap());
        store.configure(RequestLogMode::Off, 10);
        // The limit has a floor of 1 KiB.
        assert_eq!(store.max_body_bytes(), 1024);
        assert!(!store.capture(&record("b", T0, true), bodies()).unwrap());
        // What was captured stays readable.
        assert!(store.read("a").is_some());
    }

    #[test]
    fn stored_file_is_plain_json() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), RequestLogMode::All);
        store.capture(&record("r1", T0, true), bodies()).unwrap();
        let text = fs::read_to_string(tmp.path().join("requests/2026-10-02/r1.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value,
            json!({
                "client_request": r#"{"model":"sonnet","max_tokens":64,"messages":[]}"#,
                "upstream_request": r#"{"model":"claude-sonnet-4-5","max_tokens":64}"#,
                "upstream_response": "event: message_start\ndata: {}\n\n",
                "client_response": r#"{"id":"msg_1"}"#,
                "client_headers": {"authorization": "Bearer sk-pro…wxyz"},
                "upstream_headers": {"anthropic-version": "2023-06-01", "x-api-key": "sk-pro…wxyz"}
            })
        );
        assert!(!text.contains(KEY));
        // No temporary file is left behind.
        assert_eq!(all_files(tmp.path()), ["2026-10-02/r1.json"]);
        assert_eq!(
            fs::read_dir(tmp.path().join("requests/2026-10-02"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn bodies_are_redacted_before_they_are_stored() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), RequestLogMode::All);
        let captured = CapturedBodies {
            client_request: Some(format!(
                r#"{{"model":"gpt-5","max_tokens":100,"tools":[{{"type":"mcp","authorization":"{KEY}"}}]}}"#
            )),
            upstream_request: Some(format!("POST /v1beta/models/x:generateContent?key={KEY}")),
            upstream_response: Some(format!(
                "data: {{\"error\":{{\"message\":\"Incorrect API key provided: {KEY}\"}}}}\n\n"
            )),
            client_response: None,
            // Raw header maps are caught too.
            client_headers: BTreeMap::from([
                ("x-goog-api-key".to_string(), KEY.to_string()),
                ("cookie".to_string(), "session=abc".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ]),
            upstream_headers: BTreeMap::from([(
                "proxy-authorization".to_string(),
                format!("Basic {KEY}"),
            )]),
        };
        store.capture(&record("r1", T0, true), captured).unwrap();
        let text = fs::read_to_string(tmp.path().join("requests/2026-10-02/r1.json")).unwrap();
        assert!(!text.contains(KEY), "{text}");
        let back = store.read("r1").unwrap();
        assert_eq!(
            back.client_request.as_deref(),
            Some(
                r#"{"model":"gpt-5","max_tokens":100,"tools":[{"type":"mcp","authorization":"sk-pro…wxyz"}]}"#
            )
        );
        assert_eq!(
            back.upstream_request.as_deref(),
            Some("POST /v1beta/models/x:generateContent?key=sk-pro…wxyz")
        );
        assert!(
            back.upstream_response
                .unwrap()
                .contains("Incorrect API key provided: sk-pro…wxyz")
        );
        assert_eq!(back.client_response, None);
        assert_eq!(back.client_headers["x-goog-api-key"], "sk-pro…wxyz");
        assert_eq!(back.client_headers["cookie"], "[redacted]");
        assert_eq!(back.client_headers["content-type"], "application/json");
        assert_eq!(
            back.upstream_headers["proxy-authorization"],
            "Basic [redacted]"
        );
    }

    #[test]
    fn json_bodies_and_plain_headers_are_scanned_for_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), RequestLogMode::All);
        let captured = CapturedBodies {
            // What an OpenAI-compatible upstream answers on a bad key: one
            // JSON document, the key inside a string value.
            upstream_response: Some(format!(
                r#"{{"error":{{"message":"Incorrect API key provided: {KEY}","type":"invalid_request_error"}}}}"#
            )),
            // Same-protocol errors are forwarded verbatim.
            client_response: Some(format!(
                r#"{{"error":{{"message":"Incorrect API key provided: {KEY}","type":"invalid_request_error"}}}}"#
            )),
            // A raw header map: the key travels in an innocently named
            // header.
            client_headers: BTreeMap::from([
                (
                    "x-forwarded-uri".to_string(),
                    format!("/v1beta/models/g:streamGenerateContent?alt=sse&key={KEY}"),
                ),
                (
                    "sec-websocket-protocol".to_string(),
                    format!("realtime, openai-insecure-api-key.{KEY}"),
                ),
            ]),
            ..CapturedBodies::default()
        };
        store.capture(&record("r1", T0, false), captured).unwrap();
        let text = fs::read_to_string(tmp.path().join("requests/2026-10-02/r1.json")).unwrap();
        assert!(!text.contains(KEY), "{text}");
        let back = store.read("r1").unwrap();
        assert_eq!(
            back.upstream_response.as_deref(),
            Some(
                r#"{"error":{"message":"Incorrect API key provided: sk-pro…wxyz","type":"invalid_request_error"}}"#
            )
        );
        assert_eq!(back.client_response, back.upstream_response);
        assert_eq!(
            back.client_headers["x-forwarded-uri"],
            "/v1beta/models/g:streamGenerateContent?alt=sse&key=sk-pro…wxyz"
        );
        assert_eq!(
            back.client_headers["sec-websocket-protocol"],
            "realtime, openai-insecure-api-key.sk-pro…wxyz"
        );
        // Preparing what was already prepared changes nothing.
        assert_eq!(store.prepare(back.clone()), back);
    }

    #[test]
    fn truncation_marker_counts_removed_bytes() {
        assert_eq!(truncate_body("hello", 10), "hello");
        assert_eq!(truncate_body("hello", 5), "hello");
        assert_eq!(truncate_body("hello world", 5), "hello…[truncated 6 bytes]");
        assert_eq!(truncate_body("hello", 0), "…[truncated 5 bytes]");
        // Never cuts inside a character: "é" is two bytes.
        assert_eq!(truncate_body("ééé", 3), "é…[truncated 4 bytes]");
        assert_eq!(truncate_body("ééé", 4), "éé…[truncated 2 bytes]");
    }

    #[test]
    fn long_bodies_are_truncated_to_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = BodyStore::new(Some(tmp.path()), RequestLogMode::All, 2048);
        let long = "x".repeat(10_000);
        let captured = CapturedBodies {
            client_request: Some(long.clone()),
            client_response: Some("short".to_string()),
            ..CapturedBodies::default()
        };
        store.capture(&record("r1", T0, true), captured).unwrap();
        let back = store.read("r1").unwrap();
        assert_eq!(
            back.client_request.unwrap(),
            format!("{}…[truncated 7952 bytes]", &long[..2048])
        );
        assert_eq!(back.client_response.as_deref(), Some("short"));
    }

    #[test]
    fn a_secret_at_the_cut_is_redacted_not_split() {
        // The key starts just before the limit; cutting first would keep
        // its first characters in the clear.
        let filler = "a".repeat(2040);
        let body = format!(r#"{{"note":"{filler}","api_key":"{KEY}"}}"#);
        let out = prepare_body(&body, 2070);
        assert!(!out.contains("sk-proj-abc"), "{out}");
        assert!(out.contains("truncated"));
        // A JSON body that fits is redacted and stays whole.
        let small = format!(r#"{{"api_key":"{KEY}"}}"#);
        assert_eq!(prepare_body(&small, 2070), r#"{"api_key":"sk-pro…wxyz"}"#);
    }

    #[test]
    fn huge_bodies_are_cut_before_scanning() {
        let body = "y".repeat(REDACTION_SCAN_LIMIT + 5_000);
        let out = prepare_body(&body, 1024);
        assert_eq!(
            out,
            format!(
                "{}…[truncated {} bytes]",
                "y".repeat(1024),
                REDACTION_SCAN_LIMIT + 5_000 - 1024
            )
        );
    }

    #[test]
    fn ids_cannot_escape_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), RequestLogMode::All);
        for id in [
            "../evil",
            "a/b",
            "a\\b",
            "",
            "..",
            "x.json",
            &"z".repeat(200),
        ] {
            assert!(!store.capture_at(id, T0, true, bodies()).unwrap(), "{id:?}");
            assert_eq!(store.read(id), None, "{id:?}");
        }
        assert!(all_files(tmp.path()).is_empty());
        assert!(!tmp.path().join("evil.json").exists());
    }

    #[test]
    fn read_finds_uuid_ids_by_their_timestamp_and_others_by_scanning() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), RequestLogMode::All);
        let id = new_request_id();
        let now = request_id_time_ms(&id).unwrap();
        store.capture_at(&id, now, false, bodies()).unwrap();
        assert_eq!(store.read(&id).unwrap(), bodies());
        // Stored under a day far from the id's own timestamp: still found.
        let other = new_request_id();
        store.capture_at(&other, T0, false, bodies()).unwrap();
        assert_eq!(store.read(&other).unwrap(), bodies());
        // A corrupt file reads as absent.
        let path = tmp
            .path()
            .join("requests/2026-10-02")
            .join(format!("{other}.json"));
        fs::write(&path, "{ not json").unwrap();
        assert_eq!(store.read(&other), None);
    }

    #[test]
    fn prune_by_age() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), RequestLogMode::All);
        for day in 0..6 {
            store
                .capture_at(&format!("d{day}-a"), T0 + day * DAY_MS, true, bodies())
                .unwrap();
            store
                .capture_at(&format!("d{day}-b"), T0 + day * DAY_MS, true, bodies())
                .unwrap();
        }
        fs::create_dir_all(tmp.path().join("requests/not-a-day")).unwrap();
        let now = T0 + 5 * DAY_MS + 1_000;
        // Today and the two days before it stay.
        assert_eq!(store.prune(now, 2, 0), 6);
        assert_eq!(
            all_files(tmp.path()),
            [
                "2026-10-05/d3-a.json",
                "2026-10-05/d3-b.json",
                "2026-10-06/d4-a.json",
                "2026-10-06/d4-b.json",
                "2026-10-07/d5-a.json",
                "2026-10-07/d5-b.json"
            ]
        );
        assert!(!tmp.path().join("requests/2026-10-02").exists());
        assert!(tmp.path().join("requests/not-a-day").exists());
        assert_eq!(store.prune(now, 2, 0), 0);
    }

    #[test]
    fn prune_by_total_size_removes_the_oldest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), RequestLogMode::All);
        for day in 0..4 {
            store
                .capture_at(&format!("d{day}"), T0 + day * DAY_MS, true, bodies())
                .unwrap();
        }
        let size = fs::metadata(tmp.path().join("requests/2026-10-02/d0.json"))
            .unwrap()
            .len();
        let now = T0 + 3 * DAY_MS;
        // Room for two and a half files: the two oldest go.
        assert_eq!(store.prune(now, 30, size * 5 / 2), 2);
        assert_eq!(
            all_files(tmp.path()),
            ["2026-10-04/d2.json", "2026-10-05/d3.json"]
        );
        // Emptied day directories are removed.
        assert!(!tmp.path().join("requests/2026-10-02").exists());
        assert!(!tmp.path().join("requests/2026-10-03").exists());
        // Already within the cap.
        assert_eq!(store.prune(now, 30, size * 5 / 2), 0);
        assert_eq!(store.prune(now, 30, 1), 2);
        assert!(all_files(tmp.path()).is_empty());
    }

    #[test]
    fn prune_leaves_the_empty_directories_a_capture_may_be_creating() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), RequestLogMode::All);
        let now = T0 + 3 * DAY_MS + 1_000;
        // What `capture_at` leaves behind between creating the day directory
        // and writing the file: an empty directory.
        for day in ["2026-10-02", "2026-10-03", "2026-10-04", "2026-10-05"] {
            fs::create_dir_all(tmp.path().join("requests").join(day)).unwrap();
        }
        assert_eq!(store.prune(now, 30, 1), 0);
        // Today and yesterday may have a capture in progress.
        assert!(tmp.path().join("requests/2026-10-05").exists());
        assert!(tmp.path().join("requests/2026-10-04").exists());
        // Older empty directories are tidied up.
        assert!(!tmp.path().join("requests/2026-10-03").exists());
        assert!(!tmp.path().join("requests/2026-10-02").exists());

        // The capture that was in progress completes.
        store.capture_at("late", now, true, bodies()).unwrap();
        assert!(store.read("late").is_some());
    }

    /// Bodies handed to a background write are readable — redacted — from
    /// the moment they are held until the file takes over.
    #[test]
    fn held_bodies_are_readable_until_the_file_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), RequestLogMode::All);
        let raw = CapturedBodies {
            client_request: Some(format!(r#"{{"api_key":"{KEY}","model":"sonnet"}}"#)),
            upstream_headers: BTreeMap::from([("x-api-key".to_string(), KEY.to_string())]),
            ..bodies()
        };
        let expected = store.prepare(raw.clone());
        assert!(!serde_json::to_string(&expected).unwrap().contains(KEY));

        // Held, no file yet: served from memory, in the form of the file.
        let held = store.hold("r1", raw.clone());
        assert!(all_files(tmp.path()).is_empty());
        assert_eq!(store.read("r1"), Some(expected.clone()));
        assert_eq!(store.read("other"), None);
        // The raw copy is never printed.
        let printed = format!("{store:?}");
        assert!(
            printed.contains("held: 1") && !printed.contains(KEY),
            "{printed}"
        );

        // Written: the file takes over and the copy in memory is let go.
        assert!(held.write(T0, false).unwrap());
        assert_eq!(all_files(tmp.path()), ["2026-10-02/r1.json"]);
        assert!(store.inner.held.lock().is_empty());
        assert_eq!(store.read("r1"), Some(expected.clone()));

        // A write that never runs (the runtime discarded the task) holds
        // nothing back.
        drop(store.hold("r2", raw.clone()));
        assert!(store.inner.held.lock().is_empty());
        assert_eq!(store.read("r2"), None);

        // Two captures under one id: the later one is served, and the
        // earlier guard going away does not take it along.
        let first = store.hold("r3", bodies());
        let second = store.hold("r3", raw.clone());
        drop(first);
        assert_eq!(store.read("r3"), Some(expected));
        drop(second);
        assert_eq!(store.read("r3"), None);

        // A write the mode refuses stores nothing and leaves nothing behind.
        store.configure(RequestLogMode::Errors, DEFAULT_MAX_BODY_BYTES);
        assert!(!store.hold("r4", raw).write(T0, false).unwrap());
        assert_eq!(store.read("r4"), None);
        assert!(store.inner.held.lock().is_empty());
    }

    #[test]
    fn empty_capture_is_reported() {
        assert!(CapturedBodies::default().is_empty());
        assert!(!bodies().is_empty());
        let only_headers = CapturedBodies {
            client_headers: BTreeMap::from([("accept".to_string(), "*/*".to_string())]),
            ..CapturedBodies::default()
        };
        assert!(!only_headers.is_empty());
    }

    #[test]
    fn missing_fields_default_when_reading() {
        let parsed: CapturedBodies = serde_json::from_str(r#"{"client_request":"x"}"#).unwrap();
        assert_eq!(parsed.client_request.as_deref(), Some("x"));
        assert!(parsed.upstream_headers.is_empty());
    }
}
