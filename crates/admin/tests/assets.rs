//! The embedded dashboard: what is served, how, and what is not.

mod support;

use http::{HeaderMap, StatusCode};
use pretty_assertions::assert_eq;
use serde_json::json;
use support::{App, BASE};

const POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                      img-src 'self' data:; font-src 'self'; connect-src 'self' ws: wss:; \
                      frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

fn assert_security_headers(headers: &HeaderMap, what: &str) {
    assert_eq!(headers["content-security-policy"], POLICY, "{what}");
    assert_eq!(headers["x-content-type-options"], "nosniff", "{what}");
    assert_eq!(headers["referrer-policy"], "no-referrer", "{what}");
    assert_eq!(headers["x-frame-options"], "DENY", "{what}");
}

#[tokio::test]
async fn the_dashboard_is_served_without_a_secret() {
    let app = App::start().await;

    // `/admin` → `/admin/`, relative so a path prefix in front survives.
    let response = app.http.get(app.url("/admin")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.headers()["location"], "admin/");
    let target = response.url().join("admin/").unwrap();
    assert_eq!(target.path(), "/admin/");

    let response = app.http.get(app.url("/admin/")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(headers["cache-control"], "no-cache");
    assert!(headers["etag"].to_str().unwrap().starts_with('"'));
    assert_security_headers(&headers, "index");
    let html = response.text().await.unwrap();
    assert!(html.to_lowercase().contains("<!doctype html"), "{html}");
    let on_disk =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../ui/index.html"))
            .unwrap();
    assert_eq!(html, on_disk);

    // `index.html` by name is the same file.
    let response = app
        .http
        .get(app.url("/admin/index.html"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["etag"], headers["etag"]);
}

#[tokio::test]
async fn files_have_the_right_types_and_validators() {
    let app = App::start().await;
    for (path, content_type) in [
        ("/admin/js/app.js", "text/javascript; charset=utf-8"),
        ("/admin/js/lib/api.js", "text/javascript; charset=utf-8"),
        (
            "/admin/vendor/preact-htm.js",
            "text/javascript; charset=utf-8",
        ),
        ("/admin/css/base.css", "text/css; charset=utf-8"),
        ("/admin/favicon.svg", "image/svg+xml"),
        ("/admin/fonts/archivo-latin-var.woff2", "font/woff2"),
        ("/admin/vendor/LICENSES.txt", "text/plain; charset=utf-8"),
    ] {
        let response = app.http.get(app.url(path)).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let headers = response.headers().clone();
        assert_eq!(headers["content-type"], content_type, "{path}");
        assert_eq!(headers["cache-control"], "no-cache", "{path}");
        assert_security_headers(&headers, path);
        let etag = headers["etag"].to_str().unwrap().to_string();
        let body = response.bytes().await.unwrap();
        assert!(!body.is_empty(), "{path}");

        // The validator makes the next load a 304 without a body.
        let again = app
            .http
            .get(app.url(path))
            .header("if-none-match", &etag)
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED, "{path}");
        assert_eq!(again.headers()["etag"], etag.as_str(), "{path}");
        assert_eq!(again.headers()["cache-control"], "no-cache", "{path}");
        assert_security_headers(again.headers(), path);
        assert!(again.bytes().await.unwrap().is_empty(), "{path}");

        // Another version's validator gets the file.
        let stale = app
            .http
            .get(app.url(path))
            .header("if-none-match", "\"0000\"")
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::OK, "{path}");

        // HEAD: the headers of a GET, no body.
        let head = app.http.head(app.url(path)).send().await.unwrap();
        assert_eq!(head.status(), StatusCode::OK, "{path}");
        assert_eq!(head.headers()["content-type"], content_type, "{path}");
        assert_eq!(head.headers()["etag"], etag.as_str(), "{path}");
    }
}

#[tokio::test]
async fn development_material_and_unknown_paths_are_404() {
    let app = App::start().await;
    for path in [
        // Excluded from the embedding.
        "/admin/tests/check.mjs",
        "/admin/tests/dom.mjs",
        "/admin/UI_GUIDE.md",
        "/admin/package.json",
        // Ways around the exclusion that a file system would allow.
        "/admin/TESTS/check.mjs",
        "/admin/js/..%2Ftests/check.mjs",
        "/admin/js/%2E%2E/tests/check.mjs",
        "/admin/package.json.",
        "/admin/ui_guide.MD",
        // Out of the folder.
        "/admin/..%2F..%2FCargo.toml",
        "/admin/%2e%2e/%2e%2e/Cargo.toml",
        "/admin/..%5C..%5CCargo.toml",
        // Not files. Hash routing needs no fallback to the index.
        "/admin/js",
        "/admin/js/",
        "/admin/providers",
        "/admin/js/missing.js",
        "/admin/%00",
        "/admin/%FF",
    ] {
        let response = app.http.get(app.url(path)).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        let body = response.text().await.unwrap();
        assert!(!body.contains("workspace"), "{path}: {body}");
    }
    // Only GET and HEAD.
    let response = app.http.post(app.url("/admin/")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn the_dashboard_can_be_switched_off_while_the_api_stays() {
    let headless = BASE.replace("[admin]", "[admin]\nui = false");
    let app = App::start_config(&headless).await;
    for path in ["/admin", "/admin/", "/admin/js/app.js", "/admin/index.html"] {
        let response = app.http.get(app.url(path)).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(app.get("/status").await.0, StatusCode::OK);

    // Switched back on without a restart.
    let (status, body) = app.patch("/settings", json!({"admin": {"ui": true}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let response = app.http.get(app.url("/admin/")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn every_file_the_page_loads_is_served() {
    // The scripts and styles `index.html` refers to must all exist under
    // `/admin/`, or the dashboard is a blank page.
    let app = App::start().await;
    let html = app
        .http
        .get(app.url("/admin/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let mut references = Vec::new();
    for attribute in ["src=\"", "href=\""] {
        for piece in html.split(attribute).skip(1) {
            let target = piece.split('"').next().unwrap_or_default();
            let is_local = !target.is_empty()
                && !target.starts_with('#')
                && !target.contains("://")
                && !target.starts_with("data:");
            if is_local {
                references.push(target.trim_start_matches("./").to_string());
            }
        }
    }
    assert!(!references.is_empty(), "{html}");
    for reference in references {
        let url = app.url(&format!("/admin/{reference}"));
        let response = app.http.get(&url).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{reference}");
    }
}
