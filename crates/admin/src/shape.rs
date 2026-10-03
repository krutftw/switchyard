//! What is wrong with the *shape* of a request body, said to an operator.
//!
//! Request bodies are read with serde, whose errors are written for
//! programmers: `invalid type: string "x", expected struct ModelConfig at
//! line 1 column 96`. [`describe`] is the one place that turns such an error
//! into what the admin API answers: the field at fault and a sentence that
//! says which shape is expected — without Rust type names, without parser
//! positions, and without the offending value (which may be a secret pasted
//! into the wrong field). Every endpoint goes through it.

use switchyard_core::config::ConfigIssue;

/// Objects the admin API takes, by the name serde knows them under, with an
/// example of the smallest useful one.
const OBJECTS: [(&str, &str); 6] = [
    ("ModelConfig", r#"{"id": "model-name"}"#),
    ("CredentialConfig", r#"{"api_key": "sk-…"}"#),
    (
        "AliasConfig",
        r#"{"name": "fast", "targets": ["model-name"]}"#,
    ),
    (
        "PriceConfig",
        r#"{"model": "gpt-*", "input": 1.25, "output": 10}"#,
    ),
    (
        "PayloadRule",
        r#"{"models": ["gpt-*"], "set": {"temperature": 0.2}}"#,
    ),
    (
        "ProviderConfig",
        r#"{"name": "my-provider", "kind": "openai"}"#,
    ),
];

/// Number types by serde's name for them, with the range they hold.
const NUMBERS: [(&str, &str); 12] = [
    ("u8", "a whole number from 0 to 255"),
    ("u16", "a whole number from 0 to 65535"),
    ("u32", "a whole number from 0 to 4294967295"),
    ("u64", "a whole number from 0 to 18446744073709551615"),
    ("usize", "a whole number from 0 to 18446744073709551615"),
    ("i8", "a whole number from -128 to 127"),
    ("i16", "a whole number from -32768 to 32767"),
    ("i32", "a whole number from -2147483648 to 2147483647"),
    (
        "i64",
        "a whole number from -9223372036854775808 to 9223372036854775807",
    ),
    (
        "isize",
        "a whole number from -9223372036854775808 to 9223372036854775807",
    ),
    ("f32", "a number"),
    ("f64", "a number"),
];

/// The error without serde_json's ` at line L column C`.
fn without_position(text: &str) -> &str {
    let Some((head, tail)) = text.rsplit_once(" at line ") else {
        return text;
    };
    let is_number = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    match tail.split_once(" column ") {
        Some((line, column)) if is_number(line) && is_number(column) => head,
        _ => text,
    }
}

/// What serde found, as a JSON user would call it. The value itself is
/// never repeated.
fn found(what: &str) -> &'static str {
    let what = what.trim();
    if what.starts_with("string") || what.starts_with("character") {
        "a string"
    } else if what.starts_with("integer") || what.starts_with("floating point") {
        "a number"
    } else if what.starts_with("boolean") {
        "true or false"
    } else if what.starts_with("null")
        || what.starts_with("unit value")
        || what.starts_with("Option value")
    {
        "null"
    } else if what.starts_with("sequence") {
        "a list"
    } else if what.starts_with("map") {
        "an object"
    } else {
        "something else"
    }
}

/// What serde expected, as a JSON user would call it.
fn expected(what: &str) -> String {
    let what = what.trim();
    if let Some(name) = what.strip_prefix("struct ") {
        let name = name.split_whitespace().next().unwrap_or_default();
        return match OBJECTS.iter().find(|(known, _)| *known == name) {
            Some((_, example)) => format!("an object such as {example}"),
            None => "an object".to_string(),
        };
    }
    if let Some(number) = what.strip_prefix("a nonzero ") {
        return match NUMBERS.iter().find(|(name, _)| *name == number) {
            Some((_, range)) => range.replacen("from 0 ", "from 1 ", 1),
            None => "a whole number other than 0".to_string(),
        };
    }
    if let Some((_, range)) = NUMBERS.iter().find(|(name, _)| *name == what) {
        return (*range).to_string();
    }
    match what {
        "a boolean" => "true or false".to_string(),
        "a string" | "string" | "a borrowed string" => "a string".to_string(),
        "a character" => "a single character".to_string(),
        "a sequence" | "an array" => "a list".to_string(),
        "a map" | "map" => "an object".to_string(),
        "option" => "a value or null".to_string(),
        "unit" => "null".to_string(),
        _ if what.starts_with("enum ")
            || what.starts_with("variant ")
            || what.starts_with("field ") =>
        {
            "one of the accepted names".to_string()
        }
        _ if what.starts_with("a tuple") || what.starts_with("tuple ") => "a list".to_string(),
        // An expectation written for people (this crate's own visitors).
        _ => what.to_string(),
    }
}

/// The name between the first pair of backticks.
fn quoted(text: &str) -> &str {
    text.split('`').nth(1).unwrap_or_default()
}

/// `parent.field`, or `field` at the top of the body.
fn join(parent: &str, field: &str) -> String {
    if parent.is_empty() {
        field.to_string()
    } else {
        format!("{parent}.{field}")
    }
}

/// Turns a serde error about the shape of a request body into an issue: the
/// field at fault (empty when it is the body as a whole) and what is
/// expected of it.
///
/// `path` is where serde was when it failed, as `serde_path_to_error`
/// reports it (`models[0]`, `routing.cooldown.auth_secs`; `.` or nothing
/// for the top of the body); `message` is the error's own text.
///
/// | serde says | the issue says |
/// |---|---|
/// | `invalid type: string "x", expected struct ModelConfig` | `expected an object such as {"id": "model-name"}, got a string` |
/// | `invalid value: integer `-5`, expected u32` | `must be a whole number from 0 to 4294967295` |
/// | `unknown field `x`, expected one of …` | `unknown field `x`` |
/// | `missing field `name`` | path `name`: `is required` |
/// | `unknown variant `x`, expected one of `a`, `b`` | `must be one of `a`, `b`` |
pub(crate) fn describe(path: &str, message: &str) -> ConfigIssue {
    let text = without_position(message.trim());
    let mut path = match path.trim() {
        "." => String::new(),
        path => path.to_string(),
    };
    // The expectation is serde's own and comes last; what was found may
    // quote text from the request, so it is split off from the right.
    let split = |rest: &str| -> Option<(String, String)> {
        rest.rsplit_once(", expected ")
            .map(|(got, wanted)| (got.to_string(), expected(wanted)))
    };
    let message = if let Some(rest) = text.strip_prefix("invalid type: ") {
        match split(rest) {
            Some((got, wanted)) => format!("expected {wanted}, got {}", found(&got)),
            None => "has the wrong type".to_string(),
        }
    } else if let Some(rest) = text.strip_prefix("invalid value: ") {
        match split(rest) {
            Some((_, wanted)) => format!("must be {wanted}"),
            None => "is not an accepted value".to_string(),
        }
    } else if let Some(rest) = text.strip_prefix("invalid length ") {
        match split(rest) {
            Some((_, wanted)) => format!("expected {wanted}"),
            None => "has the wrong number of entries".to_string(),
        }
    } else if text.starts_with("unknown field `") {
        format!("unknown field `{}`", quoted(text))
    } else if text.starts_with("missing field `") {
        path = join(&path, quoted(text));
        "is required".to_string()
    } else if text.starts_with("duplicate field `") {
        path = join(&path, quoted(text));
        "is given twice".to_string()
    } else if let Some(rest) = text.strip_prefix("unknown variant `") {
        // "x`, expected one of `a`, `b`" or "x`, expected `a` or `b`".
        match rest.rsplit_once("`, expected ") {
            Some((_, accepted)) => format!("must be {accepted}"),
            None => "is not an accepted value".to_string(),
        }
    } else if text.starts_with("data did not match any variant") {
        "has a shape that is not accepted here".to_string()
    } else {
        text.to_string()
    };
    ConfigIssue { path, message }
}

/// Why a body is not JSON at all, with the place written the way the rest
/// of the API writes places (`line L, column C`).
pub(crate) fn syntax(error: &serde_json::Error) -> String {
    let what = syntax_reason(error);
    if error.line() == 0 {
        return what;
    }
    format!("line {}, column {}: {what}", error.line(), error.column())
}

/// Why a body is not JSON, without the place.
pub(crate) fn syntax_reason(error: &serde_json::Error) -> String {
    without_position(&error.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde::Deserialize;
    use serde::de::DeserializeOwned;
    use serde_json::json;
    use switchyard_core::config::{AliasConfig, PayloadConfig, PriceConfig, ProviderConfig};

    /// The issue for `body` read as `T`, through the same path-tracking
    /// deserialiser the routes use.
    fn issue<T: DeserializeOwned>(body: serde_json::Value) -> (String, String) {
        let text = body.to_string();
        let mut deserializer = serde_json::Deserializer::from_str(&text);
        let error = match serde_path_to_error::deserialize::<_, T>(&mut deserializer) {
            Ok(_) => panic!("{text} was accepted"),
            Err(error) => error,
        };
        let path = error.path().to_string();
        let issue = describe(&path, &error.into_inner().to_string());
        (issue.path, issue.message)
    }

    fn owned(path: &str, message: &str) -> (String, String) {
        (path.to_string(), message.to_string())
    }

    #[test]
    fn wrong_types_say_which_shape_is_expected() {
        // The reviewers' case: model names instead of model objects.
        assert_eq!(
            issue::<ProviderConfig>(json!({"name": "p", "kind": "mock", "models": ["dead-model"]})),
            owned(
                "models[0]",
                r#"expected an object such as {"id": "model-name"}, got a string"#
            )
        );
        assert_eq!(
            issue::<ProviderConfig>(json!({"name": "p", "kind": "mock", "credentials": ["sk-x"]})),
            owned(
                "credentials[0]",
                r#"expected an object such as {"api_key": "sk-…"}, got a string"#
            )
        );
        assert_eq!(
            issue::<ProviderConfig>(json!({"name": "p", "kind": "mock", "enabled": "yes"})),
            owned("enabled", "expected true or false, got a string")
        );
        assert_eq!(
            issue::<ProviderConfig>(json!({"name": "p", "kind": "mock", "api_keys": "sk-x"})),
            owned("api_keys", "expected a list, got a string")
        );
        assert_eq!(
            issue::<ProviderConfig>(json!({"name": 7, "kind": "mock"})),
            owned("name", "expected a string, got a number")
        );
        assert_eq!(
            issue::<Vec<AliasConfig>>(json!({"name": "a", "targets": ["b"]})),
            owned("", "expected a list, got an object")
        );
        assert_eq!(
            issue::<Vec<AliasConfig>>(json!(["fast"])),
            owned(
                "[0]",
                r#"expected an object such as {"name": "fast", "targets": ["model-name"]}, got a string"#
            )
        );
        assert_eq!(
            issue::<Vec<PriceConfig>>(json!([{"model": "a", "input": "cheap", "output": 1}])),
            owned("[0].input", "expected a number, got a string")
        );
        assert_eq!(
            issue::<PayloadConfig>(json!({"default": [null]})),
            owned(
                "default[0]",
                r#"expected an object such as {"models": ["gpt-*"], "set": {"temperature": 0.2}}, got null"#
            )
        );
    }

    #[test]
    fn numbers_out_of_range_say_the_range() {
        #[derive(Debug, Deserialize)]
        struct Numbers {
            #[serde(default)]
            small: u8,
            #[serde(default)]
            port: u16,
            #[serde(default)]
            count: u32,
            #[serde(default)]
            secs: u64,
            #[serde(default)]
            priority: i32,
            #[serde(default)]
            positive: Option<std::num::NonZeroU32>,
        }
        let _ = |n: Numbers| (n.small, n.port, n.count, n.secs, n.priority, n.positive);
        assert_eq!(
            issue::<Numbers>(json!({"count": -5})),
            owned("count", "must be a whole number from 0 to 4294967295")
        );
        assert_eq!(
            issue::<Numbers>(json!({"count": 5_000_000_000_u64})),
            owned("count", "must be a whole number from 0 to 4294967295")
        );
        assert_eq!(
            issue::<Numbers>(json!({"count": 1.5})),
            owned(
                "count",
                "expected a whole number from 0 to 4294967295, got a number"
            )
        );
        assert_eq!(
            issue::<Numbers>(json!({"port": 70_000})),
            owned("port", "must be a whole number from 0 to 65535")
        );
        assert_eq!(
            issue::<Numbers>(json!({"small": 300})),
            owned("small", "must be a whole number from 0 to 255")
        );
        assert_eq!(
            issue::<Numbers>(json!({"secs": 1e20})),
            owned(
                "secs",
                "expected a whole number from 0 to 18446744073709551615, got a number"
            )
        );
        assert_eq!(
            issue::<Numbers>(json!({"priority": 3_000_000_000_u64})),
            owned(
                "priority",
                "must be a whole number from -2147483648 to 2147483647"
            )
        );
        assert_eq!(
            issue::<Numbers>(json!({"positive": 0})),
            owned("positive", "must be a whole number from 1 to 4294967295")
        );
    }

    #[test]
    fn fields_and_values_that_do_not_exist_are_named() {
        assert_eq!(
            issue::<ProviderConfig>(json!({"name": "p", "kind": "mock", "websocket": true})),
            owned("websocket", "unknown field `websocket`")
        );
        assert_eq!(
            issue::<ProviderConfig>(json!({"kind": "mock"})),
            owned("name", "is required")
        );
        assert_eq!(
            issue::<Vec<AliasConfig>>(json!([{"name": "a", "targets": ["b"]}, {"targets": ["b"]}])),
            owned("[1].name", "is required")
        );
        let (path, message) = issue::<ProviderConfig>(json!({"name": "p", "kind": "closedai"}));
        assert_eq!(path, "kind");
        assert!(
            message.starts_with("must be one of `openai`, `anthropic`"),
            "{message}"
        );
        // The value that was sent is not repeated.
        assert!(!message.contains("closedai"), "{message}");
    }

    #[test]
    fn nothing_of_serde_or_of_the_request_shows() {
        let secret = "sk-live-0123456789abcdefghij";
        for body in [
            json!({"name": "p", "kind": "mock", "models": [secret]}),
            json!({"name": "p", "kind": "mock", "priority": secret}),
            json!({"name": "p", "kind": secret}),
            json!({"name": "p", "kind": "mock", "headers": [secret]}),
            json!({"name": "p", "kind": "mock", "legacy_max_tokens": secret}),
            json!({"name": "p", "kind": "mock", "credentials": [{"weight": secret}]}),
            json!({"name": "p", "kind": "mock", "models": [{"id": "m", "thinking": secret}]}),
        ] {
            let (path, message) = issue::<ProviderConfig>(body.clone());
            for leak in [
                secret,
                "struct",
                "Config",
                "u32",
                "i32",
                "u64",
                "at line",
                "column",
                "invalid type",
                "sequence",
                "Option",
            ] {
                assert!(
                    !message.contains(leak),
                    "{body}: `{leak}` in {path}: {message}"
                );
            }
        }
    }

    #[test]
    fn positions_are_cut_and_other_texts_kept() {
        assert_eq!(
            without_position("expected value at line 1 column 7"),
            "expected value"
        );
        assert_eq!(
            without_position("met at line 4 of the song"),
            "met at line 4 of the song"
        );
        // A message written for people passes through, without the position.
        assert_eq!(
            describe("key", "must not contain spaces at line 1 column 20"),
            ConfigIssue {
                path: "key".into(),
                message: "must not contain spaces".into()
            }
        );
        // The top of the body has no path.
        assert_eq!(
            describe(".", "invalid type: map, expected a sequence").path,
            ""
        );

        let error = serde_json::from_str::<serde_json::Value>("{\"a\": ").unwrap_err();
        let text = syntax(&error);
        assert!(text.starts_with("line 1, column "), "{text}");
        assert!(!text.contains(" at line "), "{text}");
    }
}
