//! Regression tests from the review: an invalid file used to be reported
//! only the first time its exact content appeared, even after the file had
//! been valid again in between.
//!
//! The brief: "invalid -> keep the old config, ConfigEvent::Rejected with the
//! issues, tracing::warn". The store remembers the hash of the last rejected
//! content so that the same broken save is not reported over and over. That
//! memory used to be cleared only when a configuration was *applied*.
//! Restoring the file to the content that is already live applies nothing
//! (the hash equals the applied one), so the memory survived — and when the
//! same mistake was saved again later, the watcher stayed silent: no event,
//! no log line, and the dashboard showed nothing while the file on disk was
//! not in use.

use std::time::Duration;
use switchyard_config_store::{ConfigEvent, ConfigStore, Source, WatchOptions};
use tokio::sync::broadcast::Receiver;

const GOOD: &str = "[server]\nport = 9000\n";
const BAD: &str = "[server]\nport = 0\n";

/// Replaces the file in one step (write beside it, then rename over it), so
/// the watcher can never observe a half-written file.
fn save(path: &std::path::Path, text: &str) {
    let staged = path.with_extension("staged");
    std::fs::write(&staged, text).unwrap();
    std::fs::rename(&staged, path).unwrap();
}

async fn next_event(events: &mut Receiver<ConfigEvent>) -> Option<ConfigEvent> {
    tokio::time::timeout(Duration::from_millis(1500), events.recv())
        .await
        .ok()
        .and_then(Result::ok)
}

async fn rejected_again_after_the_file_was_good(native: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("switchyard.toml");
    std::fs::write(&path, GOOD).unwrap();
    let store = ConfigStore::load(&path).unwrap();
    let mut events = store.events();
    store.spawn_watcher_with(
        &tokio::runtime::Handle::current(),
        WatchOptions {
            debounce: Duration::from_millis(40),
            poll_interval: Duration::from_millis(80),
            native,
        },
    );

    // The mistake is saved: reported once.
    save(&path, BAD);
    match next_event(&mut events).await {
        Some(ConfigEvent::Rejected { source, issues, .. }) => {
            assert_eq!(source, Source::File);
            assert_eq!(issues[0].path, "server.port");
        }
        other => panic!("expected the invalid file to be rejected, got {other:?}"),
    }

    // The edit is undone. The file is byte-identical to the live
    // configuration again, so nothing is published. Give the watcher ample
    // time (many debounce and poll periods) to look at the file.
    save(&path, GOOD);
    let quiet = tokio::time::timeout(Duration::from_millis(500), events.recv()).await;
    assert!(quiet.is_err(), "unexpected event: {quiet:?}");
    assert_eq!(store.current().server.port, 9000);

    // The same mistake is saved again. It is a new rejection of a file that
    // was valid a moment ago and must be reported.
    save(&path, BAD);
    match next_event(&mut events).await {
        Some(ConfigEvent::Rejected { issues, .. }) => {
            assert_eq!(issues[0].path, "server.port");
        }
        other => {
            panic!("the file on disk is invalid again but nothing was reported (got {other:?})")
        }
    }
    assert_eq!(store.current().server.port, 9000);
}

#[tokio::test]
async fn the_same_mistake_is_reported_again_after_the_file_was_valid_in_between() {
    rejected_again_after_the_file_was_good(true).await;
}

/// The same with polling only, where every look at the file is driven by the
/// poll timer rather than by file-system notifications.
#[tokio::test]
async fn the_same_mistake_is_reported_again_when_polling() {
    rejected_again_after_the_file_was_good(false).await;
}
