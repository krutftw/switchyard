//! Regression tests for review finding ADM-R4 (fixed): `GET /keys`
//! attributed usage by key *name*, not by key.
//!
//! The 30-day totals used to be looked up in the usage store's per-key
//! breakdown, which is grouped by the name a key had when each request was
//! made. The list, however, identifies keys by `id` (`client_key_id` of the
//! key) and `PATCH /keys/{id}` lets the operator change the name. So:
//!
//! * renaming a key zeroes its `usage` (while `last_used_at`, which is
//!   looked up by id, still says it was used — the two halves of one object
//!   disagree);
//! * a key created under a name that was used before shows the *other*
//!   key's requests, tokens and cost although it has never been presented.
//!
//! The request records carry `client.key_id`, so the numbers are now
//! counted per key id from the records themselves (`key_usage`): the usage
//! files when usage is persisted, the requests in memory otherwise.

mod support;

use http::StatusCode;
use serde_json::{Value, json};
use support::{App, CLIENT_KEY};

async fn traffic(app: &App) {
    for _ in 0..3 {
        assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    }
    app.gateway.telemetry().flush().await.unwrap();
}

fn by_id<'a>(keys: &'a Value, id: &str) -> &'a Value {
    keys.as_array()
        .unwrap()
        .iter()
        .find(|key| key["id"] == id)
        .unwrap_or_else(|| panic!("no key {id} in {keys}"))
}

#[tokio::test]
async fn renaming_a_key_keeps_its_usage() {
    let app = App::start().await;
    traffic(&app).await;

    let keys = app.get_ok("/keys").await;
    let id = keys[0]["id"].as_str().unwrap().to_string();
    assert_eq!(keys[0]["usage"]["requests"], 3, "{keys}");

    let (status, body) = app
        .patch(&format!("/keys/{id}"), json!({"name": "laptop"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Same key, same id, same history.
    assert_eq!(body["id"], id.as_str());
    assert_eq!(
        body["usage"]["requests"], 3,
        "the key's usage vanished with its old name: {body}"
    );

    let keys = app.get_ok("/keys").await;
    let key = by_id(&keys, &id);
    assert_eq!(key["usage"]["requests"], 3, "{keys}");
    assert!(key["usage"]["tokens"].as_u64().unwrap() > 0, "{keys}");
}

#[tokio::test]
async fn a_new_key_does_not_inherit_the_usage_of_an_earlier_key_with_that_name() {
    let app = App::start().await;
    traffic(&app).await;

    let old_id = app.get_ok("/keys").await[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    // The old key gets another name; its former name is free again.
    let (status, body) = app
        .patch(&format!("/keys/{old_id}"), json!({"name": "retired"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, created) = app.post("/keys", json!({"name": "tester"})).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let new_id = created["id"].as_str().unwrap().to_string();
    assert_ne!(new_id, old_id);

    let keys = app.get_ok("/keys").await;
    let fresh = by_id(&keys, &new_id);
    // Never presented by anyone.
    assert_eq!(fresh["usage"]["last_used_at"], Value::Null, "{keys}");
    assert_eq!(
        fresh["usage"]["requests"], 0,
        "a key that was never used shows another key's requests: {keys}"
    );
    assert_eq!(fresh["usage"]["tokens"], 0, "{keys}");

    // And the old key still has what it did, under its new name.
    let old = by_id(&keys, &old_id);
    assert_eq!(old["name"], "retired");
    assert_eq!(old["usage"]["requests"], 3, "{keys}");
    assert!(old["usage"]["last_used_at"].is_i64(), "{keys}");

    // Each key's own traffic from here on is its own.
    let new_key = created["key"].as_str().unwrap();
    assert_eq!(app.chat(new_key, "mock-echo").await, 200);
    let keys = app.get_ok("/keys").await;
    assert_eq!(by_id(&keys, &new_id)["usage"]["requests"], 1, "{keys}");
    assert_eq!(by_id(&keys, &old_id)["usage"]["requests"], 3, "{keys}");
}

#[tokio::test]
async fn usage_is_counted_as_requests_come_in_without_waiting_for_the_writer() {
    // Nothing here flushes the usage files by hand: the list must not lag
    // behind the requests that have finished.
    let app = App::start().await;
    let id = app.get_ok("/keys").await[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut seen = Vec::new();
    for round in 1..=3u64 {
        assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
        // Fails upstream the first time and while the model rests after it.
        assert!(app.chat(CLIENT_KEY, "mock-error-500").await >= 400);
        let keys = app.get_ok("/keys").await;
        let usage = &by_id(&keys, &id)["usage"];
        assert_eq!(usage["requests"], 2 * round, "{keys}");
        assert_eq!(usage["errors"], round, "{keys}");
        seen.push(usage["last_used_at"].as_i64().unwrap());
    }
    assert!(seen.is_sorted(), "{seen:?}");

    // The numbers agree with the usage store's own for the same range.
    let summary = app.get_ok("/usage/summary?range=30d").await;
    assert_eq!(summary["totals"]["requests"], 6, "{summary}");
    let keys = app.get_ok("/keys").await;
    let usage = &by_id(&keys, &id)["usage"];
    let totals = &summary["totals"];
    assert_eq!(
        usage["tokens"].as_u64().unwrap(),
        [
            "input_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "output_tokens"
        ]
        .iter()
        .map(|field| totals[*field].as_u64().unwrap())
        .sum::<u64>(),
        "{keys} vs {summary}"
    );
    assert_eq!(usage["cost"], totals["cost"]);
}

#[tokio::test]
async fn clearing_the_statistics_clears_the_keys_usage() {
    let app = App::start().await;
    traffic(&app).await;
    let keys = app.get_ok("/keys").await;
    assert_eq!(keys[0]["usage"]["requests"], 3, "{keys}");

    let (status, body) = app.delete("/usage").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let keys = app.get_ok("/keys").await;
    assert_eq!(
        keys[0]["usage"],
        json!({"requests": 0, "errors": 0, "tokens": 0, "cost": 0.0, "last_used_at": null}),
        "{keys}"
    );

    // Counting starts again from nothing, in a usage file that is new.
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    let keys = app.get_ok("/keys").await;
    assert_eq!(keys[0]["usage"]["requests"], 1, "{keys}");
}

#[tokio::test]
async fn without_usage_files_the_requests_in_memory_are_counted_by_key() {
    let config = format!("{}\n[usage]\npersist = false\n", support::BASE);
    let app = App::start_config(&config).await;
    traffic(&app).await;
    assert!(app.gateway.telemetry().usage().persist_dir().is_none());

    let keys = app.get_ok("/keys").await;
    let id = keys[0]["id"].as_str().unwrap().to_string();
    assert_eq!(keys[0]["usage"]["requests"], 3, "{keys}");

    let (status, body) = app
        .patch(&format!("/keys/{id}"), json!({"name": "laptop"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["usage"]["requests"], 3, "{body}");
    let (status, created) = app.post("/keys", json!({"name": "tester"})).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let keys = app.get_ok("/keys").await;
    assert_eq!(by_id(&keys, &id)["usage"]["requests"], 3, "{keys}");
    assert_eq!(
        by_id(&keys, created["id"].as_str().unwrap())["usage"]["requests"],
        0,
        "{keys}"
    );
}
