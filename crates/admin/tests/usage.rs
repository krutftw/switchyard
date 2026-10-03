//! Usage statistics, the request list, captured bodies and the log.

mod support;

use http::StatusCode;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{App, BASE, CLIENT_KEY};
use switchyard_core::util::now_unix_ms;
use switchyard_telemetry::LogLine;

/// [`BASE`] with bodies captured for every request and a price for the
/// mock models.
fn with_capture() -> String {
    format!(
        "{BASE}\n[logging]\nrequest_log = \"all\"\n\n[[pricing]]\nmodel = \"mock-*\"\ninput = 1.0\noutput = 2.0\n"
    )
}

/// Four requests, far enough apart to have distinct start times (the list
/// is ordered by them).
async fn traffic(app: &App) {
    let pause = || tokio::time::sleep(std::time::Duration::from_millis(3));
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    pause().await;
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    pause().await;
    assert_eq!(app.chat(CLIENT_KEY, "mock-lorem").await, 200);
    pause().await;
    assert!(app.chat(CLIENT_KEY, "mock-error-500").await >= 500);
}

#[tokio::test]
async fn the_summary_adds_up() {
    let app = App::start_config(&with_capture()).await;
    traffic(&app).await;

    let summary = app.get_ok("/usage/summary").await;
    assert_eq!(summary["range"], "24h");
    assert_eq!(summary["totals"]["requests"], 4);
    assert_eq!(summary["totals"]["errors"], 1);
    assert!(summary["totals"]["output_tokens"].as_u64().unwrap() > 0);
    assert!(summary["totals"]["cost"].as_f64().unwrap() > 0.0);
    assert_eq!(summary["error_rate"], 0.25);
    assert_eq!(summary["requests_per_minute"], 4);
    assert_eq!(summary["latency"]["samples"], 4);
    let by_model: Vec<(&str, u64)> = summary["by_model"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["name"].as_str().unwrap(),
                entry["requests"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        by_model,
        [("mock-echo", 2), ("mock-error-500", 1), ("mock-lorem", 1)]
    );
    assert_eq!(summary["by_provider"][0]["name"], "mock");
    assert_eq!(summary["by_provider"][0]["requests"], 4);
    assert_eq!(summary["by_key"][0]["name"], "tester");

    // Every documented range is accepted; unknown parameter names are ignored.
    for range in ["1h", "24h", "7d", "30d"] {
        let summary = app.get_ok(&format!("/usage/summary?range={range}")).await;
        assert_eq!(summary["range"], range);
        assert_eq!(summary["totals"]["requests"], 4, "{range}");
    }
    let summary = app.get_ok("/usage/summary?unknown=1").await;
    assert_eq!(summary["range"], "24h");
}

#[tokio::test]
async fn the_timeseries_is_bucketed_and_grouped() {
    let app = App::start().await;
    traffic(&app).await;

    let series = app
        .get_ok("/usage/timeseries?range=1h&group_by=model")
        .await;
    assert_eq!(series["range"], "1h");
    assert_eq!(series["bucket"], "minute");
    assert_eq!(series["bucket_ms"], 60_000);
    assert_eq!(series["group_by"], "model");
    let mut names: Vec<&str> = series["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["mock-echo", "mock-error-500", "mock-lorem"]);
    let points = series["points"].as_array().unwrap();
    assert!(points.len() >= 60, "{}", points.len());
    let requests: u64 = points.iter().map(|p| p["requests"].as_u64().unwrap()).sum();
    assert_eq!(requests, 4);
    let busy = points.iter().rev().find(|p| p["requests"] != 0).unwrap();
    assert!(
        busy["groups"]["mock-echo"]["requests"].as_u64().unwrap() >= 1,
        "{busy}"
    );
    assert!(busy["t"].as_i64().unwrap() % 60_000 == 0);

    for (query, bucket, group_by) in [
        ("range=24h", "hour", "none"),
        ("range=7d&bucket=day&group_by=provider", "day", "provider"),
        ("range=30d&group_by=key", "day", "key"),
        ("range=24h&bucket=auto&group_by=none", "hour", "none"),
    ] {
        let series = app.get_ok(&format!("/usage/timeseries?{query}")).await;
        assert_eq!(series["bucket"], bucket, "{query}");
        assert_eq!(series["group_by"], group_by, "{query}");
    }
}

#[tokio::test]
async fn requests_are_listed_filtered_and_paged() {
    let app = App::start_config(&with_capture()).await;
    traffic(&app).await;

    let page = app.get_ok("/requests").await;
    assert_eq!(page["total"], 4);
    assert_eq!(page["has_more"], false);
    assert_eq!(page["next_before"], Value::Null);
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    // Newest first.
    assert_eq!(items[0]["requested_model"], "mock-error-500");
    assert_eq!(items[0]["ok"], false);
    assert_eq!(items[3]["requested_model"], "mock-echo");
    let record = &items[3];
    assert_eq!(record["client"]["key_name"], "tester");
    assert_eq!(
        record["client"]["key_id"],
        switchyard_config_store::client_key_id(CLIENT_KEY)
    );
    assert_eq!(record["client_protocol"], "openai-chat");
    assert_eq!(record["endpoint"], "POST /v1/chat/completions");
    assert_eq!(record["provider"], "mock");
    assert_eq!(record["status"], 200);
    assert!(record["cost"].as_f64().unwrap() > 0.0);
    assert_eq!(record["attempts"].as_array().unwrap().len(), 1);

    // Filters.
    for (query, total) in [
        ("model=mock-echo", 2),
        ("status=error", 1),
        ("status=ok", 3),
        ("status=5xx", 1),
        ("provider=mock", 4),
        ("provider=other", 0),
        ("key=tester", 4),
        ("key=anonymous", 0),
        ("q=lorem", 1),
        ("status=success&limit=500", 3),
    ] {
        let page = app.get_ok(&format!("/requests?{query}")).await;
        assert_eq!(page["total"], total, "{query}");
    }

    // Paging with the cursor walks the list without gaps or repeats.
    let first = app.get_ok("/requests?limit=3").await;
    assert_eq!(first["items"].as_array().unwrap().len(), 3);
    assert_eq!(first["has_more"], true);
    let cursor = first["next_before"].as_str().unwrap();
    let second = app
        .get_ok(&format!("/requests?limit=3&before={cursor}"))
        .await;
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert_eq!(second["has_more"], false);
    assert_eq!(second["items"][0]["id"], items[3]["id"]);
}

#[tokio::test]
async fn a_request_is_shown_with_its_captured_bodies() {
    let app = App::start_config(&with_capture()).await;
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    let id = app.get_ok("/requests").await["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Bodies are written in the background.
    app.gateway.telemetry().flush().await.unwrap();
    let detail = app.get_ok(&format!("/requests/{id}")).await;
    assert_eq!(detail["record"]["id"], id.as_str());
    assert_eq!(detail["record"]["has_bodies"], true);
    let bodies = &detail["bodies"];
    assert!(
        bodies["client_request"]
            .as_str()
            .unwrap()
            .contains("hello there"),
        "{bodies}"
    );
    assert!(bodies["client_response"].is_string(), "{bodies}");
    assert!(bodies["client_headers"].is_object(), "{bodies}");

    let (status, body) = app
        .get("/requests/0190aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _) = app.get("/requests/..%2F..%2Fswitchyard.toml").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_request_without_capture_has_no_bodies() {
    let app = App::start().await;
    assert_eq!(app.chat(CLIENT_KEY, "mock-echo").await, 200);
    let id = app.get_ok("/requests").await["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let detail = app.get_ok(&format!("/requests/{id}")).await;
    assert_eq!(detail["record"]["has_bodies"], false);
    assert_eq!(detail["bodies"], Value::Null);
}

#[tokio::test]
async fn usage_can_be_cleared() {
    let app = App::start().await;
    traffic(&app).await;
    assert_eq!(app.get_ok("/usage/summary").await["totals"]["requests"], 4);

    let (status, body) = app.delete("/usage").await;
    assert_eq!((status, body), (StatusCode::OK, json!({"ok": true})));
    assert_eq!(app.get_ok("/usage/summary").await["totals"]["requests"], 0);
    assert_eq!(app.get_ok("/requests").await["total"], 0);
    assert_eq!(app.get_ok("/keys").await[0]["usage"]["requests"], 0);
    // The totals since start are a different counter and stay.
    assert_eq!(app.get_ok("/status").await["live"]["totals"]["requests"], 4);
}

#[tokio::test]
async fn logs_are_paged_and_filtered() {
    let app = App::start().await;
    let logs = app.gateway.telemetry().logs();
    logs.clear();
    let now = now_unix_ms();
    for (level, message) in [
        ("info", "gateway started"),
        ("debug", "picked credential"),
        ("warn", "credential cooling down"),
        ("error", "upstream connect error"),
        ("info", "request finished"),
    ] {
        logs.push(
            LogLine::new(now, level, "switchyard::test", message).with_field("request_id", "r-1"),
        );
    }

    let page = app.get_ok("/logs").await;
    let lines = page["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 5);
    assert_eq!(page["has_more"], false);
    // Oldest first.
    assert_eq!(lines[0]["message"], "gateway started");
    assert_eq!(lines[4]["message"], "request finished");
    assert_eq!(lines[0]["level"], "info");
    assert_eq!(lines[0]["target"], "switchyard::test");
    assert_eq!(lines[0]["fields"]["request_id"], "r-1");
    assert!(lines[0]["seq"].as_u64().unwrap() < lines[4]["seq"].as_u64().unwrap());

    let warn = app.get_ok("/logs?level=warn").await;
    let messages: Vec<&str> = warn["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line["message"].as_str().unwrap())
        .collect();
    assert_eq!(
        messages,
        ["credential cooling down", "upstream connect error"]
    );

    let found = app.get_ok("/logs?q=CONNECT").await;
    assert_eq!(found["lines"].as_array().unwrap().len(), 1);

    // The newest two, then the two before them.
    let newest = app.get_ok("/logs?limit=2").await;
    assert_eq!(newest["lines"][1]["message"], "request finished");
    assert_eq!(newest["has_more"], true);
    let cursor = newest["next_before"].as_u64().unwrap();
    let older = app.get_ok(&format!("/logs?limit=2&before={cursor}")).await;
    let messages: Vec<&str> = older["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line["message"].as_str().unwrap())
        .collect();
    assert_eq!(messages, ["picked credential", "credential cooling down"]);

    // Unknown parameter names are still forward-compatible.
    let page = app.get_ok("/logs?unknown=value").await;
    assert_eq!(page["lines"].as_array().unwrap().len(), 5);
}
