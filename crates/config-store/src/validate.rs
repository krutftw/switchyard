//! Parsing and validating configuration text.

use switchyard_core::Config;
use switchyard_core::config::ConfigIssue;
use switchyard_core::util::mask_secret;

/// Path used for issues that concern the file as a whole.
pub(crate) const FILE_PATH: &str = "config";

/// Parses and validates configuration text.
///
/// A syntax or schema error becomes a single issue whose `path` is
/// `line L, column C` (1-based) when the parser reports a position, and
/// `config` otherwise. A file that parses is checked with
/// [`Config::validate`] and every semantic issue is returned.
///
/// Issue messages are logged and shown in the dashboard, so text taken from
/// the file is kept out of them: a key pasted into the wrong place must not
/// end up in a log. The position says where to look instead. Concretely:
///
/// * string values are always masked;
/// * numbers are masked when longer than five characters (`port = 70000` stays
///   readable, a numeric key written without quotes does not);
/// * unknown field names and enum values are masked when longer than twenty
///   characters. Shorter ones are shown, because that is how a typo
///   (`prot`, `round-robbin`) is recognised.
///
/// They are also read by operators, not programmers, so what a setting
/// expects is said as a TOML shape — `a table such as { id = "model-name"
/// }`, `a whole number from 0 to 65535`, `an array` — and never as the name
/// of one of this program's types.
pub fn validate_text(text: &str) -> Result<Config, Vec<ConfigIssue>> {
    let text = strip_bom(text);
    let config: Config = match toml::from_str(text) {
        Ok(config) => config,
        Err(err) => {
            let path = match err.span() {
                Some(span) => {
                    let (line, column) = line_column(text, span.start);
                    format!("line {line}, column {column}")
                }
                None => FILE_PATH.to_string(),
            };
            return Err(vec![ConfigIssue {
                path,
                message: reword(&redact(err.message())),
            }]);
        }
    };
    let issues = config.validate();
    if issues.is_empty() {
        Ok(config)
    } else {
        Err(issues)
    }
}

/// Removes a leading UTF-8 byte-order mark, which some Windows editors add.
pub(crate) fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// 1-based line and column (in characters) of a byte offset.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let column = before[line_start..].chars().count() + 1;
    (line, column)
}

/// Longest unknown field name or enum value shown as written.
const LONGEST_NAME_SHOWN: usize = 20;
/// Longest number shown as written.
const LONGEST_NUMBER_SHOWN: usize = 5;

/// Hides text of the file that a parser message quotes.
///
/// The messages come from serde and have the shape `<what was found>,
/// expected <what the schema wants>`. Only the first half can contain text
/// from the file: `invalid type: string "…"`, ``integer `…` ``,
/// ``floating point `…` ``, ``unknown field `…` ``, ``unknown variant `…` ``.
/// The second half lists names from the schema and is left alone.
fn redact(message: &str) -> String {
    // The TOML reader's own wording for unknown keys of an enum variant
    // written as a table. Nothing in the schema is such a variant today; this
    // keeps the message safe should one be added.
    const UNEXPECTED: &str = "unexpected keys in table: ";
    const AVAILABLE: &str = ", available keys: ";
    if let Some(rest) = message.strip_prefix(UNEXPECTED)
        && let Some(split) = rest.rfind(AVAILABLE)
    {
        let (keys, available) = rest.split_at(split);
        return format!("{UNEXPECTED}{}{available}", shown(keys, LONGEST_NAME_SHOWN));
    }

    // The quoted text may itself contain the separator; the real one is the
    // last, because nothing after it comes from the file.
    let split = [", expected ", ", there are no "]
        .iter()
        .filter_map(|separator| message.rfind(separator))
        .max()
        .unwrap_or(message.len());
    let (found, expected) = message.split_at(split);

    // A field name or enum value may contain any character, a backtick
    // included, so it is taken as everything between the outermost backticks.
    for marker in ["unknown field `", "unknown variant `"] {
        if let Some(name) = found
            .strip_prefix(marker)
            .and_then(|rest| rest.strip_suffix('`'))
        {
            return format!("{marker}{}`{expected}", shown(name, LONGEST_NAME_SHOWN));
        }
    }

    const STRING: &str = "string \"";
    let mut out = String::with_capacity(message.len());
    let mut rest = found;
    loop {
        let string_at = rest.find(STRING);
        let tick_at = rest.find('`');
        match (string_at, tick_at) {
            (Some(start), tick) if tick.is_none_or(|t| start < t) => {
                let content_start = start + STRING.len();
                out.push_str(&rest[..content_start]);
                let tail = &rest[content_start..];
                // The value is Debug-escaped, so an unescaped quote ends it.
                let mut end = None;
                let mut escaped = false;
                for (i, c) in tail.char_indices() {
                    match c {
                        '\\' if !escaped => escaped = true,
                        '"' if !escaped => {
                            end = Some(i);
                            break;
                        }
                        _ => escaped = false,
                    }
                }
                match end {
                    Some(end) => {
                        out.push_str(&mask_secret(&tail[..end]));
                        rest = &tail[end..];
                    }
                    None => {
                        // Unterminated: hide everything that follows.
                        out.push('…');
                        rest = "";
                    }
                }
            }
            (_, Some(start)) => {
                let content_start = start + 1;
                let before = &rest[..content_start];
                out.push_str(before);
                let tail = &rest[content_start..];
                let numeric = before.ends_with("integer `") || before.ends_with("floating point `");
                match tail.find('`') {
                    Some(end) => {
                        let limit = if numeric {
                            LONGEST_NUMBER_SHOWN
                        } else {
                            LONGEST_NAME_SHOWN
                        };
                        out.push_str(&shown(&tail[..end], limit));
                        out.push('`');
                        rest = &tail[end + 1..];
                    }
                    None => {
                        out.push('…');
                        rest = "";
                    }
                }
            }
            _ => break,
        }
    }
    out.push_str(rest);
    out.push_str(expected);
    out
}

/// Tables of the configuration that are written as entries of a list, by
/// the name the parser knows them under, each with the smallest useful one
/// in TOML. These are the ones an operator gets wrong by writing a name
/// where a table goes (`models = ["gpt-5"]`).
const TABLES: [(&str, &str); 7] = [
    ("ModelConfig", r#"{ id = "model-name" }"#),
    ("CredentialConfig", r#"{ api_key = "sk-…" }"#),
    ("ClientKey", r#"{ key = "sy-…", name = "laptop" }"#),
    (
        "AliasConfig",
        r#"{ name = "fast", targets = ["model-name"] }"#,
    ),
    (
        "PriceConfig",
        r#"{ model = "gpt-*", input = 1.25, output = 10 }"#,
    ),
    (
        "PayloadRule",
        r#"{ models = ["gpt-*"], set = { temperature = 0.2 } }"#,
    ),
    (
        "ProviderConfig",
        r#"{ name = "my-provider", kind = "openai" }"#,
    ),
];

/// Number types by the parser's name for them, with the range they hold.
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

/// What the schema wants, in the words of someone who edits a TOML file
/// rather than the parser's (`struct ModelConfig`, `u16`, `a sequence`).
/// `None` for an expectation that is already written for people: a list of
/// field names or accepted values, or a sentence of the schema's own.
fn expectation(wanted: &str) -> Option<String> {
    let wanted = wanted.trim();
    if let Some(name) = wanted.strip_prefix("struct ") {
        let name = name.split_whitespace().next().unwrap_or_default();
        return Some(match TABLES.iter().find(|(known, _)| *known == name) {
            Some((_, example)) => format!("a table such as {example}"),
            None => "a table".to_string(),
        });
    }
    if let Some(number) = wanted.strip_prefix("a nonzero ") {
        return Some(match NUMBERS.iter().find(|(name, _)| *name == number) {
            Some((_, range)) => range.replacen("from 0 ", "from 1 ", 1),
            None => "a whole number other than 0".to_string(),
        });
    }
    if let Some((_, range)) = NUMBERS.iter().find(|(name, _)| *name == wanted) {
        return Some((*range).to_string());
    }
    let plain = match wanted {
        "a boolean" => "true or false",
        "a string" | "string" | "a borrowed string" | "string only" => "a string",
        "a character" => "a single character",
        "a sequence" | "an array" => "an array",
        "a map" | "map" => "a table",
        "option" => "a value",
        _ if wanted.starts_with("enum ")
            || wanted.starts_with("variant ")
            || wanted.starts_with("field ") =>
        {
            "one of the accepted names"
        }
        _ if wanted.starts_with("a tuple") || wanted.starts_with("tuple ") => "an array",
        _ => return None,
    };
    Some(plain.to_string())
}

/// Puts a parser message — values already hidden by [`redact`] — into words
/// an operator can act on: what the schema expects is said as a TOML shape
/// with its range or an example, never as the name of a type of this
/// program, and what was found is called what TOML calls it.
///
/// | the parser says | the issue says |
/// |---|---|
/// | `invalid type: string "•", expected struct ModelConfig` | `invalid type: string "•", expected a table such as { id = "model-name" }` |
/// | ``invalid value: integer `99999`, expected u16`` | ``invalid value: integer `99999`, expected a whole number from 0 to 65535`` |
/// | `invalid type: string "•••", expected a sequence` | `invalid type: string "•••", expected an array` |
/// | `invalid type: map, expected a string` | `invalid type: table, expected a string` |
///
/// Everything else — unknown fields and values with the list of accepted
/// ones, missing fields, syntax errors — is left as it is.
fn reword(message: &str) -> String {
    const EXPECTED: &str = ", expected ";
    // The TOML reader's own words for a number, a list or `true` where one
    // of a fixed set of names goes (`kind = 5`). Every such setting of the
    // schema takes the name as a string; none takes a table.
    if message == "wanted string or table" {
        return "expected a string holding one of the accepted names".to_string();
    }
    // The real separator is the last one: what follows it is the schema's.
    let Some(split) = message.rfind(EXPECTED) else {
        return message.to_string();
    };
    let (found, wanted) = (&message[..split], &message[split + EXPECTED.len()..]);
    let Some(wanted) = expectation(wanted) else {
        return message.to_string();
    };
    // serde's names for the two container shapes, when one was found where
    // something else goes.
    let found = match found {
        "invalid type: map" => "invalid type: table",
        "invalid type: sequence" => "invalid type: array",
        other => other,
    };
    format!("{found}{EXPECTED}{wanted}")
}

/// A quoted piece of the file as it may appear in a message: as written when
/// short, masked otherwise.
fn shown(value: &str, longest: usize) -> String {
    if value.chars().count() <= longest {
        value.to_string()
    } else {
        mask_secret(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn valid_text_parses() {
        let config = validate_text("[server]\nport = 9000\n").unwrap();
        assert_eq!(config.server.port, 9000);
        assert_eq!(validate_text("").unwrap(), Config::default());
    }

    #[test]
    fn byte_order_mark_is_tolerated() {
        let config = validate_text("\u{feff}[server]\nport = 9001\n").unwrap();
        assert_eq!(config.server.port, 9001);
    }

    #[test]
    fn syntax_error_reports_line_and_column() {
        let issues = validate_text("[server]\nport = \n").unwrap_err();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "line 2, column 8");
        assert!(!issues[0].message.is_empty());
        // Only the message, not toml's multi-line rendering with the source
        // line (which could carry a secret).
        assert!(!issues[0].message.contains('\n'));
    }

    #[test]
    fn schema_error_reports_position_of_the_key() {
        let issues = validate_text("[server]\nhost = \"x\"\nprot = 1\n").unwrap_err();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "line 3, column 1");
        assert!(issues[0].message.contains("prot"), "{}", issues[0].message);
    }

    #[test]
    fn semantic_issues_are_all_reported() {
        let text = r#"
[server]
port = 0

[[providers]]
name = "Bad Name"
kind = "openai-compat"

[[aliases]]
name = "loop"
targets = ["loop"]
"#;
        let issues = validate_text(text).unwrap_err();
        let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
        assert!(paths.contains(&"server.port"), "{paths:?}");
        assert!(paths.contains(&"providers[0].name"), "{paths:?}");
        assert!(paths.contains(&"providers[0].base_url"), "{paths:?}");
        // The offending target itself, not the list.
        assert!(paths.contains(&"aliases[0].targets[0]"), "{paths:?}");
    }

    #[test]
    fn type_mismatch_does_not_leak_the_value() {
        let text = "[[providers]]\nname = \"a\"\nkind = \"openai\"\napi_keys = \"sk-live-0123456789abcdefghij\"\n";
        let issues = validate_text(text).unwrap_err();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "line 4, column 12");
        assert!(
            !issues[0].message.contains("0123456789"),
            "{}",
            issues[0].message
        );
        assert!(issues[0].message.contains("string"));
    }

    #[test]
    fn redaction_handles_escapes_and_several_values() {
        assert_eq!(
            redact(r#"invalid type: string "a\"b-very-long-secret-value", expected u16"#),
            format!(
                r#"invalid type: string "{}", expected u16"#,
                mask_secret(r#"a\"b-very-long-secret-value"#)
            )
        );
        assert_eq!(
            redact(r#"string "abc" and string "def""#),
            r#"string "•••" and string "•••""#
        );
        assert_eq!(redact(r#"string "unterminated"#), r#"string "…"#);
        assert_eq!(redact("nothing quoted"), "nothing quoted");
        // Backticks inside a string value do not confuse the scan.
        assert_eq!(
            redact(r#"invalid type: string "a`b`c", expected u16"#),
            r#"invalid type: string "•••••", expected u16"#
        );
        assert_eq!(redact("integer `12345678"), "integer `…");
    }

    #[test]
    fn numbers_are_shown_only_when_short() {
        assert_eq!(
            redact("invalid value: integer `70000`, expected u16"),
            "invalid value: integer `70000`, expected u16"
        );
        assert_eq!(
            redact("invalid value: integer `-1`, expected u16"),
            "invalid value: integer `-1`, expected u16"
        );
        assert_eq!(
            redact("invalid type: integer `123456`, expected a string"),
            "invalid type: integer `••••••`, expected a string"
        );
        assert_eq!(
            redact("invalid type: integer `8675309112233445`, expected a string"),
            format!(
                "invalid type: integer `{}`, expected a string",
                mask_secret("8675309112233445")
            )
        );
        assert_eq!(
            redact(
                "invalid type: integer `12345678901234567890123456789012345678` as i128, expected a string"
            ),
            format!(
                "invalid type: integer `{}` as i128, expected a string",
                mask_secret("12345678901234567890123456789012345678")
            )
        );
        assert_eq!(
            redact("invalid type: floating point `31415926.53589793`, expected a string"),
            format!(
                "invalid type: floating point `{}`, expected a string",
                mask_secret("31415926.53589793")
            )
        );
        assert_eq!(
            redact("invalid type: boolean `true`, expected a string"),
            "invalid type: boolean `true`, expected a string"
        );
    }

    #[test]
    fn unknown_names_are_shown_only_when_short() {
        // The names the schema expects are never touched, however long.
        let expected = ", expected one of `session_affinity_ttl_secs`, `request_log_max_body_kb`";
        assert_eq!(
            redact(&format!("unknown field `prot`{expected}")),
            format!("unknown field `prot`{expected}")
        );
        let secret = "sy-live-0123456789abcdefghijklmnop";
        assert_eq!(
            redact(&format!("unknown field `{secret}`{expected}")),
            format!("unknown field `{}`{expected}", mask_secret(secret))
        );
        assert_eq!(
            redact(&format!("unknown field `{secret}`, there are no fields")),
            format!(
                "unknown field `{}`, there are no fields",
                mask_secret(secret)
            )
        );
        let message = format!("unexpected keys in table: {secret}, available keys: a, b");
        assert_eq!(
            redact(&message),
            format!(
                "unexpected keys in table: {}, available keys: a, b",
                mask_secret(secret)
            )
        );
        assert_eq!(
            redact("unexpected keys in table: x, y, available keys: a, b"),
            "unexpected keys in table: x, y, available keys: a, b"
        );
        // A name with backticks, or with the separator itself, is still one
        // name.
        let tricky = "sy `tick`, expected one of 0123456789abcdefghijklmnop";
        let message = redact(&format!("unknown variant `{tricky}`, expected `a` or `b`"));
        assert_eq!(
            message,
            format!(
                "unknown variant `{}`, expected `a` or `b`",
                mask_secret(tricky)
            )
        );
    }

    #[test]
    fn a_key_pasted_where_an_enum_goes_does_not_leak() {
        let secret = "sk-live-0123456789abcdefghijklmnop";
        let text = format!("[[providers]]\nname = \"a\"\nkind = \"{secret}\"\n");
        let issues = validate_text(&text).unwrap_err();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "line 3, column 8");
        assert!(!issues[0].message.contains(secret), "{}", issues[0].message);
        assert!(issues[0].message.contains("unknown variant"));

        // A typo stays readable.
        let issues = validate_text("[routing]\nstrategy = \"round-robbin\"\n").unwrap_err();
        assert!(
            issues[0].message.contains("round-robbin"),
            "{}",
            issues[0].message
        );
        assert!(issues[0].message.contains("round-robin"));

        assert_eq!(
            redact("unknown variant `abc`, expected `x`"),
            "unknown variant `abc`, expected `x`"
        );
        assert_eq!(redact("unknown variant `open"), "unknown variant `…");
    }

    /// Files with one thing of the wrong shape each, across every kind of
    /// value the schema has.
    const WRONG_SHAPES: [&str; 31] = [
        "[[providers]]\nname = \"a\"\nkind = [\"openai\"]\n",
        "[[providers]]\nname = \"a\"\nkind = { openai = 1 }\n",
        "[[providers]]\nname = \"a\"\nkind = true\n",
        "[routing]\nstrategy = 1\n",
        "[routing.cooldown]\nenabled = 1\n",
        "[[providers]]\nname = \"a\"\nkind = \"openai\"\n[[providers.models]]\nid = 5\n",
        "[[providers]]\nname = \"a\"\nkind = \"openai\"\n[[providers.models]]\nid = \"m\"\nthinking = \"yes\"\n",
        "[[providers]]\nname = \"a\"\nkind = \"openai\"\n[[providers.credentials]]\nweight = \"heavy\"\n",
        "[[pricing]]\nmodel = \"m\"\ninput = \"cheap\"\noutput = 1\n",
        "[[providers]]\nname = \"a\"\nkind = \"openai\"\nmodels = [\"x\"]\n",
        "[[providers]]\nname = \"a\"\nkind = \"openai\"\ncredentials = [\"sk-x\"]\n",
        "[[providers]]\nname = \"a\"\nkind = \"openai\"\napi_keys = \"sk-live-0123456789abcdefghij\"\n",
        "[[providers]]\nname = \"a\"\nkind = 5\n",
        "[[providers]]\nname = \"a\"\nkind = \"openai\"\nenabled = \"yes\"\n",
        "[[providers]]\nname = \"a\"\nkind = \"openai\"\npriority = 3000000000\n",
        "[[providers]]\nname = \"a\"\nkind = \"openai\"\npriority = 1.5\n",
        "[[providers]]\nname = \"a\"\nkind = \"openai\"\nheaders = [\"x\"]\n",
        "[[providers]]\nname = \"a\"\nkind = \"openai\"\nname2 = { a = 1 }\n",
        "[[providers]]\nname = { a = 1 }\nkind = \"openai\"\n",
        "[[providers]]\nname = [\"a\"]\nkind = \"openai\"\n",
        "providers = \"none\"\n",
        "[providers]\nname = \"a\"\n",
        "aliases = [\"fast\"]\n",
        "pricing = [1]\n",
        "[server]\nport = 99999\n",
        "[server]\nport = -1\n",
        "[server]\nport = \"8317\"\n",
        "server = 7\n",
        "[auth]\nkeys = [\"sy-key\"]\n",
        "[[auth.keys]]\nkey = \"k\"\nrate_limit_rpm = \"fast\"\n",
        "[payload]\noverride = [\"x\"]\n",
    ];

    #[test]
    fn schema_errors_are_worded_for_an_operator() {
        // Names of this program's types, and the parser's own vocabulary.
        const NOT_FOR_OPERATORS: [&str; 16] = [
            "struct",
            "Config",
            "ClientKey",
            "PayloadRule",
            "u8",
            "u16",
            "u32",
            "u64",
            "i32",
            "i64",
            "f64",
            "usize",
            "sequence",
            "map",
            "enum",
            "Option",
        ];
        for text in WRONG_SHAPES {
            let issues = validate_text(text).expect_err(text);
            assert_eq!(issues.len(), 1, "{text}");
            let message = &issues[0].message;
            assert!(issues[0].path.starts_with("line "), "{text}: {issues:?}");
            assert!(!message.contains("wanted"), "{text:?}: {message}");
            for word in NOT_FOR_OPERATORS {
                let found = message
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .any(|token| token == word || (word == "Config" && token.ends_with(word)));
                assert!(!found, "`{word}` in the message for {text:?}: {message}");
            }
        }

        // The cases the reviewers of the admin API ran into.
        let message = |text: &str| {
            let issue = validate_text(text).expect_err(text).remove(0);
            format!("{}: {}", issue.path, issue.message)
        };
        assert_eq!(
            message("[[providers]]\nname=\"a\"\nkind=\"openai\"\nmodels=[\"x\"]\n"),
            "line 4, column 9: invalid type: string \"•\", expected a table such as \
             { id = \"model-name\" }"
        );
        assert_eq!(
            message("[server]\nport = 99999\n"),
            "line 2, column 8: invalid value: integer `99999`, expected a whole number from 0 \
             to 65535"
        );
        assert_eq!(
            message(
                "[[providers]]\nname = \"a\"\nkind = \"openai\"\napi_keys = \"sk-proj-0123456789abcdef\"\n"
            ),
            format!(
                "line 4, column 12: invalid type: string \"{}\", expected an array",
                mask_secret("sk-proj-0123456789abcdef")
            )
        );
        assert_eq!(
            message("[[providers]]\nname = \"a\"\nkind = 5\n"),
            "line 3, column 8: expected a string holding one of the accepted names"
        );
        assert_eq!(
            message("[[auth.keys]]\nkey = \"k\"\nrate_limit_rpm = \"fast\"\n"),
            "line 3, column 18: invalid type: string \"••••\", expected a whole number from 0 \
             to 4294967295"
        );
    }

    #[test]
    fn rewording_touches_only_the_expectation() {
        // What the schema lists for people stays as it is.
        for kept in [
            "unknown field `prot`, expected one of `host`, `port`",
            "unknown variant `round-robbin`, expected `round-robin` or `fill-first`",
            "missing field `name`",
            "duplicate key",
            "invalid type: integer `5`, expected a string",
            "invalid value: string \"•••\", expected a proxy URL or `direct`",
        ] {
            assert_eq!(reword(kept), kept);
        }
        assert_eq!(
            reword("invalid type: map, expected a sequence"),
            "invalid type: table, expected an array"
        );
        assert_eq!(
            reword("invalid type: sequence, expected struct ServerConfig"),
            "invalid type: array, expected a table"
        );
        assert_eq!(
            reword("invalid value: integer `0`, expected a nonzero u32"),
            "invalid value: integer `0`, expected a whole number from 1 to 4294967295"
        );
        // A masked value that happens to hold the separator: the last one
        // is the schema's.
        assert_eq!(
            reword("invalid type: string \"a, expected b\", expected u16"),
            "invalid type: string \"a, expected b\", expected a whole number from 0 to 65535"
        );
    }

    #[test]
    fn line_and_column_are_one_based_and_count_characters() {
        assert_eq!(line_column("abc", 0), (1, 1));
        assert_eq!(line_column("a\nbc", 3), (2, 2));
        assert_eq!(line_column("é = 1\nx", 3), (1, 3));
        assert_eq!(line_column("ab", 99), (1, 3));
    }
}
