//! Writing the configuration file safely.

use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// SHA-256 of a file's content; used to recognise content the store has
/// already applied or written itself.
pub(crate) type ContentHash = [u8; 32];

pub(crate) fn content_hash(bytes: &[u8]) -> ContentHash {
    Sha256::digest(bytes).into()
}

/// Creates a configuration file with the given text.
///
/// Fails with [`io::ErrorKind::AlreadyExists`] when the file exists, so an
/// existing configuration is never overwritten. Missing parent directories
/// are created. On Unix the file is readable and writable by its owner only,
/// because it usually holds API keys.
pub fn write_new(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    write_new_with(path, text.as_bytes(), &write_all)
}

fn write_new_with(path: &Path, bytes: &[u8], fill: Fill<'_>) -> io::Result<()> {
    let mut file = owner_only(OpenOptions::new().write(true).create_new(true)).open(path)?;
    let written = fill(&mut file, bytes).and_then(|()| file.sync_all());
    drop(file);
    if written.is_err() {
        // A partial file would be taken for a configuration at the next
        // start, and would make a second attempt fail with "already exists".
        let _ = fs::remove_file(path);
    }
    written
}

/// New files must not be readable by other users.
fn owner_only(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// How [`persist`] got the bytes onto disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteMode {
    /// Temporary file + rename: readers never see a partial file.
    Atomic,
    /// The file itself was rewritten: because it cannot be replaced by rename
    /// (a single-file bind mount, a read-only directory), or because a
    /// replacement would not be the same file to the operating system (its
    /// access-control list on Windows, its owner on Unix).
    InPlace,
}

/// Whether an existing file is always rewritten in place.
///
/// On Windows a file's permissions are its access-control list, and that
/// belongs to the file object. A temporary file renamed over the original is
/// a new object carrying whatever the *directory* hands down, so a
/// configuration file an administrator restricted (it holds API keys) would
/// become readable by everyone the directory is readable by at the first
/// save — and its content would sit in the temporary file under that wider
/// list before the rename. The standard library can neither read nor set an
/// access-control list, and this workspace uses no `unsafe` to call the
/// system functions that can. Writing into the file itself keeps the list,
/// along with everything else that is attached to the file object.
///
/// The price is that the write is not atomic there: a reader can catch the
/// file half-written for a moment (the store's own watcher reads again in
/// that case), and a power failure in the middle of a write can leave a
/// damaged file. A failed write is rolled back, see [`write_in_place`].
const REWRITES_EXISTING_FILE: bool = cfg!(windows);

/// How long to wait before each further attempt to open a file that another
/// process holds open (a quarter of a second in all).
const BUSY_WAITS: [Duration; 4] = [
    Duration::from_millis(25),
    Duration::from_millis(50),
    Duration::from_millis(75),
    Duration::from_millis(100),
];

/// Whether opening the file failed because another process has it open
/// without sharing it, or has locked part of it (Windows error codes 32 and
/// 33). Other systems do not refuse an open for that reason.
fn is_held_by_another_process(error: &io::Error) -> bool {
    cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33))
}

/// Writes the content into an open file. A parameter so that tests can make
/// the write fail the way a full disk does.
type Fill<'a> = &'a dyn Fn(&mut File, &[u8]) -> io::Result<()>;

fn write_all(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)
}

/// Why the temporary-file route failed.
enum AtomicFailure {
    /// The temporary file could not be created or could not take the place
    /// of the original. That is about the *directory* (not writable, the file
    /// is a bind mount, another process holds it), so writing into the file
    /// itself may still work.
    Replace(io::Error),
    /// The temporary file could not be given the owner and group of the
    /// original (only root may give files away). Writing into the file
    /// itself keeps them.
    KeepOwner(io::Error),
    /// The content could not be written out (disk full, quota exceeded, I/O
    /// error). Writing into the original would fail the same way, only after
    /// damaging it, so nothing more is tried.
    Write(io::Error),
}

/// Replaces the content of the configuration file.
///
/// The bytes go to a temporary file beside the target, are flushed to disk,
/// and the temporary file is renamed over the original, so the file is at all
/// times either the old or the new version.
///
/// When the temporary file cannot be created, or cannot be renamed over the
/// original, the file is rewritten in place instead. When *writing* the
/// temporary file fails — a full disk — nothing else is attempted and the
/// original is left exactly as it was.
///
/// The original file's permissions are kept, and a symlinked configuration
/// stays a symlink (the link's target is replaced):
///
/// * **Unix** — the replacement gets the original's mode, owner and group.
///   When the owner or group cannot be set (the process is not root and does
///   not own the original) the file is rewritten in place, which keeps them;
///   only when that is not permitted either is it replaced by a file owned by
///   the process — the one way left to save at all. Extended attributes
///   (POSIX ACLs, SELinux labels set on the file itself) are not carried over
///   to a replacement; the directory's defaults apply to it.
/// * **Windows** — an existing file is always rewritten in place, so that its
///   access-control list stays (see [`REWRITES_EXISTING_FILE`]).
///
/// `temp_override` replaces the generated temporary path; tests use it to
/// make the atomic path fail.
pub(crate) fn persist(
    path: &Path,
    bytes: &[u8],
    temp_override: Option<&Path>,
) -> io::Result<WriteMode> {
    persist_with(path, bytes, temp_override, &write_all)
}

fn persist_with(
    path: &Path,
    bytes: &[u8],
    temp_override: Option<&Path>,
    fill: Fill<'_>,
) -> io::Result<WriteMode> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if REWRITES_EXISTING_FILE && fs::metadata(&target).is_ok() {
        // No fallback to a replacement: one that succeeded where writing the
        // file was refused would also drop the permissions that refused it.
        let mut waited = 0;
        loop {
            match write_in_place(&target, bytes, fill) {
                Ok(()) => return Ok(WriteMode::InPlace),
                // Another program has the file open for itself: a virus
                // scanner or an indexer looking at the previous save, an
                // editor in the middle of its own. That usually passes
                // within moments.
                Err(error) if is_held_by_another_process(&error) && waited < BUSY_WAITS.len() => {
                    std::thread::sleep(BUSY_WAITS[waited]);
                    waited += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }
    let tmp = match temp_override {
        Some(tmp) => tmp.to_path_buf(),
        None => temp_path(&target),
    };
    let (atomic_error, owner_at_stake) = match write_atomic(&target, &tmp, bytes, fill, true) {
        Ok(()) => return Ok(WriteMode::Atomic),
        Err(AtomicFailure::Write(error)) => return Err(error),
        Err(AtomicFailure::Replace(error)) => (error, false),
        Err(AtomicFailure::KeepOwner(error)) => (error, true),
    };
    let in_place_error = match write_in_place(&target, bytes, fill) {
        Ok(()) => {
            tracing::debug!(
                path = %path.display(),
                error = %atomic_error,
                "configuration file could not be replaced atomically; rewrote it in place"
            );
            return Ok(WriteMode::InPlace);
        }
        Err(error) => error,
    };
    if owner_at_stake {
        // The file belongs to someone else and cannot be written to, but its
        // directory lets this process replace it. Saving matters more than
        // the owner.
        match write_atomic(&target, &tmp, bytes, fill, false) {
            Ok(()) => {
                tracing::warn!(
                    path = %path.display(),
                    "the configuration file could only be saved by replacing it; \
                     it is now owned by the user the gateway runs as"
                );
                return Ok(WriteMode::Atomic);
            }
            Err(AtomicFailure::Write(error)) => return Err(error),
            Err(AtomicFailure::Replace(_) | AtomicFailure::KeepOwner(_)) => {}
        }
    }
    Err(io::Error::new(
        in_place_error.kind(),
        format!("{in_place_error} (replacing the file failed first: {atomic_error})"),
    ))
}

fn temp_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp_name = format!(".{name}.{}.{nanos:09}.tmp", std::process::id());
    match target.parent() {
        Some(parent) => parent.join(tmp_name),
        None => PathBuf::from(tmp_name),
    }
}

/// Writes `bytes` to `tmp` and renames it over `target`. With `keep_owner`,
/// a replacement that cannot be given the original's owner and group is not
/// put in place.
fn write_atomic(
    target: &Path,
    tmp: &Path,
    bytes: &[u8],
    fill: Fill<'_>,
    keep_owner: bool,
) -> Result<(), AtomicFailure> {
    let mut file = owner_only(OpenOptions::new().write(true).create_new(true))
        .open(tmp)
        .map_err(AtomicFailure::Replace)?;
    // From here on the temporary file exists and must not be left behind.
    let discard = |failure: AtomicFailure| {
        let _ = fs::remove_file(tmp);
        failure
    };
    if let Err(error) = fill(&mut file, bytes).and_then(|()| file.sync_all()) {
        drop(file);
        return Err(discard(AtomicFailure::Write(error)));
    }
    drop(file);
    if keep_owner {
        copy_owner(target, tmp).map_err(|e| discard(AtomicFailure::KeepOwner(e)))?;
    }
    copy_permissions(target, tmp).map_err(|e| discard(AtomicFailure::Replace(e)))?;
    fs::rename(tmp, target).map_err(|e| discard(AtomicFailure::Replace(e)))?;
    sync_parent(target);
    Ok(())
}

/// Rewrites the file itself. Not atomic, so two precautions limit the damage
/// of a failure half-way: the old content is overwritten rather than
/// truncated first (a shorter or equally long replacement needs no new disk
/// space), and when writing fails the previous content is put back.
fn write_in_place(target: &Path, bytes: &[u8], fill: Fill<'_>) -> io::Result<()> {
    enum Before {
        Content(Vec<u8>),
        Absent,
        /// Exists but cannot be read: nothing to put back, nothing to remove.
        Unknown,
    }
    let before = match fs::read(target) {
        Ok(content) => Before::Content(content),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Before::Absent,
        Err(_) => Before::Unknown,
    };
    let written = (|| {
        let mut file =
            owner_only(OpenOptions::new().write(true).create(true).truncate(false)).open(target)?;
        fill(&mut file, bytes)?;
        file.set_len(bytes.len() as u64)?;
        file.sync_all()
    })();
    if written.is_err() {
        // Best effort; the error that is reported is the original one.
        match &before {
            // Written over what is there, not after truncating: the space
            // the old content occupied is still the file's, so putting it
            // back cannot fail for lack of space — the very condition that
            // most likely caused the failure.
            Before::Content(previous) => {
                let _ = OpenOptions::new()
                    .write(true)
                    .open(target)
                    .and_then(|mut file| {
                        file.write_all(previous)?;
                        file.set_len(previous.len() as u64)?;
                        file.sync_all()
                    });
            }
            // The file did not exist before: do not leave a partial one.
            Before::Absent => {
                let _ = fs::remove_file(target);
            }
            Before::Unknown => {}
        }
    }
    written
}

/// Gives the replacement file the owner and group of the file it replaces,
/// when they differ from its own. A gateway run as root must not turn a
/// file that belongs to the service user into one only root can read.
#[cfg(unix)]
fn copy_owner(target: &Path, tmp: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Ok(original) = fs::metadata(target) else {
        // No original (it was deleted meanwhile): nothing to keep.
        return Ok(());
    };
    let created = fs::metadata(tmp)?;
    if created.uid() == original.uid() && created.gid() == original.gid() {
        return Ok(());
    }
    std::os::unix::fs::chown(tmp, Some(original.uid()), Some(original.gid()))
}

#[cfg(not(unix))]
fn copy_owner(_target: &Path, _tmp: &Path) -> io::Result<()> {
    Ok(())
}

/// Gives the replacement file the mode of the file it replaces. (Elsewhere a
/// replacement only ever stands in for a file that did not exist.)
#[cfg(unix)]
fn copy_permissions(target: &Path, tmp: &Path) -> io::Result<()> {
    match fs::metadata(target) {
        Ok(meta) => fs::set_permissions(tmp, meta.permissions()),
        // No original (it was deleted meanwhile): keep the owner-only mode.
        Err(_) => Ok(()),
    }
}

#[cfg(not(unix))]
fn copy_permissions(_target: &Path, _tmp: &Path) -> io::Result<()> {
    Ok(())
}

/// Makes the rename itself durable. Failure is harmless: the data is on disk
/// under one of the two names either way.
#[cfg(unix)]
fn sync_parent(target: &Path) {
    if let Some(parent) = target.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent(_target: &Path) {
    // Directories cannot be opened for syncing on this platform.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_new_creates_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("switchyard.toml");
        write_new(&path, "[server]\nport = 1\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "[server]\nport = 1\n");
        let err = write_new(&path, "other").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&path).unwrap(), "[server]\nport = 1\n");
    }

    #[cfg(unix)]
    #[test]
    fn write_new_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        write_new(&path, "").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// How an existing file is replaced when nothing stands in the way.
    const USUAL: WriteMode = if REWRITES_EXISTING_FILE {
        WriteMode::InPlace
    } else {
        WriteMode::Atomic
    };

    #[test]
    fn persist_replaces_the_content_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        fs::write(&path, "old content, longer than the new").unwrap();
        assert_eq!(persist(&path, b"new", None).unwrap(), USUAL);
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(names_in(dir.path()), vec!["switchyard.toml"]);
    }

    /// The temporary-file route itself, on every platform (on Windows
    /// `persist` only takes it for a file that does not exist yet).
    #[test]
    fn the_atomic_route_replaces_the_file_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        fs::write(&path, "old").unwrap();
        let tmp = temp_path(&path);
        assert!(write_atomic(&path, &tmp, b"new", &write_all, true).is_ok());
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(names_in(dir.path()), vec!["switchyard.toml"]);

        // A write error is told apart from "cannot replace", and cleans up.
        let failure = write_atomic(&path, &tmp, b"newer", &disk_full, true);
        assert!(matches!(failure, Err(AtomicFailure::Write(_))));
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(names_in(dir.path()), vec!["switchyard.toml"]);
    }

    /// Windows: the file that is there is the file that is written, so
    /// whatever is attached to it — above all its access-control list —
    /// stays. Shown here by an open handle that keeps seeing the file.
    #[cfg(windows)]
    #[test]
    fn an_existing_file_is_rewritten_not_replaced() {
        use std::io::{Read, Seek, SeekFrom};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        fs::write(&path, "old").unwrap();
        let mut handle = File::open(&path).unwrap();
        assert_eq!(
            persist(&path, b"new content", None).unwrap(),
            WriteMode::InPlace
        );
        let mut seen = String::new();
        handle.seek(SeekFrom::Start(0)).unwrap();
        handle.read_to_string(&mut seen).unwrap();
        assert_eq!(seen, "new content", "the handle points at a replaced file");
    }

    /// Windows: a file another program holds open for a moment is waited
    /// for; one that stays held is an error and keeps its content.
    #[cfg(windows)]
    #[test]
    fn a_file_held_by_another_process_is_waited_for_briefly() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        fs::write(&path, "old").unwrap();
        // Opened without sharing: nobody else can open the file meanwhile.
        let exclusive = || {
            OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&path)
                .unwrap()
        };

        let held = exclusive();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            drop(held);
        });
        assert_eq!(persist(&path, b"new", None).unwrap(), WriteMode::InPlace);
        release.join().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");

        let held = exclusive();
        let error = persist(&path, b"newer", None).unwrap_err();
        assert!(is_held_by_another_process(&error), "{error}");
        drop(held);
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(names_in(dir.path()), vec!["switchyard.toml"]);
    }

    #[test]
    fn persist_falls_back_to_in_place_when_the_temp_file_is_unusable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        fs::write(&path, "old content that is longer").unwrap();
        // A directory sits where the temporary file should go.
        let blocked = dir.path().join("blocked.tmp");
        fs::create_dir(&blocked).unwrap();
        assert_eq!(
            persist(&path, b"new", Some(&blocked)).unwrap(),
            WriteMode::InPlace
        );
        // Truncated, not merely overwritten at the start.
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert!(blocked.is_dir());
    }

    /// What a full disk looks like: part of the content goes out, then the
    /// write fails.
    fn disk_full(file: &mut File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(&bytes[..bytes.len() / 2])?;
        Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "no space left on device",
        ))
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Regression: a failed write of the temporary file used to trigger the
    /// in-place fallback, which truncated the original and then failed the
    /// same way, leaving an empty configuration behind.
    #[test]
    fn a_failed_write_of_the_temporary_file_leaves_the_original_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        fs::write(&path, "[server]\nport = 9000\n").unwrap();
        let err = persist_with(&path, b"[server]\nport = 9100\n", None, &disk_full).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[server]\nport = 9000\n",
            "the original must be what it was after a write error"
        );
        assert_eq!(
            names_in(dir.path()),
            vec!["switchyard.toml"],
            "no temp file left"
        );
    }

    /// When the file can only be rewritten in place and that write fails
    /// half-way, the previous content is put back.
    #[test]
    fn a_failed_in_place_write_restores_the_previous_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        let original = "# keep me\n[server]\nport = 9000\n";
        fs::write(&path, original).unwrap();
        let blocked = dir.path().join("blocked.tmp");
        fs::create_dir(&blocked).unwrap();
        let err = persist_with(
            &path,
            b"[server]\nport = 9100\n# a longer replacement than the original text\n",
            Some(&blocked),
            &disk_full,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
        assert!(err.to_string().contains("no space left"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), original);

        // A file that did not exist is not left behind half-written.
        let fresh = dir.path().join("fresh.toml");
        persist_with(&fresh, b"[server]\nport = 1\n", Some(&blocked), &disk_full).unwrap_err();
        assert!(!fresh.exists());
    }

    #[test]
    fn write_new_does_not_leave_a_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        write_new_with(&path, b"[server]\nport = 1\n", &disk_full).unwrap_err();
        assert!(!path.exists());
        // So a second attempt can succeed.
        write_new(&path, "[server]\nport = 1\n").unwrap();
    }

    #[test]
    fn in_place_rewrite_handles_longer_and_shorter_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        let blocked = dir.path().join("blocked.tmp");
        fs::create_dir(&blocked).unwrap();
        fs::write(&path, "short").unwrap();
        for text in ["a much longer replacement text", "tiny", ""] {
            assert_eq!(
                persist(&path, text.as_bytes(), Some(&blocked)).unwrap(),
                WriteMode::InPlace
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
    }

    #[test]
    fn persist_creates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        persist(&path, b"fresh", None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "fresh");
    }

    #[cfg(unix)]
    #[test]
    fn persist_keeps_the_original_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(persist(&path, b"new", None).unwrap(), WriteMode::Atomic);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    /// The owner is kept by a replacement whenever the process may set it.
    /// As an ordinary user that is the case for files it owns (nothing to
    /// change); as root for any file.
    #[cfg(unix)]
    #[test]
    fn persist_keeps_the_original_owner() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        fs::write(&path, "old").unwrap();
        // Root can give the file to somebody else first, which is the case
        // that matters; anyone else keeps the file as created.
        let _ = std::os::unix::fs::chown(&path, Some(12345), Some(12345));
        let before = fs::metadata(&path).unwrap();
        persist(&path, b"new", None).unwrap();
        let after = fs::metadata(&path).unwrap();
        assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn persist_keeps_a_symlink_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        let link = dir.path().join("switchyard.toml");
        fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        persist(&link, b"new", None).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
    }

    #[test]
    fn hashes_differ_by_content() {
        assert_eq!(content_hash(b"a"), content_hash(b"a"));
        assert_ne!(content_hash(b"a"), content_hash(b"b"));
    }
}
