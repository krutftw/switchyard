use crate::AppError;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../app-ui"]
#[include = "**/*.html"]
#[include = "**/*.js"]
#[include = "**/*.css"]
#[include = "**/*.svg"]
#[include = "**/*.woff2"]
#[exclude = "tests/**"]
#[exclude = "node_modules/**"]
#[exclude = ".*"]
#[exclude = "**/.*"]
struct AppAssets;

pub(crate) async fn serve(request: Request) -> Response {
    let path = request.uri().path();
    if path == "/api" || path.starts_with("/api/") {
        return AppError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "This API route does not exist.",
        )
        .into_response();
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return AppError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "This resource is read-only.",
        )
        .into_response();
    }
    let path = if path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };
    if request.uri().query().is_some() || !servable(path) {
        return AppError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "This app asset does not exist.",
        )
        .into_response();
    }
    let Some(file) = AppAssets::get(path) else {
        return AppError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "This app asset does not exist.",
        )
        .into_response();
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let body = if *request.method() == Method::HEAD {
        Body::empty()
    } else {
        Body::from(file.data.into_owned())
    };
    let mut response = Response::new(body);
    if let Ok(value) = mime.as_ref().parse() {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

fn servable(path: &str) -> bool {
    if path.len() > 256 || path.starts_with("tests/") || path.starts_with("node_modules/") {
        return false;
    }
    if !path.split('/').all(|segment| {
        !segment.is_empty()
            && !segment.starts_with('.')
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    }) {
        return false;
    }
    path == "index.html"
        || [".js", ".css", ".svg", ".woff2"]
            .iter()
            .any(|suffix| path.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn asset_whitelist_excludes_documents_tests_and_path_navigation() {
        for allowed in [
            "index.html",
            "app.js",
            "vendor/preact-htm.js",
            "fonts/archivo-latin-var.woff2",
        ] {
            assert!(servable(allowed));
        }
        for denied in [
            "../gateway.toml",
            "tests/diff.test.mjs",
            "DESIGN.md",
            "index.html/",
            "app%2ejs",
            "x\\app.js",
            ".env",
            "package.json",
        ] {
            assert!(!servable(denied));
        }
    }
}
