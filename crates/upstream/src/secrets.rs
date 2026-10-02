//! Keeping credentials out of logs, `Debug` output and errors.
//!
//! Two separate concerns live here:
//!
//! * **Displaying headers.** A provider's configured `headers` are where
//!   operators put gateway credentials, under names nobody can enumerate
//!   (`Helicone-Auth`, `Ocp-Apim-Subscription-Key`, …). So header values are
//!   shown only for a short allow-list of names that never carry a
//!   credential; everything else is masked or hidden.
//! * **Scrubbing upstream answers.** A careless upstream quotes the key it
//!   rejected ("Invalid API key: sk-…"). The error handed to the gateway is
//!   shown to clients and written to request logs, so every credential the
//!   call presented is removed from it first ([`Scrubber`]).

use crate::target::{Auth, Target};
use percent_encoding::percent_decode_str;
use std::borrow::Cow;
use switchyard_core::UpstreamError;
use switchyard_core::util::mask_secret;

/// What replaces a credential found in an upstream answer.
pub(crate) const REDACTED: &str = "[redacted]";

/// What a header value of unknown sensitivity is shown as.
const HIDDEN: &str = "<redacted>";

/// A secret at least this long is removed wherever it occurs.
const ANYWHERE_MIN_BYTES: usize = 8;

/// A shorter secret (self-hosted servers use keys like `sk-1234`) could be
/// part of an ordinary word, so it is removed only where it stands alone.
/// Anything shorter than this protects nothing and is left alone.
const TOKEN_MIN_BYTES: usize = 4;

/// Secrets shorter than this are hidden completely when displayed:
/// `mask_secret` keeps up to seven characters, which is most of a short key.
const MASK_MIN_CHARS: usize = 20;

/// Header names whose values never carry a credential and may be shown.
fn is_plain_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "accept"
            | "accept-encoding"
            | "accept-language"
            | "anthropic-beta"
            | "anthropic-version"
            | "content-type"
            | "openai-beta"
            | "user-agent"
    )
}

/// Whether a header's name says it carries a credential. Such values are
/// marked sensitive on the wire representation, masked when displayed and
/// scrubbed from upstream answers.
pub(crate) fn is_secret_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "auth",
        "key",
        "token",
        "secret",
        "cookie",
        "password",
        "passwd",
        "credential",
        "signature",
        "session",
    ]
    .iter()
    .any(|fragment| n.contains(fragment))
}

/// Whether a configured header value has to be treated as confidential:
/// everything that is not known to be harmless.
pub(crate) fn is_confidential_header(name: &str) -> bool {
    !is_plain_header(name)
}

/// A secret as it may be shown: a short prefix and suffix of long secrets,
/// nothing at all of short ones.
pub(crate) fn mask(secret: &str) -> String {
    let secret = secret.trim();
    if secret.is_empty() {
        String::new()
    } else if secret.chars().count() < MASK_MIN_CHARS {
        "••••••••".to_string()
    } else {
        mask_secret(secret)
    }
}

/// A header value as it may be shown in logs and `Debug` output.
///
/// * names on the harmless allow-list: the value itself;
/// * names that announce a credential: the masked value, keeping an
///   authentication scheme word (`Bearer …`) readable;
/// * any other name: hidden. An unknown header is as likely to be a
///   gateway credential as a label.
pub(crate) fn display_header_value(name: &str, value: &str) -> String {
    if is_plain_header(name) {
        return value.to_string();
    }
    if !is_secret_header(name) {
        return HIDDEN.to_string();
    }
    match split_scheme(value) {
        Some((scheme, rest)) => format!("{scheme} {}", mask(rest)),
        None => mask(value),
    }
}

/// Splits `Bearer abc` / `Basic abc` into the scheme word and the secret.
fn split_scheme(value: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = value.trim().split_once(' ')?;
    let rest = rest.trim();
    let is_scheme = !scheme.is_empty()
        && scheme.len() <= 16
        && scheme.chars().all(|c| c.is_ascii_alphabetic() || c == '-');
    (is_scheme && !rest.is_empty() && !rest.contains(' ')).then_some((scheme, rest))
}

/// A character a key or token can be made of.
fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
}

/// Replaces `secret` in `text` by [`REDACTED`].
///
/// Long secrets are removed wherever they occur; short ones only where they
/// stand as a token of their own; placeholders of up to three bytes are left
/// alone so they do not blank out ordinary words.
pub(crate) fn redact_secret<'a>(text: &'a str, secret: &str) -> Cow<'a, str> {
    let secret = secret.trim();
    if secret.len() < TOKEN_MIN_BYTES || !text.contains(secret) {
        return Cow::Borrowed(text);
    }
    if secret.len() >= ANYWHERE_MIN_BYTES {
        return Cow::Owned(text.replace(secret, REDACTED));
    }
    // A short "secret" spelled like a JSON literal (`1234`, `true`) would
    // also match bare values of an error body that is forwarded as JSON,
    // and replacing those would make the body unparseable.
    if serde_json::from_str::<serde_json::Value>(secret).is_ok() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(secret) {
        let end = at + secret.len();
        let before = rest[..at].chars().next_back();
        let after = rest[end..].chars().next();
        let standalone = !before.is_some_and(is_key_char) && !after.is_some_and(is_key_char);
        out.push_str(&rest[..at]);
        out.push_str(if standalone { REDACTED } else { secret });
        rest = &rest[end..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Removes the credentials one upstream call presented from whatever the
/// upstream answered.
#[derive(Clone, Default)]
pub(crate) struct Scrubber {
    /// Longest first, so a secret that contains another is removed whole.
    secrets: Vec<String>,
}

impl Scrubber {
    /// A scrubber for everything `target` sends as a credential: its API
    /// key and the values of configured headers whose names announce one.
    pub(crate) fn for_target(target: &Target) -> Scrubber {
        let mut scrubber = Scrubber::default();
        if let Auth::ApiKey(key) = &target.auth {
            scrubber.add(key);
        }
        for (name, value) in &target.headers {
            if is_secret_header(name) {
                scrubber.add(value);
                if let Some((_, secret)) = split_scheme(value) {
                    scrubber.add(secret);
                }
            }
        }
        // A base URL may carry a credential of its own: `user:password@`,
        // or a key in a query string that is kept on every request.
        if let Ok(base) = url::Url::parse(target.base_url.trim()) {
            if let Some(password) = base.password() {
                scrubber.add(password);
                scrubber.add(&percent_decode_str(password).decode_utf8_lossy());
            }
            for (name, value) in base.query_pairs() {
                if is_secret_header(&name) {
                    scrubber.add(&value);
                }
            }
        }
        scrubber
    }

    /// Also removes `secret` (an access token minted for the call).
    pub(crate) fn add(&mut self, secret: &str) {
        let secret = secret.trim();
        if secret.len() < TOKEN_MIN_BYTES || self.secrets.iter().any(|s| s == secret) {
            return;
        }
        self.secrets.push(secret.to_string());
        self.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    }

    /// `text` without any of the secrets.
    pub(crate) fn text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(text);
        for secret in &self.secrets {
            if let Cow::Owned(clean) = redact_secret(&out, secret) {
                out = Cow::Owned(clean);
            }
        }
        out
    }

    fn field(&self, field: &mut String) {
        if let Cow::Owned(clean) = self.text(field) {
            *field = clean;
        }
    }

    /// Removes the secrets from every text an error carries.
    pub(crate) fn error(&self, mut error: UpstreamError) -> UpstreamError {
        if self.secrets.is_empty() {
            return error;
        }
        self.field(&mut error.info.message);
        for text in [
            &mut error.info.error_type,
            &mut error.info.code,
            &mut error.body,
        ]
        .into_iter()
        .flatten()
        {
            self.field(text);
        }
        error
    }
}

impl std::fmt::Debug for Scrubber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Scrubber({} secrets)", self.secrets.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::Protocol;
    use switchyard_core::config::{ProviderKind, ProxySetting};

    #[test]
    fn harmless_headers_are_shown_and_everything_else_is_not() {
        assert_eq!(
            display_header_value("Content-Type", "application/json"),
            "application/json"
        );
        assert_eq!(
            display_header_value("anthropic-beta", "tools-2024-04-04"),
            "tools-2024-04-04"
        );
        // Unknown names are hidden outright.
        assert_eq!(display_header_value("x-team", "blue"), "<redacted>");
        assert_eq!(
            display_header_value("OpenAI-Organization", "org-abc"),
            "<redacted>"
        );
        // Credential names keep the scheme and a masked value.
        assert_eq!(
            display_header_value("Authorization", "Bearer sk-proj-abcdefghijklmnopqrstuvwxyz"),
            "Bearer sk-pro…wxyz"
        );
        assert_eq!(
            display_header_value("Helicone-Auth", "Bearer sk-helicone-abcdefg-hijklmn"),
            "Bearer sk-hel…klmn"
        );
        assert_eq!(
            display_header_value(
                "Ocp-Apim-Subscription-Key",
                "0123456789abcdef0123456789abcdef"
            ),
            "012345…cdef"
        );
        // Short secrets show nothing, not even their length.
        assert_eq!(display_header_value("x-api-key", "hunter2pass"), "••••••••");
        assert_eq!(
            display_header_value("Authorization", "Bearer sk-1234"),
            "Bearer ••••••••"
        );
    }

    #[test]
    fn credential_names() {
        for name in [
            "Authorization",
            "Proxy-Authorization",
            "Helicone-Auth",
            "X-Auth-Key",
            "Ocp-Apim-Subscription-Key",
            "x-api-key",
            "api_key",
            "X-Goog-Api-Key",
            "x-secret-token",
            "Cookie",
            "X-Session-Id",
            "x-amz-signature",
        ] {
            assert!(is_secret_header(name), "{name}");
            assert!(is_confidential_header(name), "{name}");
        }
        for name in ["x-team", "x-title", "http-referer"] {
            assert!(!is_secret_header(name), "{name}");
            assert!(is_confidential_header(name), "{name}");
        }
        for name in ["User-Agent", "content-type", "anthropic-version"] {
            assert!(!is_confidential_header(name), "{name}");
        }
    }

    #[test]
    fn long_secrets_are_removed_anywhere() {
        let key = "sk-test-0123456789abcdef";
        assert_eq!(
            redact_secret(&format!("Invalid API key: {key}."), key),
            "Invalid API key: [redacted]."
        );
        assert_eq!(
            redact_secret(&format!("x{key}y and \"{key}\""), key),
            "x[redacted]y and \"[redacted]\""
        );
        assert!(matches!(
            redact_secret("nothing here", key),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn short_secrets_are_removed_only_as_whole_tokens() {
        assert_eq!(
            redact_secret("key sk-12 rejected, task-123 kept", "sk-12"),
            "key [redacted] rejected, task-123 kept"
        );
        assert_eq!(
            redact_secret("\"blue\" is not bluetooth", "blue"),
            "\"[redacted]\" is not bluetooth"
        );
        // Placeholders protect nothing.
        assert_eq!(redact_secret("the key is bad", "key"), "the key is bad");
        assert_eq!(redact_secret("anything", ""), "anything");
        // Short values spelled like JSON literals are left alone: replacing
        // them could turn a JSON error body into something unparseable.
        let body = r#"{"error":{"code":1234,"retryable":true}}"#;
        assert_eq!(redact_secret(body, "1234"), body);
        assert_eq!(redact_secret(body, "true"), body);
    }

    #[test]
    fn scrubbing_keeps_a_json_body_valid() {
        let key = "sk-live-0123456789";
        let body = serde_json::json!({"error": {"message": format!("bad key {key}"), "code": 401}})
            .to_string();
        let clean = redact_secret(&body, key);
        let parsed: serde_json::Value = serde_json::from_str(&clean).unwrap();
        assert_eq!(parsed["error"]["message"], "bad key [redacted]");
    }

    fn target() -> Target {
        Target {
            provider: "p".into(),
            kind: ProviderKind::OpenaiCompat,
            base_url: "https://gateway.example/v1".into(),
            protocol: Protocol::OpenaiChat,
            model: "m".into(),
            auth: Auth::ApiKey("  sk-upstream-0123456789  ".into()),
            headers: vec![
                (
                    "Helicone-Auth".into(),
                    "Bearer sk-helicone-abcdefghijkl".into(),
                ),
                ("X-Title".into(), "switchyard".into()),
            ],
            proxy: ProxySetting::Direct,
            project: String::new(),
            location: String::new(),
        }
    }

    #[test]
    fn scrubber_covers_the_key_and_credential_headers() {
        let mut scrubber = Scrubber::for_target(&target());
        scrubber.add("ya29.access-token-value");
        let text = "key sk-upstream-0123456789, via Bearer sk-helicone-abcdefghijkl, \
                    token ya29.access-token-value, app switchyard";
        assert_eq!(
            scrubber.text(text),
            "key [redacted], via [redacted], token [redacted], app switchyard"
        );
        assert_eq!(format!("{scrubber:?}"), "Scrubber(4 secrets)");
    }

    #[test]
    fn scrubber_covers_credentials_in_the_base_url() {
        let mut t = target();
        t.auth = Auth::None;
        t.headers.clear();
        t.base_url =
            "https://user:pass%40word1@host.test/v1?api-version=2024-10-21&key=AIzaQueryKey123"
                .into();
        let scrubber = Scrubber::for_target(&t);
        assert_eq!(
            scrubber.text("key AIzaQueryKey123 / pass@word1 / pass%40word1 / 2024-10-21"),
            "key [redacted] / [redacted] / [redacted] / 2024-10-21"
        );
    }

    #[test]
    fn scrubber_cleans_every_text_of_an_error() {
        let scrubber = Scrubber::for_target(&target());
        let mut error = UpstreamError::transport("rejected sk-upstream-0123456789");
        error.info.code = Some("bad:sk-upstream-0123456789".into());
        error.info.error_type = Some("invalid_request_error".into());
        error.body = Some("{\"error\":\"sk-upstream-0123456789\"}".into());
        let clean = scrubber.error(error);
        assert_eq!(clean.info.message, "rejected [redacted]");
        assert_eq!(clean.info.code.as_deref(), Some("bad:[redacted]"));
        assert_eq!(
            clean.info.error_type.as_deref(),
            Some("invalid_request_error")
        );
        assert_eq!(clean.body.as_deref(), Some("{\"error\":\"[redacted]\"}"));
    }

    #[test]
    fn a_target_without_credentials_scrubs_nothing() {
        let mut t = target();
        t.auth = Auth::None;
        t.headers.clear();
        let scrubber = Scrubber::for_target(&t);
        let error = UpstreamError::transport("plain");
        assert_eq!(scrubber.error(error.clone()), error);
    }
}
