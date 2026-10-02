//! Who gets in: availability, the secret, the lockout, the loopback rule
//! and the environment overrides.

mod support;

use http::{HeaderValue, Method, StatusCode};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{App, BASE, SECRET, read, read_with_headers};
use switchyard_admin::AdminOptions;

/// A peer on another machine.
const REMOTE_PEER: &str = "203.0.113.7:40000";

async fn status_of(app: &App, path: &str) -> StatusCode {
    app.get(path).await.0
}

// ---------------------------------------------------------------------------
// Availability
// ---------------------------------------------------------------------------

#[tokio::test]
async fn without_a_secret_the_admin_interface_does_not_exist() {
    let no_secret = r#"
[[providers]]
name = "mock"
kind = "mock"
"#;
    let app = App::start_config(no_secret).await;
    for path in ["/status", "/config", "/keys", "/no-such-route"] {
        let (status, body) = app.get(path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(body, json!({"error": {"message": "not found"}}), "{path}");
    }
    assert_eq!(app.post("/login", json!({})).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        app.post("/ws-ticket", json!({})).await.0,
        StatusCode::NOT_FOUND
    );
    // The WebSocket route and the dashboard are gone too.
    assert_eq!(app.upgrade_status(&app.ws_url("anything"), &[]).await, 404);
    for path in ["/admin", "/admin/", "/admin/js/app.js"] {
        let response = app.http.get(app.url(path)).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn a_disabled_admin_interface_does_not_exist_either() {
    let disabled = BASE.replace("[admin]", "[admin]\nenabled = false");
    let app = App::start_config(&disabled).await;
    assert_eq!(status_of(&app, "/status").await, StatusCode::NOT_FOUND);
    let response = app.http.get(app.url("/admin/")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Not even the environment's secret switches it back on.
    let app = App::start_with(
        &disabled,
        AdminOptions {
            secret_override: Some("from-the-environment".into()),
            ..AdminOptions::default()
        },
    )
    .await;
    assert_eq!(status_of(&app, "/status").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_reference_to_an_unset_variable_is_no_secret() {
    let unresolved = BASE.replace(
        "secret = \"test-admin-secret-0123456789abcdef\"",
        "secret = \"env:SWITCHYARD_ADMIN_TEST_VARIABLE_THAT_IS_NOT_SET\"",
    );
    let app = App::start_config(&unresolved).await;
    assert_eq!(status_of(&app, "/status").await, StatusCode::NOT_FOUND);
    // Presenting the reference text itself does not help.
    let (status, _) = read(
        app.http
            .get(app.api("/status"))
            .bearer_auth("env:SWITCHYARD_ADMIN_TEST_VARIABLE_THAT_IS_NOT_SET"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_secret_reference_is_resolved_when_used() {
    // Cargo sets this variable for the test process.
    let Ok(value) = std::env::var("CARGO_PKG_NAME") else {
        return;
    };
    let referenced = BASE.replace(
        "secret = \"test-admin-secret-0123456789abcdef\"",
        "secret = \"env:CARGO_PKG_NAME\"",
    );
    let app = App::start_config(&referenced).await;
    let (status, _) = read(app.http.get(app.api("/status")).bearer_auth(&value)).await;
    assert_eq!(status, StatusCode::OK);
    // The reference is a name, not the secret.
    let (status, _) = read(
        app.http
            .get(app.api("/status"))
            .bearer_auth("env:CARGO_PKG_NAME"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// The secret
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_secret_is_required_and_accepted_in_three_spellings() {
    let app = App::start().await;

    let (status, headers, body) = read_with_headers(app.http.get(app.api("/status"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body,
        json!({"error": {"message": "the admin secret is required"}})
    );
    assert_eq!(headers["www-authenticate"], "Bearer");
    assert_eq!(headers["cache-control"], "no-store");

    let (status, body) = read(
        app.http
            .get(app.api("/status"))
            .bearer_auth("not-the-secret"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body,
        json!({"error": {"message": "the admin secret is not correct"}})
    );

    // Unknown routes are refused the same way: nothing can be probed
    // without the secret.
    let (status, _) = read(app.http.get(app.api("/no-such-route"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let accepted = [
        app.http.get(app.api("/status")).bearer_auth(SECRET),
        app.http
            .get(app.api("/status"))
            .header("authorization", format!("bearer {SECRET}")),
        app.http
            .get(app.api("/status"))
            .header("authorization", SECRET),
        app.http
            .get(app.api("/status"))
            .header("x-admin-secret", SECRET),
        // A wrong Authorization next to the right x-admin-secret.
        app.http
            .get(app.api("/status"))
            .bearer_auth("stale")
            .header("x-admin-secret", SECRET),
    ];
    for request in accepted {
        let (status, body) = read(request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let (status, body) = app.post("/login", json!({})).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"ok": true})));

    // With the secret, an unknown route is a 404 and a wrong method a 405,
    // both in the admin error shape.
    let (status, body) = app.get("/no-such-route").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        json!({"error": {"message": "no such admin API route"}})
    );
    let (status, body) = app.send(Method::DELETE, "/status", None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(body["error"]["message"].is_string(), "{body}");
}

#[tokio::test]
async fn a_secret_that_is_not_ascii_is_compared_as_bytes() {
    let secret = "pässwörd-ключ-🔐";
    let config = BASE.replace("test-admin-secret-0123456789abcdef", secret);
    let app = App::start_config(&config).await;

    // What a browser sends: the UTF-8 bytes of the secret.
    let header = |text: &str| HeaderValue::from_bytes(format!("Bearer {text}").as_bytes()).unwrap();
    let (status, _) = read(
        app.http
            .get(app.api("/status"))
            .header("authorization", header(secret)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The same letters without their accents are a different secret.
    let (status, _) = read(
        app.http
            .get(app.api("/status"))
            .header("authorization", header("passwörd-ключ-🔐")),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Lockout
// ---------------------------------------------------------------------------

async fn wrong_secret(app: &App, peer: &str) -> (StatusCode, http::HeaderMap, Value) {
    read_with_headers(
        app.http
            .get(app.api("/status"))
            .header("x-test-peer", peer)
            .bearer_auth("definitely-wrong"),
    )
    .await
}

#[tokio::test]
async fn five_wrong_secrets_lock_the_address_out_for_thirty_minutes() {
    let app = App::start().await;
    let peer = "127.0.0.9:1000";

    for attempt in 1..=4 {
        let (status, _, _) = wrong_secret(&app, peer).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {attempt}");
    }
    let (status, headers, body) = wrong_secret(&app, peer).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers["retry-after"], "1800");
    assert_eq!(
        body["error"]["message"],
        "too many failed sign-in attempts from this address; try again in 30 minutes"
    );

    // Locked: even the right secret is refused, from any port of that
    // address, and the wait counts down.
    let (status, headers, _) = read_with_headers(
        app.http
            .get(app.api("/status"))
            .header("x-test-peer", "127.0.0.9:2000")
            .bearer_auth(SECRET),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let wait: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1790..=1800).contains(&wait), "{wait}");
    let (status, _) = read(
        app.http
            .post(app.api("/login"))
            .header("x-test-peer", peer)
            .bearer_auth(SECRET)
            .json(&json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Another address is not affected.
    let (status, _) = app.get("/status").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_success_resets_the_count_and_a_missing_secret_is_not_counted() {
    let app = App::start().await;
    let peer = "127.0.0.10:1000";
    let right = || {
        app.http
            .get(app.api("/status"))
            .header("x-test-peer", peer)
            .bearer_auth(SECRET)
    };

    for _ in 0..4 {
        assert_eq!(wrong_secret(&app, peer).await.0, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(read(right()).await.0, StatusCode::OK);
    // The slate is clean: four more failures are tolerated again.
    for _ in 0..4 {
        assert_eq!(wrong_secret(&app, peer).await.0, StatusCode::UNAUTHORIZED);
    }
    // Requests without any secret guess nothing and never lock.
    for _ in 0..10 {
        let (status, _) = read(app.http.get(app.api("/status")).header("x-test-peer", peer)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(read(right()).await.0, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// The loopback rule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remote_peers_are_refused_unless_allowed() {
    let app = App::start().await;

    let remote = || {
        app.http
            .get(app.api("/status"))
            .header("x-test-peer", REMOTE_PEER)
            .bearer_auth(SECRET)
    };
    let (status, body) = read(remote()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body,
        json!({"error": {"message": "remote admin access is disabled"}})
    );
    // Refused before the secret is looked at: wrong secrets from remote
    // peers do not even count.
    for _ in 0..6 {
        let (status, _, _) = wrong_secret(&app, REMOTE_PEER).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    // A header cannot make a remote peer local …
    let (status, _) = read(remote().header("x-forwarded-for", "127.0.0.1")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // … but it does give away a reverse proxy on this machine.
    for (name, value) in [
        ("x-forwarded-for", "198.51.100.4"),
        ("forwarded", "for=198.51.100.4;proto=https"),
        ("x-real-ip", "198.51.100.4"),
    ] {
        let (status, body) = read(
            app.http
                .get(app.api("/status"))
                .header(name, value)
                .bearer_auth(SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{name}: {body}");
    }
    // The WebSocket is under the same rule (a ticket bought locally does
    // not travel).
    let ticket = app.ticket().await;
    assert_eq!(
        app.upgrade_status(&app.ws_url(&ticket), &[("x-forwarded-for", "198.51.100.4")])
            .await,
        403
    );

    // The local peer is fine and is told so.
    let status = app.get_ok("/status").await;
    assert_eq!(
        status["admin"],
        json!({"allow_remote": false, "remote": false})
    );

    // The dashboard's files are not data: anyone who reaches the listener
    // may load the sign-in page.
    let response = app
        .http
        .get(app.url("/admin/"))
        .header("x-test-peer", REMOTE_PEER)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn allow_remote_admits_remote_peers() {
    let app = App::start().await;
    let remote = || {
        app.http
            .get(app.api("/status"))
            .header("x-test-peer", REMOTE_PEER)
            .header("x-forwarded-for", "198.51.100.4")
            .bearer_auth(SECRET)
    };
    assert_eq!(read(remote()).await.0, StatusCode::FORBIDDEN);

    // Switched on in the live configuration: no restart.
    let (status, body) = app
        .patch("/settings", json!({"admin": {"allow_remote": true}}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = read(remote()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["admin"], json!({"allow_remote": true, "remote": true}));
    // Remote peers still need the secret, and are locked out like anyone.
    let (status, _, _) = wrong_secret(&app, REMOTE_PEER).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Environment overrides
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_environment_overrides_secret_and_remote_access() {
    let options = AdminOptions {
        secret_override: Some("  secret-from-the-environment  ".into()),
        allow_remote_override: true,
        ..AdminOptions::default()
    };
    let app = App::start_with(BASE, options).await;

    // The file's secret no longer opens anything.
    let (status, _) = read(app.http.get(app.api("/status")).bearer_auth(SECRET)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = read(
        app.http
            .get(app.api("/status"))
            .header("x-test-peer", REMOTE_PEER)
            .bearer_auth("secret-from-the-environment"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["admin"], json!({"allow_remote": true, "remote": true}));
    assert_eq!(body["listen"], app.addr.to_string());

    // The override also stands in for a missing `admin.secret`.
    let no_secret = "[[providers]]\nname = \"mock\"\nkind = \"mock\"\n";
    let app = App::start_with(
        no_secret,
        AdminOptions {
            secret_override: Some("only-in-the-environment".into()),
            ..AdminOptions::default()
        },
    )
    .await;
    assert_eq!(app.get("/status").await.0, StatusCode::OK);
    // The masked configuration does not show it: it is not in the file.
    let config = app.get_ok("/config").await;
    assert_eq!(config["config"]["admin"]["secret"], "");
}

#[test]
fn options_default_to_no_override() {
    let options = AdminOptions::default();
    assert!(options.secret_override.is_none());
    assert!(!options.allow_remote_override);
    assert!(options.listen.is_none());
    // Reading the environment never panics, whatever it holds.
    let _ = AdminOptions::from_env();
}
