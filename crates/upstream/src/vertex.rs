//! Google service accounts and OAuth access tokens for Vertex AI.
//!
//! A service-account key file is exchanged for a short-lived access token
//! with the two-legged JWT-bearer grant:
//!
//! 1. sign a JWT (`RS256`) with the account's private key — claims `iss` =
//!    `client_email`, `scope`, `aud` = the account's `token_uri`, `iat`,
//!    `exp` = `iat` + 1 h;
//! 2. `POST` it form-encoded to the token endpoint as
//!    `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion=<jwt>`;
//! 3. the answer carries `access_token` and `expires_in`.
//!
//! [`TokenSource`] caches the token until 60 s before it expires and makes
//! sure concurrent callers share one exchange — and its outcome, whether
//! that is a token or a failure.

use crate::client::deadline_after;
use crate::request::{VERTEX_SCOPE, redact_url};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use ring::rand::SystemRandom;
use ring::rsa::KeyPair;
use ring::signature::RSA_PKCS1_SHA256;
use serde_json::{Value, json};
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use switchyard_core::util::now_unix;
use switchyard_core::{FailureClass, UpstreamError, UpstreamErrorInfo};
use tokio::time::Instant;

/// Token endpoint used when a key file names none.
pub const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// Lifetime requested for the signed assertion (Google's maximum).
const ASSERTION_LIFETIME_SECS: i64 = 3600;

/// The assertion is back-dated slightly so a gateway whose clock runs a few
/// seconds fast is not rejected with "token used too early".
const CLOCK_SKEW_SECS: i64 = 30;

/// A cached token is replaced this long before it expires.
const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// Lifetime assumed when the token endpoint does not state one.
const DEFAULT_EXPIRES_IN_SECS: u64 = 3600;

/// Longest lifetime a token endpoint is believed. Google issues one hour
/// (twelve with an organisation policy); a larger `expires_in` is a broken
/// endpoint, and trusting it would keep a dead token cached indefinitely.
const MAX_EXPIRES_IN_SECS: u64 = 12 * 3600;

/// Total time allowed for one token exchange.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
/// OAuth responses are small JSON documents, including on failure.
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;

/// A service-account key file could not be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid service account key: {0}")]
pub struct ServiceAccountError(pub String);

fn invalid(message: impl Into<String>) -> ServiceAccountError {
    ServiceAccountError(message.into())
}

/// A parsed Google service-account key.
pub struct ServiceAccount {
    /// `client_email`: the account's identity and the JWT issuer.
    pub client_email: String,
    /// `project_id`: the default Vertex AI project.
    pub project_id: String,
    /// `private_key_id`: sent as the JWT `kid`. May be empty.
    pub private_key_id: String,
    /// `token_uri`: where the assertion is exchanged, and its audience.
    pub token_uri: String,
    key: KeyPair,
    /// Hex SHA-256 of the public key; identifies the key without exposing it.
    fingerprint: String,
}

impl fmt::Debug for ServiceAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceAccount")
            .field("client_email", &self.client_email)
            .field("project_id", &self.project_id)
            .field("private_key_id", &self.private_key_id)
            .field("token_uri", &redact_url(&self.token_uri))
            .field("key_fingerprint", &self.fingerprint)
            .finish()
    }
}

fn required_string<'a>(root: &'a Value, key: &str) -> Result<&'a str, ServiceAccountError> {
    match root.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim()),
        Some(Value::String(_)) | Some(Value::Null) | None => {
            Err(invalid(format!("`{key}` is missing")))
        }
        Some(_) => Err(invalid(format!("`{key}` must be a string"))),
    }
}

/// Repairs the usual copy/paste damage of a PEM block: literal `\n`
/// sequences left behind by double JSON-encoding, Windows or doubled line
/// endings, indentation and surrounding whitespace.
fn normalize_pem(raw: &str) -> String {
    raw.replace("\\r", "\r")
        .replace("\\n", "\n")
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_private_key(raw: &str) -> Result<KeyPair, ServiceAccountError> {
    let text = normalize_pem(raw);
    let block =
        pem::parse(text.as_bytes()).map_err(|_| invalid("`private_key` is not a PEM block"))?;
    let key = match block.tag() {
        // PKCS#8, what Google issues.
        "PRIVATE KEY" => KeyPair::from_pkcs8(block.contents()),
        // PKCS#1, what some tools convert it to.
        "RSA PRIVATE KEY" => KeyPair::from_der(block.contents()),
        "ENCRYPTED PRIVATE KEY" => {
            return Err(invalid(
                "`private_key` is encrypted; an unencrypted key is required",
            ));
        }
        other => {
            return Err(invalid(format!(
                "`private_key` has PEM type `{other}`; expected `PRIVATE KEY` (PKCS#8)"
            )));
        }
    };
    key.map_err(|rejected| {
        invalid(format!(
            "`private_key` is not a usable RSA key ({rejected})"
        ))
    })
}

impl ServiceAccount {
    /// Parses and validates a service-account key file.
    ///
    /// Required: `type` = `service_account`, `client_email`, `project_id` and
    /// an RSA `private_key` in PEM (PKCS#8, or PKCS#1). `token_uri` defaults
    /// to Google's endpoint and must be an `http(s)` URL when present.
    pub fn from_json(text: &str) -> Result<ServiceAccount, ServiceAccountError> {
        let root: Value =
            serde_json::from_str(text).map_err(|_| invalid("the file is not valid JSON"))?;
        if !root.is_object() {
            return Err(invalid("the file must contain a JSON object"));
        }
        match root.get("type").and_then(Value::as_str).map(str::trim) {
            Some("service_account") => {}
            Some(other) if !other.is_empty() => {
                return Err(invalid(format!(
                    "`type` is `{other}`; only `service_account` keys are supported"
                )));
            }
            _ => return Err(invalid("`type` is missing; expected `service_account`")),
        }
        let client_email = required_string(&root, "client_email")?;
        if !client_email.contains('@') || client_email.contains(char::is_whitespace) {
            return Err(invalid("`client_email` is not an e-mail address"));
        }
        let project_id = required_string(&root, "project_id")?;
        let private_key = required_string(&root, "private_key")?;
        let key = parse_private_key(private_key)?;

        let token_uri = match root.get("token_uri") {
            None | Some(Value::Null) => DEFAULT_TOKEN_URI.to_string(),
            Some(Value::String(s)) if s.trim().is_empty() => DEFAULT_TOKEN_URI.to_string(),
            Some(Value::String(s)) => s.trim().to_string(),
            Some(_) => return Err(invalid("`token_uri` must be a string")),
        };
        let parsed =
            url::Url::parse(&token_uri).map_err(|_| invalid("`token_uri` is not a valid URL"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(invalid("`token_uri` must be an http(s) URL"));
        }

        let private_key_id = root
            .get("private_key_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let digest = ring::digest::digest(&ring::digest::SHA256, key.public().as_ref());
        let fingerprint = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();

        Ok(ServiceAccount {
            client_email: client_email.to_string(),
            project_id: project_id.to_string(),
            private_key_id,
            token_uri,
            key,
            fingerprint,
        })
    }

    /// Reads and parses a key file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<ServiceAccount, ServiceAccountError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| invalid(format!("cannot read `{}`: {e}", path.display())))?;
        ServiceAccount::from_json(&text)
    }

    /// Hex SHA-256 of the public key. Stable for a given key and safe to log.
    pub fn key_fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The public key as a DER `RSAPublicKey`, for verifying signatures.
    pub fn public_key_der(&self) -> &[u8] {
        self.key.public().as_ref()
    }

    /// Identity under which tokens for this key are cached.
    pub(crate) fn cache_key(&self) -> String {
        format!(
            "{}\n{}\n{}",
            self.client_email, self.token_uri, self.fingerprint
        )
    }

    /// Builds and signs the JWT assertion for `scope`, issued at `now` (unix
    /// seconds).
    pub fn sign_assertion(&self, scope: &str, now: i64) -> Result<String, UpstreamError> {
        let mut header = json!({"alg": "RS256", "typ": "JWT"});
        if !self.private_key_id.is_empty() {
            header["kid"] = Value::String(self.private_key_id.clone());
        }
        let issued_at = now - CLOCK_SKEW_SECS;
        let claims = json!({
            "iss": self.client_email,
            "scope": scope,
            "aud": self.token_uri,
            "iat": issued_at,
            "exp": issued_at + ASSERTION_LIFETIME_SECS,
        });
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| {
                token_error(
                    FailureClass::Auth,
                    format!(
                        "could not sign the token request for service account {}",
                        self.client_email
                    ),
                )
            })?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }
}

/// A failure to obtain an access token. `status` stays `0`: the model
/// endpoint was never reached.
fn token_error(class: FailureClass, message: String) -> UpstreamError {
    UpstreamError {
        status: 0,
        class,
        info: UpstreamErrorInfo {
            message,
            ..UpstreamErrorInfo::default()
        },
        retry_after_ms: None,
        body: None,
        content_type: None,
    }
}

#[derive(Clone)]
struct CachedToken {
    value: String,
    /// When the token stops being handed out.
    refresh_at: Instant,
}

/// One token exchange, awaited by every caller that arrived while it was
/// under way. Resolves to the minted token.
type Flight = Shared<BoxFuture<'static, Result<String, UpstreamError>>>;

#[derive(Default)]
struct TokenState {
    cached: Option<CachedToken>,
    /// The exchange in progress, if any.
    flight: Option<Flight>,
}

/// Clears the flight slot when the exchange task ends, however it ends
/// (completion, a panic, the runtime shutting down), so that a later caller
/// starts a new exchange instead of waiting on a dead one.
struct Landing(Arc<Mutex<TokenState>>);

impl Drop for Landing {
    fn drop(&mut self) {
        self.0.lock().flight = None;
    }
}

/// Mints and caches OAuth access tokens for one service account.
///
/// Exchanges are *single-flight*: callers that find no usable token while an
/// exchange is under way wait for it and share its outcome — the token, or
/// the failure. A failure is not cached; the next caller after it starts a
/// new exchange. The exchange runs in a task of its own, so it completes
/// (and its token is cached) even when the caller that started it goes away.
pub struct TokenSource {
    account: Arc<ServiceAccount>,
    /// Where the assertion is posted. Differs from the account's `token_uri`
    /// only when overridden (tests, a mirror of the token endpoint).
    endpoint: String,
    scope: String,
    state: Arc<Mutex<TokenState>>,
}

impl fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSource")
            .field("account", &self.account.client_email)
            .field("endpoint", &redact_url(&self.endpoint))
            .field("scope", &self.scope)
            .finish()
    }
}

impl TokenSource {
    /// A token source for `account` with the Vertex AI scope.
    pub fn new(account: Arc<ServiceAccount>) -> TokenSource {
        TokenSource {
            endpoint: account.token_uri.clone(),
            account,
            scope: VERTEX_SCOPE.to_string(),
            state: Arc::new(Mutex::new(TokenState::default())),
        }
    }

    /// Posts the assertion to `uri` instead of the account's `token_uri`.
    /// The assertion's audience is unchanged.
    pub fn with_token_uri(mut self, uri: impl Into<String>) -> TokenSource {
        self.endpoint = uri.into();
        self
    }

    /// Requests `scope` (space separated list) instead of the default.
    pub fn with_scope(mut self, scope: impl Into<String>) -> TokenSource {
        self.scope = scope.into();
        self
    }

    /// The account this source mints tokens for.
    pub fn account(&self) -> &ServiceAccount {
        &self.account
    }

    /// A valid access token, minted through `http` when none is cached.
    pub async fn token(&self, http: &reqwest::Client) -> Result<String, UpstreamError> {
        self.token_and_origin(http).await.map(|(token, _)| token)
    }

    /// Like [`TokenSource::token`], also telling whether the token comes from
    /// an exchange this call started or waited for (`true`) or from the
    /// cache (`false`).
    pub(crate) async fn token_and_origin(
        &self,
        http: &reqwest::Client,
    ) -> Result<(String, bool), UpstreamError> {
        let flight = {
            let mut state = self.state.lock();
            if let Some(cached) = state.cached.as_ref()
                && Instant::now() < cached.refresh_at
            {
                return Ok((cached.value.clone(), false));
            }
            state.cached = None;
            match &state.flight {
                Some(flight) => flight.clone(),
                None => {
                    let flight = self.take_off(http.clone());
                    state.flight = Some(flight.clone());
                    flight
                }
            }
        };
        flight.await.map(|token| (token, true))
    }

    /// Starts an exchange in a task of its own and returns the future every
    /// waiting caller shares.
    fn take_off(&self, http: reqwest::Client) -> Flight {
        let account = self.account.clone();
        let endpoint = self.endpoint.clone();
        let scope = self.scope.clone();
        let state = self.state.clone();
        let who = self.account.client_email.clone();
        let task = tokio::spawn(async move {
            let landing = Landing(state);
            let (value, expires_in) = exchange(&account, &endpoint, &scope, &http).await?;
            let usable_for = expires_in.saturating_sub(REFRESH_MARGIN);
            // Cached before the flight slot is cleared (when `landing`
            // drops), so no caller can slip in between and start a second
            // exchange.
            landing.0.lock().cached = Some(CachedToken {
                value: value.clone(),
                refresh_at: deadline_after(usable_for),
            });
            Ok(value)
        });
        async move {
            match task.await {
                Ok(outcome) => outcome,
                // The task was torn down with its runtime, or panicked.
                Err(_) => Err(token_error(
                    FailureClass::Transport,
                    format!("token exchange for service account {who} was interrupted"),
                )),
            }
        }
        .boxed()
        .shared()
    }

    /// Forgets the cached token if it still is `token` (the upstream rejected
    /// it). A newer token minted meanwhile is kept.
    pub async fn invalidate(&self, token: &str) {
        let mut state = self.state.lock();
        if state
            .cached
            .as_ref()
            .is_some_and(|cached| cached.value == token)
        {
            state.cached = None;
        }
    }
}

/// Performs one token exchange: returns the access token and how long the
/// endpoint says it lives.
async fn exchange(
    account: &ServiceAccount,
    endpoint: &str,
    scope: &str,
    http: &reqwest::Client,
) -> Result<(String, Duration), UpstreamError> {
    let who = &account.client_email;
    let assertion = account.sign_assertion(scope, now_unix())?;
    // Every character of a JWT is URL-safe, so the form body needs no
    // further escaping.
    let form = format!(
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={assertion}"
    );
    let mut response = http
        .post(endpoint)
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(http::header::ACCEPT, "application/json")
        .header(http::header::USER_AGENT, crate::request::USER_AGENT)
        .timeout(EXCHANGE_TIMEOUT)
        .body(form)
        .send()
        .await
        .map_err(|e| {
            token_error(
                FailureClass::Transport,
                format!(
                    "token exchange for service account {who} failed: {}",
                    crate::client::describe_transport(&e)
                ),
            )
        })?;
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        token_error(
            FailureClass::Transport,
            format!(
                "token exchange for service account {who} failed while reading the answer: {}",
                crate::client::describe_transport(&e)
            ),
        )
    })? {
        if chunk.len() > MAX_TOKEN_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(token_error(
                FailureClass::Transport,
                "token exchange response exceeds the byte limit".to_string(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let parsed: Option<Value> = serde_json::from_slice(&body).ok();

    if !status.is_success() {
        // OAuth error shape: {"error":"invalid_grant","error_description":"…"}.
        let code = parsed
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let description = parsed
            .as_ref()
            .and_then(|v| v.get("error_description"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let detail = match (code.is_empty(), description.is_empty()) {
            (false, false) => format!(": {code}: {description}"),
            (false, true) => format!(": {code}"),
            (true, false) => format!(": {description}"),
            (true, true) => String::new(),
        };
        let class = match status.as_u16() {
            429 => FailureClass::RateLimit,
            s if s >= 500 => FailureClass::Server,
            // The key was revoked, the account deleted or disabled, the
            // clock is far off: nothing another attempt would fix.
            _ => FailureClass::Auth,
        };
        let mut error = token_error(
            class,
            format!(
                "token exchange for service account {who} was rejected (HTTP {}){detail}",
                status.as_u16()
            ),
        );
        error.info.code = (!code.is_empty()).then(|| code.to_string());
        return Err(error);
    }

    let token = parsed
        .as_ref()
        .and_then(|v| v.get("access_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let Some(token) = token else {
        return Err(token_error(
            FailureClass::Server,
            format!("token exchange for service account {who} returned no access_token"),
        ));
    };
    let expires_in = parsed
        .as_ref()
        .and_then(|v| switchyard_core::util::u64_field(v, "expires_in"))
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_EXPIRES_IN_SECS)
        .min(MAX_EXPIRES_IN_SECS);
    Ok((token.to_string(), Duration::from_secs(expires_in)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    const PKCS8: &str = include_str!("../tests/fixtures/test_rsa_pkcs8.pem");
    const PKCS1: &str = include_str!("../tests/fixtures/test_rsa_pkcs1.pem");

    fn key_json(private_key: &str) -> Value {
        json!({
            "type": "service_account",
            "project_id": "demo-project",
            "private_key_id": "key-id-1",
            "private_key": private_key,
            "client_email": "svc@demo-project.iam.gserviceaccount.com",
            "client_id": "1234567890",
            "auth_uri": "https://accounts.google.com/o/oauth2/auth",
            "token_uri": "https://oauth2.googleapis.com/token",
            "universe_domain": "googleapis.com"
        })
    }

    fn account() -> ServiceAccount {
        ServiceAccount::from_json(&key_json(PKCS8).to_string()).unwrap()
    }

    fn rejects(mutate: impl FnOnce(&mut Value), needle: &str) {
        let mut json = key_json(PKCS8);
        mutate(&mut json);
        let err = ServiceAccount::from_json(&json.to_string()).unwrap_err();
        assert!(err.0.contains(needle), "expected `{needle}` in `{}`", err.0);
    }

    #[test]
    fn parses_a_google_key_file() {
        let sa = account();
        assert_eq!(sa.client_email, "svc@demo-project.iam.gserviceaccount.com");
        assert_eq!(sa.project_id, "demo-project");
        assert_eq!(sa.private_key_id, "key-id-1");
        assert_eq!(sa.token_uri, DEFAULT_TOKEN_URI);
        assert_eq!(sa.key_fingerprint().len(), 64);
    }

    #[test]
    fn accepts_pkcs1_and_damaged_line_endings() {
        let a = account();
        let b = ServiceAccount::from_json(&key_json(PKCS1).to_string()).unwrap();
        // Same key in both encodings.
        assert_eq!(a.key_fingerprint(), b.key_fingerprint());

        let crlf = PKCS8.replace('\n', "\r\n");
        let c = ServiceAccount::from_json(&key_json(&crlf).to_string()).unwrap();
        assert_eq!(a.key_fingerprint(), c.key_fingerprint());

        // Double-encoded: the PEM contains the two characters `\` `n`.
        let escaped = PKCS8.trim().replace('\n', "\\n");
        let d = ServiceAccount::from_json(&key_json(&escaped).to_string()).unwrap();
        assert_eq!(a.key_fingerprint(), d.key_fingerprint());
    }

    #[test]
    fn token_uri_defaults_and_is_validated() {
        let mut json = key_json(PKCS8);
        json.as_object_mut().unwrap().remove("token_uri");
        let sa = ServiceAccount::from_json(&json.to_string()).unwrap();
        assert_eq!(sa.token_uri, DEFAULT_TOKEN_URI);

        rejects(
            |j| j["token_uri"] = json!("ftp://example.test/token"),
            "token_uri",
        );
        rejects(|j| j["token_uri"] = json!("not a url"), "token_uri");
        rejects(|j| j["token_uri"] = json!(42), "token_uri");
    }

    #[test]
    fn rejects_unusable_files() {
        assert!(ServiceAccount::from_json("not json").is_err());
        assert!(ServiceAccount::from_json("[1,2]").is_err());
        rejects(|j| j["type"] = json!("authorized_user"), "authorized_user");
        rejects(
            |j| {
                j.as_object_mut().unwrap().remove("type");
            },
            "`type`",
        );
        rejects(|j| j["client_email"] = json!(""), "client_email");
        rejects(|j| j["client_email"] = json!("no-at-sign"), "client_email");
        rejects(
            |j| {
                j.as_object_mut().unwrap().remove("project_id");
            },
            "project_id",
        );
        rejects(
            |j| {
                j.as_object_mut().unwrap().remove("private_key");
            },
            "private_key",
        );
        rejects(|j| j["private_key"] = json!("garbage"), "PEM");
        rejects(
            |j| {
                j["private_key"] =
                    json!("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n")
            },
            "RSA key",
        );
        rejects(
            |j| {
                j["private_key"] =
                    json!("-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n")
            },
            "CERTIFICATE",
        );
    }

    #[test]
    fn errors_and_debug_never_contain_key_material() {
        let body = PKCS8.lines().nth(1).unwrap();
        let sa = account();
        let debug = format!("{sa:?}");
        assert!(!debug.contains(body), "{debug}");
        assert!(!debug.contains("PRIVATE KEY"));
        assert!(debug.contains("svc@demo-project.iam.gserviceaccount.com"));

        // A truncated key is rejected without echoing it.
        let truncated: String =
            PKCS8.lines().take(6).collect::<Vec<_>>().join("\n") + "\n-----END PRIVATE KEY-----\n";
        let err = ServiceAccount::from_json(&key_json(&truncated).to_string()).unwrap_err();
        assert!(!err.to_string().contains(body), "{err}");

        let source = TokenSource::new(Arc::new(account()));
        assert!(!format!("{source:?}").contains(body));
    }

    fn decode_segment(segment: &str) -> Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segment).unwrap()).unwrap()
    }

    #[test]
    fn assertion_is_a_verifiable_rs256_jwt() {
        let sa = account();
        let now = 1_800_000_000;
        let jwt = sa.sign_assertion(VERTEX_SCOPE, now).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert!(!jwt.contains('=') && !jwt.contains('+') && !jwt.contains('/'));

        assert_eq!(
            decode_segment(parts[0]),
            json!({"alg": "RS256", "typ": "JWT", "kid": "key-id-1"})
        );
        let claims = decode_segment(parts[1]);
        assert_eq!(claims["iss"], "svc@demo-project.iam.gserviceaccount.com");
        assert_eq!(
            claims["scope"],
            "https://www.googleapis.com/auth/cloud-platform"
        );
        assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
        assert_eq!(claims["iat"], now - 30);
        assert_eq!(
            claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
            3600
        );
        assert!(claims.get("sub").is_none());

        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        let signed = format!("{}.{}", parts[0], parts[1]);
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, sa.public_key_der())
            .verify(signed.as_bytes(), &signature)
            .expect("signature verifies with the account's public key");
        // …and not for a tampered payload.
        assert!(
            UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, sa.public_key_der())
                .verify(format!("{signed}x").as_bytes(), &signature)
                .is_err()
        );
    }

    #[test]
    fn kid_is_omitted_when_the_file_has_none() {
        let mut json = key_json(PKCS8);
        json.as_object_mut().unwrap().remove("private_key_id");
        let sa = ServiceAccount::from_json(&json.to_string()).unwrap();
        let jwt = sa.sign_assertion(VERTEX_SCOPE, 1_800_000_000).unwrap();
        let header = decode_segment(jwt.split('.').next().unwrap());
        assert_eq!(header, json!({"alg": "RS256", "typ": "JWT"}));
    }

    #[test]
    fn from_file_reads_and_reports_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.json");
        std::fs::write(&path, key_json(PKCS8).to_string()).unwrap();
        assert_eq!(
            ServiceAccount::from_file(&path).unwrap().project_id,
            "demo-project"
        );
        let err = ServiceAccount::from_file(dir.path().join("absent.json")).unwrap_err();
        assert!(err.0.contains("cannot read"), "{err}");
    }
}
