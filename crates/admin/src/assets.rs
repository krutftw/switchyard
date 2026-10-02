//! The dashboard's static files, embedded in the binary and served under
//! `/admin/`.
//!
//! Release builds carry the files; debug builds read them from `ui/` on
//! every request, so a browser reload shows an edit without rebuilding.
//!
//! The files need no authentication — the sign-in page has to load — and
//! are served to any peer while the admin interface is enabled (they hold
//! no data; the API behind them is what is restricted). They are *not*
//! served when the admin interface is off, has no secret, or `admin.ui` is
//! false: then every path answers 404.
//!
//! The dashboard uses hash routing (`/admin/#/providers`), so there is no
//! fallback to `index.html`: a path that is not a file is a 404.

use crate::Shared;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response;
use http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, HeaderValue, IF_NONE_MATCH,
    LOCATION, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use http::{HeaderMap, StatusCode};
use rust_embed::RustEmbed;

/// The dashboard sources. Tests, notes, the Node manifest and dotfiles are
/// development material and stay out of the binary.
#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../ui"]
#[exclude = "tests/**"]
#[exclude = "node_modules/**"]
#[exclude = "*.md"]
#[exclude = "package.json"]
#[exclude = "package-lock.json"]
#[exclude = ".*"]
#[exclude = "**/.*"]
struct Dashboard;

/// Everything comes from this origin; inline styles are allowed because the
/// component kit positions elements with `style` attributes; the live-event
/// WebSocket is the one connection that is not plain same-origin HTTP.
const CONTENT_SECURITY_POLICY_VALUE: &str = "default-src 'self'; script-src 'self'; \
     style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; \
     connect-src 'self' ws: wss:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

fn security_headers(headers: &mut HeaderMap) {
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY_VALUE),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
}

fn respond(status: StatusCode, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    security_headers(response.headers_mut());
    response
}

fn not_found() -> Response {
    respond(StatusCode::NOT_FOUND, Body::from("Not found"))
}

/// Whether a path may name a dashboard file at all. Independent of what
/// the embedding excluded, and stricter: debug builds read the real
/// directory, where `tests/`, notes and dotfiles do exist and where the
/// file system would happily resolve `..`, another letter case, or a
/// Windows spelling such as `package.json.` to them.
fn servable(path: &str) -> bool {
    if path.is_empty() || path.len() > 512 {
        return false;
    }
    let plain = |segment: &str| {
        !segment.is_empty()
            && !segment.starts_with('.')
            && !segment.ends_with('.')
            && segment
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    };
    if !path.split('/').all(plain) {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    let first = lower.split('/').next().unwrap_or_default();
    let name = lower.rsplit('/').next().unwrap_or_default();
    !(matches!(first, "tests" | "node_modules")
        || name.ends_with(".md")
        || matches!(name, "package.json" | "package-lock.json"))
}

/// The `Content-Type` of a dashboard file. Spelled out for the types the
/// dashboard uses, because browsers refuse a module script or a stylesheet
/// served under the wrong one.
fn content_type(path: &str) -> HeaderValue {
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let known = match extension.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "",
    };
    if !known.is_empty() {
        return HeaderValue::from_static(known);
    }
    mime_guess::from_path(path)
        .first_raw()
        .map(HeaderValue::from_static)
        .unwrap_or(HeaderValue::from_static("application/octet-stream"))
}

/// A strong validator: the first half of the file's SHA-256.
fn etag(hash: &[u8; 32]) -> String {
    let mut tag = String::with_capacity(34);
    tag.push('"');
    for byte in &hash[..16] {
        tag.push_str(&format!("{byte:02x}"));
    }
    tag.push('"');
    tag
}

/// Whether `If-None-Match` names the current version of the file.
fn matches_etag(headers: &HeaderMap, current: &str) -> bool {
    headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate.trim_start_matches("W/") == current)
}

/// Reads a dashboard file. Debug builds touch the file system here, so the
/// lookup runs off the async threads there.
async fn load(path: String) -> Option<rust_embed::EmbeddedFile> {
    if cfg!(debug_assertions) {
        tokio::task::spawn_blocking(move || Dashboard::get(&path))
            .await
            .ok()
            .flatten()
    } else {
        Dashboard::get(&path)
    }
}

async fn serve(state: &Shared, path: &str, request_headers: &HeaderMap) -> Response {
    if !state.access().serves_dashboard() {
        return not_found();
    }
    if !servable(path) {
        return not_found();
    }
    let Some(file) = load(path.to_string()).await else {
        return not_found();
    };
    let tag = etag(&file.metadata.sha256_hash());
    let fresh = matches_etag(request_headers, &tag);
    let mut response = if fresh {
        respond(StatusCode::NOT_MODIFIED, Body::empty())
    } else {
        // Embedded data is static and is not copied; debug builds hand
        // over what they just read.
        let body = match file.data {
            std::borrow::Cow::Borrowed(bytes) => Body::from(bytes),
            std::borrow::Cow::Owned(bytes) => Body::from(bytes),
        };
        let mut response = respond(StatusCode::OK, body);
        response
            .headers_mut()
            .insert(CONTENT_TYPE, content_type(path));
        response
    };
    let headers = response.headers_mut();
    if let Ok(tag) = HeaderValue::from_str(&tag) {
        headers.insert(ETAG, tag);
    }
    // Always revalidate: the files change with the binary, and the ETag
    // makes the check cheap.
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// `GET /admin` → `/admin/`. The location is relative, so the redirect
/// also works behind a reverse proxy that adds a path prefix.
pub(crate) async fn redirect(State(state): State<Shared>) -> Response {
    if !state.access().serves_dashboard() {
        return not_found();
    }
    let mut response = respond(StatusCode::PERMANENT_REDIRECT, Body::empty());
    response
        .headers_mut()
        .insert(LOCATION, HeaderValue::from_static("admin/"));
    response
}

/// `GET /admin/`.
pub(crate) async fn index(State(state): State<Shared>, headers: HeaderMap) -> Response {
    serve(&state, "index.html", &headers).await
}

/// `GET /admin/{path}`.
pub(crate) async fn file(
    State(state): State<Shared>,
    path: Result<Path<String>, axum::extract::rejection::PathRejection>,
    headers: HeaderMap,
) -> Response {
    match path {
        Ok(Path(path)) => serve(&state, &path, &headers).await,
        Err(_) => not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn development_material_is_never_servable() {
        for path in [
            "index.html",
            "favicon.svg",
            "js/app.js",
            "js/lib/api.js",
            "css/pages/login.css",
            "fonts/archivo-latin-var.woff2",
            "vendor/preact-htm.js",
            "vendor/LICENSES.txt",
        ] {
            assert!(servable(path), "{path}");
        }
        for path in [
            "",
            "tests/check.mjs",
            "TESTS/check.mjs",
            "tests",
            "UI_GUIDE.md",
            "ui_guide.MD",
            "js/notes.md",
            "package.json",
            "Package.JSON",
            "package.json.",
            "package.json::$DATA",
            "node_modules/x/index.js",
            ".gitignore",
            "js/.eslintrc.json",
            ".git/config",
            "js/../tests/check.mjs",
            "../Cargo.toml",
            "js//app.js",
            "/etc/passwd",
            "js\\app.js",
            "js/app.js\0",
            "js/app .js",
            "index.html/",
        ] {
            assert!(!servable(path), "{path:?}");
        }
        assert!(!servable(&"a/".repeat(300)));
    }

    #[test]
    fn excluded_files_are_not_embedded() {
        assert!(Dashboard::get("index.html").is_some());
        assert!(Dashboard::get("js/app.js").is_some());
        for excluded in ["tests/check.mjs", "UI_GUIDE.md", "package.json"] {
            assert!(Dashboard::get(excluded).is_none(), "{excluded}");
        }
        // Whatever is embedded passes the path filter too: the two agree.
        for name in Dashboard::iter() {
            assert!(servable(&name), "{name}");
        }
    }

    #[test]
    fn content_types_of_the_dashboards_files() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("js/app.js"), "text/javascript; charset=utf-8");
        assert_eq!(
            content_type("js/worker.MJS"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type("css/base.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("favicon.svg"), "image/svg+xml");
        assert_eq!(content_type("fonts/a.woff2"), "font/woff2");
        assert_eq!(content_type("data/routes.json"), "application/json");
        assert_eq!(
            content_type("vendor/LICENSES.txt"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(content_type("img/logo.webp"), "image/webp");
        assert_eq!(content_type("blob"), "application/octet-stream");
    }

    #[test]
    fn etags_match_strong_weak_and_wildcard() {
        let tag = etag(&[0xab; 32]);
        assert_eq!(tag, format!("\"{}\"", "ab".repeat(16)));
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
            headers
        };
        assert!(matches_etag(&with(&tag), &tag));
        assert!(matches_etag(&with(&format!("W/{tag}")), &tag));
        assert!(matches_etag(&with(&format!("\"other\", {tag}")), &tag));
        assert!(matches_etag(&with("*"), &tag));
        assert!(!matches_etag(&with("\"other\""), &tag));
        assert!(!matches_etag(&HeaderMap::new(), &tag));
    }

    #[test]
    fn the_policy_keeps_everything_on_this_origin() {
        let mut headers = HeaderMap::new();
        security_headers(&mut headers);
        assert_eq!(
            headers[CONTENT_SECURITY_POLICY],
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; font-src 'self'; connect-src 'self' ws: wss:; \
             frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
        );
        assert_eq!(headers[X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(headers[REFERRER_POLICY], "no-referrer");
        assert_eq!(headers[X_FRAME_OPTIONS], "DENY");
    }
}
