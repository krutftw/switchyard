//! Service-account token minting against a local token endpoint, and
//! Vertex AI calls authenticated with the minted token.

mod common;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::{client, dead_addr, serve, timeouts};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use switchyard_core::config::{ProviderKind, ProxySetting};
use switchyard_core::{FailureClass, Protocol};
use switchyard_upstream::{
    Auth, HttpClients, Operation, ServiceAccount, Target, TokenSource, adapt_vertex_anthropic_body,
};

/// A throwaway 2048-bit RSA key generated for these tests only.
const TEST_KEY_PEM: &str = include_str!("fixtures/test_rsa_pkcs8.pem");

const EMAIL: &str = "gateway@demo-project.iam.gserviceaccount.com";

fn key_file(token_uri: &str) -> String {
    json!({
        "type": "service_account",
        "project_id": "demo-project",
        "private_key_id": "0123456789abcdef",
        "private_key": TEST_KEY_PEM,
        "client_email": EMAIL,
        "client_id": "100000000000000000001",
        "token_uri": token_uri,
    })
    .to_string()
}

fn account(token_uri: &str) -> Arc<ServiceAccount> {
    Arc::new(ServiceAccount::from_json(&key_file(token_uri)).unwrap())
}

/// A stand-in for Google's token endpoint and for Vertex AI.
#[derive(Clone)]
struct Google {
    /// Public key assertions must verify against.
    public_key: Arc<Vec<u8>>,
    mints: Arc<AtomicUsize>,
    claims: Arc<Mutex<Vec<Value>>>,
    expires_in: u64,
    delay: Duration,
    /// Tokens the model endpoint no longer accepts.
    revoked: Arc<Mutex<HashSet<String>>>,
    reject_exchange: bool,
}

impl Google {
    fn new() -> Google {
        let reference =
            ServiceAccount::from_json(&key_file("https://unused.invalid/token")).unwrap();
        Google {
            public_key: Arc::new(reference.public_key_der().to_vec()),
            mints: Arc::new(AtomicUsize::new(0)),
            claims: Arc::new(Mutex::new(Vec::new())),
            expires_in: 3600,
            delay: Duration::ZERO,
            revoked: Arc::new(Mutex::new(HashSet::new())),
            reject_exchange: false,
        }
    }

    fn mints(&self) -> usize {
        self.mints.load(Ordering::SeqCst)
    }

    async fn start(&self) -> SocketAddr {
        let app = Router::new()
            .route("/token", post(token_endpoint))
            .route(
                "/v1/projects/{project}/locations/{location}/publishers/{publisher}/models/{model_action}",
                post(model_endpoint),
            )
            .with_state(self.clone());
        serve(app).await
    }
}

fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({"error": "invalid_request", "error_description": message})),
    )
        .into_response()
}

/// Implements the JWT-bearer grant the way Google's endpoint does: checks
/// the form encoding, the grant type and the assertion's RS256 signature.
async fn token_endpoint(State(google): State<Google>, headers: HeaderMap, body: Bytes) -> Response {
    tokio::time::sleep(google.delay).await;
    let n = google.mints.fetch_add(1, Ordering::SeqCst) + 1;
    if google.reject_exchange {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(
                json!({"error": "invalid_grant", "error_description": "Invalid JWT Signature."}),
            ),
        )
            .into_response();
    }
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        != Some("application/x-www-form-urlencoded")
    {
        return bad_request("wrong content type");
    }
    let form = String::from_utf8(body.to_vec()).unwrap();
    let field = |name: &str| {
        form.split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    };
    if field("grant_type").as_deref()
        != Some("urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer")
    {
        return bad_request("wrong grant type");
    }
    let Some(assertion) = field("assertion") else {
        return bad_request("no assertion");
    };
    let segments: Vec<&str> = assertion.split('.').collect();
    if segments.len() != 3 {
        return bad_request("assertion is not a JWT");
    }
    let signed = format!("{}.{}", segments[0], segments[1]);
    let Ok(signature) = URL_SAFE_NO_PAD.decode(segments[2]) else {
        return bad_request("signature is not base64url");
    };
    if UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, google.public_key.as_slice())
        .verify(signed.as_bytes(), &signature)
        .is_err()
    {
        return bad_request("signature does not verify");
    }
    let header: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segments[0]).unwrap()).unwrap();
    if header["alg"] != "RS256" || header["typ"] != "JWT" {
        return bad_request("wrong JWT header");
    }
    let claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segments[1]).unwrap()).unwrap();
    google.claims.lock().unwrap().push(claims);
    axum::Json(json!({
        "access_token": format!("ya29.test-token-{n}"),
        "expires_in": google.expires_in,
        "token_type": "Bearer"
    }))
    .into_response()
}

async fn model_endpoint(
    State(google): State<Google>,
    Path((project, location, publisher, model_action)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    if !bearer.starts_with("ya29.test-token-") || google.revoked.lock().unwrap().contains(&bearer) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {
                "code": 401,
                "message": "Request had invalid authentication credentials. Expected OAuth 2 access token, login cookie or other valid authentication credential.",
                "status": "UNAUTHENTICATED"
            }})),
        )
            .into_response();
    }
    axum::Json(json!({
        "project": project,
        "location": location,
        "publisher": publisher,
        "model_action": model_action,
        "token": bearer,
        "has_api_key_header": headers.contains_key("x-goog-api-key"),
        "body": serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
    }))
    .into_response()
}

fn http() -> reqwest::Client {
    HttpClients::new()
        .unwrap()
        .client(&ProxySetting::Direct, Duration::from_secs(5))
        .unwrap()
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test]
async fn mints_a_token_with_a_valid_assertion_and_caches_it() {
    let google = Google::new();
    let addr = google.start().await;
    let token_uri = format!("http://{addr}/token");
    let source = TokenSource::new(account(&token_uri));
    let http = http();

    let first = source.token(&http).await.unwrap();
    assert_eq!(first, "ya29.test-token-1");
    // Served from the cache: no second exchange.
    assert_eq!(source.token(&http).await.unwrap(), first);
    assert_eq!(source.token(&http).await.unwrap(), first);
    assert_eq!(google.mints(), 1);

    let claims = google.claims.lock().unwrap()[0].clone();
    assert_eq!(claims["iss"], EMAIL);
    assert_eq!(
        claims["scope"],
        "https://www.googleapis.com/auth/cloud-platform"
    );
    assert_eq!(claims["aud"], token_uri);
    let iat = claims["iat"].as_i64().unwrap();
    let exp = claims["exp"].as_i64().unwrap();
    assert_eq!(exp - iat, 3600);
    assert!(
        (now_unix() - iat).abs() < 120,
        "iat {iat} is not close to now"
    );
    assert!(claims.get("sub").is_none());
}

#[tokio::test]
async fn concurrent_callers_share_one_exchange() {
    let mut google = Google::new();
    google.delay = Duration::from_millis(150);
    let addr = google.start().await;
    let source = Arc::new(TokenSource::new(account(&format!("http://{addr}/token"))));
    let http = http();

    let calls = (0..24).map(|_| {
        let source = source.clone();
        let http = http.clone();
        tokio::spawn(async move { source.token(&http).await.unwrap() })
    });
    let tokens: Vec<String> = futures::future::join_all(calls)
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert!(
        tokens.iter().all(|t| t == "ya29.test-token-1"),
        "{tokens:?}"
    );
    assert_eq!(google.mints(), 1);
}

#[tokio::test]
async fn tokens_are_replaced_a_minute_before_they_expire() {
    // A token valid for 30 s is already inside the 60 s refresh margin, so
    // it is never served from the cache.
    let mut google = Google::new();
    google.expires_in = 30;
    let addr = google.start().await;
    let source = TokenSource::new(account(&format!("http://{addr}/token")));
    let http = http();
    assert_eq!(source.token(&http).await.unwrap(), "ya29.test-token-1");
    assert_eq!(source.token(&http).await.unwrap(), "ya29.test-token-2");
    assert_eq!(google.mints(), 2);

    // One valid for two minutes is reused.
    let mut google = Google::new();
    google.expires_in = 120;
    let addr = google.start().await;
    let source = TokenSource::new(account(&format!("http://{addr}/token")));
    assert_eq!(source.token(&http).await.unwrap(), "ya29.test-token-1");
    assert_eq!(source.token(&http).await.unwrap(), "ya29.test-token-1");
    assert_eq!(google.mints(), 1);
}

#[tokio::test]
async fn invalidating_a_token_forces_a_new_exchange() {
    let google = Google::new();
    let addr = google.start().await;
    let source = TokenSource::new(account(&format!("http://{addr}/token")));
    let http = http();
    let first = source.token(&http).await.unwrap();
    // Invalidating some other token changes nothing.
    source.invalidate("ya29.something-else").await;
    assert_eq!(source.token(&http).await.unwrap(), first);
    source.invalidate(&first).await;
    assert_eq!(source.token(&http).await.unwrap(), "ya29.test-token-2");
    assert_eq!(google.mints(), 2);
}

#[tokio::test]
async fn the_token_endpoint_can_be_overridden_without_changing_the_audience() {
    let google = Google::new();
    let addr = google.start().await;
    let source = TokenSource::new(account("https://oauth2.googleapis.com/token"))
        .with_token_uri(format!("http://{addr}/token"));
    assert_eq!(source.token(&http()).await.unwrap(), "ya29.test-token-1");
    let claims = google.claims.lock().unwrap()[0].clone();
    assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
}

#[tokio::test]
async fn a_rejected_exchange_is_an_auth_failure_and_is_not_cached() {
    let mut google = Google::new();
    google.reject_exchange = true;
    let addr = google.start().await;
    let source = TokenSource::new(account(&format!("http://{addr}/token")));
    let http = http();
    for attempt in 1..=2 {
        let error = source.token(&http).await.unwrap_err();
        assert_eq!(error.class, FailureClass::Auth);
        assert_eq!(error.status, 0);
        assert_eq!(error.info.code.as_deref(), Some("invalid_grant"));
        let message = &error.info.message;
        assert!(message.contains(EMAIL), "{message}");
        assert!(message.contains("HTTP 400"), "{message}");
        assert!(
            message.contains("invalid_grant: Invalid JWT Signature."),
            "{message}"
        );
        assert!(!message.contains("eyJ"), "the assertion leaked: {message}");
        assert_eq!(google.mints(), attempt);
    }
}

#[tokio::test]
async fn an_unreachable_token_endpoint_is_a_transport_failure() {
    let dead = dead_addr().await;
    let source = TokenSource::new(account(&format!("http://{dead}/token?secret=in-query")));
    let error = source.token(&http()).await.unwrap_err();
    assert_eq!(error.class, FailureClass::Transport);
    assert!(error.info.message.contains(EMAIL), "{}", error.info.message);
    assert!(
        !error.info.message.contains("in-query"),
        "{}",
        error.info.message
    );
}

fn vertex_target(addr: SocketAddr, protocol: Protocol, model: &str) -> Target {
    Target {
        provider: "vertex-test".into(),
        kind: ProviderKind::Vertex,
        // A custom base URL replaces the regional Google host.
        base_url: format!("http://{addr}"),
        protocol,
        model: model.into(),
        auth: Auth::ServiceAccount(account(&format!("http://{addr}/token"))),
        headers: Vec::new(),
        proxy: ProxySetting::Direct,
        project: String::new(),
        location: "us-central1".into(),
    }
}

async fn json_body(response: switchyard_upstream::UpstreamResponse) -> Value {
    serde_json::from_slice(&response.body.collect().await.unwrap()).unwrap()
}

#[tokio::test]
async fn vertex_calls_carry_the_minted_bearer_token() {
    let google = Google::new();
    let addr = google.start().await;
    let client = client();
    let target = vertex_target(addr, Protocol::Gemini, "gemini-2.5-pro");
    let body =
        Bytes::from(json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}).to_string());
    let none = HeaderMap::new();

    let seen = json_body(
        client
            .send(
                &target,
                &Operation::Generate { stream: false },
                body.clone(),
                &none,
                timeouts(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(seen["project"], "demo-project");
    assert_eq!(seen["location"], "us-central1");
    assert_eq!(seen["publisher"], "google");
    assert_eq!(seen["model_action"], "gemini-2.5-pro:generateContent");
    assert_eq!(seen["token"], "ya29.test-token-1");
    assert_eq!(seen["has_api_key_header"], false);
    assert_eq!(seen["body"]["contents"][0]["parts"][0]["text"], "hi");

    // Further calls — including from a freshly parsed copy of the same key
    // file — reuse the token.
    for _ in 0..3 {
        let again = vertex_target(addr, Protocol::Gemini, "gemini-2.5-pro");
        let seen = json_body(
            client
                .send(
                    &again,
                    &Operation::CountTokens,
                    body.clone(),
                    &none,
                    timeouts(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(seen["model_action"], "gemini-2.5-pro:countTokens");
        assert_eq!(seen["token"], "ya29.test-token-1");
    }
    assert_eq!(google.mints(), 1);
    assert_eq!(
        client
            .access_token(&target, Duration::from_secs(5))
            .await
            .unwrap()
            .as_deref(),
        Some("ya29.test-token-1")
    );
}

#[tokio::test]
async fn a_cached_token_the_upstream_rejects_is_replaced_once() {
    let google = Google::new();
    let addr = google.start().await;
    let client = client();
    let target = vertex_target(addr, Protocol::Gemini, "gemini-2.5-pro");
    let body = Bytes::from_static(b"{}");
    let none = HeaderMap::new();
    let op = Operation::Generate { stream: false };

    client
        .send(&target, &op, body.clone(), &none, timeouts())
        .await
        .unwrap();
    google
        .revoked
        .lock()
        .unwrap()
        .insert("ya29.test-token-1".into());

    // The cached token fails with 401; a new one is minted transparently.
    let seen = json_body(
        client
            .send(&target, &op, body.clone(), &none, timeouts())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(seen["token"], "ya29.test-token-2");
    assert_eq!(google.mints(), 2);

    // A token that is rejected right after being minted is not retried:
    // the account itself is the problem.
    google
        .revoked
        .lock()
        .unwrap()
        .insert("ya29.test-token-2".into());
    google
        .revoked
        .lock()
        .unwrap()
        .insert("ya29.test-token-3".into());
    let error = client
        .send(&target, &op, body.clone(), &none, timeouts())
        .await
        .unwrap_err();
    assert_eq!(error.status, 401);
    assert_eq!(error.class, FailureClass::Auth);
    assert_eq!(error.info.error_type.as_deref(), Some("UNAUTHENTICATED"));
    assert_eq!(google.mints(), 3);
}

#[tokio::test]
async fn claude_on_vertex_uses_raw_predict_and_the_adapted_body() {
    let google = Google::new();
    let addr = google.start().await;
    let target = vertex_target(addr, Protocol::Anthropic, "claude-opus-5");
    let op = Operation::Generate { stream: false };
    let mut body = json!({
        "model": "claude-opus-5",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "Hello"}]
    });
    adapt_vertex_anthropic_body(&mut body, &op);

    let seen = json_body(
        client()
            .send(
                &target,
                &op,
                Bytes::from(body.to_string()),
                &HeaderMap::new(),
                timeouts(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(seen["publisher"], "anthropic");
    assert_eq!(seen["model_action"], "claude-opus-5:rawPredict");
    assert_eq!(seen["token"], "ya29.test-token-1");
    assert_eq!(
        seen["body"],
        json!({
            "anthropic_version": "vertex-2023-10-16",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "Hello"}]
        })
    );
}

#[tokio::test]
async fn a_failed_exchange_fails_the_call_before_the_model_is_contacted() {
    let mut google = Google::new();
    google.reject_exchange = true;
    let addr = google.start().await;
    let target = vertex_target(addr, Protocol::Gemini, "gemini-2.5-pro");
    let error = client()
        .send(
            &target,
            &Operation::Generate { stream: false },
            Bytes::from_static(b"{}"),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.class, FailureClass::Auth);
    assert_eq!(error.status, 0);
}

#[tokio::test]
async fn an_exchange_outlives_the_caller_that_started_it() {
    let mut google = Google::new();
    google.delay = Duration::from_millis(200);
    let addr = google.start().await;
    let source = Arc::new(TokenSource::new(account(&format!("http://{addr}/token"))));
    let http = http();

    // The first caller gives up (its client disconnected) mid-exchange.
    let impatient = {
        let source = source.clone();
        let http = http.clone();
        tokio::spawn(async move { source.token(&http).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    impatient.abort();
    assert!(impatient.await.unwrap_err().is_cancelled());

    // A caller arriving meanwhile joins the same exchange instead of
    // starting over, and the token is cached for everyone after it.
    assert_eq!(source.token(&http).await.unwrap(), "ya29.test-token-1");
    assert_eq!(source.token(&http).await.unwrap(), "ya29.test-token-1");
    assert_eq!(google.mints(), 1);
}

#[tokio::test]
async fn a_failed_exchange_is_not_remembered() {
    // Callers that waited for a failed exchange share its failure (see
    // `review_vertex.rs`); the next caller after it starts afresh.
    let mut google = Google::new();
    google.reject_exchange = true;
    google.delay = Duration::from_millis(100);
    let addr = google.start().await;
    let source = Arc::new(TokenSource::new(account(&format!("http://{addr}/token"))));
    let http = http();

    for round in 1..=2 {
        let callers = (0..5).map(|_| {
            let source = source.clone();
            let http = http.clone();
            tokio::spawn(async move { source.token(&http).await })
        });
        for outcome in futures::future::join_all(callers).await {
            let error = outcome.unwrap().unwrap_err();
            assert_eq!(error.class, FailureClass::Auth);
            assert_eq!(error.info.code.as_deref(), Some("invalid_grant"));
        }
        assert_eq!(google.mints(), round, "one exchange per round");
    }
}

/// A model endpoint that refuses the call and quotes the bearer token.
async fn quotes_the_token(headers: HeaderMap) -> Response {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    (
        StatusCode::FORBIDDEN,
        axum::Json(json!({"error": {
            "code": 403,
            "message": format!("Token {bearer} lacks the aiplatform.endpoints.predict permission."),
            "status": "PERMISSION_DENIED"
        }})),
    )
        .into_response()
}

#[tokio::test]
async fn a_minted_token_quoted_by_the_upstream_is_scrubbed_from_the_error() {
    let google = Google::new();
    let token_addr = google.start().await;
    let model_addr = serve(Router::new().route(
        "/v1/projects/{project}/locations/{location}/publishers/{publisher}/models/{model_action}",
        post(quotes_the_token),
    ))
    .await;
    let mut target = vertex_target(token_addr, Protocol::Gemini, "gemini-2.5-pro");
    target.base_url = format!("http://{model_addr}");

    let error = client()
        .send(
            &target,
            &Operation::Generate { stream: false },
            Bytes::from_static(b"{}"),
            &HeaderMap::new(),
            timeouts(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 403);
    assert_eq!(
        error.info.message,
        "Token [redacted] lacks the aiplatform.endpoints.predict permission."
    );
    assert!(!error.body.unwrap().contains("ya29."));
}

#[tokio::test]
async fn api_key_targets_need_no_token() {
    let target = common::target(
        ProviderKind::Vertex,
        Protocol::Gemini,
        "http://127.0.0.1:9",
        "gemini-2.5-pro",
    );
    assert_eq!(
        client()
            .access_token(&target, Duration::from_secs(1))
            .await
            .unwrap(),
        None
    );
}
