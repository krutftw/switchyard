//! Model discovery against local servers that page like the real APIs.

mod common;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use common::{KEY, client, serve, target};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use switchyard_core::config::ProviderKind;
use switchyard_core::{FailureClass, Protocol};

/// Query strings of the requests a server received, in order.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<HashMap<String, String>>>>);

impl Seen {
    fn record(&self, query: HashMap<String, String>) {
        self.0.lock().unwrap().push(query);
    }

    fn all(&self) -> Vec<HashMap<String, String>> {
        self.0.lock().unwrap().clone()
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}})),
    )
        .into_response()
}

async fn anthropic_models(
    State(seen): State<Seen>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if headers.get("x-api-key").and_then(|v| v.to_str().ok()) != Some(KEY)
        || headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok())
            != Some("2023-06-01")
    {
        return unauthorized();
    }
    let after = query.get("after_id").cloned();
    seen.record(query);
    let page = match after.as_deref() {
        None => json!({
            "data": [
                {"type": "model", "id": "claude-opus-5", "display_name": "Claude Opus 5",
                 "created_at": "2026-07-24T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                {"type": "model", "id": "claude-sonnet-5", "display_name": "Claude Sonnet 5",
                 "created_at": "2026-06-01T00:00:00Z"}
            ],
            "first_id": "claude-opus-5", "last_id": "claude-sonnet-5", "has_more": true
        }),
        Some("claude-sonnet-5") => json!({
            "data": [
                {"type": "model", "id": "claude-haiku-4-5", "display_name": "Claude Haiku 4.5",
                 "created_at": "2025-10-01T00:00:00Z"},
                // A repeated entry must not be listed twice.
                {"type": "model", "id": "claude-opus-5", "display_name": "Claude Opus 5"}
            ],
            "first_id": "claude-haiku-4-5", "last_id": "claude-opus-5", "has_more": false
        }),
        Some(other) => panic!("unexpected cursor {other}"),
    };
    axum::Json(page).into_response()
}

#[tokio::test]
async fn anthropic_listing_follows_after_id() {
    let seen = Seen::default();
    let addr = serve(
        Router::new()
            .route("/v1/models", get(anthropic_models))
            .with_state(seen.clone()),
    )
    .await;
    let target = target(
        ProviderKind::Anthropic,
        Protocol::Anthropic,
        format!("http://{addr}"),
        "",
    );
    let models = client().list_models(&target).await.unwrap();

    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        ["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"]
    );
    assert_eq!(models[0].display_name.as_deref(), Some("Claude Opus 5"));
    assert_eq!(models[0].context_window, Some(1_000_000));
    assert_eq!(models[0].max_output_tokens, Some(128_000));
    assert_eq!(models[0].owned_by.as_deref(), Some("anthropic"));
    assert!(models.iter().all(|m| !m.known && m.thinking.is_none()));

    let requests = seen.all();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].get("limit").map(String::as_str), Some("1000"));
    assert_eq!(requests[0].get("after_id"), None);
    assert_eq!(requests[1].get("limit").map(String::as_str), Some("1000"));
    assert_eq!(
        requests[1].get("after_id").map(String::as_str),
        Some("claude-sonnet-5")
    );
}

async fn gemini_models(
    State(seen): State<Seen>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()) != Some(KEY) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": {"code": 400, "message": "API key not valid. Please pass a valid API key.", "status": "INVALID_ARGUMENT",
                "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "API_KEY_INVALID", "domain": "googleapis.com"}]}})),
        )
            .into_response();
    }
    let token = query.get("pageToken").cloned();
    seen.record(query);
    let page = match token.as_deref() {
        None => json!({
            "models": [
                {"name": "models/gemini-3.8-flash", "displayName": "Gemini 3.8 Flash",
                 "inputTokenLimit": 1048576, "outputTokenLimit": 65536,
                 "supportedGenerationMethods": ["generateContent", "countTokens"]},
                {"name": "models/gemini-embedding-001", "displayName": "Gemini Embedding",
                 "supportedGenerationMethods": ["embedContent"]}
            ],
            // Characters that must be escaped when sent back.
            "nextPageToken": "tok/en+2&x=y z"
        }),
        Some("tok/en+2&x=y z") => json!({
            "models": [
                {"name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro",
                 "inputTokenLimit": 1048576, "outputTokenLimit": 65536,
                 "supportedGenerationMethods": ["generateContent", "countTokens"]}
            ]
        }),
        Some(other) => panic!("unexpected page token {other:?}"),
    };
    axum::Json(page).into_response()
}

#[tokio::test]
async fn gemini_listing_follows_page_tokens_and_filters_by_method() {
    let seen = Seen::default();
    let addr = serve(
        Router::new()
            .route("/v1beta/models", get(gemini_models))
            .with_state(seen.clone()),
    )
    .await;
    let target = target(
        ProviderKind::Gemini,
        Protocol::Gemini,
        format!("http://{addr}"),
        "",
    );
    let models = client().list_models(&target).await.unwrap();

    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["gemini-3.8-flash", "gemini-2.5-pro"]);
    assert_eq!(models[0].display_name.as_deref(), Some("Gemini 3.8 Flash"));
    assert_eq!(models[0].context_window, Some(1_048_576));
    assert_eq!(models[0].max_output_tokens, Some(65_536));
    assert!(models.iter().all(|m| !m.known));

    let requests = seen.all();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].get("pageSize").map(String::as_str),
        Some("1000")
    );
    // The token survived the round trip byte for byte.
    assert_eq!(
        requests[1].get("pageToken").map(String::as_str),
        Some("tok/en+2&x=y z")
    );
    // The key travels in a header, never in the URL.
    assert!(requests.iter().all(|q| !q.contains_key("key")));
}

#[tokio::test]
async fn a_rejected_key_is_reported_as_an_auth_failure() {
    let seen = Seen::default();
    let app = Router::new()
        .route("/v1beta/models", get(gemini_models))
        .route("/v1/models", get(anthropic_models))
        .with_state(seen);
    let addr = serve(app).await;
    let client = client();

    let mut gemini = target(
        ProviderKind::Gemini,
        Protocol::Gemini,
        format!("http://{addr}"),
        "",
    );
    gemini.auth = switchyard_upstream::Auth::ApiKey("wrong-key".into());
    let error = client.list_models(&gemini).await.unwrap_err();
    // Google answers a bad key with a 400.
    assert_eq!(error.status, 400);
    assert_eq!(error.class, FailureClass::Auth);
    assert_eq!(error.info.code.as_deref(), Some("API_KEY_INVALID"));

    let mut anthropic = target(
        ProviderKind::Anthropic,
        Protocol::Anthropic,
        format!("http://{addr}"),
        "",
    );
    anthropic.auth = switchyard_upstream::Auth::ApiKey("wrong-key".into());
    let error = client.list_models(&anthropic).await.unwrap_err();
    assert_eq!(error.status, 401);
    assert_eq!(error.class, FailureClass::Auth);
    assert_eq!(
        error.info.error_type.as_deref(),
        Some("authentication_error")
    );
}

#[tokio::test]
async fn openai_listing() {
    async fn models(headers: HeaderMap) -> Response {
        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {KEY}").as_str())
        );
        axum::Json(json!({"object": "list", "data": [
            {"id": "gpt-6-astra", "object": "model", "created": 1767225600, "owned_by": "openai"},
            {"id": "gpt-5.6-terra", "object": "model", "created": 1759000000, "owned_by": "system"},
            {"id": "text-embedding-3-large", "object": "model", "created": 1705953180, "owned_by": "system"}
        ]}))
        .into_response()
    }
    let addr = serve(Router::new().route("/v1/models", get(models))).await;
    for kind in [ProviderKind::Openai, ProviderKind::OpenaiCompat] {
        let target = target(kind, Protocol::OpenaiChat, format!("http://{addr}/v1"), "");
        let models = client().list_models(&target).await.unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        // The OpenAI listing says nothing about capabilities, so nothing is
        // filtered here.
        assert_eq!(
            ids,
            ["gpt-6-astra", "gpt-5.6-terra", "text-embedding-3-large"]
        );
        assert_eq!(models[0].created, Some(1767225600));
        assert_eq!(models[0].owned_by.as_deref(), Some("openai"));
    }
}

#[tokio::test]
async fn vertex_listing_uses_the_publisher_catalogue() {
    async fn models(Query(query): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
        assert_eq!(
            headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()),
            Some(KEY)
        );
        assert_eq!(query.get("pageSize").map(String::as_str), Some("1000"));
        axum::Json(json!({"publisherModels": [
            {"name": "publishers/google/models/gemini-2.5-pro", "versionId": "default"},
            {"name": "publishers/google/models/imagen-4.0-generate-001"},
            {"name": "publishers/google/models/gemini-3.8-flash"}
        ]}))
        .into_response()
    }
    let addr = serve(Router::new().route("/v1beta1/publishers/google/models", get(models))).await;
    let target = target(
        ProviderKind::Vertex,
        Protocol::Gemini,
        format!("http://{addr}"),
        "",
    );
    let models = client().list_models(&target).await.unwrap();
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["gemini-2.5-pro", "gemini-3.8-flash"]);
}

#[tokio::test]
async fn a_listing_that_loops_is_cut_off() {
    async fn looping(
        State(seen): State<Seen>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Response {
        seen.record(query);
        axum::Json(json!({
            "models": [{"name": "models/gemini-loop", "supportedGenerationMethods": ["generateContent"]}],
            "nextPageToken": "again"
        }))
        .into_response()
    }
    let seen = Seen::default();
    let addr = serve(
        Router::new()
            .route("/v1beta/models", get(looping))
            .with_state(seen.clone()),
    )
    .await;
    let target = target(
        ProviderKind::Gemini,
        Protocol::Gemini,
        format!("http://{addr}"),
        "",
    );
    let models = client().list_models(&target).await.unwrap();
    assert_eq!(models.len(), 1);
    // First page, then the page for "again" — which names itself and stops.
    assert_eq!(seen.all().len(), 2);
}

#[tokio::test]
async fn a_listing_that_is_not_json_is_an_error() {
    async fn html() -> Response {
        (
            [("content-type", "text/html")],
            "<html><body>Welcome to nginx</body></html>",
        )
            .into_response()
    }
    let addr = serve(Router::new().route("/v1/models", get(html))).await;
    let target = target(
        ProviderKind::OpenaiCompat,
        Protocol::OpenaiChat,
        format!("http://{addr}/v1"),
        "",
    );
    let error = client().list_models(&target).await.unwrap_err();
    assert_eq!(error.class, FailureClass::Transport);
    assert!(
        error.info.message.contains("not JSON"),
        "{}",
        error.info.message
    );
    assert!(error.body.as_deref().unwrap().contains("Welcome to nginx"));
}

#[tokio::test]
async fn a_missing_listing_endpoint_is_reported_with_its_status() {
    let addr = serve(Router::new()).await;
    let target = target(
        ProviderKind::OpenaiCompat,
        Protocol::OpenaiChat,
        format!("http://{addr}/v1"),
        "",
    );
    let error = client().list_models(&target).await.unwrap_err();
    assert_eq!(error.status, 404);
}

#[tokio::test]
async fn the_mock_provider_lists_its_models_without_a_network() {
    let target = target(ProviderKind::Mock, Protocol::OpenaiChat, "mock://local", "");
    let models = client().list_models(&target).await.unwrap();
    assert_eq!(models, switchyard_upstream::mock_models());
    assert!(models.iter().any(|m| m.id == "mock-echo"));
}
