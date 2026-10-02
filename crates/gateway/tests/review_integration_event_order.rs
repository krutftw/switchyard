//! Review of the integration pass: the order of `config.reloaded` events.
//!
//! The config store now announces that a refused file is valid again
//! (`ConfigEvent::Applied` plus a notification of the `watch` channel), so
//! that a dashboard showing "file refused" can stop. The gateway hears the
//! refusal on one channel (the store's broadcast of `ConfigEvent`s) and the
//! recovery on another (the `watch` channel of applied configurations), and
//! `follow_config` reads the two with an unbiased `tokio::select!`. When
//! both are waiting — the task did not get to run in between, or was still
//! inside `apply_config`, which now awaits file reads — the order in which
//! they are published is left to chance: `ok: true` can go out *before* the
//! `ok: false` it answers, and a dashboard then shows "file refused" for a
//! file that is fine, with no further event to correct it.
//!
//! The test keeps the runtime's only worker thread busy while the file is
//! refused and repaired, so that both notifications are waiting when the
//! gateway's task runs again.

// The shared support module re-exports more than this file uses.
#[allow(unused_imports)]
mod support;

use std::time::Duration;
use support::{FOUR_PROVIDERS, Harness};
use switchyard_telemetry::Event;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_recovery_is_announced_after_the_refusal_it_ends() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let good = std::fs::read_to_string(&harness.config_path).unwrap();
    let store = harness.gateway.config_store();

    let mut wrong_order = Vec::new();
    for round in 0..24 {
        let mut events = harness.gateway.telemetry().subscribe();

        // Occupy the worker thread (the gateway's follower task runs on
        // it) for the time the two reloads below take. This test's own
        // future runs on the test thread and is not affected.
        let (started, running) = tokio::sync::oneshot::channel::<()>();
        let busy = tokio::spawn(async move {
            let _ = started.send(());
            std::thread::sleep(Duration::from_millis(80));
        });
        running.await.unwrap();

        // A broken save is refused …
        std::fs::write(&harness.config_path, format!("{good}\n[server\n")).unwrap();
        assert!(store.reload_from_disk().await.is_err());
        // … and the file is put back.
        std::fs::write(&harness.config_path, &good).unwrap();
        store.reload_from_disk().await.unwrap();
        busy.await.unwrap();

        let mut seen: Vec<bool> = Vec::new();
        while seen.len() < 2 {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("two config.reloaded events are published")
                .expect("the event bus is open");
            if let Event::ConfigReloaded { ok, .. } = event {
                seen.push(ok);
            }
        }
        if seen != [false, true] {
            wrong_order.push((round, seen));
        }
    }
    assert!(
        wrong_order.is_empty(),
        "the refusal was announced after the recovery, so the last word is \"refused\" for a \
         file that is fine (round, [ok of the 1st event, ok of the 2nd]): {wrong_order:?}"
    );
}

/// The `ok` of the `config.reloaded` events published while `saves` runs
/// with the gateway's follower task kept from running, in order, up to the
/// first quiet moment after `at_least` of them.
async fn announced_while_busy<F>(harness: &Harness, at_least: usize, saves: F) -> Vec<bool>
where
    F: AsyncFnOnce(),
{
    let mut events = harness.gateway.telemetry().subscribe();
    let (started, running) = tokio::sync::oneshot::channel::<()>();
    let busy = tokio::spawn(async move {
        let _ = started.send(());
        std::thread::sleep(Duration::from_millis(80));
    });
    running.await.unwrap();
    saves().await;
    busy.await.unwrap();

    let mut seen: Vec<bool> = Vec::new();
    loop {
        let wait = if seen.len() < at_least {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(150)
        };
        match tokio::time::timeout(wait, events.recv()).await {
            Ok(Ok(Event::ConfigReloaded { ok, .. })) => seen.push(ok),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => panic!("the event bus is closed: {error}"),
            Err(_) if seen.len() >= at_least => return seen,
            Err(_) => panic!("only {seen:?} of {at_least} config.reloaded events were published"),
        }
    }
}

/// The other way round: a good save followed by a broken one. Looking at
/// refusals first would announce the refusal before the configuration it
/// came after, and the last word would be "applied" for a file that is
/// refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_refusal_is_announced_after_the_configuration_it_came_after() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let good = std::fs::read_to_string(&harness.config_path).unwrap();
    let store = harness.gateway.config_store();

    for round in 0..6 {
        let seen = announced_while_busy(&harness, 2, async || {
            std::fs::write(&harness.config_path, format!("{good}\n# save {round}\n")).unwrap();
            store.reload_from_disk().await.unwrap();
            std::fs::write(&harness.config_path, format!("{good}\n[server\n")).unwrap();
            assert!(store.reload_from_disk().await.is_err());
        })
        .await;
        assert_eq!(seen, [true, false], "round {round}");

        // Put right again for the next round.
        std::fs::write(&harness.config_path, &good).unwrap();
        harness.reload().await;
    }
}

/// Applied, refused and put right before the gateway looks at any of it:
/// the configuration waiting is applied on the first verdict, and the last
/// verdict — the file is fine again — still has to be the last word.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn the_last_word_is_that_of_the_last_save() {
    let harness = Harness::start(FOUR_PROVIDERS).await;
    let good = std::fs::read_to_string(&harness.config_path).unwrap();
    let store = harness.gateway.config_store();

    for round in 0..6 {
        let last = good.replace(
            "sy-test-key-0123456789",
            &format!("sy-test-key-round-{round}"),
        );
        assert_ne!(last, good, "the fixture has the client key to replace");
        let seen = announced_while_busy(&harness, 3, async || {
            std::fs::write(&harness.config_path, format!("{good}\n# save {round}\n")).unwrap();
            store.reload_from_disk().await.unwrap();
            std::fs::write(&harness.config_path, format!("{good}\n[server\n")).unwrap();
            assert!(store.reload_from_disk().await.is_err());
            std::fs::write(&harness.config_path, &last).unwrap();
            store.reload_from_disk().await.unwrap();
        })
        .await;
        assert_eq!(seen, [true, false, true], "round {round}");
        // What is in effect is the last save, not the first.
        assert_eq!(
            harness.gateway.scheduler().config().auth.keys[0].key,
            format!("sy-test-key-round-{round}"),
            "round {round}"
        );
    }
}
