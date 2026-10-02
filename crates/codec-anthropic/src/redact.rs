//! Credential redaction for error text that came from an upstream.
//!
//! Such text is never trusted to be free of credentials: compatible servers
//! and relays echo the key they were called with ("Incorrect API key
//! provided: sk-…"), and a proxy in front of the vendor may quote the
//! request's `Authorization` header in its complaint. The key in question is
//! the operator's provider key, and the message is shown to the gateway's
//! client, so everything that enters through `decode_error` or an in-stream
//! error frame goes through [`redact_secrets`] first.
//!
//! The same module exists in each codec crate (the codecs depend on
//! `switchyard-core` only and must not depend on each other).

/// What a credential is replaced with.
pub(crate) const REDACTED: &str = "[REDACTED]";

/// Names whose assigned value (`name=value`, `name: value`, `"name":"value"`)
/// is a credential. Matching is by substring, so `token` also covers
/// `access_token`, `refresh_token`, `id_token` and the like; plurals and
/// counters (`max_tokens: 5`, `token_count: 5`) do not match because the
/// separator must follow the name directly.
const SECRET_NAMES: &[&str] = &[
    "authorization",
    "api_key",
    "api-key",
    "apikey",
    "token",
    "secret",
    "password",
];

/// Bare keys recognisable by their shape: prefix, characters the key
/// continues with besides ASCII letters and digits, and the shortest run
/// after the prefix that is taken for a key (masked keys such as
/// `sk-proj-****abcd` stay below it).
///
/// * `sk-…`: OpenAI, Anthropic (`sk-ant-…`), OpenRouter, DeepSeek and most
///   compatible servers;
/// * `AIza…`: Google API keys;
/// * `ya29.…`: Google OAuth access tokens (Vertex AI).
const KEY_SHAPES: &[(&str, &[u8], usize)] = &[
    ("sk-", b"_-", 20),
    ("AIza", b"_-", 30),
    ("ya29.", b"_-.", 20),
];

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_quote(byte: Option<&u8>) -> bool {
    matches!(byte, Some(b'"' | b'\''))
}

fn skip_whitespace(bytes: &[u8], mut at: usize) -> usize {
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    at
}

/// Length in bytes of the value that starts at `text`: everything up to
/// whitespace or a character that ends a value in a query string, a header
/// dump or a JSON document.
fn value_len(text: &str) -> usize {
    text.char_indices()
        .find(|(_, c)| c.is_whitespace() || matches!(c, '"' | '\'' | '&' | ',' | ';' | '}'))
        .map_or(text.len(), |(offset, _)| offset)
}

/// `Bearer <token>` becomes `Bearer [REDACTED]`. A purely alphabetic word
/// after `Bearer` ("Bearer auth", "bearer token") is prose, not a credential,
/// and is left alone so the vendor's own guidance stays readable.
fn redact_bearer(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(found) = lower[from..].find("bearer") {
        let start = from + found;
        let after = start + "bearer".len();
        from = after;
        if start > 0 && is_word_byte(bytes[start - 1]) {
            continue;
        }
        let token_start = skip_whitespace(bytes, after);
        if token_start == after {
            continue;
        }
        // The token68 alphabet plus `_` (base64url, `sk_live_…` style keys).
        let run = bytes[token_start..]
            .iter()
            .take_while(|b| is_word_byte(**b) || b".~+/=-".contains(b))
            .count();
        // A full stop that ends the sentence is not part of the token.
        let token = text[token_start..token_start + run].trim_end_matches('.');
        let looks_secret = token.len() >= 24 || token.bytes().any(|b| !b.is_ascii_alphabetic());
        if token.is_empty() || !looks_secret {
            continue;
        }
        out.push_str(&text[copied..token_start]);
        out.push_str(REDACTED);
        copied = token_start + token.len();
        from = copied;
    }
    out.push_str(&text[copied..]);
    out
}

/// `<secret name> [=:] <value>`: the value becomes `[REDACTED]`. Optional
/// quotes around the name and the value are tolerated, which also covers
/// secret-named keys of a JSON document rendered as text. For
/// `authorization: <scheme> <credential>` the credential is what is hidden.
fn redact_assignments(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut at = 0;
    while at < bytes.len() {
        let name = SECRET_NAMES
            .iter()
            .find(|name| bytes[at..].starts_with(name.as_bytes()));
        let name_len = match name {
            Some(name) => name.len(),
            // `?key=...` / `&key=...`: the query-string spelling of an API key.
            None if at > 0
                && matches!(bytes[at - 1], b'?' | b'&')
                && bytes[at..].starts_with(b"key=") =>
            {
                "key".len()
            }
            None => {
                at += 1;
                continue;
            }
        };
        let mut cursor = at + name_len;
        at += 1;
        if is_quote(bytes.get(cursor)) {
            cursor += 1;
        }
        cursor = skip_whitespace(bytes, cursor);
        if !matches!(bytes.get(cursor), Some(b'=' | b':')) {
            continue;
        }
        cursor = skip_whitespace(bytes, cursor + 1);
        let quote = bytes.get(cursor).copied().filter(|b| is_quote(Some(b)));
        if quote.is_some() {
            cursor += 1;
        }
        // Only ASCII has been consumed since the name started, so `cursor`
        // is a character boundary of `text`. A quoted value runs to its
        // closing quote, a bare one to the next separator.
        let quoted_len = quote.and_then(|quote| {
            (cursor..bytes.len()).find(|&i| bytes[i] == quote && bytes[i - 1] != b'\\')
        });
        let (mut start, mut end) = match quoted_len {
            Some(close) => (cursor, close),
            None => (cursor, cursor + value_len(&text[cursor..])),
        };
        if start == end {
            continue;
        }
        let scheme = &lower[start..end];
        if scheme == "bearer" || scheme == "basic" {
            let credential = skip_whitespace(bytes, end);
            let len = value_len(&text[credential..]);
            if credential == end || len == 0 {
                at = end;
                continue;
            }
            (start, end) = (credential, credential + len);
        }
        if start >= copied && !text[start..end].starts_with(REDACTED) {
            out.push_str(&text[copied..start]);
            out.push_str(REDACTED);
            copied = end;
        }
        at = end.max(at);
    }
    out.push_str(&text[copied..]);
    out
}

/// Bare keys of one of the [`KEY_SHAPES`], wherever they appear.
fn redact_key_shape(text: &str, prefix: &str, extra: &[u8], min_run: usize) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(found) = text[from..].find(prefix) {
        let start = from + found;
        let body = start + prefix.len();
        from = body;
        if start > 0 && is_word_byte(bytes[start - 1]) {
            continue;
        }
        let run = bytes[body..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || extra.contains(b))
            .count();
        if run < min_run {
            continue;
        }
        // A full stop that ends the sentence is not part of the key.
        let end = body + text[body..body + run].trim_end_matches('.').len();
        out.push_str(&text[copied..start]);
        out.push_str(REDACTED);
        copied = end;
        from = copied;
    }
    out.push_str(&text[copied..]);
    out
}

/// Removes credentials from error text: `Bearer` tokens, values assigned to
/// secret-looking names, and bare vendor keys. Idempotent.
pub(crate) fn redact_secrets(text: &str) -> String {
    let mut out = redact_assignments(&redact_bearer(text));
    for (prefix, extra, min_run) in KEY_SHAPES {
        if out.contains(prefix) {
            out = redact_key_shape(&out, prefix, extra, *min_run);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_vendor_keys_are_removed() {
        assert_eq!(
            redact_secrets(
                "Incorrect API key provided: sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCD. Check it."
            ),
            "Incorrect API key provided: [REDACTED]. Check it."
        );
        assert_eq!(
            redact_secrets(
                "invalid x-api-key: sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCD-xyzAA"
            ),
            "invalid x-api-key: [REDACTED]"
        );
        assert_eq!(
            redact_secrets(
                "API key not valid: AIzaSyA1234567890abcdefghijklmnopqrstuvw. Please pass a valid API key."
            ),
            "API key not valid: [REDACTED]. Please pass a valid API key."
        );
        assert_eq!(
            redact_secrets("token ya29.a0AfH6SMBx-abcdefghijklmnopqrstuvwxyz.0123 expired."),
            "token [REDACTED] expired."
        );
    }

    #[test]
    fn bearer_tokens_and_assignments_are_removed() {
        assert_eq!(
            redact_secrets("header Authorization: Bearer abc.def-123 is not valid"),
            "header Authorization: Bearer [REDACTED] is not valid"
        );
        assert_eq!(
            redact_secrets("GET https://host/v1beta/models?key=abc123&alt=sse failed"),
            "GET https://host/v1beta/models?key=[REDACTED]&alt=sse failed"
        );
        assert_eq!(
            redact_secrets(r#"{"api_key":"abc","n":1}"#),
            r#"{"api_key":"[REDACTED]","n":1}"#
        );
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        for text in [
            "max_tokens: 5 is too large for this model",
            "You need to provide your API key in an Authorization header using Bearer auth.",
            "Incorrect API key provided: sk-proj-****abcd.",
            "the task-force met; ask-me-anything-about-this-very-long-hyphenated-word",
            "Rate limit reached for requests. Please try again in 1.5s.",
            "",
        ] {
            assert_eq!(redact_secrets(text), text);
        }
    }

    #[test]
    fn redaction_is_idempotent_and_survives_multibyte_text() {
        for text in [
            "clé: token=abc défaut",
            "Bearer abcdefghijklmnopqrstuvwxyz0123 and sk-abcdefghijklmnopqrstuvwxyz",
            "api_key=sk-abcdefghijklmnopqrstuvwxyz) token: 123",
            "é sk-éééé AIza é ya29. é",
        ] {
            let once = redact_secrets(text);
            assert_eq!(redact_secrets(&once), once, "{text}");
        }
        assert_eq!(
            redact_secrets("clé: token=abc défaut"),
            "clé: token=[REDACTED] défaut"
        );
    }
}
