//! Providers and credentials: views, CRUD, masked secrets, tests,
//! discovery, cooldown reset, enable / disable.

mod support;

use http::StatusCode;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{App, BASE, CLIENT_KEY};

const KEY_1: &str = "sk-upstream-one-1111111111111111";
const KEY_2: &str = "sk-upstream-two-2222222222222222";
const KEY_3: &str = "sk-upstream-three-33333333333333";
const HEADER_SECRET: &str = "Bearer extra-header-secret-4444444444";
const PROXY_PASSWORD: &str = "proxy-password-5555555555";

/// [`BASE`] plus a provider with every kind of secret: shorthand keys, a
/// labelled credential, a reference, a credential header and a proxy
/// password. Its endpoint is a port nothing listens on and discovery is
/// off, so nothing leaves the machine.
fn with_upstream() -> String {
    format!(
        r#"{BASE}
# A real-looking provider.
[[providers]]
name = "upstream"
kind = "openai-compat"
base_url = "http://127.0.0.1:9/v1"
discover = false
proxy = "http://corp:{PROXY_PASSWORD}@127.0.0.1:3128"
api_keys = ["{KEY_1}", "{KEY_2}"] # two shorthand keys

[providers.headers]
X-Extra-Auth = "{HEADER_SECRET}"
X-Team = "platform"

[[providers.credentials]]
api_key = "{KEY_3}"
label = "team"
weight = 3

[[providers.credentials]]
api_key = "env:SWITCHYARD_ADMIN_TEST_VARIABLE_THAT_IS_NOT_SET"
label = "from-env"

[[providers.models]]
id = "up-model"
alias = "nice-model"
"#
    )
}

fn names(list: &Value) -> Vec<&str> {
    list.as_array()
        .expect("a list")
        .iter()
        .map(|item| item["name"].as_str().unwrap_or_default())
        .collect()
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_list_merges_configuration_and_runtime_state() {
    let app = App::start_config(&with_upstream()).await;
    let list = app.get_ok("/providers").await;
    assert_eq!(names(&list), ["mock", "upstream"]);

    let mock = &list[0];
    assert_eq!(mock["kind"], "mock");
    assert_eq!(mock["enabled"], true);
    assert_eq!(mock["effective_base_url"], "mock://local");
    assert_eq!(
        mock["protocols"],
        json!(["openai-chat", "openai-responses", "anthropic", "gemini"])
    );
    assert_eq!(mock["model_count"], 8);
    assert_eq!(mock["models"].as_array().unwrap().len(), 8);
    assert!(
        mock["models"]
            .as_array()
            .unwrap()
            .contains(&json!("mock-echo"))
    );
    // The keyless credential every mock provider has.
    let implicit = &mock["credentials"][0];
    assert_eq!(implicit["source"], "implicit");
    assert_eq!(implicit["index"], Value::Null);
    assert_eq!(implicit["status"], "ready");
    assert_eq!(implicit["usable"], true);
    assert_eq!(implicit["masked_key"], "");

    let upstream = &list[1];
    assert_eq!(upstream["base_url"], "http://127.0.0.1:9/v1");
    assert_eq!(upstream["effective_base_url"], "http://127.0.0.1:9/v1");
    assert_eq!(upstream["protocols"], json!(["openai-chat"]));
    assert_eq!(upstream["models"], json!(["nice-model"]));
    assert_eq!(upstream["model_count"], 1);
    assert_eq!(upstream["api_keys"], json!(["sk-ups…1111", "sk-ups…2222"]));
    assert_eq!(upstream["headers"]["X-Team"], "platform");
    assert_eq!(upstream["headers"]["X-Extra-Auth"], "Bearer…4444");
    assert!(
        upstream["proxy"]
            .as_str()
            .unwrap()
            .starts_with("http://corp:")
    );

    let credentials = upstream["credentials"].as_array().unwrap();
    let summary: Vec<(String, Value, String, String)> = credentials
        .iter()
        .map(|c| {
            (
                c["source"].as_str().unwrap().to_string(),
                c["index"].clone(),
                c["masked_key"].as_str().unwrap().to_string(),
                c["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                "api_keys".to_string(),
                json!(0),
                "sk-ups…1111".to_string(),
                "ready".to_string()
            ),
            (
                "api_keys".to_string(),
                json!(1),
                "sk-ups…2222".to_string(),
                "ready".to_string()
            ),
            (
                "credentials".to_string(),
                json!(0),
                "sk-ups…3333".to_string(),
                "ready".to_string()
            ),
            (
                "credentials".to_string(),
                json!(1),
                "env:SWITCHYARD_ADMIN_TEST_VARIABLE_THAT_IS_NOT_SET".to_string(),
                "unusable".to_string()
            ),
        ]
    );
    let team = &credentials[2];
    assert_eq!(team["label"], "team");
    assert_eq!(team["weight"], 3);
    assert_eq!(team["priority"], 0);
    assert_eq!(team["disabled"], false);
    assert_eq!(team["requests"], 0);
    assert_eq!(team["cooldown_until"], Value::Null);
    assert_eq!(team["model_cooldowns"], json!([]));
    assert_eq!(team["last_error"], Value::Null);
    assert!(team["id"].as_str().unwrap().starts_with("upstream:"));
    let from_env = &credentials[3];
    assert_eq!(from_env["usable"], false);
    assert!(
        from_env["unusable_reason"]
            .as_str()
            .unwrap()
            .contains("is not set")
    );

    // `config` is the entry as the form edits it and as PUT takes it back:
    // the provider's own `credentials` and `models`, secrets masked.
    let editable = &upstream["config"];
    assert_eq!(editable["name"], "upstream");
    assert_eq!(
        editable["models"],
        json!([{"id": "up-model", "alias": "nice-model"}])
    );
    assert_eq!(
        editable["credentials"][0],
        json!({"api_key": "sk-ups…3333", "label": "team", "weight": 3})
    );
    assert_eq!(editable["wire_api"], "auto");
    assert_eq!(editable["legacy_max_tokens"], Value::Null);

    // One provider by name is the same view.
    assert_eq!(app.get_ok("/providers/upstream").await, *upstream);
    let (status, body) = app.get("/providers/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["error"]["message"],
        "there is no provider named `nope`"
    );
}

// ---------------------------------------------------------------------------
// Create, replace, rename, delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn providers_are_created_replaced_renamed_and_deleted() {
    let app = App::start().await;

    let (status, created) = app
        .post(
            "/providers",
            json!({
                "name": " second ",
                "kind": "openai-compat",
                "base_url": "http://127.0.0.1:9/v1",
                "discover": false,
                "api_keys": ["", KEY_1, "   "],
                "models": [{"id": "m-one"}],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["name"], "second");
    assert_eq!(created["api_keys"], json!(["sk-ups…1111"]));
    assert_eq!(created["credentials"].as_array().unwrap().len(), 1);
    assert_eq!(created["credentials"][0]["status"], "ready");
    assert_eq!(created["models"], json!(["m-one"]));
    // Blank rows were dropped, the key was written in full, comments stay.
    let file = app.file();
    assert!(
        file.contains(&format!("api_keys = [\"{KEY_1}\"]")),
        "{file}"
    );
    assert!(file.contains("# The built-in fake models."), "{file}");
    assert_eq!(names(&app.get_ok("/providers").await), ["mock", "second"]);

    // The name is taken now.
    let (status, body) = app
        .post("/providers", json!({"name": "second", "kind": "mock"}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["error"]["message"],
        "a provider named `second` already exists"
    );

    // Replace: every field comes from the body.
    let (status, replaced) = app
        .put(
            "/providers/second",
            json!({
                "name": "second",
                "kind": "openai-compat",
                "base_url": "http://127.0.0.1:9/v1",
                "discover": false,
                "api_keys": [created["api_keys"][0]],
                "prefix": "two",
                "models": [{"id": "m-one"}, {"id": "m-two", "alias": "second-best"}],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["prefix"], "two");
    assert_eq!(
        replaced["models"],
        json!(["m-one", "second-best", "two/m-one", "two/second-best"])
    );
    assert_eq!(replaced["model_count"], 2);
    assert!(app.file().contains(&format!("api_keys = [\"{KEY_1}\"]")));

    // Rename through the body; payload rules that name the provider follow.
    let (status, body) = app
        .put(
            "/payload",
            json!({"override": [{"models": ["*"], "provider": "second", "set": {"temperature": 0}}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut renamed = replaced["config"].clone();
    renamed["name"] = json!("third");
    let (status, body) = app.put("/providers/second", renamed.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "third");
    assert_eq!(names(&app.get_ok("/providers").await), ["mock", "third"]);
    assert_eq!(app.get("/providers/second").await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        app.get_ok("/payload").await["override"][0]["provider"],
        "third"
    );
    // The masked key came back with the rename and is still the real one.
    assert!(app.file().contains(&format!("api_keys = [\"{KEY_1}\"]")));

    // Renaming onto an existing name is a conflict; the old name is gone.
    renamed["name"] = json!("mock");
    let (status, _) = app.put("/providers/third", renamed.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = app.put("/providers/second", renamed).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = app.delete("/providers/third").await;
    assert_eq!((status, body), (StatusCode::OK, json!({"ok": true})));
    assert_eq!(
        app.delete("/providers/third").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(names(&app.get_ok("/providers").await), ["mock"]);
    assert!(!app.file().contains(KEY_1));
}

#[tokio::test]
async fn invalid_providers_are_refused_and_change_nothing() {
    let app = App::start().await;

    // Not the schema: 400, naming the field.
    let (status, body) = app
        .post(
            "/providers",
            json!({"name": "x", "kind": "mock", "colour": "red"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("colour"),
        "{body}"
    );
    let (status, body) = app
        .post("/providers", json!({"name": "x", "kind": "telepathy"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["issues"][0]["path"], "kind");
    let (status, _) = app.post("/providers", json!({"kind": "mock"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = app.post("/providers", json!([1, 2])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The schema, but not a valid configuration: 422 with issues.
    let (status, body) = app
        .post(
            "/providers",
            json!({"name": "Bad Name", "kind": "openai-compat", "proxy": "ftp://nope"}),
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
        // By their place in the request body, the provider entry.
        ["name", "base_url", "proxy"]
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("the configuration is not valid: name: may only contain"),
        "{body}"
    );

    // A masked secret in a new provider has nothing to stand for.
    let (status, body) = app
        .post(
            "/providers",
            json!({"name": "copy", "kind": "openai", "api_keys": ["sk-ups…1111"]}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["issues"][0]["path"], "api_keys[0]");

    let (status, body) = app
        .put(
            "/providers/mock",
            json!({"name": "mock", "kind": "openai-compat"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["issues"][0]["path"], "base_url");

    assert_eq!(app.file(), BASE);
    assert_eq!(names(&app.get_ok("/providers").await), ["mock"]);
}

// ---------------------------------------------------------------------------
// Masked secrets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn masked_secrets_round_trip_through_put() {
    let app = App::start_config(&with_upstream()).await;
    let shown = app.get_ok("/providers/upstream").await["config"].clone();
    // Nothing the dashboard was shown is a secret.
    let text = shown.to_string();
    for secret in [KEY_1, KEY_2, KEY_3, HEADER_SECRET, PROXY_PASSWORD] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }

    // Saved unchanged: every secret is still the stored one.
    let (status, body) = app.put("/providers/upstream", shown.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let stored = app.gateway.config();
    let upstream = stored.provider("upstream").unwrap();
    assert_eq!(upstream.api_keys, [KEY_1, KEY_2]);
    assert_eq!(upstream.credentials[0].api_key, KEY_3);
    assert_eq!(
        upstream.credentials[1].api_key,
        "env:SWITCHYARD_ADMIN_TEST_VARIABLE_THAT_IS_NOT_SET"
    );
    assert_eq!(upstream.headers["X-Extra-Auth"], HEADER_SECRET);
    assert_eq!(
        upstream.proxy,
        format!("http://corp:{PROXY_PASSWORD}@127.0.0.1:3128")
    );
    // An edit that changes nothing leaves the file alone, comments and all.
    assert_eq!(app.file(), with_upstream());

    // First key removed, second kept by its mask, a new one added; the
    // labelled credential gets a new key; an emptied credential key keeps
    // the stored one; the reference stays as written.
    let mut edited = shown.clone();
    edited["api_keys"] = json!([shown["api_keys"][1], "sk-brand-new-key-66666666666666666"]);
    edited["credentials"][0]["api_key"] = json!("sk-replaced-7777777777777777");
    edited["credentials"][1]["api_key"] = json!("");
    edited["priority"] = json!(5);
    let (status, body) = app.put("/providers/upstream", edited).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["priority"], 5);
    assert_eq!(body["api_keys"], json!(["sk-ups…2222", "sk-bra…6666"]));
    let stored = app.gateway.config();
    let upstream = stored.provider("upstream").unwrap();
    assert_eq!(
        upstream.api_keys,
        [KEY_2, "sk-brand-new-key-66666666666666666"]
    );
    assert_eq!(
        upstream.credentials[0].api_key,
        "sk-replaced-7777777777777777"
    );
    assert_eq!(
        upstream.credentials[1].api_key,
        "env:SWITCHYARD_ADMIN_TEST_VARIABLE_THAT_IS_NOT_SET"
    );
    assert_eq!(upstream.headers["X-Extra-Auth"], HEADER_SECRET);
    let file = app.file();
    assert!(!file.contains(KEY_1), "{file}");
    assert!(file.contains("# A real-looking provider."), "{file}");
    assert!(file.contains("# two shorthand keys"), "{file}");

    // A mask that matches nothing stored is refused, not saved as a key.
    let mut bogus = shown;
    bogus["api_keys"] = json!(["sk-zzz…9999"]);
    let (status, body) = app.put("/providers/upstream", bogus).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["issues"][0]["path"], "api_keys[0]");
    assert_eq!(
        app.gateway
            .config()
            .provider("upstream")
            .unwrap()
            .api_keys
            .len(),
        2
    );
}

// ---------------------------------------------------------------------------
// Test and discover
// ---------------------------------------------------------------------------

/// A local stand-in for an OpenAI-compatible upstream: it lists two models
/// and rejects every generation request, quoting the key it was given the
/// way careless upstreams do.
async fn fake_upstream() -> std::net::SocketAddr {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/v1/models",
            get(|| async {
                axum::Json(
                    json!({"object": "list", "data": [{"id": "fake-small"}, {"id": "fake-large"}]}),
                )
            }),
        )
        .route(
            "/v1/chat/completions",
            post(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(json!({"error": {
                        "message": format!("Incorrect API key provided: {KEY_1}"),
                        "type": "invalid_request_error",
                    }})),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

#[tokio::test]
async fn a_provider_can_be_tested_and_asked_for_its_models() {
    let upstream = fake_upstream().await;
    let config = format!(
        r#"{BASE}
[[providers]]
name = "fake"
kind = "openai-compat"
base_url = "http://{upstream}/v1"
discover = false
api_keys = ["{KEY_1}"]
[[providers.models]]
id = "fake-small"

[[providers]]
name = "keyless"
kind = "openai"
discover = false
api_keys = ["env:SWITCHYARD_ADMIN_TEST_VARIABLE_THAT_IS_NOT_SET"]
[[providers.models]]
id = "gpt-test"
"#
    );
    let app = App::start_config(&config).await;

    let (status, body) = app.post("/providers/mock/test", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["status"], 200);
    assert_eq!(body["credential"], "mock");
    assert!(body["model"].is_string());
    assert!(body["latency_ms"].is_u64());
    assert!(body.get("error").is_none());

    // A named model, and no body at all.
    let (status, body) = app
        .post("/providers/mock/test", json!({"model": "mock-error-500"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], false);
    assert_eq!(body["status"], 500);
    assert_eq!(body["model"], "mock-error-500");
    assert!(body["error"].is_string());
    let (status, body) = app
        .send(http::Method::POST, "/providers/mock/test", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);

    // An upstream that refuses is a result, not an HTTP error, and what it
    // said is shown without the key it quoted.
    let (status, body) = app.post("/providers/fake/test", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], false);
    assert_eq!(body["status"], 401);
    assert_eq!(body["model"], "fake-small");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("Incorrect API key"),
        "{body}"
    );
    assert!(!body.to_string().contains(KEY_1), "{body}");

    let (status, body) = app.post("/providers/nope/test", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = app
        .post("/providers/mock/test", json!({"modle": "x"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = app.post("/providers/mock/discover", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let models = body["models"].as_array().unwrap();
    assert_eq!(models.len(), 8);
    assert_eq!(models[0]["id"], "mock-echo");
    let (status, body) = app.post("/providers/fake/discover", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["fake-small", "fake-large"]);

    let (status, body) = app.post("/providers/nope/discover", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // A provider that cannot be asked answers in the admin error shape.
    let (status, body) = app.post("/providers/keyless/discover", json!({})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no usable credential"),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

async fn credential_id(app: &App, provider: &str, position: usize) -> String {
    app.get_ok(&format!("/providers/{provider}")).await["credentials"][position]["id"]
        .as_str()
        .expect("a credential id")
        .to_string()
}

#[tokio::test]
async fn a_cooldown_can_be_reset() {
    let app = App::start().await;
    let id = credential_id(&app, "mock", 0).await;

    // A failing mock model rests itself on the credential.
    assert!(app.chat(CLIENT_KEY, "mock-error-500").await >= 500);
    let resting = app.get_ok("/providers/mock").await;
    let cooldowns = resting["credentials"][0]["model_cooldowns"]
        .as_array()
        .unwrap();
    assert_eq!(cooldowns.len(), 1, "{resting}");
    assert_eq!(cooldowns[0]["model"], "mock-error-500");
    assert!(cooldowns[0]["until"].as_i64().unwrap() > 1_600_000_000_000);
    assert_eq!(resting["credentials"][0]["failures"], 1);
    assert_eq!(resting["credentials"][0]["last_error"]["status"], 500);

    let (status, body) = app
        .post(&format!("/credentials/{id}/reset"), json!({}))
        .await;
    assert_eq!((status, body), (StatusCode::OK, json!({"ok": true})));
    let rested = app.get_ok("/providers/mock").await;
    assert_eq!(rested["credentials"][0]["model_cooldowns"], json!([]));
    // Counters are history and stay.
    assert_eq!(rested["credentials"][0]["failures"], 1);

    let (status, body) = app
        .post("/credentials/mock:000000000000/reset", json!({}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn disabling_a_credential_is_written_to_the_file_and_takes_effect() {
    let app = App::start().await;
    let id = credential_id(&app, "mock", 0).await;
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);

    let (status, view) = app
        .post(&format!("/credentials/{id}/disable"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["name"], "mock");
    let credential = &view["credentials"][0];
    assert_eq!(credential["id"], id);
    assert_eq!(credential["disabled"], true);
    assert_eq!(credential["status"], "disabled");
    assert_eq!(credential["source"], "credentials");
    assert_eq!(credential["index"], 0);
    // In the file, so it survives a restart …
    let file = app.file();
    assert!(file.contains("disabled = true"), "{file}");
    assert!(file.contains("# The built-in fake models."), "{file}");
    let reread = switchyard_config_store::validate_text(&file).unwrap();
    assert!(reread.providers[0].credentials[0].disabled);
    // … and in effect: nothing can serve the provider's models now.
    let refused = app.chat(CLIENT_KEY, "mock-echo").await;
    assert!(refused >= 400, "{refused}");

    // Disabling twice is not an error.
    let (status, _) = app
        .post(&format!("/credentials/{id}/disable"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, view) = app
        .post(&format!("/credentials/{id}/enable"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["credentials"][0]["disabled"], false);
    assert_eq!(view["credentials"][0]["status"], "ready");
    assert!(!app.gateway.config().providers[0].credentials[0].disabled);
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);

    for action in ["enable", "disable"] {
        let (status, body) = app
            .post(
                &format!("/credentials/mock:ffffffffffff/{action}"),
                json!({}),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }
}

#[tokio::test]
async fn disabling_a_shorthand_key_turns_it_into_a_credential_entry() {
    let app = App::start_config(&with_upstream()).await;
    let first = credential_id(&app, "upstream", 0).await;
    let team = credential_id(&app, "upstream", 2).await;

    let (status, view) = app
        .post(&format!("/credentials/{first}/disable"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    // The key left `api_keys` and became the last `credentials` entry,
    // under the same id.
    assert_eq!(view["api_keys"], json!(["sk-ups…2222"]));
    let credentials = view["credentials"].as_array().unwrap();
    assert_eq!(credentials.len(), 4);
    let moved = credentials
        .iter()
        .find(|c| c["id"] == first.as_str())
        .unwrap();
    assert_eq!(moved["source"], "credentials");
    assert_eq!(moved["index"], 2);
    assert_eq!(moved["disabled"], true);
    assert_eq!(moved["status"], "disabled");
    assert_eq!(moved["masked_key"], "sk-ups…1111");

    let stored = app.gateway.config();
    let upstream = stored.provider("upstream").unwrap();
    assert_eq!(upstream.api_keys, [KEY_2]);
    assert_eq!(upstream.credentials[2].api_key, KEY_1);
    assert!(upstream.credentials[2].disabled);
    let file = app.file();
    assert!(file.contains("# two shorthand keys"), "{file}");
    assert!(file.contains("# A real-looking provider."), "{file}");

    // A `credentials[]` entry just gets the flag.
    let (status, view) = app
        .post(&format!("/credentials/{team}/disable"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    let stored = app.gateway.config();
    let upstream = stored.provider("upstream").unwrap();
    assert!(upstream.credentials[0].disabled);
    assert_eq!(upstream.credentials[0].label, "team");
    assert_eq!(upstream.credentials[0].api_key, KEY_3);

    // Enabling the converted key flips the flag of the entry it has become.
    let (status, view) = app
        .post(&format!("/credentials/{first}/enable"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    let moved = view["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == first.as_str())
        .unwrap();
    assert_eq!(moved["disabled"], false);
    assert_eq!(moved["status"], "ready");
}

#[tokio::test]
async fn a_credential_switched_off_at_runtime_is_switched_on_through_the_api() {
    let app = App::start().await;
    let id = credential_id(&app, "mock", 0).await;
    assert!(app.gateway.scheduler().set_runtime_disabled(&id, true));
    assert_eq!(
        app.get_ok("/providers/mock").await["credentials"][0]["disabled"],
        true
    );

    let (status, view) = app
        .post(&format!("/credentials/{id}/enable"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["credentials"][0]["disabled"], false);
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    // Nothing had to be written for that.
    assert_eq!(app.file(), BASE);
}

/// Regression (CFG-1): two keys of up to 11 characters both show as
/// `••••••••`. Deleting the first one and saving kept the deleted key and
/// dropped the good one; giving the second a label was refused as "the same
/// key is listed twice".
#[tokio::test]
async fn keys_that_mask_alike_are_never_mixed_up_on_save() {
    let config = format!(
        "{BASE}\n[[providers]]\nname = \"local-vllm\"\nkind = \"openai-compat\"\n\
         base_url = \"http://127.0.0.1:1/v1\"\ndiscover = false\n\
         api_keys = [\"old-leaked\", \"new-good\"]\n"
    );
    let app = App::start_config(&config).await;
    let view = app.get_ok("/providers/local-vllm").await;
    let entry = view["config"].clone();
    let mask = entry["api_keys"][0].clone();
    assert_eq!(mask, entry["api_keys"][1]);

    // Row 1 deleted: refused, nothing written.
    let mut deleted = entry.clone();
    deleted["api_keys"] = json!([mask]);
    let (status, body) = app.put("/providers/local-vllm", deleted).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["issues"][0]["path"], "api_keys[0]", "{body}");
    assert!(
        app.file()
            .contains("api_keys = [\"old-leaked\", \"new-good\"]")
    );

    // Row 2 gets a label: each key stays with its row.
    let mut labelled = entry.clone();
    labelled["api_keys"] = json!([mask]);
    labelled["credentials"] = json!([{"api_key": mask, "label": "second"}]);
    let (status, body) = app.put("/providers/local-vllm", labelled).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let stored = app.gateway.config();
    let provider = stored.provider("local-vllm").unwrap();
    assert_eq!(provider.api_keys, ["old-leaked"]);
    assert_eq!(provider.credentials[0].api_key, "new-good");
    assert_eq!(provider.credentials[0].label, "second");
}
