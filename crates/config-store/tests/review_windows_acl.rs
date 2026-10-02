//! Review finding: on Windows every write through the store resets the
//! configuration file's access-control list.
//!
//! The brief: "Persisting: write to a temp file in the same directory + fsync
//! + rename over the original … Preserve the original file's permissions."
//!
//! `persist.rs` copies the Unix mode onto the temporary file and does nothing
//! on other platforms ("Only Unix has permissions worth copying: the one
//! Windows attribute `std` exposes is read-only"). But Windows files do have
//! permissions — the ACL — and the rename replaces the original file object
//! with the temporary one, whose ACL is whatever the *directory* hands down.
//! An administrator who locked the file down (it holds API keys, client keys
//! and the admin secret) loses that protection the first time anything is
//! changed in the dashboard: afterwards the file is readable by everyone the
//! directory is readable by.
//!
//! The test restricts the file to the current user with inheritance switched
//! off, makes one edit through the store, and compares the ACL (as printed by
//! `icacls`, which ships with Windows) before and after.
//!
//! Expected: the file's ACL is the same after the write — replace the file in
//! a way that keeps its security descriptor, or rewrite such a file in place
//! (the store already has a careful in-place path).

#![cfg(windows)]

use std::path::Path;
use std::process::Command;
use switchyard_config_store::ConfigStore;

const BASE: &str = "# Team gateway\n[server]\nport = 9000 # custom\n";

/// The file's access-control entries as `icacls` prints them, one per
/// element, without the path. `None` when `icacls` cannot be run.
fn access_control_entries(path: &Path) -> Option<Vec<String>> {
    let output = Command::new("icacls").arg(path).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).replace(&path.display().to_string(), "");
    let mut entries: Vec<String> = text
        .lines()
        .map(str::trim)
        // Entries look like `NAME:(I)(F)`; the summary line does not.
        .filter(|line| line.contains(":("))
        .map(str::to_string)
        .collect();
    entries.sort();
    Some(entries)
}

/// Removes inherited entries and grants the current user full control: the
/// file is now private to that user.
fn restrict_to_current_user(path: &Path) -> bool {
    let Ok(user) = std::env::var("USERNAME") else {
        return false;
    };
    Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{user}:(F)"))
        .output()
        .is_ok_and(|output| output.status.success())
}

#[tokio::test]
async fn a_write_through_the_store_keeps_the_files_access_control_list() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("switchyard.toml");
    std::fs::write(&path, BASE).unwrap();

    if !restrict_to_current_user(&path) {
        eprintln!("icacls is not usable here; skipping");
        return;
    }
    let Some(before) = access_control_entries(&path) else {
        eprintln!("icacls is not usable here; skipping");
        return;
    };
    assert_eq!(
        before.len(),
        1,
        "the file should be private to one user now: {before:?}"
    );
    assert!(
        !before[0].contains("(I)"),
        "the entry should not be inherited: {before:?}"
    );

    let store = ConfigStore::load(&path).unwrap();
    store
        .update(|c| {
            c.server.port = 9100;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        BASE.replace("9000", "9100")
    );
    let after_update = access_control_entries(&path).expect("icacls worked a moment ago");
    assert_eq!(
        after_update, before,
        "update() replaced the file's access-control list with the directory's"
    );

    store.replace_text("[server]\nport = 9200\n").await.unwrap();
    let after_replace = access_control_entries(&path).expect("icacls worked a moment ago");
    assert_eq!(
        after_replace, before,
        "replace_text() replaced the file's access-control list with the directory's"
    );
}
