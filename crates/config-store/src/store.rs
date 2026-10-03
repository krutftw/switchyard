//! The live configuration: loading, watching, editing.

use crate::error::ConfigStoreError;
use crate::merge::{self, Strategy};
use crate::persist::{self, ContentHash, content_hash};
use crate::validate::{FILE_PATH, strip_bom, validate_text};
use arc_swap::ArcSwap;
use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};
use switchyard_core::Config;
use switchyard_core::config::ConfigIssue;
use tokio::sync::{OwnedMutexGuard, broadcast, mpsc, watch};

/// Where a configuration change came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The file changed on disk (an editor, config management, a manual
    /// reload request).
    File,
    /// An edit made through the store ([`ConfigStore::update`],
    /// [`ConfigStore::replace_text`]), i.e. the admin API.
    Admin,
}

/// What happened to a configuration the store was asked to take.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigEvent {
    /// A configuration became the live one.
    ///
    /// Also sent, with [`Source::File`], when a file that was
    /// [`Rejected`](ConfigEvent::Rejected) returns to the content of the
    /// configuration in effect: nothing changes then, but the rejection is
    /// over, and this is the only event that says so. Exactly one is sent
    /// per recovery.
    Applied { source: Source, at: SystemTime },
    /// A configuration was refused; the previous one stays live.
    Rejected {
        source: Source,
        issues: Vec<ConfigIssue>,
        at: SystemTime,
    },
}

/// Timing of the file watcher. The defaults are right for production; tests
/// shorten them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchOptions {
    /// Quiet period after the last file-system event before the file is
    /// read. Editors and deployment tools touch a file several times per
    /// save.
    pub debounce: Duration,
    /// How often the file is polled. Polling backs up the native watcher
    /// (some bind mounts and network file systems deliver no events) and
    /// replaces it when it cannot be created.
    pub poll_interval: Duration,
    /// Use the operating system's change notifications. When false, or when
    /// they are unavailable, the store polls only.
    pub native: bool,
}

impl Default for WatchOptions {
    fn default() -> Self {
        WatchOptions {
            debounce: Duration::from_millis(300),
            poll_interval: Duration::from_secs(2),
            native: true,
        }
    }
}

/// The configuration file and the live configuration parsed from it.
///
/// Cheap to clone; all clones share one state.
///
/// * [`current`](Self::current) / [`subscribe`](Self::subscribe) give the
///   live [`Config`]; a new value is published every time a configuration is
///   applied, whether it came from the file or from an edit.
/// * [`spawn_watcher`](Self::spawn_watcher) makes the store follow the file:
///   a valid change is applied, an invalid one is reported
///   ([`ConfigEvent::Rejected`]) and ignored.
/// * [`update`](Self::update) and [`replace_text`](Self::replace_text) change
///   the file and the live configuration together. Writers are serialised;
///   the live configuration is swapped only after the file was written.
///
/// # How the file is written
///
/// The file keeps its permissions across edits; it usually holds API keys.
///
/// * On Unix the new text goes to a temporary file beside the original
///   (readable by its owner only), is flushed to disk and renamed over the
///   original, taking over its mode, owner and group: the file is at all
///   times the old or the new version. When the file cannot be replaced that
///   way — a single-file bind mount, a directory that is not writable, an
///   owner the process may not assign — it is rewritten in place. Extended
///   attributes set on the file itself (POSIX ACLs, SELinux labels) are not
///   carried over to a replacement.
/// * On Windows an existing file is always rewritten in place, because the
///   access-control list belongs to the file and a replacement would get the
///   directory's instead. The write is therefore not atomic there.
///
/// A write that fails half-way (a full disk) puts the previous content back.
#[derive(Clone)]
pub struct ConfigStore {
    inner: Arc<Inner>,
}

struct Inner {
    /// `dir` joined with the file name.
    path: PathBuf,
    /// Canonical directory containing the file.
    dir: PathBuf,
    file_name: OsString,
    current: ArcSwap<Config>,
    /// The configuration the process started with, for
    /// [`ConfigStore::restart_required`].
    started_with: Arc<Config>,
    updates: watch::Sender<Arc<Config>>,
    events: broadcast::Sender<ConfigEvent>,
    /// Serialises everything that reads-then-applies or writes the file.
    /// Shared, so that a write in progress can keep the lock by itself when
    /// the caller that started it goes away (see [`ConfigStore::commit`]).
    write_lock: Arc<tokio::sync::Mutex<()>>,
    hashes: parking_lot::Mutex<Hashes>,
    watching: AtomicBool,
    watch_guard: parking_lot::Mutex<Option<WatchGuard>>,
    /// Test hook: where the temporary file of the next writes goes.
    #[cfg(test)]
    temp_override: parking_lot::Mutex<Option<PathBuf>>,
}

/// Content the store has already dealt with, so that seeing it again on disk
/// (its own write coming back as a file event, an editor saving without
/// changes) causes no second event.
#[derive(Default)]
struct Hashes {
    /// Content of the file the live configuration came from. A write by the
    /// store is always followed by applying what was written, under the same
    /// lock, so this is also the last content the store wrote.
    applied: Option<ContentHash>,
    /// The text of that file. When the file on disk is gone or empty, an
    /// edit made through the store is merged into this text instead, so the
    /// comments and layout of the last good version are not lost.
    applied_text: Option<Arc<str>>,
    /// Content that was last refused, remembered so that the same broken
    /// save is reported once and not at every look. Forgotten as soon as the
    /// file holds anything else — applied, or already live — so that the same
    /// mistake made again later is reported again. While it is set, the file
    /// going back to the live content is announced (see
    /// [`ConfigStore::is_live`]).
    rejected: Option<ContentHash>,
    /// What was wrong with that content and when it was refused; set and
    /// cleared together with `rejected` (see [`ConfigStore::rejection`]).
    rejection: Option<Rejection>,
}

/// The file on disk, refused: what is wrong with it and since when.
///
/// The store keeps this for as long as the file holds the refused content
/// (see [`ConfigStore::rejection`]), so that anyone who looks later — a
/// dashboard page loaded after the [`ConfigEvent::Rejected`] went out — can
/// still tell that the file is not in effect.
#[derive(Clone, Debug, PartialEq)]
pub struct Rejection {
    /// Every issue of the file. Issue texts name fields and rules, never
    /// values.
    pub issues: Vec<ConfigIssue>,
    /// When the content was refused.
    pub at: SystemTime,
}

/// Keeps the watcher alive; dropping it (with the store) ends the watch task.
struct WatchGuard {
    _watcher: Option<RecommendedWatcher>,
    _signal: mpsc::UnboundedSender<()>,
}

/// How the content of the file relates to the live configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Live {
    /// The file holds something else.
    No,
    /// The file holds the live configuration.
    Yes,
    /// The file holds the live configuration again after having been
    /// rejected; this was announced.
    Recovered,
}

/// Outcome of looking at the file on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    /// The file is gone or unreadable; nothing changes.
    Missing,
    /// Content the store already knows.
    Unchanged,
    /// A configuration was applied, or the file went back to the live one
    /// after a rejection; announced either way.
    Applied,
    Rejected,
    /// The content is unusable right now; it may be half-written.
    Retry,
}

impl std::fmt::Debug for ConfigStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigStore")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

impl ConfigStore {
    /// Reads, parses and validates the configuration file.
    ///
    /// Fails with [`ConfigStoreError::Io`] when the file cannot be read and
    /// with [`ConfigStoreError::Invalid`] — listing every issue — when its
    /// content is not a valid configuration.
    pub fn load(path: &Path) -> Result<ConfigStore, ConfigStoreError> {
        let absolute = std::path::absolute(path).map_err(|e| ConfigStoreError::io(path, e))?;
        let bytes = std::fs::read(&absolute).map_err(|e| ConfigStoreError::io(&absolute, e))?;
        let hash = content_hash(&bytes);
        let text = decode(bytes).map_err(ConfigStoreError::Invalid)?;
        let config = validate_text(&text).map_err(ConfigStoreError::Invalid)?;

        let file_name = absolute
            .file_name()
            .map(OsStr::to_os_string)
            .ok_or_else(|| {
                ConfigStoreError::io(
                    &absolute,
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a file path"),
                )
            })?;
        let parent = absolute.parent().unwrap_or_else(|| Path::new("."));
        let dir = simplify(std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf()));
        let path = dir.join(&file_name);

        let config = Arc::new(config);
        let (updates, _) = watch::channel(config.clone());
        let (events, _) = broadcast::channel(64);
        Ok(ConfigStore {
            inner: Arc::new(Inner {
                path,
                dir,
                file_name,
                current: ArcSwap::new(config.clone()),
                started_with: config,
                updates,
                events,
                write_lock: Arc::new(tokio::sync::Mutex::new(())),
                hashes: parking_lot::Mutex::new(Hashes {
                    applied: Some(hash),
                    applied_text: Some(Arc::from(text)),
                    ..Hashes::default()
                }),
                watching: AtomicBool::new(false),
                watch_guard: parking_lot::Mutex::new(None),
                #[cfg(test)]
                temp_override: parking_lot::Mutex::new(None),
            }),
        })
    }

    /// The live configuration.
    pub fn current(&self) -> Arc<Config> {
        self.inner.current.load_full()
    }

    /// Absolute path of the configuration file.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Canonical directory containing the configuration file. Relative paths
    /// in the configuration (`server.data_dir`, service-account files, TLS
    /// certificates) are relative to it.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Resolves a path from the configuration against [`dir`](Self::dir).
    /// Absolute paths are returned unchanged.
    pub fn resolve_path(&self, relative: &str) -> PathBuf {
        let relative = Path::new(relative.trim());
        if relative.is_absolute() {
            relative.to_path_buf()
        } else {
            self.inner.dir.join(relative)
        }
    }

    /// The file's current text, exactly as it is on disk.
    pub fn raw_text(&self) -> std::io::Result<String> {
        std::fs::read_to_string(&self.inner.path)
    }

    /// When the file was last modified, if the file system can tell.
    pub fn modified_at(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.inner.path).ok()?.modified().ok()
    }

    /// A receiver that sees every applied configuration. Its initial value is
    /// the configuration live at the time of the call.
    pub fn subscribe(&self) -> watch::Receiver<Arc<Config>> {
        self.inner.updates.subscribe()
    }

    /// A receiver of [`ConfigEvent`]s: one per applied configuration and one
    /// per rejected file content. Slow receivers lose events; they never
    /// block the store.
    pub fn events(&self) -> broadcast::Receiver<ConfigEvent> {
        self.inner.events.subscribe()
    }

    /// The refusal of the file on disk, while it lasts: `Some` from the
    /// moment content of the file was refused ([`ConfigEvent::Rejected`],
    /// by the watcher or [`reload_from_disk`](Self::reload_from_disk)) until
    /// the store next sees the file hold anything else: a configuration it
    /// applies, the configuration in effect again, or a write of its own.
    /// The previous configuration stays in effect all that time. (An edit
    /// refused with [`ConfigStoreError::DiskInvalid`] is the caller's answer,
    /// not a verdict on the file, and does not set it.)
    ///
    /// It is the state as of the store's last look at the file; with the
    /// watcher running that look is at most a poll interval old.
    pub fn rejection(&self) -> Option<Rejection> {
        self.inner.hashes.lock().rejection.clone()
    }

    /// Settings whose live value differs from the value the process started
    /// with and that only take effect after a restart, as dotted paths:
    /// `server.host`, `server.port`, `server.tls`, `server.data_dir`.
    pub fn restart_required(&self) -> Vec<String> {
        let started = &self.inner.started_with.server;
        let current = self.current();
        let now = &current.server;
        let mut changed = Vec::new();
        if started.host != now.host {
            changed.push("server.host".to_string());
        }
        if started.port != now.port {
            changed.push("server.port".to_string());
        }
        if started.tls != now.tls {
            changed.push("server.tls".to_string());
        }
        if started.data_dir != now.data_dir {
            changed.push("server.data_dir".to_string());
        }
        changed
    }

    /// Starts following the file on disk with the default timing. Calling it
    /// again does nothing.
    ///
    /// The file's *directory* is watched (not recursively) and events are
    /// filtered by file name, because editors and configuration management
    /// replace files by rename and container runtimes bind-mount single
    /// files. After a quiet period the file is read:
    ///
    /// * content the store applied or wrote itself is ignored — unless the
    ///   file was rejected in between: its return to the configuration in
    ///   effect is announced once ([`ConfigEvent::Applied`] with
    ///   [`Source::File`], and subscribers are notified), so that whoever
    ///   showed "file refused" can stop;
    /// * a valid configuration becomes the live one
    ///   ([`ConfigEvent::Applied`] with [`Source::File`]);
    /// * an invalid one — or an empty file, which is what a save in progress
    ///   looks like — is read once more after another quiet period, then
    ///   reported ([`ConfigEvent::Rejected`]) while the previous configuration
    ///   stays live. One broken save is reported once, however often the file
    ///   is looked at; the same mistake saved again after the file was good in
    ///   between is reported again;
    /// * a missing file changes nothing.
    ///
    /// The file is also polled every two seconds, which catches changes the
    /// operating system does not report and is the only mechanism when
    /// change notifications are unavailable.
    pub fn spawn_watcher(&self, handle: &tokio::runtime::Handle) {
        self.spawn_watcher_with(handle, WatchOptions::default());
    }

    /// [`spawn_watcher`](Self::spawn_watcher) with explicit timing.
    pub fn spawn_watcher_with(&self, handle: &tokio::runtime::Handle, options: WatchOptions) {
        if self.inner.watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let (signal, signals) = mpsc::unbounded_channel();
        let watcher = if options.native {
            self.native_watcher(signal.clone())
        } else {
            None
        };
        let native = watcher.is_some();
        *self.inner.watch_guard.lock() = Some(WatchGuard {
            _watcher: watcher,
            _signal: signal,
        });
        handle.spawn(watch_loop(
            Arc::downgrade(&self.inner),
            signals,
            options,
            native,
        ));
    }

    fn native_watcher(&self, signal: mpsc::UnboundedSender<()>) -> Option<RecommendedWatcher> {
        let file_name = self.inner.file_name.clone();
        let handler = move |event: notify::Result<notify::Event>| {
            let relevant = match &event {
                Ok(event) => event_concerns(event, &file_name),
                // An error (a queue overflow, say) may have swallowed a
                // change, so look at the file.
                Err(_) => true,
            };
            if relevant {
                // The receiver is gone only while shutting down.
                let _ = signal.send(());
            }
        };
        let result = notify::recommended_watcher(handler).and_then(|mut watcher| {
            watcher.watch(&self.inner.dir, RecursiveMode::NonRecursive)?;
            Ok(watcher)
        });
        match result {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                tracing::warn!(
                    dir = %self.inner.dir.display(),
                    %error,
                    "cannot watch the configuration directory; polling the file instead"
                );
                None
            }
        }
    }

    /// Re-reads the file now and applies it, whether or not its content
    /// changed (subscribers are notified either way, so a manual reload can
    /// be used to re-run everything that depends on the configuration).
    /// Unlike the watcher, an explicit reload takes an empty file for what it
    /// formally is: a configuration with every setting at its default.
    ///
    /// An unreadable file is an [`ConfigStoreError::Io`] error; an invalid
    /// one is [`ConfigStoreError::Invalid`], also reported as
    /// [`ConfigEvent::Rejected`], and the previous configuration stays live.
    pub async fn reload_from_disk(&self) -> Result<Arc<Config>, ConfigStoreError> {
        let _guard = self.inner.write_lock.lock().await;
        let bytes = self
            .read_file()
            .await
            .map_err(|e| ConfigStoreError::io(&self.inner.path, e))?;
        let hash = content_hash(&bytes);
        match parse_bytes(bytes) {
            Ok((config, text)) => Ok(self.apply(Arc::new(config), Source::File, hash, &text)),
            Err(issues) => {
                self.reject(issues.clone(), Source::File, hash);
                Err(ConfigStoreError::Invalid(issues))
            }
        }
    }

    /// Edits the configuration: `edit` receives a copy of the live
    /// configuration; the result is validated, written to the file and then
    /// applied.
    ///
    /// The file is rewritten **preserving its formatting** — only the values
    /// the edit changed are touched (see [`crate::merge`]). An edit that
    /// changes nothing leaves the file untouched and publishes nothing.
    ///
    /// Errors: [`ConfigStoreError::Edit`] when `edit` itself fails (its
    /// message is passed through) or the result cannot be written as TOML,
    /// [`ConfigStoreError::Invalid`] when the result fails validation,
    /// [`ConfigStoreError::DiskInvalid`] when the file on disk holds an
    /// invalid manual edit (see below), [`ConfigStoreError::Io`] when the
    /// file cannot be written. In every error case neither the file nor the
    /// live configuration changes.
    ///
    /// Concurrent calls run one after the other, each on the result of the
    /// previous one. A valid change made to the file on disk that the watcher
    /// has not picked up yet is adopted first, so it is not overwritten.
    ///
    /// A file on disk that holds something other than the live configuration
    /// and is **not valid** — unparsable, not UTF-8, or breaking a rule: a
    /// manual edit in progress, or one the store rejected — is never
    /// overwritten by an edit: the call fails with
    /// [`ConfigStoreError::DiskInvalid`], listing what is wrong with the
    /// file, and neither the file nor the live configuration changes.
    /// Writing the edit would mean rewriting the file from the last valid
    /// configuration and silently discarding what the operator typed. The
    /// file has to be fixed or restored by hand, or replaced as a whole with
    /// [`replace_text`](Self::replace_text). (An edit that changes nothing
    /// writes nothing and is therefore not refused.)
    ///
    /// A file that is gone or empty holds nothing to lose: the edit is
    /// merged into the text of the last applied configuration and the file
    /// is recreated from the result, so the comments and layout of the last
    /// good version survive. This is logged as a warning.
    ///
    /// Dropping the returned future is safe at any point. Before the write
    /// starts nothing has changed. Once it has started, the write and the
    /// swap of the live configuration finish together in the background, with
    /// the store still locked against other writers: the file and the live
    /// configuration never disagree, and the change is announced like any
    /// other.
    pub async fn update<F>(&self, edit: F) -> Result<Arc<Config>, ConfigStoreError>
    where
        F: FnOnce(&mut Config) -> Result<(), String> + Send,
    {
        self.update_unsetting(|config| edit(config).map(|()| Vec::new()))
            .await
    }

    /// [`update`](Self::update) whose `edit` also names settings to take out
    /// of the file: dotted paths (`server.port`, `routing.cooldown`) of
    /// settings or tables the edit put back to their default, which should
    /// be left out of the file so the default applies — and a later change
    /// of the default too — instead of being written as explicit values
    /// (see [`merge::render_update_unsetting`]).
    ///
    /// Such a key is removed even when the configuration does not change (a
    /// file that spells out a default), which then rewrites the file and
    /// announces the configuration like any applied edit. Only when the file
    /// is broken, missing or empty does an edit that changes nothing still
    /// write nothing.
    pub async fn update_unsetting<F>(&self, edit: F) -> Result<Arc<Config>, ConfigStoreError>
    where
        F: FnOnce(&mut Config) -> Result<Vec<String>, String> + Send,
    {
        let guard = self.inner.write_lock.clone().lock_owned().await;

        let on_disk = match self.read_file().await {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(ConfigStoreError::io(&self.inner.path, e)),
        };
        // The text the edit is merged into: the file as it is, provided it
        // holds a valid configuration.
        let mut base_text: Option<Arc<str>> = None;
        // What is wrong with the file, when it holds something that is not
        // live and not valid: a hand edit in progress, which must not be
        // overwritten.
        let mut disk_issues: Option<Vec<ConfigIssue>> = None;
        // The file is gone or empty: nothing in it to lose.
        let mut disk_empty = on_disk.is_none();
        if let Some(bytes) = on_disk {
            let hash = content_hash(&bytes);
            match decode(bytes) {
                // An empty file is what a truncated save looks like.
                Ok(text) if is_blank(&text) => disk_empty = true,
                Ok(text) => {
                    if self.is_live(hash) != Live::No {
                        base_text = Some(Arc::from(text));
                    } else {
                        match validate_text(&text) {
                            Ok(config) => {
                                self.apply(Arc::new(config), Source::File, hash, &text);
                                base_text = Some(Arc::from(text));
                            }
                            Err(issues) => disk_issues = Some(issues),
                        }
                    }
                }
                Err(issues) => disk_issues = Some(issues),
            }
        }

        let current = self.current();
        let mut next = (*current).clone();
        let unset = edit(&mut next).map_err(ConfigStoreError::Edit)?;
        let issues = next.validate();
        if !issues.is_empty() {
            return Err(ConfigStoreError::Invalid(issues));
        }
        let unchanged = next == *current;
        if unchanged && (unset.is_empty() || disk_empty || disk_issues.is_some()) {
            return Ok(current);
        }
        if let Some(issues) = disk_issues {
            tracing::warn!(
                path = %self.inner.path.display(),
                "the configuration file on disk is not valid; refusing to overwrite it with \
                 an edit"
            );
            return Err(ConfigStoreError::DiskInvalid(issues));
        }

        let base_text = base_text
            .or_else(|| self.inner.hashes.lock().applied_text.clone())
            .unwrap_or_else(|| Arc::from(""));
        let rendered = merge::render_update_unsetting(&base_text, &next, &unset)
            .map_err(ConfigStoreError::Edit)?;
        if unchanged && rendered.strategy == Strategy::Unchanged {
            // The keys to unset were not in the file to begin with.
            return Ok(current);
        }
        if disk_empty {
            tracing::warn!(
                path = %self.inner.path.display(),
                "the configuration file on disk is missing or empty; recreating it from the \
                 last applied configuration and this change"
            );
        }
        if rendered.strategy == Strategy::Rewritten && !is_blank(&base_text) {
            tracing::warn!(
                path = %self.inner.path.display(),
                "the configuration file could not be edited in place and was rewritten; \
                 its comments and formatting were not preserved"
            );
        }
        let next = as_written(next, &rendered.text);
        self.commit(guard, Arc::new(next), rendered.text).await
    }

    /// Replaces the whole file with `text` (the dashboard's raw editor).
    ///
    /// The text is validated first; an invalid one is refused with
    /// [`ConfigStoreError::Invalid`] and nothing is written. A valid one is
    /// written verbatim and applied — whatever the file holds at that
    /// moment, a broken manual edit included: replacing the whole file is
    /// what the caller asked for, and it is the way out of
    /// [`ConfigStoreError::DiskInvalid`].
    ///
    /// Like [`update`](Self::update), the returned future can be dropped at
    /// any point: a write that has started is completed and applied.
    pub async fn replace_text(&self, text: &str) -> Result<Arc<Config>, ConfigStoreError> {
        let config = validate_text(text).map_err(ConfigStoreError::Invalid)?;
        let guard = self.inner.write_lock.clone().lock_owned().await;
        self.commit(guard, Arc::new(config), text.to_string()).await
    }

    // -- internals ---------------------------------------------------------

    /// Whether the file on disk, whose content has this hash, is the live
    /// configuration. Callers hold the write lock.
    ///
    /// Finding the live content on disk also ends any earlier rejection: the
    /// file is good again, so the same mistake saved once more later is a new
    /// event and must be reported again, not taken for the one already
    /// reported. And the recovery itself is news: whoever was told that the
    /// file was refused is told, once, that it is fine again
    /// ([`Live::Recovered`]).
    fn is_live(&self, hash: ContentHash) -> Live {
        let recovered = {
            let mut hashes = self.inner.hashes.lock();
            if hashes.applied != Some(hash) {
                return Live::No;
            }
            hashes.rejection = None;
            hashes.rejected.take().is_some()
        };
        if recovered {
            self.announce_recovery();
            Live::Recovered
        } else {
            Live::Yes
        }
    }

    /// Tells subscribers that the file, refused earlier, is back to the
    /// content of the configuration in effect.
    ///
    /// Nothing changes, yet it is announced like an applied file — the same
    /// way a manual reload of unchanged content is — because a
    /// [`ConfigEvent::Rejected`] went out for this file and nothing else
    /// would ever take it back.
    fn announce_recovery(&self) {
        let config = self.current();
        self.inner.updates.send_replace(config);
        // No receiver is not an error.
        let _ = self.inner.events.send(ConfigEvent::Applied {
            source: Source::File,
            at: SystemTime::now(),
        });
        tracing::info!(
            path = %self.inner.path.display(),
            source = ?Source::File,
            "configuration applied: the file is valid again and matches the configuration \
             in effect"
        );
    }

    /// Whether this content was the last one refused, with nothing else seen
    /// in the file since.
    fn is_reported(&self, hash: ContentHash) -> bool {
        self.inner.hashes.lock().rejected == Some(hash)
    }

    async fn read_file(&self) -> std::io::Result<Vec<u8>> {
        let path = self.inner.path.clone();
        blocking(move || std::fs::read(path)).await
    }

    /// Writes `text` to the file and makes `config`, which it describes, the
    /// live configuration.
    ///
    /// The two steps are one unit of work that runs off the async threads and
    /// owns the write lock. It does not depend on the caller: when the
    /// caller's future is dropped while the file is being written (an admin
    /// client that disconnects, a timeout), the write still completes, the
    /// configuration is still applied and announced, and only then is the
    /// lock released. Without this a dropped caller would leave a file that
    /// the store never applied, and would unlock the store for the next
    /// writer while the file was still being replaced.
    async fn commit(
        &self,
        guard: OwnedMutexGuard<()>,
        config: Arc<Config>,
        text: String,
    ) -> Result<Arc<Config>, ConfigStoreError> {
        let store = self.clone();
        let temp = self.temp_override();
        blocking(move || {
            // Held until the file is written and the configuration swapped.
            let _guard = guard;
            let hash = content_hash(text.as_bytes());
            persist::persist(&store.inner.path, text.as_bytes(), temp.as_deref())?;
            Ok(store.apply(config, Source::Admin, hash, &text))
        })
        .await
        .map_err(|e| ConfigStoreError::io(&self.inner.path, e))
    }

    #[cfg(test)]
    fn temp_override(&self) -> Option<PathBuf> {
        self.inner.temp_override.lock().clone()
    }

    #[cfg(not(test))]
    fn temp_override(&self) -> Option<PathBuf> {
        None
    }

    /// Makes `config` the live configuration. Callers hold the write lock.
    /// `text` is the content of the file the configuration came from or was
    /// written to, and `hash` its hash.
    fn apply(
        &self,
        config: Arc<Config>,
        source: Source,
        hash: ContentHash,
        text: &str,
    ) -> Arc<Config> {
        self.inner.current.store(config.clone());
        {
            let mut hashes = self.inner.hashes.lock();
            hashes.applied = Some(hash);
            hashes.applied_text = Some(Arc::from(text));
            hashes.rejected = None;
            hashes.rejection = None;
        }
        self.inner.updates.send_replace(config.clone());
        // No receiver is not an error.
        let _ = self.inner.events.send(ConfigEvent::Applied {
            source,
            at: SystemTime::now(),
        });
        tracing::info!(
            path = %self.inner.path.display(),
            source = ?source,
            "configuration applied"
        );
        config
    }

    fn reject(&self, issues: Vec<ConfigIssue>, source: Source, hash: ContentHash) {
        let at = SystemTime::now();
        {
            let mut hashes = self.inner.hashes.lock();
            hashes.rejected = Some(hash);
            hashes.rejection = Some(Rejection {
                issues: issues.clone(),
                at,
            });
        }
        // Issue texts name fields and rules, never values.
        let summary = issues
            .iter()
            .map(ConfigIssue::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        tracing::warn!(
            path = %self.inner.path.display(),
            issues = %summary,
            "configuration rejected; keeping the previous one"
        );
        let _ = self
            .inner
            .events
            .send(ConfigEvent::Rejected { source, issues, at });
    }

    /// Looks at the file after a change notification.
    async fn check_disk(&self, final_attempt: bool) -> Check {
        let _guard = self.inner.write_lock.lock().await;
        let bytes = match self.read_file().await {
            Ok(bytes) => bytes,
            Err(error) => {
                // Deleted (and probably about to be recreated) or locked by
                // the program writing it.
                tracing::debug!(
                    path = %self.inner.path.display(),
                    %error,
                    "configuration file is not readable right now"
                );
                return Check::Missing;
            }
        };
        let hash = content_hash(&bytes);
        match self.is_live(hash) {
            Live::Recovered => return Check::Applied,
            Live::Yes => return Check::Unchanged,
            Live::No if self.is_reported(hash) => return Check::Unchanged,
            Live::No => {}
        }
        let blank = std::str::from_utf8(&bytes).is_ok_and(is_blank);
        let parsed = if blank {
            // An empty file is a valid configuration in principle, but
            // appearing under a running gateway it is a truncated save, and
            // applying it would drop every provider and key.
            Err(vec![ConfigIssue {
                path: FILE_PATH.to_string(),
                message: "the file is empty".to_string(),
            }])
        } else {
            parse_bytes(bytes)
        };
        match parsed {
            Ok((config, text)) => {
                self.apply(Arc::new(config), Source::File, hash, &text);
                Check::Applied
            }
            Err(_) if !final_attempt => Check::Retry,
            Err(issues) => {
                self.reject(issues, Source::File, hash);
                Check::Rejected
            }
        }
    }
}

/// Whether a file holds nothing: no bytes, only white space, or only the
/// byte-order mark some Windows editors put in front of what they save — an
/// emptied document saved from such an editor is exactly those three bytes.
fn is_blank(text: &str) -> bool {
    strip_bom(text).trim().is_empty()
}

/// `config` as reading back `text`, the file just rendered for it, gives
/// it. The two are equal — the merge checks that — but maps compare without
/// regard to order: a payload rule the edit left unchanged keeps the order
/// its fields have in the file even when the edit listed them otherwise,
/// and a field added to a rule goes at its end. Reading the text back makes
/// the live configuration list them as the file does, so what an edit
/// answers is what the next reload of the file would give.
fn as_written(config: Config, text: &str) -> Config {
    match toml::from_str::<Config>(strip_bom(text)) {
        Ok(read) if read == config => read,
        _ => config,
    }
}

/// The file content as text.
fn decode(bytes: Vec<u8>) -> Result<String, Vec<ConfigIssue>> {
    String::from_utf8(bytes).map_err(|_| {
        vec![ConfigIssue {
            path: FILE_PATH.to_string(),
            message: "the file is not valid UTF-8".to_string(),
        }]
    })
}

/// The configuration in a file, together with the file content as text.
fn parse_bytes(bytes: Vec<u8>) -> Result<(Config, String), Vec<ConfigIssue>> {
    let text = decode(bytes)?;
    let config = validate_text(&text)?;
    Ok((config, text))
}

/// Runs blocking file-system work off the async threads when a Tokio runtime
/// is available, and inline otherwise.
async fn blocking<T, F>(work: F) -> std::io::Result<T>
where
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => match handle.spawn_blocking(work).await {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::other("the file operation was interrupted")),
        },
        Err(_) => work(),
    }
}

/// Whether a file-system event may have changed the configuration file.
fn event_concerns(event: &notify::Event, file_name: &OsStr) -> bool {
    // Reads (including our own) must not trigger a re-read.
    if let EventKind::Access(kind) = event.kind
        && kind != AccessKind::Close(AccessMode::Write)
    {
        return false;
    }
    event.need_rescan()
        || event.paths.is_empty()
        || event
            .paths
            .iter()
            .any(|p| p.file_name().is_some_and(|n| same_file_name(n, file_name)))
}

fn same_file_name(a: &OsStr, b: &OsStr) -> bool {
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// Cheap fingerprint used by the poll: modification time and size.
async fn file_stat(path: &Path) -> Option<(Option<SystemTime>, u64)> {
    let path = path.to_path_buf();
    blocking(move || std::fs::metadata(path))
        .await
        .ok()
        .map(|meta| (meta.modified().ok(), meta.len()))
}

async fn watch_loop(
    store: Weak<Inner>,
    mut signals: mpsc::UnboundedReceiver<()>,
    options: WatchOptions,
    native: bool,
) {
    let path = match store.upgrade() {
        Some(inner) => inner.path.clone(),
        None => return,
    };
    let mut ticker = tokio::time::interval(options.poll_interval.max(Duration::from_millis(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    // The fingerprint is always taken *before* the file is read, so a change
    // that lands while it is being read shows up at the next tick.
    let mut last_stat = file_stat(&path).await;
    // The file may have changed between loading and the start of the watch.
    let mut triggered = Some(false);

    loop {
        let settle = match triggered.take() {
            Some(settle) => settle,
            None => tokio::select! {
                signal = signals.recv() => match signal {
                    Some(()) => true,
                    // The store is gone.
                    None => return,
                },
                _ = ticker.tick() => {
                    if store.strong_count() == 0 {
                        return;
                    }
                    let stat = file_stat(&path).await;
                    let changed = stat != last_stat;
                    last_stat = stat;
                    // With working notifications the poll only has to notice
                    // what they missed. Without them it is the sole mechanism
                    // and compares content, since timestamps can be too
                    // coarse to show a quick second save.
                    if native && !changed {
                        continue;
                    }
                    false
                }
            },
        };

        if settle {
            // Wait until the events stop: one save produces several.
            loop {
                match tokio::time::timeout(options.debounce, signals.recv()).await {
                    Ok(Some(())) => continue,
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            last_stat = file_stat(&path).await;
        }

        let Some(inner) = store.upgrade() else {
            return;
        };
        let store_handle = ConfigStore { inner };
        if store_handle.check_disk(false).await == Check::Retry {
            tokio::time::sleep(options.debounce).await;
            while signals.try_recv().is_ok() {}
            last_stat = file_stat(&path).await;
            store_handle.check_disk(true).await;
        }
    }
}

/// `std::fs::canonicalize` returns verbatim (`\\?\C:\…`) paths on Windows.
/// Those do not accept `/` as a separator, which breaks joining the relative
/// paths found in configuration files, so the prefix is removed when the
/// plain form means the same.
#[cfg(windows)]
fn simplify(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    // Plain paths are limited to 260 characters unless long paths are
    // enabled system-wide.
    if text.len() >= 240 {
        return path;
    }
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'\\'
        {
            return PathBuf::from(rest);
        }
    }
    path
}

#[cfg(not(windows))]
fn simplify(path: PathBuf) -> PathBuf {
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const BASE: &str = "# my gateway\n[server]\nport = 9000 # custom\n";

    fn store_with(text: &str) -> (tempfile::TempDir, ConfigStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        std::fs::write(&path, text).unwrap();
        let store = ConfigStore::load(&path).unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn update_falls_back_to_in_place_write_when_the_temp_file_is_unusable() {
        let (dir, store) = store_with(BASE);
        // A directory occupies the temporary file's path, so the atomic
        // replace cannot even start.
        let blocked = dir.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        *store.inner.temp_override.lock() = Some(blocked.clone());

        let applied = store
            .update(|c| {
                c.server.port = 9100;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(applied.server.port, 9100);
        assert_eq!(
            store.raw_text().unwrap(),
            "# my gateway\n[server]\nport = 9100 # custom\n"
        );
        assert!(blocked.is_dir());

        // And replace_text takes the same route.
        store.replace_text("[server]\nport = 9200\n").await.unwrap();
        assert_eq!(store.raw_text().unwrap(), "[server]\nport = 9200\n");
        assert_eq!(store.current().server.port, 9200);
    }

    #[tokio::test]
    async fn failed_write_changes_nothing() {
        let (dir, store) = store_with(BASE);
        let mut events = store.events();
        // Neither the temporary file nor the target can be written: the
        // target's path is taken over by a directory.
        let blocked = dir.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        *store.inner.temp_override.lock() = Some(blocked);
        std::fs::remove_file(store.path()).unwrap();
        std::fs::create_dir(store.path()).unwrap();

        let err = store
            .update(|c| {
                c.server.port = 9100;
                Ok(())
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ConfigStoreError::Io { .. }), "{err}");
        assert_eq!(store.current().server.port, 9000);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn unusable_content_is_retried_once_before_it_is_reported() {
        let (_dir, store) = store_with(BASE);
        let mut events = store.events();
        let path = store.path().to_path_buf();

        // Unchanged content: nothing to do.
        assert_eq!(store.check_disk(false).await, Check::Unchanged);

        // Half-written: first look says "try again", silently.
        std::fs::write(&path, "[server]\nport = ").unwrap();
        assert_eq!(store.check_disk(false).await, Check::Retry);
        assert!(events.try_recv().is_err());
        // The save completes before the second look: applied, never reported.
        std::fs::write(&path, "[server]\nport = 9300\n").unwrap();
        assert_eq!(store.check_disk(true).await, Check::Applied);
        assert_eq!(store.current().server.port, 9300);
        assert!(matches!(
            events.try_recv(),
            Ok(ConfigEvent::Applied {
                source: Source::File,
                ..
            })
        ));

        // Truncated: same patience, and an empty file is never applied.
        std::fs::write(&path, "").unwrap();
        assert_eq!(store.check_disk(false).await, Check::Retry);
        assert_eq!(store.check_disk(true).await, Check::Rejected);
        assert_eq!(store.current().server.port, 9300);
        match events.try_recv() {
            Ok(ConfigEvent::Rejected { issues, .. }) => {
                assert_eq!(issues[0].path, "config");
                assert_eq!(issues[0].message, "the file is empty");
            }
            other => panic!("unexpected {other:?}"),
        }
        // Already reported: seeing it again is not news.
        assert_eq!(store.check_disk(false).await, Check::Unchanged);
        assert!(events.try_recv().is_err());

        // Semantically invalid content gets the same treatment.
        std::fs::write(&path, "[server]\nport = 0\n").unwrap();
        assert_eq!(store.check_disk(false).await, Check::Retry);
        assert_eq!(store.check_disk(true).await, Check::Rejected);
        assert!(matches!(
            events.try_recv(),
            Ok(ConfigEvent::Rejected { .. })
        ));

        // Not UTF-8.
        std::fs::write(&path, b"[server]\nhost = \"\xff\"\n").unwrap();
        assert_eq!(store.check_disk(true).await, Check::Rejected);
        match events.try_recv() {
            Ok(ConfigEvent::Rejected { issues, .. }) => {
                assert_eq!(issues[0].message, "the file is not valid UTF-8");
            }
            other => panic!("unexpected {other:?}"),
        }

        // Gone.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(store.check_disk(false).await, Check::Missing);
        assert_eq!(store.current().server.port, 9300);
        assert!(events.try_recv().is_err());
    }

    /// Regression: the memory of the last rejected content used to survive
    /// the file going back to the live content, so the same mistake made a
    /// second time was never reported.
    #[tokio::test]
    async fn a_rejection_is_news_again_once_the_file_was_good_in_between() {
        let (_dir, store) = store_with(BASE);
        let mut events = store.events();
        let path = store.path().to_path_buf();
        let bad = "[server]\nport = 0\n";
        let rejected = |event| matches!(event, Ok(ConfigEvent::Rejected { .. }));

        std::fs::write(&path, bad).unwrap();
        assert_eq!(store.check_disk(true).await, Check::Rejected);
        assert!(rejected(events.try_recv()));
        // Looking at the same broken file again is not news.
        assert_eq!(store.check_disk(true).await, Check::Unchanged);
        assert!(events.try_recv().is_err());

        let recovered = |event| {
            matches!(
                event,
                Ok(ConfigEvent::Applied {
                    source: Source::File,
                    ..
                })
            )
        };

        // Back to the live content: nothing to apply, but the recovery is
        // announced (once)…
        std::fs::write(&path, BASE).unwrap();
        assert_eq!(store.check_disk(true).await, Check::Applied);
        assert!(recovered(events.try_recv()));
        assert_eq!(store.check_disk(true).await, Check::Unchanged);
        assert!(events.try_recv().is_err());
        // …and the same mistake made again is reported again.
        std::fs::write(&path, bad).unwrap();
        assert_eq!(store.check_disk(false).await, Check::Retry);
        assert_eq!(store.check_disk(true).await, Check::Rejected);
        assert!(rejected(events.try_recv()));

        // An edit through the store that finds the live content on disk ends
        // the rejection just the same.
        std::fs::write(&path, BASE).unwrap();
        store.update(|_| Ok(())).await.unwrap();
        assert!(recovered(events.try_recv()));
        assert!(events.try_recv().is_err());
        std::fs::write(&path, bad).unwrap();
        assert_eq!(store.check_disk(true).await, Check::Rejected);
        assert!(rejected(events.try_recv()));

        // A file that is briefly unreadable or missing is not "good in
        // between": the same content coming back is the same save.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(store.check_disk(true).await, Check::Missing);
        std::fs::write(&path, bad).unwrap();
        assert_eq!(store.check_disk(true).await, Check::Unchanged);
        assert!(events.try_recv().is_err());
        assert_eq!(store.current().server.port, 9000);
    }

    /// Regression (QA-ONB-03, BE-1): a refused file was announced once and
    /// then forgotten, so whoever looked afterwards — a dashboard page
    /// loaded later — could not tell that the file was not in effect.
    #[tokio::test]
    async fn the_refusal_of_the_file_stays_queryable_until_the_file_changes() {
        let (_dir, store) = store_with(BASE);
        let path = store.path().to_path_buf();
        assert_eq!(store.rejection(), None);

        std::fs::write(&path, "[server]\nport = 0\n").unwrap();
        assert_eq!(store.check_disk(true).await, Check::Rejected);
        let refused = store.rejection().expect("the refusal is remembered");
        assert_eq!(refused.issues[0].path, "server.port");
        // Looking again changes nothing, the time included.
        assert_eq!(store.check_disk(true).await, Check::Unchanged);
        assert_eq!(store.rejection(), Some(refused.clone()));
        // Another broken version replaces it.
        std::fs::write(&path, "[server]\nport = \"x\"\n").unwrap();
        assert_eq!(store.check_disk(true).await, Check::Rejected);
        let second = store.rejection().unwrap();
        assert_ne!(second.issues, refused.issues);

        // Back to the configuration in effect: over.
        std::fs::write(&path, BASE).unwrap();
        assert_eq!(store.check_disk(true).await, Check::Applied);
        assert_eq!(store.rejection(), None);

        // A valid new version: over too.
        std::fs::write(&path, "[server]\nport = 0\n").unwrap();
        assert_eq!(store.check_disk(true).await, Check::Rejected);
        std::fs::write(&path, "[server]\nport = 9001\n").unwrap();
        assert_eq!(store.check_disk(true).await, Check::Applied);
        assert_eq!(store.rejection(), None);

        // A manual reload that refuses the file records it too, and an edit
        // refused because of it leaves it as it is.
        std::fs::write(&path, "[server]\nport = 0\n").unwrap();
        assert!(store.reload_from_disk().await.is_err());
        let refused = store.rejection().expect("refused by the reload");
        let edit = |c: &mut Config| {
            c.server.port = 9002;
            Ok(())
        };
        let error = store.update(edit).await.unwrap_err();
        assert!(matches!(error, ConfigStoreError::DiskInvalid(_)), "{error}");
        assert_eq!(store.rejection(), Some(refused));

        // Replacing the whole file ends it.
        store.replace_text(BASE).await.unwrap();
        assert_eq!(store.rejection(), None);
    }

    /// The text of the last applied configuration is the base of an edit
    /// when the file on disk is empty: there is nothing in it to lose.
    #[tokio::test]
    async fn an_edit_over_an_empty_file_starts_from_the_last_applied_text() {
        let (_dir, store) = store_with(BASE);
        let path = store.path().to_path_buf();

        // Picked up by the watcher: this is now the last applied text.
        let second = "# second version\n[server]\nport = 9400 # by hand\n";
        std::fs::write(&path, second).unwrap();
        assert_eq!(store.check_disk(true).await, Check::Applied);

        for empty in ["", "   \n"] {
            std::fs::write(&path, empty).unwrap();
            store
                .update(|c| {
                    c.server.port += 1;
                    Ok(())
                })
                .await
                .unwrap();
        }
        assert_eq!(
            store.raw_text().unwrap(),
            "# second version\n[server]\nport = 9402 # by hand\n"
        );
        assert_eq!(store.current().server.port, 9402);
    }

    /// Regression: an edit through the store used to rewrite a file that
    /// held a broken manual edit from the last valid configuration,
    /// discarding what the operator was typing. It is refused instead, the
    /// refusal is not mistaken for the watcher's verdict, and the recovery
    /// that follows a verdict is announced exactly once.
    #[tokio::test]
    async fn an_edit_never_overwrites_a_broken_file() {
        let (_dir, store) = store_with(BASE);
        let mut events = store.events();
        let path = store.path().to_path_buf();
        let edit = |c: &mut Config| {
            c.server.port += 1;
            Ok(())
        };

        for broken in [
            &b"[server\nport = "[..],
            b"[server]\nprot = 1\n",
            b"[server]\nport = 0\n",
            b"[server]\nhost = \"\xff\"\n",
        ] {
            std::fs::write(&path, broken).unwrap();
            let err = store.update(edit).await.unwrap_err();
            assert!(matches!(err, ConfigStoreError::DiskInvalid(_)), "{err}");
            assert!(!err.issues().is_empty());
            assert_eq!(std::fs::read(&path).unwrap(), broken);
            assert_eq!(store.current().server.port, 9000);
        }
        // The refusal is the caller's answer, not a verdict on the file.
        assert!(events.try_recv().is_err());

        // The watcher gives the verdict; the edit is still refused.
        assert_eq!(store.check_disk(true).await, Check::Rejected);
        assert!(matches!(
            events.try_recv(),
            Ok(ConfigEvent::Rejected { .. })
        ));
        assert!(matches!(
            store.update(edit).await,
            Err(ConfigStoreError::DiskInvalid(_))
        ));
        // An edit that changes nothing writes nothing, so it is let through.
        store.update(|_| Ok(())).await.unwrap();
        assert!(events.try_recv().is_err());

        // The file is put back: the next edit goes through, and the
        // recovery it found is announced once, ahead of the edit itself.
        std::fs::write(&path, BASE).unwrap();
        store.update(edit).await.unwrap();
        assert_eq!(store.current().server.port, 9001);
        assert!(matches!(
            events.try_recv(),
            Ok(ConfigEvent::Applied {
                source: Source::File,
                ..
            })
        ));
        assert!(matches!(
            events.try_recv(),
            Ok(ConfigEvent::Applied {
                source: Source::Admin,
                ..
            })
        ));
        assert!(events.try_recv().is_err());
        assert_eq!(store.check_disk(true).await, Check::Unchanged);
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn blank_files() {
        for blank in ["", " \n\t\r\n", "\u{feff}", "\u{feff}\r\n  \n"] {
            assert!(is_blank(blank), "{blank:?}");
        }
        for text in [
            "# only a comment\n",
            "\u{feff}[server]\n",
            "x",
            "\u{feff}\u{feff}",
        ] {
            assert!(!is_blank(text), "{text:?}");
        }
    }

    /// Regression: a file holding nothing but a byte-order mark passed for a
    /// configuration with every setting at its default and was applied.
    #[tokio::test]
    async fn a_byte_order_mark_alone_is_an_empty_file() {
        let (_dir, store) = store_with(BASE);
        let mut events = store.events();
        let path = store.path().to_path_buf();

        for content in [&b"\xEF\xBB\xBF"[..], b"\xEF\xBB\xBF\r\n\r\n"] {
            std::fs::write(&path, content).unwrap();
            assert_eq!(store.check_disk(false).await, Check::Retry);
            assert_eq!(store.check_disk(true).await, Check::Rejected);
            match events.try_recv() {
                Ok(ConfigEvent::Rejected { issues, .. }) => {
                    assert_eq!(issues[0].message, "the file is empty");
                }
                other => panic!("unexpected {other:?}"),
            }
            assert_eq!(store.current().server.port, 9000);
        }

        // An edit through the store starts from the last applied text.
        store
            .update(|c| {
                c.server.port = 9100;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            store.raw_text().unwrap(),
            "# my gateway\n[server]\nport = 9100 # custom\n"
        );

        // A mark in front of real content is still fine.
        std::fs::write(&path, "\u{feff}[server]\nport = 9300\n").unwrap();
        assert_eq!(store.check_disk(false).await, Check::Applied);
        assert_eq!(store.current().server.port, 9300);
    }

    /// Wakes the test when the future it polls by hand can make progress.
    struct Progress(tokio::sync::Notify);

    impl std::task::Wake for Progress {
        fn wake(self: Arc<Self>) {
            self.0.notify_one();
        }
    }

    /// Polls a future `polls` times — waiting, between two polls, until the
    /// operation it is blocked on has finished — and drops it right after the
    /// last poll, i.e. with the next operation just started. Returns whether
    /// the future ran to completion.
    async fn poll_and_drop<F: std::future::Future>(future: F, polls: usize) -> bool {
        let progress = Arc::new(Progress(tokio::sync::Notify::new()));
        let waker = std::task::Waker::from(progress.clone());
        let mut context = std::task::Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        for poll in 0..polls {
            if future.as_mut().poll(&mut context).is_ready() {
                return true;
            }
            if poll + 1 < polls {
                tokio::time::timeout(Duration::from_secs(10), progress.0.notified())
                    .await
                    .expect("the pending operation finishes");
            }
        }
        false
    }

    /// Regression: dropping `update()` while the file was being written left
    /// the file replaced but never applied, and released the write lock under
    /// the write. Whatever the moment of the drop, the edit is either absent
    /// or complete; the next writer finds file and live configuration in
    /// agreement and builds on them; and every change that reached the file
    /// was announced as the admin edit it is.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_update_is_either_absent_or_complete() {
        // An update waits twice: for the file to be read, then for it to be
        // written. Never polled, it has not started; after one poll it is
        // (all but always) reading; after two the write is under way at the
        // least; the third poll finds it finished.
        for polls in 0..=3 {
            let (_dir, store) = store_with(BASE);
            let mut events = store.events();

            let completed = poll_and_drop(
                store.update(|c| {
                    c.server.port = 9100;
                    Ok(())
                }),
                polls,
            )
            .await;
            assert!(completed || polls < 3);
            // The next writer queues behind whatever is still in flight.
            let applied = store
                .update(|c| {
                    c.server.host = "0.0.0.0".to_string();
                    Ok(())
                })
                .await
                .unwrap();

            let on_disk = validate_text(&store.raw_text().unwrap()).unwrap();
            assert_eq!(on_disk, *store.current(), "after {polls} poll(s)");
            assert_eq!(on_disk, *applied, "after {polls} poll(s)");
            assert_eq!(on_disk.server.host, "0.0.0.0");
            // Once the write was started it is carried through.
            let went_through = on_disk.server.port == 9100;
            match polls {
                0 => assert!(!went_through),
                1 => assert!(went_through || on_disk.server.port == 9000),
                _ => assert!(went_through, "after {polls} poll(s)"),
            }

            let mut announced = 0;
            while let Ok(event) = events.try_recv() {
                assert!(
                    matches!(
                        event,
                        ConfigEvent::Applied {
                            source: Source::Admin,
                            ..
                        }
                    ),
                    "after {polls} poll(s): {event:?}"
                );
                announced += 1;
            }
            assert_eq!(announced, 1 + usize::from(went_through));
        }
    }

    /// The same for the raw editor, whose first await after the lock is the
    /// write itself: one poll starts it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_replace_text_is_completed_and_announced() {
        let (_dir, store) = store_with(BASE);
        let mut events = store.events();
        poll_and_drop(store.replace_text("[server]\nport = 9200\n"), 1).await;

        // The lock is held until the write and the swap are both done.
        let _guard = store.inner.write_lock.lock().await;
        assert_eq!(store.raw_text().unwrap(), "[server]\nport = 9200\n");
        assert_eq!(store.current().server.port, 9200);
        assert!(matches!(
            events.try_recv(),
            Ok(ConfigEvent::Applied {
                source: Source::Admin,
                ..
            })
        ));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn event_filter() {
        use notify::event::{CreateKind, ModifyKind};
        let name = OsStr::new("switchyard.toml");
        let event = |kind, path: &str| notify::Event::new(kind).add_path(PathBuf::from(path));
        assert!(event_concerns(
            &event(
                EventKind::Modify(ModifyKind::Any),
                "/etc/sy/switchyard.toml"
            ),
            name
        ));
        assert!(event_concerns(
            &event(
                EventKind::Create(CreateKind::File),
                "/etc/sy/switchyard.toml"
            ),
            name
        ));
        assert!(!event_concerns(
            &event(EventKind::Modify(ModifyKind::Any), "/etc/sy/other.toml"),
            name
        ));
        assert!(!event_concerns(
            &event(
                EventKind::Modify(ModifyKind::Any),
                "/etc/sy/.switchyard.toml.1.tmp"
            ),
            name
        ));
        assert!(!event_concerns(
            &event(
                EventKind::Access(AccessKind::Open(AccessMode::Read)),
                "/etc/sy/switchyard.toml"
            ),
            name
        ));
        assert!(event_concerns(
            &event(
                EventKind::Access(AccessKind::Close(AccessMode::Write)),
                "/etc/sy/switchyard.toml"
            ),
            name
        ));
        // No path at all: be safe and look.
        assert!(event_concerns(&notify::Event::new(EventKind::Any), name));
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_prefix_is_removed() {
        assert_eq!(
            simplify(PathBuf::from(r"\\?\C:\Users\me\cfg")),
            PathBuf::from(r"C:\Users\me\cfg")
        );
        assert_eq!(
            simplify(PathBuf::from(r"\\?\UNC\server\share\cfg")),
            PathBuf::from(r"\\server\share\cfg")
        );
        assert_eq!(
            simplify(PathBuf::from(r"C:\plain")),
            PathBuf::from(r"C:\plain")
        );
    }
}
