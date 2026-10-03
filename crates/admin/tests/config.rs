//! Status and the configuration as a whole: masked view, raw text,
//! validation, the settings patch, reload, concurrent edits.

mod support;

use http::{Method, StatusCode};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{App, BASE, CLIENT_KEY, SECRET, read};
use switchyard_admin::AdminOptions;
use switchyard_gateway::Gateway;

fn issue_paths(body: &Value) -> Vec<String> {
    body["error"]["issues"]
        .as_array()
        .map(|issues| {
            issues
                .iter()
                .map(|issue| issue["path"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_describes_the_running_gateway() {
    let app = App::start().await;
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);

    let status = app.get_ok("/status").await;
    assert_eq!(status["version"], Gateway::version());
    assert!(status["started_at"].as_i64().unwrap() > 1_600_000_000_000);
    assert!(status["uptime_ms"].is_u64());
    assert_eq!(
        status["config_path"],
        app.gateway.config_store().path().display().to_string()
    );
    assert!(status["data_dir"].as_str().unwrap().ends_with("data"));
    assert_eq!(status["listen"], app.addr.to_string());
    assert_eq!(status["restart_required"], json!([]));
    assert_eq!(status["warnings"], json!([]));
    assert_eq!(
        status["counts"],
        json!({
            "providers": 1,
            "credentials": 1,
            "credentials_ready": 1,
            "models": 8,
            "client_keys": 1,
        })
    );
    assert_eq!(status["live"]["totals"]["requests"], 1);
    assert_eq!(status["live"]["in_flight"], 0);
    assert_eq!(
        status["admin"],
        json!({"allow_remote": false, "remote": false})
    );
    assert_eq!(status["auth_required"], true);

    // Nothing may cache an admin answer.
    let response = app.request(Method::GET, "/status").send().await.unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
}

#[tokio::test]
async fn status_reports_warnings_and_pending_restarts() {
    let config = format!(
        "{BASE}\n[[providers]]\nname = \"broken\"\nkind = \"openai\"\ndiscover = false\n\
         api_keys = [\"env:SWITCHYARD_ADMIN_TEST_VARIABLE_THAT_IS_NOT_SET\"]\n\
         [[providers.models]]\nid = \"gpt-test\"\n"
    );
    let app = App::start_config(&config).await;
    let status = app.get_ok("/status").await;
    let warnings = status["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].as_str().unwrap().contains("broken"),
        "{warnings:?}"
    );
    assert_eq!(status["counts"]["credentials"], 2);
    assert_eq!(status["counts"]["credentials_ready"], 1);

    let (code, body) = app
        .patch("/settings", json!({"server": {"port": 9999}}))
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["restart_required"], json!(["server.port"]));
    let status = app.get_ok("/status").await;
    assert_eq!(status["restart_required"], json!(["server.port"]));
    assert_eq!(status["command_line_overrides"], json!([]));
}

/// Regression (SU-5): with `--port` on the command line a changed
/// `server.port` was reported as waiting for a restart, which does not apply
/// it: the command line wins again.
#[tokio::test]
async fn a_setting_the_command_line_fixes_is_never_waiting_for_a_restart() {
    let app = App::start_with(
        BASE,
        AdminOptions {
            command_line: vec!["server.port".into()],
            ..AdminOptions::default()
        },
    )
    .await;
    let (code, body) = app
        .patch(
            "/settings",
            json!({"server": {"port": 9999, "data_dir": "elsewhere"}}),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    // Saved to the file, but not pending: only `data_dir` is.
    assert_eq!(body["config"]["server"]["port"], 9999);
    assert_eq!(body["restart_required"], json!(["server.data_dir"]));
    assert_eq!(body["command_line_overrides"], json!(["server.port"]));
    let status = app.get_ok("/status").await;
    assert_eq!(status["restart_required"], json!(["server.data_dir"]));
    assert_eq!(status["command_line_overrides"], json!(["server.port"]));
    let view = app.get_ok("/config").await;
    assert_eq!(view["restart_required"], json!(["server.data_dir"]));
    assert_eq!(view["command_line_overrides"], json!(["server.port"]));
}

/// Regression (QA-ONB-03, BE-1): a hand edit the gateway refused was only
/// announced by a live frame, so a page loaded afterwards showed nothing:
/// `warnings` was empty and `GET /config` had no trace of it.
#[tokio::test]
async fn a_refused_file_is_reported_by_status_and_config_until_it_is_fixed() {
    let app = App::start().await;
    let status = app.get_ok("/status").await;
    assert_eq!(status["config_rejected"], Value::Null);
    assert_eq!(app.get_ok("/config").await["config_rejected"], Value::Null);

    let path = app.gateway.config_store().path().to_path_buf();
    let broken = format!("{BASE}\n[[providers]]\nname = \"Lab Bad!\"\nkind = \"mock\"\n");
    std::fs::write(&path, &broken).unwrap();
    let (code, body) = app.post("/reload", json!({})).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Any page loaded from now on can tell.
    for _ in 0..2 {
        let status = app.get_ok("/status").await;
        let rejected = &status["config_rejected"];
        assert_eq!(
            rejected["issues"][0]["path"], "providers[1].name",
            "{status}"
        );
        assert!(rejected["at"].as_i64().unwrap() > 1_600_000_000_000);
        let message = rejected["message"].as_str().unwrap();
        assert!(message.contains("refused"), "{message}");
        assert!(message.contains("providers[1].name"), "{message}");
        assert_eq!(status["warnings"], json!([message]));
        // The configuration in effect is still the previous one.
        assert_eq!(status["counts"]["providers"], 1);
        let view = app.get_ok("/config").await;
        assert_eq!(view["config_rejected"], *rejected);
    }

    // A refused edit (409) says the same; the time stays that of the
    // refusal.
    let at = app.get_ok("/status").await["config_rejected"]["at"].clone();
    let (code, body) = app
        .patch("/settings", json!({"logging": {"level": "debug"}}))
        .await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert_eq!(app.get_ok("/status").await["config_rejected"]["at"], at);

    // Fixed by hand and taken up: gone.
    std::fs::write(&path, BASE).unwrap();
    let (code, body) = app.post("/reload", json!({})).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["config_rejected"], Value::Null);
    let status = app.get_ok("/status").await;
    assert_eq!(status["config_rejected"], Value::Null);
    assert_eq!(status["warnings"], json!([]));

    // Replacing the whole file ends it as well.
    std::fs::write(&path, &broken).unwrap();
    let (code, _) = app.post("/reload", json!({})).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_ne!(app.get_ok("/status").await["config_rejected"], Value::Null);
    let (code, body) = app.put("/config/raw", json!({"text": BASE})).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(app.get_ok("/status").await["config_rejected"], Value::Null);
}

// ---------------------------------------------------------------------------
// GET /config, /config/raw, /config/validate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_config_view_is_masked_and_the_raw_text_is_verbatim() {
    let app = App::start().await;

    let view = app.get_ok("/config").await;
    assert_eq!(
        view["path"],
        app.gateway.config_store().path().display().to_string()
    );
    assert_eq!(view["restart_required"], json!([]));
    let config = &view["config"];
    assert_eq!(config["admin"]["secret"], "test-a…cdef");
    assert_eq!(config["auth"]["keys"][0]["key"], "sy-tes…cdef");
    assert_eq!(config["auth"]["keys"][0]["name"], "tester");
    assert_eq!(config["providers"][0]["name"], "mock");
    assert_eq!(config["server"]["port"], 8317);
    assert_eq!(config["upstream"]["proxy"], "direct");
    assert!(!view.to_string().contains(SECRET));
    assert!(!view.to_string().contains(CLIENT_KEY));

    let raw = app.get_ok("/config/raw").await;
    assert_eq!(raw["text"], BASE);
    assert_eq!(raw["path"], view["path"]);
    assert!(raw["modified_at"].as_i64().unwrap() > 1_600_000_000_000);
}

#[tokio::test]
async fn validate_always_answers_200_with_the_verdict() {
    let app = App::start().await;

    let (status, body) = app.post("/config/validate", json!({"text": BASE})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"ok": true, "issues": []}));

    let (status, body) = app
        .post(
            "/config/validate",
            json!({"text": "[server]\nport = 0\n\n[[providers]]\nname = \"Bad Name\"\nkind = \"mock\"\n"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], false);
    let paths: Vec<&str> = body["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|issue| issue["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["server.port", "providers[0].name"]);

    // A syntax error says where.
    let (status, body) = app
        .post("/config/validate", json!({"text": "[server\nport = 1"}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], false);
    assert!(
        body["issues"][0]["path"]
            .as_str()
            .unwrap()
            .starts_with("line 1"),
        "{body}"
    );

    // Nothing was applied or written.
    assert_eq!(app.file(), BASE);

    // The envelope itself is checked.
    let (status, body) = app.post("/config/validate", json!({"txt": "x"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = read(
        app.request(Method::POST, "/config/validate")
            .header("content-type", "application/json")
            .body("{not json"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not valid JSON"),
        "{body}"
    );
    let (status, body) = read(app.request(Method::POST, "/config/validate")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"].as_str().unwrap().contains("empty"),
        "{body}"
    );
}

#[tokio::test]
async fn the_raw_editor_validates_writes_verbatim_and_applies() {
    let app = App::start().await;

    let (status, body) = app
        .put(
            "/config/raw",
            json!({"text": "[routing]\nmax_attempts = 0\n"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(issue_paths(&body), ["routing.max_attempts"]);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("routing.max_attempts"),
        "{body}"
    );
    assert_eq!(app.file(), BASE);

    let (status, body) = app
        .put("/config/raw", json!({"text": "this is not toml"}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(app.file(), BASE);

    let edited = format!(
        "{BASE}\n# added by hand\n[routing]\nstrategy = \"fill-first\" # keep caches warm\n"
    );
    let (status, body) = app.put("/config/raw", json!({"text": edited})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["config"]["routing"]["strategy"], "fill-first");
    assert_eq!(body["config"]["admin"]["secret"], "test-a…cdef");
    assert_eq!(app.file(), edited);
    // The gateway runs on it by the time the answer arrives.
    assert_eq!(
        app.gateway.scheduler().config().routing.strategy,
        switchyard_core::config::Strategy::FillFirst
    );
}

/// The raw editor replaces everything, and an empty text is a valid
/// configuration (all defaults) that has no admin secret: saving it would
/// answer once and then lock the dashboard out. Such a save is refused and
/// nothing changes.
#[tokio::test]
async fn a_raw_text_that_would_lock_the_dashboard_out_is_refused() {
    let app = App::start().await;
    let before = app.file();

    for text in [
        "",
        "   \n",
        "[server]\nport = 8317\n",
        "[admin]\nsecret = \"\"\n",
    ] {
        let (status, body) = app.put("/config/raw", json!({"text": text})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text:?}: {body}");
        assert_eq!(body["error"]["issues"][0]["path"], "admin.secret", "{body}");
        assert_eq!(app.file(), before, "{text:?} must not be written");
    }

    // A reference to a variable that is not set is no secret either.
    let (status, body) = app
        .put(
            "/config/raw",
            json!({"text": "[admin]\nsecret = \"env:SWITCHYARD_TEST_NO_SUCH_VARIABLE\"\n"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["issues"][0]["path"], "admin.secret", "{body}");

    // Switching the interface off outright is refused the same way.
    let (status, body) = app
        .patch("/settings", json!({"admin": {"enabled": false}}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["error"]["issues"][0]["path"], "admin.enabled",
        "{body}"
    );

    // Nothing changed, and the interface still answers.
    assert_eq!(app.file(), before);
    assert_eq!(app.get("/status").await.0, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// PATCH /settings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn settings_are_patched_in_place_and_comments_survive() {
    let app = App::start().await;

    let (status, body) = app
        .patch(
            "/settings",
            json!({
                "routing": {"strategy": "least-latency", "cooldown": {"transient_secs": 5}},
                "streaming": {"keepalive_secs": 7},
                "logging": {"level": "debug", "request_log": "errors"},
                "usage": {"retention_days": 14},
                "auth": {"required": false},
                "upstream": {"request_timeout_secs": 120},
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let config = &body["config"];
    assert_eq!(config["routing"]["strategy"], "least-latency");
    assert_eq!(config["routing"]["cooldown"]["transient_secs"], 5);
    assert_eq!(config["routing"]["cooldown"]["quota_secs"], 3600);
    assert_eq!(config["streaming"]["keepalive_secs"], 7);
    assert_eq!(config["logging"]["level"], "debug");
    assert_eq!(config["logging"]["request_log"], "errors");
    assert_eq!(config["usage"]["retention_days"], 14);
    assert_eq!(config["auth"]["required"], false);
    assert_eq!(config["upstream"]["request_timeout_secs"], 120);
    assert_eq!(body["restart_required"], json!([]));
    assert_eq!(
        body["path"],
        app.gateway.config_store().path().display().to_string()
    );

    // The file kept every comment and gained only what changed.
    let file = app.file();
    for comment in [
        "# Switchyard test configuration.",
        "# The dashboard signs in with this.",
        "proxy = \"direct\" # never the developer's proxy",
        "# The built-in fake models.",
    ] {
        assert!(file.contains(comment), "lost `{comment}`:\n{file}");
    }
    assert!(file.contains("strategy = \"least-latency\""), "{file}");
    assert!(file.contains("keepalive_secs = 7"), "{file}");
    assert!(file.contains(&format!("secret = \"{SECRET}\"")), "{file}");
    // Applied.
    assert_eq!(app.gateway.config().streaming.keepalive_secs, 7);
    assert!(!app.gateway.config().auth.required);

    // `null` puts a setting back to its default.
    let (status, body) = app
        .patch("/settings", json!({"streaming": {"keepalive_secs": null}}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["config"]["streaming"]["keepalive_secs"], 15);

    // An empty patch changes nothing and is not an error.
    let before = app.file();
    let (status, _) = app.patch("/settings", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(app.file(), before);
}

#[tokio::test]
async fn settings_refuse_what_they_must_not_touch() {
    let app = App::start().await;

    let (status, body) = app
        .patch(
            "/settings",
            json!({"providers": [], "auth": {"keys": []}, "pricing": [], "usage": {"enabled": false}}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(issue_paths(&body), ["providers", "auth.keys", "pricing"]);
    // Nothing of a refused patch is applied, not even its valid part.
    assert!(app.gateway.config().usage.enabled);

    // Unknown fields and wrong types are 400s that name the field.
    let (status, body) = app
        .patch("/settings", json!({"routing": {"stratgy": "x"}}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["issues"][0]["message"]
            .as_str()
            .unwrap()
            .contains("stratgy"),
        "{body}"
    );
    let (status, body) = app
        .patch(
            "/settings",
            json!({"routing": {"cooldown": {"auth_secs": "long"}}}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(issue_paths(&body), ["routing.cooldown.auth_secs"]);
    let (status, _) = app
        .patch("/settings", json!({"routing": {"strategy": "fastest"}}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = app.patch("/settings", json!(["routing"])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A value of the right type that breaks a rule is a 422 with issues.
    let (status, body) = app
        .patch(
            "/settings",
            json!({"server": {"port": 0}, "logging": {"level": "loud"}}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let mut paths = issue_paths(&body);
    paths.sort();
    assert_eq!(paths, ["logging.level", "server.port"]);

    assert_eq!(app.file(), BASE);
}

#[tokio::test]
async fn the_admin_secret_round_trips_masked_and_can_be_changed() {
    let app = App::start().await;
    let masked = app.get_ok("/config").await["config"]["admin"]["secret"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(masked, "test-a…cdef");

    // The form sends back what it was shown: the mask (or nothing).
    for unchanged in [masked.as_str(), ""] {
        let (status, body) = app
            .patch(
                "/settings",
                json!({"admin": {"secret": unchanged, "ui": true}}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(app.file().contains(&format!("secret = \"{SECRET}\"")));
        assert_eq!(app.get("/status").await.0, StatusCode::OK);
    }

    // A mask of something else cannot be resolved.
    let (status, body) = app
        .patch("/settings", json!({"admin": {"secret": "othe…r123"}}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(issue_paths(&body), ["admin.secret"]);

    // A new value replaces the secret: the old one stops working at once.
    let new_secret = "a-new-admin-secret-9876543210";
    let (status, body) = app
        .patch("/settings", json!({"admin": {"secret": new_secret}}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["config"]["admin"]["secret"], "a-ne…210");
    assert!(!body.to_string().contains(new_secret));
    assert!(app.file().contains(&format!("secret = \"{new_secret}\"")));
    assert_eq!(app.get("/status").await.0, StatusCode::UNAUTHORIZED);
    let (status, _) = read(app.http.get(app.api("/status")).bearer_auth(new_secret)).await;
    assert_eq!(status, StatusCode::OK);

    // A reference is kept as written, and shown as written.
    let (status, body) = read(
        app.http
            .patch(app.api("/settings"))
            .bearer_auth(new_secret)
            .json(&json!({"admin": {"secret": "env:CARGO_PKG_NAME"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["config"]["admin"]["secret"], "env:CARGO_PKG_NAME");
    assert!(app.file().contains("secret = \"env:CARGO_PKG_NAME\""));
}

#[tokio::test]
async fn a_masked_proxy_password_survives_a_settings_edit() {
    let config = BASE.replace(
        "proxy = \"direct\" # never the developer's proxy",
        "proxy = \"http://corp:proxy-password-0123456789@127.0.0.1:3128\"",
    );
    let app = App::start_config(&config).await;
    let shown = app.get_ok("/config").await["config"]["upstream"]["proxy"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!shown.contains("proxy-password-0123456789"), "{shown}");
    assert!(shown.starts_with("http://corp:"), "{shown}");

    let (status, body) = app
        .patch(
            "/settings",
            json!({"upstream": {"proxy": shown, "connect_timeout_secs": 9}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        app.gateway.config().upstream.proxy,
        "http://corp:proxy-password-0123456789@127.0.0.1:3128"
    );
    assert_eq!(app.gateway.config().upstream.connect_timeout_secs, 9);
}

// ---------------------------------------------------------------------------
// POST /reload
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reload_applies_the_file_on_disk_or_says_why_not() {
    let app = App::start().await;

    // The file is not watched in tests: an edit only takes effect on reload.
    let edited = format!("{BASE}\n[routing]\nmax_attempts = 7\n");
    std::fs::write(&app.config_path, &edited).unwrap();
    assert_eq!(app.gateway.config().routing.max_attempts, 3);
    let (status, body) = app.post("/reload", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["config"]["routing"]["max_attempts"], 7);
    assert_eq!(app.gateway.scheduler().config().routing.max_attempts, 7);

    std::fs::write(&app.config_path, "[routing]\nmax_attempts = 0\n").unwrap();
    let (status, body) = app.post("/reload", json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(issue_paths(&body), ["routing.max_attempts"]);
    // The previous configuration stays in effect — and with it the secret.
    assert_eq!(app.gateway.config().routing.max_attempts, 7);
    assert_eq!(app.get("/status").await.0, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_edits_are_all_applied() {
    let app = App::start().await;

    let mut edits = Vec::new();
    for i in 0..8 {
        edits.push(app.post("/keys", json!({"name": format!("key-{i}")})));
    }
    let providers: Vec<_> = (0..4)
        .map(|i| {
            app.post(
                "/providers",
                json!({"name": format!("extra-{i}"), "kind": "mock", "prefix": format!("x{i}")}),
            )
        })
        .collect();
    let settings = [
        app.patch("/settings", json!({"streaming": {"keepalive_secs": 21}})),
        app.patch("/settings", json!({"routing": {"max_attempts": 5}})),
        app.patch("/settings", json!({"usage": {"retention_days": 9}})),
    ];
    let aliases = app.put(
        "/aliases",
        json!([{"name": "fast", "targets": ["mock-echo"]}]),
    );

    let (keys, providers, settings, aliases) = tokio::join!(
        futures::future::join_all(edits),
        futures::future::join_all(providers),
        futures::future::join_all(settings),
        aliases,
    );
    for (status, body) in keys.iter().chain(&providers) {
        assert_eq!(*status, StatusCode::CREATED, "{body}");
    }
    for (status, body) in settings.iter().chain([&aliases]) {
        assert_eq!(*status, StatusCode::OK, "{body}");
    }

    // Every edit is in the live configuration and in the file.
    let config = app.gateway.config();
    let mut names: Vec<&str> = config
        .auth
        .keys
        .iter()
        .map(|key| key.name.as_str())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "key-0", "key-1", "key-2", "key-3", "key-4", "key-5", "key-6", "key-7", "tester"
        ]
    );
    assert_eq!(config.providers.len(), 5);
    assert_eq!(config.streaming.keepalive_secs, 21);
    assert_eq!(config.routing.max_attempts, 5);
    assert_eq!(config.usage.retention_days, 9);
    assert_eq!(config.aliases.len(), 1);

    let reread = switchyard_config_store::validate_text(&app.file()).expect("a valid file");
    assert_eq!(reread, *config);
    assert!(app.file().contains("# The dashboard signs in with this."));
    // And the scheduler runs on exactly that.
    assert_eq!(*app.gateway.scheduler().config(), *config);
}
