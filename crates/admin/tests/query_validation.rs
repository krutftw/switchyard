//! Strict API query validation, independent of telemetry's internal defaults.

mod support;

use http::{Method, StatusCode};
use pretty_assertions::assert_eq;
use serde_json::Value;
use support::{App, BASE, CLIENT_KEY, read};
use switchyard_telemetry::LogLine;

fn named_error(status: StatusCode, body: &Value, parameter: &str) {
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["issues"][0]["path"], parameter, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&format!("`{parameter}`")),
        "{body}"
    );
}

#[tokio::test]
async fn malformed_typed_values_never_silently_change_the_query() {
    let app = App::start().await;
    for (path, parameter, values) in [
        (
            "/requests",
            "limit",
            &["many", "-1", "1.5", "18446744073709551616", ""][..],
        ),
        (
            "/requests",
            "status",
            &["bogus", "99", "600", "6xx", ""][..],
        ),
        (
            "/requests",
            "since",
            &["yesterday", "1.5", "9223372036854775808", ""][..],
        ),
        ("/requests", "before", &["garbage", ""][..]),
        ("/usage/summary", "range", &["forever", ""][..]),
        ("/usage/timeseries", "range", &["forever", ""][..]),
        ("/usage/timeseries", "bucket", &["second", ""][..]),
        ("/usage/timeseries", "group_by", &["endpoint", ""][..]),
        (
            "/logs",
            "limit",
            &["many", "-1", "1.5", "18446744073709551616", ""][..],
        ),
        ("/logs", "level", &["shouting", ""][..]),
        (
            "/logs",
            "before",
            &["x", "-1", "1.5", "18446744073709551616", ""][..],
        ),
    ] {
        for value in values {
            let (status, body) = app.get(&format!("{path}?{parameter}={value}")).await;
            named_error(status, &body, parameter);
        }
    }
    let (_, body) = app.get("/requests?status=private-marker-123").await;
    assert!(!body.to_string().contains("private-marker-123"));
}

#[tokio::test]
async fn query_checks_cover_routes_without_extractors_and_keep_auth_precedence() {
    let app = App::start().await;
    for (method, path, parameter) in [
        (Method::GET, "/status?x=1&x=2", "x"),
        (Method::GET, "/config?x=1&%78=2", "x"),
        (Method::POST, "/login?x=1&x=2", "x"),
        (Method::PATCH, "/settings?x=1&x=2", "x"),
        (Method::DELETE, "/usage?x=1&x=2", "x"),
        (Method::GET, "/requests/missing?x=1&x=2", "x"),
        (Method::GET, "/logs?q=%GG", "q"),
        (Method::GET, "/status?x=%FF", "x"),
        (Method::GET, "/requests?x=1&x=2", "x"),
    ] {
        let (status, body) = app.send(method, path, None).await;
        named_error(status, &body, parameter);
    }

    let path = "/status?x=1&x=2";
    let (status, _) = read(app.http.get(app.api(path))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = read(
        app.request(Method::GET, path)
            .header("x-test-peer", "203.0.113.7:40000"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let disabled = App::start_config(&BASE.replace("[admin]", "[admin]\nenabled = false")).await;
    assert_eq!(disabled.get(path).await.0, StatusCode::NOT_FOUND);

    // A malformed WebSocket query must not consume an otherwise valid ticket.
    let ticket = app.ticket().await;
    for (query, parameter) in [
        (format!("ticket={ticket}&ticket={ticket}"), "ticket"),
        (format!("ticket={ticket}&x=1&x=2"), "x"),
        (format!("ticket={ticket}&x=%FF"), "x"),
    ] {
        let (status, body) = app.get(&format!("/ws?{query}")).await;
        named_error(status, &body, parameter);
        assert!(!body.to_string().contains(&ticket));
    }
    assert_eq!(app.upgrade_status(&app.ws_url(&ticket), &[]).await, 101);
}

#[tokio::test]
async fn defaults_aliases_and_numeric_clamps_remain_valid() {
    let app = App::start().await;
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    assert_eq!(app.get_ok("/usage/summary").await["range"], "24h");
    let series = app
        .get_ok("/usage/timeseries?range=HOUR&bucket=min&group_by=client")
        .await;
    assert_eq!(series["range"], "1h");
    assert_eq!(series["bucket"], "minute");
    assert_eq!(series["group_by"], "key");
    for (query, count) in [
        ("", 2),
        ("?limit=0&status=SUCCESS", 1),
        ("?limit=99999&status=2XX&model=&q=", 2),
        ("?since=-1&unknown=value", 2),
    ] {
        let page = app.get_ok(&format!("/requests{query}")).await;
        assert_eq!(page["items"].as_array().unwrap().len(), count);
    }

    let logs = app.gateway.telemetry().logs();
    logs.clear();
    for _ in 0..205 {
        logs.push(LogLine::new(1, "warn", "switchyard::test", "query test"));
    }
    for (query, count) in [
        ("", 200),
        ("?limit=0&level=WARNING", 1),
        ("?limit=99999&before=18446744073709551615", 205),
        ("?before=0", 0),
        ("?before=1", 0),
    ] {
        let page = app.get_ok(&format!("/logs{query}")).await;
        assert_eq!(page["lines"].as_array().unwrap().len(), count);
    }
}
