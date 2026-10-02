//! Review finding: the "an empty file is a truncated save" guard does not
//! recognise a file that holds nothing but a UTF-8 byte-order mark.
//!
//! The brief: "empty or half-written files (parse failure) are retried once
//! after another debounce interval before being reported; … invalid -> keep
//! the old config". The store implements this by refusing a file whose bytes
//! are all ASCII whitespace ("An empty file is a valid configuration in
//! principle, but appearing under a running gateway it is a truncated save,
//! and applying it would drop every provider and key").
//!
//! The store otherwise goes out of its way to support files that start with a
//! byte-order mark (Windows editors add one; `validate_text` strips it, the
//! merge keeps it). But the emptiness test is made on the raw bytes, before
//! the mark is stripped, so `EF BB BF` — what Notepad writes when an emptied
//! document is saved as "UTF-8 with BOM", and what a BOM-first writer leaves
//! behind when it is interrupted — is not "empty". It parses as the all-defaults
//! configuration and is applied: every provider, client key and the admin
//! secret are dropped from the running gateway.
//!
//! `update()` has the same hole (`text.trim()` does not remove U+FEFF): where
//! a zero-byte file makes it fall back to the last applied text, a BOM-only
//! file is first *adopted* (wiping the live configuration) and then used as the
//! base of the merge, so the edit is written into an otherwise empty file and
//! the last good text is gone for good.
//!
//! Expected: a file that is empty once the byte-order mark is removed is
//! treated exactly like a zero-byte file.

use std::time::Duration;
use switchyard_config_store::{ConfigEvent, ConfigStore, WatchOptions};

const BASE: &str = "# Team gateway\n[server]\nport = 9000 # custom\n\n[admin]\nsecret = \"env:ADMIN_SECRET\"\n\n# The one provider\n[[providers]]\nname = \"mock\"\nkind = \"mock\"\n";

fn fast() -> WatchOptions {
    WatchOptions {
        debounce: Duration::from_millis(60),
        poll_interval: Duration::from_millis(150),
        native: true,
    }
}

async fn watcher_sees(content: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("switchyard.toml");
    std::fs::write(&path, BASE).unwrap();
    let store = ConfigStore::load(&path).unwrap();
    let mut events = store.events();
    store.spawn_watcher_with(&tokio::runtime::Handle::current(), fast());

    std::fs::write(&path, content).unwrap();
    let event = tokio::time::timeout(Duration::from_millis(1500), events.recv()).await;

    assert!(
        !matches!(event, Ok(Ok(ConfigEvent::Applied { .. }))),
        "a file holding only {content:?} was applied as a configuration"
    );
    let live = store.current();
    assert_eq!(
        live.providers.len(),
        1,
        "the provider was dropped by a file holding only {content:?}"
    );
    assert_eq!(live.server.port, 9000);
    assert_eq!(live.admin.secret, "env:ADMIN_SECRET");
}

/// Exactly the three bytes of the mark.
#[tokio::test]
async fn the_watcher_does_not_apply_a_file_that_is_only_a_byte_order_mark() {
    watcher_sees(b"\xEF\xBB\xBF").await;
}

/// The mark followed by a line break (an "empty" document with one blank
/// line).
#[tokio::test]
async fn the_watcher_does_not_apply_a_byte_order_mark_followed_by_blank_lines() {
    watcher_sees(b"\xEF\xBB\xBF\r\n").await;
}

/// The same file under an admin edit: a zero-byte file makes `update()` start
/// from the last applied text; a BOM-only file must do the same.
#[tokio::test]
async fn an_update_over_a_file_that_is_only_a_byte_order_mark_restores_the_last_good_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("switchyard.toml");
    std::fs::write(&path, BASE).unwrap();
    let store = ConfigStore::load(&path).unwrap();

    std::fs::write(&path, b"\xEF\xBB\xBF").unwrap();
    let applied = store
        .update(|c| {
            c.server.port = 9100;
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(applied.server.port, 9100);
    assert_eq!(
        applied.providers.len(),
        1,
        "the edit was applied to an all-defaults configuration: the provider is gone"
    );
    assert_eq!(applied.admin.secret, "env:ADMIN_SECRET");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("# The one provider\n[[providers]]\nname = \"mock\"\n"),
        "the last good text was not restored:\n{text}"
    );
}
