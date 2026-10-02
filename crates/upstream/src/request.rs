//! Turns a [`Target`] and an [`Operation`] into a concrete HTTP request
//! description: method, URL and headers. Pure — no I/O — so every provider
//! kind × protocol × operation combination can be unit tested.
//!
//! # URL shapes
//!
//! | kind | root | generate | count tokens | models |
//! |---|---|---|---|---|
//! | `openai`, `openai-compat` | `{base}` (`/v1` appended only when the base URL has no path) | `/chat/completions`, `/responses` | `/responses/input_tokens` | `/models` |
//! | `anthropic` | `{base}/v1` | `/messages` | `/messages/count_tokens` | `/models?limit=1000` |
//! | `gemini` | `{base}/v1beta` | `/models/{m}:generateContent`, `:streamGenerateContent?alt=sse` | `:countTokens` | `/models?pageSize=1000` |
//! | `vertex` (Gemini) | `{host}/v1` | `/projects/{p}/locations/{l}/publishers/google/models/{m}:…` | `:countTokens` | `/v1beta1/publishers/google/models` |
//! | `vertex` (Claude) | `{host}/v1` | `…/publishers/anthropic/models/{m}:rawPredict`, `:streamRawPredict` | `…/models/count-tokens:rawPredict` | — |
//!
//! A version segment already present at the end of the configured base URL
//! is never doubled.
//!
//! # Header rules
//!
//! 1. Built-in defaults: `content-type`, `accept` (`text/event-stream` for
//!    streams), `user-agent: switchyard/<version>`, `anthropic-version`.
//! 2. An allow-list of client headers is forwarded: `anthropic-beta` and an
//!    `anthropic-version` override to Anthropic-protocol upstreams;
//!    `openai-beta`, `openai-organization`, `openai-project` to OpenAI
//!    upstreams. Nothing else the client sent reaches the upstream — in
//!    particular never its credentials, cookies, `host`, `content-length` or
//!    `x-request-id`.
//! 3. The credential.
//! 4. Headers from the provider configuration, which override everything
//!    above except the credential header(s). `anthropic-beta` is merged with
//!    the client's value instead of replacing it, because both sides
//!    legitimately contribute beta flags.

use crate::secrets::{display_header_value, is_confidential_header};
use crate::target::{Auth, Operation, Target};
use http::Method;
use http::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
    USER_AGENT as USER_AGENT_HEADER,
};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use serde_json::{Map, Value};
use std::fmt;
use switchyard_core::config::ProviderKind;
use switchyard_core::{FailureClass, Protocol, UpstreamError, UpstreamErrorInfo};

/// `User-Agent` sent on every upstream request.
pub const USER_AGENT: &str = concat!("switchyard/", env!("CARGO_PKG_VERSION"));

/// Default `anthropic-version` header for the first-party Messages API.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `anthropic_version` body field required by Claude models on Vertex AI.
pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

/// Host root used for Vertex AI when the provider sets no custom base URL.
const VERTEX_DEFAULT_BASE: &str = "https://aiplatform.googleapis.com";

/// OAuth scope requested for Vertex AI access tokens.
pub const VERTEX_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");
const X_GOOG_API_KEY: HeaderName = HeaderName::from_static("x-goog-api-key");
const ANTHROPIC_VERSION_HEADER: HeaderName = HeaderName::from_static("anthropic-version");
const ANTHROPIC_BETA: HeaderName = HeaderName::from_static("anthropic-beta");
const OPENAI_BETA: HeaderName = HeaderName::from_static("openai-beta");
const OPENAI_ORGANIZATION: HeaderName = HeaderName::from_static("openai-organization");
const OPENAI_PROJECT: HeaderName = HeaderName::from_static("openai-project");
const SEC_WEBSOCKET_PROTOCOL: HeaderName = HeaderName::from_static("sec-websocket-protocol");

/// Characters escaped inside one URL path segment. `@`, `.`, `-`, `_`, `~`
/// and `=` stay literal (Vertex model ids such as `claude-opus-4-5@20251101`
/// are written unescaped in Google's own URLs); `:` is escaped because the
/// Google APIs use it to separate the resource from the method.
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b':')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Characters escaped inside one query-string value.
const QUERY_VALUE: &AsciiSet = &PATH_SEGMENT.add(b'&').add(b'=').add(b'+');

/// A fully described upstream HTTP request, minus the body.
#[derive(Clone, PartialEq, Eq)]
pub struct BuiltRequest {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    /// The target authenticates with a service account: the caller must mint
    /// an OAuth access token and install it with [`BuiltRequest::set_bearer`]
    /// before sending.
    pub needs_access_token: bool,
}

impl BuiltRequest {
    /// Installs `Authorization: Bearer <token>`.
    pub fn set_bearer(&mut self, token: &str) -> Result<(), UpstreamError> {
        self.headers
            .insert(AUTHORIZATION, secret_value(&format!("Bearer {token}"))?);
        self.needs_access_token = false;
        Ok(())
    }

    /// A header value as text, if present and printable.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

impl fmt::Debug for BuiltRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(String, String)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let shown = v.to_str().unwrap_or("<binary>");
                (
                    k.as_str().to_string(),
                    display_header_value(k.as_str(), shown),
                )
            })
            .collect();
        f.debug_struct("BuiltRequest")
            .field("method", &self.method)
            .field("url", &redact_url(&self.url))
            .field("headers", &headers)
            .field("needs_access_token", &self.needs_access_token)
            .finish()
    }
}

/// Renders a URL for logs and error messages: credentials in the authority
/// are dropped and the query string (which may carry keys) is replaced by a
/// marker.
pub fn redact_url(raw: &str) -> String {
    let raw = raw.trim();
    match url::Url::parse(raw) {
        Ok(u) => {
            let mut out = format!("{}://{}", u.scheme(), u.host_str().unwrap_or(""));
            if let Some(port) = u.port() {
                out.push(':');
                out.push_str(&port.to_string());
            }
            out.push_str(u.path());
            if u.query().is_some() {
                out.push_str("?<redacted>");
            }
            out
        }
        Err(_) => match raw.split_once('?') {
            Some((head, _)) => format!("{head}?<redacted>"),
            None => raw.to_string(),
        },
    }
}

/// An error raised before any network I/O because the request cannot be
/// built. Classified as a transport failure: the client's request is not at
/// fault and another provider may still be able to serve it.
pub(crate) fn local_error(message: impl Into<String>) -> UpstreamError {
    UpstreamError::transport(message)
}

fn credential_error(message: impl Into<String>) -> UpstreamError {
    UpstreamError {
        status: 0,
        class: FailureClass::Auth,
        info: UpstreamErrorInfo {
            message: message.into(),
            ..UpstreamErrorInfo::default()
        },
        retry_after_ms: None,
        body: None,
        content_type: None,
    }
}

fn secret_value(value: &str) -> Result<HeaderValue, UpstreamError> {
    let mut v = HeaderValue::from_str(value).map_err(|_| {
        credential_error(
            "the upstream credential contains characters that cannot be sent in an HTTP header",
        )
    })?;
    v.set_sensitive(true);
    Ok(v)
}

// ---------------------------------------------------------------------------
// URLs
// ---------------------------------------------------------------------------

/// A base URL split into the part paths are appended to and its query.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Root {
    /// Scheme, authority and path, without a trailing slash.
    base: String,
    /// Query string of the configured base URL (kept on every request, as
    /// the OpenAI SDKs do; Azure-style `?api-version=` relies on it).
    query: Option<String>,
    /// Last non-empty path segment, lower-cased.
    last_segment: Option<String>,
    host: String,
}

fn split_base(raw: &str, provider: &str) -> Result<Root, UpstreamError> {
    let raw = raw.trim();
    let parsed = url::Url::parse(raw)
        .map_err(|_| local_error(format!("provider `{provider}` has an invalid base URL")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(local_error(format!(
            "provider `{provider}`: base URL must start with http:// or https://"
        )));
    }
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    if host.is_empty() {
        return Err(local_error(format!(
            "provider `{provider}`: base URL has no host"
        )));
    }
    let without_fragment = raw.split_once('#').map_or(raw, |(head, _)| head);
    let (path_part, query) = match without_fragment.split_once('?') {
        Some((head, q)) if !q.is_empty() => (head, Some(q.to_string())),
        Some((head, _)) => (head, None),
        None => (without_fragment, None),
    };
    let last_segment = parsed
        .path_segments()
        .and_then(|mut segments| segments.rfind(|s| !s.is_empty()))
        .map(str::to_ascii_lowercase);
    Ok(Root {
        base: path_part.trim_end_matches('/').to_string(),
        query,
        last_segment,
        host,
    })
}

/// `v1`, `v1beta`, `v1beta1`, `v2`, `v4`, …
fn is_version_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    chars.next() == Some('v')
        && chars.next().is_some_and(|c| c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_alphanumeric())
}

impl Root {
    fn ends_with_version(&self) -> bool {
        self.last_segment.as_deref().is_some_and(is_version_segment)
    }

    fn push(&mut self, segment: &str) {
        self.base.push('/');
        self.base.push_str(segment);
        self.last_segment = Some(segment.to_ascii_lowercase());
    }

    /// Appends a relative path and merges query strings.
    fn join(&self, relative: &str, extra_query: Option<&str>) -> String {
        let (rel_path, rel_query) = match relative.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (relative, None),
        };
        let mut url = format!("{}/{}", self.base, rel_path.trim_start_matches('/'));
        let mut first = true;
        for q in [self.query.as_deref(), rel_query, extra_query]
            .into_iter()
            .flatten()
        {
            let q = q.trim_start_matches('?').trim_matches('&');
            if q.is_empty() {
                continue;
            }
            url.push(if first { '?' } else { '&' });
            url.push_str(q);
            first = false;
        }
        url
    }
}

/// Whether a Vertex target addresses a partner Claude model (Anthropic
/// Messages body, `publishers/anthropic`, `rawPredict`).
fn vertex_is_anthropic(target: &Target) -> bool {
    target.protocol == Protocol::Anthropic
}

/// Whether `model` on Vertex AI is a Claude model, which must be spoken to
/// in the Anthropic Messages protocol.
pub fn is_vertex_anthropic_model(model: &str) -> bool {
    let id = model.trim().trim_matches('/');
    if id.to_ascii_lowercase().contains("publishers/anthropic/")
        || id.to_ascii_lowercase().starts_with("anthropic/")
    {
        return true;
    }
    let tail = id.rsplit('/').next().unwrap_or(id);
    tail.to_ascii_lowercase().starts_with("claude")
}

/// The protocol a Vertex AI model must be addressed in: Anthropic Messages
/// for Claude models, Gemini for everything else. The gateway should use this
/// when choosing the upstream protocol for a `vertex` provider.
pub fn vertex_protocol_for_model(model: &str) -> Protocol {
    if is_vertex_anthropic_model(model) {
        Protocol::Anthropic
    } else {
        Protocol::Gemini
    }
}

/// Host root for Vertex AI: the configured base URL when it is not the
/// default, otherwise derived from the location — the bare host for `global`,
/// `aiplatform.{us,eu}.rep.googleapis.com` for the multi-regions, and
/// `{region}-aiplatform.googleapis.com` for regions.
fn vertex_root(target: &Target, location: &str) -> Result<Root, UpstreamError> {
    let configured = target.base_url.trim().trim_end_matches('/');
    if !configured.is_empty() && !configured.eq_ignore_ascii_case(VERTEX_DEFAULT_BASE) {
        return split_base(configured, &target.provider);
    }
    if !location
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(local_error(format!(
            "provider `{}`: `{location}` is not a valid Vertex AI location",
            target.provider
        )));
    }
    let host = match location {
        "global" => VERTEX_DEFAULT_BASE.to_string(),
        "us" | "eu" => format!("https://aiplatform.{location}.rep.googleapis.com"),
        region => format!("https://{region}-aiplatform.googleapis.com"),
    };
    split_base(&host, &target.provider)
}

fn vertex_location(target: &Target) -> String {
    let l = target.location.trim().to_ascii_lowercase();
    if l.is_empty() {
        "global".to_string()
    } else {
        l
    }
}

fn vertex_project(target: &Target) -> String {
    let configured = target.project.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    match &target.auth {
        Auth::ServiceAccount(sa) => sa.project_id.clone(),
        _ => String::new(),
    }
}

/// Percent-encodes a slash separated resource path segment by segment.
fn encode_resource(path: &str, what: &str, provider: &str) -> Result<String, UpstreamError> {
    let mut out = String::with_capacity(path.len() + 8);
    for (i, segment) in path.split('/').enumerate() {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(local_error(format!(
                "provider `{provider}`: `{path}` is not a usable {what}"
            )));
        }
        if i > 0 {
            out.push('/');
        }
        out.extend(utf8_percent_encode(segment, PATH_SEGMENT));
    }
    Ok(out)
}

fn required_model(target: &Target) -> Result<&str, UpstreamError> {
    let model = target.model.trim().trim_matches('/');
    if model.is_empty() {
        return Err(local_error(format!(
            "provider `{}`: no upstream model id for this call",
            target.provider
        )));
    }
    Ok(model)
}

/// Gemini API resource name of the target model: `models/{id}` unless the id
/// already names a resource collection (`models/…`, `tunedModels/…`).
fn gemini_model_resource(target: &Target) -> Result<String, UpstreamError> {
    let model = required_model(target)?;
    let resource = if model.starts_with("models/") || model.starts_with("tunedModels/") {
        model.to_string()
    } else {
        format!("models/{model}")
    };
    encode_resource(&resource, "model id", &target.provider)
}

/// Vertex AI resource name of the target model.
///
/// * `projects/…` — already a full resource name (tuned endpoints);
/// * `publishers/…`, `endpoints/…` — relative to the project and location;
/// * `{publisher}/{model}` — shorthand for `publishers/{publisher}/models/{model}`;
/// * anything else — a model of `default_publisher`.
fn vertex_model_resource(
    target: &Target,
    model: &str,
    default_publisher: &str,
    location: &str,
) -> Result<String, UpstreamError> {
    let model = model.trim().trim_matches('/');
    if model.starts_with("projects/") {
        return encode_resource(model, "model id", &target.provider);
    }
    let tail = if model.starts_with("publishers/") || model.starts_with("endpoints/") {
        model.to_string()
    } else if let Some(rest) = model.strip_prefix("models/") {
        format!("publishers/{default_publisher}/models/{rest}")
    } else if let Some((publisher, name)) = model.split_once('/') {
        format!("publishers/{publisher}/models/{name}")
    } else {
        format!("publishers/{default_publisher}/models/{model}")
    };
    let project = vertex_project(target);
    let resource = if project.is_empty() {
        match &target.auth {
            // "Express mode": an API key addresses publisher models without
            // a project or location.
            Auth::ApiKey(_) => tail,
            _ => {
                return Err(local_error(format!(
                    "provider `{}`: a Vertex AI project is required",
                    target.provider
                )));
            }
        }
    } else {
        format!("projects/{project}/locations/{location}/{tail}")
    };
    encode_resource(&resource, "model id", &target.provider)
}

fn unsupported(target: &Target, what: &str) -> UpstreamError {
    local_error(format!(
        "provider `{}` ({}) does not support {what} over the {} protocol",
        target.provider, target.kind, target.protocol
    ))
}

/// The versioned API root of a target: what [`Operation::Raw`] paths are
/// relative to.
fn api_root(target: &Target) -> Result<Root, UpstreamError> {
    match target.kind {
        ProviderKind::Openai | ProviderKind::OpenaiCompat => {
            let mut root = split_base(&target.base_url, &target.provider)?;
            // `https://api.openai.com` and `https://api.openai.com/v1` both
            // work; a base with any other path (`/api/paas/v4`, an Azure
            // deployment URL) is taken as written.
            if root.last_segment.is_none() {
                root.push("v1");
            }
            Ok(root)
        }
        ProviderKind::Anthropic => {
            let mut root = split_base(&target.base_url, &target.provider)?;
            if root.last_segment.as_deref() != Some("v1") {
                root.push("v1");
            }
            Ok(root)
        }
        ProviderKind::Gemini => {
            let mut root = split_base(&target.base_url, &target.provider)?;
            if !root.ends_with_version() {
                root.push("v1beta");
            }
            Ok(root)
        }
        ProviderKind::Vertex => {
            let mut root = vertex_root(target, &vertex_location(target))?;
            if !root.ends_with_version() {
                root.push("v1");
            }
            Ok(root)
        }
        ProviderKind::Mock => Err(local_error(format!(
            "provider `{}` is the built-in mock and has no network endpoint",
            target.provider
        ))),
    }
}

fn sanitize_relative(target: &Target, path: &str) -> Result<String, UpstreamError> {
    let trimmed = path.trim().trim_start_matches('/');
    let (path_only, _) = trimmed.split_once('?').unwrap_or((trimmed, ""));
    let bad = path_only.is_empty()
        || path_only.contains("://")
        || path_only.contains('\\')
        || path_only.contains('#')
        || path_only
            .split('/')
            .any(|s| s == ".." || s == "." || percent_decode_str(s).decode_utf8_lossy() == "..");
    if bad {
        return Err(local_error(format!(
            "provider `{}`: `{path}` is not a valid relative endpoint path",
            target.provider
        )));
    }
    let mut rel = trimmed;
    if matches!(
        target.kind,
        ProviderKind::Openai | ProviderKind::OpenaiCompat
    ) {
        // Callers sometimes pass the client-facing route (`v1/embeddings`);
        // the version already lives in the root.
        if let Some(rest) = rel.strip_prefix("v1/") {
            rel = rest;
        }
    }
    Ok(rel.to_string())
}

fn endpoint(target: &Target, op: &Operation) -> Result<(Method, String), UpstreamError> {
    let root = api_root(target)?;
    if let Operation::Raw {
        method,
        path,
        query,
    } = op
    {
        let rel = sanitize_relative(target, path)?;
        return Ok((method.clone(), root.join(&rel, query.as_deref())));
    }
    let openai = matches!(
        target.kind,
        ProviderKind::Openai | ProviderKind::OpenaiCompat
    );
    match (target.kind, target.protocol, op) {
        // ---- OpenAI and compatibles ---------------------------------
        (_, Protocol::OpenaiChat, Operation::Generate { .. }) if openai => {
            Ok((Method::POST, root.join("chat/completions", None)))
        }
        (_, Protocol::OpenaiResponses, Operation::Generate { .. }) if openai => {
            Ok((Method::POST, root.join("responses", None)))
        }
        (_, Protocol::OpenaiResponses, Operation::CountTokens) if openai => {
            Ok((Method::POST, root.join("responses/input_tokens", None)))
        }
        (_, _, Operation::ListModels) if openai => Ok((Method::GET, root.join("models", None))),

        // ---- Anthropic ----------------------------------------------
        (ProviderKind::Anthropic, Protocol::Anthropic, Operation::Generate { .. }) => {
            Ok((Method::POST, root.join("messages", None)))
        }
        (ProviderKind::Anthropic, Protocol::Anthropic, Operation::CountTokens) => {
            Ok((Method::POST, root.join("messages/count_tokens", None)))
        }
        (ProviderKind::Anthropic, _, Operation::ListModels) => {
            Ok((Method::GET, root.join("models", Some("limit=1000"))))
        }

        // ---- Gemini API ---------------------------------------------
        (ProviderKind::Gemini, Protocol::Gemini, Operation::Generate { stream }) => {
            let model = gemini_model_resource(target)?;
            Ok(if *stream {
                (
                    Method::POST,
                    root.join(&format!("{model}:streamGenerateContent"), Some("alt=sse")),
                )
            } else {
                (
                    Method::POST,
                    root.join(&format!("{model}:generateContent"), None),
                )
            })
        }
        (ProviderKind::Gemini, Protocol::Gemini, Operation::CountTokens) => {
            let model = gemini_model_resource(target)?;
            Ok((
                Method::POST,
                root.join(&format!("{model}:countTokens"), None),
            ))
        }
        (ProviderKind::Gemini, _, Operation::ListModels) => {
            Ok((Method::GET, root.join("models", Some("pageSize=1000"))))
        }

        // ---- Vertex AI ----------------------------------------------
        (ProviderKind::Vertex, Protocol::Gemini | Protocol::Anthropic, op) => {
            vertex_endpoint(target, op)
        }

        _ => Err(unsupported(
            target,
            match op {
                Operation::Generate { .. } => "generation",
                Operation::CountTokens => "token counting",
                Operation::ListModels => "model listing",
                Operation::Raw { .. } => "raw requests",
            },
        )),
    }
}

fn vertex_endpoint(target: &Target, op: &Operation) -> Result<(Method, String), UpstreamError> {
    let location = vertex_location(target);
    let root = api_root(target)?;
    if matches!(op, Operation::ListModels) {
        // The publisher model catalogue only exists in the v1beta1 API.
        let mut list_root = vertex_root(target, &location)?;
        if list_root.ends_with_version() {
            // A custom base that pins a version: list under it.
            return Ok((
                Method::GET,
                list_root.join("publishers/google/models", Some("pageSize=1000")),
            ));
        }
        list_root.push("v1beta1");
        return Ok((
            Method::GET,
            list_root.join("publishers/google/models", Some("pageSize=1000")),
        ));
    }

    if vertex_is_anthropic(target) {
        let (model, action) = match op {
            Operation::Generate { stream: false } => (required_model(target)?, "rawPredict"),
            Operation::Generate { stream: true } => (required_model(target)?, "streamRawPredict"),
            // Token counting has one shared endpoint; the model travels in
            // the body.
            Operation::CountTokens => ("count-tokens", "rawPredict"),
            _ => return Err(unsupported(target, "this operation")),
        };
        let resource = vertex_model_resource(target, model, "anthropic", &location)?;
        return Ok((
            Method::POST,
            root.join(&format!("{resource}:{action}"), None),
        ));
    }

    let model = required_model(target)?;
    if is_vertex_anthropic_model(model) {
        // Sending a Gemini-shaped body to a Claude model can only fail with
        // an opaque vendor error; say what is wrong instead.
        return Err(local_error(format!(
            "provider `{}`: Claude models on Vertex AI must be addressed with the anthropic protocol",
            target.provider
        )));
    }
    let resource = vertex_model_resource(target, model, "google", &location)?;
    match op {
        Operation::Generate { stream: false } => Ok((
            Method::POST,
            root.join(&format!("{resource}:generateContent"), None),
        )),
        Operation::Generate { stream: true } => Ok((
            Method::POST,
            root.join(
                &format!("{resource}:streamGenerateContent"),
                Some("alt=sse"),
            ),
        )),
        Operation::CountTokens => Ok((
            Method::POST,
            root.join(&format!("{resource}:countTokens"), None),
        )),
        _ => Err(unsupported(target, "this operation")),
    }
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

/// Headers a provider configuration may not set: they describe the
/// connection or the body framing and are owned by the HTTP client.
fn is_transport_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "upgrade"
            | "te"
            | "trailer"
            | "keep-alive"
            | "proxy-authorization"
            | "proxy-connection"
    )
}

fn client_values(headers: &HeaderMap, name: &HeaderName) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

fn first_client_value(headers: &HeaderMap, name: &HeaderName) -> Option<HeaderValue> {
    client_values(headers, name)
        .into_iter()
        .next()
        .and_then(|v| HeaderValue::from_str(&v).ok())
}

/// Splits comma separated token lists into their tokens, keeping first
/// occurrences.
fn merge_tokens_list<'a>(lists: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for list in lists {
        for token in list.split(',') {
            let token = token.trim();
            if !token.is_empty() && !out.iter().any(|t| t.eq_ignore_ascii_case(token)) {
                out.push(token);
            }
        }
    }
    out
}

/// Merges comma separated token lists, keeping first occurrences.
fn merge_tokens<'a>(lists: impl IntoIterator<Item = &'a str>) -> String {
    merge_tokens_list(lists).join(",")
}

/// Which family of vendor headers applies to a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialect {
    Openai,
    /// First-party (or compatible) Anthropic Messages API.
    Anthropic,
    /// Claude on Vertex AI: Anthropic body, Google transport.
    VertexAnthropic,
    Google,
}

fn dialect(target: &Target) -> Dialect {
    match target.kind {
        ProviderKind::Openai | ProviderKind::OpenaiCompat => Dialect::Openai,
        ProviderKind::Anthropic => Dialect::Anthropic,
        ProviderKind::Vertex if vertex_is_anthropic(target) => Dialect::VertexAnthropic,
        ProviderKind::Gemini | ProviderKind::Vertex | ProviderKind::Mock => Dialect::Google,
    }
}

/// Installs the credential and returns the header names it occupies, plus
/// whether an access token still has to be minted.
fn apply_auth(
    target: &Target,
    headers: &mut HeaderMap,
) -> Result<(Vec<HeaderName>, bool), UpstreamError> {
    let key = target.auth.api_key();
    match (&target.auth, target.kind) {
        (Auth::ServiceAccount(_), ProviderKind::Vertex) => {
            // Reserved now, filled in once the token is minted.
            Ok((vec![AUTHORIZATION], true))
        }
        (Auth::ServiceAccount(_), _) => Err(credential_error(format!(
            "provider `{}`: service accounts are only supported by vertex providers",
            target.provider
        ))),
        (_, ProviderKind::Openai | ProviderKind::OpenaiCompat) => match key {
            Some(k) => {
                headers.insert(AUTHORIZATION, secret_value(&format!("Bearer {k}"))?);
                Ok((vec![AUTHORIZATION], false))
            }
            None => Ok((Vec::new(), false)),
        },
        (_, ProviderKind::Anthropic) => match key {
            Some(k) => {
                headers.insert(X_API_KEY, secret_value(k)?);
                let first_party = split_base(&target.base_url, &target.provider)
                    .map(|r| r.host == "api.anthropic.com")
                    .unwrap_or(false);
                if first_party {
                    Ok((vec![X_API_KEY], false))
                } else {
                    // Anthropic-compatible gateways disagree on which header
                    // they read; the first-party API accepts either, so
                    // third parties get both.
                    headers.insert(AUTHORIZATION, secret_value(&format!("Bearer {k}"))?);
                    Ok((vec![X_API_KEY, AUTHORIZATION], false))
                }
            }
            None => Ok((Vec::new(), false)),
        },
        (_, ProviderKind::Gemini | ProviderKind::Vertex) => match key {
            Some(k) => {
                headers.insert(X_GOOG_API_KEY, secret_value(k)?);
                Ok((vec![X_GOOG_API_KEY], false))
            }
            None => Ok((Vec::new(), false)),
        },
        (_, ProviderKind::Mock) => Ok((Vec::new(), false)),
    }
}

/// Applies the provider's configured headers on top of everything else.
fn apply_config_headers(target: &Target, headers: &mut HeaderMap, reserved: &[HeaderName]) {
    for (raw_name, raw_value) in &target.headers {
        let (raw_name, raw_value) = (raw_name.trim(), raw_value.trim());
        if raw_name.is_empty() || raw_value.is_empty() {
            continue;
        }
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(raw_name.as_bytes()),
            HeaderValue::from_str(raw_value),
        ) else {
            tracing::warn!(
                provider = %target.provider,
                header = %raw_name,
                "ignoring configured header that is not valid HTTP"
            );
            continue;
        };
        if is_transport_header(name.as_str()) || reserved.contains(&name) {
            tracing::debug!(
                provider = %target.provider,
                header = %name,
                "configured header is managed by the gateway and was not applied"
            );
            continue;
        }
        if name == ANTHROPIC_BETA {
            let existing = headers
                .get(&ANTHROPIC_BETA)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let merged = merge_tokens([raw_value, existing.as_str()]);
            if let Ok(v) = HeaderValue::from_str(&merged) {
                headers.insert(ANTHROPIC_BETA, v);
            }
            continue;
        }
        let mut value = value;
        // Operators put gateway credentials here under names nobody can
        // enumerate; only values of known-harmless headers are left
        // printable by the HTTP stack's own debug output.
        if is_confidential_header(name.as_str()) {
            value.set_sensitive(true);
        }
        headers.insert(name, value);
    }
}

/// Forwards the allow-listed client headers that make sense for `dialect`.
fn forward_client_headers(dialect: Dialect, client: &HeaderMap, headers: &mut HeaderMap) {
    match dialect {
        Dialect::Anthropic | Dialect::VertexAnthropic => {
            let betas = client_values(client, &ANTHROPIC_BETA);
            let merged = merge_tokens(betas.iter().map(String::as_str));
            if !merged.is_empty()
                && let Ok(v) = HeaderValue::from_str(&merged)
            {
                headers.insert(ANTHROPIC_BETA, v);
            }
            // On Vertex the API version is a body field, not a header.
            if dialect == Dialect::Anthropic
                && let Some(v) = first_client_value(client, &ANTHROPIC_VERSION_HEADER)
            {
                headers.insert(ANTHROPIC_VERSION_HEADER, v);
            }
        }
        Dialect::Openai => {
            for name in [OPENAI_BETA, OPENAI_ORGANIZATION, OPENAI_PROJECT] {
                if let Some(v) = first_client_value(client, &name) {
                    headers.insert(name, v);
                }
            }
        }
        Dialect::Google => {}
    }
}

/// Describes the HTTP request for `op` against `target`.
///
/// `body` is only inspected for emptiness (to decide whether a
/// `content-type` is needed); `client_headers` are the headers of the
/// client's request to the gateway, of which a small allow-list is forwarded.
///
/// For a service-account target the `Authorization` header is left out and
/// [`BuiltRequest::needs_access_token`] is set.
pub fn build_request(
    target: &Target,
    op: &Operation,
    body: &[u8],
    client_headers: &HeaderMap,
) -> Result<BuiltRequest, UpstreamError> {
    let (method, url) = endpoint(target, op)?;
    let dialect = dialect(target);
    let mut headers = HeaderMap::new();

    let has_body = !body.is_empty() || matches!(method, Method::POST | Method::PUT | Method::PATCH);
    let is_raw = matches!(op, Operation::Raw { .. });
    if has_body {
        let content_type = if is_raw {
            first_client_value(client_headers, &CONTENT_TYPE)
        } else {
            None
        };
        headers.insert(
            CONTENT_TYPE,
            content_type.unwrap_or_else(|| HeaderValue::from_static("application/json")),
        );
    }
    if op.is_stream() {
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        // Compressed event streams get buffered by intermediaries, which
        // defeats streaming; ask for the bytes as they are.
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    } else if is_raw {
        // Binary endpoints (speech, images) decide their own media type.
        if let Some(accept) = first_client_value(client_headers, &ACCEPT) {
            headers.insert(ACCEPT, accept);
        }
    } else {
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    }
    headers.insert(USER_AGENT_HEADER, HeaderValue::from_static(USER_AGENT));
    if dialect == Dialect::Anthropic {
        headers.insert(
            ANTHROPIC_VERSION_HEADER,
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
    }

    forward_client_headers(dialect, client_headers, &mut headers);
    let (reserved, needs_access_token) = apply_auth(target, &mut headers)?;
    apply_config_headers(target, &mut headers, &reserved);

    Ok(BuiltRequest {
        method,
        url,
        headers,
        needs_access_token,
    })
}

/// Headers a caller may never pass through to an upstream WebSocket
/// handshake: credentials, and everything the handshake itself owns.
fn is_forbidden_ws_header(name: &str) -> bool {
    is_transport_header(name)
        || matches!(
            name,
            "authorization"
                | "x-api-key"
                | "x-goog-api-key"
                | "cookie"
                | "x-request-id"
                | "sec-websocket-key"
                | "sec-websocket-version"
                | "sec-websocket-accept"
                // Compression is not negotiated by this client; relaying the
                // caller's offer would corrupt framing.
                | "sec-websocket-extensions"
        )
}

/// Describes the WebSocket handshake for `path_and_query` (relative to the
/// provider's versioned API root, e.g. `responses` or
/// `realtime?model=gpt-realtime`).
///
/// `extra_headers` are headers the caller wants on the handshake — typically
/// a vetted subset of the client's own (`openai-beta`,
/// `sec-websocket-protocol`, `openai-safety-identifier`). Credentials and
/// handshake-owned headers among them are dropped, as is any
/// `openai-insecure-api-key.*` subprotocol (it would carry the *client's*
/// key upstream).
///
/// The handshake is well-formed whatever the caller passes: subprotocols
/// offered on several `sec-websocket-protocol` lines leave as one header,
/// and a `user-agent` among the extra headers replaces the gateway's own
/// instead of being sent next to it. Headers from the provider configuration
/// are applied last, as for HTTP requests.
pub fn build_ws_request(
    target: &Target,
    path_and_query: &str,
    extra_headers: &HeaderMap,
) -> Result<BuiltRequest, UpstreamError> {
    let root = api_root(target)?;
    let rel = sanitize_relative(target, path_and_query)?;
    let http_url = root.join(&rel, None);
    let url = if let Some(rest) = http_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        // `split_base` only admits http and https, compared case-sensitively
        // here; normalise the rare upper-case scheme.
        let (scheme, rest) = http_url.split_once("://").unwrap_or(("", &http_url));
        match scheme.to_ascii_lowercase().as_str() {
            "https" => format!("wss://{rest}"),
            "http" => format!("ws://{rest}"),
            _ => {
                return Err(local_error(format!(
                    "provider `{}`: base URL must start with http:// or https://",
                    target.provider
                )));
            }
        }
    };

    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT_HEADER, HeaderValue::from_static(USER_AGENT));
    for name in extra_headers.keys() {
        if is_forbidden_ws_header(name.as_str()) {
            continue;
        }
        let values = extra_headers.get_all(name);
        if *name == SEC_WEBSOCKET_PROTOCOL {
            // The offer may arrive on several header lines (RFC 6455 §11.3.4);
            // it leaves on one. The WebSocket library only looks at the
            // first line when it checks which subprotocol the server chose,
            // so an offer split over lines would make it reject a choice the
            // upstream was entitled to make.
            let lines: Vec<&str> = values.iter().filter_map(|v| v.to_str().ok()).collect();
            let offered: Vec<&str> = merge_tokens_list(lines.iter().copied())
                .into_iter()
                .filter(|p| {
                    !p.to_ascii_lowercase()
                        .starts_with("openai-insecure-api-key.")
                })
                .collect();
            if let Some(v) = (!offered.is_empty())
                .then(|| HeaderValue::from_str(&offered.join(", ")).ok())
                .flatten()
            {
                headers.insert(name.clone(), v);
            }
            continue;
        }
        // The caller's first value replaces a built-in default (so the
        // handshake never carries two `User-Agent` lines); further values of
        // the same name are the caller's own repetition and are kept.
        for (index, value) in values.iter().enumerate() {
            if index == 0 {
                headers.insert(name.clone(), value.clone());
            } else if *name != USER_AGENT_HEADER {
                headers.append(name.clone(), value.clone());
            }
        }
    }
    let (reserved, needs_access_token) = apply_auth(target, &mut headers)?;
    apply_config_headers(target, &mut headers, &reserved);

    Ok(BuiltRequest {
        method: Method::GET,
        url,
        headers,
        needs_access_token,
    })
}

/// The URL of one page of a provider's model listing.
pub(crate) fn list_models_page_url(
    target: &Target,
    page_param: Option<(&str, &str)>,
) -> Result<String, UpstreamError> {
    let (_, mut url) = endpoint(target, &Operation::ListModels)?;
    if let Some((name, value)) = page_param {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(name);
        url.push('=');
        url.extend(utf8_percent_encode(value, QUERY_VALUE));
    }
    Ok(url)
}

// ---------------------------------------------------------------------------
// Body adaptation
// ---------------------------------------------------------------------------

/// Rewrites an Anthropic Messages body for Claude on Vertex AI.
///
/// Vertex differs from the first-party API in two ways: the API version is a
/// body field (`anthropic_version: "vertex-2023-10-16"`) instead of a header,
/// and the model is named in the URL, so `model` must not be in the body.
/// Token counting is the exception — its URL is shared, and the model stays
/// in the body.
///
/// Bodies that are not JSON objects are left untouched.
pub fn adapt_vertex_anthropic_body(body: &mut Value, op: &Operation) {
    let Value::Object(map) = body else {
        return;
    };
    let keep_model = matches!(op, Operation::CountTokens);
    let mut out = Map::with_capacity(map.len() + 1);
    out.insert(
        "anthropic_version".to_string(),
        Value::String(VERTEX_ANTHROPIC_VERSION.to_string()),
    );
    for (key, value) in std::mem::take(map) {
        if key == "anthropic_version" || (key == "model" && !keep_model) {
            continue;
        }
        out.insert(key, value);
    }
    *map = out;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vertex::ServiceAccount;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use switchyard_core::config::ProxySetting;

    const KEY: &str = "sk-test-key-0123456789";

    fn target(kind: ProviderKind, protocol: Protocol, base: &str, model: &str) -> Target {
        Target {
            provider: "p".into(),
            kind,
            base_url: base.into(),
            protocol,
            model: model.into(),
            auth: Auth::ApiKey(KEY.into()),
            headers: Vec::new(),
            proxy: ProxySetting::Inherit,
            project: String::new(),
            location: String::new(),
        }
    }

    fn service_account() -> Arc<ServiceAccount> {
        let pem = include_str!("../tests/fixtures/test_rsa_pkcs8.pem");
        let json = json!({
            "type": "service_account",
            "project_id": "sa-project",
            "private_key_id": "kid-1",
            "private_key": pem,
            "client_email": "svc@sa-project.iam.gserviceaccount.com",
            "token_uri": "https://oauth2.googleapis.com/token"
        });
        Arc::new(ServiceAccount::from_json(&json.to_string()).unwrap())
    }

    fn build(t: &Target, op: Operation) -> BuiltRequest {
        build_request(t, &op, b"{}", &HeaderMap::new()).unwrap()
    }

    const GENERATE: Operation = Operation::Generate { stream: false };
    const STREAM: Operation = Operation::Generate { stream: true };

    // ------------------------------------------------------------- URLs

    #[test]
    fn openai_urls_for_every_operation() {
        let cases: [(Protocol, Operation, &str, Method); 6] = [
            (
                Protocol::OpenaiChat,
                GENERATE,
                "/chat/completions",
                Method::POST,
            ),
            (
                Protocol::OpenaiChat,
                STREAM,
                "/chat/completions",
                Method::POST,
            ),
            (
                Protocol::OpenaiResponses,
                GENERATE,
                "/responses",
                Method::POST,
            ),
            (
                Protocol::OpenaiResponses,
                STREAM,
                "/responses",
                Method::POST,
            ),
            (
                Protocol::OpenaiResponses,
                Operation::CountTokens,
                "/responses/input_tokens",
                Method::POST,
            ),
            (
                Protocol::OpenaiChat,
                Operation::ListModels,
                "/models",
                Method::GET,
            ),
        ];
        for kind in [ProviderKind::Openai, ProviderKind::OpenaiCompat] {
            for (protocol, op, suffix, method) in cases.clone() {
                let t = target(kind, protocol, "https://api.openai.com/v1", "gpt-5");
                let r = build(&t, op.clone());
                assert_eq!(
                    r.url,
                    format!("https://api.openai.com/v1{suffix}"),
                    "{kind} {op:?}"
                );
                assert_eq!(r.method, method);
            }
        }
    }

    #[test]
    fn openai_base_url_variants_never_double_the_version() {
        let cases = [
            (
                "https://api.openai.com",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "https://api.openai.com/",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "https://api.openai.com/v1",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "https://api.openai.com/v1/",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "http://localhost:11434/v1",
                "http://localhost:11434/v1/chat/completions",
            ),
            (
                "http://localhost:8000",
                "http://localhost:8000/v1/chat/completions",
            ),
            (
                "https://openrouter.ai/api/v1",
                "https://openrouter.ai/api/v1/chat/completions",
            ),
            (
                "https://open.bigmodel.cn/api/paas/v4",
                "https://open.bigmodel.cn/api/paas/v4/chat/completions",
            ),
            (
                "https://generativelanguage.googleapis.com/v1beta/openai/",
                "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions",
            ),
            (
                "https://res.openai.azure.com/openai/v1?api-version=preview",
                "https://res.openai.azure.com/openai/v1/chat/completions?api-version=preview",
            ),
        ];
        for (base, expected) in cases {
            let t = target(ProviderKind::OpenaiCompat, Protocol::OpenaiChat, base, "m");
            assert_eq!(build(&t, GENERATE).url, expected, "base {base}");
        }
    }

    #[test]
    fn openai_chat_has_no_count_tokens_endpoint() {
        let t = target(
            ProviderKind::Openai,
            Protocol::OpenaiChat,
            "https://api.openai.com/v1",
            "gpt-5",
        );
        let err = build_request(&t, &Operation::CountTokens, b"{}", &HeaderMap::new()).unwrap_err();
        assert_eq!(err.status, 0);
        assert!(
            err.info.message.contains("token counting"),
            "{}",
            err.info.message
        );
    }

    #[test]
    fn raw_paths_are_relative_to_the_versioned_root() {
        let t = target(
            ProviderKind::Openai,
            Protocol::OpenaiChat,
            "https://api.openai.com/v1",
            "m",
        );
        let op = |path: &str, query: Option<&str>| Operation::Raw {
            method: Method::POST,
            path: path.into(),
            query: query.map(str::to_string),
        };
        assert_eq!(
            build(&t, op("embeddings", None)).url,
            "https://api.openai.com/v1/embeddings"
        );
        assert_eq!(
            build(&t, op("/embeddings", None)).url,
            "https://api.openai.com/v1/embeddings"
        );
        assert_eq!(
            build(&t, op("v1/images/generations", None)).url,
            "https://api.openai.com/v1/images/generations"
        );
        assert_eq!(
            build(&t, op("audio/speech", Some("a=1&b=2"))).url,
            "https://api.openai.com/v1/audio/speech?a=1&b=2"
        );
        let get = Operation::Raw {
            method: Method::GET,
            path: "responses/resp_1".into(),
            query: None,
        };
        let r = build_request(&t, &get, b"", &HeaderMap::new()).unwrap();
        assert_eq!(r.method, Method::GET);
        assert_eq!(r.url, "https://api.openai.com/v1/responses/resp_1");
        assert!(r.headers.get(CONTENT_TYPE).is_none());

        let a = target(
            ProviderKind::Anthropic,
            Protocol::Anthropic,
            "https://api.anthropic.com",
            "m",
        );
        assert_eq!(
            build(&a, op("messages/batches", None)).url,
            "https://api.anthropic.com/v1/messages/batches"
        );
        let g = target(
            ProviderKind::Gemini,
            Protocol::Gemini,
            "https://generativelanguage.googleapis.com",
            "m",
        );
        assert_eq!(
            build(&g, op("models/embedding-001:embedContent", None)).url,
            "https://generativelanguage.googleapis.com/v1beta/models/embedding-001:embedContent"
        );
    }

    #[test]
    fn raw_paths_cannot_escape_the_root() {
        let t = target(
            ProviderKind::Openai,
            Protocol::OpenaiChat,
            "https://api.openai.com/v1",
            "m",
        );
        for path in [
            "",
            "../admin",
            "a/../../b",
            "a/%2e%2e/b",
            "https://evil.test/x",
            "a\\b",
            "./x",
        ] {
            let op = Operation::Raw {
                method: Method::POST,
                path: path.into(),
                query: None,
            };
            assert!(
                build_request(&t, &op, b"{}", &HeaderMap::new()).is_err(),
                "path {path:?} should be rejected"
            );
        }
    }

    #[test]
    fn anthropic_urls() {
        for base in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com/v1/",
        ] {
            let t = target(
                ProviderKind::Anthropic,
                Protocol::Anthropic,
                base,
                "claude-opus-5",
            );
            assert_eq!(
                build(&t, GENERATE).url,
                "https://api.anthropic.com/v1/messages"
            );
            assert_eq!(
                build(&t, STREAM).url,
                "https://api.anthropic.com/v1/messages"
            );
            assert_eq!(
                build(&t, Operation::CountTokens).url,
                "https://api.anthropic.com/v1/messages/count_tokens"
            );
            let list = build_request(&t, &Operation::ListModels, b"", &HeaderMap::new()).unwrap();
            assert_eq!(list.url, "https://api.anthropic.com/v1/models?limit=1000");
            assert_eq!(list.method, Method::GET);
        }
        let t = target(
            ProviderKind::Anthropic,
            Protocol::Anthropic,
            "https://api.deepseek.com/anthropic",
            "deepseek-chat",
        );
        assert_eq!(
            build(&t, GENERATE).url,
            "https://api.deepseek.com/anthropic/v1/messages"
        );
    }

    #[test]
    fn gemini_urls() {
        let base = "https://generativelanguage.googleapis.com";
        let t = target(
            ProviderKind::Gemini,
            Protocol::Gemini,
            base,
            "gemini-3.8-flash",
        );
        assert_eq!(
            build(&t, GENERATE).url,
            format!("{base}/v1beta/models/gemini-3.8-flash:generateContent")
        );
        assert_eq!(
            build(&t, STREAM).url,
            format!("{base}/v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse")
        );
        assert_eq!(
            build(&t, Operation::CountTokens).url,
            format!("{base}/v1beta/models/gemini-3.8-flash:countTokens")
        );
        let list = build_request(&t, &Operation::ListModels, b"", &HeaderMap::new()).unwrap();
        assert_eq!(list.url, format!("{base}/v1beta/models?pageSize=1000"));
        assert_eq!(list.method, Method::GET);
    }

    #[test]
    fn gemini_base_with_version_is_not_doubled() {
        for base in [
            "https://generativelanguage.googleapis.com/v1beta",
            "https://generativelanguage.googleapis.com/v1beta/",
        ] {
            let t = target(
                ProviderKind::Gemini,
                Protocol::Gemini,
                base,
                "gemini-2.5-pro",
            );
            assert_eq!(
                build(&t, GENERATE).url,
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent"
            );
        }
        let t = target(
            ProviderKind::Gemini,
            Protocol::Gemini,
            "https://generativelanguage.googleapis.com/v1",
            "gemini-2.5-pro",
        );
        assert_eq!(
            build(&t, GENERATE).url,
            "https://generativelanguage.googleapis.com/v1/models/gemini-2.5-pro:generateContent"
        );
    }

    #[test]
    fn gemini_model_resource_names() {
        let base = "https://generativelanguage.googleapis.com";
        let url = |model: &str| {
            build(
                &target(ProviderKind::Gemini, Protocol::Gemini, base, model),
                GENERATE,
            )
            .url
        };
        // Already a resource name: `models/` is not doubled.
        assert_eq!(
            url("models/gemini-2.5-flash"),
            format!("{base}/v1beta/models/gemini-2.5-flash:generateContent")
        );
        // Tuned models live in their own collection.
        assert_eq!(
            url("tunedModels/my-tuned-model-123"),
            format!("{base}/v1beta/tunedModels/my-tuned-model-123:generateContent")
        );
        // Odd characters are escaped per segment; the method separator is
        // the only literal colon.
        assert_eq!(
            url("weird model:v1?x#y"),
            format!("{base}/v1beta/models/weird%20model%3Av1%3Fx%23y:generateContent")
        );
        for bad in ["", "  ", "models//x", "a/../b"] {
            let t = target(ProviderKind::Gemini, Protocol::Gemini, base, bad);
            assert!(
                build_request(&t, &GENERATE, b"{}", &HeaderMap::new()).is_err(),
                "model {bad:?}"
            );
        }
    }

    fn vertex(protocol: Protocol, model: &str, location: &str) -> Target {
        let mut t = target(
            ProviderKind::Vertex,
            protocol,
            "https://aiplatform.googleapis.com",
            model,
        );
        t.auth = Auth::ServiceAccount(service_account());
        t.location = location.into();
        t
    }

    #[test]
    fn vertex_hosts_by_location() {
        let cases = [
            ("global", "https://aiplatform.googleapis.com", "global"),
            ("", "https://aiplatform.googleapis.com", "global"),
            (
                "us-central1",
                "https://us-central1-aiplatform.googleapis.com",
                "us-central1",
            ),
            (
                "europe-west4",
                "https://europe-west4-aiplatform.googleapis.com",
                "europe-west4",
            ),
            ("us", "https://aiplatform.us.rep.googleapis.com", "us"),
            ("eu", "https://aiplatform.eu.rep.googleapis.com", "eu"),
            (
                "US-EAST5",
                "https://us-east5-aiplatform.googleapis.com",
                "us-east5",
            ),
        ];
        for (location, host, segment) in cases {
            let t = vertex(Protocol::Gemini, "gemini-2.5-pro", location);
            assert_eq!(
                build(&t, GENERATE).url,
                format!(
                    "{host}/v1/projects/sa-project/locations/{segment}/publishers/google/models/gemini-2.5-pro:generateContent"
                ),
                "location {location:?}"
            );
        }
    }

    #[test]
    fn vertex_gemini_operations() {
        let t = vertex(Protocol::Gemini, "gemini-3.8-flash", "us-central1");
        let prefix = "https://us-central1-aiplatform.googleapis.com/v1/projects/sa-project/locations/us-central1/publishers/google/models/gemini-3.8-flash";
        assert_eq!(build(&t, GENERATE).url, format!("{prefix}:generateContent"));
        assert_eq!(
            build(&t, STREAM).url,
            format!("{prefix}:streamGenerateContent?alt=sse")
        );
        assert_eq!(
            build(&t, Operation::CountTokens).url,
            format!("{prefix}:countTokens")
        );
        let list = build_request(&t, &Operation::ListModels, b"", &HeaderMap::new()).unwrap();
        assert_eq!(
            list.url,
            "https://us-central1-aiplatform.googleapis.com/v1beta1/publishers/google/models?pageSize=1000"
        );
        assert_eq!(list.method, Method::GET);
    }

    #[test]
    fn vertex_project_override_and_requirements() {
        let mut t = vertex(Protocol::Gemini, "gemini-2.5-pro", "global");
        t.project = "explicit-project".into();
        assert!(
            build(&t, GENERATE)
                .url
                .contains("/projects/explicit-project/locations/global/")
        );

        // A bearer-less, project-less target cannot be addressed.
        let mut t = target(
            ProviderKind::Vertex,
            Protocol::Gemini,
            "https://aiplatform.googleapis.com",
            "gemini-2.5-pro",
        );
        t.auth = Auth::None;
        assert!(build_request(&t, &GENERATE, b"{}", &HeaderMap::new()).is_err());

        let mut t = vertex(Protocol::Gemini, "gemini-2.5-pro", "bad location!");
        t.project = "p".into();
        assert!(build_request(&t, &GENERATE, b"{}", &HeaderMap::new()).is_err());
    }

    #[test]
    fn vertex_api_key_uses_express_paths_and_goog_header() {
        let t = target(
            ProviderKind::Vertex,
            Protocol::Gemini,
            "https://aiplatform.googleapis.com",
            "gemini-2.5-flash",
        );
        let r = build(&t, STREAM);
        assert_eq!(
            r.url,
            "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        assert_eq!(r.header("x-goog-api-key"), Some(KEY));
        assert!(r.headers.get(AUTHORIZATION).is_none());
        assert!(!r.needs_access_token);

        // With a project the full resource path is used, still with the key.
        let mut t = t;
        t.project = "my-proj".into();
        t.location = "us-central1".into();
        let r = build(&t, GENERATE);
        assert_eq!(
            r.url,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1/publishers/google/models/gemini-2.5-flash:generateContent"
        );
        assert_eq!(r.header("x-goog-api-key"), Some(KEY));
    }

    #[test]
    fn vertex_service_account_defers_the_bearer_token() {
        let t = vertex(Protocol::Gemini, "gemini-2.5-pro", "global");
        let mut r = build(&t, GENERATE);
        assert!(r.needs_access_token);
        assert!(r.headers.get(AUTHORIZATION).is_none());
        assert!(r.headers.get("x-goog-api-key").is_none());
        r.set_bearer("ya29.token").unwrap();
        assert!(!r.needs_access_token);
        assert_eq!(r.header("authorization"), Some("Bearer ya29.token"));
        assert!(r.headers[AUTHORIZATION].is_sensitive());
    }

    #[test]
    fn vertex_claude_uses_raw_predict() {
        let t = vertex(Protocol::Anthropic, "claude-opus-4-5@20251101", "us-east5");
        let prefix = "https://us-east5-aiplatform.googleapis.com/v1/projects/sa-project/locations/us-east5/publishers/anthropic/models";
        let r = build(&t, GENERATE);
        assert_eq!(
            r.url,
            format!("{prefix}/claude-opus-4-5@20251101:rawPredict")
        );
        assert_eq!(
            build(&t, STREAM).url,
            format!("{prefix}/claude-opus-4-5@20251101:streamRawPredict")
        );
        assert_eq!(
            build(&t, Operation::CountTokens).url,
            format!("{prefix}/count-tokens:rawPredict")
        );
        // The version is a body field on Vertex, never a header.
        assert!(r.headers.get("anthropic-version").is_none());
        assert!(r.headers.get("x-api-key").is_none());
        assert!(r.needs_access_token);

        let g = vertex(Protocol::Anthropic, "claude-opus-5", "global");
        assert_eq!(
            build(&g, GENERATE).url,
            "https://aiplatform.googleapis.com/v1/projects/sa-project/locations/global/publishers/anthropic/models/claude-opus-5:rawPredict"
        );
    }

    #[test]
    fn vertex_claude_with_the_gemini_protocol_is_rejected_locally() {
        let t = vertex(Protocol::Gemini, "claude-sonnet-4-6", "global");
        let err = build_request(&t, &GENERATE, b"{}", &HeaderMap::new()).unwrap_err();
        assert!(
            err.info.message.contains("anthropic protocol"),
            "{}",
            err.info.message
        );
        assert_eq!(
            vertex_protocol_for_model("claude-sonnet-4-6"),
            Protocol::Anthropic
        );
        assert_eq!(
            vertex_protocol_for_model("anthropic/claude-opus-5"),
            Protocol::Anthropic
        );
        assert_eq!(
            vertex_protocol_for_model("publishers/anthropic/models/claude-opus-5"),
            Protocol::Anthropic
        );
        assert_eq!(
            vertex_protocol_for_model("gemini-2.5-pro"),
            Protocol::Gemini
        );
        assert_eq!(
            vertex_protocol_for_model("publishers/google/models/gemini-3-pro"),
            Protocol::Gemini
        );
    }

    #[test]
    fn vertex_model_id_forms() {
        let url =
            |model: &str| build(&vertex(Protocol::Gemini, model, "us-central1"), GENERATE).url;
        let host = "https://us-central1-aiplatform.googleapis.com/v1";
        let scope = "projects/sa-project/locations/us-central1";
        assert_eq!(
            url("models/gemini-2.5-pro"),
            format!("{host}/{scope}/publishers/google/models/gemini-2.5-pro:generateContent")
        );
        assert_eq!(
            url("publishers/meta/models/llama-4"),
            format!("{host}/{scope}/publishers/meta/models/llama-4:generateContent")
        );
        assert_eq!(
            url("google/gemini-2.5-flash"),
            format!("{host}/{scope}/publishers/google/models/gemini-2.5-flash:generateContent")
        );
        assert_eq!(
            url("endpoints/1234567890"),
            format!("{host}/{scope}/endpoints/1234567890:generateContent")
        );
        assert_eq!(
            url("projects/other/locations/europe-west1/endpoints/42"),
            format!("{host}/projects/other/locations/europe-west1/endpoints/42:generateContent")
        );
    }

    #[test]
    fn vertex_custom_base_url_is_used_verbatim() {
        let mut t = vertex(Protocol::Gemini, "gemini-2.5-pro", "us-central1");
        t.base_url = "https://vertex-proxy.internal.example/".into();
        assert_eq!(
            build(&t, GENERATE).url,
            "https://vertex-proxy.internal.example/v1/projects/sa-project/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent"
        );
    }

    /// Every provider kind × protocol × operation: the URL that is called,
    /// or `None` when the combination is rejected before any I/O.
    #[test]
    fn kind_protocol_operation_matrix() {
        use Protocol as P;
        use ProviderKind as K;
        const O: &str = "https://api.openai.com/v1";
        const A: &str = "https://api.anthropic.com";
        const G: &str = "https://generativelanguage.googleapis.com";
        const V: &str = "https://aiplatform.googleapis.com";

        // Columns: generate, stream, count tokens, list models, raw
        // `POST embeddings`. Paths are relative to the base URL.
        type Row = [Option<&'static str>; 5];
        let openai_chat: Row = [
            Some("/chat/completions"),
            Some("/chat/completions"),
            None,
            Some("/models"),
            Some("/embeddings"),
        ];
        let openai_responses: Row = [
            Some("/responses"),
            Some("/responses"),
            Some("/responses/input_tokens"),
            Some("/models"),
            Some("/embeddings"),
        ];
        let openai_other: Row = [None, None, None, Some("/models"), Some("/embeddings")];
        let anthropic_native: Row = [
            Some("/v1/messages"),
            Some("/v1/messages"),
            Some("/v1/messages/count_tokens"),
            Some("/v1/models?limit=1000"),
            Some("/v1/embeddings"),
        ];
        let anthropic_other: Row = [
            None,
            None,
            None,
            Some("/v1/models?limit=1000"),
            Some("/v1/embeddings"),
        ];
        let gemini_native: Row = [
            Some("/v1beta/models/m:generateContent"),
            Some("/v1beta/models/m:streamGenerateContent?alt=sse"),
            Some("/v1beta/models/m:countTokens"),
            Some("/v1beta/models?pageSize=1000"),
            Some("/v1beta/embeddings"),
        ];
        let gemini_other: Row = [
            None,
            None,
            None,
            Some("/v1beta/models?pageSize=1000"),
            Some("/v1beta/embeddings"),
        ];
        let vertex_gemini: Row = [
            Some("/v1/publishers/google/models/m:generateContent"),
            Some("/v1/publishers/google/models/m:streamGenerateContent?alt=sse"),
            Some("/v1/publishers/google/models/m:countTokens"),
            Some("/v1beta1/publishers/google/models?pageSize=1000"),
            Some("/v1/embeddings"),
        ];
        let vertex_claude: Row = [
            Some("/v1/publishers/anthropic/models/m:rawPredict"),
            Some("/v1/publishers/anthropic/models/m:streamRawPredict"),
            Some("/v1/publishers/anthropic/models/count-tokens:rawPredict"),
            Some("/v1beta1/publishers/google/models?pageSize=1000"),
            Some("/v1/embeddings"),
        ];
        let vertex_other: Row = [None, None, None, None, Some("/v1/embeddings")];
        let nothing: Row = [None; 5];

        let table: [(K, &str, P, Row); 24] = [
            (K::Openai, O, P::OpenaiChat, openai_chat),
            (K::Openai, O, P::OpenaiResponses, openai_responses),
            (K::Openai, O, P::Anthropic, openai_other),
            (K::Openai, O, P::Gemini, openai_other),
            (K::OpenaiCompat, O, P::OpenaiChat, openai_chat),
            (K::OpenaiCompat, O, P::OpenaiResponses, openai_responses),
            (K::OpenaiCompat, O, P::Anthropic, openai_other),
            (K::OpenaiCompat, O, P::Gemini, openai_other),
            (K::Anthropic, A, P::OpenaiChat, anthropic_other),
            (K::Anthropic, A, P::OpenaiResponses, anthropic_other),
            (K::Anthropic, A, P::Anthropic, anthropic_native),
            (K::Anthropic, A, P::Gemini, anthropic_other),
            (K::Gemini, G, P::OpenaiChat, gemini_other),
            (K::Gemini, G, P::OpenaiResponses, gemini_other),
            (K::Gemini, G, P::Anthropic, gemini_other),
            (K::Gemini, G, P::Gemini, gemini_native),
            (K::Vertex, V, P::OpenaiChat, vertex_other),
            (K::Vertex, V, P::OpenaiResponses, vertex_other),
            (K::Vertex, V, P::Anthropic, vertex_claude),
            (K::Vertex, V, P::Gemini, vertex_gemini),
            (K::Mock, "mock://local", P::OpenaiChat, nothing),
            (K::Mock, "mock://local", P::OpenaiResponses, nothing),
            (K::Mock, "mock://local", P::Anthropic, nothing),
            (K::Mock, "mock://local", P::Gemini, nothing),
        ];
        // Every combination is listed exactly once.
        for kind in K::ALL {
            for protocol in P::ALL {
                let rows = table
                    .iter()
                    .filter(|(k, _, p, _)| *k == kind && *p == protocol)
                    .count();
                assert_eq!(rows, 1, "{kind} {protocol}");
            }
        }

        let operations = [
            (GENERATE, Method::POST),
            (STREAM, Method::POST),
            (Operation::CountTokens, Method::POST),
            (Operation::ListModels, Method::GET),
            (Operation::raw_post("embeddings"), Method::POST),
        ];
        for (kind, base, protocol, row) in table {
            let t = target(kind, protocol, base, "m");
            for ((op, method), expected) in operations.iter().zip(row) {
                let built = build_request(&t, op, b"{}", &HeaderMap::new());
                let label = format!("{kind} / {protocol} / {op:?}");
                match (built, expected) {
                    (Ok(request), Some(path)) => {
                        assert_eq!(request.url, format!("{base}{path}"), "{label}");
                        assert_eq!(&request.method, method, "{label}");
                    }
                    (Err(error), None) => {
                        assert_eq!(error.status, 0, "{label}");
                        assert_eq!(error.class, FailureClass::Transport, "{label}");
                    }
                    (Ok(request), None) => {
                        panic!("{label}: expected a rejection, got {}", request.url)
                    }
                    (Err(error), Some(path)) => {
                        panic!(
                            "{label}: expected {path}, got error `{}`",
                            error.info.message
                        )
                    }
                }
            }
        }
    }

    #[test]
    fn unsupported_combinations_fail_before_any_io() {
        let combos = [
            (ProviderKind::Openai, Protocol::Anthropic),
            (ProviderKind::Openai, Protocol::Gemini),
            (ProviderKind::OpenaiCompat, Protocol::Gemini),
            (ProviderKind::Anthropic, Protocol::OpenaiChat),
            (ProviderKind::Anthropic, Protocol::Gemini),
            (ProviderKind::Gemini, Protocol::OpenaiChat),
            (ProviderKind::Gemini, Protocol::Anthropic),
            (ProviderKind::Vertex, Protocol::OpenaiResponses),
        ];
        for (kind, protocol) in combos {
            let t = target(kind, protocol, "https://example.test", "m");
            let err = build_request(&t, &GENERATE, b"{}", &HeaderMap::new()).unwrap_err();
            assert_eq!(err.status, 0, "{kind} {protocol}");
            assert_eq!(err.class, FailureClass::Transport);
        }
        for protocol in Protocol::ALL {
            let t = target(ProviderKind::Mock, protocol, "mock://local", "mock-echo");
            for op in [
                GENERATE,
                STREAM,
                Operation::CountTokens,
                Operation::ListModels,
            ] {
                assert!(build_request(&t, &op, b"{}", &HeaderMap::new()).is_err());
            }
        }
    }

    #[test]
    fn invalid_base_urls_are_reported_without_echoing_them() {
        for base in ["", "not a url", "ftp://x/y", "mock://local"] {
            let t = target(ProviderKind::OpenaiCompat, Protocol::OpenaiChat, base, "m");
            let err = build_request(&t, &GENERATE, b"{}", &HeaderMap::new()).unwrap_err();
            assert!(
                err.info.message.contains("base URL"),
                "{}",
                err.info.message
            );
        }
    }

    // ---------------------------------------------------------- headers

    #[test]
    fn default_headers_per_kind() {
        let o = build(
            &target(
                ProviderKind::Openai,
                Protocol::OpenaiChat,
                "https://api.openai.com/v1",
                "m",
            ),
            GENERATE,
        );
        assert_eq!(o.header("authorization"), Some(&*format!("Bearer {KEY}")));
        assert_eq!(o.header("content-type"), Some("application/json"));
        assert_eq!(o.header("accept"), Some("application/json"));
        assert_eq!(o.header("user-agent"), Some(USER_AGENT));
        assert!(USER_AGENT.starts_with("switchyard/"));
        assert!(o.headers.get("accept-encoding").is_none());
        assert!(o.headers[AUTHORIZATION].is_sensitive());

        let a = build(
            &target(
                ProviderKind::Anthropic,
                Protocol::Anthropic,
                "https://api.anthropic.com",
                "m",
            ),
            GENERATE,
        );
        assert_eq!(a.header("x-api-key"), Some(KEY));
        assert_eq!(a.header("anthropic-version"), Some("2023-06-01"));
        assert!(a.headers.get(AUTHORIZATION).is_none());
        assert!(a.headers.get("anthropic-beta").is_none());

        let g = build(
            &target(
                ProviderKind::Gemini,
                Protocol::Gemini,
                "https://generativelanguage.googleapis.com",
                "m",
            ),
            GENERATE,
        );
        assert_eq!(g.header("x-goog-api-key"), Some(KEY));
        assert!(g.headers.get(AUTHORIZATION).is_none());
        // The key never travels in the URL.
        assert!(!g.url.contains(KEY) && !g.url.contains("key="));
    }

    #[test]
    fn streams_ask_for_event_streams() {
        for (kind, protocol, base) in [
            (
                ProviderKind::Openai,
                Protocol::OpenaiResponses,
                "https://api.openai.com/v1",
            ),
            (
                ProviderKind::Anthropic,
                Protocol::Anthropic,
                "https://api.anthropic.com",
            ),
            (
                ProviderKind::Gemini,
                Protocol::Gemini,
                "https://generativelanguage.googleapis.com",
            ),
        ] {
            let r = build(&target(kind, protocol, base, "m"), STREAM);
            assert_eq!(r.header("accept"), Some("text/event-stream"), "{kind}");
            assert_eq!(r.header("accept-encoding"), Some("identity"));
            assert_eq!(r.header("content-type"), Some("application/json"));
        }
    }

    #[test]
    fn keyless_openai_compat_sends_no_authorization() {
        let mut t = target(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            "http://localhost:11434/v1",
            "llama",
        );
        t.auth = Auth::None;
        assert!(build(&t, GENERATE).headers.get(AUTHORIZATION).is_none());
        t.auth = Auth::ApiKey("   ".into());
        assert!(build(&t, GENERATE).headers.get(AUTHORIZATION).is_none());
        // Without a gateway-managed credential the operator may supply one.
        t.headers = vec![("Authorization".into(), "Basic dXNlcjpwYXNz".into())];
        let r = build(&t, GENERATE);
        assert_eq!(r.header("authorization"), Some("Basic dXNlcjpwYXNz"));
        assert!(r.headers[AUTHORIZATION].is_sensitive());
    }

    #[test]
    fn anthropic_compatible_hosts_get_both_credential_headers() {
        let t = target(
            ProviderKind::Anthropic,
            Protocol::Anthropic,
            "https://gateway.example/anthropic",
            "m",
        );
        let r = build(&t, GENERATE);
        assert_eq!(r.header("x-api-key"), Some(KEY));
        assert_eq!(r.header("authorization"), Some(&*format!("Bearer {KEY}")));
    }

    fn client_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn client_credentials_and_bookkeeping_headers_are_never_forwarded() {
        let client = client_headers(&[
            ("authorization", "Bearer client-gateway-key"),
            ("x-api-key", "client-gateway-key"),
            ("x-goog-api-key", "client-gateway-key"),
            ("cookie", "session=abc"),
            ("host", "gateway.local"),
            ("content-length", "12345"),
            ("x-request-id", "req-from-client"),
            ("x-forwarded-for", "10.1.2.3"),
            ("user-agent", "curl/8"),
            ("accept", "text/html"),
            ("content-type", "text/plain"),
            ("accept-encoding", "br"),
            ("x-stainless-lang", "python"),
        ]);
        let targets = [
            target(
                ProviderKind::Openai,
                Protocol::OpenaiChat,
                "https://api.openai.com/v1",
                "m",
            ),
            target(
                ProviderKind::Anthropic,
                Protocol::Anthropic,
                "https://api.anthropic.com",
                "m",
            ),
            target(
                ProviderKind::Gemini,
                Protocol::Gemini,
                "https://generativelanguage.googleapis.com",
                "m",
            ),
        ];
        for t in targets {
            for op in [
                GENERATE,
                STREAM,
                Operation::CountTokens,
                Operation::ListModels,
            ] {
                if t.protocol == Protocol::OpenaiChat && op == Operation::CountTokens {
                    continue;
                }
                let r = build_request(&t, &op, b"{}", &client).unwrap();
                for (_, v) in &r.headers {
                    let v = v.to_str().unwrap();
                    assert!(
                        !v.contains("client-gateway-key"),
                        "{:?} leaked the client key",
                        t.kind
                    );
                }
                for absent in [
                    "cookie",
                    "host",
                    "content-length",
                    "x-request-id",
                    "x-forwarded-for",
                    "x-stainless-lang",
                ] {
                    assert!(
                        r.headers.get(absent).is_none(),
                        "{absent} forwarded to {:?}",
                        t.kind
                    );
                }
                assert_eq!(r.header("user-agent"), Some(USER_AGENT));
                assert_eq!(r.header("content-type"), Some("application/json"));
                assert_ne!(r.header("accept"), Some("text/html"));
            }
        }
    }

    #[test]
    fn anthropic_beta_and_version_are_forwarded_to_anthropic_only() {
        let client = client_headers(&[
            (
                "anthropic-beta",
                "interleaved-thinking-2025-05-14, context-1m-2025-08-07",
            ),
            (
                "anthropic-beta",
                "context-1m-2025-08-07,files-api-2025-04-14",
            ),
            ("anthropic-version", "2024-10-22"),
            ("openai-beta", "responses_websockets=2026-02-06"),
        ]);
        let a = target(
            ProviderKind::Anthropic,
            Protocol::Anthropic,
            "https://api.anthropic.com",
            "m",
        );
        let r = build_request(&a, &GENERATE, b"{}", &client).unwrap();
        assert_eq!(
            r.header("anthropic-beta"),
            Some("interleaved-thinking-2025-05-14,context-1m-2025-08-07,files-api-2025-04-14")
        );
        assert_eq!(r.header("anthropic-version"), Some("2024-10-22"));
        assert!(r.headers.get("openai-beta").is_none());

        let o = target(
            ProviderKind::Openai,
            Protocol::OpenaiResponses,
            "https://api.openai.com/v1",
            "m",
        );
        let r = build_request(&o, &GENERATE, b"{}", &client).unwrap();
        assert!(r.headers.get("anthropic-beta").is_none());
        assert!(r.headers.get("anthropic-version").is_none());
        assert_eq!(
            r.header("openai-beta"),
            Some("responses_websockets=2026-02-06")
        );

        let g = target(
            ProviderKind::Gemini,
            Protocol::Gemini,
            "https://generativelanguage.googleapis.com",
            "m",
        );
        let r = build_request(&g, &GENERATE, b"{}", &client).unwrap();
        assert!(r.headers.get("anthropic-beta").is_none());
        assert!(r.headers.get("openai-beta").is_none());

        // Claude on Vertex: betas yes, version header no.
        let v = vertex(Protocol::Anthropic, "claude-opus-5", "global");
        let r = build_request(&v, &GENERATE, b"{}", &client).unwrap();
        assert!(
            r.header("anthropic-beta")
                .unwrap()
                .contains("files-api-2025-04-14")
        );
        assert!(r.headers.get("anthropic-version").is_none());
    }

    #[test]
    fn openai_organization_and_project_yield_to_configuration() {
        let client = client_headers(&[
            ("openai-organization", "org-client"),
            ("openai-project", "proj-client"),
        ]);
        let mut t = target(
            ProviderKind::Openai,
            Protocol::OpenaiChat,
            "https://api.openai.com/v1",
            "m",
        );
        let r = build_request(&t, &GENERATE, b"{}", &client).unwrap();
        assert_eq!(r.header("openai-organization"), Some("org-client"));
        assert_eq!(r.header("openai-project"), Some("proj-client"));

        t.headers = vec![("OpenAI-Organization".into(), "org-config".into())];
        let r = build_request(&t, &GENERATE, b"{}", &client).unwrap();
        assert_eq!(r.header("openai-organization"), Some("org-config"));
        assert_eq!(r.header("openai-project"), Some("proj-client"));
    }

    #[test]
    fn configured_headers_override_defaults_but_not_the_credential() {
        let mut t = target(
            ProviderKind::Anthropic,
            Protocol::Anthropic,
            "https://api.anthropic.com",
            "m",
        );
        t.headers = vec![
            ("User-Agent".into(), "my-proxy/1.0".into()),
            ("anthropic-version".into(), "2099-01-01".into()),
            ("x-api-key".into(), "sk-from-config".into()),
            ("X-Custom".into(), " value ".into()),
            ("anthropic-beta".into(), "config-beta-1".into()),
            ("Host".into(), "evil.example".into()),
            ("Content-Length".into(), "1".into()),
            ("bad header name".into(), "x".into()),
            ("x-bad-value".into(), "line\nbreak".into()),
            ("".into(), "x".into()),
            ("x-empty".into(), "".into()),
        ];
        let client = client_headers(&[
            ("anthropic-version", "2024-10-22"),
            ("anthropic-beta", "client-beta-1, config-beta-1"),
        ]);
        let r = build_request(&t, &STREAM, b"{}", &client).unwrap();
        assert_eq!(r.header("user-agent"), Some("my-proxy/1.0"));
        assert_eq!(r.header("anthropic-version"), Some("2099-01-01"));
        assert_eq!(r.header("x-api-key"), Some(KEY));
        assert_eq!(r.header("x-custom"), Some("value"));
        assert_eq!(
            r.header("anthropic-beta"),
            Some("config-beta-1,client-beta-1")
        );
        assert_eq!(r.header("accept"), Some("text/event-stream"));
        for absent in ["host", "content-length", "x-bad-value", "x-empty"] {
            assert!(r.headers.get(absent).is_none(), "{absent}");
        }

        // The same rule for bearer credentials.
        let mut o = target(
            ProviderKind::Openai,
            Protocol::OpenaiChat,
            "https://api.openai.com/v1",
            "m",
        );
        o.headers = vec![
            ("authorization".into(), "Bearer sk-from-config".into()),
            ("accept".into(), "application/x-ndjson".into()),
        ];
        let r = build(&o, GENERATE);
        assert_eq!(r.header("authorization"), Some(&*format!("Bearer {KEY}")));
        assert_eq!(r.header("accept"), Some("application/x-ndjson"));
    }

    #[test]
    fn raw_requests_keep_the_clients_media_types() {
        let t = target(
            ProviderKind::Openai,
            Protocol::OpenaiChat,
            "https://api.openai.com/v1",
            "m",
        );
        let client = client_headers(&[
            ("content-type", "multipart/form-data; boundary=xyz"),
            ("accept", "audio/mpeg"),
        ]);
        let r = build_request(&t, &Operation::raw_post("audio/speech"), b"x", &client).unwrap();
        assert_eq!(
            r.header("content-type"),
            Some("multipart/form-data; boundary=xyz")
        );
        assert_eq!(r.header("accept"), Some("audio/mpeg"));
        let r = build_request(
            &t,
            &Operation::raw_post("embeddings"),
            b"{}",
            &HeaderMap::new(),
        )
        .unwrap();
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert!(r.headers.get("accept").is_none());
    }

    #[test]
    fn credentials_with_control_characters_are_rejected() {
        let mut t = target(
            ProviderKind::Openai,
            Protocol::OpenaiChat,
            "https://api.openai.com/v1",
            "m",
        );
        t.auth = Auth::ApiKey("sk-abc\r\nx-injected: 1".into());
        let err = build_request(&t, &GENERATE, b"{}", &HeaderMap::new()).unwrap_err();
        assert_eq!(err.class, FailureClass::Auth);
        assert!(!err.info.message.contains("sk-abc"));
    }

    #[test]
    fn service_accounts_are_vertex_only() {
        let mut t = target(
            ProviderKind::Gemini,
            Protocol::Gemini,
            "https://generativelanguage.googleapis.com",
            "m",
        );
        t.auth = Auth::ServiceAccount(service_account());
        let err = build_request(&t, &GENERATE, b"{}", &HeaderMap::new()).unwrap_err();
        assert_eq!(err.class, FailureClass::Auth);
    }

    #[test]
    fn built_request_debug_masks_secrets() {
        let mut t = target(
            ProviderKind::Openai,
            Protocol::OpenaiChat,
            "https://api.openai.com/v1?key=query-secret-123456",
            "m",
        );
        t.headers = vec![("x-upstream-token".into(), "tok-abcdefghijklmnop".into())];
        let text = format!("{:?}", build(&t, GENERATE));
        assert!(!text.contains(KEY), "{text}");
        assert!(!text.contains("tok-abcdefghijklmnop"), "{text}");
        assert!(!text.contains("query-secret-123456"), "{text}");
        assert!(text.contains("api.openai.com/v1/chat/completions"));
    }

    #[test]
    fn url_redaction() {
        assert_eq!(
            redact_url("https://user:pw@host.test:8443/a/b?key=secret&x=1"),
            "https://host.test:8443/a/b?<redacted>"
        );
        assert_eq!(redact_url("https://host.test/a"), "https://host.test/a");
        assert_eq!(redact_url("nonsense?key=1"), "nonsense?<redacted>");
    }

    // -------------------------------------------------------- websocket

    #[test]
    fn websocket_urls_swap_the_scheme() {
        let t = target(
            ProviderKind::Openai,
            Protocol::OpenaiResponses,
            "https://api.openai.com/v1",
            "gpt-5",
        );
        let r = build_ws_request(&t, "responses", &HeaderMap::new()).unwrap();
        assert_eq!(r.url, "wss://api.openai.com/v1/responses");
        assert_eq!(r.method, Method::GET);
        assert_eq!(r.header("authorization"), Some(&*format!("Bearer {KEY}")));
        assert_eq!(r.header("user-agent"), Some(USER_AGENT));
        assert!(r.headers.get("content-type").is_none());

        let r =
            build_ws_request(&t, "/v1/realtime?model=gpt-realtime-2.1", &HeaderMap::new()).unwrap();
        assert_eq!(
            r.url,
            "wss://api.openai.com/v1/realtime?model=gpt-realtime-2.1"
        );

        let local = target(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiResponses,
            "http://127.0.0.1:9000/prefix/v1",
            "m",
        );
        let r = build_ws_request(&local, "responses", &HeaderMap::new()).unwrap();
        assert_eq!(r.url, "ws://127.0.0.1:9000/prefix/v1/responses");
    }

    #[test]
    fn websocket_extra_headers_are_vetted() {
        let mut t = target(
            ProviderKind::Openai,
            Protocol::OpenaiResponses,
            "https://api.openai.com/v1",
            "gpt-5",
        );
        t.headers = vec![("OpenAI-Organization".into(), "org-config".into())];
        let extra = client_headers(&[
            ("openai-beta", "realtime=v1"),
            ("authorization", "Bearer client-key"),
            ("cookie", "a=b"),
            ("sec-websocket-key", "abc"),
            ("sec-websocket-extensions", "permessage-deflate"),
            (
                "sec-websocket-protocol",
                "realtime, openai-insecure-api-key.sk-client, openai-organization.org-1",
            ),
            ("openai-safety-identifier", "user-hash"),
        ]);
        let r = build_ws_request(&t, "realtime?model=m", &extra).unwrap();
        assert_eq!(r.header("openai-beta"), Some("realtime=v1"));
        assert_eq!(r.header("authorization"), Some(&*format!("Bearer {KEY}")));
        assert_eq!(r.header("openai-organization"), Some("org-config"));
        assert_eq!(r.header("openai-safety-identifier"), Some("user-hash"));
        assert_eq!(
            r.header("sec-websocket-protocol"),
            Some("realtime, openai-organization.org-1")
        );
        for absent in ["cookie", "sec-websocket-key", "sec-websocket-extensions"] {
            assert!(r.headers.get(absent).is_none(), "{absent}");
        }
    }

    fn lines(r: &BuiltRequest, name: &str) -> Vec<String> {
        r.headers
            .get_all(name)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn websocket_subprotocol_lines_are_joined_into_one_header() {
        let t = target(
            ProviderKind::Openai,
            Protocol::OpenaiResponses,
            "https://api.openai.com/v1",
            "gpt-5",
        );
        // RFC 6455 lets a client spread its offer over several lines.
        let extra = client_headers(&[
            ("sec-websocket-protocol", "alpha"),
            (
                "sec-websocket-protocol",
                "openai-insecure-api-key.sk-client",
            ),
            ("sec-websocket-protocol", "beta, alpha"),
        ]);
        let r = build_ws_request(&t, "realtime?model=m", &extra).unwrap();
        assert_eq!(lines(&r, "sec-websocket-protocol"), vec!["alpha, beta"]);

        // An offer that only carried the client's key leaves no header.
        let extra = client_headers(&[
            (
                "sec-websocket-protocol",
                "openai-insecure-api-key.sk-client",
            ),
            ("sec-websocket-protocol", " , "),
        ]);
        let r = build_ws_request(&t, "realtime?model=m", &extra).unwrap();
        assert!(r.headers.get("sec-websocket-protocol").is_none());
    }

    #[test]
    fn websocket_handshake_has_exactly_one_user_agent() {
        let mut t = target(
            ProviderKind::Openai,
            Protocol::OpenaiResponses,
            "https://api.openai.com/v1",
            "gpt-5",
        );
        // The caller's value replaces the default …
        let extra = client_headers(&[
            ("user-agent", "relayed-client/1.0"),
            ("user-agent", "second-line/2.0"),
        ]);
        let r = build_ws_request(&t, "responses", &extra).unwrap();
        assert_eq!(lines(&r, "user-agent"), vec!["relayed-client/1.0"]);
        // … and the provider configuration has the last word.
        t.headers = vec![("User-Agent".into(), "configured/3.0".into())];
        let r = build_ws_request(&t, "responses", &extra).unwrap();
        assert_eq!(lines(&r, "user-agent"), vec!["configured/3.0"]);
        // Other repeated headers the caller passes stay repeated.
        let extra = client_headers(&[("x-trace", "a"), ("x-trace", "b")]);
        let r = build_ws_request(&t, "responses", &extra).unwrap();
        assert_eq!(lines(&r, "x-trace"), vec!["a", "b"]);
    }

    #[test]
    fn configured_headers_are_confidential_unless_known_harmless() {
        let mut t = target(
            ProviderKind::OpenaiCompat,
            Protocol::OpenaiChat,
            "https://gateway.example/v1",
            "m",
        );
        t.headers = vec![
            (
                "Helicone-Auth".into(),
                "Bearer sk-helicone-0123456789".into(),
            ),
            ("X-Title".into(), "my-app-label".into()),
            ("User-Agent".into(), "custom/1.0".into()),
        ];
        for r in [
            build(&t, GENERATE),
            build_ws_request(&t, "responses", &HeaderMap::new()).unwrap(),
        ] {
            // Sent as configured …
            assert_eq!(
                r.header("helicone-auth"),
                Some("Bearer sk-helicone-0123456789")
            );
            // … marked sensitive for the HTTP stack's own logging …
            assert!(r.headers["helicone-auth"].is_sensitive());
            assert!(r.headers["x-title"].is_sensitive());
            assert!(!r.headers["user-agent"].is_sensitive());
            // … and absent from our Debug output.
            let text = format!("{r:?}");
            assert!(!text.contains("sk-helicone-0123456789"), "{text}");
            assert!(!text.contains("my-app-label"), "{text}");
            assert!(text.contains("helicone-auth") && text.contains("custom/1.0"));
        }
    }

    // ------------------------------------------------------------ bodies

    #[test]
    fn vertex_anthropic_body_for_generation() {
        let mut body = json!({
            "model": "claude-opus-5",
            "max_tokens": 1024,
            "stream": true,
            "messages": [{"role": "user", "content": "Hello"}]
        });
        adapt_vertex_anthropic_body(&mut body, &STREAM);
        assert_eq!(
            body,
            json!({
                "anthropic_version": "vertex-2023-10-16",
                "max_tokens": 1024,
                "stream": true,
                "messages": [{"role": "user", "content": "Hello"}]
            })
        );
        // Key order: the version leads, the rest keeps its order.
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["anthropic_version", "max_tokens", "stream", "messages"]
        );
    }

    #[test]
    fn vertex_anthropic_body_is_idempotent_and_overrides_stale_versions() {
        let mut body = json!({"anthropic_version": "2023-06-01", "model": "m", "messages": []});
        adapt_vertex_anthropic_body(&mut body, &GENERATE);
        let once = body.clone();
        adapt_vertex_anthropic_body(&mut body, &GENERATE);
        assert_eq!(body, once);
        assert_eq!(
            body,
            json!({"anthropic_version": "vertex-2023-10-16", "messages": []})
        );
    }

    #[test]
    fn vertex_anthropic_count_tokens_keeps_the_model() {
        let mut body = json!({"model": "claude-opus-5", "messages": []});
        adapt_vertex_anthropic_body(&mut body, &Operation::CountTokens);
        assert_eq!(
            body,
            json!({"anthropic_version": "vertex-2023-10-16", "model": "claude-opus-5", "messages": []})
        );
        let mut not_an_object = json!([1, 2]);
        adapt_vertex_anthropic_body(&mut not_an_object, &GENERATE);
        assert_eq!(not_an_object, json!([1, 2]));
    }

    #[test]
    fn model_listing_page_urls() {
        let a = target(
            ProviderKind::Anthropic,
            Protocol::Anthropic,
            "https://api.anthropic.com",
            "",
        );
        assert_eq!(
            list_models_page_url(&a, Some(("after_id", "claude-opus-4-5"))).unwrap(),
            "https://api.anthropic.com/v1/models?limit=1000&after_id=claude-opus-4-5"
        );
        let g = target(
            ProviderKind::Gemini,
            Protocol::Gemini,
            "https://generativelanguage.googleapis.com",
            "",
        );
        assert_eq!(
            list_models_page_url(&g, Some(("pageToken", "a b&c=d/e+f"))).unwrap(),
            "https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000&pageToken=a%20b%26c%3Dd%2Fe%2Bf"
        );
        let o = target(
            ProviderKind::Openai,
            Protocol::OpenaiChat,
            "https://api.openai.com/v1",
            "",
        );
        assert_eq!(
            list_models_page_url(&o, None).unwrap(),
            "https://api.openai.com/v1/models"
        );
    }
}
