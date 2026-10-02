//! Secret redaction for everything telemetry stores or shows: captured
//! headers and bodies, URLs, log messages and log fields.
//!
//! Two kinds of replacement are used:
//!
//! * credentials an operator may need to tell apart (API keys, bearer tokens)
//!   are *masked* with [`switchyard_core::util::mask_secret`], which keeps a
//!   short prefix and suffix;
//! * values that are secret in their entirety (passwords, `Basic`
//!   credentials, URL userinfo, cookies, private keys) are replaced by
//!   [`REDACTED`].
//!
//! Secrets are found in two ways, and both always apply:
//!
//! * **by name** — the value of a header, JSON key, query parameter or
//!   `name=value` pair whose name [`is_secret_key`];
//! * **by shape** — anywhere in free text, including every string inside a
//!   JSON document and the value of every header: `Bearer …` / `Basic …`
//!   credentials, vendor key shapes (`sk-…`, `AIza…`, …), URL userinfo,
//!   secret query parameters and PEM private keys (see [`redact_text`]).
//!
//! Token **counters** (`max_tokens`, `input_tokens`, `promptTokenCount`,
//! `x-ratelimit-remaining-tokens`, …) are never touched: the name test
//! excludes them and name-based JSON redaction only ever rewrites strings.
//!
//! Redaction recognises its own output (`abc…wxyz`, `•••`, `[redacted]`)
//! and leaves it alone, so headers, bodies and messages can safely pass
//! through it more than once.
//!
//! This is a best-effort safety net for data that is stored and displayed,
//! never a transformation of data that is forwarded. It errs on the side of
//! hiding: a captured prompt that discusses `password: …` loses that word.

use regex::{Captures, Regex};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::LazyLock;
use switchyard_core::util::mask_secret;

/// Replacement for values that must not be shown even partially.
pub const REDACTED: &str = "[redacted]";

/// Lower-cases a name and drops everything that is not a letter or digit, so
/// `X-Api-Key`, `api_key` and `apiKey` all compare equal.
fn compact(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether a header, JSON key, query parameter or log field with this name
/// holds a secret.
///
/// Names are compared case-insensitively and ignoring separators. A name
/// containing `token` is secret unless it is a counter or a pagination
/// cursor (`tokens`, `token_count`, `token_limit`, `page_token`, …).
pub fn is_secret_key(name: &str) -> bool {
    let c = compact(name);
    const ALWAYS: [&str; 9] = [
        "apikey",
        "secret",
        "password",
        "passwd",
        "passphrase",
        "authorization",
        "privatekey",
        "accesskey",
        "cookie",
    ];
    if ALWAYS.iter().any(|needle| c.contains(needle)) || c == "bearer" {
        return true;
    }
    if c.contains("token") {
        const NOT_SECRET: [&str; 6] = [
            "tokens",
            "tokencount",
            "tokenlimit",
            "tokenbudget",
            "tokenizer",
            "tokentype",
        ];
        return !(NOT_SECRET.iter().any(|needle| c.contains(needle)) || c.ends_with("pagetoken"));
    }
    false
}

/// Whether a value is the output of this module: [`REDACTED`], the bullets
/// [`mask_secret`] turns a short secret into, or its `abc…wxyz` /
/// `abcdef…wxyz` form. Masking such a value again would only destroy the
/// prefix an operator uses to recognise a key.
///
/// The test is on the exact shape, not on "contains an ellipsis": text that
/// was merely truncated with `…` by someone else is still redacted.
fn already_redacted(value: &str) -> bool {
    if value.starts_with('•') || value.starts_with("[redacted") {
        return true;
    }
    let head: Vec<char> = value.chars().take(11).collect();
    [3usize, 6]
        .into_iter()
        .any(|prefix| head.len() >= prefix + 5 && head[prefix] == '…')
}

fn mask(value: &str) -> String {
    if value.is_empty() || already_redacted(value) {
        value.to_string()
    } else {
        mask_secret(value)
    }
}

/// HTTP authentication schemes whose name is kept in front of the redacted
/// credential, as a regex alternation.
const AUTH_SCHEMES: &str =
    "bearer|basic|digest|negotiate|ntlm|token|key|apikey|dpop|aws4-hmac-sha256";

fn is_auth_scheme(word: &str) -> bool {
    AUTH_SCHEMES
        .split('|')
        .any(|scheme| word.eq_ignore_ascii_case(scheme))
}

/// Redacts the credential that follows an authentication scheme. `Basic`
/// credentials are `base64(user:password)`: any part of them shown is part
/// of the password, so they are hidden entirely. Everything else is masked.
fn redact_credential(scheme: &str, credential: &str) -> String {
    if already_redacted(credential) {
        credential.to_string()
    } else if scheme.eq_ignore_ascii_case("basic") {
        REDACTED.to_string()
    } else {
        mask(credential)
    }
}

/// Redacts the value of an `Authorization`-like header: `<scheme>
/// <credential>` keeps the scheme word when it is a known one; anything else
/// is a bare credential and is masked whole.
fn redact_authorization(value: &str) -> String {
    if let Some((scheme, rest)) = value.split_once(char::is_whitespace) {
        let rest = rest.trim();
        if !rest.is_empty() && is_auth_scheme(scheme) {
            return format!("{scheme} {}", redact_credential(scheme, rest));
        }
    }
    if is_auth_scheme(value) {
        // A scheme word on its own carries no credential.
        return value.to_string();
    }
    mask(value)
}

/// Redacts the value of a field whose name [`is_secret_key`].
///
/// * `authorization`-like names keep a known scheme word: `Bearer
///   sk-pro…wxyz`, `Basic [redacted]`;
/// * passwords, cookies, generic "secret" and private keys become
///   [`REDACTED`];
/// * everything else (API keys, tokens) is masked.
pub fn redact_secret_value(name: &str, value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let c = compact(name);
    const FULL: [&str; 6] = [
        "password",
        "passwd",
        "passphrase",
        "secret",
        "privatekey",
        "cookie",
    ];
    if FULL.iter().any(|needle| c.contains(needle)) {
        return REDACTED.to_string();
    }
    if c.contains("authorization") {
        return redact_authorization(value.trim());
    }
    mask(value.trim())
}

/// Redacts one header value. A header whose name [`is_secret_key`] is
/// redacted as a whole ([`redact_secret_value`]); the value of any other
/// header goes through [`redact_text`], which catches a credential carried
/// by an innocently named header: the original URI a reverse proxy forwards
/// (`x-original-uri: /v1beta/…?key=…`), a `referer` with an access token,
/// or the `sec-websocket-protocol` browser clients put an API key in.
pub fn redact_header_value(name: &str, value: &str) -> String {
    if is_secret_key(name) {
        redact_secret_value(name, value)
    } else {
        redact_text(value)
    }
}

/// Turns a header list into a map that is safe to store: names lower-cased,
/// repeated headers joined with `", "`, the values of `authorization`,
/// `proxy-authorization`, `x-api-key`, `x-goog-api-key`, `api-key`, `cookie`,
/// `set-cookie` and any other secret-looking header redacted, and every
/// other value scanned for credentials (see [`redact_header_value`]).
///
/// Accepts anything iterable as `(name, value)` string pairs, e.g.
/// `headers.iter().map(|(k, v)| (k.as_str(), String::from_utf8_lossy(v.as_bytes())))`.
pub fn redact_headers<I, K, V>(headers: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in headers {
        let name = name.as_ref().trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        let value = redact_header_value(&name, value.as_ref());
        match out.get_mut(&name) {
            Some(existing) => {
                existing.push_str(", ");
                existing.push_str(&value);
            }
            None => {
                out.insert(name, value);
            }
        }
    }
    out
}

/// How many levels of "a JSON document inside a JSON string" are followed
/// before the inner text is treated as plain text.
const MAX_EMBEDDED_JSON_DEPTH: usize = 4;

/// Strings shorter than this cannot hold anything the text rules match.
const MIN_SCANNED_LEN: usize = 6;

/// Redacts secrets inside a JSON document in place and reports whether
/// anything changed.
///
/// * A **string** stored under a key that [`is_secret_key`] is redacted as
///   a whole (strings inside an array under such a key too). Numbers,
///   booleans and nulls are never rewritten, which keeps token counters
///   intact whatever they are called. Objects are always descended into, so
///   a JSON-schema property that merely happens to be *named* `password`
///   survives.
/// * Every **other string** goes through [`redact_text`]: an upstream error
///   message that echoes the key, a `Bearer …` value under some custom
///   header name, a URL with `?key=`. A string that is itself a JSON
///   document (tool-call `arguments`) is redacted as JSON.
///
/// The `token` member of a log-probability entry (OpenAI: `{"token": "Hi",
/// "logprob": -0.1}`, Gemini: `{"token": "Hi", "tokenId": 5,
/// "logProbability": -0.1}`) is model output, not a credential, and is not
/// treated as a secret name.
pub fn redact_json_secrets(value: &mut Value) -> bool {
    redact_json(value, 0)
}

/// Whether a sibling member marks an object as a log-probability entry,
/// whose `token` is a piece of model output: a numeric `logprob` (OpenAI),
/// `logProbability` or `tokenId` (Gemini).
fn marks_model_token(key: &str, value: &Value) -> bool {
    const MARKERS: [&str; 6] = [
        "logprob",
        "log_prob",
        "logProbability",
        "log_probability",
        "tokenId",
        "token_id",
    ];
    value.is_number() && MARKERS.iter().any(|name| key.eq_ignore_ascii_case(name))
}

fn redact_json(value: &mut Value, embedded: usize) -> bool {
    match value {
        Value::Object(map) => {
            let token_is_model_output = map.contains_key("token")
                && map.iter().any(|(key, value)| marks_model_token(key, value));
            let mut changed = false;
            for (key, child) in map.iter_mut() {
                let secret = is_secret_key(key) && !(token_is_model_output && key == "token");
                changed |= if secret {
                    redact_under_secret_key(key, child, embedded)
                } else {
                    redact_json(child, embedded)
                };
            }
            changed
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items.iter_mut() {
                changed |= redact_json(item, embedded);
            }
            changed
        }
        Value::String(text) => redact_string_leaf(text, embedded),
        _ => false,
    }
}

fn looks_like_json(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

/// Redacts a string that sits under an ordinary key.
fn redact_string_leaf(text: &mut String, embedded: usize) -> bool {
    if text.len() < MIN_SCANNED_LEN {
        return false;
    }
    if embedded < MAX_EMBEDDED_JSON_DEPTH
        && looks_like_json(text)
        && let Ok(mut inner) = serde_json::from_str::<Value>(text)
    {
        if !redact_json(&mut inner, embedded + 1) {
            return false;
        }
        *text = inner.to_string();
        return true;
    }
    let redacted = match redact_text_cow(text) {
        Cow::Owned(redacted) if redacted != *text => redacted,
        _ => return false,
    };
    *text = redacted;
    true
}

fn redact_string_in_place(key: &str, text: &mut String) -> bool {
    if text.is_empty() {
        return false;
    }
    let replacement = redact_secret_value(key, text);
    if replacement == *text {
        return false;
    }
    *text = replacement;
    true
}

fn redact_under_secret_key(key: &str, value: &mut Value, embedded: usize) -> bool {
    match value {
        Value::String(text) => redact_string_in_place(key, text),
        Value::Array(items) => {
            let mut changed = false;
            for item in items.iter_mut() {
                changed |= match item {
                    Value::String(text) => redact_string_in_place(key, text),
                    other => redact_json(other, embedded),
                };
            }
            changed
        }
        Value::Object(_) => redact_json(value, embedded),
        _ => false,
    }
}

/// Whether a query parameter name carries a secret: `key`, `sig`,
/// `signature` or anything [`is_secret_key`]. A trailing `[]` is ignored.
fn is_secret_query_param(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let lower = lower
        .strip_suffix("[]")
        .or_else(|| lower.strip_suffix("%5b%5d"))
        .unwrap_or(&lower);
    matches!(lower, "key" | "sig" | "signature") || is_secret_key(lower)
}

/// Redacts the values of secret query parameters (`key=`, `api_key=`,
/// `access_token=`, …) in a URL or path-and-query string. Everything else,
/// including the fragment, is returned unchanged.
pub fn redact_url(url: &str) -> String {
    let Some((before, after)) = url.split_once('?') else {
        return url.to_string();
    };
    let (query, fragment) = match after.split_once('#') {
        Some((query, fragment)) => (query, Some(fragment)),
        None => (after, None),
    };
    let redacted: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) if is_secret_query_param(name) && !value.is_empty() => {
                format!("{name}={}", redact_secret_value(name, value))
            }
            _ => pair.to_string(),
        })
        .collect();
    let mut out = format!("{before}?{}", redacted.join("&"));
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

fn regex(pattern: &str) -> Regex {
    // The patterns are literals in this file, exercised by the unit tests.
    Regex::new(pattern).expect("redaction pattern is a valid regular expression")
}

/// Characters that end a bare (unquoted) value in free text.
const BARE: &str = r#"[^\s"',;&}\]]"#;

/// A PEM private key, or the rest of the text when its end marker is
/// missing (a truncated body).
static PRIVATE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)")
});

/// `scheme://userinfo@`: the userinfo runs up to the last `@` of the
/// authority, so a password containing `@` is covered too.
static USERINFO: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)\b([a-z][a-z0-9+.\-]*://)[^\s/?#"'<>\\]+@"#));

/// `authorization: <value>` in its header, `name=value` and JSON spellings
/// (the quotes may be backslash-escaped: JSON inside a JSON string).
/// Groups: 1 everything before the value, 2 quote after the name, 3 quote
/// before the value, 4 first word, 5 blanks, 6 second word.
static AUTHORIZATION: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r#"(?i)(\b(?:proxy-)?authorization(\\*["'])?[ \t]*[:=][ \t]*(\\*["'])?)({BARE}+)(?:([ \t]+)({BARE}+))?"#
    ))
});

/// `Bearer <credential>` anywhere.
static BEARER: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)\b(bearer)([ \t]+)([A-Za-z0-9._~+/=\-]{8,})"));

/// `Basic <base64>` anywhere.
static BASIC: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)\b(basic)([ \t]+)([A-Za-z0-9+/]{8,}={0,2})"));

/// Vendor key shapes: OpenAI / Anthropic / OpenRouter / DeepSeek (`sk-…`),
/// Google API keys (`AIza…`), Google OAuth access tokens (`ya29.…`), Groq
/// (`gsk_…`) and xAI (`xai-…`).
static KEY_SHAPE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"\b(sk-[A-Za-z0-9_\-]{16,}|AIza[0-9A-Za-z_\-]{30,}|ya29\.[0-9A-Za-z_\-.]{20,}|gsk_[A-Za-z0-9]{20,}|xai-[A-Za-z0-9]{20,})",
    )
});

/// The WebSocket sub-protocol browser clients of the OpenAI Realtime API
/// carry their key in.
static WS_PROTOCOL_KEY: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)(openai-insecure-api-key\.)([^\s,;"'\\]+)"#));

/// Secret query parameters inside free text. Groups: 1 `?name=`, 2 name,
/// 3 value.
static QUERY_PARAM: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"(?i)([?&](key|api[_-]?key|access[_-]?token|token|client[_-]?secret|secret|password|signature|sig)=)([^&\s"'<>#\\]+)"#,
    )
});

/// `name: value` / `name=value` / `"name":"value"` for well-known secret
/// names, also as the tail of a longer name (`OPENAI_API_KEY`,
/// `db.password`, `x-goog-api-key`). The value is a quoted string (taken up
/// to its closing quote), a backslash-escaped quoted string, or a bare
/// word, optionally wrapped in Rust's `Some(`.
/// Groups: 1 name, 2 quote after the name, 3 double-quoted value,
/// 4 single-quoted value, 5 escaped-quoted value, 6 bare value.
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r#"(?i)\b(?:[a-z0-9_.\-]*[_.\-])?(api[_-]?key|access[_-]?token|refresh[_-]?token|id[_-]?token|auth[_-]?token|authorization[_-]token|client[_-]?secret|secret[_-]?key|private[_-]?key|password|passwd)(\\*["'])?[ \t]*[:=][ \t]*(?:Some\()?(?:"((?:[^"\\\n]|\\.)*)"|'([^'\n]*)'|\\+"([^"\\\n]*)\\+"|({BARE}{{4,}}))"#
    ))
});

/// Whether a word that follows `Bearer` or `Basic` in free text is a
/// credential rather than prose ("the bearer of bad news", "basic
/// understanding"): it contains a digit, or mixes cases beyond a leading
/// capital. Generated tokens practically always do; dictionary words do not.
fn looks_generated(token: &str) -> bool {
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
    let has_inner_upper = token.chars().skip(1).any(|c| c.is_ascii_uppercase());
    has_digit || (has_lower && has_inner_upper)
}

/// Unquoted words that stand for "no value".
fn is_literal(value: &str) -> bool {
    ["null", "none", "nil", "true", "false", "undefined"]
        .iter()
        .any(|literal| value.eq_ignore_ascii_case(literal))
}

/// Whether a bare word is source code or a placeholder rather than a value
/// (`os.environ[…`, `input(…`, `<your key>`, `{api_key}`). Keys, tokens
/// and base64 never contain brackets.
fn looks_like_code(value: &str) -> bool {
    value.contains(['(', ')', '[', '{', '<', '>'])
}

/// Whether a bare word is an ordinary word of a sentence: letters only
/// (apart from closing punctuation) and not mixed-case like a generated
/// token.
fn is_plain_word(word: &str) -> bool {
    let core = word.trim_end_matches(['.', '`', ':', '!', '?']);
    !core.is_empty() && core.chars().all(|c| c.is_ascii_alphabetic()) && !looks_generated(core)
}

/// Splits the backslashes off the end of a value: in JSON carried inside a
/// JSON string the closing quote is `\"`, and the backslash belongs to it.
fn split_trailing_backslashes(value: &str) -> (&str, &str) {
    let core = value.trim_end_matches('\\');
    (core, &value[core.len()..])
}

/// Applies one rule, keeping the text borrowed when nothing matched.
fn rewrite<'t>(
    text: Cow<'t, str>,
    pattern: &Regex,
    replace: impl FnMut(&Captures<'_>) -> String,
) -> Cow<'t, str> {
    let replaced = match pattern.replace_all(&text, replace) {
        Cow::Borrowed(_) => None,
        Cow::Owned(replaced) => Some(replaced),
    };
    match replaced {
        Some(replaced) => Cow::Owned(replaced),
        None => text,
    }
}

fn redact_authorization_match(caps: &Captures<'_>) -> String {
    let unchanged = || caps[0].to_string();
    let name_quoted = caps.get(2).is_some();
    let value_quoted = caps.get(3).is_some();
    if name_quoted && !value_quoted {
        // `"authorization":null`: a JSON value that is not a string.
        return unchanged();
    }
    // In running text (no quotes around the value) a plain word after
    // "authorization:" is the rest of a sentence and a bracketed one is a
    // placeholder: "Authorization: handled by the gateway", "Authorization:
    // Bearer <token>". Inside quotes it is a header value, whatever it
    // looks like.
    let harmless = |word: &str| !value_quoted && (is_plain_word(word) || looks_like_code(word));
    let prefix = &caps[1];
    let blanks = caps.get(5).map_or("", |m| m.as_str());
    let (first, first_tail) = split_trailing_backslashes(&caps[4]);
    let second = caps.get(6).map(|m| split_trailing_backslashes(m.as_str()));

    if let Some((credential, tail)) = second
        && is_auth_scheme(first)
    {
        // A known scheme: whatever follows is the credential, however short.
        if harmless(credential) {
            return unchanged();
        }
        return format!(
            "{prefix}{first}{blanks}{}{tail}",
            redact_credential(first, credential)
        );
    }

    // No scheme we know: the first word is a bare credential (the gateway
    // itself accepts the raw `Authorization` value as a client key), unless
    // it says nothing.
    let first_out =
        if first.len() < 4 || is_literal(first) || is_auth_scheme(first) || harmless(first) {
            first.to_string()
        } else {
            mask(first)
        };
    match second {
        None => format!("{prefix}{first_out}{first_tail}"),
        Some((second, second_tail)) => {
            // Possibly "<unknown scheme> <credential>": the second word is
            // only touched when it looks like a credential.
            let second_out = if looks_generated(second) && !looks_like_code(second) {
                mask(second)
            } else {
                second.to_string()
            };
            format!("{prefix}{first_out}{first_tail}{blanks}{second_out}{second_tail}")
        }
    }
}

fn redact_assignment_match(caps: &Captures<'_>) -> String {
    let unchanged = || caps[0].to_string();
    let name = &caps[1];
    let name_quoted = caps.get(2).is_some();
    let (Some(whole), Some(value)) = (
        caps.get(0),
        caps.get(3)
            .or_else(|| caps.get(4))
            .or_else(|| caps.get(5))
            .or_else(|| caps.get(6)),
    ) else {
        return unchanged();
    };
    let bare = caps.get(6).is_some();
    let (core, tail) = if bare {
        split_trailing_backslashes(value.as_str())
    } else {
        (value.as_str(), "")
    };
    // A bare value after a quoted name is a JSON literal or number
    // (`"api_key":null`); bare `None` / `null` / `false` say "no value";
    // a bare value with brackets is source code or a placeholder
    // (`api_key = os.environ[…`, `password = input(…`, `api_key=<your key>`).
    if core.is_empty()
        || already_redacted(core)
        || (bare && (name_quoted || is_literal(core) || looks_like_code(core)))
    {
        return unchanged();
    }
    let text = whole.as_str();
    let before = &text[..value.start() - whole.start()];
    let after = &text[value.end() - whole.start()..];
    format!("{before}{}{tail}{after}", redact_secret_value(name, core))
}

fn redact_text_cow(text: &str) -> Cow<'_, str> {
    let out = Cow::Borrowed(text);
    let out = rewrite(out, &PRIVATE_KEY, |_| REDACTED.to_string());
    let out = rewrite(out, &USERINFO, |caps| format!("{}{REDACTED}@", &caps[1]));
    let out = rewrite(out, &AUTHORIZATION, redact_authorization_match);
    let out = rewrite(out, &BEARER, |caps| {
        let token = &caps[3];
        // A sentence's full stop is not part of a token.
        let core = token.trim_end_matches('.');
        let all_letters = core.chars().all(|c| c.is_ascii_alphabetic());
        if looks_generated(core) || (core.len() >= 16 && !all_letters) {
            format!(
                "{}{}{}{}",
                &caps[1],
                &caps[2],
                mask(core),
                &token[core.len()..]
            )
        } else {
            caps[0].to_string()
        }
    });
    let out = rewrite(out, &BASIC, |caps| {
        // Padding alone proves nothing: "basic settings=default".
        if looks_generated(caps[3].trim_end_matches('=')) {
            format!("{}{}{REDACTED}", &caps[1], &caps[2])
        } else {
            caps[0].to_string()
        }
    });
    let out = rewrite(out, &QUERY_PARAM, |caps| {
        format!("{}{}", &caps[1], redact_secret_value(&caps[2], &caps[3]))
    });
    let out = rewrite(out, &ASSIGNMENT, redact_assignment_match);
    let out = rewrite(out, &WS_PROTOCOL_KEY, |caps| {
        format!("{}{}", &caps[1], mask(&caps[2]))
    });
    rewrite(out, &KEY_SHAPE, |caps| mask(&caps[1]))
}

/// Redacts obvious secrets in free text: log messages, error strings,
/// header values, strings inside JSON and bodies that are not JSON.
///
/// Recognised, in this order:
///
/// * PEM private keys → [`REDACTED`];
/// * URL userinfo (`scheme://user:password@host`) → `scheme://[redacted]@host`;
/// * `authorization: <scheme> <credential>` (also `proxy-authorization`,
///   `=` instead of `:`, quoted as in JSON or a debug-printed map): any
///   credential after a known scheme, however short; `Basic` credentials →
///   [`REDACTED`], others masked. A value without a scheme is a bare
///   credential and is masked. Outside quotes, a plain word ("authorization:
///   failed") or a placeholder (`Bearer <token>`) is left alone;
/// * `Bearer <token>` and `Basic <base64>` anywhere, when the word that
///   follows looks generated rather than like prose;
/// * secret query parameters (`?key=`, `&access_token=`, …);
/// * `name=value` / `name: value` / `"name":"value"` pairs whose name ends
///   in a well-known secret name (`api_key`, `OPENAI_API_KEY`,
///   `client_secret`, `password`, …); passwords and secrets →
///   [`REDACTED`], keys and tokens masked. JSON literals (`"api_key":null`)
///   and unquoted source code (`api_key = os.environ[…]`) are left alone;
/// * `openai-insecure-api-key.<key>` (WebSocket sub-protocol);
/// * vendor key shapes (`sk-…`, `AIza…`, `ya29.…`, `gsk_…`, `xai-…`).
///
/// Ordinary prose is left untouched. A secret that matches none of these
/// (a custom-format key quoted in a sentence) cannot be recognised.
pub fn redact_text(text: &str) -> String {
    redact_text_cow(text).into_owned()
}

/// Redacts `text` if it is one JSON document: `None` when it is not,
/// borrowed when nothing in it needed redacting (the exact bytes are kept),
/// re-serialised otherwise.
fn redact_json_document(text: &str) -> Option<Cow<'_, str>> {
    if !looks_like_json(text) {
        return None;
    }
    let mut value = serde_json::from_str::<Value>(text).ok()?;
    Some(if redact_json_secrets(&mut value) {
        Cow::Owned(value.to_string())
    } else {
        Cow::Borrowed(text)
    })
}

/// Where the JSON payload of a line starts, if it has one: after an SSE
/// `data:` prefix, or at the start of the line (JSON lines).
fn json_payload_start(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("data:").unwrap_or(line);
    let payload = rest.trim_start_matches([' ', '\t']);
    (payload.starts_with('{') || payload.starts_with('[')).then(|| line.len() - payload.len())
}

/// Redacts a captured request or response body.
///
/// * A body that is one JSON document goes through [`redact_json_secrets`].
/// * Otherwise each line that carries a JSON document — an SSE `data:` line
///   or a JSON-lines record — is redacted as JSON, and everything else
///   (other SSE fields, plain text, truncated JSON) through [`redact_text`].
///
/// JSON is re-serialised only when something in it was redacted, so
/// untouched bodies keep their exact bytes.
///
/// A secret split across stream events (a tool call's arguments arriving in
/// fragments) cannot be recognised in the captured stream.
pub fn redact_body(body: &str) -> String {
    if let Some(redacted) = redact_json_document(body) {
        return redacted.into_owned();
    }
    let mut out = String::with_capacity(body.len());
    // Start of the run of lines not yet copied to `out`; the run is redacted
    // as one text so rules can see across its line breaks.
    let mut pending = 0;
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let content = line.trim_end_matches(['\n', '\r']);
        let Some(start) = json_payload_start(content) else {
            continue;
        };
        let Some(redacted) = redact_json_document(&content[start..]) else {
            continue;
        };
        out.push_str(&redact_text_cow(&body[pending..line_start]));
        out.push_str(&content[..start]);
        out.push_str(&redacted);
        out.push_str(&line[content.len()..]);
        pending = offset;
    }
    out.push_str(&redact_text_cow(&body[pending..]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    const KEY: &str = "sk-proj-abcdefghijklmnopqrstuvwxyz";
    const KEY_MASKED: &str = "sk-pro…wxyz";
    const GOOGLE_KEY: &str = "AIzaSyA-0123456789abcdefghijklmnopqrstu";
    /// base64("user:password")
    const BASIC: &str = "dXNlcjpwYXNzd29yZA==";

    #[test]
    fn secret_names() {
        for name in [
            "Authorization",
            "proxy-authorization",
            "x-api-key",
            "X-Goog-Api-Key",
            "api-key",
            "api_key",
            "apiKey",
            "OPENAI_API_KEY",
            "cookie",
            "Set-Cookie",
            "password",
            "client_secret",
            "private_key",
            "access_token",
            "refreshToken",
            "authorization_token",
            "token",
            "x-amz-security-token",
            "aws_access_key_id",
        ] {
            assert!(is_secret_key(name), "{name} should be secret");
        }
    }

    #[test]
    fn counters_and_ordinary_names_are_not_secret() {
        for name in [
            "max_tokens",
            "max_output_tokens",
            "max_completion_tokens",
            "input_tokens",
            "output_tokens",
            "total_tokens",
            "cached_tokens",
            "reasoning_tokens",
            "budget_tokens",
            "cache_read_input_tokens",
            "promptTokenCount",
            "candidatesTokenCount",
            "thoughtsTokenCount",
            "totalTokens",
            "inputTokenLimit",
            "outputTokenLimit",
            "token_type",
            "nextPageToken",
            "page_token",
            "x-ratelimit-remaining-tokens",
            "anthropic-ratelimit-tokens-limit",
            "model",
            "content-type",
            "credential_id",
            "key",
            "user-agent",
            "anthropic-beta",
        ] {
            assert!(!is_secret_key(name), "{name} should not be secret");
        }
    }

    #[test]
    fn redacted_values_are_recognised_by_shape() {
        for value in [
            "•••••",
            "[redacted]",
            "[redacted",
            "abc…6789",
            "sk-pro…wxyz",
            "sk-pro…wxyz, sk-pro…abcd",
        ] {
            assert!(already_redacted(value), "{value}");
        }
        for value in [
            "",
            "hunter2",
            // Truncated by someone else: the ellipsis is not ours.
            "hunter2…",
            "abcdefgh12345678…",
            "ab…cdefg",
            "abc…def",
        ] {
            assert!(!already_redacted(value), "{value}");
        }
    }

    #[test]
    fn headers_are_lowercased_joined_and_redacted() {
        let headers = redact_headers([
            ("Authorization", format!("Bearer {KEY}")),
            ("X-Api-Key", KEY.to_string()),
            ("x-goog-api-key", GOOGLE_KEY.to_string()),
            ("Proxy-Authorization", format!("Basic {BASIC}")),
            ("Cookie", "session=abc; other=def".to_string()),
            ("Content-Type", "application/json".to_string()),
            ("Accept", "text/event-stream".to_string()),
            ("accept", "application/json".to_string()),
            ("x-ratelimit-remaining-tokens", "149984".to_string()),
        ]);
        assert_eq!(headers["authorization"], format!("Bearer {KEY_MASKED}"));
        assert_eq!(headers["x-api-key"], KEY_MASKED);
        assert_eq!(headers["x-goog-api-key"], "AIzaSy…rstu");
        assert_eq!(headers["proxy-authorization"], "Basic [redacted]");
        assert_eq!(headers["cookie"], REDACTED);
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["accept"], "text/event-stream, application/json");
        assert_eq!(headers["x-ratelimit-remaining-tokens"], "149984");
        assert_eq!(headers.len(), 8);
    }

    #[test]
    fn redacting_headers_twice_changes_nothing() {
        let once = redact_headers([
            ("authorization", format!("Bearer {KEY}")),
            ("authorization", "Bearer abc123def456".to_string()),
            ("proxy-authorization", format!("Basic {BASIC}")),
            ("x-api-key", "short".to_string()),
            ("cookie", "a=b".to_string()),
            ("x-original-uri", format!("/v1/models?key={GOOGLE_KEY}")),
            ("referer", "http://bob:pw@host/".to_string()),
        ]);
        assert_eq!(
            once["authorization"],
            format!("Bearer {KEY_MASKED}, Bearer abc…f456")
        );
        let twice = redact_headers(once.iter());
        assert_eq!(twice, once);
    }

    #[test]
    fn authorization_values() {
        assert_eq!(redact_header_value("authorization", KEY), KEY_MASKED);
        assert_eq!(redact_header_value("authorization", ""), "");
        // The credential after a scheme is redacted however short it is.
        assert_eq!(
            redact_header_value("Authorization", "Bearer sy-test"),
            "Bearer •••••••"
        );
        // A scheme word alone carries nothing.
        assert_eq!(redact_header_value("authorization", "Bearer"), "Bearer");
        // Basic credentials are a password: nothing of them is kept, not
        // even for the shortest one (base64("admin:pw")).
        assert_eq!(
            redact_header_value("proxy-authorization", "Basic YWRtaW46cHc="),
            "Basic [redacted]"
        );
        assert_eq!(
            redact_header_value("authorization", "basic   YWRtaW46cHc="),
            "basic [redacted]"
        );
        // An unknown first word is not assumed to be a scheme: it may be
        // the secret itself.
        let out = redact_header_value("authorization", "s3cr3t-part-one part-two");
        assert_eq!(out, "s3cr3t…-two");
        assert_eq!(
            redact_header_value(
                "authorization",
                "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20260101/us-east-1/s3/aws4_request, SignedHeaders=host, Signature=fe5f80f77d5fa3beca038a248ff027d0445342fe2855ddc963176630326f1024"
            ),
            "AWS4-HMAC-SHA256 Creden…1024"
        );
    }

    #[test]
    fn short_secrets_are_fully_hidden() {
        assert_eq!(redact_header_value("x-api-key", "abc"), "•••");
    }

    #[test]
    fn header_values_are_scanned_whatever_the_header_is_called() {
        let headers = redact_headers([
            (
                "x-original-uri",
                format!("/v1beta/models/gemini-2.5-pro:generateContent?key={GOOGLE_KEY}&alt=sse"),
            ),
            (
                "sec-websocket-protocol",
                "realtime, openai-insecure-api-key.my-team-key-prod-1, openai-beta.realtime-v1"
                    .to_string(),
            ),
            (
                "referer",
                "https://app.example/chat?access_token=abcdef0123456789abcdef".to_string(),
            ),
            ("x-upstream-auth", format!("Bearer {KEY}")),
            ("x-proxy", "http://alice:hunter2@10.0.0.1:3128".to_string()),
            ("user-agent", "curl/8.5.0".to_string()),
            ("anthropic-beta", "prompt-caching-2024-07-31".to_string()),
        ]);
        assert_eq!(
            headers["x-original-uri"],
            "/v1beta/models/gemini-2.5-pro:generateContent?key=AIzaSy…rstu&alt=sse"
        );
        assert_eq!(
            headers["sec-websocket-protocol"],
            "realtime, openai-insecure-api-key.my-…od-1, openai-beta.realtime-v1"
        );
        assert_eq!(
            headers["referer"],
            "https://app.example/chat?access_token=abcdef…cdef"
        );
        assert_eq!(headers["x-upstream-auth"], format!("Bearer {KEY_MASKED}"));
        assert_eq!(headers["x-proxy"], "http://[redacted]@10.0.0.1:3128");
        assert_eq!(headers["user-agent"], "curl/8.5.0");
        assert_eq!(headers["anthropic-beta"], "prompt-caching-2024-07-31");
    }

    #[test]
    fn json_secrets_are_redacted_but_counters_survive() {
        let mut body = json!({
            "model": "gpt-5",
            "max_tokens": 1024,
            "max_output_tokens": 2048,
            "api_key": KEY,
            "password": "hunter2-and-more",
            "usage": {"input_tokens": 12, "output_tokens": 34, "total_tokens": 46},
            "tools": [{
                "type": "mcp",
                "server_url": "https://example.com/mcp",
                "authorization": KEY,
                "headers": {
                    "Authorization": format!("Bearer {KEY}"),
                    "Proxy-Authorization": format!("Basic {BASIC}"),
                    "X-Trace": "abc"
                }
            }],
            "mcp_servers": [{"name": "x", "authorization_token": KEY}],
            "api_keys": [KEY, "short"]
        });
        assert!(redact_json_secrets(&mut body));
        assert_eq!(
            body,
            json!({
                "model": "gpt-5",
                "max_tokens": 1024,
                "max_output_tokens": 2048,
                "api_key": KEY_MASKED,
                "password": REDACTED,
                "usage": {"input_tokens": 12, "output_tokens": 34, "total_tokens": 46},
                "tools": [{
                    "type": "mcp",
                    "server_url": "https://example.com/mcp",
                    "authorization": KEY_MASKED,
                    "headers": {
                        "Authorization": format!("Bearer {KEY_MASKED}"),
                        "Proxy-Authorization": "Basic [redacted]",
                        "X-Trace": "abc"
                    }
                }],
                "mcp_servers": [{"name": "x", "authorization_token": KEY_MASKED}],
                "api_keys": [KEY_MASKED, "•••••"]
            })
        );
        // Redacting again changes nothing.
        let once = body.clone();
        assert!(!redact_json_secrets(&mut body));
        assert_eq!(body, once);
    }

    #[test]
    fn json_without_secrets_reports_no_change() {
        let mut body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 64,
            "system": "You are a basic assistant. Authorization is handled elsewhere.",
            "messages": [
                {"role": "user", "content": "what is a bearer token? how do I reset my password?"},
                {"role": "assistant", "content": [{"type": "text", "text": "Use the form."}]}
            ],
            "usageMetadata": {"promptTokenCount": 3, "totalTokenCount": 9}
        });
        let before = body.clone();
        assert!(!redact_json_secrets(&mut body));
        assert_eq!(body, before);
    }

    #[test]
    fn json_string_values_are_scanned_under_any_key() {
        let mut body = json!({
            "error": {
                "message": format!("Incorrect API key provided: {KEY}. You can find your API key at https://example.com/keys"),
                "type": "invalid_request_error"
            },
            "tools": [{"headers": {"X-Upstream-Auth": format!("Bearer {KEY}")}}],
            "url": format!("https://h/v1beta/models?key={GOOGLE_KEY}&alt=sse"),
            "proxy": "socks5://bob:s3cr3t@10.0.0.1:1080",
            "note": "Proxy-Authorization: Basic YWRtaW46cHc=",
            "list": ["plain", format!("x-api-key: {KEY}")],
            "n": 5,
            "short": "ok"
        });
        assert!(redact_json_secrets(&mut body));
        assert_eq!(
            body,
            json!({
                "error": {
                    "message": "Incorrect API key provided: sk-pro…wxyz. You can find your API key at https://example.com/keys",
                    "type": "invalid_request_error"
                },
                "tools": [{"headers": {"X-Upstream-Auth": "Bearer sk-pro…wxyz"}}],
                "url": "https://h/v1beta/models?key=AIzaSy…rstu&alt=sse",
                "proxy": "socks5://[redacted]@10.0.0.1:1080",
                "note": "Proxy-Authorization: Basic [redacted]",
                "list": ["plain", "x-api-key: sk-pro…wxyz"],
                "n": 5,
                "short": "ok"
            })
        );
    }

    #[test]
    fn json_carried_in_a_string_is_redacted_as_json() {
        // Chat Completions tool-call arguments.
        let arguments =
            json!({"password": "hunter2", "api_key": KEY, "city": "Paris", "max_tokens": 5});
        let mut body = json!({
            "messages": [{
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "login", "arguments": arguments.to_string()}
                }]
            }]
        });
        assert!(redact_json_secrets(&mut body));
        let redacted = body["messages"][0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(redacted).unwrap(),
            json!({"password": REDACTED, "api_key": KEY_MASKED, "city": "Paris", "max_tokens": 5})
        );

        // Embedded JSON without secrets keeps its exact text.
        let text = "{ \"city\": \"Paris\" }";
        let mut body = json!({"arguments": text});
        assert!(!redact_json_secrets(&mut body));
        assert_eq!(body["arguments"], text);
    }

    #[test]
    fn deeply_nested_json_strings_fall_back_to_text_rules() {
        // Each level wraps the previous document in a string.
        let mut document = json!({"api_key": KEY}).to_string();
        for _ in 0..MAX_EMBEDDED_JSON_DEPTH + 3 {
            document = json!({"inner": document}).to_string();
        }
        let out = redact_body(&document);
        assert!(!out.contains("abcdefghijklmnopqrstuvwxyz"), "{out}");
    }

    #[test]
    fn schema_property_named_like_a_secret_is_kept() {
        let mut body = json!({
            "tools": [{
                "name": "login",
                "input_schema": {
                    "type": "object",
                    "properties": {"password": {"type": "string", "description": "the password"}},
                    "required": ["password"]
                }
            }]
        });
        let before = body.clone();
        assert!(!redact_json_secrets(&mut body));
        assert_eq!(body, before);
    }

    #[test]
    fn logprob_tokens_are_model_output() {
        let mut body = json!({
            "logprobs": {"content": [{
                "token": "Hello", "logprob": -0.01, "bytes": [72],
                "top_logprobs": [{"token": "Hello", "logprob": -0.01, "bytes": [72]}]
            }]},
            "token": "an-actual-secret-token"
        });
        assert!(redact_json_secrets(&mut body));
        assert_eq!(body["logprobs"]["content"][0]["token"], "Hello");
        assert_eq!(
            body["logprobs"]["content"][0]["top_logprobs"][0]["token"],
            "Hello"
        );
        assert_eq!(body["token"], "an-act…oken");

        // A string under `logprob` proves nothing about its sibling.
        let mut body = json!({"token": "an-actual-secret-token", "logprob": "n/a"});
        assert!(redact_json_secrets(&mut body));
        assert_eq!(body["token"], "an-act…oken");
    }

    #[test]
    fn gemini_logprob_tokens_are_model_output() {
        let mut body = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "Hello world"}]},
                "avgLogprobs": -0.15,
                "logprobsResult": {
                    "chosenCandidates": [
                        {"token": "Hello", "tokenId": 4521, "logProbability": -0.1},
                        {"token": " world", "tokenId": 991, "logProbability": -0.2}
                    ],
                    "topCandidates": [{"candidates": [
                        {"token": "Greetings", "tokenId": 77, "logProbability": -2.3},
                        {"token": "Salutations-and-greetings", "logProbability": -4.0},
                        {"token": "Howdy-partner-how-are-you", "tokenId": 12}
                    ]}]
                }
            }]
        });
        let before = body.clone();
        assert!(!redact_json_secrets(&mut body));
        assert_eq!(body, before);
    }

    #[test]
    fn numeric_values_under_secret_names_are_untouched() {
        let mut body = json!({"token": 5, "secret": true, "password": null});
        assert!(!redact_json_secrets(&mut body));
        assert_eq!(body, json!({"token": 5, "secret": true, "password": null}));
    }

    #[test]
    fn url_query_values_are_masked() {
        assert_eq!(
            redact_url(&format!(
                "/v1beta/models/gemini-2.5-pro:generateContent?key={KEY}&alt=sse"
            )),
            format!("/v1beta/models/gemini-2.5-pro:generateContent?key={KEY_MASKED}&alt=sse")
        );
        assert_eq!(
            redact_url(&format!("https://h/p?alt=sse&api_key={KEY}#frag")),
            format!("https://h/p?alt=sse&api_key={KEY_MASKED}#frag")
        );
        assert_eq!(
            redact_url(&format!("/x?access_token={KEY}&keys[]=1&key[]={KEY}")),
            format!("/x?access_token={KEY_MASKED}&keys[]=1&key[]={KEY_MASKED}")
        );
        assert_eq!(redact_url("/v1/models"), "/v1/models");
        assert_eq!(
            redact_url("/v1/models?limit=5&max_tokens=7"),
            "/v1/models?limit=5&max_tokens=7"
        );
        assert_eq!(redact_url("/x?key="), "/x?key=");
        // Passwords are not shown even partially.
        assert_eq!(
            redact_url("/login?user=bob&password=correct-horse-battery"),
            "/login?user=bob&password=[redacted]"
        );
        let once = redact_url(&format!("/x?key={KEY}&password=abc"));
        assert_eq!(redact_url(&once), once);
    }

    #[test]
    fn text_bearer_and_key_shapes() {
        assert_eq!(
            redact_text(&format!("sending Authorization: Bearer {KEY} upstream")),
            format!("sending Authorization: Bearer {KEY_MASKED} upstream")
        );
        assert_eq!(
            redact_text(&format!("Incorrect API key provided: {KEY}.")),
            format!("Incorrect API key provided: {KEY_MASKED}.")
        );
        assert_eq!(
            redact_text(&format!("key {GOOGLE_KEY} rejected")),
            "key AIzaSy…rstu rejected"
        );
        assert_eq!(
            redact_text("token ya29.a0AfH6SMBx-abcdefghijklmnop expired"),
            "token ya29.a…mnop expired"
        );
        assert_eq!(
            redact_text("groq gsk_abcdefghijklmnopqrstuvwx and xai-abcdefghijklmnopqrstuvwx"),
            "groq gsk_ab…uvwx and xai-ab…uvwx"
        );
        // A bearer credential outside an authorization header.
        assert_eq!(
            redact_text("client sent Bearer abc123def456ghi, rejected."),
            "client sent Bearer abc…6ghi, rejected."
        );
        assert_eq!(
            redact_text("got bearer Zx81kqPlmN7vbnQw."),
            "got bearer Zx8…bnQw."
        );
    }

    #[test]
    fn text_authorization_headers() {
        for (input, expected) in [
            (
                format!("Proxy-Authorization: Basic {BASIC}"),
                "Proxy-Authorization: Basic [redacted]".to_string(),
            ),
            (
                format!("sent authorization: Basic {BASIC} to the proxy"),
                "sent authorization: Basic [redacted] to the proxy".to_string(),
            ),
            (
                format!(r#"headers={{"proxy-authorization": "Basic {BASIC}"}}"#),
                r#"headers={"proxy-authorization": "Basic [redacted]"}"#.to_string(),
            ),
            // Shorter than anything the context-free Bearer rule accepts.
            (
                "Authorization: Bearer abc123def456".to_string(),
                "Authorization: Bearer abc…f456".to_string(),
            ),
            (
                "authorization=Bearer sy-test&x=1".to_string(),
                "authorization=Bearer •••••••&x=1".to_string(),
            ),
            (
                "Authorization: Token 0123456789abcdef0123".to_string(),
                "Authorization: Token 012345…0123".to_string(),
            ),
            // A bare credential.
            (
                "authorization: abcdef0123456789".to_string(),
                "authorization: abc…6789".to_string(),
            ),
            // JSON carried inside a JSON string: the quotes are escaped.
            (
                r#"{"body":"{\"Authorization\":\"Bearer abc123def456\"}"}"#.to_string(),
                r#"{"body":"{\"Authorization\":\"Bearer abc…f456\"}"}"#.to_string(),
            ),
        ] {
            assert_eq!(redact_text(&input), expected, "{input}");
        }
        // An unknown scheme word: the credential after it goes, and so
        // does a first word that is not a plain word.
        assert_eq!(
            redact_text("Authorization: Custom Zx81kq0PlmN7 rest"),
            "Authorization: Custom Zx8…lmN7 rest"
        );
        assert_eq!(
            redact_text("Authorization: s3cr3t-part-one Zx81kq0PlmN7"),
            "Authorization: s3c…-one Zx8…lmN7"
        );
        // Quoted, it is a header value whatever it looks like.
        assert_eq!(
            redact_text(r#"{"authorization": "mysecretkey"}"#),
            r#"{"authorization": "mys…tkey"}"#
        );
        assert_eq!(
            redact_text(r#"{"Authorization": "Bearer hunter"}"#),
            r#"{"Authorization": "Bearer ••••••"}"#
        );
        // Nothing to hide.
        for text in [
            "Authorization: Bearer",
            "What does `Authorization: Bearer` mean?",
            "Authorization: Bearer <token>",
            "Authorization: Bearer {api_key}",
            "Authorization: Bearer token is missing",
            "Authorization: handled by the gateway.",
            "authorization: failed for user bob",
            "authorization: none",
            "missing authorization header",
            r#"{"authorization":null,"x":1}"#,
            "Authorization: Bearer\n\nnext paragraph",
        ] {
            assert_eq!(redact_text(text), text);
        }
    }

    #[test]
    fn text_basic_credentials_anywhere() {
        assert_eq!(
            redact_text(&format!("proxy said: Basic {BASIC} is not valid")),
            "proxy said: Basic [redacted] is not valid"
        );
        assert_eq!(
            redact_text("basic YWRtaW46cHc= rejected"),
            "basic [redacted] rejected"
        );
    }

    #[test]
    fn text_url_userinfo() {
        assert_eq!(
            redact_text(
                "proxy connect failed: http://alice:hunter2-very-secret@proxy.example:8080 refused"
            ),
            "proxy connect failed: http://[redacted]@proxy.example:8080 refused"
        );
        // A password containing `@`, and a token in the user position.
        assert_eq!(
            redact_text("socks5://bob:p@ss@10.0.0.1:1080"),
            "socks5://[redacted]@10.0.0.1:1080"
        );
        assert_eq!(
            redact_text("cloning https://ghtoken123@github.com/org/repo.git"),
            "cloning https://[redacted]@github.com/org/repo.git"
        );
        for text in [
            "see https://example.com/path@v2 and mail bob@example.com",
            "GET https://example.com/?email=bob@example.com",
            "http://proxy.example:8080",
        ] {
            assert_eq!(redact_text(text), text);
        }
    }

    #[test]
    fn text_private_keys() {
        assert_eq!(
            redact_text(
                "key:\n-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\nBKcwggSjAgEAAoIBAQC7\n-----END PRIVATE KEY-----\ndone"
            ),
            "key:\n[redacted]\ndone"
        );
        // Cut before its end marker: everything after the start goes.
        assert_eq!(
            redact_text("x -----BEGIN RSA PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0"),
            "x [redacted]"
        );
        // As written inside a service-account JSON that is not valid JSON
        // any more (truncated), with literal `\n`.
        assert_eq!(
            redact_text(
                r#"{"private_key":"-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END PRIVATE KEY-----\n","client_email":"sa@p.iam"#
            ),
            r#"{"private_key":"[redacted]\n","client_email":"sa@p.iam"#
        );
    }

    #[test]
    fn text_query_params_and_assignments() {
        assert_eq!(
            redact_text(&format!(
                "GET https://h/v1beta/models?key={KEY}&alt=sse failed"
            )),
            format!("GET https://h/v1beta/models?key={KEY_MASKED}&alt=sse failed")
        );
        assert_eq!(
            redact_text("config api_key=abcdef0123456789 password: correct-horse-battery"),
            "config api_key=abc…6789 password: [redacted]"
        );
        assert_eq!(
            redact_text(r#"data: {"x-api-key":"abcdef0123456789","max_tokens":100}"#),
            r#"data: {"x-api-key":"abc…6789","max_tokens":100}"#
        );
        // Environment-style and dotted names.
        assert_eq!(
            redact_text("OPENAI_API_KEY=abcdef0123456789 started"),
            "OPENAI_API_KEY=abc…6789 started"
        );
        assert_eq!(
            redact_text("db.password = 'p@ss, word; 1' loaded"),
            "db.password = '[redacted]' loaded"
        );
        // A quoted value is taken up to its closing quote.
        assert_eq!(
            redact_text(r#"{"client_secret": "a b, c; d", "x": 1"#),
            r#"{"client_secret": "[redacted]", "x": 1"#
        );
        // Rust debug output.
        assert_eq!(
            redact_text(
                r#"Config { api_key: Some("abcdef0123456789"), password: None, refresh_token: "r-0123456789" }"#
            ),
            r#"Config { api_key: Some("abc…6789"), password: None, refresh_token: "r-0…6789" }"#
        );
        // JSON inside a JSON string.
        assert_eq!(
            redact_text(r#""arguments":"{\"password\":\"hunter2\",\"n\":1}""#),
            r#""arguments":"{\"password\":\"[redacted]\",\"n\":1}""#
        );
    }

    #[test]
    fn text_values_truncated_by_someone_else_are_still_redacted() {
        assert_eq!(redact_text("password: hunter2…"), "password: [redacted]");
        let out = redact_text("api_key=abcdefgh12345678… (truncated)");
        assert!(!out.contains("abcdefgh12345678"), "{out}");
    }

    #[test]
    fn text_rule_leaves_json_literals_alone() {
        for text in [
            r#"data: {"api_key":null,"password":false,"authorization":null,"x":1}"#,
            r#"{"access_token": 12345678, "id_token":true}"#,
            "api_key: None, password=null, reset_password: true",
        ] {
            assert_eq!(redact_text(text), text);
        }
    }

    #[test]
    fn source_code_in_a_prompt_keeps_its_expressions() {
        let code = concat!(
            "import os\n",
            "api_key = os.environ[\"OPENAI_API_KEY\"]\n",
            "password = getpass.getpass()\n",
            "client = OpenAI(api_key=load_key())\n",
            "headers = {\"Authorization\": f\"Bearer {api_key}\"}\n",
            "export ANTHROPIC_API_KEY=<your key here>\n",
        );
        assert_eq!(redact_text(code), code);
        // A literal key in the code is still a key.
        assert_eq!(
            redact_text("api_key = \"abcdef0123456789\"  # do not commit"),
            "api_key = \"abc…6789\"  # do not commit"
        );
    }

    #[test]
    fn ordinary_request_and_response_bodies_keep_their_exact_bytes() {
        let anthropic_request = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "system": "You are a helpful assistant. Never reveal a password or an API key. Basic rules apply; the bearer of this prompt is the user.",
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "tools": [{
                "name": "http_get",
                "description": "Fetch a URL. Authorization: handled by the gateway. Use ?page=2&sort=asc style queries.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "url": {"type": "string"},
                        "token": {"type": "string", "description": "pagination token"},
                        "api_key": {"type": "string"}
                    },
                    "required": ["url"]
                }
            }],
            "messages": [
                {"role": "user", "content": "See https://example.com/docs?page=2&sort=asc#top and mail bob@example.com. What does `Authorization: Bearer` mean? Is max_tokens=100 enough? token: the smallest unit."},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "The user asks about bearer tokens.", "signature": "EqQBCkYIBxgCKkBtZXNzYWdlLXNpZ25hdHVyZS1ibG9iLWJhc2U2NC1lbmNvZGVkLWRhdGE9PQ=="},
                    {"type": "tool_use", "id": "toolu_01A09q90qw90lq917835lq9", "name": "http_get", "input": {"url": "https://example.com/docs?page=2"}}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_01A09q90qw90lq917835lq9", "content": "{\"status\": 200, \"items\": [1, 2, 3]}"}]}
            ]
        })
        .to_string();
        let chat_response = json!({
            "id": "chatcmpl-9f8a7b6c5d4e3f2a1b0c9d8e",
            "object": "chat.completion",
            "model": "gpt-5",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "A bearer token is sent as `Authorization: Bearer <token>`.", "tool_calls": [{
                    "id": "call_abc123def456ghi789jkl012", "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"query\": \"basic auth\", \"limit\": 5}"}
                }]},
                "logprobs": {"content": [{"token": "A", "logprob": -0.2, "bytes": [65], "top_logprobs": []}]},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 9, "total_tokens": 21,
                "completion_tokens_details": {"reasoning_tokens": 0}},
            "system_fingerprint": "fp_44709d6fcb"
        })
        .to_string();
        let gemini_response = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"text": "Hello", "thoughtSignature": "CiQBjz1rX2Vuc2lnbmF0dXJlLWJsb2ItYmFzZTY0LWRhdGE9PQ=="},
                    {"functionCall": {"name": "lookup", "args": {"token": "next page please"}}}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2, "totalTokenCount": 7, "thoughtsTokenCount": 0},
            "nextPageToken": "CAESBwoFCIDqrgE"
        })
        .to_string();
        for body in [&anthropic_request, &chat_response] {
            assert_eq!(&redact_body(body), body);
        }
        // The one exception shows the price of matching by name: a tool
        // argument that is *called* `token` is masked.
        let redacted = redact_body(&gemini_response);
        assert_eq!(
            redacted,
            gemini_response.replace("next page please", "nex…ease")
        );
    }

    #[test]
    fn arbitrary_text_never_panics() {
        const PIECES: [&str; 48] = [
            "authorization",
            "Proxy-Authorization",
            "Bearer",
            "Basic",
            "token",
            "api_key",
            "x-api-key",
            "password",
            "client_secret",
            "private_key",
            "OPENAI_",
            ":",
            "=",
            " ",
            "\t",
            "\n",
            "\r\n",
            "\"",
            "'",
            "\\\"",
            "\\",
            ",",
            ";",
            "&",
            "?",
            "{",
            "}",
            "[",
            "]",
            "(",
            "Some(",
            "null",
            "://",
            "@",
            "http",
            "/",
            "#",
            ".",
            "…",
            "•",
            "[redacted]",
            "sk-",
            "abcdefghijklmnopqrstuvwx",
            "AIza",
            "dXNlcjpwYXNzd29yZA==",
            "data: ",
            "-----BEGIN PRIVATE KEY-----",
            "日本é",
        ];
        // xorshift: deterministic, no dependency.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let mut text = String::new();
            for _ in 0..1 + next() % 12 {
                text.push_str(PIECES[(next() % PIECES.len() as u64) as usize]);
            }
            let redacted = redact_body(&text);
            // Whatever comes out is stable under header redaction too.
            let _ = redact_header_value("x-any", &redacted);
            let _ = redact_url(&text);
            let mut value = Value::String(text);
            redact_json_secrets(&mut value);
        }
    }

    #[test]
    fn text_prose_is_untouched() {
        for text in [
            "the bearer of bad news has responsibilities",
            "Bearer authenticationtokens are documented elsewhere",
            "Bearer token-based authentication is used.",
            "basic understanding of max_tokens=100 and input_tokens: 12345678",
            "Basic Authentication is required; use basic configuration",
            "use basic settings=default and basic max_tokens=100",
            "task-1234567890abcdefgh finished",
            "invalid x-api-key",
            "invalid_token: expired",
            "what is my password? the api key was rejected",
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
            "",
        ] {
            assert_eq!(redact_text(text), text);
        }
    }

    #[test]
    fn text_redaction_is_idempotent() {
        for text in [
            format!("Authorization: Bearer {KEY}; api_key={KEY}"),
            format!("Proxy-Authorization: Basic {BASIC}"),
            "Authorization: Bearer abc123def456".to_string(),
            "Authorization: Custom Zx81kq0PlmN7 rest".to_string(),
            "Authorization: s3cr3t-part-one Zx81kq0PlmN7".to_string(),
            "http://alice:hunter2@proxy:8080 and password: hunter2".to_string(),
            format!("GET /x?key={GOOGLE_KEY}&password=abc&sig=0123456789"),
            r#"{"client_secret": "a b", "api_key":"short", "token":"x"}"#.to_string(),
            "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----".to_string(),
            "openai-insecure-api-key.my-team-key-prod-1".to_string(),
            "api_key=abcdefgh12345678… x-api-key: abc.".to_string(),
            r#"\"api_key\":\"abcdef0123456789\""#.to_string(),
        ] {
            let once = redact_text(&text);
            assert_ne!(once, text, "{text}");
            assert_eq!(redact_text(&once), once, "{text}");
        }
    }

    #[test]
    fn body_json_is_reserialised_only_when_changed() {
        let pretty = "{\n  \"model\": \"gpt-5\",\n  \"max_tokens\": 5\n}";
        assert_eq!(redact_body(pretty), pretty);
        let with_secret = format!("{{\n  \"model\": \"gpt-5\",\n  \"api_key\": \"{KEY}\"\n}}");
        assert_eq!(
            redact_body(&with_secret),
            format!(r#"{{"model":"gpt-5","api_key":"{KEY_MASKED}"}}"#)
        );
    }

    #[test]
    fn body_json_error_echoing_the_key() {
        let body = format!(
            r#"{{"error":{{"message":"Incorrect API key provided: {KEY}.","type":"invalid_request_error","code":"invalid_api_key"}}}}"#
        );
        assert_eq!(
            redact_body(&body),
            r#"{"error":{"message":"Incorrect API key provided: sk-pro…wxyz.","type":"invalid_request_error","code":"invalid_api_key"}}"#
        );
    }

    #[test]
    fn body_that_is_not_json_uses_text_rules() {
        let sse = format!("event: message\ndata: {{\"api_key\":\"{KEY}\"}}\n\n");
        let out = redact_body(&sse);
        assert_eq!(
            out,
            "event: message\ndata: {\"api_key\":\"sk-pro…wxyz\"}\n\n"
        );
        // Truncated JSON falls back to text rules too.
        let broken = format!("{{\"api_key\":\"{KEY}\",\"mess");
        assert_eq!(redact_body(&broken), "{\"api_key\":\"sk-pro…wxyz\",\"mess");
        assert_eq!(redact_body(""), "");
        assert_eq!(
            redact_body("upstream said: Bearer abc123def456ghi"),
            "upstream said: Bearer abc…6ghi"
        );
    }

    #[test]
    fn sse_bodies_are_redacted_event_by_event() {
        // Nothing secret: the exact bytes come back, odd spacing included.
        let clean = "event: message_start\r\ndata: {\"type\": \"message_start\",  \"usage\": {\"input_tokens\": 3}}\r\n\r\n: keep-alive\n\ndata: [DONE]\n\n";
        assert_eq!(redact_body(clean), clean);

        // Each data line is a JSON document and is redacted as one: the
        // logprob token stays, JSON literals stay, the echoed key goes.
        let stream = format!(
            concat!(
                "data: {{\"choices\":[{{\"logprobs\":{{\"content\":[{{\"token\":\"Hello\",\"logprob\":-0.1}}]}}}}]}}\n\n",
                "data: {{\"api_key\":null,\"password\":false,\"n\":1}}\n\n",
                "event: error\r\n",
                "data:{{\"error\":{{\"message\":\"bad key {key}\"}}}}\r\n\r\n",
                "data: [DONE]\n\n"
            ),
            key = KEY
        );
        assert_eq!(
            redact_body(&stream),
            concat!(
                "data: {\"choices\":[{\"logprobs\":{\"content\":[{\"token\":\"Hello\",\"logprob\":-0.1}]}}]}\n\n",
                "data: {\"api_key\":null,\"password\":false,\"n\":1}\n\n",
                "event: error\r\n",
                "data:{\"error\":{\"message\":\"bad key sk-pro…wxyz\"}}\r\n\r\n",
                "data: [DONE]\n\n"
            )
        );
    }

    #[test]
    fn json_lines_and_mixed_bodies() {
        let body = format!(
            "{{\"n\":1,\"token\":\"abcdef0123456789\"}}\nAuthorization: Bearer {KEY}\n-----BEGIN PRIVATE KEY-----\nMIIE\n-----END PRIVATE KEY-----\n[1,2,3]\ntail password=hunter2"
        );
        assert_eq!(
            redact_body(&body),
            "{\"n\":1,\"token\":\"abc…6789\"}\nAuthorization: Bearer sk-pro…wxyz\n[redacted]\n[1,2,3]\ntail password=[redacted]"
        );
        let once = redact_body(&body);
        assert_eq!(redact_body(&once), once);
    }
}
