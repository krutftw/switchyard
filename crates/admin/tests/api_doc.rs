//! Keeps `API.md` honest: every example in it is a real exchange with a
//! gateway running the mock provider.
//!
//! `docs/API.template.md` holds the prose with `{{example:<name>}}`
//! placeholders; this test plays the scenario below, records each exchange
//! under a name and renders the template.
//!
//! * A normal run checks that the scenario still works, that template and
//!   scenario agree (every placeholder has an example, every example is
//!   used) and that `API.md` was generated from the current template.
//! * `SWITCHYARD_WRITE_API_DOC=1 cargo test -p switchyard-admin --test api_doc`
//!   rewrites `API.md`.
//!
//! Examples are real, with three cosmetic changes: the temporary directory
//! is shown as `/etc/switchyard`, the random port as `8317`, and long
//! lists are cut to their first entries (the prose says where).

mod support;

use futures::StreamExt;
use http::{Method, StatusCode};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use support::{App, Socket, frame_of, next_frame, send_json, subscribe};
use switchyard_admin::AdminOptions;
use switchyard_core::util::now_unix_ms;
use switchyard_telemetry::{Event, LogLine};

const TEMPLATE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/API.template.md");
const OUTPUT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/API.md");

const SECRET: &str = "s3cr3t-admin-secret-change-me-0001";
const CLIENT_KEY: &str = "sy-Zk3vTq8LmW2xYb7NcR5dHs9JfP4gAe6Uo1iKtQwE";
const VENDOR_KEY_1: &str = "sk-live-4f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c";
const VENDOR_KEY_2: &str = "sk-live-0a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d";

fn config() -> String {
    format!(
        r#"# Switchyard configuration.

[admin]
secret = "{SECRET}"

[upstream]
proxy = "direct"

[logging]
request_log = "all"

[[auth.keys]]
key = "{CLIENT_KEY}"
name = "laptop"

# The built-in fake models.
[[providers]]
name = "mock"
kind = "mock"

[[providers]]
name = "vendor"
kind = "openai-compat"
base_url = "http://127.0.0.1:9/v1"
discover = false
api_keys = ["{VENDOR_KEY_1}"]

[[providers.credentials]]
api_key = "{VENDOR_KEY_2}"
label = "team"
weight = 2

[[providers.models]]
id = "vendor-large"
alias = "large"

[[pricing]]
model = "mock-*"
input = 0.5
output = 1.5
"#
    )
}

/// How an example is trimmed for the page.
#[derive(Clone, Copy, Default)]
struct Trim {
    /// Arrays longer than this keep only their first entries.
    arrays: Option<usize>,
    /// Arrays nested deeper than this many arrays are left alone.
    depth: usize,
    /// Strings longer than this are cut (with an ellipsis).
    strings: Option<usize>,
}

impl Trim {
    const NONE: Trim = Trim {
        arrays: None,
        depth: usize::MAX,
        strings: None,
    };

    /// Cuts every array, however deep.
    const fn arrays(max: usize) -> Trim {
        Trim {
            arrays: Some(max),
            depth: usize::MAX,
            strings: None,
        }
    }

    /// Cuts only the outermost arrays.
    const fn outer(max: usize) -> Trim {
        Trim {
            arrays: Some(max),
            depth: 1,
            strings: None,
        }
    }

    const fn strings(max: usize) -> Trim {
        Trim {
            arrays: None,
            depth: usize::MAX,
            strings: Some(max),
        }
    }

    fn apply(self, value: &mut Value) {
        self.apply_at(value, 0);
    }

    fn apply_at(self, value: &mut Value, arrays_above: usize) {
        match value {
            Value::Array(items) => {
                if let Some(max) = self.arrays
                    && arrays_above < self.depth
                {
                    items.truncate(max);
                }
                items
                    .iter_mut()
                    .for_each(|item| self.apply_at(item, arrays_above + 1));
            }
            Value::Object(map) => map
                .values_mut()
                .for_each(|item| self.apply_at(item, arrays_above)),
            Value::String(text) => {
                if let Some(max) = self.strings
                    && text.chars().count() > max
                {
                    *text = text.chars().take(max).collect::<String>() + "…";
                }
            }
            _ => {}
        }
    }
}

struct Recorder {
    app: App,
    examples: BTreeMap<String, String>,
}

impl Recorder {
    /// Replaces what differs from run to run for no interesting reason.
    fn neutral(&self, text: &str) -> String {
        let dir = self.app.gateway.config_store().dir().display().to_string();
        let json_dir = dir.replace('\\', "\\\\");
        text.replace(&format!("{json_dir}\\\\"), "/etc/switchyard/")
            .replace(&format!("{dir}\\"), "/etc/switchyard/")
            .replace(&format!("{dir}/"), "/etc/switchyard/")
            .replace(&json_dir, "/etc/switchyard")
            .replace(&dir, "/etc/switchyard")
            .replace(&self.app.addr.to_string(), "127.0.0.1:8317")
    }

    fn pretty(&self, value: &Value) -> String {
        self.neutral(&serde_json::to_string_pretty(value).expect("JSON"))
    }

    fn record(&mut self, name: &str, text: String) {
        let previous = self.examples.insert(name.to_string(), text);
        assert!(previous.is_none(), "example `{name}` recorded twice");
    }

    /// One exchange with the admin API, rendered as request and response.
    #[allow(clippy::too_many_arguments)]
    async fn exchange(
        &mut self,
        name: &str,
        request: reqwest::RequestBuilder,
        line: &str,
        body: Option<&Value>,
        expect: StatusCode,
        trim: Trim,
        show_headers: &[&str],
    ) -> Value {
        let response = request.send().await.expect("a response");
        let status = response.status();
        let headers = response.headers().clone();
        let text = response.text().await.expect("a body");
        assert_eq!(status, expect, "{line}: {text}");

        let mut out = String::from("```http\n");
        out.push_str(line);
        out.push('\n');
        if let Some(body) = body {
            out.push('\n');
            out.push_str(&self.pretty(body));
            out.push('\n');
        }
        out.push_str("```\n\n```http\n");
        out.push_str(&format!(
            "HTTP/1.1 {} {}\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or_default()
        ));
        for name in show_headers {
            if let Some(value) = headers.get(*name) {
                out.push_str(&format!("{name}: {}\n", value.to_str().unwrap_or_default()));
            }
        }
        let parsed: Option<Value> = serde_json::from_str(&text).ok();
        match &parsed {
            Some(value) => {
                let mut shown = value.clone();
                trim.apply(&mut shown);
                out.push('\n');
                out.push_str(&self.pretty(&shown));
                out.push('\n');
            }
            None if !text.is_empty() => {
                out.push('\n');
                out.push_str(&self.neutral(text.trim_end()));
                out.push('\n');
            }
            None => {}
        }
        out.push_str("```");
        self.record(name, out);
        parsed.unwrap_or(Value::Null)
    }

    /// A signed-in call.
    async fn call(
        &mut self,
        name: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
        expect: StatusCode,
        trim: Trim,
    ) -> Value {
        let mut request = self.app.request(method.clone(), path);
        if let Some(body) = &body {
            request = request.json(body);
        }
        let line = format!("{method} /admin/api{path}");
        self.exchange(name, request, &line, body.as_ref(), expect, trim, &[])
            .await
    }

    async fn get(&mut self, name: &str, path: &str, trim: Trim) -> Value {
        self.call(name, Method::GET, path, None, StatusCode::OK, trim)
            .await
    }

    /// A frame of the live stream, rendered as JSON.
    fn frame(&mut self, name: &str, frame: &Value, trim: Trim) {
        let mut shown = frame.clone();
        trim.apply(&mut shown);
        let text = format!("```json\n{}\n```", self.pretty(&shown));
        self.record(name, text);
    }
}

/// An OpenAI-compatible upstream that answers its model listing with `429`
/// and `Retry-After: 17`.
async fn rate_limited_upstream() -> std::net::SocketAddr {
    use axum::response::IntoResponse;
    let app = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(|| async {
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", "17")],
                axum::Json(json!({"error": {
                    "message": "Rate limit reached for requests",
                    "type": "requests",
                    "code": "rate_limit_exceeded",
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

/// A log line as the capture layer produces it: stored in the buffer (which
/// numbers it) and announced on the bus.
fn log_event(app: &App, message: &str) -> Event {
    let line = LogLine::new(
        now_unix_ms(),
        "info",
        "switchyard_gateway::pipeline",
        message,
    )
    .with_field("model", "mock-echo");
    Event::Log(app.gateway.telemetry().logs().push(line))
}

async fn live_frames(recorder: &mut Recorder) {
    let app = &recorder.app;
    let ticket = app.ticket().await;
    let (mut socket, _): (Socket, _) = tokio_tungstenite::connect_async(app.ws_url(&ticket))
        .await
        .expect("the upgrade");
    let hello = next_frame(&mut socket).await;
    let (stats, _) = frame_of(&mut socket, "stats").await;

    send_json(&mut socket, json!({"type": "ping"})).await;
    let (pong, _) = frame_of(&mut socket, "pong").await;

    send_json(
        &mut socket,
        json!({"type": "subscribe", "topics": ["request.started", "request.finished", "log", "credential", "config.reloaded"]}),
    )
    .await;
    let (subscribed, _) = frame_of(&mut socket, "subscribed").await;

    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    let (started, _) = frame_of(&mut socket, "request.started").await;
    let (finished, _) = frame_of(&mut socket, "request.finished").await;

    assert!(app.chat(CLIENT_KEY, "mock-error-429").await >= 400);
    let (credential, _) = frame_of(&mut socket, "credential").await;
    frame_of(&mut socket, "request.finished").await;

    app.gateway
        .telemetry()
        .publish(log_event(app, "request finished"));
    let (log, _) = frame_of(&mut socket, "log").await;

    let (status, _) = app
        .patch("/settings", json!({"streaming": {"keepalive_secs": 20}}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (reloaded, _) = frame_of(&mut socket, "config.reloaded").await;

    // Fall behind on purpose: nothing below yields to the socket's task.
    subscribe(&mut socket, &["log"]).await;
    for i in 0..1500 {
        app.gateway
            .telemetry()
            .publish(log_event(app, &format!("line {i}")));
    }
    let (lagged, _) = frame_of(&mut socket, "lagged").await;
    drop(socket);

    recorder.frame("ws_hello", &hello, Trim::NONE);
    recorder.frame("ws_stats", &stats, Trim::NONE);
    recorder.frame("ws_pong", &pong, Trim::NONE);
    recorder.frame("ws_subscribed", &subscribed, Trim::NONE);
    recorder.frame("ws_request_started", &started, Trim::NONE);
    recorder.frame("ws_request_finished", &finished, Trim::NONE);
    recorder.frame("ws_credential", &credential, Trim::NONE);
    recorder.frame("ws_log", &log, Trim::NONE);
    recorder.frame("ws_config_reloaded", &reloaded, Trim::NONE);
    recorder.frame("ws_lagged", &lagged, Trim::NONE);
}

/// The playground's streaming answer, as the bytes on the wire.
async fn playground_stream(recorder: &mut Recorder) {
    let body = json!({
        "protocol": "anthropic",
        "model": "mock-echo",
        "stream": true,
        "body": {
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "Hello"}],
        },
    });
    let response = recorder
        .app
        .request(Method::POST, "/playground")
        .json(&body)
        .send()
        .await
        .expect("a response");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response.headers()["content-type"]
        .to_str()
        .unwrap()
        .to_string();
    let mut stream = response.bytes_stream();
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        text.push_str(&String::from_utf8_lossy(&chunk.expect("a chunk")));
    }
    let out = format!(
        "```http\nPOST /admin/api/playground\n\n{}\n```\n\n```http\nHTTP/1.1 200 OK\ncontent-type: {content_type}\n\n{}\n```",
        recorder.pretty(&body),
        text.trim_end()
    );
    recorder.record("playground_stream", out);
}

async fn scenario() -> Recorder {
    let app = App::start_with(
        &config(),
        AdminOptions {
            secret_override: None,
            allow_remote_override: false,
            listen: None,
        },
    )
    .await;
    let mut r = Recorder {
        app,
        examples: BTreeMap::new(),
    };
    r.app.secret = SECRET.to_string();
    let ok = StatusCode::OK;

    // --- Sign-in and errors -------------------------------------------------
    r.call(
        "login",
        Method::POST,
        "/login",
        Some(json!({})),
        ok,
        Trim::NONE,
    )
    .await;
    {
        let request = r.app.http.get(r.app.api("/status")).bearer_auth("wrong");
        r.exchange(
            "error_401",
            request,
            "GET /admin/api/status",
            None,
            StatusCode::UNAUTHORIZED,
            Trim::NONE,
            &["www-authenticate"],
        )
        .await;
        let request = r
            .app
            .http
            .get(r.app.api("/status"))
            .bearer_auth(SECRET)
            .header("x-forwarded-for", "203.0.113.7");
        r.exchange(
            "error_403",
            request,
            "GET /admin/api/status",
            None,
            StatusCode::FORBIDDEN,
            Trim::NONE,
            &[],
        )
        .await;
        // Five wrong secrets from another (fake) loopback address.
        for _ in 0..4 {
            let response = r
                .app
                .http
                .get(r.app.api("/status"))
                .header("x-test-peer", "127.0.0.77:5000")
                .bearer_auth("wrong")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let request = r
            .app
            .http
            .get(r.app.api("/status"))
            .header("x-test-peer", "127.0.0.77:5000")
            .bearer_auth("wrong");
        r.exchange(
            "error_429",
            request,
            "GET /admin/api/status",
            None,
            StatusCode::TOO_MANY_REQUESTS,
            Trim::NONE,
            &["retry-after"],
        )
        .await;
    }
    r.call(
        "error_404",
        Method::GET,
        "/nothing-here",
        None,
        StatusCode::NOT_FOUND,
        Trim::NONE,
    )
    .await;

    // --- Traffic, so the numbers below are not all zero ----------------------
    assert_eq!(r.app.chat(CLIENT_KEY, "mock-echo").await, 200);
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    assert_eq!(r.app.chat(CLIENT_KEY, "mock-think").await, 200);
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    assert!(r.app.chat(CLIENT_KEY, "mock-error-500").await >= 500);
    r.app.gateway.telemetry().flush().await.unwrap();
    let logs = r.app.gateway.telemetry().logs();
    logs.clear();
    for (level, target, message) in [
        (
            "info",
            "switchyard_gateway::gateway",
            "configuration applied",
        ),
        (
            "warn",
            "switchyard_gateway::failover",
            "upstream attempt failed",
        ),
        ("info", "switchyard_gateway::pipeline", "request finished"),
    ] {
        logs.push(
            LogLine::new(now_unix_ms(), level, target, message).with_field("provider", "mock"),
        );
    }

    // --- Status, tickets ----------------------------------------------------
    r.get("status", "/status", Trim::NONE).await;
    r.call(
        "ws_ticket",
        Method::POST,
        "/ws-ticket",
        Some(json!({})),
        ok,
        Trim::NONE,
    )
    .await;

    // --- Configuration --------------------------------------------------------
    r.get("config_get", "/config", Trim::NONE).await;
    let raw = r.get("config_raw_get", "/config/raw", Trim::NONE).await;
    let text = raw["text"].as_str().unwrap().to_string();
    r.call(
        "config_validate_ok",
        Method::POST,
        "/config/validate",
        Some(json!({"text": "[server]\nport = 9000\n"})),
        ok,
        Trim::NONE,
    )
    .await;
    r.call(
        "config_validate_bad",
        Method::POST,
        "/config/validate",
        Some(json!({"text": "[server]\nport = 0\n\n[[providers]]\nname = \"My Provider\"\nkind = \"openai-compat\"\n"})),
        ok,
        Trim::NONE,
    )
    .await;
    r.call(
        "config_raw_put_invalid",
        Method::PUT,
        "/config/raw",
        Some(json!({"text": "[routing]\nmax_attempts = 0\n"})),
        StatusCode::UNPROCESSABLE_ENTITY,
        Trim::NONE,
    )
    .await;
    r.call(
        "config_raw_put",
        Method::PUT,
        "/config/raw",
        Some(json!({"text": format!("{text}\n[routing]\nstrategy = \"fill-first\"\n")})),
        ok,
        Trim::arrays(1),
    )
    .await;
    r.call(
        "settings_patch",
        Method::PATCH,
        "/settings",
        Some(json!({
            "routing": {"strategy": "round-robin", "cooldown": {"transient_secs": 30}},
            "logging": {"level": "debug"},
            "server": {"port": 9000},
            "auth": {"required": true},
        })),
        ok,
        Trim::arrays(1),
    )
    .await;
    r.call(
        "settings_patch_refused",
        Method::PATCH,
        "/settings",
        Some(json!({"providers": [], "routing": {"stratgy": "fill-first"}})),
        StatusCode::BAD_REQUEST,
        Trim::NONE,
    )
    .await;
    r.call(
        "settings_patch_invalid",
        Method::PATCH,
        "/settings",
        Some(json!({"logging": {"level": "loud"}})),
        StatusCode::UNPROCESSABLE_ENTITY,
        Trim::NONE,
    )
    .await;
    r.call(
        "reload",
        Method::POST,
        "/reload",
        Some(json!({})),
        ok,
        Trim::arrays(1),
    )
    .await;

    // --- Providers ------------------------------------------------------------
    r.get("providers_list", "/providers", Trim::arrays(2)).await;
    r.get("provider_get_mock", "/providers/mock", Trim::NONE)
        .await;
    let vendor = r.get("provider_get", "/providers/vendor", Trim::NONE).await;
    r.call(
        "provider_create",
        Method::POST,
        "/providers",
        Some(json!({
            "name": "second-mock",
            "kind": "mock",
            "prefix": "lab",
        })),
        StatusCode::CREATED,
        Trim::arrays(3),
    )
    .await;
    r.call(
        "provider_create_conflict",
        Method::POST,
        "/providers",
        Some(json!({"name": "mock", "kind": "mock"})),
        StatusCode::CONFLICT,
        Trim::NONE,
    )
    .await;
    r.call(
        "provider_create_invalid",
        Method::POST,
        "/providers",
        Some(json!({"name": "Another One", "kind": "openai-compat"})),
        StatusCode::UNPROCESSABLE_ENTITY,
        Trim::NONE,
    )
    .await;
    let mut edited = vendor["config"].clone();
    edited["api_keys"] = json!([
        vendor["config"]["api_keys"][0],
        "sk-live-9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b"
    ]);
    edited["priority"] = json!(10);
    r.call(
        "provider_put",
        Method::PUT,
        "/providers/vendor",
        Some(edited),
        ok,
        Trim::NONE,
    )
    .await;
    r.call(
        "provider_delete",
        Method::DELETE,
        "/providers/second-mock",
        None,
        ok,
        Trim::NONE,
    )
    .await;
    r.call(
        "provider_test",
        Method::POST,
        "/providers/mock/test",
        Some(json!({"model": "mock-echo"})),
        ok,
        Trim::NONE,
    )
    .await;
    r.call(
        "provider_test_failed",
        Method::POST,
        "/providers/mock/test",
        Some(json!({"model": "mock-error-401"})),
        ok,
        Trim::NONE,
    )
    .await;
    r.call(
        "provider_discover",
        Method::POST,
        "/providers/mock/discover",
        Some(json!({})),
        ok,
        Trim::arrays(2),
    )
    .await;
    {
        // A provider whose upstream refuses to list its models, there for
        // this one example only.
        let upstream = rate_limited_upstream().await;
        let (status, body) = r
            .app
            .post(
                "/providers",
                json!({
                    "name": "limited",
                    "kind": "openai-compat",
                    "base_url": format!("http://{upstream}/v1"),
                    "discover": false,
                    "models": [{"id": "limited-large"}],
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let request = r
            .app
            .request(Method::POST, "/providers/limited/discover")
            .json(&json!({}));
        r.exchange(
            "provider_discover_failed",
            request,
            "POST /admin/api/providers/limited/discover",
            Some(&json!({})),
            StatusCode::BAD_GATEWAY,
            Trim::NONE,
            // Shown if it were there: a discover failure carries none.
            &["retry-after"],
        )
        .await;
        let (status, body) = r.app.delete("/providers/limited").await;
        assert_eq!(status, ok, "{body}");
    }

    let mock = r.app.get_ok("/providers/mock").await;
    let mock_credential = mock["credentials"][0]["id"].as_str().unwrap().to_string();
    let vendor = r.app.get_ok("/providers/vendor").await;
    let shorthand = vendor["credentials"][0]["id"].as_str().unwrap().to_string();
    r.call(
        "credential_reset",
        Method::POST,
        &format!("/credentials/{mock_credential}/reset"),
        Some(json!({})),
        ok,
        Trim::NONE,
    )
    .await;
    r.call(
        "credential_disable",
        Method::POST,
        &format!("/credentials/{shorthand}/disable"),
        Some(json!({})),
        ok,
        Trim::NONE,
    )
    .await;
    r.call(
        "credential_enable",
        Method::POST,
        &format!("/credentials/{shorthand}/enable"),
        Some(json!({})),
        ok,
        Trim::arrays(1),
    )
    .await;
    r.call(
        "credential_unknown",
        Method::POST,
        "/credentials/mock:000000000000/reset",
        Some(json!({})),
        StatusCode::NOT_FOUND,
        Trim::NONE,
    )
    .await;

    // --- Models ---------------------------------------------------------------
    r.call(
        "aliases_put",
        Method::PUT,
        "/aliases",
        Some(json!([
            {"name": "fast", "targets": ["mock-echo"]},
            {"name": "smart", "targets": ["mock-think(high)", "large"], "hide_targets": false},
        ])),
        ok,
        Trim::NONE,
    )
    .await;
    r.get("aliases_get", "/aliases", Trim::NONE).await;
    r.call(
        "aliases_put_invalid",
        Method::PUT,
        "/aliases",
        Some(json!([{"name": "loop", "targets": ["loop"]}])),
        StatusCode::UNPROCESSABLE_ENTITY,
        Trim::NONE,
    )
    .await;
    r.call(
        "payload_put",
        Method::PUT,
        "/payload",
        Some(json!({
            "default": [{"models": ["mock-*"], "set": {"temperature": 0.2}}],
            "override": [{"models": ["large"], "protocol": "openai-chat", "provider": "vendor", "set": {"reasoning_effort": "high"}}],
            "filter": [{"models": ["*"], "remove": ["metadata.trace_id"]}],
        })),
        ok,
        Trim::NONE,
    )
    .await;
    r.get("payload_get", "/payload", Trim::NONE).await;
    r.call(
        "payload_put_invalid",
        Method::PUT,
        "/payload",
        Some(json!({
            "override": [{"models": ["large"], "set": {"response_format": null}}],
        })),
        StatusCode::UNPROCESSABLE_ENTITY,
        Trim::NONE,
    )
    .await;
    r.call(
        "pricing_put",
        Method::PUT,
        "/pricing",
        Some(json!([
            {"model": "mock-*", "input": 0.5, "output": 1.5},
            {"model": "vendor-large", "input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75},
        ])),
        ok,
        Trim::NONE,
    )
    .await;
    r.get("pricing_get", "/pricing", Trim::NONE).await;
    r.get("models", "/models", Trim::outer(3)).await;
    r.get("catalog", "/catalog", Trim::outer(2)).await;

    // --- Keys -----------------------------------------------------------------
    let created = r
        .call(
            "key_create",
            Method::POST,
            "/keys",
            Some(json!({"name": "ci", "models": ["mock-*"], "rate_limit_rpm": 120})),
            StatusCode::CREATED,
            Trim::NONE,
        )
        .await;
    let key_id = created["id"].as_str().unwrap().to_string();
    r.call(
        "key_create_conflict",
        Method::POST,
        "/keys",
        Some(json!({"name": "ci"})),
        StatusCode::CONFLICT,
        Trim::NONE,
    )
    .await;
    r.get("keys_list", "/keys", Trim::NONE).await;
    r.call(
        "key_patch",
        Method::PATCH,
        &format!("/keys/{key_id}"),
        Some(json!({"name": "ci-runner", "rate_limit_rpm": null, "enabled": false})),
        ok,
        Trim::NONE,
    )
    .await;
    r.call(
        "key_reveal",
        Method::POST,
        &format!("/keys/{key_id}/reveal"),
        Some(json!({})),
        ok,
        Trim::NONE,
    )
    .await;
    r.call(
        "key_delete",
        Method::DELETE,
        &format!("/keys/{key_id}"),
        None,
        ok,
        Trim::NONE,
    )
    .await;

    // --- Playground -----------------------------------------------------------
    r.call(
        "playground_chat",
        Method::POST,
        "/playground",
        Some(json!({
            "protocol": "openai-chat",
            "model": "mock-echo",
            "body": {"messages": [{"role": "user", "content": "Hello"}]},
        })),
        ok,
        Trim::NONE,
    )
    .await;
    r.call(
        "playground_gemini",
        Method::POST,
        "/playground",
        Some(json!({
            "protocol": "gemini",
            "model": "mock-echo",
            "body": {"contents": [{"role": "user", "parts": [{"text": "Hello"}]}]},
        })),
        ok,
        Trim::NONE,
    )
    .await;
    r.call(
        "playground_error",
        Method::POST,
        "/playground",
        Some(json!({
            "protocol": "anthropic",
            "model": "no-such-model",
            "body": {"max_tokens": 16, "messages": [{"role": "user", "content": "Hello"}]},
        })),
        StatusCode::NOT_FOUND,
        Trim::NONE,
    )
    .await;
    r.call(
        "playground_bad_envelope",
        Method::POST,
        "/playground",
        Some(json!({"protocol": "gemini", "body": {"contents": []}})),
        StatusCode::BAD_REQUEST,
        Trim::NONE,
    )
    .await;
    playground_stream(&mut r).await;
    r.app.gateway.telemetry().flush().await.unwrap();

    // --- Usage ----------------------------------------------------------------
    r.get("usage_summary", "/usage/summary?range=24h", Trim::NONE)
        .await;
    {
        // The interesting buckets are the last ones: show those.
        let series = r
            .app
            .get_ok("/usage/timeseries?range=1h&bucket=minute&group_by=model")
            .await;
        let mut shown = series.clone();
        let points = shown["points"].as_array_mut().unwrap();
        let keep = points.len().saturating_sub(2);
        points.drain(..keep);
        let text = format!(
            "```http\nGET /admin/api/usage/timeseries?range=1h&bucket=minute&group_by=model\n```\n\n```http\nHTTP/1.1 200 OK\n\n{}\n```",
            r.pretty(&shown)
        );
        r.record("usage_timeseries", text);
    }
    let page = r
        .get("requests_list", "/requests?limit=2&status=ok", Trim::NONE)
        .await;
    let failed = r
        .app
        .get_ok("/requests?status=error&model=mock-error-500")
        .await;
    let failed_id = failed["items"][0]["id"].as_str().unwrap().to_string();
    r.get(
        "request_detail",
        &format!("/requests/{failed_id}"),
        Trim::strings(400),
    )
    .await;
    assert!(
        page["items"]
            .as_array()
            .is_some_and(|items| items.len() == 2)
    );
    r.get("logs", "/logs?limit=3", Trim::NONE).await;

    // --- Live events ----------------------------------------------------------
    live_frames(&mut r).await;

    // --- Clearing, last -------------------------------------------------------
    r.call(
        "usage_delete",
        Method::DELETE,
        "/usage",
        None,
        ok,
        Trim::NONE,
    )
    .await;
    r
}

fn placeholders(template: &str) -> Vec<String> {
    template
        .split("{{example:")
        .skip(1)
        .filter_map(|rest| rest.split_once("}}"))
        .map(|(name, _)| name.to_string())
        .collect()
}

fn render(template: &str, examples: &BTreeMap<String, String>) -> String {
    let mut out = template.replace("\r\n", "\n");
    for (name, text) in examples {
        out = out.replace(&format!("{{{{example:{name}}}}}"), text);
    }
    out
}

#[tokio::test]
async fn api_md_is_generated_from_real_exchanges() {
    let recorder = scenario().await;
    let template = std::fs::read_to_string(TEMPLATE).expect("docs/API.template.md");

    // Template and scenario agree.
    let wanted: BTreeSet<String> = placeholders(&template).into_iter().collect();
    let recorded: BTreeSet<String> = recorder.examples.keys().cloned().collect();
    let missing: Vec<&String> = wanted.difference(&recorded).collect();
    let unused: Vec<&String> = recorded.difference(&wanted).collect();
    assert!(
        missing.is_empty(),
        "the template uses examples nobody records: {missing:?}"
    );
    assert!(
        unused.is_empty(),
        "examples the template does not use: {unused:?}"
    );

    let rendered = render(&template, &recorder.examples);
    assert!(
        !rendered.contains("{{example:"),
        "an unresolved placeholder remains"
    );
    // The examples must never teach anyone a secret that was not asked for.
    for secret in [SECRET, VENDOR_KEY_1, VENDOR_KEY_2] {
        let shown = rendered.matches(secret).count();
        // The raw file is the one place the configured secrets appear:
        // read once, written back once.
        assert!(
            shown <= 2,
            "{secret} appears {shown} times in the rendered page"
        );
    }

    if std::env::var_os("SWITCHYARD_WRITE_API_DOC").is_some() {
        std::fs::write(OUTPUT, &rendered).expect("API.md is writable");
        return;
    }

    // `API.md` was generated from this template: all of the template's own
    // lines are in it, in order.
    let current = std::fs::read_to_string(OUTPUT)
        .expect("API.md exists; generate it with SWITCHYARD_WRITE_API_DOC=1")
        .replace("\r\n", "\n");
    let mut rest = current.as_str();
    for line in template.replace("\r\n", "\n").lines() {
        if line.contains("{{example:") || line.trim().is_empty() {
            continue;
        }
        match rest.find(line) {
            Some(at) => rest = &rest[at + line.len()..],
            None => panic!(
                "API.md is out of date (missing `{line}`); regenerate it with \
                 SWITCHYARD_WRITE_API_DOC=1 cargo test -p switchyard-admin --test api_doc"
            ),
        }
    }
}
