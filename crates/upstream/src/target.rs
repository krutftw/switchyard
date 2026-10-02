//! What an upstream call is addressed to, and what kind of call it is.

use crate::proxy::{redact_proxy_url, resolve_proxy};
use crate::secrets::{Scrubber, display_header_value, mask};
use crate::vertex::ServiceAccount;
use http::Method;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use switchyard_core::Protocol;
use switchyard_core::config::{
    CredentialConfig, ProviderConfig, ProviderKind, ProxySetting, UpstreamConfig,
};

/// Everything needed to reach one credential of one provider.
#[derive(Clone)]
pub struct Target {
    /// Provider name from the configuration (for logs and errors).
    pub provider: String,
    pub kind: ProviderKind,
    /// Endpoint root as configured (see
    /// [`ProviderConfig::effective_base_url`]). A trailing slash is tolerated.
    pub base_url: String,
    /// Protocol the request body is written in.
    pub protocol: Protocol,
    /// Upstream model id.
    pub model: String,
    pub auth: Auth,
    /// Extra headers from the provider configuration, applied after the
    /// built-in ones. Their values are treated as confidential: `Debug`
    /// shows them only for a few harmless names (`user-agent`, …).
    pub headers: Vec<(String, String)>,
    /// Effective proxy (see [`resolve_proxy`]).
    pub proxy: ProxySetting,
    /// Vertex only: Google Cloud project. Empty = the service account's.
    pub project: String,
    /// Vertex only: region, `global`, or a multi-region (`us`, `eu`). Empty =
    /// `global`.
    pub location: String,
}

impl Target {
    /// Assembles a target from configuration: base URL, extra headers,
    /// project/location and the proxy precedence (credential > provider >
    /// `upstream.proxy`). The secret must already be resolved into `auth`.
    pub fn for_provider(
        provider: &ProviderConfig,
        credential: &CredentialConfig,
        global_proxy: &str,
        protocol: Protocol,
        model: impl Into<String>,
        auth: Auth,
    ) -> Target {
        Target {
            provider: provider.name.clone(),
            kind: provider.kind,
            base_url: provider.effective_base_url(),
            protocol,
            model: model.into(),
            auth,
            headers: provider
                .headers
                .iter()
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                .collect(),
            proxy: resolve_proxy(&credential.proxy, &provider.proxy, global_proxy),
            project: provider.project.trim().to_string(),
            location: provider.location.trim().to_string(),
        }
    }

    /// `text` with this target's credentials — its API key and the values
    /// of configured headers whose names announce a credential — replaced by
    /// `[redacted]`.
    ///
    /// [`crate::UpstreamClient::send`] already does this for the errors it
    /// returns. Use it for anything else an upstream said that is logged or
    /// shown to a client (an in-stream error event, a WebSocket close
    /// reason): careless servers quote the key they rejected.
    pub fn redact(&self, text: &str) -> String {
        Scrubber::for_target(self).text(text).into_owned()
    }
}

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, String)> = self
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), display_header_value(k, v)))
            .collect();
        let proxy = match &self.proxy {
            ProxySetting::Inherit => "inherit".to_string(),
            ProxySetting::Direct => "direct".to_string(),
            ProxySetting::Url(u) => redact_proxy_url(u),
        };
        f.debug_struct("Target")
            .field("provider", &self.provider)
            .field("kind", &self.kind)
            .field("base_url", &crate::request::redact_url(&self.base_url))
            .field("protocol", &self.protocol)
            .field("model", &self.model)
            .field("auth", &self.auth)
            .field("headers", &headers)
            .field("proxy", &proxy)
            .field("project", &self.project)
            .field("location", &self.location)
            .finish()
    }
}

/// How a call authenticates.
#[derive(Clone)]
pub enum Auth {
    /// No credential (local OpenAI-compatible servers, the mock provider).
    None,
    /// A static API key.
    ApiKey(String),
    /// A Google service account; an OAuth access token is minted on demand.
    ServiceAccount(Arc<ServiceAccount>),
}

impl Auth {
    /// The API key, when this is a non-empty [`Auth::ApiKey`].
    pub fn api_key(&self) -> Option<&str> {
        match self {
            Auth::ApiKey(k) if !k.trim().is_empty() => Some(k.trim()),
            _ => None,
        }
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::None => f.write_str("None"),
            Auth::ApiKey(k) => write!(f, "ApiKey({})", mask(k)),
            Auth::ServiceAccount(sa) => write!(f, "ServiceAccount({})", sa.client_email),
        }
    }
}

/// What an upstream call does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    /// The generation call of the target's protocol.
    Generate { stream: bool },
    /// The protocol's token-counting endpoint.
    CountTokens,
    /// The provider's model listing.
    ListModels,
    /// Any other endpoint, addressed relative to the provider's versioned API
    /// root (`embeddings`, `images/generations`, `audio/speech`, …).
    Raw {
        method: Method,
        /// Relative path, e.g. `embeddings`.
        path: String,
        /// Query string without the leading `?`.
        query: Option<String>,
    },
}

impl Operation {
    /// Whether the response body is consumed as a stream.
    pub fn is_stream(&self) -> bool {
        matches!(self, Operation::Generate { stream: true })
    }

    /// A raw `POST` to `path`.
    pub fn raw_post(path: impl Into<String>) -> Operation {
        Operation::Raw {
            method: Method::POST,
            path: path.into(),
            query: None,
        }
    }
}

/// Time limits of one upstream call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    /// Limit for establishing the connection (TCP, proxy, TLS).
    pub connect: Duration,
    /// Total limit for a non-streaming call, from sending the request to the
    /// last byte of the response. Zero disables it. Streaming calls ignore
    /// it: their idle timeout is enforced by the consumer of the stream.
    pub request: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            connect: Duration::from_secs(30),
            request: Duration::from_secs(600),
        }
    }
}

impl Timeouts {
    /// The limits configured in `[upstream]`.
    pub fn from_config(config: &UpstreamConfig) -> Self {
        Timeouts {
            connect: Duration::from_secs(config.connect_timeout_secs.max(1)),
            request: Duration::from_secs(config.request_timeout_secs),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            provider: "openai".into(),
            kind: ProviderKind::Openai,
            base_url: "https://user:hunter2@api.openai.com/v1?key=sk-in-query-1234567890".into(),
            protocol: Protocol::OpenaiChat,
            model: "gpt-5".into(),
            auth: Auth::ApiKey("sk-proj-abcdefghijklmnopqrstuvwxyz".into()),
            headers: vec![
                ("x-team".into(), "blue-team-label".into()),
                ("User-Agent".into(), "custom-agent/1.0".into()),
                ("x-secret-token".into(), "tok-abcdefghijklmnop".into()),
                (
                    "Helicone-Auth".into(),
                    "Bearer sk-helicone-0123456789abcdef".into(),
                ),
                (
                    "Ocp-Apim-Subscription-Key".into(),
                    "0123456789abcdef0123456789abcdef".into(),
                ),
                ("X-Auth-Key".into(), "short-key-1".into()),
                (
                    "Authorization".into(),
                    "Bearer other-key-abcdefghijkl".into(),
                ),
            ],
            proxy: ProxySetting::Url("http://puser:ppass@proxy.local:3128".into()),
            project: String::new(),
            location: String::new(),
        }
    }

    #[test]
    fn debug_never_prints_secrets() {
        let text = format!("{:?}", target());
        for secret in [
            "sk-proj-abcdefghijklmnopqrstuvwxyz",
            "tok-abcdefghijklmnop",
            "other-key-abcdefghijkl",
            "sk-helicone-0123456789abcdef",
            "0123456789abcdef0123456789abcdef",
            "short-key-1",
            // A header of unknown purpose is hidden too: nobody can list
            // every name a gateway credential travels under.
            "blue-team-label",
            "hunter2",
            "ppass",
            "sk-in-query-1234567890",
        ] {
            assert!(!text.contains(secret), "{secret} leaked in {text}");
        }
        // Header names stay visible, and so do values of harmless headers.
        assert!(text.contains("x-team") && text.contains("Helicone-Auth"));
        assert!(text.contains("custom-agent/1.0"));
        assert!(text.contains("proxy.local:3128"));
        assert!(text.contains("Bearer "));
        assert!(text.contains("api.openai.com"));
    }

    #[test]
    fn redact_removes_the_targets_credentials_from_text() {
        let t = target();
        assert_eq!(
            t.redact("Incorrect API key provided: sk-proj-abcdefghijklmnopqrstuvwxyz."),
            "Incorrect API key provided: [redacted]."
        );
        assert_eq!(
            t.redact("bad Helicone-Auth sk-helicone-0123456789abcdef"),
            "bad Helicone-Auth [redacted]"
        );
        assert_eq!(t.redact("nothing secret"), "nothing secret");
    }

    #[test]
    fn auth_debug_and_accessors() {
        assert_eq!(format!("{:?}", Auth::None), "None");
        let dbg = format!("{:?}", Auth::ApiKey("sk-abcdefghijklmnop".into()));
        assert!(!dbg.contains("abcdefghijklmnop"));
        assert_eq!(Auth::ApiKey("  k  ".into()).api_key(), Some("k"));
        assert_eq!(Auth::ApiKey("   ".into()).api_key(), None);
        assert_eq!(Auth::None.api_key(), None);
    }

    #[test]
    fn operations() {
        assert!(Operation::Generate { stream: true }.is_stream());
        assert!(!Operation::Generate { stream: false }.is_stream());
        assert!(!Operation::CountTokens.is_stream());
        assert_eq!(
            Operation::raw_post("embeddings"),
            Operation::Raw {
                method: Method::POST,
                path: "embeddings".into(),
                query: None
            }
        );
    }

    #[test]
    fn timeouts_from_config() {
        let t = Timeouts::from_config(&UpstreamConfig::default());
        assert_eq!(t.connect, Duration::from_secs(30));
        assert_eq!(t.request, Duration::from_secs(600));
        assert_eq!(Timeouts::default(), t);
    }

    #[test]
    fn target_from_provider_config() {
        let mut p = ProviderConfig::new("local", ProviderKind::OpenaiCompat);
        p.base_url = "http://localhost:11434/v1/".into();
        p.proxy = "http://provider-proxy:3128".into();
        p.headers.insert(" x-a ".into(), " 1 ".into());
        p.headers.insert("x-empty".into(), "  ".into());
        let mut c = CredentialConfig::default();
        let t = Target::for_provider(
            &p,
            &c,
            "http://global:1",
            Protocol::OpenaiChat,
            "llama",
            Auth::None,
        );
        assert_eq!(t.base_url, "http://localhost:11434/v1");
        assert_eq!(t.headers, vec![("x-a".to_string(), "1".to_string())]);
        assert_eq!(
            t.proxy,
            ProxySetting::Url("http://provider-proxy:3128".into())
        );
        c.proxy = "direct".into();
        let t = Target::for_provider(
            &p,
            &c,
            "http://global:1",
            Protocol::OpenaiChat,
            "llama",
            Auth::None,
        );
        assert_eq!(t.proxy, ProxySetting::Direct);
    }
}
