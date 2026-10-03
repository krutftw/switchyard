//! Stage-7 fixes of the configuration routes: `null` in a settings patch,
//! Validate agreeing with Save, one wording per secret problem, and issues
//! of prices and payload rules at the exact field.

mod support;

use http::StatusCode;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{App, BASE};
use switchyard_admin::AdminOptions;

fn issues(body: &Value) -> Vec<(String, String)> {
    body["error"]["issues"]
        .as_array()
        .or_else(|| body["issues"].as_array())
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

/// Regression (A2-1): `null` wrote the default into the file as an explicit
/// value, so a later change of the default never reached that file. It now
/// takes the key out, with the comments about it, and leaves the rest.
#[tokio::test]
async fn null_in_a_settings_patch_takes_the_setting_out_of_the_file() {
    let config = format!(
        "{BASE}\n[server]\nport = 8317\n\n[routing]\n# Keep caches warm.\nstrategy = \"fill-first\"\n\
         max_attempts = 5\n\n[routing.cooldown]\nauth_secs = 60\n"
    );
    let app = App::start_config(&config).await;

    let (status, body) = app
        .patch(
            "/settings",
            json!({
                "routing": {"strategy": null, "cooldown": null, "max_attempts": 4},
                "server": {"port": null},
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["config"]["routing"]["strategy"], "round-robin");
    assert_eq!(body["config"]["routing"]["cooldown"]["auth_secs"], 1800);
    assert_eq!(body["config"]["server"]["port"], 8317);
    assert_eq!(
        app.file(),
        format!("{BASE}\n[routing]\nmax_attempts = 4\n"),
        "the keys and the comment about `strategy` are gone, nothing else changed"
    );

    // The default spelled out in the file (`port = 8317`) went although
    // the configuration did not change; asking again writes nothing.
    let before = app.file();
    let (status, _) = app
        .patch("/settings", json!({"server": {"port": null}}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(app.file(), before);

    // `admin.secret: null` keeps the secret, like `""`.
    let (status, body) = app
        .patch("/settings", json!({"admin": {"secret": null}}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(app.file(), before);
    assert_eq!(app.get("/status").await.0, StatusCode::OK);
}

/// Regression (A2-5): Validate said "ok" for texts Save refused because
/// they would lock the dashboard out.
#[tokio::test]
async fn validate_reports_what_save_refuses() {
    let app = App::start().await;
    let texts = [
        "",
        "[server]\nport = 9000\n",
        "[admin]\nsecret = \"env:SWITCHYARD_TEST_NO_SUCH_VARIABLE\"\n",
        "[admin]\nenabled = false\nsecret = \"some-secret-value-123\"\n",
        "[admin]\nsecret = \"env:\"\n",
        "[admin]\nsecret = \"${}\"\n",
        "[admin]\nsecret = \" padded-secret-1234\"\n",
    ];
    for text in texts {
        let (status, verdict) = app.post("/config/validate", json!({"text": text})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(verdict["ok"], false, "{text:?}: {verdict}");
        let (status, refusal) = app.put("/config/raw", json!({"text": text})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text:?}");
        // The same issues, worded the same.
        assert_eq!(issues(&verdict), issues(&refusal), "{text:?}");
    }
    assert_eq!(app.file(), BASE);

    // The unset variable is named, and the reason is not "missing".
    let (_, verdict) = app
        .post(
            "/config/validate",
            json!({"text": "[admin]\nsecret = \"env:SWITCHYARD_TEST_NO_SUCH_VARIABLE\"\n"}),
        )
        .await;
    let (path, message) = &issues(&verdict)[0];
    assert_eq!(path, "admin.secret");
    assert!(
        message.contains("`SWITCHYARD_TEST_NO_SUCH_VARIABLE`, which is not set"),
        "{message}"
    );

    // With the secret coming from the environment, an empty text is fine
    // for both.
    let app = App::start_with(
        BASE,
        AdminOptions {
            secret_override: Some(support::SECRET.to_string()),
            ..AdminOptions::default()
        },
    )
    .await;
    let (_, verdict) = app.post("/config/validate", json!({"text": ""})).await;
    assert_eq!(verdict, json!({"ok": true, "issues": []}));
}

/// Regression (A2-5, A2-3, A2-4): one secret problem read differently on
/// each route ("is missing: saving this would leave…" from the settings
/// patch, "names no environment variable after `env:`" from the raw
/// editor), `${}` was described as `env:`, and a secret no header can carry
/// was accepted.
#[tokio::test]
async fn a_secret_problem_reads_the_same_on_every_route() {
    let app = App::start().await;
    for (secret, expected) in [
        ("env:", "names no environment variable after `env:`"),
        ("${ }", "names no environment variable between `${` and `}`"),
        ("env:SWITCHYARD_TEST_NO_SUCH_VARIABLE", "which is not set"),
        (
            "trailing-space-secret ",
            "must not start or end with a space",
        ),
        ("line\nbreak-secret-1234", "control characters"),
    ] {
        let (status, patched) = app
            .patch("/settings", json!({"admin": {"secret": secret}}))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{secret:?}: {patched}"
        );
        let text = format!("[admin]\nsecret = {}\n", json!(secret));
        let (status, raw) = app.put("/config/raw", json!({"text": text})).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{secret:?}: {raw}"
        );
        let (_, verdict) = app.post("/config/validate", json!({"text": text})).await;

        let patched = issues(&patched);
        assert_eq!(patched.len(), 1, "{secret:?}: {patched:?}");
        assert_eq!(patched[0].0, "admin.secret");
        assert!(patched[0].1.contains(expected), "{secret:?}: {patched:?}");
        assert_eq!(issues(&raw), patched, "{secret:?}");
        assert_eq!(issues(&verdict), patched, "{secret:?}");
    }
    assert_eq!(app.file(), BASE);
    assert_eq!(app.get("/status").await.0, StatusCode::OK);
}

/// Regression (A2-7): a bad price was reported at its row (`[1]`).
#[tokio::test]
async fn a_bad_price_is_reported_at_its_field() {
    let app = App::start().await;
    let (status, body) = app
        .put(
            "/pricing",
            json!([
                {"model": "a-*", "input": 1.0, "output": 2.0},
                {"model": "b-*", "input": -1.0, "output": 2.0, "cache_write": -0.5},
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(paths(&body), ["[1].input", "[1].cache_write"]);
    assert_eq!(app.file(), BASE);
}

/// Regression (A2-6): payload rule paths that can never address a field
/// were accepted (and silently matched nothing).
#[tokio::test]
async fn payload_rule_paths_are_checked_at_the_path() {
    let app = App::start().await;
    let (status, body) = app
        .put(
            "/payload",
            json!({
                "default": [{"models": ["*"], "set": {"temperature": 0.2, "a..b": 1, " top_p": 1}}],
                "override": [{"models": ["*"], "set": {"": 1}}],
                "filter": [{"models": ["*"], "remove": ["user", "metadata.", "has space"]}],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        paths(&body),
        [
            "default[0].set.a..b",
            "default[0].set. top_p",
            "override[0].set.",
            "filter[0].remove[1]",
            "filter[0].remove[2]",
        ]
    );
    let messages = issues(&body);
    assert!(messages[0].1.contains("empty part"), "{messages:?}");
    assert!(messages[2].1.starts_with("is empty"), "{messages:?}");
    assert_eq!(app.file(), BASE);

    // The grammar's own forms are accepted.
    let (status, body) = app
        .put(
            "/payload",
            json!({
                "override": [{"models": ["*"], "set": {"messages.0.role": "developer", "metadata.trace\\.id": "t"}}],
                "filter": [{"models": ["*"], "remove": ["generationConfig.topK"]}],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Regression (A2-8): `GET /payload` listed the fields of `set` sorted, not
/// in the order of the file, and saving rules kept that order for the
/// rules it left alone.
#[tokio::test]
async fn payload_fields_keep_the_order_of_the_file() {
    let config = format!(
        "{BASE}\n[[payload.override]]\nmodels = [\"*\"]\n\
         set = {{ \"zeta\" = 1, \"alpha\" = {{ \"y\" = 1, \"b\" = 2 }}, \"mid\" = 3 }}\n\n\
         [[payload.override]]\nmodels = [\"gpt-*\"]\nset = {{ \"top_p\" = 0.9, \"seed\" = 7 }}\n"
    );
    let app = App::start_config(&config).await;
    let keys = |rule: &Value| -> Vec<String> {
        rule["set"].as_object().unwrap().keys().cloned().collect()
    };
    let payload = app.get_ok("/payload").await;
    assert_eq!(keys(&payload["override"][0]), ["zeta", "alpha", "mid"]);
    assert_eq!(
        payload["override"][0]["set"]["alpha"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["y", "b"]
    );
    assert_eq!(keys(&payload["override"][1]), ["top_p", "seed"]);

    // Edit the second rule; send the first back sorted, as a client that
    // reorders keys would.
    let (status, body) = app
        .put(
            "/payload",
            json!({"override": [
                {"models": ["*"], "set": {"alpha": {"b": 2, "y": 1}, "mid": 3, "zeta": 1}},
                {"models": ["gpt-*"], "set": {"top_p": 0.5, "seed": 7}},
            ]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(keys(&body["override"][0]), ["zeta", "alpha", "mid"]);
    assert_eq!(
        app.file(),
        config.replace("\"top_p\" = 0.9", "\"top_p\" = 0.5"),
        "the untouched rule keeps its text"
    );
    let payload = app.get_ok("/payload").await;
    assert_eq!(keys(&payload["override"][0]), ["zeta", "alpha", "mid"]);
}
