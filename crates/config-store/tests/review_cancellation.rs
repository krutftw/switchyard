//! Review finding: `update()` and `replace_text()` are not cancellation-safe.
//!
//! The brief: "Writes are serialised by an async mutex. … The in-memory config
//! is only swapped after the write succeeded." Both hold only when the future
//! is driven to completion. The write runs on the blocking pool
//! (`spawn_blocking`) while the future — which owns the write-lock guard and
//! is the only thing that will ever call `apply` — waits for it. Dropping the
//! future at that await point (what happens to an HTTP handler's future when
//! the admin client disconnects, or to anything wrapped in a timeout or a
//! `select!`):
//!
//! * does **not** stop the write: the file is replaced anyway;
//! * skips `apply`: the live configuration, the subscribers and the "last
//!   applied" hash never learn about it — the store returned nothing, yet the
//!   file changed. With no watcher the gateway keeps serving the old
//!   configuration while `raw_text()` shows the new one; with a watcher the
//!   store's own write comes back as an `Applied { source: File }`;
//! * releases the write lock **while the write is still in flight**, so the
//!   next `update()` can read the file and write it concurrently with the
//!   orphaned write. Whichever rename lands last wins on disk, which can undo
//!   an update that already returned `Ok`; when the file can only be rewritten
//!   in place (the single-file bind mount case) two unsynchronised in-place
//!   writes can interleave.
//!
//! Expected: once a store operation has started writing, the write and the
//! swap complete together (run the critical section in a task of its own, or
//! otherwise shield it from the caller's cancellation) — or nothing is
//! written. Either way the file and the live configuration agree afterwards.

use std::future::Future;
use std::path::Path;
use std::task::{Context, Waker};
use std::time::Duration;
use switchyard_config_store::{ConfigStore, validate_text};

const BASE: &str = "# Team gateway\n[server]\nport = 9000 # custom\n";

/// Polls `future` at most `polls` times, pausing after each poll so the
/// blocking file operation it is waiting for can finish, then drops it.
/// Returns whether it ran to completion.
async fn poll_then_drop<F: Future>(future: F, polls: usize) -> bool {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..polls {
        if future.as_mut().poll(&mut context).is_ready() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn port_on_disk(path: &Path) -> u16 {
    let text = std::fs::read_to_string(path).expect("the file is readable");
    validate_text(&text)
        .expect("the file holds a valid configuration")
        .server
        .port
}

/// The port in the file and the live port, once things have settled: whatever
/// was left running in the background gets time to finish, and a store that
/// completes the operation on its own gets a further second to do so.
async fn settled(path: &Path, store: &ConfigStore) -> (u16, u16) {
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut ports = (port_on_disk(path), store.current().server.port);
    for _ in 0..20 {
        if ports.0 == ports.1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        ports = (port_on_disk(path), store.current().server.port);
    }
    ports
}

/// The typed edit, dropped after one, two and three polls. After two polls
/// the write has been handed to the blocking pool; the file ends up with the
/// new port while the live configuration keeps the old one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_dropped_half_way_leaves_file_and_live_configuration_in_agreement() {
    for polls in 1..=3 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        std::fs::write(&path, BASE).unwrap();
        let store = ConfigStore::load(&path).unwrap();

        let completed = poll_then_drop(
            store.update(|c| {
                c.server.port = 9100;
                Ok(())
            }),
            polls,
        )
        .await;
        let (on_disk, live) = settled(&path, &store).await;
        assert_eq!(
            on_disk, live,
            "update() dropped after {polls} poll(s) (ran to completion: {completed}): \
             the file and the live configuration disagree"
        );
        // Only the case that exposes the problem needs the long wait.
        if !completed && on_disk == 9100 {
            break;
        }
    }
}

/// The raw editor's path: the very first await after taking the lock is the
/// write, so a single poll is enough.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replace_text_dropped_half_way_leaves_file_and_live_configuration_in_agreement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("switchyard.toml");
    std::fs::write(&path, BASE).unwrap();
    let store = ConfigStore::load(&path).unwrap();
    let mut events = store.events();

    let completed = poll_then_drop(store.replace_text("[server]\nport = 9200\n"), 1).await;
    let (on_disk, live) = settled(&path, &store).await;
    assert_eq!(
        on_disk, live,
        "replace_text() dropped after one poll (ran to completion: {completed}): \
         the file and the live configuration disagree"
    );
    // And if the write did go through, it was announced.
    if live == 9200 {
        assert!(
            events.try_recv().is_ok(),
            "the file was replaced without an Applied event"
        );
    }
}
