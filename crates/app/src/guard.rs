//! The local app's exact loopback-origin and host-session access boundary.

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::Arc;
use subtle::ConstantTimeEq;

/// Credentials are intentionally neither printable nor exposed outside this module.
pub(crate) struct AccessPolicy {
    authority: String,
    origin: String,
    token: String,
}

impl AccessPolicy {
    pub(crate) fn new(port: u16, token: String) -> Self {
        let authority = format!("127.0.0.1:{port}");
        Self {
            origin: format!("http://{authority}"),
            authority,
            token,
        }
    }
}

/// A header is usable only when it was supplied exactly once.
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a HeaderValue> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn denied(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

/// Do not let an inner handler grant cross-origin access accidentally.
fn without_cors(mut response: Response) -> Response {
    let names: Vec<_> = response
        .headers()
        .keys()
        .filter(|name| name.as_str().starts_with("access-control-"))
        .cloned()
        .collect();
    for name in names {
        response.headers_mut().remove(name);
    }
    response
}

pub(crate) async fn enforce(
    State(policy): State<Arc<AccessPolicy>>,
    req: Request,
    next: Next,
) -> Response {
    let headers = req.headers();
    let Some(host) = single_header(headers, header::HOST.as_str()) else {
        return denied(
            StatusCode::BAD_REQUEST,
            "invalid_host",
            "Exactly one Host header is required.",
        );
    };
    if host.as_bytes() != policy.authority.as_bytes() {
        return denied(
            StatusCode::FORBIDDEN,
            "host_not_allowed",
            "This host is not allowed.",
        );
    }

    if headers.contains_key(header::ORIGIN)
        && single_header(headers, header::ORIGIN.as_str())
            .is_none_or(|origin| origin.as_bytes() != policy.origin.as_bytes())
    {
        return denied(
            StatusCode::FORBIDDEN,
            "origin_not_allowed",
            "This origin is not allowed.",
        );
    }
    if headers.contains_key("sec-fetch-site")
        && !single_header(headers, "sec-fetch-site")
            .is_some_and(|site| matches!(site.as_bytes(), b"same-origin" | b"none"))
    {
        return denied(
            StatusCode::FORBIDDEN,
            "origin_not_allowed",
            "This request context is not allowed.",
        );
    }

    let path = req.uri().path();
    if path == "/api" || path.starts_with("/api/") {
        let authenticated = single_header(headers, header::AUTHORIZATION.as_str())
            .and_then(|authorization| authorization.as_bytes().strip_prefix(b"Bearer "))
            .is_some_and(|token| {
                !policy.token.is_empty() && bool::from(token.ct_eq(policy.token.as_bytes()))
            });
        if !authenticated {
            return denied(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "A valid host-session token is required.",
            );
        }
    }

    without_cors(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::middleware;
    use axum::routing::get;
    use serde_json::Value;
    use tower::ServiceExt;

    const AUTHORITY: &str = "127.0.0.1:18317";
    const ORIGIN: &str = "http://127.0.0.1:18317";
    const TOKEN: &str = "unit-test-session-token";
    const AUTHORIZATION: &str = "Bearer unit-test-session-token";

    async fn inner() -> impl IntoResponse {
        (
            [
                ("access-control-allow-origin", "*"),
                ("access-control-allow-credentials", "true"),
                ("access-control-allow-private-network", "true"),
                ("x-inner-handler", "reached"),
            ],
            "ok",
        )
    }

    fn app(token: &str) -> Router {
        Router::new()
            .route("/", get(inner))
            .route("/api", get(inner))
            .route("/api/status", get(inner))
            .route("/apiary", get(inner))
            .layer(middleware::from_fn_with_state(
                Arc::new(AccessPolicy::new(18317, token.to_string())),
                enforce,
            ))
    }

    async fn send(path: &str, headers: &[(&str, &str)]) -> Response {
        let mut request = Request::builder().uri(path);
        for &(name, value) in headers {
            request = request.header(name, value);
        }
        app(TOKEN)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn every_route_requires_one_exact_host() {
        for path in ["/", "/api", "/api/status"] {
            for headers in [
                vec![("authorization", AUTHORIZATION)],
                vec![("host", AUTHORITY), ("host", AUTHORITY)],
            ] {
                assert_eq!(send(path, &headers).await.status(), StatusCode::BAD_REQUEST);
            }
            for host in ["localhost:18317", "127.0.0.1", "127.0.0.1:18318"] {
                let response =
                    send(path, &[("host", host), ("authorization", AUTHORIZATION)]).await;
                assert_eq!(response.status(), StatusCode::FORBIDDEN);
            }
        }
    }

    #[tokio::test]
    async fn cli_and_exact_origin_requests_are_allowed() {
        for path in ["/api", "/api/status"] {
            let headers = [("host", AUTHORITY), ("authorization", AUTHORIZATION)];
            assert_eq!(send(path, &headers).await.status(), StatusCode::OK);
            let mut headers = headers.to_vec();
            headers.push(("origin", ORIGIN));
            headers.push(("sec-fetch-site", "same-origin"));
            assert_eq!(send(path, &headers).await.status(), StatusCode::OK);
        }
        for path in ["/", "/apiary"] {
            let response = send(path, &[("host", AUTHORITY), ("sec-fetch-site", "none")]).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn supplied_origins_must_be_single_and_exact() {
        for origin in ["null", "https://127.0.0.1:18317", "http://127.0.0.1:18318"] {
            let response = send("/", &[("host", AUTHORITY), ("origin", origin)]).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        let response = send(
            "/",
            &[("host", AUTHORITY), ("origin", ORIGIN), ("origin", ORIGIN)],
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn foreign_or_ambiguous_fetch_contexts_are_refused() {
        for site in ["cross-site", "same-site", "", "unknown"] {
            let response = send("/", &[("host", AUTHORITY), ("sec-fetch-site", site)]).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        let response = send(
            "/",
            &[
                ("host", AUTHORITY),
                ("sec-fetch-site", "same-origin"),
                ("sec-fetch-site", "none"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn api_requires_a_single_exact_bearer_header() {
        for path in ["/api", "/api/status"] {
            assert_eq!(
                send(path, &[("host", AUTHORITY)]).await.status(),
                StatusCode::UNAUTHORIZED
            );
            for authorization in [
                "Bearer wrong-session-token",
                "bearer unit-test-session-token",
                "Basic unit-test-session-token",
                "Bearer  unit-test-session-token",
                "Bearer unit-test-session-token ",
                "Bearer ",
            ] {
                let response = send(
                    path,
                    &[("host", AUTHORITY), ("authorization", authorization)],
                )
                .await;
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            }
            for duplicate in [AUTHORIZATION, "Bearer another-token"] {
                let response = send(
                    path,
                    &[
                        ("host", AUTHORITY),
                        ("authorization", AUTHORIZATION),
                        ("authorization", duplicate),
                    ],
                )
                .await;
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            }
        }
        let response = send(
            "/api/status?token=unit-test-session-token",
            &[("host", AUTHORITY)],
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn empty_policy_token_cannot_authorize() {
        let request = Request::builder()
            .uri("/api/status")
            .header("host", AUTHORITY)
            .header("authorization", "Bearer ")
            .body(Body::empty())
            .unwrap();
        let response = app("").oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn responses_never_grant_cors_and_errors_have_safe_json() {
        let response = send("/", &[("host", AUTHORITY)]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-inner-handler"], "reached");
        assert!(
            !response
                .headers()
                .keys()
                .any(|name| name.as_str().starts_with("access-control-"))
        );

        let response = send("/api/status", &[("host", AUTHORITY)]).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        assert!(
            !response
                .headers()
                .keys()
                .any(|name| name.as_str().starts_with("access-control-"))
        );
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(
            body,
            json!({ "error": {
                "code": "unauthorized", "message": "A valid host-session token is required."
            }})
        );
    }
}
