use crate::{AppError, Result, assets, guard};
use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use switchyard_agent::{AppEngine, Decision};
use switchyard_agent_adapters::{AdapterManager, ApprovalDecision, StartRequest};

const BODY_LIMIT: usize = 256 * 1024;

pub(crate) struct AppState {
    pub(crate) engine: AppEngine,
    pub(crate) gateway: switchyard_gateway::Gateway,
    pub(crate) config_path: PathBuf,
    pub(crate) adapters: AdapterManager,
    pub(crate) accounts: crate::accounts::AccountManager,
}

pub(crate) fn router(
    state: Arc<AppState>,
    port: u16,
    token: String,
    stopped: tokio_util::sync::CancellationToken,
) -> Router {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/models", get(models))
        .route(
            "/api/providers",
            get(crate::providers::list).post(crate::providers::create),
        )
        .route(
            "/api/providers/{name}/refresh",
            post(crate::providers::refresh),
        )
        .route("/api/projects", get(projects).post(open_project))
        .route("/api/sessions", get(sessions).post(create_session))
        .route("/api/sessions/{id}", get(session))
        .route("/api/sessions/{id}/turns", post(submit_turn))
        .route("/api/sessions/{id}/events", get(events))
        .route(
            "/api/sessions/{id}/operations/{operation_id}/decision",
            post(decide_operation),
        )
        .route("/api/sessions/{id}/interrupt", post(interrupt))
        .route("/api/sessions/{id}/recovery", post(acknowledge_recovery))
        .route(
            "/api/account-profiles",
            get(crate::accounts::list).post(crate::accounts::create),
        )
        .route(
            "/api/account-profiles/{id}/refresh",
            post(crate::accounts::refresh),
        )
        .route(
            "/api/account-profiles/{id}/login",
            get(crate::accounts::login_status).post(crate::accounts::start_login),
        )
        .route(
            "/api/account-profiles/{id}/login/cancel",
            post(crate::accounts::cancel_login),
        )
        .route(
            "/api/account-profiles/{id}/login/open",
            post(crate::accounts::open_login),
        )
        .route(
            "/api/account-profiles/{id}/select",
            post(crate::accounts::select),
        )
        .route(
            "/api/account-profiles/{id}/terminal",
            post(crate::accounts::launch_terminal),
        )
        .route("/api/adapters", get(adapters))
        .route(
            "/api/adapter-runs",
            get(adapter_runs).post(start_adapter_run),
        )
        .route("/api/adapter-runs/{id}", get(adapter_run))
        .route("/api/adapter-runs/{id}/events", get(adapter_events))
        .route("/api/adapter-runs/{id}/decisions", post(decide_adapter))
        .route("/api/adapter-runs/{id}/interrupt", post(interrupt_adapter))
        .route(
            "/api/adapter-runs/{id}/recovery",
            post(acknowledge_adapter_recovery),
        )
        .method_not_allowed_fallback(|| async {
            AppError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "This method is not supported by this route.",
            )
        })
        .fallback(assets::serve)
        .with_state(state)
        .layer(middleware::from_fn_with_state(
            Arc::new(guard::AccessPolicy::new(port, token)),
            guard::enforce,
        ))
        .layer(middleware::from_fn_with_state(stopped, response_policy))
}

/// Apply to the outermost layer so access errors receive the same browser policy.
async fn response_policy(
    State(stopped): State<tokio_util::sync::CancellationToken>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = tokio::select! {
        biased;
        () = stopped.cancelled() => AppError::new(StatusCode::SERVICE_UNAVAILABLE, "host_stopping", "The local app host is shutting down.").into_response(),
        result = tokio::time::timeout(Duration::from_secs(15), next.run(request)) => match result {
            Ok(response) => response,
            Err(_) => AppError::new(StatusCode::REQUEST_TIMEOUT, "request_timeout", "This request timed out. Read the current session state before retrying a mutation.").into_response(),
        },
    };
    // Axum path rejections must retain the public JSON error shape as well.
    if response.status().is_client_error()
        && !response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|value| value.as_bytes().starts_with(b"application/json"))
    {
        response = AppError::new(
            response.status(),
            "invalid_request",
            "The request could not be accepted.",
        )
        .into_response();
    }
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert(header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'".parse().unwrap());
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    headers.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    headers.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    response
}

pub(crate) fn no_query(uri: &Uri) -> Result<()> {
    if uri.query().is_some() {
        return Err(AppError::invalid(
            "This route does not accept query parameters.",
        ));
    }
    Ok(())
}

fn decode_query_part(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err(AppError::invalid(
                    "A query parameter contains invalid escaping.",
                ));
            }
            index += 2;
        }
        index += 1;
    }
    let replaced = value.replace('+', " ");
    let decoded = percent_encoding::percent_decode_str(&replaced)
        .decode_utf8()
        .map_err(|_| AppError::invalid("Query parameters must contain valid UTF-8."))?
        .into_owned();
    if decoded.chars().any(char::is_control) {
        return Err(AppError::invalid(
            "Query parameters cannot contain control characters.",
        ));
    }
    Ok(decoded)
}

fn query(uri: &Uri, allowed: &[&str]) -> Result<BTreeMap<String, String>> {
    let mut output = BTreeMap::new();
    if let Some(raw) = uri.query() {
        if raw.is_empty() || raw.len() > 2048 {
            return Err(AppError::invalid("The query string is empty or too long."));
        }
        for pair in raw.split('&') {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| AppError::invalid("Every query parameter requires a value."))?;
            let key = decode_query_part(key)?;
            let value = decode_query_part(value)?;
            if !allowed.contains(&key.as_str()) || output.insert(key, value).is_some() {
                return Err(AppError::invalid("Unknown or duplicate query parameter."));
            }
        }
    }
    Ok(output)
}

fn event_cursor(uri: &Uri, max_limit: usize) -> Result<(u64, usize)> {
    let values = query(uri, &["after_seq", "limit"])?;
    let integer = |name: &str, default: u64| -> Result<u64> {
        values.get(name).map_or(Ok(default), |value| {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(AppError::invalid(
                    "Event cursor and limit must be unsigned decimal integers.",
                ));
            }
            value
                .parse()
                .map_err(|_| AppError::invalid("Event cursor or limit is out of range."))
        })
    };
    let after_seq = integer("after_seq", 0)?;
    let limit = integer("limit", 200)?;
    if limit == 0 || limit > max_limit as u64 {
        return Err(AppError::invalid(format!(
            "Event limit must be between 1 and {max_limit}."
        )));
    }
    Ok((after_seq, limit as usize))
}

pub(crate) async fn body<T: DeserializeOwned>(request: Request) -> Result<T> {
    no_query(request.uri())?;
    let mut content_types = request.headers().get_all(header::CONTENT_TYPE).iter();
    let content_type = content_types.next().and_then(|value| value.to_str().ok());
    if content_types.next().is_some()
        || !content_type.is_some_and(|value| {
            let mut parts = value.split(';').map(str::trim);
            parts
                .next()
                .is_some_and(|part| part.eq_ignore_ascii_case("application/json"))
                && parts.all(|part| {
                    part.eq_ignore_ascii_case("charset=utf-8")
                        || part.eq_ignore_ascii_case("charset=\"utf-8\"")
                })
        })
    {
        return Err(AppError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "Send exactly one application/json Content-Type.",
        ));
    }
    let bytes = to_bytes(request.into_body(), BODY_LIMIT)
        .await
        .map_err(|_| {
            AppError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_limit",
                "The JSON request body could not be read within the 256 KiB limit.",
            )
        })?;
    serde_json::from_slice(&bytes)
        .map_err(|_| AppError::invalid("The JSON body does not match this route's fields."))
}

fn command_id(value: &str) -> Result<()> {
    if uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value) {
        Ok(())
    } else {
        Err(AppError::invalid(
            "command_id must be a canonical lowercase UUID retained across retries.",
        ))
    }
}

async fn status(State(state): State<Arc<AppState>>, request: Request) -> Result<Json<Value>> {
    no_query(request.uri())?;
    Ok(Json(
        json!({"status": state.engine.status(), "version": env!("CARGO_PKG_VERSION"), "gateway_config_path": state.config_path}),
    ))
}

async fn models(State(state): State<Arc<AppState>>, request: Request) -> Result<Json<Value>> {
    no_query(request.uri())?;
    Ok(Json(json!({"models": state.engine.models()?})))
}

async fn projects(State(state): State<Arc<AppState>>, request: Request) -> Result<Json<Value>> {
    no_query(request.uri())?;
    Ok(Json(json!({"projects": state.engine.list_projects()?})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenProject {
    path: String,
}

async fn open_project(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<impl IntoResponse> {
    let input: OpenProject = body(request).await?;
    let project = state.engine.open_project(&input.path)?;
    Ok((StatusCode::CREATED, Json(json!({"project": project}))))
}

async fn sessions(State(state): State<Arc<AppState>>, request: Request) -> Result<Json<Value>> {
    let values = query(request.uri(), &["project_id"])?;
    let project_id = values.get("project_id").map(String::as_str);
    if project_id.is_some_and(str::is_empty) {
        return Err(AppError::invalid("project_id cannot be empty."));
    }
    Ok(Json(
        json!({"sessions": state.engine.list_sessions(project_id)?}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSession {
    project_id: String,
    model: String,
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<impl IntoResponse> {
    let input: CreateSession = body(request).await?;
    let session = state
        .engine
        .create_session(&input.project_id, &input.model)?;
    Ok((StatusCode::CREATED, Json(json!({"session": session}))))
}

async fn session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    no_query(request.uri())?;
    Ok(Json(
        serde_json::to_value(state.engine.session(&id)?)
            .map_err(|_| AppError::local("Could not read the session."))?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitTurn {
    command_id: String,
    text: String,
}

async fn submit_turn(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<impl IntoResponse> {
    let input: SubmitTurn = body(request).await?;
    command_id(&input.command_id)?;
    let run = state
        .engine
        .submit_turn(&id, &input.command_id, &input.text)?;
    Ok((StatusCode::ACCEPTED, Json(json!({"run": run}))))
}

async fn events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    let (after_seq, limit) = event_cursor(request.uri(), 500)?;
    Ok(Json(
        json!({"events": state.engine.events(&id, after_seq, limit)?}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationDecision {
    expected_hash: String,
    decision: Decision,
}

async fn decide_operation(
    State(state): State<Arc<AppState>>,
    Path((id, operation_id)): Path<(String, String)>,
    request: Request,
) -> Result<Json<Value>> {
    let input: OperationDecision = body(request).await?;
    state
        .engine
        .decide_operation(&id, &operation_id, &input.expected_hash, input.decision)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Interrupt {
    run_id: String,
}

async fn interrupt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    let input: Interrupt = body(request).await?;
    state.engine.interrupt(&id, &input.run_id)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryAcknowledgement {
    expected_revision: u64,
    note: String,
}

async fn acknowledge_recovery(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    let input: RecoveryAcknowledgement = body(request).await?;
    let session = state
        .engine
        .acknowledge_recovery(&id, input.expected_revision, &input.note)?;
    Ok(Json(json!({"session": session})))
}

async fn adapters(State(state): State<Arc<AppState>>, request: Request) -> Result<Json<Value>> {
    no_query(request.uri())?;
    Ok(Json(json!({"adapters": state.adapters.discover().await})))
}

async fn adapter_runs(State(state): State<Arc<AppState>>, request: Request) -> Result<Json<Value>> {
    no_query(request.uri())?;
    Ok(Json(json!({"runs": state.adapters.list_runs()})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartAdapterRun {
    adapter_id: String,
    project_id: String,
    prompt: String,
    command_id: String,
    profile_id: Option<String>,
    /// Continue the saved conversation of this run instead of starting a new one.
    #[serde(default)]
    continue_run_id: Option<String>,
}

async fn start_adapter_run(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<impl IntoResponse> {
    let input: StartAdapterRun = body(request).await?;
    command_id(&input.command_id)?;
    let project = state
        .engine
        .list_projects()?
        .into_iter()
        .find(|project| project.id == input.project_id)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "Open the project in this workspace before starting an adapter run.",
            )
        })?;
    let run = state.accounts.start_run(
        &state.adapters,
        StartRequest {
            adapter_id: input.adapter_id,
            project_path: PathBuf::from(project.root),
            prompt: input.prompt,
            command_id: input.command_id,
            continue_run_id: input.continue_run_id,
            profile: None,
        },
        input.profile_id.as_deref(),
    )?;
    Ok((StatusCode::ACCEPTED, Json(json!({"run": run}))))
}

async fn adapter_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    no_query(request.uri())?;
    Ok(Json(json!({"run": state.adapters.run(&id)?})))
}

async fn adapter_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    let (after_seq, limit) = event_cursor(request.uri(), 200)?;
    Ok(Json(
        json!({"events": state.adapters.events(&id, after_seq, limit)?}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterDecision {
    approval_id: String,
    expected_hash: String,
    decision: ApprovalDecision,
}

async fn decide_adapter(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    let input: AdapterDecision = body(request).await?;
    state
        .adapters
        .decide(
            &id,
            &input.approval_id,
            &input.expected_hash,
            input.decision,
        )
        .await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

async fn interrupt_adapter(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    let _: Empty = body(request).await?;
    state.adapters.interrupt(&id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn acknowledge_adapter_recovery(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    let _: Empty = body(request).await?;
    Ok(Json(
        json!({"run": state.adapters.acknowledge_recovery(&id)?}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppOptions, startup::AppRuntime};
    use axum::body::Body;
    use tower::ServiceExt;

    struct Harness {
        _temp: tempfile::TempDir,
        runtime: AppRuntime,
        router: Router,
    }

    impl Harness {
        async fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let options =
                AppOptions::new(temp.path().join("gateway.toml"), temp.path().join("state"));
            let runtime = AppRuntime::start(&options).await.unwrap();
            let state = Arc::new(AppState {
                engine: runtime.engine.clone(),
                gateway: runtime.gateway.clone(),
                config_path: runtime.config_path.clone(),
                adapters: AdapterManager::new(),
                accounts: crate::accounts::AccountManager::open(&options.data_dir).unwrap(),
            });
            Self {
                router: router(
                    state,
                    19317,
                    "test-host-token".into(),
                    tokio_util::sync::CancellationToken::new(),
                ),
                runtime,
                _temp: temp,
            }
        }

        async fn request(
            &self,
            method: &str,
            path: &str,
            value: Option<Value>,
        ) -> (StatusCode, Value) {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header("host", "127.0.0.1:19317")
                .header("authorization", "Bearer test-host-token")
                .header("content-type", "application/json")
                .body(value.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .unwrap();
            let response = self.router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert_eq!(response.headers()["x-content-type-options"], "nosniff");
            let status = response.status();
            let value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            (status, value)
        }
    }

    #[test]
    fn event_cursor_rejects_ambiguous_or_unbounded_queries() {
        for query in [
            "limit=0",
            "limit=501",
            "limit=-1",
            "limit=+1",
            "limit=1&limit=2",
            "after_seq=18446744073709551616",
            "after_seq=1.0",
            "after_seq=%GG",
            "after_seq=%FF",
            "limit",
            "limit=2&x=1",
            "limit=2&",
        ] {
            let uri = format!("/api/sessions/test/events?{query}")
                .parse()
                .unwrap();
            assert!(event_cursor(&uri, 500).is_err(), "accepted {query}");
        }
        assert_eq!(
            event_cursor(&"/events?after_seq=7&limit=25".parse().unwrap(), 500).unwrap(),
            (7, 25)
        );
        assert_eq!(
            event_cursor(&"/events".parse().unwrap(), 500).unwrap(),
            (0, 200)
        );
    }

    #[tokio::test]
    async fn routes_enforce_json_fields_queries_methods_and_body_limit() {
        let harness = Harness::new().await;
        for (method, path, value, expected) in [
            (
                "GET",
                "/api/status?token=unused",
                None,
                StatusCode::BAD_REQUEST,
            ),
            (
                "GET",
                "/api/sessions?project_id=a&project_id=b",
                None,
                StatusCode::BAD_REQUEST,
            ),
            (
                "GET",
                "/api/account-profiles?refresh=true",
                None,
                StatusCode::BAD_REQUEST,
            ),
            (
                "POST",
                "/api/account-profiles",
                Some(json!({"agent_id":"codex","name":"Fixture","home":"outside"})),
                StatusCode::BAD_REQUEST,
            ),
            (
                "POST",
                "/api/projects",
                Some(json!({"path":".","extra":true})),
                StatusCode::BAD_REQUEST,
            ),
            (
                "POST",
                "/api/sessions/unknown/recovery",
                Some(json!({"expected_revision":0,"note":"reviewed","allow_all":true})),
                StatusCode::BAD_REQUEST,
            ),
            (
                "POST",
                "/api/adapter-runs",
                Some(
                    json!({"adapter_id":"codex","project_path":".","prompt":"test","command_id":uuid::Uuid::new_v4().to_string()}),
                ),
                StatusCode::BAD_REQUEST,
            ),
            (
                "POST",
                "/api/adapter-runs",
                Some(
                    json!({"adapter_id":"codex","project_id":"unknown","prompt":"test","command_id":uuid::Uuid::new_v4().to_string()}),
                ),
                StatusCode::NOT_FOUND,
            ),
            (
                "DELETE",
                "/api/projects",
                None,
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            ("GET", "/api/missing", None, StatusCode::NOT_FOUND),
            (
                "POST",
                "/api/projects",
                Some(json!({"path":"x".repeat(BODY_LIMIT)})),
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
        ] {
            let (status, body) = harness.request(method, path, value).await;
            assert_eq!(status, expected, "{method} {path}: {body}");
            assert!(body["error"]["code"].is_string());
        }
        harness.runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn account_routes_create_cached_profiles_and_gate_unverified_runs() {
        let harness = Harness::new().await;
        let (status, initial) = harness.request("GET", "/api/account-profiles", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(initial["profiles"].as_array().unwrap().len(), 1);
        assert_eq!(initial["profiles"][0]["id"], "system-codex");
        assert_eq!(initial["profiles"][0]["read_only"], true);
        assert_eq!(initial["profiles"][0]["refreshing"], false);
        let (status, created) = harness
            .request(
                "POST",
                "/api/account-profiles",
                Some(json!({"agent_id":"codex","name":"API fixture"})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let id = created["profile"]["id"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(id).is_ok());
        assert_eq!(created["profile"]["managed"], true);
        assert_eq!(created["profile"]["refreshing"], false);
        assert_eq!(created["profile"]["account"]["auth_status"], "unknown");
        let (status, login) = harness
            .request("GET", &format!("/api/account-profiles/{id}/login"), None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(login["login"].is_null());
        let (status, selected) = harness
            .request(
                "POST",
                &format!("/api/account-profiles/{id}/select"),
                Some(json!({})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(selected["defaults"]["codex"], id);
        assert_eq!(selected["profile"]["selected"], true);
        let project = harness
            .runtime
            .engine
            .open_project(harness._temp.path())
            .unwrap();
        let (status, rejected) = harness
            .request(
                "POST",
                "/api/adapter-runs",
                Some(json!({
                    "adapter_id":"codex", "project_id":project.id,
                    "profile_id":id, "prompt":"This fixture must not spawn a CLI.",
                    "command_id":uuid::Uuid::new_v4().to_string()
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(rejected["error"]["code"], "profile_not_signed_in");
        let (_, runs) = harness.request("GET", "/api/adapter-runs", None).await;
        assert!(runs["runs"].as_array().unwrap().is_empty());
        harness.runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn continuation_and_recovery_routes_reject_unknown_runs_without_spawning() {
        let harness = Harness::new().await;
        let project = harness
            .runtime
            .engine
            .open_project(harness._temp.path())
            .unwrap();
        let (status, missing) = harness
            .request(
                "POST",
                "/api/adapter-runs",
                Some(json!({
                    "adapter_id":"codex", "project_id":project.id,
                    "prompt":"This fixture must not spawn a CLI.",
                    "continue_run_id":"missing",
                    "command_id":uuid::Uuid::new_v4().to_string()
                })),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
        let (status, malformed) = harness
            .request(
                "POST",
                "/api/adapter-runs",
                Some(json!({
                    "adapter_id":"codex", "project_id":project.id,
                    "prompt":"test", "continue_run_id":5,
                    "command_id":uuid::Uuid::new_v4().to_string()
                })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{malformed}");
        let (status, recovery) = harness
            .request(
                "POST",
                "/api/adapter-runs/missing/recovery",
                Some(json!({})),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{recovery}");
        assert!(recovery["error"]["code"].is_string());
        let (_, runs) = harness.request("GET", "/api/adapter-runs", None).await;
        assert!(runs["runs"].as_array().unwrap().is_empty());
        harness.runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retrying_the_same_command_does_not_create_a_second_turn() {
        let harness = Harness::new().await;
        let project = harness
            .runtime
            .engine
            .open_project(harness._temp.path())
            .unwrap();
        let (_, session) = harness
            .request(
                "POST",
                "/api/sessions",
                Some(json!({"project_id":project.id,"model":"mock-echo"})),
            )
            .await;
        let id = session["session"]["id"].as_str().unwrap();
        assert_eq!(
            harness
                .request(
                    "POST",
                    &format!("/api/sessions/{id}/recovery"),
                    Some(json!({"expected_revision":0,"note":"Reviewed local files."}))
                )
                .await
                .0,
            StatusCode::CONFLICT
        );
        let path = format!("/api/sessions/{id}/turns");
        let input = json!({"command_id":uuid::Uuid::new_v4().to_string(),"text":"echo this once"});
        let (status, first) = harness.request("POST", &path, Some(input.clone())).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let (status, second) = harness.request("POST", &path, Some(input.clone())).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(first["run"]["id"], second["run"]["id"]);
        let altered = json!({"command_id":input["command_id"],"text":"a different turn"});
        assert_eq!(
            harness.request("POST", &path, Some(altered)).await.0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            harness
                .runtime
                .engine
                .events(id, 0, 500)
                .unwrap()
                .iter()
                .filter(|event| event.kind == "turn.started")
                .count(),
            1
        );
        harness.runtime.shutdown().await.unwrap();
    }
}
