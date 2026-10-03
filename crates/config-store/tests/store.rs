//! The store itself: loading, editing, the file watcher.

use pretty_assertions::assert_eq;
use std::path::{Path, PathBuf};
use std::time::Duration;
use switchyard_config_store::merge::REWRITE_HEADER;
use switchyard_config_store::{ConfigEvent, ConfigStore, ConfigStoreError, Source, WatchOptions};
use switchyard_core::Config;
use switchyard_core::config::{ClientKey, ProviderConfig, ProviderKind, TlsConfig};
use tokio::sync::broadcast::Receiver;

const BASE: &str = r#"# Team gateway
[server]
host = "127.0.0.1"   # loopback only
port = 9000

[admin]
secret = "env:ADMIN_SECRET"

# The one provider
[[providers]]
name = "mock"
kind = "mock"
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: ConfigStore,
}

fn fixture(text: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("switchyard.toml");
    std::fs::write(&path, text).unwrap();
    let store = ConfigStore::load(&path).unwrap();
    Fixture {
        _dir: dir,
        path,
        store,
    }
}

/// Fast timing so each test stays well under a second or two.
fn fast() -> WatchOptions {
    WatchOptions {
        debounce: Duration::from_millis(60),
        poll_interval: Duration::from_millis(150),
        native: true,
    }
}

async fn next_event(events: &mut Receiver<ConfigEvent>) -> ConfigEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("timed out waiting for a configuration event")
        .expect("event channel closed")
}

/// Asserts that nothing is published for a while (several debounce and poll
/// periods of [`fast`]).
async fn assert_quiet(events: &mut Receiver<ConfigEvent>) {
    let waited = tokio::time::timeout(Duration::from_millis(600), events.recv()).await;
    assert!(waited.is_err(), "unexpected event: {waited:?}");
}

fn key(value: &str) -> ClientKey {
    ClientKey {
        key: value.to_string(),
        name: String::new(),
        enabled: true,
        models: Vec::new(),
        rate_limit_rpm: None,
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

#[test]
fn load_reads_the_file() {
    let f = fixture(BASE);
    let config = f.store.current();
    assert_eq!(config.server.port, 9000);
    assert_eq!(config.providers[0].name, "mock");

    assert!(f.store.path().is_absolute());
    assert_eq!(f.store.path().file_name().unwrap(), "switchyard.toml");
    assert_eq!(f.store.path().parent().unwrap(), f.store.dir());
    assert_eq!(f.store.raw_text().unwrap(), BASE);
    assert!(f.store.modified_at().is_some());
    assert!(f.store.restart_required().is_empty());

    // Relative paths resolve against the file's directory, whatever the
    // separator; absolute ones are kept.
    let resolved = f.store.resolve_path("data/usage");
    assert_eq!(resolved, f.store.dir().join("data").join("usage"));
    assert!(resolved.starts_with(f.store.dir()));
    std::fs::create_dir_all(&resolved).unwrap();
    assert!(resolved.is_dir());
    let absolute = f.store.dir().join("elsewhere");
    assert_eq!(f.store.resolve_path(absolute.to_str().unwrap()), absolute);

    // Clones share state.
    let clone = f.store.clone();
    assert_eq!(clone.path(), f.store.path());
}

#[test]
fn load_of_a_missing_file_is_an_io_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nope.toml");
    match ConfigStore::load(&path).unwrap_err() {
        ConfigStoreError::Io {
            path: reported,
            source,
        } => {
            assert_eq!(reported.file_name().unwrap(), "nope.toml");
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("expected Io, got {other}"),
    }
}

#[test]
fn load_of_unparsable_text_reports_the_position() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("switchyard.toml");
    std::fs::write(&path, "[server]\nport = 9000\nhost = \n").unwrap();
    let err = ConfigStore::load(&path).unwrap_err();
    let issues = err.issues();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].path, "line 3, column 8");
    assert!(
        err.to_string()
            .starts_with("invalid configuration: line 3, column 8: ")
    );
}

#[test]
fn load_of_an_invalid_config_lists_every_issue() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("switchyard.toml");
    std::fs::write(
        &path,
        "[server]\nport = 0\nhost = \"\"\n\n[[providers]]\nname = \"UPPER\"\nkind = \"openai-compat\"\n",
    )
    .unwrap();
    let err = ConfigStore::load(&path).unwrap_err();
    assert!(matches!(err, ConfigStoreError::Invalid(_)));
    let paths: Vec<&str> = err.issues().iter().map(|i| i.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "server.port",
            "server.host",
            "providers[0].name",
            "providers[0].base_url"
        ]
    );
    let message = err.to_string();
    for path in paths {
        assert!(message.contains(path), "{message}");
    }
}

#[test]
fn an_empty_file_loads_as_the_defaults() {
    let f = fixture("");
    assert_eq!(*f.store.current(), Config::default());
}

#[test]
fn the_store_and_its_futures_can_cross_threads() {
    fn assert_send_sync_clone<T: Send + Sync + Clone + 'static>() {}
    fn assert_send<T: Send>(_: &T) {}
    assert_send_sync_clone::<ConfigStore>();

    let f = fixture(BASE);
    // Handlers of a multi-threaded server hold these across await points.
    assert_send(&f.store.update(|c| {
        c.server.port = 1;
        Ok(())
    }));
    assert_send(&f.store.replace_text(""));
    assert_send(&f.store.reload_from_disk());
}

// ---------------------------------------------------------------------------
// Typed updates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_persists_preserving_formatting_and_notifies() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    let mut updates = f.store.subscribe();
    assert_eq!(updates.borrow_and_update().server.port, 9000);

    let applied = f
        .store
        .update(|c| {
            c.server.port = 9100;
            c.auth.keys.push(key("sy-new"));
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(applied.server.port, 9100);
    assert_eq!(*f.store.current(), *applied);

    // The file: one value changed, one block added, comments intact.
    assert_eq!(
        std::fs::read_to_string(&f.path).unwrap(),
        BASE.replace("port = 9000", "port = 9100") + "\n[[auth.keys]]\nkey = \"sy-new\"\n"
    );
    // What is on disk is what is live.
    let reloaded = ConfigStore::load(&f.path).unwrap();
    assert_eq!(*reloaded.current(), *applied);

    // Subscribers saw it, exactly once.
    assert!(updates.has_changed().unwrap());
    assert_eq!(updates.borrow_and_update().server.port, 9100);
    assert!(!updates.has_changed().unwrap());
    match next_event(&mut events).await {
        ConfigEvent::Applied { source, .. } => assert_eq!(source, Source::Admin),
        other => panic!("unexpected {other:?}"),
    }
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn update_that_changes_nothing_writes_nothing() {
    let f = fixture(BASE);
    let before = f.store.modified_at();
    let mut events = f.store.events();
    let same = f.store.update(|_| Ok(())).await.unwrap();
    assert_eq!(same.server.port, 9000);
    let same = f
        .store
        .update(|c| {
            c.server.port = 9000;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(same.server.port, 9000);
    assert_eq!(std::fs::read_to_string(&f.path).unwrap(), BASE);
    assert_eq!(f.store.modified_at(), before);
    assert!(events.try_recv().is_err());
}

/// Regression (A2-1): settings put back to their default leave the file,
/// also when the file spelled the default out and nothing changes.
#[tokio::test]
async fn unset_settings_are_taken_out_of_the_file() {
    let text = BASE.replace("port = 9000", "port = 8317");
    let f = fixture(&text);
    let mut events = f.store.events();

    // Nothing changes, but the explicit default goes.
    let applied = f
        .store
        .update_unsetting(|c| {
            c.server.port = 8317;
            Ok(vec!["server.port".to_string()])
        })
        .await
        .unwrap();
    assert_eq!(applied.server.port, 8317);
    assert_eq!(
        std::fs::read_to_string(&f.path).unwrap(),
        text.replace("port = 8317\n", "")
    );
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied { .. }
    ));

    // Asked again, there is nothing left to take out: no write.
    let before = std::fs::read_to_string(&f.path).unwrap();
    f.store
        .update_unsetting(|_| Ok(vec!["server.port".to_string()]))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&f.path).unwrap(), before);
    assert!(events.try_recv().is_err());

    // A value that is not the default is written, not dropped.
    f.store
        .update_unsetting(|c| {
            c.server.host = "0.0.0.0".into();
            Ok(vec!["server.host".to_string()])
        })
        .await
        .unwrap();
    assert!(
        std::fs::read_to_string(&f.path)
            .unwrap()
            .contains("host = \"0.0.0.0\"")
    );
}

/// Regression (A2-8): what an edit leaves live lists the fields of a
/// payload rule as the file does, whatever order the edit gave them.
#[tokio::test]
async fn the_live_configuration_keeps_the_fields_of_payload_rules_in_file_order() {
    let text = format!(
        "{BASE}\n[[payload.override]]\nmodels = [\"*\"]\nset = {{ \"zeta\" = 1, \"alpha\" = 2 }}\n"
    );
    let f = fixture(&text);
    let order = |config: &Config| -> Vec<String> {
        config.payload.overrides[0].set.keys().cloned().collect()
    };
    assert_eq!(order(&f.store.current()), ["zeta", "alpha"]);

    // The rule sent back sorted, with one field added in the middle.
    let applied = f
        .store
        .update(|c| {
            let rule = &mut c.payload.overrides[0];
            rule.set.insert("beta".into(), serde_json::json!(3));
            rule.set.sort_keys();
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(order(&applied), ["zeta", "alpha", "beta"]);
    assert_eq!(order(&f.store.current()), ["zeta", "alpha", "beta"]);
    assert!(
        std::fs::read_to_string(&f.path)
            .unwrap()
            .contains("set = { \"zeta\" = 1, \"alpha\" = 2, beta = 3 }")
    );
}

#[tokio::test]
async fn failed_edits_change_nothing() {
    let f = fixture(BASE);
    let mut events = f.store.events();

    let err = f
        .store
        .update(|c| {
            c.server.port = 1;
            Err("no such provider `x`".to_string())
        })
        .await
        .unwrap_err();
    assert!(matches!(&err, ConfigStoreError::Edit(m) if m == "no such provider `x`"));

    let err = f
        .store
        .update(|c| {
            c.server.port = 0;
            c.providers
                .push(ProviderConfig::new("mock", ProviderKind::Mock));
            Ok(())
        })
        .await
        .unwrap_err();
    let paths: Vec<&str> = err.issues().iter().map(|i| i.path.as_str()).collect();
    assert_eq!(paths, vec!["server.port", "providers[1].name"]);

    assert_eq!(f.store.current().server.port, 9000);
    assert_eq!(std::fs::read_to_string(&f.path).unwrap(), BASE);
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn update_adopts_an_edit_made_on_disk_first() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    // Someone edits the file; no watcher is running to notice.
    let external = BASE.replace("port = 9000", "port = 9500");
    std::fs::write(&f.path, &external).unwrap();

    let applied = f
        .store
        .update(|c| {
            assert_eq!(
                c.server.port, 9500,
                "the edit starts from the file's content"
            );
            c.server.host = "0.0.0.0".into();
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(applied.server.port, 9500);
    assert_eq!(applied.server.host, "0.0.0.0");
    assert_eq!(
        std::fs::read_to_string(&f.path).unwrap(),
        external.replace("host = \"127.0.0.1\"", "host = \"0.0.0.0\"")
    );
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::Admin,
            ..
        }
    ));
}

/// A manual edit left the file unparsable, semantically invalid or not even
/// text. An edit through the store must not overwrite it from the last valid
/// configuration — that would silently discard what the operator was typing:
/// it is refused, naming what is wrong with the file, and nothing changes.
#[tokio::test]
async fn update_refuses_to_overwrite_a_broken_file() {
    let semantically_invalid = BASE
        .replace("port = 9000", "port = 0 # zero is not a port")
        .replace("# Team gateway", "# Team gateway (edited by hand)");
    let cases: [(&[u8], &str); 3] = [
        (b"[server\nthis is not toml", "line 1, column 8"),
        (semantically_invalid.as_bytes(), "server.port"),
        (
            b"[server]\nport = 9000\nhost = \"\xff\xfe\"\n[broken",
            "config",
        ),
    ];
    for (broken, issue_path) in cases {
        let f = fixture(BASE);
        let mut events = f.store.events();
        std::fs::write(&f.path, broken).unwrap();
        let err = f
            .store
            .update(|c| {
                c.server.port = 9100;
                Ok(())
            })
            .await
            .unwrap_err();
        match &err {
            ConfigStoreError::DiskInvalid(issues) => {
                assert_eq!(issues[0].path, issue_path, "{err}");
            }
            other => panic!("unexpected {other:?}"),
        }
        let message = err.to_string();
        assert!(
            message.starts_with(
                "the configuration file on disk is not valid and was not overwritten; fix or \
                 restore the file, or replace it through the raw editor: "
            ),
            "{message}"
        );
        // Neither the file nor the live configuration changed, and nothing
        // was announced.
        assert_eq!(std::fs::read(&f.path).unwrap(), broken);
        assert_eq!(f.store.current().server.port, 9000);
        assert!(events.try_recv().is_err());

        // An edit that changes nothing writes nothing: not refused.
        f.store.update(|_| Ok(())).await.unwrap();
        // An edit that is itself invalid is told so first.
        let err = f
            .store
            .update(|c| {
                c.server.port = 0;
                Ok(())
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ConfigStoreError::Invalid(_)), "{err}");
        assert_eq!(std::fs::read(&f.path).unwrap(), broken);

        // Replacing the whole file is explicit, and the way out.
        let replaced = "# replaced\n[server]\nport = 9200\n";
        f.store.replace_text(replaced).await.unwrap();
        assert_eq!(std::fs::read_to_string(&f.path).unwrap(), replaced);
        assert_eq!(f.store.current().server.port, 9200);
        // …after which edits go through again.
        f.store
            .update(|c| {
                c.server.port = 9300;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&f.path).unwrap(),
            "# replaced\n[server]\nport = 9300\n"
        );
    }
}

/// An empty file holds nothing to lose: the edit goes into the latest
/// applied text, whoever applied it, comments and all.
#[tokio::test]
async fn update_over_an_empty_file_restores_the_last_good_text() {
    let f = fixture(BASE);
    f.store
        .replace_text("# second version\n[server]\nport = 9200 # custom\n")
        .await
        .unwrap();
    std::fs::write(&f.path, "").unwrap();
    let applied = f
        .store
        .update(|c| {
            c.server.port = 9300;
            Ok(())
        })
        .await
        .unwrap();
    let text = std::fs::read_to_string(&f.path).unwrap();
    assert_eq!(text, "# second version\n[server]\nport = 9300 # custom\n");
    assert!(!text.contains(REWRITE_HEADER));
    assert_eq!(*ConfigStore::load(&f.path).unwrap().current(), *applied);
}

#[tokio::test]
async fn update_recreates_a_deleted_file() {
    let f = fixture(BASE);
    std::fs::remove_file(&f.path).unwrap();
    let applied = f
        .store
        .update(|c| {
            c.server.port = 9100;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(*ConfigStore::load(&f.path).unwrap().current(), *applied);
    // With the comments and layout it had.
    assert_eq!(
        std::fs::read_to_string(&f.path).unwrap(),
        BASE.replace("port = 9000", "port = 9100")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_updates_are_serialised_and_none_is_lost() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    let mut tasks = Vec::new();
    for i in 0..24 {
        let store = f.store.clone();
        tasks.push(tokio::spawn(async move {
            store
                .update(move |c| {
                    c.auth.keys.push(key(&format!("sy-{i:02}")));
                    Ok(())
                })
                .await
                .map(|config| config.auth.keys.len())
        }));
    }
    let mut sizes = Vec::new();
    for task in tasks {
        sizes.push(task.await.unwrap().unwrap());
    }
    // Each update saw the result of the previous one.
    sizes.sort_unstable();
    assert_eq!(sizes, (1..=24).collect::<Vec<_>>());

    let config = f.store.current();
    let mut keys: Vec<String> = config.auth.keys.iter().map(|k| k.key.clone()).collect();
    keys.sort();
    assert_eq!(
        keys,
        (0..24).map(|i| format!("sy-{i:02}")).collect::<Vec<_>>()
    );

    // The file agrees and kept its comments.
    let text = std::fs::read_to_string(&f.path).unwrap();
    assert!(text.starts_with("# Team gateway\n[server]\nhost = \"127.0.0.1\"   # loopback only\n"));
    assert!(text.contains("# The one provider\n[[providers]]\n"));
    assert_eq!(*ConfigStore::load(&f.path).unwrap().current(), *config);

    for _ in 0..24 {
        assert!(matches!(
            next_event(&mut events).await,
            ConfigEvent::Applied {
                source: Source::Admin,
                ..
            }
        ));
    }
    assert!(events.try_recv().is_err());
}

// ---------------------------------------------------------------------------
// Raw text
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replace_text_validates_then_writes_verbatim() {
    let f = fixture(BASE);
    let mut events = f.store.events();

    // Invalid: refused, nothing written.
    let err = f
        .store
        .replace_text("[server]\nport = 0\n")
        .await
        .unwrap_err();
    assert_eq!(err.issues()[0].path, "server.port");
    let err = f
        .store
        .replace_text("[server]\nport = \n")
        .await
        .unwrap_err();
    assert_eq!(err.issues()[0].path, "line 2, column 8");
    assert_eq!(std::fs::read_to_string(&f.path).unwrap(), BASE);
    assert_eq!(f.store.current().server.port, 9000);
    assert!(events.try_recv().is_err());

    // Valid: written exactly as given, odd formatting and all.
    let text = "# rewritten by hand\n\n[server]\n  port   =   9300   # odd spacing\r\n";
    let applied = f.store.replace_text(text).await.unwrap();
    assert_eq!(applied.server.port, 9300);
    assert!(applied.providers.is_empty());
    assert_eq!(std::fs::read(&f.path).unwrap(), text.as_bytes());
    assert_eq!(f.store.current().server.port, 9300);
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::Admin,
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// Restart-only settings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restart_required_lists_changed_listener_settings() {
    let f = fixture(BASE);
    assert!(f.store.restart_required().is_empty());

    // Live-reloadable settings do not count.
    f.store
        .update(|c| {
            c.auth.required = false;
            c.logging.level = "debug".into();
            Ok(())
        })
        .await
        .unwrap();
    assert!(f.store.restart_required().is_empty());

    f.store
        .update(|c| {
            c.server.port = 9001;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(f.store.restart_required(), vec!["server.port"]);

    f.store
        .update(|c| {
            c.server.host = "0.0.0.0".into();
            c.server.data_dir = "/var/lib/switchyard".into();
            c.server.tls = Some(TlsConfig {
                cert: "c.pem".into(),
                key: "k.pem".into(),
            });
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        f.store.restart_required(),
        vec![
            "server.host",
            "server.port",
            "server.tls",
            "server.data_dir"
        ]
    );

    // Back to the values the process started with: nothing to restart for.
    f.store
        .update(|c| {
            c.server = Config::default().server;
            c.server.port = 9000;
            Ok(())
        })
        .await
        .unwrap();
    assert!(f.store.restart_required().is_empty());
}

// ---------------------------------------------------------------------------
// Manual reload
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reload_from_disk_applies_or_reports() {
    let f = fixture(BASE);
    let mut events = f.store.events();

    std::fs::write(&f.path, BASE.replace("9000", "9400")).unwrap();
    let config = f.store.reload_from_disk().await.unwrap();
    assert_eq!(config.server.port, 9400);
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));

    // Unchanged content is applied again on request.
    f.store.reload_from_disk().await.unwrap();
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));

    std::fs::write(&f.path, "[server]\nport = 0\n").unwrap();
    let err = f.store.reload_from_disk().await.unwrap_err();
    assert_eq!(err.issues()[0].path, "server.port");
    assert_eq!(f.store.current().server.port, 9400);
    match next_event(&mut events).await {
        ConfigEvent::Rejected { source, issues, .. } => {
            assert_eq!(source, Source::File);
            assert_eq!(issues[0].path, "server.port");
        }
        other => panic!("unexpected {other:?}"),
    }

    std::fs::remove_file(&f.path).unwrap();
    let err = f.store.reload_from_disk().await.unwrap_err();
    assert!(matches!(err, ConfigStoreError::Io { .. }));
    assert_eq!(f.store.current().server.port, 9400);
}

// ---------------------------------------------------------------------------
// Watching
// ---------------------------------------------------------------------------

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn watcher_applies_an_external_edit() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    let mut updates = f.store.subscribe();
    f.store
        .spawn_watcher_with(&tokio::runtime::Handle::current(), fast());
    // Idempotent.
    f.store
        .spawn_watcher_with(&tokio::runtime::Handle::current(), fast());
    f.store.spawn_watcher(&tokio::runtime::Handle::current());

    write(&f.path, &BASE.replace("9000", "9600"));
    match next_event(&mut events).await {
        ConfigEvent::Applied { source, .. } => assert_eq!(source, Source::File),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(f.store.current().server.port, 9600);
    updates.changed().await.unwrap();
    assert_eq!(updates.borrow_and_update().server.port, 9600);
    assert_eq!(f.store.restart_required(), vec!["server.port"]);

    // Saving the same content again is not a change.
    write(&f.path, &BASE.replace("9000", "9600"));
    assert_quiet(&mut events).await;
}

#[tokio::test]
async fn watcher_with_default_timing_applies_an_external_edit() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    f.store.spawn_watcher(&tokio::runtime::Handle::current());
    write(&f.path, &BASE.replace("9000", "9650"));
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));
    assert_eq!(f.store.current().server.port, 9650);
}

#[tokio::test]
async fn watcher_rejects_an_invalid_file_and_keeps_the_old_config() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    f.store
        .spawn_watcher_with(&tokio::runtime::Handle::current(), fast());

    let secret = "sk-live-0123456789abcdefghijklmnop";
    write(
        &f.path,
        &format!(
            "[server]\nport = 0\n\n[[providers]]\nname = \"x\"\nkind = \"openai\"\napi_keys = [\"{secret}\"]\nprefix = \"a/b\"\n"
        ),
    );
    match next_event(&mut events).await {
        ConfigEvent::Rejected { source, issues, .. } => {
            assert_eq!(source, Source::File);
            let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
            assert_eq!(paths, vec!["server.port", "providers[0].prefix"]);
            for issue in &issues {
                assert!(!issue.message.contains(secret));
            }
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(f.store.current().server.port, 9000);
    assert_eq!(f.store.current().providers[0].name, "mock");

    // Unparsable text: one issue with the position.
    write(&f.path, "[server]\nport = \n");
    match next_event(&mut events).await {
        ConfigEvent::Rejected { issues, .. } => {
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].path, "line 2, column 8");
        }
        other => panic!("unexpected {other:?}"),
    }
    // The same broken content is reported once.
    write(&f.path, "[server]\nport = \n");
    assert_quiet(&mut events).await;
    assert_eq!(f.store.current().server.port, 9000);

    // Fixing the file brings it back.
    write(&f.path, &BASE.replace("9000", "9700"));
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));
    assert_eq!(f.store.current().server.port, 9700);
}

#[tokio::test]
async fn watcher_does_not_apply_a_truncated_file() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    f.store
        .spawn_watcher_with(&tokio::runtime::Handle::current(), fast());

    // Truncate-then-write, as a non-atomic save does: the empty moment must
    // not wipe the configuration. (The retry that covers a watcher catching
    // the empty moment is tested deterministically next to the store.)
    write(&f.path, "");
    write(&f.path, &BASE.replace("9000", "9750"));
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));
    assert_eq!(f.store.current().server.port, 9750);
    assert_eq!(f.store.current().providers.len(), 1);

    // A file that stays empty is reported, not applied.
    write(&f.path, "\n");
    match next_event(&mut events).await {
        ConfigEvent::Rejected { issues, .. } => {
            assert_eq!(issues[0].message, "the file is empty");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(f.store.current().providers.len(), 1);
}

#[tokio::test]
async fn watcher_survives_rename_replace() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    f.store
        .spawn_watcher_with(&tokio::runtime::Handle::current(), fast());

    // How editors and configuration management save: write a sibling, rename
    // it over the original.
    for port in ["9801", "9802"] {
        let staged = f.path.with_file_name("switchyard.toml.staged");
        write(&staged, &BASE.replace("9000", port));
        std::fs::rename(&staged, &f.path).unwrap();
        assert!(matches!(
            next_event(&mut events).await,
            ConfigEvent::Applied {
                source: Source::File,
                ..
            }
        ));
        assert_eq!(f.store.current().server.port.to_string(), port);
    }
}

#[tokio::test]
async fn watcher_survives_delete_then_recreate() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    f.store
        .spawn_watcher_with(&tokio::runtime::Handle::current(), fast());

    std::fs::remove_file(&f.path).unwrap();
    // A missing file changes nothing and reports nothing.
    assert_quiet(&mut events).await;
    assert_eq!(f.store.current().server.port, 9000);

    write(&f.path, &BASE.replace("9000", "9900"));
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));
    assert_eq!(f.store.current().server.port, 9900);

    // And once more, to show the watch is still attached.
    std::fs::remove_file(&f.path).unwrap();
    write(&f.path, &BASE.replace("9000", "9901"));
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));
    assert_eq!(f.store.current().server.port, 9901);
}

#[tokio::test]
async fn watcher_ignores_the_stores_own_writes() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    f.store
        .spawn_watcher_with(&tokio::runtime::Handle::current(), fast());

    f.store
        .update(|c| {
            c.server.port = 9100;
            Ok(())
        })
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::Admin,
            ..
        }
    ));
    // The write comes back as file events; they must not apply it again.
    assert_quiet(&mut events).await;

    f.store
        .replace_text(&BASE.replace("9000", "9200"))
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::Admin,
            ..
        }
    ));
    assert_quiet(&mut events).await;
    assert_eq!(f.store.current().server.port, 9200);

    // External edits are still seen afterwards.
    write(&f.path, &BASE.replace("9000", "9300"));
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));
    assert_eq!(f.store.current().server.port, 9300);
}

#[tokio::test]
async fn content_the_store_wrote_earlier_is_not_ignored_forever() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    f.store
        .spawn_watcher_with(&tokio::runtime::Handle::current(), fast());

    // The store writes B…
    f.store
        .update(|c| {
            c.server.port = 9100;
            Ok(())
        })
        .await
        .unwrap();
    let written = std::fs::read_to_string(&f.path).unwrap();
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::Admin,
            ..
        }
    ));
    // …someone changes the file to C…
    write(&f.path, &BASE.replace("9000", "9200"));
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));
    assert_eq!(f.store.current().server.port, 9200);
    // …and then back to exactly B, which must be applied again.
    write(&f.path, &written);
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));
    assert_eq!(f.store.current().server.port, 9100);
}

#[tokio::test]
async fn a_change_made_before_the_watcher_started_is_picked_up() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    write(&f.path, &BASE.replace("9000", "9450"));
    f.store
        .spawn_watcher_with(&tokio::runtime::Handle::current(), fast());
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));
    assert_eq!(f.store.current().server.port, 9450);
}

/// Regression: after the watcher refused the file, putting back the content
/// of the configuration in effect produced no event at all (the content was
/// already live), so a dashboard showed "file refused" for ever. The
/// recovery is announced, exactly once.
#[tokio::test]
async fn watcher_announces_a_refused_file_that_is_put_back() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    let mut updates = f.store.subscribe();
    updates.borrow_and_update();
    f.store
        .spawn_watcher_with(&tokio::runtime::Handle::current(), fast());

    write(&f.path, "[server]\nport = \n");
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Rejected {
            source: Source::File,
            ..
        }
    ));
    assert!(!updates.has_changed().unwrap());

    // Exactly the live content again.
    write(&f.path, BASE);
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));
    // Subscribers are told too, as for a manual reload.
    updates.changed().await.unwrap();
    assert_eq!(updates.borrow_and_update().server.port, 9000);
    // Once: further looks at the same file say nothing. (That a later
    // rejection and recovery are announced again, and that saving the live
    // content without a rejection before it is no news, is tested next to
    // the store and in `watcher_applies_an_external_edit`.)
    assert_quiet(&mut events).await;
    assert!(!updates.has_changed().unwrap());
}

/// The same recovery through a manual reload: one event for the reload, and
/// none from the watcher that looks at the file afterwards. And a rejection
/// that a manual reload reported is taken back by the watcher.
#[tokio::test]
async fn reload_from_disk_and_the_watcher_announce_one_recovery_between_them() {
    let applied_from_file = |event| {
        matches!(
            event,
            ConfigEvent::Applied {
                source: Source::File,
                ..
            }
        )
    };

    // Rejected by a manual reload (no watcher yet), put back, reloaded.
    let f = fixture(BASE);
    let mut events = f.store.events();
    write(&f.path, "[server]\nport = 0\n");
    assert!(f.store.reload_from_disk().await.is_err());
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Rejected { .. }
    ));
    write(&f.path, BASE);
    f.store.reload_from_disk().await.unwrap();
    assert!(applied_from_file(next_event(&mut events).await));
    // The watcher starts and looks at the file: nothing left to announce.
    f.store
        .spawn_watcher_with(&tokio::runtime::Handle::current(), fast());
    assert_quiet(&mut events).await;

    // Rejected by a manual reload, put back, noticed by the watcher (which
    // looks at the file when it starts).
    let f = fixture(BASE);
    let mut events = f.store.events();
    write(&f.path, "[server]\nport = 0\n");
    assert!(f.store.reload_from_disk().await.is_err());
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Rejected { .. }
    ));
    write(&f.path, BASE);
    f.store
        .spawn_watcher_with(&tokio::runtime::Handle::current(), fast());
    assert!(applied_from_file(next_event(&mut events).await));
    assert_quiet(&mut events).await;
    assert_eq!(f.store.current().server.port, 9000);
}

#[tokio::test]
async fn polling_alone_follows_the_file() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    f.store.spawn_watcher_with(
        &tokio::runtime::Handle::current(),
        WatchOptions {
            debounce: Duration::from_millis(40),
            poll_interval: Duration::from_millis(80),
            native: false,
        },
    );

    write(&f.path, &BASE.replace("9000", "9950"));
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::File,
            ..
        }
    ));
    assert_eq!(f.store.current().server.port, 9950);

    // Its own writes are not re-applied either.
    f.store
        .update(|c| {
            c.server.port = 9951;
            Ok(())
        })
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Applied {
            source: Source::Admin,
            ..
        }
    ));
    assert_quiet(&mut events).await;

    write(&f.path, "[server]\nport = 0\n");
    assert!(matches!(
        next_event(&mut events).await,
        ConfigEvent::Rejected {
            source: Source::File,
            ..
        }
    ));
    assert_eq!(f.store.current().server.port, 9951);
    // The refused file is the operator's to fix: an edit does not replace it.
    let err = f
        .store
        .update(|c| {
            c.server.port = 9952;
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ConfigStoreError::DiskInvalid(_)), "{err}");
    assert_eq!(
        std::fs::read_to_string(&f.path).unwrap(),
        "[server]\nport = 0\n"
    );
    assert_quiet(&mut events).await;
}

#[tokio::test]
async fn dropping_the_store_stops_the_watcher() {
    let f = fixture(BASE);
    let mut events = f.store.events();
    f.store
        .spawn_watcher_with(&tokio::runtime::Handle::current(), fast());
    let path = f.path.clone();
    let Fixture { _dir, store, .. } = f;
    drop(store);
    // The channel closes once the last handle (and with it the watch task's
    // reason to live) is gone.
    write(&path, &BASE.replace("9000", "9990"));
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                _ => continue,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the event channel never closed");
}
