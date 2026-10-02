//! No literal secret leaves through the admin API, except where showing
//! one is the point: `GET /config/raw`, `POST /keys` and
//! `POST /keys/{id}/reveal`.

mod support;

use http::{Method, StatusCode};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use std::sync::Arc;
use support::{App, next_frame};
use switchyard_core::util::now_unix_ms;
use switchyard_telemetry::{Event, LogLine};

const ADMIN_SECRET: &str = "ADMINSECRET-aaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CLIENT_KEY_1: &str = "CLIENTKEYONE-bbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const CLIENT_KEY_2: &str = "CLIENTKEYTWO-cccccccccccccccccccccccccccc";
const SHORT_CLIENT_KEY: &str = "SHORTKEY7";
const API_KEY_1: &str = "APIKEYONE-dddddddddddddddddddddddddddddd";
const API_KEY_2: &str = "APIKEYTWO-eeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const CREDENTIAL_KEY: &str = "CREDENTIALKEY-ffffffffffffffffffffffffff";
const HEADER_SECRET: &str = "HEADERSECRET-gggggggggggggggggggggggggg";
const UPSTREAM_PROXY_PASSWORD: &str = "UPSTREAMPROXYPW-hhhhhhhhhhhhhhhhhhhh";
const PROVIDER_PROXY_PASSWORD: &str = "PROVIDERPROXYPW-iiiiiiiiiiiiiiiiiiii";
const CREDENTIAL_PROXY_PASSWORD: &str = "CREDENTIALPROXYPW-jjjjjjjjjjjjjjjj";
const BASE_URL_PASSWORD: &str = "BASEURLPW-kkkkkkkkkkkkkkkkkkkkkkkkkkkk";
const MOCK_HEADER_SECRET: &str = "MOCKHEADERSECRET-llllllllllllllllllll";

const SECRETS: [&str; 13] = [
    ADMIN_SECRET,
    CLIENT_KEY_1,
    CLIENT_KEY_2,
    SHORT_CLIENT_KEY,
    API_KEY_1,
    API_KEY_2,
    CREDENTIAL_KEY,
    HEADER_SECRET,
    UPSTREAM_PROXY_PASSWORD,
    PROVIDER_PROXY_PASSWORD,
    CREDENTIAL_PROXY_PASSWORD,
    BASE_URL_PASSWORD,
    MOCK_HEADER_SECRET,
];

/// A configuration with a distinctive secret in every place one can live.
/// The real-looking provider points at a closed local port and is never
/// called; the mock provider serves the traffic.
fn config() -> String {
    format!(
        r#"
[admin]
secret = "{ADMIN_SECRET}"

[upstream]
proxy = "http://corp:{UPSTREAM_PROXY_PASSWORD}@127.0.0.1:3128"

[logging]
request_log = "all"

[[auth.keys]]
key = "{CLIENT_KEY_1}"
name = "first"

[[auth.keys]]
key = "{CLIENT_KEY_2}"

[[auth.keys]]
key = "{SHORT_CLIENT_KEY}"
name = "short"
enabled = false

[[providers]]
name = "mock"
kind = "mock"

[providers.headers]
X-Mock-Token = "{MOCK_HEADER_SECRET}"

[[providers]]
name = "vendor"
kind = "openai-compat"
base_url = "http://user:{BASE_URL_PASSWORD}@127.0.0.1:9/v1"
discover = false
proxy = "socks5://corp:{PROVIDER_PROXY_PASSWORD}@127.0.0.1:1080"
api_keys = ["{API_KEY_1}", "{API_KEY_2}"]

[providers.headers]
Authorization = "Bearer {HEADER_SECRET}"

[[providers.credentials]]
api_key = "{CREDENTIAL_KEY}"
label = "team"
proxy = "http://corp:{CREDENTIAL_PROXY_PASSWORD}@127.0.0.1:3129"

[[providers.models]]
id = "vendor-model"

[[aliases]]
name = "fast"
targets = ["mock-echo"]

[payload]
[[payload.override]]
models = ["mock-*"]
[payload.override.set]
user = "gateway"

[[pricing]]
model = "mock-*"
input = 1.0
output = 2.0
"#
    )
}

fn assert_clean(what: &str, text: &str) {
    for secret in SECRETS {
        assert!(!text.contains(secret), "{what} leaks {secret}:\n{text}");
    }
}

async fn app() -> App {
    let mut app = App::start_config(&config()).await;
    app.secret = ADMIN_SECRET.to_string();
    app
}

/// Traffic that puts the client keys, and the failures of upstreams, into
/// records, bodies and logs.
async fn traffic(app: &App) {
    assert_eq!(app.chat(CLIENT_KEY_1, "mock-echo").await, 200);
    assert_eq!(app.chat(CLIENT_KEY_2, "fast").await, 200);
    assert!(app.chat(CLIENT_KEY_1, "mock-error-500").await >= 500);
    assert!(app.chat(CLIENT_KEY_1, "mock-error-401").await >= 400);
    let (status, _) = app
        .post(
            "/playground",
            json!({
                "protocol": "openai-chat",
                "model": "mock-echo",
                "body": {"messages": [{"role": "user", "content": "hi"}]},
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    app.gateway.telemetry().flush().await.unwrap();
    app.gateway.telemetry().logs().push(LogLine::new(
        now_unix_ms(),
        "warn",
        "switchyard::test",
        "credential cooling down",
    ));
}

#[tokio::test]
async fn no_get_endpoint_returns_a_literal_secret() {
    let app = app().await;
    traffic(&app).await;

    let request_ids: Vec<String> = app.get_ok("/requests").await["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(request_ids.len(), 5);

    // Every GET route of the admin API.
    let mut paths: Vec<String> = [
        "/status",
        "/config",
        "/providers",
        "/providers/mock",
        "/providers/vendor",
        "/models",
        "/catalog",
        "/aliases",
        "/payload",
        "/pricing",
        "/keys",
        "/usage/summary",
        "/usage/summary?range=30d",
        "/usage/timeseries",
        "/usage/timeseries?range=1h&group_by=model",
        "/usage/timeseries?range=1h&group_by=provider",
        "/usage/timeseries?range=1h&group_by=key",
        "/requests",
        "/requests?q=mock",
        "/logs",
    ]
    .iter()
    .map(|path| path.to_string())
    .collect();
    paths.extend(request_ids.iter().map(|id| format!("/requests/{id}")));

    for path in &paths {
        let response = app.request(Method::GET, path).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let text = response.text().await.unwrap();
        assert!(text.len() > 1, "{path}");
        assert_clean(path, &text);
    }

    // The check can fail: the raw file — the one GET that is meant to show
    // secrets — contains every one of them.
    let raw = app.get_ok("/config/raw").await["text"]
        .as_str()
        .unwrap()
        .to_string();
    for secret in SECRETS {
        assert!(raw.contains(secret), "{secret}");
    }
    // And what is shown instead is recognisably the masked form.
    let config = app.get_ok("/config").await;
    assert_eq!(config["config"]["admin"]["secret"], "ADMINS…aaaa");
    assert_eq!(config["config"]["auth"]["keys"][2]["key"], "••••••••");
    let vendor = app.get_ok("/providers/vendor").await;
    assert_eq!(vendor["api_keys"][0], "APIKEY…dddd");
    assert_eq!(vendor["credentials"][2]["masked_key"], "CREDEN…ffff");
    assert!(
        vendor["base_url"]
            .as_str()
            .unwrap()
            .starts_with("http://user:")
    );
    assert!(
        vendor["base_url"]
            .as_str()
            .unwrap()
            .ends_with("@127.0.0.1:9/v1")
    );
    assert!(
        vendor["credentials"][2]["proxy"]
            .as_str()
            .unwrap()
            .ends_with("@127.0.0.1:3129")
    );
}

#[tokio::test]
async fn mutations_and_errors_do_not_return_secrets_either() {
    let app = app().await;
    traffic(&app).await;

    let vendor = app.get_ok("/providers/vendor").await;
    let vendor_config = vendor["config"].clone();
    let credential = vendor["credentials"][0]["id"].as_str().unwrap().to_string();
    let first_key = app.get_ok("/keys").await[0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let calls: Vec<(Method, String, Value)> = vec![
        (Method::POST, "/login".into(), json!({})),
        (
            Method::PATCH,
            "/settings".into(),
            json!({"routing": {"max_attempts": 4}}),
        ),
        (
            Method::PATCH,
            "/settings".into(),
            json!({"server": {"port": 0}}),
        ),
        (Method::POST, "/reload".into(), json!({})),
        (
            Method::PUT,
            "/providers/vendor".into(),
            vendor_config.clone(),
        ),
        (Method::POST, "/providers".into(), vendor_config),
        (Method::POST, "/providers/mock/test".into(), json!({})),
        (
            Method::POST,
            "/providers/mock/test".into(),
            json!({"model": "mock-error-401"}),
        ),
        (Method::POST, "/providers/mock/discover".into(), json!({})),
        (
            Method::POST,
            format!("/credentials/{credential}/disable"),
            json!({}),
        ),
        (
            Method::POST,
            format!("/credentials/{credential}/enable"),
            json!({}),
        ),
        (
            Method::POST,
            format!("/credentials/{credential}/reset"),
            json!({}),
        ),
        (
            Method::PUT,
            "/aliases".into(),
            json!([{"name": "fast", "targets": ["mock-echo"]}]),
        ),
        (
            Method::PUT,
            "/aliases".into(),
            json!([{"name": "", "targets": []}]),
        ),
        (
            Method::PUT,
            "/pricing".into(),
            json!([{"model": "mock-*", "input": 1, "output": 2}]),
        ),
        (
            Method::PATCH,
            format!("/keys/{first_key}"),
            json!({"rate_limit_rpm": 100}),
        ),
        (
            Method::POST,
            "/keys".into(),
            json!({"name": "dup", "key": CLIENT_KEY_2}),
        ),
        (
            Method::POST,
            "/config/validate".into(),
            json!({"text": config()}),
        ),
        (
            Method::POST,
            "/config/validate".into(),
            // A key pasted where a number belongs must not be echoed.
            json!({"text": format!("[server]\nport = \"{API_KEY_1}\"\n")}),
        ),
        (
            Method::PUT,
            "/config/raw".into(),
            json!({"text": format!("{}\n[routing]\nmax_attempts = 0\n", config())}),
        ),
        (Method::PUT, "/config/raw".into(), json!({"text": config()})),
        (Method::DELETE, "/usage".into(), Value::Null),
    ];
    for (method, path, body) in calls {
        let what = format!("{method} {path}");
        let body = (!body.is_null()).then_some(body);
        let response = {
            let mut request = app.request(method, &path);
            if let Some(body) = body {
                request = request.json(&body);
            }
            request.send().await.unwrap()
        };
        let status = response.status();
        let text = response.text().await.unwrap();
        assert!(status.as_u16() < 500, "{what}: {status} {text}");
        assert_clean(&what, &text);
    }

    // The configuration still holds every secret: nothing was replaced by
    // its mask on the way through.
    let raw = app.get_ok("/config/raw").await["text"]
        .as_str()
        .unwrap()
        .to_string();
    for secret in SECRETS {
        assert!(raw.contains(secret), "{secret} was lost:\n{raw}");
    }
}

#[tokio::test]
async fn the_live_stream_carries_no_secrets() {
    let app = app().await;
    let (mut socket, hello) = app.live().await;
    let mut frames = vec![hello];
    traffic(&app).await;
    app.gateway
        .telemetry()
        .publish(Event::Log(Arc::new(LogLine::new(
            now_unix_ms(),
            "info",
            "switchyard::test",
            "end of traffic",
        ))));
    loop {
        let frame = next_frame(&mut socket).await;
        let done = frame["type"] == "log" && frame["data"]["message"] == "end of traffic";
        frames.push(frame);
        if done {
            break;
        }
    }
    let kinds: Vec<&str> = frames.iter().map(|f| f["type"].as_str().unwrap()).collect();
    for kind in ["request.started", "request.finished", "credential", "stats"] {
        assert!(kinds.contains(&kind), "{kinds:?}");
    }
    for frame in &frames {
        assert_clean("a live frame", &frame.to_string());
    }
}

#[tokio::test]
async fn only_the_key_endpoints_show_a_key() {
    let app = app().await;
    let keys = app.get_ok("/keys").await;
    let id = keys[0]["id"].as_str().unwrap();
    let (status, revealed) = app.post(&format!("/keys/{id}/reveal"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(revealed["key"], CLIENT_KEY_1);
    // Revealing needs the secret like everything else.
    let response = app
        .http
        .post(app.api(&format!("/keys/{id}/reveal")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_clean("the 401", &response.text().await.unwrap());

    let (status, created) = app.post("/keys", json!({"name": "new"})).await;
    assert_eq!(status, StatusCode::CREATED);
    let key = created["key"].as_str().unwrap();
    assert!(
        app.get_ok("/config/raw").await["text"]
            .as_str()
            .unwrap()
            .contains(key)
    );
    assert!(!app.get_ok("/keys").await.to_string().contains(key));
    assert!(!app.get_ok("/config").await.to_string().contains(key));
}
