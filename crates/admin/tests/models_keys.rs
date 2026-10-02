//! The model table, the catalog, the whole-section editors (aliases,
//! payload rules, prices) and the client keys.

mod support;

use http::StatusCode;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{App, BASE, CLIENT_KEY};

fn issue_paths(body: &Value) -> Vec<&str> {
    body["error"]["issues"]
        .as_array()
        .map(|issues| {
            issues
                .iter()
                .map(|issue| issue["path"].as_str().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Models and catalog
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_model_table_lists_routes_and_availability() {
    let app = App::start().await;
    let (status, _) = app
        .put(
            "/aliases",
            json!([{"name": "fast", "targets": ["mock-echo(high)", "mock-lorem"]}]),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let models = app.get_ok("/models").await;
    let names: Vec<&str> = models
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "fast",
            "mock-echo",
            "mock-error-401",
            "mock-error-429",
            "mock-error-500",
            "mock-lorem",
            "mock-slow",
            "mock-think",
            "mock-tools",
        ]
    );
    let alias = &models[0];
    assert_eq!(
        alias["alias_targets"],
        json!(["mock-echo(high)", "mock-lorem"])
    );
    assert_eq!(alias["hidden"], false);
    assert_eq!(alias["ignored"], false);
    // One route per configured target, each saying which target it is.
    let targets: Vec<&str> = alias["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|route| route["target"].as_str().unwrap())
        .collect();
    assert_eq!(targets, ["mock-echo(high)", "mock-lorem"]);
    let echo = &models[1];
    assert_eq!(echo["info"]["id"], "mock-echo");
    assert!(echo.get("alias_targets").is_none());
    assert_eq!(
        echo["routes"],
        json!([{
            "provider": "mock",
            "upstream_model": "mock-echo",
            "priority": 0,
            "credentials_total": 1,
            "credentials_available": 1,
        }])
    );
    assert_eq!(echo["ignored"], false);
}

#[tokio::test]
async fn the_catalog_lists_known_models() {
    let app = App::start().await;
    let catalog = app.get_ok("/catalog").await;
    let entries = catalog.as_array().unwrap();
    assert!(entries.len() > 10, "{}", entries.len());
    for entry in entries {
        assert!(entry["id"].is_string(), "{entry}");
        assert!(entry["family"].is_string(), "{entry}");
        assert_eq!(entry["known"], true, "{entry}");
    }
}

// ---------------------------------------------------------------------------
// Aliases, payload, pricing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn aliases_payload_rules_and_prices_are_replaced_as_wholes() {
    let app = App::start().await;
    assert_eq!(app.get_ok("/aliases").await, json!([]));
    assert_eq!(
        app.get_ok("/payload").await,
        json!({"default": [], "override": [], "filter": []})
    );
    assert_eq!(app.get_ok("/pricing").await, json!([]));

    // Aliases.
    let (status, body) = app
        .put(
            "/aliases",
            json!([
                {"name": "fast", "targets": ["mock-echo"]},
                {"name": "smart", "targets": ["mock-think(high)", "mock-echo"], "hide_targets": true},
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let expected = json!([
        {"name": "fast", "targets": ["mock-echo"], "hide_targets": false},
        {"name": "smart", "targets": ["mock-think(high)", "mock-echo"], "hide_targets": true},
    ]);
    assert_eq!(body, expected);
    assert_eq!(app.get_ok("/aliases").await, expected);
    // In effect: the alias routes.
    assert_eq!(app.chat(CLIENT_KEY, "fast").await, 200);

    // Payload rules.
    let rules = json!({
        "default": [{"models": ["mock-*"], "set": {"temperature": 0.2}}],
        "override": [{"models": ["*"], "protocol": "openai-chat", "provider": "mock", "set": {"user": "gateway"}}],
        "filter": [{"models": ["mock-echo"], "remove": ["logit_bias", "metadata.trace"]}],
    });
    let (status, body) = app.put("/payload", rules).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let expected = json!({
        "default": [{"models": ["mock-*"], "protocol": null, "provider": "", "set": {"temperature": 0.2}, "remove": []}],
        "override": [{"models": ["*"], "protocol": "openai-chat", "provider": "mock", "set": {"user": "gateway"}, "remove": []}],
        "filter": [{"models": ["mock-echo"], "protocol": null, "provider": "", "set": {}, "remove": ["logit_bias", "metadata.trace"]}],
    });
    assert_eq!(body, expected);
    assert_eq!(app.get_ok("/payload").await, expected);

    // Prices.
    let (status, body) = app
        .put(
            "/pricing",
            json!([
                {"model": "mock-*", "input": 1.5, "output": 6, "cache_read": 0.15},
                {"model": "*", "input": 0, "output": 0},
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let expected = json!([
        {"model": "mock-*", "input": 1.5, "output": 6.0, "cache_read": 0.15, "cache_write": null},
        {"model": "*", "input": 0.0, "output": 0.0, "cache_read": null, "cache_write": null},
    ]);
    assert_eq!(body, expected);
    assert_eq!(app.get_ok("/pricing").await, expected);

    // All of it is in the file, next to the comments that were there.
    let file = app.file();
    assert!(file.contains("[[aliases]]"), "{file}");
    assert!(file.contains("[[pricing]]"), "{file}");
    assert!(
        file.contains("# The dashboard signs in with this."),
        "{file}"
    );
    let reread = switchyard_config_store::validate_text(&file).unwrap();
    assert_eq!(reread, *app.gateway.config());

    // An empty list clears a section.
    let (status, body) = app.put("/aliases", json!([])).await;
    assert_eq!((status, body), (StatusCode::OK, json!([])));
    assert!(!app.file().contains("[[aliases]]"));
}

#[tokio::test]
async fn invalid_sections_are_refused_with_issues() {
    let app = App::start().await;

    // 422: the right shape, a broken rule.
    let (status, body) = app
        .put(
            "/aliases",
            json!([
                {"name": "loop", "targets": ["loop"]},
                {"name": "", "targets": ["mock-echo"]},
                {"name": "empty", "targets": []},
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        issue_paths(&body),
        // By their place in the request body, the list of aliases.
        ["[0].targets[0]", "[1].name", "[2].targets"]
    );
    assert_eq!(
        body["error"]["message"],
        "the configuration is not valid: [0].targets[0]: an alias cannot target itself; \
         [1].name: must not be empty; [2].targets: needs at least one target"
    );
    let (status, body) = app
        .put(
            "/payload",
            json!({"default": [{"models": [], "set": {}}], "filter": [{"models": ["*"]}]}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        issue_paths(&body),
        ["default[0].models", "default[0].set", "filter[0].remove"]
    );
    let (status, body) = app
        .put("/pricing", json!([{"model": "", "input": -1, "output": 2}]))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(issue_paths(&body), ["[0].model", "[0]"]);

    // 400: not the shape at all.
    let (status, body) = app.put("/aliases", json!({"name": "fast"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = app.put("/aliases", json!([{"name": "fast"}])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("targets"),
        "{body}"
    );
    let (status, body) = app.put("/payload", json!({"defaults": []})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = app
        .put(
            "/payload",
            json!({"override": [{"models": ["*"], "protocol": "smoke-signals"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(issue_paths(&body), ["override[0].protocol"]);
    let (status, body) = app
        .put("/pricing", json!([{"model": "x", "input": "cheap"}]))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(issue_paths(&body), ["[0].input"]);

    assert_eq!(app.file(), BASE);
}

// ---------------------------------------------------------------------------
// Client keys
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_key_list_is_masked_and_carries_usage() {
    let app = App::start().await;
    let keys = app.get_ok("/keys").await;
    assert_eq!(
        keys,
        json!([{
            "id": switchyard_config_store::client_key_id(CLIENT_KEY),
            "name": "tester",
            "masked": "sy-tes…cdef",
            "is_reference": false,
            "resolved": true,
            "enabled": true,
            "models": [],
            "rate_limit_rpm": null,
            "usage": {"requests": 0, "errors": 0, "tokens": 0, "cost": 0.0, "last_used_at": null},
        }])
    );
}

#[tokio::test]
async fn a_key_lives_from_creation_to_deletion() {
    let app = App::start().await;

    // Created: generated, shown once.
    let (status, created) = app
        .post(
            "/keys",
            json!({"name": " laptop ", "models": ["mock-*", " "], "rate_limit_rpm": 600}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let key = created["key"].as_str().unwrap().to_string();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(key.len(), 43);
    assert!(key.starts_with("sy-"));
    assert!(key[3..].bytes().all(|c| c.is_ascii_alphanumeric()), "{key}");
    assert_eq!(id, switchyard_config_store::client_key_id(&key));
    assert_eq!(created["is_reference"], false);

    // Listed: masked.
    let keys = app.get_ok("/keys").await;
    assert_eq!(keys.as_array().unwrap().len(), 2);
    let listed = &keys[1];
    assert_eq!(listed["id"], id.as_str());
    assert_eq!(listed["name"], "laptop");
    assert_eq!(listed["models"], json!(["mock-*"]));
    assert_eq!(listed["rate_limit_rpm"], 600);
    assert_eq!(listed["enabled"], true);
    assert!(!keys.to_string().contains(&key));
    assert!(app.file().contains(&key));

    // Revealed: on request only.
    let (status, body) = app.post(&format!("/keys/{id}/reveal"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"key": key, "is_reference": false}));

    // Used: two requests, one failing. The key works as soon as the
    // creation call has returned.
    assert_eq!(app.chat(&key, "mock-echo").await, 200);
    assert!(app.chat(&key, "mock-error-500").await >= 500);
    let listed = app.get_ok("/keys").await[1].clone();
    assert_eq!(listed["usage"]["requests"], 2);
    assert_eq!(listed["usage"]["errors"], 1);
    assert!(listed["usage"]["tokens"].as_u64().unwrap() > 0, "{listed}");
    assert!(listed["usage"]["cost"].is_number());
    assert!(listed["usage"]["last_used_at"].as_i64().unwrap() > 1_600_000_000_000);
    // The other key's numbers are its own.
    assert_eq!(app.get_ok("/keys").await[0]["usage"]["requests"], 0);

    // Patched: only what is given changes.
    let (status, patched) = app
        .patch(
            &format!("/keys/{id}"),
            json!({"name": "workstation", "models": []}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["id"], id.as_str());
    assert_eq!(patched["name"], "workstation");
    assert_eq!(patched["models"], json!([]));
    assert_eq!(patched["rate_limit_rpm"], 600);
    let (status, patched) = app
        .patch(
            &format!("/keys/{id}"),
            json!({"rate_limit_rpm": null, "enabled": false}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["rate_limit_rpm"], Value::Null);
    assert_eq!(patched["enabled"], false);
    assert_eq!(patched["name"], "workstation");
    // Disabled: the gateway no longer knows the key.
    let presented = switchyard_gateway::PresentedCredentials {
        x_api_key: Some(key.clone()),
        ..Default::default()
    };
    assert!(app.gateway.authenticate(&presented).is_err());
    let (status, _) = app
        .patch(&format!("/keys/{id}"), json!({"enabled": true}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(app.gateway.authenticate(&presented).is_ok());

    // Deleted.
    let (status, body) = app.delete(&format!("/keys/{id}")).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"ok": true})));
    assert!(app.gateway.authenticate(&presented).is_err());
    assert!(!app.file().contains(&key));
    assert_eq!(app.get_ok("/keys").await.as_array().unwrap().len(), 1);
    // The comments of the file outlived all of it.
    assert!(app.file().contains("# The dashboard signs in with this."));

    for (method, path) in [
        (http::Method::DELETE, format!("/keys/{id}")),
        (http::Method::PATCH, format!("/keys/{id}")),
        (http::Method::POST, format!("/keys/{id}/reveal")),
    ] {
        let (status, body) = app.send(method, &path, Some(json!({}))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
}

#[tokio::test]
async fn keys_can_be_supplied_and_duplicates_are_conflicts() {
    let app = App::start().await;

    // The caller's own key.
    let (status, created) = app
        .post(
            "/keys",
            json!({"name": "imported", "key": "  my-own-key-0123456789  "}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["key"], "my-own-key-0123456789");
    assert_eq!(app.chat("my-own-key-0123456789", "mock-echo").await, 200);

    // A reference: shown as written everywhere, never resolved for display.
    let (status, created) = app
        .post(
            "/keys",
            json!({"name": "from-env", "key": "env:CARGO_PKG_NAME"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["key"], "env:CARGO_PKG_NAME");
    assert_eq!(created["is_reference"], true);
    let id = created["id"].as_str().unwrap();
    let listed = app.get_ok("/keys").await[2].clone();
    assert_eq!(listed["masked"], "env:CARGO_PKG_NAME");
    assert_eq!(listed["is_reference"], true);
    let (_, revealed) = app.post(&format!("/keys/{id}/reveal"), json!({})).await;
    assert_eq!(
        revealed,
        json!({"key": "env:CARGO_PKG_NAME", "is_reference": true})
    );

    // The same key, or the same name, twice.
    let (status, body) = app
        .post("/keys", json!({"name": "again", "key": CLIENT_KEY}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["message"], "this client key already exists");
    let (status, body) = app.post("/keys", json!({"name": "Tester"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["error"]["message"],
        "a client key named `Tester` already exists"
    );
    let tester = switchyard_config_store::client_key_id(CLIENT_KEY);
    let (status, _) = app
        .patch(&format!("/keys/{tester}"), json!({"name": "imported"}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Renaming a key to the name it already has is fine.
    let (status, _) = app
        .patch(&format!("/keys/{tester}"), json!({"name": "tester"}))
        .await;
    assert_eq!(status, StatusCode::OK);

    // Bad requests.
    for (body, path) in [
        (json!({"name": "  "}), Some("name")),
        (json!({"models": []}), None),
        (json!({"name": "x", "key": "has a space"}), Some("key")),
        (
            json!({"name": "x", "rate_limit_rpm": -5}),
            Some("rate_limit_rpm"),
        ),
        (json!({"name": "x", "colour": "red"}), None),
    ] {
        let (status, answer) = app.post("/keys", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {answer}");
        if let Some(path) = path {
            assert_eq!(issue_paths(&answer), [path], "{body}: {answer}");
        }
    }
    let (status, answer) = app
        .patch(&format!("/keys/{tester}"), json!({"key": "a-new-key"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert_eq!(app.get_ok("/keys").await.as_array().unwrap().len(), 3);
}
