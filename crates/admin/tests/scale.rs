//! Configuration edits with many providers: an edit of one provider must
//! not cost more the more providers there are around it.
//!
//! The dashboard's engineers measured `POST /providers` at about 3 s and
//! `DELETE /providers/{name}` at about 11 s with 230 to 300 providers on a
//! debug build. The bounds here are generous on purpose (a loaded CI machine
//! running a debug build); the measured times are printed with
//! `cargo test -p switchyard-admin --test scale -- --nocapture`.

mod support;

use http::StatusCode;
use serde_json::json;
use std::fmt::Write as _;
use std::time::{Duration, Instant};
use support::{App, BASE};

/// How many providers surround the one being edited.
const PROVIDERS: usize = 300;

/// The longest any single edit may take. The target is well under 500 ms on
/// a debug build; four times that keeps the test steady on a busy machine.
const BOUND: Duration = Duration::from_secs(2);

/// [`BASE`] plus `count` mock providers (`lab-000` …), each with a prefix of
/// its own, and a few aliases.
fn many_providers(count: usize) -> String {
    let mut text = String::from(BASE);
    for index in 0..count {
        write!(
            text,
            "\n[[providers]]\nname = \"lab-{index:03}\"\nkind = \"mock\"\nprefix = \"lab{index:03}\"\npriority = {}\n",
            index % 3
        )
        .expect("writing to a string");
    }
    for index in 0..10 {
        write!(
            text,
            "\n[[aliases]]\nname = \"alias-{index}\"\ntargets = [\"mock-echo\", \"lab{index:03}/mock-think(high)\"]\n"
        )
        .expect("writing to a string");
    }
    text
}

/// The id of a provider's first credential.
async fn body_id(app: &App, provider: &str) -> String {
    let view = app.get_ok(&format!("/providers/{provider}")).await;
    view["credentials"][0]["id"]
        .as_str()
        .expect("a credential id")
        .to_string()
}

#[tokio::test]
async fn edits_stay_fast_with_three_hundred_providers() {
    let app = App::start_config(&many_providers(PROVIDERS)).await;
    let mut timings: Vec<(&str, Duration)> = Vec::new();

    // Warm up: the first request pays for the connection.
    let list = app.get_ok("/providers").await;
    assert_eq!(list.as_array().map(Vec::len), Some(PROVIDERS + 1));

    let started = Instant::now();
    let (status, body) = app
        .post(
            "/providers",
            json!({"name": "newcomer", "kind": "mock", "prefix": "new"}),
        )
        .await;
    timings.push(("POST /providers", started.elapsed()));
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["credentials"][0]["status"], "ready", "{body}");
    assert_eq!(body["model_count"], 8, "{body}");

    let mut edited = body["config"].clone();
    edited["priority"] = json!(7);
    let started = Instant::now();
    let (status, body) = app.put("/providers/newcomer", edited).await;
    timings.push(("PUT /providers/{name}", started.elapsed()));
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["priority"], 7);
    assert_eq!(body["credentials"][0]["priority"], 7, "{body}");

    let started = Instant::now();
    let (status, body) = app
        .put(
            "/aliases",
            json!([
                {"name": "fast", "targets": ["mock-echo"]},
                {"name": "smart", "targets": ["new/mock-think(high)", "lab007/mock-lorem"]},
            ]),
        )
        .await;
    timings.push(("PUT /aliases", started.elapsed()));
    assert_eq!(status, StatusCode::OK, "{body}");

    // A credential switch: the edit has to find the credential by its id.
    let id = body_id(&app, "lab-200").await;
    let started = Instant::now();
    let (status, body) = app
        .post(&format!("/credentials/{id}/disable"), json!({}))
        .await;
    timings.push(("POST /credentials/{id}/disable", started.elapsed()));
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["credentials"][0]["status"], "disabled", "{body}");

    let started = Instant::now();
    let (status, body) = app
        .patch("/settings", json!({"routing": {"max_attempts": 4}}))
        .await;
    timings.push(("PATCH /settings", started.elapsed()));
    assert_eq!(status, StatusCode::OK, "{body}");

    // One from the middle: every entry after it moves up in the file.
    let started = Instant::now();
    let (status, body) = app.delete("/providers/lab-150").await;
    timings.push(("DELETE /providers/{name} (middle)", started.elapsed()));
    assert_eq!(status, StatusCode::OK, "{body}");

    let started = Instant::now();
    let (status, body) = app.delete("/providers/newcomer").await;
    timings.push(("DELETE /providers/{name} (last)", started.elapsed()));
    assert_eq!(status, StatusCode::OK, "{body}");

    let started = Instant::now();
    let list = app.get_ok("/providers").await;
    timings.push(("GET /providers", started.elapsed()));
    assert_eq!(list.as_array().map(Vec::len), Some(PROVIDERS));

    // Every answer described a gateway already running on the edit.
    let models = app.get_ok("/models").await;
    let smart = models
        .as_array()
        .and_then(|models| models.iter().find(|model| model["name"] == "smart"))
        .expect("the alias is in the model table");
    assert_eq!(smart["ignored"], false, "{smart}");

    for (what, took) in &timings {
        println!("{what}: {} ms", took.as_millis());
    }
    for (what, took) in &timings {
        assert!(
            *took < BOUND,
            "{what} took {} ms with {PROVIDERS} providers (bound: {} ms)",
            took.as_millis(),
            BOUND.as_millis()
        );
    }
}
