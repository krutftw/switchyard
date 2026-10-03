//! Regression tests for review finding ADM-R3 (fixed): a request whose
//! values cannot be written as TOML was answered `500 Internal Server
//! Error`.
//!
//! `AdminState::edit_config` maps a `ConfigStoreError::Edit` that did not
//! come from the handler's own closure to `ApiFailure::internal`. The store
//! also returns `Edit` when the edited configuration validates but cannot be
//! rendered: a JSON `null` anywhere inside a payload rule's `set` (TOML has
//! no null), or an integer beyond the signed 64-bit range. Both are plain
//! client mistakes — the request names the offending field — and nothing is
//! wrong with the gateway, yet the dashboard was told "internal error" with
//! no `issues`.
//!
//! Now: every edit is checked for such values before it reaches the store
//! (`state::writable`) and refused with a 422 whose `issues` name each
//! field by its place in the request body, as for every other value the
//! configuration cannot hold; nothing changes. Values TOML *can* hold are
//! written, however odd.

mod support;

use http::StatusCode;
use serde_json::{Value, json};
use support::App;

fn assert_client_error(what: &str, status: StatusCode, body: &Value, path_fragment: &str) {
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "{what}: expected 400 or 422, got {status}: {body}"
    );
    let issues = body["error"]["issues"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        issues.iter().any(|issue| issue["path"]
            .as_str()
            .unwrap_or_default()
            .contains(path_fragment)),
        "{what}: no issue names `{path_fragment}`: {body}"
    );
}

#[tokio::test]
async fn a_null_in_a_payload_rule_is_a_client_error() {
    let app = App::start().await;
    let file = app.file();

    // "Set this field to null" is a natural thing to try in an override
    // rule; TOML cannot express it.
    for (what, set) in [
        ("top-level null", json!({"response_format": null})),
        ("nested null", json!({"reasoning": {"effort": null}})),
        ("null in an array", json!({"stop": [null, "END"]})),
    ] {
        let (status, body) = app
            .put(
                "/payload",
                json!({"override": [{"models": ["*"], "set": set}]}),
            )
            .await;
        assert_client_error(what, status, &body, "set");
    }
    assert_eq!(app.file(), file, "a refused edit must not touch the file");
    assert_eq!(app.get_ok("/payload").await["override"], json!([]));
}

#[tokio::test]
async fn an_integer_toml_cannot_hold_is_a_client_error() {
    let app = App::start().await;
    let file = app.file();

    let (status, body) = app
        .patch("/settings", json!({"routing": {"max_wait_secs": u64::MAX}}))
        .await;
    assert_client_error("routing.max_wait_secs", status, &body, "max_wait_secs");

    let (status, body) = app
        .put(
            "/payload",
            json!({"override": [{"models": ["*"], "set": {"seed": u64::MAX}}]}),
        )
        .await;
    assert_client_error("payload seed", status, &body, "seed");

    assert_eq!(app.file(), file, "a refused edit must not touch the file");
}

#[tokio::test]
async fn the_refusal_is_a_422_that_names_every_field_by_its_place() {
    let app = App::start().await;
    let file = app.file();

    let (status, body) = app
        .put(
            "/payload",
            json!({
                "default": [{"models": ["*"], "set": {"seed": u64::MAX}}],
                "override": [
                    {"models": ["a"], "set": {"temperature": 0.5}},
                    {"models": ["b"], "set": {"stop": ["END", null], "reasoning": {"effort": null}}},
                ],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let paths: Vec<&str> = body["error"]["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|issue| issue["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        [
            "default[0].set.seed",
            "override[1].set.stop[1]",
            "override[1].set.reasoning.effort",
        ],
        "{body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("the configuration is not valid: default[0].set.seed: "),
        "{body}"
    );

    // The same check stands behind every mutation, not just these two
    // routes: a number field of a provider's model, for one.
    let (status, body) = app
        .post(
            "/providers",
            json!({
                "name": "wide",
                "kind": "mock",
                "models": [{"id": "m", "context_window": u64::MAX}],
            }),
        )
        .await;
    assert_client_error("context_window", status, &body, "models[0].context_window");
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // What validation finds is reported in the same answer.
    let (status, body) = app
        .patch(
            "/settings",
            json!({"server": {"body_limit_mb": 0}, "routing": {"max_wait_secs": u64::MAX}}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_client_error("body_limit_mb", status, &body, "server.body_limit_mb");
    assert_client_error("max_wait_secs", status, &body, "routing.max_wait_secs");

    assert_eq!(app.file(), file, "a refused edit must not touch the file");
    assert_eq!(app.get_ok("/providers").await.as_array().unwrap().len(), 1);
}

/// What API.md says about whole numbers no 64-bit integer holds, checked
/// against the running code: JSON has one number type, and a reader takes a
/// whole number beyond 18446744073709551615 for a floating-point number.
/// In a free-form value it is stored as one; a setting that takes a whole
/// number refuses it for its shape. Neither is the 422 of an integer a TOML
/// file cannot hold, which is for 9223372036854775808 to
/// 18446744073709551615.
#[tokio::test]
async fn a_whole_number_beyond_64_bits_is_a_floating_point_number() {
    let app = App::start().await;
    let raw = |method: http::Method, path: &str, body: &'static str| {
        support::read(
            app.request(method, path)
                .header("content-type", "application/json")
                .body(body),
        )
    };

    let (status, body) = raw(
        http::Method::PUT,
        "/payload",
        r#"{"override": [{"models": ["x"], "set": {"seed": 99999999999999999999}}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["override"][0]["set"]["seed"], json!(1e20));
    assert!(body["override"][0]["set"]["seed"].is_f64(), "{body}");
    // The file holds a float, and reads back as the same.
    let (status, body) = app.post("/reload", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        app.get_ok("/payload").await["override"][0]["set"]["seed"],
        json!(1e20)
    );

    // One step below, the number is still an integer — one TOML cannot
    // hold: the documented 422.
    let (status, body) = raw(
        http::Method::PUT,
        "/payload",
        r#"{"override": [{"models": ["x"], "set": {"seed": 18446744073709551615}}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["error"]["issues"][0]["path"],
        json!("override[0].set.seed")
    );

    // A setting that is a whole number does not take a floating-point one.
    let file = app.file();
    let (status, body) = raw(
        http::Method::PATCH,
        "/settings",
        r#"{"routing": {"max_wait_secs": 99999999999999999999}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"]["issues"][0],
        json!({
            "path": "routing.max_wait_secs",
            "message": "expected a whole number from 0 to 18446744073709551615, got a number"
        })
    );
    assert_eq!(app.file(), file, "a refused edit must not touch the file");
}

#[tokio::test]
async fn values_toml_can_hold_are_written_however_odd() {
    let app = App::start().await;
    // The largest integers, awkward keys, control characters, line breaks,
    // empty and nested containers: all of it has a TOML spelling, so none
    // of it may be refused — and it must come back as it was sent. (The
    // keys are rule paths, which may not be empty or hold spaces; the
    // values and the keys inside them may.)
    let set = json!({
        "largest": i64::MAX,
        "smallest": i64::MIN,
        "floats": [1.0, -0.0, 1e300, 5e-324],
        "nested": {"": "an empty key", "line\nbreak": "a key with a line break"},
        "a.b\\.c": "dots",
        "quote\"s'ticks'=#[x]": true,
        "line-break": "text\r\nwith \"quotes\", a \\ and a \u{0}",
        "ключ": "значение 🔐",
        "empty": {"list": [], "table": {}},
        "mixed": [1, "two", 3.5, false, [1, [2]], {"k": {"deep": ["v"]}}],
        "looks-like-a-date": "2026-10-02T12:00:00Z",
    });
    let rules = json!({"override": [{"models": ["*"], "set": set}]});
    let (status, body) = app.put("/payload", rules.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["override"][0]["set"], rules["override"][0]["set"]);

    // The file holds it too: a reload from disk changes nothing, and the
    // next edit is merged into a file that still reads.
    let (status, body) = app.post("/reload", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        app.get_ok("/payload").await["override"][0]["set"],
        rules["override"][0]["set"]
    );
    let (status, body) = app
        .patch("/settings", json!({"routing": {"max_wait_secs": i64::MAX}}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["config"]["routing"]["max_wait_secs"], i64::MAX);
    assert!(app.file().contains("9223372036854775807"));
    assert!(app.file().contains("# The dashboard signs in with this."));
}
