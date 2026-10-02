//! Regression tests of the integration pass: what the dashboard's engineers
//! found when they built the pages against the running admin API.
//!
//! * statuses and messages: a broken file on disk (409), conflicts that
//!   name their field, shape errors an operator can act on, the stricter
//!   validation as 422 with paths relative to the request body;
//! * `"api_key": null` for a credential that has no key;
//! * views: tidy client keys, what `/status` counts, the state of disabled
//!   providers and of model discovery, the new pass-through fields;
//! * captured bodies and service-account files, seen through the API.

mod support;

use http::StatusCode;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{App, BASE, CLIENT_KEY, frame_of};
use switchyard_admin::AdminOptions;

const KEY: &str = "sk-upstream-key-0123456789abcdef";

fn issues(body: &Value) -> Vec<(String, String)> {
    body["error"]["issues"]
        .as_array()
        .map(|issues| {
            issues
                .iter()
                .map(|issue| {
                    (
                        issue["path"].as_str().unwrap_or_default().to_string(),
                        issue["message"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn paths(body: &Value) -> Vec<String> {
    issues(body).into_iter().map(|(path, _)| path).collect()
}

fn message(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// A broken file on disk
// ---------------------------------------------------------------------------

/// The operator is editing `switchyard.toml` by hand and the file does not
/// parse at the moment. A change made in the dashboard used to rewrite the
/// file from the last valid configuration, discarding the edit; now it is
/// refused with a 409 that says what to do and what is wrong with the file.
#[tokio::test]
async fn an_edit_over_a_broken_file_is_a_conflict_that_says_what_to_do() {
    let app = App::start().await;
    let broken = format!("{BASE}\n[routing]\nmax_attempts = \n");
    std::fs::write(&app.config_path, &broken).unwrap();

    for (what, (status, body)) in [
        (
            "PATCH /settings",
            app.patch("/settings", json!({"server": {"body_limit_mb": 33}}))
                .await,
        ),
        (
            "POST /keys",
            app.post("/keys", json!({"name": "second"})).await,
        ),
        (
            "POST /providers",
            app.post("/providers", json!({"name": "second", "kind": "mock"}))
                .await,
        ),
        (
            "PUT /aliases",
            app.put(
                "/aliases",
                json!([{"name": "fast", "targets": ["mock-echo"]}]),
            )
            .await,
        ),
    ] {
        assert_eq!(status, StatusCode::CONFLICT, "{what}: {body}");
        let said = message(&body);
        for part in [
            "the configuration file on disk is not valid",
            "was not saved",
            "fix or restore the file",
            "PUT /config/raw",
        ] {
            assert!(said.contains(part), "{what}: `{part}` is missing: {said}");
        }
        // The file's own issues, by their place in the file.
        let issues = issues(&body);
        assert_eq!(issues.len(), 1, "{what}: {body}");
        assert!(issues[0].0.starts_with("line "), "{what}: {body}");
        assert!(said.contains(&issues[0].0), "{what}: {said}");
    }
    // Nothing was written, nothing changed.
    assert_eq!(app.file(), broken);
    let unchanged = switchyard_core::Config::default().server.body_limit_mb;
    assert_eq!(app.gateway.config().server.body_limit_mb, unchanged);
    assert_eq!(app.get_ok("/keys").await.as_array().unwrap().len(), 1);

    // An edit that changes nothing has nothing to overwrite.
    let (status, body) = app
        .patch("/settings", json!({"server": {"body_limit_mb": unchanged}}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The way out the message names: replace the file as a whole.
    let (status, body) = app.put("/config/raw", json!({"text": BASE})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = app
        .patch("/settings", json!({"server": {"body_limit_mb": 33}}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(app.gateway.config().server.body_limit_mb, 33);
}

/// What is wrong with the *shape* of a file is said in an operator's words
/// wherever a file is judged: no type names of the program, and no value
/// from the broken line.
#[tokio::test]
async fn shape_errors_in_a_file_are_worded_for_an_operator() {
    let app = App::start().await;
    let secret = "sk-proj-0123456789abcdefghijklmnop";
    let models_as_names =
        format!("{BASE}\n[[providers]]\nname = \"a\"\nkind = \"openai\"\nmodels = [\"x\"]\n");
    let port_too_large = "[server]\nport = 99999\n".to_string();
    let key_not_in_a_list = format!(
        "{BASE}\n[[providers]]\nname = \"a\"\nkind = \"openai\"\napi_keys = \"{secret}\"\n"
    );
    let cases = [
        (
            &models_as_names,
            "invalid type: string \"•\", expected a table such as { id = \"model-name\" }",
        ),
        (
            &port_too_large,
            "invalid value: integer `99999`, expected a whole number from 0 to 65535",
        ),
        (
            &key_not_in_a_list,
            "invalid type: string \"sk-pro…mnop\", expected an array",
        ),
    ];
    let plain = |what: &str, text: &str| {
        for word in ["struct", "Config", "u16", "sequence", secret] {
            assert!(!text.contains(word), "{what}: `{word}` in {text}");
        }
    };

    for (text, expected) in cases {
        let (status, body) = app.put("/config/raw", json!({"text": text})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let found = issues(&body);
        assert_eq!(found.len(), 1, "{body}");
        assert!(found[0].0.starts_with("line "), "{body}");
        assert_eq!(found[0].1, expected);
        plain("PUT /config/raw", &body.to_string());

        let (status, body) = app.post("/config/validate", json!({"text": text})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], json!(false), "{body}");
        assert_eq!(body["issues"][0]["message"], json!(expected), "{body}");
        plain("POST /config/validate", &body.to_string());
    }

    // The same file saved by hand: the reload is refused and so is an edit
    // over it, both in the same words.
    std::fs::write(&app.config_path, &key_not_in_a_list).unwrap();
    let (status, body) = app.post("/reload", json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(issues(&body)[0].1, cases[2].1);
    plain("POST /reload", &body.to_string());
    let (status, body) = app
        .patch("/settings", json!({"server": {"body_limit_mb": 33}}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(issues(&body)[0].1, cases[2].1);
    assert!(message(&body).ends_with(cases[2].1), "{body}");
    plain("the 409 of an edit", &body.to_string());
}

// ---------------------------------------------------------------------------
// Conflicts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conflicts_name_the_field_that_is_taken() {
    let app = App::start().await;

    // Client keys: the name, then the key itself.
    let (status, body) = app.post("/keys", json!({"name": "Tester"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(message(&body), "a client key named `Tester` already exists");
    assert_eq!(
        body["error"]["issues"],
        json!([{"path": "name", "message": "is the name of another client key"}])
    );
    let (status, body) = app
        .post("/keys", json!({"name": "second", "key": CLIENT_KEY}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(message(&body), "this client key already exists");
    assert_eq!(
        body["error"]["issues"],
        json!([{"path": "key", "message": "is already a client key"}])
    );

    // Renaming a key to a name that is taken.
    let (status, created) = app.post("/keys", json!({"name": "second"})).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap();
    let (status, body) = app
        .patch(&format!("/keys/{id}"), json!({"name": "tester"}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(paths(&body), ["name"]);

    // Providers: created under a taken name, renamed to one.
    let (status, body) = app
        .post("/providers", json!({"name": "mock", "kind": "mock"}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(message(&body), "a provider named `mock` already exists");
    assert_eq!(
        body["error"]["issues"],
        json!([{"path": "name", "message": "is the name of another provider"}])
    );
    let (status, body) = app
        .post("/providers", json!({"name": "other", "kind": "mock"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = app
        .put("/providers/other", json!({"name": "mock", "kind": "mock"}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(paths(&body), ["name"]);
}

// ---------------------------------------------------------------------------
// Shape errors
// ---------------------------------------------------------------------------

/// Words that give away the deserialiser, or repeat what was sent.
const LEAKS: [&str; 12] = [
    "struct",
    "Config",
    " u32",
    " u64",
    " i32",
    " at line ",
    "invalid type",
    "invalid value",
    "expected one of",
    "sequence",
    "dead-model",
    "smoke-signals",
];

#[tokio::test]
async fn shape_errors_say_what_is_expected_in_plain_words() {
    let app = App::start().await;
    let (status, created) = app.post("/keys", json!({"name": "ci"})).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let key = format!("/keys/{}", created["id"].as_str().unwrap());

    let whole_number = "must be a whole number from 1 to 4294967295";
    let cases: Vec<(&str, &str, Value, &str, &str)> = vec![
        // The reviewers' findings, as they sent them.
        (
            "POST",
            "/providers",
            json!({"name": "p", "kind": "mock", "models": ["dead-model"]}),
            "models[0]",
            r#"expected an object such as {"id": "model-name"}, got a string"#,
        ),
        (
            "POST",
            "/keys",
            json!({"name": "y", "rate_limit_rpm": -5}),
            "rate_limit_rpm",
            whole_number,
        ),
        (
            "POST",
            "/keys",
            json!({"name": "y", "rate_limit_rpm": 5_000_000_000_u64}),
            "rate_limit_rpm",
            whole_number,
        ),
        (
            "POST",
            "/keys",
            json!({"name": "y", "rate_limit_rpm": 1.5}),
            "rate_limit_rpm",
            "expected a whole number from 1 to 4294967295, got a number",
        ),
        (
            "PATCH",
            key.as_str(),
            json!({"rate_limit_rpm": -1}),
            "rate_limit_rpm",
            whole_number,
        ),
        (
            "PATCH",
            key.as_str(),
            json!({"rate_limit_rpm": "many"}),
            "rate_limit_rpm",
            "expected a whole number from 1 to 4294967295, got a string",
        ),
        (
            "POST",
            "/keys",
            json!({"name": "y", "enabled": true}),
            "enabled",
            "unknown field `enabled`",
        ),
        ("POST", "/keys", json!({}), "name", "is required"),
        (
            "PATCH",
            "/settings",
            json!({"routing": {"cooldown": {"quota_secs": 1e20}}}),
            "routing.cooldown.quota_secs",
            "expected a whole number from 0 to 18446744073709551615, got a number",
        ),
        (
            "PATCH",
            "/settings",
            json!({"server": {"port": 70_000}}),
            "server.port",
            "must be a whole number from 0 to 65535",
        ),
        (
            "PATCH",
            "/settings",
            json!({"streaming": {"keepalive": 5}}),
            "streaming.keepalive",
            "unknown field `keepalive`",
        ),
        (
            "PUT",
            "/payload",
            json!({"default": [{"models": ["x"], "protocol": "smoke-signals", "set": {"a": 1}}]}),
            "default[0].protocol",
            "must be one of `openai-chat`, `openai-responses`, `anthropic`, `gemini`",
        ),
        (
            "PUT",
            "/pricing",
            json!([{"model": "x", "input": "cheap"}]),
            "[0].input",
            "expected a number, got a string",
        ),
        (
            "PUT",
            "/aliases",
            json!([{"name": "fast"}]),
            "[0].targets",
            "is required",
        ),
        (
            "PUT",
            "/providers/mock",
            json!({"name": "mock", "kind": "mock", "credentials": [{"weight": "heavy"}]}),
            "credentials[0].weight",
            "expected a whole number from 0 to 4294967295, got a string",
        ),
        (
            "POST",
            "/playground",
            json!({"protocol": "smoke-signals", "body": {}}),
            "protocol",
            "must be one of `openai-chat`, `openai-responses`, `anthropic`, `gemini`",
        ),
    ];
    for (method, path, body, field, said) in cases {
        let (status, answer) = app
            .send(method.parse().unwrap(), path, Some(body.clone()))
            .await;
        let what = format!("{method} {path} {body}");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {answer}");
        assert_eq!(
            issues(&answer),
            [(field.to_string(), said.to_string())],
            "{what}"
        );
        // The sentence alone says it too.
        let lead = if path == "/settings" {
            "the settings patch does not fit the configuration schema"
        } else {
            "invalid request"
        };
        assert_eq!(
            message(&answer),
            format!("{lead}: {field}: {said}"),
            "{what}"
        );
        for leak in LEAKS {
            assert!(
                !answer.to_string().contains(leak),
                "{what}: `{leak}` in {answer}"
            );
        }
    }

    // The body as a whole: no field to name, the same plain words.
    let (status, answer) = app.put("/aliases", json!({"name": "fast"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert_eq!(
        answer,
        json!({"error": {"message": "invalid request body: expected a list, got an object"}})
    );

    // Not JSON at all: where it stops being JSON, not in the parser's words.
    let (status, _, answer) = support::read_with_headers(
        app.request(http::Method::POST, "/keys")
            .body("{\"name\": \"x\","),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(
        message(&answer).starts_with("the request body is not valid JSON: line 1, column "),
        "{answer}"
    );
    assert!(!message(&answer).contains(" at line "), "{answer}");

    // Nothing of all this changed anything.
    assert_eq!(app.get_ok("/providers").await.as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// The stricter validation, by the request's own fields
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_keys_are_refused_with_paths_into_the_request() {
    let app = App::start().await;
    let file = app.file();

    // A limit of 0 would refuse every request; "no limit" is written by
    // leaving the field out.
    let (status, body) = app
        .post("/keys", json!({"name": "zero", "rate_limit_rpm": 0}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        issues(&body),
        [(
            "rate_limit_rpm".to_string(),
            "must be at least 1; leave it out for no limit".to_string()
        )]
    );
    assert_eq!(
        message(&body),
        "the configuration is not valid: rate_limit_rpm: must be at least 1; leave it out for \
         no limit"
    );

    // A reference that names no variable.
    for empty in ["env:", "${}", "env:  "] {
        let (status, body) = app.post("/keys", json!({"name": "x", "key": empty})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{empty}: {body}");
        assert_eq!(paths(&body), ["key"], "{empty}: {body}");
    }

    // The same through PATCH, on a key that is not the first in the file.
    let (status, created) = app.post("/keys", json!({"name": "second"})).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let file_with_key = app.file();
    assert_ne!(file_with_key, file);
    let (status, body) = app
        .patch(
            &format!("/keys/{}", created["id"].as_str().unwrap()),
            json!({"rate_limit_rpm": 0}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(paths(&body), ["rate_limit_rpm"]);
    assert_eq!(app.file(), file_with_key, "a refused edit writes nothing");
}

#[tokio::test]
async fn invalid_providers_are_refused_with_paths_into_the_request() {
    let app = App::start().await;
    let file = app.file();
    let entry = json!({
        "name": "upstream",
        "kind": "openai-compat",
        "base_url": "http://127.0.0.1:9/v1",
        "discover": false,
        "headers": {"X Team": "platform", "X-Empty": "", "X-Fine": "yes"},
        "api_keys": ["env:", KEY],
        "credentials": [{"api_key": "${}"}, {"api_key": KEY}],
        "models": [
            {"id": "fine"},
            {"id": "upside-down", "thinking": {"min": 5000, "max": 100}},
        ],
    });
    let expected = [
        "headers.X Team",
        "headers.X-Empty",
        "api_keys[0]",
        "credentials[0].api_key",
        "credentials[1].api_key",
        "models[1].thinking",
    ];

    let (status, body) = app.post("/providers", entry.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(paths(&body), expected, "{body}");
    assert!(
        message(&body).starts_with("the configuration is not valid: headers.X Team: "),
        "{body}"
    );
    assert!(!body.to_string().contains("providers["), "{body}");

    // The same entry over an existing provider: the same paths, whatever
    // the provider's place in the file.
    let (status, body) = app
        .post(
            "/providers",
            json!({"name": "upstream", "kind": "openai-compat", "base_url": "http://127.0.0.1:9/v1", "discover": false}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let file_with_provider = app.file();
    assert_ne!(file_with_provider, file);
    let (status, body) = app.put("/providers/upstream", entry).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(paths(&body), expected, "{body}");
    assert_eq!(app.file(), file_with_provider);
}

#[tokio::test]
async fn invalid_aliases_and_settings_are_refused_too() {
    let app = App::start().await;
    let file = app.file();

    let (status, body) = app
        .put(
            "/aliases",
            json!([
                {"name": "Has Space", "targets": ["mock-echo"]},
                {"name": "padded ", "targets": ["mock-echo"]},
                {"name": "mock-echo(high)", "targets": ["mock-lorem"]},
                {"name": "fine", "targets": ["mock-echo", " mock-lorem ", ""]},
                {"name": "loop", "targets": ["mock-echo", "LOOP"]},
                {"name": "none", "targets": []},
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        paths(&body),
        [
            "[0].name",
            "[1].name",
            "[2].name",
            "[3].targets[1]",
            "[3].targets[2]",
            "[4].targets[1]",
            "[5].targets",
        ],
        "{body}"
    );

    // Settings a gateway could not start with, or that contradict each
    // other. The settings patch is the configuration itself: its paths are
    // the same either way.
    for (patch, path) in [
        (json!({"server": {"host": "not a host"}}), "server.host"),
        (json!({"server": {"data_dir": ""}}), "server.data_dir"),
        (
            json!({"routing": {"cooldown": {"rate_limit_base_secs": 100, "rate_limit_max_secs": 10}}}),
            "routing.cooldown.rate_limit_max_secs",
        ),
        (json!({"admin": {"secret": "env:"}}), "admin.secret"),
    ] {
        let (status, body) = app.patch("/settings", patch.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{patch}: {body}");
        assert_eq!(paths(&body), [path], "{patch}: {body}");
    }

    let (status, body) = app
        .put(
            "/pricing",
            json!([{"model": "a", "input": 1, "output": 2, "cache_read": -0.5}]),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(paths(&body), ["[0]"]);

    assert_eq!(app.file(), file);
}

// ---------------------------------------------------------------------------
// A credential without a key
// ---------------------------------------------------------------------------

/// The reviewers' scenario: stored `[{api_key: K, label: "old"}, {label:
/// "keyless via proxy", proxy}]` is replaced by `[{label: "local", proxy}]`.
/// With an empty key the new credential was handed K (the key at its
/// position); `"api_key": null` says it has none, in one request.
#[tokio::test]
async fn a_null_api_key_removes_the_key_in_one_request() {
    let app = App::start().await;
    let stored = json!({
        "name": "local",
        "kind": "openai-compat",
        "base_url": "http://127.0.0.1:9/v1",
        "discover": false,
        "models": [{"id": "local-model"}],
        "credentials": [
            {"api_key": KEY, "label": "old"},
            {"label": "keyless via proxy", "proxy": "http://127.0.0.1:3128"},
        ],
    });
    let (status, body) = app.post("/providers", stored.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(app.file().contains(KEY));

    let replacement = |api_key: Value| {
        let mut entry = stored.clone();
        entry["credentials"] =
            json!([{"label": "local", "api_key": api_key, "proxy": "http://127.0.0.1:3128"}]);
        entry
    };

    // What the dashboard had to send before: an empty key keeps the stored
    // one — still true, and still what an untouched field means.
    let (status, body) = app.put("/providers/local", replacement(json!(""))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["credentials"][0]["masked_key"], "sk-ups…cdef",
        "{body}"
    );
    assert!(app.file().contains(KEY));

    // `null`: no key anywhere.
    let (status, body) = app.put("/providers/local", replacement(Value::Null)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["credentials"].as_array().unwrap().len(), 1);
    let credential = &body["credentials"][0];
    assert_eq!(credential["label"], "local");
    assert_eq!(credential["masked_key"], "");
    assert_eq!(credential["proxy"], "http://127.0.0.1:3128");
    assert_eq!(credential["status"], "ready");
    assert_eq!(
        body["config"]["credentials"],
        json!([{"label": "local", "proxy": "http://127.0.0.1:3128"}])
    );
    let config = app.gateway.config();
    let provider = config.provider("local").unwrap();
    assert_eq!(provider.credentials.len(), 1);
    assert_eq!(provider.credentials[0].api_key, "");
    let file = app.file();
    assert!(!file.contains(KEY), "{file}");
    assert!(!file.contains("no key"), "{file}");

    // A new provider may say it too.
    let (status, body) = app
        .post(
            "/providers",
            json!({
                "name": "fresh",
                "kind": "openai-compat",
                "base_url": "http://127.0.0.1:9/v1",
                "discover": false,
                "credentials": [{"label": "none", "api_key": null}],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["credentials"][0]["masked_key"], "");

    // Where a key is required, a credential without one is still refused.
    let (status, body) = app
        .post(
            "/providers",
            json!({"name": "strict", "kind": "openai", "credentials": [{"api_key": null}]}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(paths(&body), ["credentials[0]"]);
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// A file written by hand may pad a name and repeat or blank a pattern; the
/// list shows what the gateway makes of it, as `POST` and `PATCH` store it.
#[tokio::test]
async fn the_key_list_shows_hand_edited_values_tidied() {
    let config = BASE.replace(
        "name = \"tester\"",
        "name = \"  padded name  \"\nmodels = [\"\", \" mock-* \", \"mock-*\", \"gpt-*\", \"  \"]",
    );
    let app = App::start_config(&config).await;
    let keys = app.get_ok("/keys").await;
    assert_eq!(keys[0]["name"], "padded name");
    assert_eq!(keys[0]["models"], json!(["mock-*", "gpt-*"]));

    // What the endpoints store is the same form.
    let (status, created) = app
        .post(
            "/keys",
            json!({"name": " ci ", "models": ["a-*", " a-* ", "", "b"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let keys = app.get_ok("/keys").await;
    assert_eq!(keys[1]["name"], "ci");
    assert_eq!(keys[1]["models"], json!(["a-*", "b"]));
    assert_eq!(app.gateway.config().auth.keys[1].models, ["a-*", "b"]);
}

/// [`BASE`] plus a provider that is switched off, one with an unusable
/// credential, an alias that leads nowhere and one that hides its target.
fn mixed() -> String {
    format!(
        r#"{BASE}
[[providers]]
name = "parked"
kind = "openai-compat"
enabled = false
base_url = "http://127.0.0.1:9/v1"
discover = false
api_keys = ["{KEY}", "env:SWITCHYARD_ADMIN_TEST_VARIABLE_THAT_IS_NOT_SET"]
[[providers.models]]
id = "parked-model"

[[providers]]
name = "upstream"
kind = "openai-compat"
base_url = "http://127.0.0.1:9/v1"
discover = false
api_keys = ["{KEY}", "env:SWITCHYARD_ADMIN_TEST_VARIABLE_THAT_IS_NOT_SET"]
[[providers.models]]
id = "up-model"

[[aliases]]
name = "nowhere"
targets = ["no-such-model"]

[[aliases]]
name = "smart"
targets = ["mock-think(high)", "up-model"]
hide_targets = true
"#
    )
}

#[tokio::test]
async fn status_counts_what_is_in_service_and_says_whether_tls_is_on() {
    let app = App::start_config(&mixed()).await;
    let status = app.get_ok("/status").await;
    assert_eq!(status["tls"], false);
    assert_eq!(status["listen"], app.addr.to_string());
    // mock: 1 ready. upstream: 1 ready, 1 unusable. parked: not counted.
    assert_eq!(status["counts"]["providers"], 3);
    assert_eq!(status["counts"]["credentials"], 3);
    assert_eq!(status["counts"]["credentials_ready"], 2);

    // The model count is the model table without its ignored entries —
    // hidden names included: they are routable.
    let models = app.get_ok("/models").await;
    let entries = models.as_array().unwrap();
    let ignored: Vec<&str> = entries
        .iter()
        .filter(|entry| entry["ignored"] == true)
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(ignored, ["nowhere"]);
    assert!(entries.iter().any(|entry| entry["hidden"] == true));
    assert_eq!(
        status["counts"]["models"],
        entries.len() - ignored.len(),
        "{status}"
    );

    // A listener with TLS says so.
    let tls = App::start_with(
        BASE,
        AdminOptions {
            tls: true,
            ..AdminOptions::default()
        },
    )
    .await;
    assert_eq!(tls.get_ok("/status").await["tls"], true);
}

#[tokio::test]
async fn providers_show_what_switched_a_credential_off_and_how_discovery_stands() {
    let app = App::start_config(&mixed()).await;
    let list = app.get_ok("/providers").await;

    // A provider that is switched off: its credentials are not `ready`,
    // whatever state they would be in otherwise.
    let parked = &list[1];
    assert_eq!(parked["name"], "parked");
    for credential in parked["credentials"].as_array().unwrap() {
        assert_eq!(credential["status"], "disabled", "{credential}");
        assert_eq!(credential["disabled_by"], "provider", "{credential}");
        // The credential's own switch is not what did it.
        assert_eq!(credential["disabled"], false, "{credential}");
    }
    // A provider in service: `disabled_by` is there, and empty.
    let upstream = &list[2];
    assert_eq!(upstream["credentials"][0]["status"], "ready");
    assert_eq!(upstream["credentials"][0]["disabled_by"], Value::Null);
    assert_eq!(upstream["credentials"][1]["status"], "unusable");

    // Discovery: every provider says where it stands, with every field.
    let off = json!({"state": "off", "at": null, "error": null, "models": 0});
    for provider in list.as_array().unwrap() {
        assert_eq!(provider["discovery"], off, "{}", provider["name"]);
    }

    // A credential switched off by itself says so too.
    let id = upstream["credentials"][0]["id"].as_str().unwrap();
    let (status, body) = app
        .post(&format!("/credentials/{id}/disable"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let switched = body["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|credential| credential["id"] == id)
        .unwrap();
    assert_eq!(switched["status"], "disabled");
    assert_eq!(switched["disabled_by"], "credential");

    // A provider that wants discovery and cannot have it: the state says
    // that it failed and why, instead of an empty model list that could
    // mean anything.
    let upstream = refusing_upstream().await;
    let (status, body) = app
        .post(
            "/providers",
            json!({"name": "refused", "kind": "openai-compat", "base_url": format!("http://{upstream}/v1"), "api_keys": [KEY]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["discovery"]["state"], "pending", "{body}");
    assert!(body["discovery"]["at"].is_i64(), "{body}");
    let failed = support::eventually(|| {
        let states = app.gateway.discovery_states();
        let state = states.get("refused")?;
        (serde_json::to_value(state).ok()?["state"] == "failed").then_some(())
    });
    failed.await;
    let view = app.get_ok("/providers/refused").await;
    let discovery = &view["discovery"];
    assert_eq!(discovery["state"], "failed", "{view}");
    assert!(discovery["at"].is_i64(), "{view}");
    let error = discovery["error"].as_str().unwrap_or_default();
    assert!(error.contains("no such organisation"), "{view}");
    assert!(!error.contains(KEY), "the upstream quoted the key: {view}");
    assert_eq!(discovery["models"], 0);
    assert_eq!(view["models"], json!([]));

    // An edit of something else does not ask the upstream again: the state
    // stays as it is, time included.
    let (status, body) = app
        .put(
            "/aliases",
            json!([{"name": "fast", "targets": ["mock-echo"]}]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        app.get_ok("/providers/refused").await["discovery"],
        *discovery
    );
}

/// An OpenAI-compatible upstream that refuses to list its models, quoting
/// the key it was asked with.
async fn refusing_upstream() -> std::net::SocketAddr {
    use axum::response::IntoResponse;
    let app = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(|| async {
            (
                StatusCode::FORBIDDEN,
                axum::Json(json!({"error": {
                    "message": format!("no such organisation for key {KEY}"),
                    "type": "invalid_request_error",
                }})),
            )
                .into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a listening socket");
    let addr = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

#[tokio::test]
async fn the_model_table_says_which_target_and_tier_a_route_belongs_to() {
    let app = App::start_config(&mixed()).await;
    let models = app.get_ok("/models").await;
    let entry = |name: &str| -> Value {
        models
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no model `{name}` in {models}"))
    };

    let smart = entry("smart");
    assert_eq!(smart["ignored"], false);
    assert_eq!(
        smart["routes"],
        json!([
            {
                "provider": "mock",
                "upstream_model": "mock-think",
                "target": "mock-think(high)",
                "priority": 0,
                "credentials_total": 1,
                "credentials_available": 1,
            },
            {
                "provider": "upstream",
                "upstream_model": "up-model",
                "target": "up-model",
                "priority": 0,
                "credentials_total": 2,
                "credentials_available": 1,
            },
        ])
    );
    let nowhere = entry("nowhere");
    assert_eq!(nowhere["ignored"], true);
    assert_eq!(nowhere["routes"], json!([]));
    // A model's own routes carry the tier and no target.
    let think = entry("mock-think");
    assert_eq!(think["hidden"], true);
    assert_eq!(think["ignored"], false);
    assert_eq!(think["routes"][0]["priority"], 0);
    assert!(think["routes"][0].get("target").is_none(), "{think}");
}

#[tokio::test]
async fn request_and_log_pages_carry_what_a_client_needs_to_join_them_with_live_events() {
    let app = App::start().await;
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    let started_at = app.get_ok("/status").await["started_at"].clone();
    assert!(started_at.is_i64());

    // The request list says how many requests it can hold.
    let page = app.get_ok("/requests?limit=1").await;
    assert_eq!(page["capacity"], 2000, "{page}");
    assert_eq!(page["total"], 1);

    // The log: the newest sequence number whatever the filter, a target
    // filter of its own, and the process the numbers belong to.
    let logs = app.gateway.telemetry().logs();
    logs.clear();
    for (target, message) in [
        ("switchyard_gateway::pipeline", "request finished"),
        ("switchyard_server::app", "request"),
        ("switchyard_gateway::failover", "upstream attempt failed"),
    ] {
        logs.push(switchyard_telemetry::LogLine::new(
            switchyard_core::util::now_unix_ms(),
            "info",
            target,
            message,
        ));
    }
    let page = app.get_ok("/logs").await;
    assert_eq!(page["lines"].as_array().unwrap().len(), 3);
    let last_seq = page["last_seq"].as_u64().unwrap();
    assert_eq!(page["lines"][2]["seq"], last_seq);
    assert_eq!(page["started_at"], started_at);

    let filtered = app
        .get_ok("/logs?target=switchyard_gateway::&q=request")
        .await;
    let targets: Vec<&str> = filtered["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line["target"].as_str().unwrap())
        .collect();
    assert_eq!(targets, ["switchyard_gateway::pipeline"]);
    assert_eq!(filtered["last_seq"], last_seq, "whatever the filter");
    assert_eq!(filtered["started_at"], started_at);
    let none = app.get_ok("/logs?target=nope::").await;
    assert_eq!(none["lines"], json!([]));
    assert_eq!(none["last_seq"], last_seq);

    // The live stream names the process too, and says how many requests
    // its percentiles are made of.
    let (mut socket, hello) = app.live().await;
    assert_eq!(hello["data"]["started_at"], started_at, "{hello}");
    let (stats, _) = frame_of(&mut socket, "stats").await;
    assert_eq!(stats["data"]["latency_samples"], 1, "{stats}");
}

// ---------------------------------------------------------------------------
// Captured bodies and service-account files, through the API
// ---------------------------------------------------------------------------

/// `GET /requests/{id}` straight after the `request.finished` frame used to
/// answer `has_bodies: true` with `"bodies": null` for some 60 ms.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bodies_are_there_when_the_finished_frame_arrives() {
    let app = App::start_config(&format!("{BASE}\n[logging]\nrequest_log = \"all\"\n")).await;
    let (mut socket, _) = app.live().await;
    support::subscribe(&mut socket, &["request.finished"]).await;

    for round in 0..8 {
        let model = if round == 3 {
            "mock-error-500"
        } else {
            "mock-lorem"
        };
        app.chat(CLIENT_KEY, model).await;
        let (frame, _) = frame_of(&mut socket, "request.finished").await;
        let record = &frame["data"];
        assert_eq!(record["has_bodies"], true, "{frame}");
        let id = record["id"].as_str().unwrap();
        // At once: no waiting, no retry.
        let detail = app.get_ok(&format!("/requests/{id}")).await;
        assert_eq!(detail["record"]["has_bodies"], true, "{detail}");
        assert!(
            detail["bodies"]["client_request"].is_string(),
            "round {round}: has_bodies is true but the bodies are not there: {detail}"
        );
        assert!(detail["bodies"]["client_response"].is_string(), "{detail}");
    }
}

/// A vertex credential whose key file does not exist was created `ready`
/// and `usable`, with no warning anywhere.
#[tokio::test]
async fn a_vertex_credential_without_its_key_file_is_unusable_from_the_start() {
    let app = App::start().await;
    let (status, body) = app
        .post(
            "/providers",
            json!({
                "name": "vertex-eu",
                "kind": "vertex",
                "location": "europe-west4",
                "models": [{"id": "gemini-2.5-pro"}],
                "credentials": [{"service_account_file": "vertex-sa.json", "label": "eu"}],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let credential = &body["credentials"][0];
    assert_eq!(credential["status"], "unusable", "{body}");
    assert_eq!(credential["usable"], false);
    let reason = credential["unusable_reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("vertex-sa.json") && reason.contains("cannot read"),
        "{reason}"
    );
    // The directory the file is looked for in is nobody's business.
    assert!(
        !reason.contains(&app.dir.path().display().to_string()),
        "{reason}"
    );

    let status = app.get_ok("/status").await;
    let warnings = status["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{status}");
    assert!(
        warnings[0].as_str().is_some_and(
            |warning| warning.contains("vertex-eu") && warning.contains("vertex-sa.json")
        ),
        "{status}"
    );
    assert_eq!(status["counts"]["credentials"], 2);
    assert_eq!(status["counts"]["credentials_ready"], 1);

    // The model has a route, and nothing behind it that could serve it.
    let models = app.get_ok("/models").await;
    let model = models
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "gemini-2.5-pro")
        .unwrap();
    assert_eq!(model["routes"][0]["credentials_available"], 0, "{model}");
}
