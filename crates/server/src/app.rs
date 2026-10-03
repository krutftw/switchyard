//! The router: one dispatcher behind the layers every client response goes
//! through (request id, `server` header, CORS, panic recovery, access log).

use crate::body;
use crate::handlers;
use crate::lifecycle::Lifecycle;
use crate::origin;
use crate::respond;
use crate::route::{self, Resolved, Route};
use crate::ws;
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::any;
use http::header::{SERVER, VARY};
use http::request::Parts;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use std::any::Any;
use std::net::{IpAddr, SocketAddr};
use std::time::Instant;
use switchyard_core::{ApiError, Protocol};
use switchyard_gateway::{ClientIdentity, Gateway, PresentedCredentials};
use switchyard_telemetry::new_request_id;
use tower_http::catch_panic::CatchPanicLayer;

const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// How much of an unknown path a 404 repeats back, in characters.
const MAX_ECHOED_PATH: usize = 200;

/// The query parameter a client key may be given in.
const KEY_PARAM: &str = "key";

/// The query parameter a WebSocket ticket is given in.
pub(crate) const TICKET_PARAM: &str = "ticket";

/// Query parameters that carry a credential: never passed on, never shown.
const CREDENTIAL_PARAMS: [&str; 2] = [KEY_PARAM, TICKET_PARAM];

/// What every handler needs.
#[derive(Clone)]
pub(crate) struct App {
    pub(crate) gateway: Gateway,
}

/// See [`crate::router`].
pub(crate) fn router(gateway: Gateway) -> Router {
    let app = App { gateway };
    // Two catch-all routes rather than a fallback: the binary merges this
    // router with the admin one, and two routers cannot both bring a
    // fallback. The admin's `/admin…` routes are more specific and win.
    let routes = Router::new()
        .route("/", any(dispatch))
        .route("/{*path}", any(dispatch));
    wrap(routes, app)
}

/// Applies the layers of the client API to `routes` — to these routes only,
/// so nothing leaks onto whatever the router is merged with.
fn wrap(routes: Router<App>, app: App) -> Router {
    routes
        // Innermost: a panicking handler becomes a marked 500 …
        .layer(CatchPanicLayer::custom(panicked))
        // … which the outer layer, knowing the request, renders properly.
        .layer(middleware::from_fn_with_state(app.clone(), finalize))
        .with_state(app)
}

/// Marks the response of a handler that panicked.
#[derive(Clone, Copy, Debug)]
struct Panicked;

fn panicked(payload: Box<dyn Any + Send + 'static>) -> Response {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&'static str>().copied())
        .unwrap_or("(no message)");
    tracing::error!(panic = message, "a request handler panicked");
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response.extensions_mut().insert(Panicked);
    response
}

/// Whether a path belongs to the admin API. The client API's CORS policy
/// never applies there, even when a request for it ends up here (the admin
/// router is not mounted, or does not know the path).
fn is_admin_path(path: &str) -> bool {
    path == "/admin" || path.starts_with("/admin/")
}

/// Adds the permissive CORS headers of the client API.
fn add_cors(headers: &mut HeaderMap, requested_headers: Option<HeaderValue>) {
    headers.insert(
        HeaderName::from_static("access-control-allow-origin"),
        HeaderValue::from_static("*"),
    );
    headers.insert(
        HeaderName::from_static("access-control-allow-methods"),
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    headers.insert(
        HeaderName::from_static("access-control-allow-headers"),
        requested_headers.unwrap_or(HeaderValue::from_static("*")),
    );
    headers.insert(
        HeaderName::from_static("access-control-expose-headers"),
        HeaderValue::from_static(
            "x-request-id, retry-after, x-switchyard-provider, x-switchyard-model",
        ),
    );
    headers.insert(
        HeaderName::from_static("access-control-max-age"),
        HeaderValue::from_static("600"),
    );
}

/// The outer layer: answers CORS preflights, renders panics, and stamps
/// every response.
async fn finalize(State(app): State<App>, request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    // The path only: a query string may carry the client's key or ticket.
    let path = request.uri().path().to_string();
    let cors = app.gateway.config().server.cors && !is_admin_path(&path);
    let requested_headers = request
        .headers()
        .get("access-control-request-headers")
        .cloned();

    let mut response = if cors && method == Method::OPTIONS {
        // A preflight carries no credentials: answered before authentication.
        let mut response = Response::new(axum::body::Body::empty());
        *response.status_mut() = StatusCode::NO_CONTENT;
        response.headers_mut().insert(
            VARY,
            HeaderValue::from_static("access-control-request-headers"),
        );
        response
    } else {
        next.run(request).await
    };

    if response.extensions().get::<Panicked>().is_some() {
        response = respond::error(
            &app.gateway,
            route::protocol_of(&path),
            &ApiError::internal("the gateway failed to handle this request"),
        );
    }

    let headers = response.headers_mut();
    if !headers.contains_key(&X_REQUEST_ID)
        && let Ok(id) = HeaderValue::from_str(&new_request_id())
    {
        headers.insert(X_REQUEST_ID, id);
    }
    headers.insert(SERVER, HeaderValue::from_static("switchyard"));
    if cors {
        add_cors(headers, requested_headers);
    }

    tracing::debug!(
        method = %method,
        path = %path,
        status = response.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        request_id = response
            .headers()
            .get(&X_REQUEST_ID)
            .and_then(|id| id.to_str().ok())
            .unwrap_or(""),
        "request"
    );
    response
}

/// A parsed query string.
#[derive(Clone, Debug, Default)]
pub(crate) struct Query {
    pairs: Vec<(String, String)>,
    raw: String,
}

impl Query {
    pub(crate) fn parse(raw: Option<&str>) -> Query {
        let raw = raw.unwrap_or("");
        Query {
            pairs: form_urlencoded::parse(raw.as_bytes())
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect(),
            raw: raw.to_string(),
        }
    }

    /// The first value of `name`.
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The query string as received, without the parameters named in
    /// `drop` — and never with the client's `key` or WebSocket `ticket`.
    /// `None` when nothing is left.
    pub(crate) fn without(&self, drop: &[&str]) -> Option<String> {
        let kept: Vec<&str> = self
            .raw
            .split('&')
            .filter(|pair| !pair.is_empty())
            .filter(|pair| {
                let name = pair.split('=').next().unwrap_or(pair);
                let name: String = form_urlencoded::parse(name.as_bytes())
                    .next()
                    .map(|(name, _)| name.into_owned())
                    .unwrap_or_default();
                !CREDENTIAL_PARAMS.contains(&name.as_str()) && !drop.contains(&name.as_str())
            })
            .collect();
        if kept.is_empty() {
            None
        } else {
            Some(kept.join("&"))
        }
    }
}

/// The client's address for the request record: the peer of the
/// connection, or — only when that peer is a reverse proxy on this very
/// machine — what the proxy says in `X-Forwarded-For`.
///
/// The header is honoured for loopback peers alone because anyone can send
/// it; and of its entries only the last one counts, the one the local proxy
/// itself appended: everything further left is again just what a client
/// claimed. When that last entry is not an address, the peer is recorded —
/// never an earlier entry.
pub(crate) fn client_ip(parts: &Parts) -> Option<String> {
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        // A dual-stack listener reports IPv4 peers as `::ffff:a.b.c.d`.
        .map(|ConnectInfo(peer)| peer.ip().to_canonical())?;
    if peer.is_loopback()
        && let Some(forwarded) = forwarded_for(&parts.headers)
    {
        return Some(forwarded.to_string());
    }
    Some(peer.to_string())
}

/// The address the last `X-Forwarded-For` entry names.
fn forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
    let last = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .rfind(|entry| !entry.is_empty())?;
    forwarded_address(last)
}

/// One `X-Forwarded-For` entry as an address. Proxies write it bare
/// (`203.0.113.9`, `2001:db8::1`), with the client's port
/// (`203.0.113.9:4711`, `[2001:db8::1]:4711`), or in brackets alone.
fn forwarded_address(entry: &str) -> Option<IpAddr> {
    let address = if let Ok(address) = entry.parse::<IpAddr>() {
        address
    } else if let Ok(socket) = entry.parse::<SocketAddr>() {
        socket.ip()
    } else {
        entry
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .and_then(|inner| inner.parse::<IpAddr>().ok())?
    };
    Some(address.to_canonical())
}

/// Everything about a request that outlives its body: who sent it, from
/// where, with which headers and query.
pub(crate) struct Context {
    pub(crate) app: App,
    pub(crate) headers: HeaderMap,
    pub(crate) query: Query,
    pub(crate) client_ip: Option<String>,
    pub(crate) identity: ClientIdentity,
    /// The protocol errors are rendered in.
    pub(crate) protocol: Protocol,
    /// Present when served by [`crate::BoundServer::serve`].
    pub(crate) lifecycle: Option<Lifecycle>,
}

impl Context {
    pub(crate) fn gateway(&self) -> &Gateway {
        &self.app.gateway
    }

    /// An error response in this request's protocol.
    pub(crate) fn error(&self, error: &ApiError) -> Response {
        respond::error(&self.app.gateway, self.protocol, error)
    }
}

/// The one handler: resolves the route, authenticates, and hands over.
async fn dispatch(State(app): State<App>, request: Request) -> Response {
    let (mut parts, body) = request.into_parts();
    // A request refused before its body was looked at still has that body
    // read away first (within the body limit), so that a client in the
    // middle of sending it sees the answer and not a reset connection.
    let refuse = async |response: Response, headers: &HeaderMap, body: Body| {
        let limit = body::limit_bytes(app.gateway.config().server.body_limit_mb);
        body::discard(body, headers, limit).await;
        response
    };
    let (route, protocol) = match route::resolve(&parts.method, parts.uri.path()) {
        Resolved::Route { route, protocol } => (route, protocol),
        Resolved::NotFound { protocol } => {
            // The path is the client's own text: echoed, but not at any
            // length. (Never the query string, where a key may be.)
            let path: String = parts.uri.path().chars().take(MAX_ECHOED_PATH).collect();
            let error = ApiError::not_found(format!("no route for {} {path}", parts.method))
                .with_code("not_found");
            let response = respond::error(&app.gateway, protocol, &error);
            return refuse(response, &parts.headers, body).await;
        }
        Resolved::MethodNotAllowed { protocol, allow } => {
            let response =
                respond::method_not_allowed(&app.gateway, protocol, &parts.method, allow);
            return refuse(response, &parts.headers, body).await;
        }
    };

    if route.is_public() {
        return match route {
            Route::Health => handlers::health(&parts.method),
            _ => handlers::banner(),
        };
    }
    // `/v1/models` is shared by two API families: a client that announces
    // itself as an Anthropic one is answered — refused, too — in that shape.
    let protocol = match route {
        Route::Models | Route::Model(_) => handlers::models_protocol(&parts.headers),
        _ => protocol,
    };

    let query = Query::parse(parts.uri.query());
    let mut presented = PresentedCredentials::from_headers(
        &parts.headers,
        query.get(KEY_PARAM).map(str::to_string),
    );
    // WebSocket upgrades also take a single-use ticket bought with a key
    // (`POST /v1/ws-ticket`), so that a browser need not put the key itself
    // into the URL.
    if route.is_websocket() {
        presented.ws_ticket = query.get(TICKET_PARAM).map(str::to_string);
    }
    // The Realtime route accepts one more place for the key (a WebSocket
    // subprotocol, the only place a browser can put it).
    if route == Route::Realtime
        && presented.authorization.is_none()
        && let Some(key) = ws::realtime::subprotocol_key(&parts.headers)
    {
        presented.authorization = Some(format!("Bearer {key}"));
    }
    let identity = match app.gateway.authenticate(&presented) {
        Ok(identity) => identity,
        Err(first) => {
            // A browser client that also sent an `Authorization` header of
            // its own still gets its subprotocol key looked at.
            let retried = (route == Route::Realtime)
                .then(|| ws::realtime::subprotocol_key(&parts.headers))
                .flatten()
                .and_then(|key| {
                    app.gateway
                        .authenticate(&PresentedCredentials {
                            authorization: Some(format!("Bearer {key}")),
                            ..PresentedCredentials::default()
                        })
                        .ok()
                });
            match retried {
                Some(identity) => identity,
                None => {
                    let response = respond::error(&app.gateway, protocol, &first);
                    return refuse(response, &parts.headers, body).await;
                }
            }
        }
    };

    // Anonymous access never authorizes another site's browser requests,
    // including names that resolve to this listener through DNS rebinding.
    // With CORS switched off, other sites' pages get nothing done here:
    // not over a WebSocket (which a browser opens to any host and lets the
    // page read), and not blind either. See `origin`.
    let config = app.gateway.config();
    if !route.is_listing()
        && ((identity.anonymous && !origin::anonymous_host_is_allowed(&parts, &config.server.host))
            || ((identity.anonymous || !config.server.cors) && origin::is_foreign_page(&parts)))
    {
        let error = ApiError::permission(
            "this gateway does not serve web pages of other origins without permitted authentication",
        )
        .with_code("origin_not_allowed");
        let response = respond::error(&app.gateway, protocol, &error);
        return refuse(response, &parts.headers, body).await;
    }

    let context = Context {
        client_ip: client_ip(&parts),
        lifecycle: parts.extensions.get::<Lifecycle>().cloned(),
        headers: parts.headers.clone(),
        query,
        identity,
        protocol,
        app,
    };

    match route {
        Route::Banner | Route::Health => handlers::banner(),
        Route::Models => handlers::models(&context),
        Route::Model(id) => handlers::model(&context, &id),
        Route::GeminiModels => handlers::gemini_models(&context),
        Route::GeminiModel(name) => handlers::gemini_model(&context, &name),
        Route::ResponsesWebSocket => ws::responses::upgrade(context, &mut parts),
        Route::Realtime => ws::realtime::upgrade(context, &mut parts).await,
        Route::WsTicket => handlers::ws_ticket(&context, body).await,
        Route::ChatCompletions
        | Route::Completions
        | Route::Responses
        | Route::ResponsesInputTokens
        | Route::Messages
        | Route::MessagesCountTokens
        | Route::GeminiAction { .. }
        | Route::Raw(_) => handlers::with_body(context, route, body).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use switchyard_gateway::GatewayOptions;
    use tower::ServiceExt;

    async fn gateway(extra: &str) -> (Gateway, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        std::fs::write(
            &path,
            format!("[upstream]\nproxy = \"direct\"\n[usage]\npersist = false\n{extra}"),
        )
        .unwrap();
        let gateway = Gateway::start(GatewayOptions::new(&path).watch(false))
            .await
            .unwrap();
        (gateway, dir)
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn request(method: &str, path: &str) -> Request {
        http::Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn a_panicking_handler_answers_500_in_the_routes_protocol() {
        let (gateway, _dir) = gateway("").await;
        let app = App { gateway };
        let routes = Router::new().route(
            "/{*path}",
            any(|| async {
                if true {
                    panic!("boom");
                }
            }),
        );
        let router = wrap(routes, app);

        let response = router
            .clone()
            .oneshot(request("POST", "/v1/messages"))
            .await
            .unwrap();
        assert_eq!(response.status(), 500);
        assert!(response.headers().contains_key("x-request-id"));
        assert_eq!(response.headers()["server"], "switchyard");
        let body = body_json(response).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "api_error");

        let response = router
            .clone()
            .oneshot(request("POST", "/v1beta/models/x:generateContent"))
            .await
            .unwrap();
        assert_eq!(response.status(), 500);
        let body = body_json(response).await;
        assert_eq!(body["error"]["code"], 500);
        assert_eq!(body["error"]["status"], "INTERNAL");

        let response = router
            .oneshot(request("POST", "/v1/chat/completions"))
            .await
            .unwrap();
        assert_eq!(response.status(), 500);
        let body = body_json(response).await;
        assert_eq!(body["error"]["type"], "server_error");
        // Nothing about the panic itself reaches the client.
        assert!(!body.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn cors_never_applies_to_admin_paths() {
        let (gateway, _dir) = gateway("").await;
        let router = router(gateway);
        let response = router
            .clone()
            .oneshot(request("OPTIONS", "/v1/chat/completions"))
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");

        for path in ["/admin", "/admin/api/status"] {
            let response = router
                .clone()
                .oneshot(request("OPTIONS", path))
                .await
                .unwrap();
            assert_eq!(response.status(), 404, "{path}");
            assert!(
                !response
                    .headers()
                    .contains_key("access-control-allow-origin"),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn the_router_merges_with_one_that_owns_admin_and_a_fallback() {
        let (gateway, _dir) = gateway("").await;
        let admin = Router::new()
            .route("/admin/api/{*path}", any(|| async { "admin api" }))
            .route("/admin/", any(|| async { "dashboard" }))
            .fallback(|| async { "someone else's fallback" });
        let merged = router(gateway).merge(admin);

        let response = merged
            .clone()
            .oneshot(request("GET", "/admin/api/status"))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        // The admin's routes are not touched by the client API's layers.
        assert!(!response.headers().contains_key("x-request-id"));
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );

        let response = merged
            .clone()
            .oneshot(request("GET", "/healthz"))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(body_json(response).await["status"], "ok");

        let response = merged.oneshot(request("GET", "/nope")).await.unwrap();
        assert_eq!(response.status(), 404);
        assert!(body_json(response).await["error"]["message"].is_string());
    }

    #[tokio::test]
    async fn the_router_merges_with_the_admin_routers_shape_in_either_order() {
        let (gateway, _dir) = gateway("").await;
        // The shape of the admin crate's router: the dashboard's routes, and
        // the API mounted as a service that brings its own 404.
        let admin = || {
            let api = Router::new()
                .route("/status", any(|| async { "status" }))
                .fallback(|| async { (StatusCode::NOT_FOUND, "admin 404") });
            Router::new()
                .route("/admin", any(|| async { "redirect" }))
                .route("/admin/", any(|| async { "dashboard" }))
                .route("/admin/{*path}", any(|| async { "file" }))
                .nest_service("/admin/api", api)
        };
        for merged in [
            router(gateway.clone()).merge(admin()),
            admin().merge(router(gateway.clone())),
        ] {
            for (path, status) in [
                ("/admin", 200),
                ("/admin/", 200),
                ("/admin/app.js", 200),
                ("/admin/api/status", 200),
                ("/admin/api/nope", 404),
                ("/healthz", 200),
                ("/v1/nope", 404),
                ("/administrator", 404),
            ] {
                let response = merged.clone().oneshot(request("GET", path)).await.unwrap();
                let ours = response.headers().contains_key("x-request-id");
                assert_eq!(response.status(), status, "{path}");
                // Everything under `/admin` is the admin router's; nothing
                // else is.
                assert_eq!(ours, !is_admin_path(path), "{path}");
            }
        }
    }

    #[test]
    fn query_strings() {
        let query = Query::parse(Some("key=sk%2D1&alt=sse&model=a%20b&flag&key=second"));
        assert_eq!(query.get("key"), Some("sk-1"));
        assert_eq!(query.get("alt"), Some("sse"));
        assert_eq!(query.get("model"), Some("a b"));
        assert_eq!(query.get("flag"), Some(""));
        assert_eq!(query.get("missing"), None);
        assert_eq!(
            query.without(&["model"]).as_deref(),
            Some("alt=sse&flag"),
            "the key never survives, however often it is given"
        );
        assert_eq!(Query::parse(Some("key=x")).without(&[]), None);
        assert_eq!(
            Query::parse(Some("ticket=t1&model=m&tick%65t=t2&ticket"))
                .without(&[])
                .as_deref(),
            Some("model=m"),
            "nor does a WebSocket ticket"
        );
        assert_eq!(
            Query::parse(Some("%6Bey=x&a=1")).without(&[]).as_deref(),
            Some("a=1")
        );
        assert_eq!(Query::parse(None).get("key"), None);
        assert_eq!(Query::parse(None).without(&[]), None);
    }

    fn parts_from(peer: Option<&str>, forwarded: &[&str]) -> Parts {
        let mut builder = http::Request::builder().uri("/");
        for value in forwarded {
            builder = builder.header("x-forwarded-for", *value);
        }
        let (mut parts, ()) = builder.body(()).unwrap().into_parts();
        if let Some(peer) = peer {
            parts
                .extensions
                .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        }
        parts
    }

    #[test]
    fn forwarded_addresses_count_only_from_loopback_peers() {
        assert_eq!(client_ip(&parts_from(None, &["203.0.113.9"])), None);
        assert_eq!(
            client_ip(&parts_from(Some("198.51.100.7:4000"), &["203.0.113.9"])).as_deref(),
            Some("198.51.100.7")
        );
        assert_eq!(
            client_ip(&parts_from(Some("127.0.0.1:4000"), &[])).as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            client_ip(&parts_from(
                Some("127.0.0.1:4000"),
                &["10.9.9.9, 203.0.113.9"]
            ))
            .as_deref(),
            Some("203.0.113.9")
        );
        // Several header lines are one list.
        assert_eq!(
            client_ip(&parts_from(
                Some("[::1]:4000"),
                &["203.0.113.9", "198.51.100.1, 2001:db8::1 ,"]
            ))
            .as_deref(),
            Some("2001:db8::1")
        );
        assert_eq!(
            client_ip(&parts_from(Some("127.0.0.1:4000"), &["not an address"])).as_deref(),
            Some("127.0.0.1")
        );
    }

    #[test]
    fn only_the_last_forwarded_entry_counts_in_whatever_form_it_is_written() {
        let recorded =
            |forwarded: &[&str]| client_ip(&parts_from(Some("127.0.0.1:4000"), forwarded));
        // Proxies that append the client's port.
        assert_eq!(
            recorded(&["6.6.6.6, 203.0.113.9:4711"]).as_deref(),
            Some("203.0.113.9")
        );
        assert_eq!(
            recorded(&["6.6.6.6, [2001:db8::1]:443"]).as_deref(),
            Some("2001:db8::1")
        );
        assert_eq!(
            recorded(&["6.6.6.6, [2001:db8::1]"]).as_deref(),
            Some("2001:db8::1")
        );
        assert_eq!(
            recorded(&["203.0.113.9:4711"]).as_deref(),
            Some("203.0.113.9")
        );
        assert_eq!(
            recorded(&["6.6.6.6, ::ffff:203.0.113.9"]).as_deref(),
            Some("203.0.113.9")
        );
        // What the proxy appended is not an address: the proxy itself is
        // recorded. The entries before it are what a client claimed, and
        // never stand in.
        for last in [
            "unknown",
            "_hidden",
            "203.0.113.9:port",
            "[2001:db8::1",
            "1.2.3",
        ] {
            assert_eq!(
                recorded(&[&format!("6.6.6.6, {last}")]).as_deref(),
                Some("127.0.0.1"),
                "{last}"
            );
        }

        // A proxy on this machine that reaches a dual-stack listener over
        // IPv4 is a loopback peer like any other.
        assert_eq!(
            client_ip(&parts_from(
                Some("[::ffff:127.0.0.1]:4000"),
                &["203.0.113.9"]
            ))
            .as_deref(),
            Some("203.0.113.9")
        );
        assert_eq!(
            client_ip(&parts_from(
                Some("[::ffff:198.51.100.7]:4000"),
                &["203.0.113.9"]
            ))
            .as_deref(),
            Some("198.51.100.7")
        );
    }
}
