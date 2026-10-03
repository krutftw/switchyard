//! Regression tests of the last fix round before the first release, seen
//! through the running admin API:
//!
//! * the request list's `client_model` and `since` filters, and query
//!   parameters that are refused by name (repeated, malformed, a `before`
//!   that is no cursor);
//! * what `/status` counts as credentials in service;
//! * model patterns of client keys repeated in another case;
//! * `shadows_model` in the model table;
//! * keyless credentials that round-trip (`"api_key": null`) and the model
//!   count of a disabled provider;
//! * a playground body that is not JSON, reported at its place in the body.

mod support;

use http::StatusCode;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{App, BASE, CLIENT_KEY};
use switchyard_core::util::now_unix_ms;

fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_string())
        .collect()
}

fn issue(body: &Value) -> (String, String) {
    let issues = body["error"]["issues"].as_array().expect("issues");
    assert_eq!(issues.len(), 1, "{body}");
    (
        issues[0]["path"].as_str().unwrap().to_string(),
        issues[0]["message"].as_str().unwrap().to_string(),
    )
}

// ---------------------------------------------------------------------------
// C1: the request list's filters and refused query parameters
// ---------------------------------------------------------------------------

/// `client_model` selects exactly what a `by_model` row counts, and `since`
/// keeps what started from then on; both combine with the other filters
/// and with each other, and `total` follows them.
#[tokio::test]
async fn client_model_and_since_select_what_the_summary_rows_count() {
    let config = format!("{BASE}\n[[aliases]]\nname = \"fast\"\ntargets = [\"mock-echo\"]\n");
    let app = App::start_config(&config).await;
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    assert_eq!(app.chat(CLIENT_KEY, "fast").await, 200);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let middle = now_unix_ms();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert_eq!(app.chat(CLIENT_KEY, "Mock-Echo").await, 200);
    assert!(app.chat(CLIENT_KEY, "mock-error-500").await >= 500);
    assert_eq!(app.chat(CLIENT_KEY, "no-such-model").await, 404);
    app.gateway.telemetry().flush().await.unwrap();

    // `model=mock-echo` also finds the alias's request by its upstream
    // name; the `mock-echo` row of the summary does not count it.
    let by_model = app.get_ok("/requests?model=mock-echo").await;
    assert_eq!(by_model["total"], 3, "{by_model}");
    let summary = app.get_ok("/usage/summary?range=1h").await;
    let rows = summary["by_model"].as_array().unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        let name = row["name"].as_str().unwrap();
        let listed = app
            .get_ok(&format!("/requests?client_model={name}&limit=500"))
            .await;
        assert_eq!(listed["total"], row["requests"], "{name}: {listed}");
        assert_eq!(ids(&listed).len() as u64, row["requests"].as_u64().unwrap());
    }
    let echo = app.get_ok("/requests?client_model=MOCK-ECHO").await;
    assert_eq!(echo["total"], 2, "{echo}");
    let alias = app.get_ok("/requests?client_model=fast").await;
    assert_eq!(alias["total"], 1, "{alias}");
    assert_eq!(alias["items"][0]["client_model"], "fast");

    // `since`: only what started at or after it.
    let recent = app.get_ok(&format!("/requests?since={middle}")).await;
    assert_eq!(recent["total"], 3, "{recent}");
    for item in recent["items"].as_array().unwrap() {
        assert!(item["started_at"].as_i64().unwrap() >= middle, "{item}");
    }
    let both = app
        .get_ok(&format!("/requests?since={middle}&client_model=mock-echo"))
        .await;
    assert_eq!(both["total"], 1, "{both}");
    assert_eq!(both["items"][0]["requested_model"], "Mock-Echo");
    let failed = app
        .get_ok(&format!("/requests?since={middle}&status=error"))
        .await;
    assert_eq!(failed["total"], 2, "{failed}");
    let none = app
        .get_ok(&format!("/requests?since={}", now_unix_ms() + 60_000))
        .await;
    assert_eq!(none["total"], 0);
    // Paging within `since`.
    let first = app
        .get_ok(&format!("/requests?since={middle}&limit=2"))
        .await;
    assert_eq!(
        (first["total"].as_u64(), first["has_more"].as_bool()),
        (Some(3), Some(true))
    );
    let cursor = first["next_before"].as_str().unwrap();
    let second = app
        .get_ok(&format!("/requests?since={middle}&limit=2&before={cursor}"))
        .await;
    assert_eq!(ids(&second).len(), 1, "{second}");
}

/// Regression: an unparseable `before` answered an empty page with
/// `total: 0`, a `since` did not exist, and a repeated parameter was
/// refused with a message that did not say which.
#[tokio::test]
async fn refused_query_parameters_are_named() {
    let app = App::start().await;
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    app.gateway.telemetry().flush().await.unwrap();

    for (path, param) in [
        ("/requests?since=yesterday", "since"),
        ("/requests?since=1.5", "since"),
        ("/requests?before=garbage", "before"),
        ("/requests?limit=1&limit=2", "limit"),
        ("/requests?client_model=a&client_model=b", "client_model"),
        ("/usage/summary?range=1h&range=24h", "range"),
        ("/usage/timeseries?group_by=model&group_by=key", "group_by"),
        ("/logs?level=warn&level=info", "level"),
    ] {
        let (status, body) = app.get(path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        let (at, said) = issue(&body);
        assert_eq!(at, param, "{path}: {body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert_eq!(
            message,
            format!("invalid query parameter `{param}`: {said}"),
            "{path}"
        );
        assert!(!message.contains("garbage") && !message.contains("yesterday"));
    }

    // Omitted filters retain their defaults, and every cursor form works.
    let page = app.get_ok("/requests?unknown=1").await;
    assert_eq!(page["total"], 1);
    let id = page["items"][0]["id"].as_str().unwrap().to_string();
    for cursor in [
        id.clone(),
        now_unix_ms().to_string(),
        page["items"][0]["started_at"].to_string(),
    ] {
        let (status, body) = app.get(&format!("/requests?before={cursor}")).await;
        assert_eq!(status, StatusCode::OK, "{cursor}: {body}");
    }
    let (status, _) = app.get("/logs?before=abc&limit=x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// A1-3: credentials in service
// ---------------------------------------------------------------------------

/// Regression: `counts.credentials` left out the credentials of a disabled
/// provider but counted credentials switched off one by one.
#[tokio::test]
async fn status_counts_no_disabled_credential() {
    let config = format!(
        "{BASE}\n[[providers]]\nname = \"upstream\"\nkind = \"openai-compat\"\n\
         base_url = \"http://127.0.0.1:9/v1\"\ndiscover = false\n\
         api_keys = [\"sk-one-0123456789abcdef\", \"sk-two-0123456789abcdef\", \"sk-three-0123456789abcd\"]\n\
         [[providers.models]]\nid = \"up-model\"\n"
    );
    let app = App::start_config(&config).await;
    let counts = |status: &Value| {
        (
            status["counts"]["credentials"].as_u64().unwrap(),
            status["counts"]["credentials_ready"].as_u64().unwrap(),
        )
    };
    assert_eq!(counts(&app.get_ok("/status").await), (4, 4));

    let providers = app.get_ok("/providers/upstream").await;
    let first = providers["credentials"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let second = providers["credentials"][1]["id"]
        .as_str()
        .unwrap()
        .to_string();
    // Off in the configuration (`disabled_by: credential`).
    let (status, body) = app
        .post(&format!("/credentials/{first}/disable"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(counts(&app.get_ok("/status").await), (3, 3));
    // Off at runtime only (`disabled_by: runtime`).
    let _ = app.gateway.scheduler().set_runtime_disabled(&second, true);
    let view = app.get_ok("/providers/upstream").await;
    let runtime = view["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == second.as_str())
        .unwrap();
    assert_eq!(runtime["disabled_by"], "runtime", "{runtime}");
    assert_eq!(counts(&app.get_ok("/status").await), (2, 2));
    // And the whole provider off: none of its credentials.
    let mut entry = view["config"].clone();
    entry["enabled"] = json!(false);
    let (status, body) = app.put("/providers/upstream", entry).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(counts(&app.get_ok("/status").await), (1, 1));
}

// ---------------------------------------------------------------------------
// A1-4: model patterns repeated in another case
// ---------------------------------------------------------------------------

#[tokio::test]
async fn key_model_patterns_repeat_ignoring_case() {
    let app = App::start().await;
    let (status, body) = app
        .post(
            "/keys",
            json!({"name": "ci", "models": ["Mock-*", "mock-*", " MOCK-* ", "gpt-5", "GPT-5"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();
    let listed = app.get_ok("/keys").await;
    let key = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == id.as_str())
        .unwrap()
        .clone();
    assert_eq!(key["models"], json!(["Mock-*", "gpt-5"]));
    let (status, patched) = app
        .patch(
            &format!("/keys/{id}"),
            json!({"models": ["claude-*", "CLAUDE-*"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["models"], json!(["claude-*"]));
    assert!(!app.file().contains("CLAUDE-*"));
}

// ---------------------------------------------------------------------------
// C5 and C6
// ---------------------------------------------------------------------------

#[tokio::test]
async fn aliases_say_whether_they_hide_a_model() {
    let config = format!(
        "{BASE}\n[[aliases]]\nname = \"MOCK-ECHO\"\ntargets = [\"mock-think\"]\n\n\
         [[aliases]]\nname = \"fast\"\ntargets = [\"mock-echo\"]\n\n\
         [[aliases]]\nname = \"MOCK-ERROR-429\"\ntargets = [\"no-such-model\"]\n"
    );
    let app = App::start_config(&config).await;
    let models = app.get_ok("/models").await;
    let flag = |name: &str| {
        models
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == name && entry.get("alias_targets").is_some())
            .map(|entry| entry["shadows_model"].clone())
    };
    assert_eq!(flag("MOCK-ECHO"), Some(json!(true)));
    assert_eq!(flag("fast"), Some(json!(false)));
    // An ignored alias hides nothing, though a model has its name in
    // another case.
    assert_eq!(flag("MOCK-ERROR-429"), Some(json!(false)));
    for entry in models.as_array().unwrap() {
        assert!(entry["shadows_model"].is_boolean(), "{entry}");
    }
}

/// Regression: `config.credentials[]` left out `api_key` for a keyless
/// credential, which `PUT` reads as "keep the key stored at this place":
/// moving the keyless entry onto a keyed row handed it that key.
#[tokio::test]
async fn keyless_credentials_round_trip_and_disabled_providers_count_no_model() {
    const KEY: &str = "sk-upstream-key-0123456789abcdef";
    let app = App::start().await;
    let (status, body) = app
        .post(
            "/providers",
            json!({
                "name": "local",
                "kind": "openai-compat",
                "base_url": "http://127.0.0.1:9/v1",
                "discover": false,
                "models": [{"id": "local-model"}],
                "credentials": [
                    {"api_key": KEY, "label": "keyed"},
                    {"label": "keyless", "proxy": "http://127.0.0.1:3128"},
                ],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let config = &body["config"];
    assert_eq!(config["credentials"][1]["api_key"], Value::Null, "{config}");
    assert_eq!(config["credentials"][0]["api_key"], "sk-ups…cdef");

    // Sent back untouched: nothing changes.
    let before = app.file();
    let (status, same) = app.put("/providers/local", config.clone()).await;
    assert_eq!(status, StatusCode::OK, "{same}");
    assert_eq!(same["config"], *config);
    assert_eq!(app.file(), before);

    // Reordered: the keyless entry takes the keyed row's place and stays
    // keyless; the keyed one keeps its key by its mask.
    let mut moved = config.clone();
    let credentials = moved["credentials"].as_array_mut().unwrap();
    credentials.swap(0, 1);
    let (status, body) = app.put("/providers/local", moved).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["credentials"][0]["label"], "keyless");
    assert_eq!(body["credentials"][0]["masked_key"], "");
    assert_eq!(body["credentials"][1]["masked_key"], "sk-ups…cdef");
    assert_eq!(body["config"]["credentials"][0]["api_key"], Value::Null);
    assert_eq!(app.file().matches(KEY).count(), 1);

    // Relabelled: still keyless.
    let mut relabelled = body["config"].clone();
    relabelled["credentials"][0]["label"] = json!("renamed");
    let (status, body) = app.put("/providers/local", relabelled).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["credentials"][0]["masked_key"], "");

    // Disabled: no name routes to it, and its count says the same.
    assert_eq!(body["model_count"], 1, "{body}");
    let mut off = body["config"].clone();
    off["enabled"] = json!(false);
    let (status, body) = app.put("/providers/local", off).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["models"], json!([]));
    assert_eq!(body["model_count"], 0, "{body}");
    let listed = app.get_ok("/providers").await;
    let local = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "local")
        .unwrap();
    assert_eq!(local["model_count"], 0);
}

// ---------------------------------------------------------------------------
// A1-8: the playground's body position
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_playground_body_that_is_not_json_is_reported_in_the_body() {
    let app = App::start().await;
    // Spliced as text, the way the dashboard builds the envelope.
    let envelope = r#"{"protocol":"openai-chat","body":{"model": "mock-echo", "messages": [}}"#;
    let response = app
        .request(http::Method::POST, "/playground")
        .header("content-type", "application/json")
        .body(envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.unwrap();
    let (path, message) = issue(&body);
    assert_eq!(path, "body");
    assert!(
        message.starts_with("is not valid JSON: line 1, column 37: "),
        "{message}"
    );
}
