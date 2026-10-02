//! Reads a YAML document into a JSON value tree, for `import-cliproxy`.
//!
//! The parser crate's own loader copies an anchored node every time an
//! alias refers to it, without limit, so a small file of nested aliases can
//! ask for gigabytes. This reader builds the tree from the parser's events
//! itself and charges every node and every byte of text — aliased copies
//! and the copies kept for anchors included — against a budget. It also
//! expands `<<` merge keys (explicit keys win; of several merged mappings
//! the earlier wins) and refuses duplicate keys, as the program whose files
//! are being imported does.
//!
//! There are two readings of plain (unquoted) scalars:
//!
//! * [`parse`] types them the way the YAML core schema does: `012345` is
//!   the number 12345 and `0x1F` is 31. Right for values that may be of any
//!   type.
//! * [`parse_verbatim`] keeps what the file says: a plain scalar is a
//!   number or a boolean only when that value is written back exactly as it
//!   stands in the file, and the text otherwise. Right for settings that
//!   are text (keys, passwords, names), where `012345` is not `12345`.
//!
//! Mapping keys are text in both.

use serde_json::{Map, Number, Value};
use std::collections::HashMap;
use switchyard_core::util::mask_secret;
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser, Tag};
use yaml_rust2::scanner::{Marker, TScalarStyle};
use yaml_rust2::yaml::Yaml;

/// Most nodes (scalars and collections; aliased copies and the copies kept
/// for anchors included) a document may expand to. A real configuration has
/// a few thousand.
const MAX_NODES: usize = 200_000;
/// Most bytes of text (scalars and keys; aliased copies and the copies kept
/// for anchors included) a document may expand to. A node is one node
/// however long it is, so the node budget alone would let a long scalar be
/// multiplied for free. Twice the largest file the importer reads: a file
/// without anchors always fits, and so does one whose values each have one.
const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Deepest nesting accepted, so that walking the tree cannot exhaust the
/// stack.
const MAX_DEPTH: usize = 64;
/// Longest mapping key quoted in an error message as written.
const LONGEST_KEY_SHOWN: usize = 40;

/// Why a YAML document could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct YamlError {
    /// 1-based line of the problem, when known.
    pub line: Option<usize>,
    /// What is wrong. Never quotes a value of the document.
    pub message: String,
}

impl std::fmt::Display for YamlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for YamlError {}

/// How plain scalars are read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Scalars {
    /// Null, booleans and numbers as the core schema types them.
    #[default]
    Typed,
    /// A number or a boolean only when it is spelt canonically.
    Verbatim,
}

/// Parses the first document of `text`, typing plain scalars as the YAML
/// core schema does. An empty document is `Value::Null`.
pub fn parse(text: &str) -> Result<Value, YamlError> {
    parse_with(text, Scalars::Typed)
}

/// Parses the first document of `text`, keeping plain scalars as written.
///
/// A plain scalar becomes a number or a boolean only when the text is the
/// canonical spelling of that value (`8317`, `-2.5`, `true`), so turning it
/// back into text gives exactly what the file says. Everything else that
/// [`parse`] would type and thereby rewrite — leading zeros, hexadecimal
/// and exponent forms, digit strings too long for a number, `True` — stays
/// the text of the file. Null (`~`, `null`, nothing) is `Value::Null`.
pub fn parse_verbatim(text: &str) -> Result<Value, YamlError> {
    parse_with(text, Scalars::Verbatim)
}

fn parse_with(text: &str, scalars: Scalars) -> Result<Value, YamlError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut builder = Builder {
        scalars,
        ..Builder::default()
    };
    let mut parser = Parser::new_from_str(text);
    if let Err(error) = parser.load(&mut builder, false) {
        // A problem the builder found comes first: the parser may have
        // stumbled over what followed it.
        if let Some(error) = builder.error {
            return Err(error);
        }
        return Err(YamlError {
            line: Some(error.marker().line()),
            message: scan_message(error.info()),
        });
    }
    if let Some(error) = builder.error {
        return Err(error);
    }
    Ok(builder.root.unwrap_or(Value::Null))
}

/// The scanner's messages are fixed sentences, except for the one about
/// duplicate keys, which is produced by a loader this module does not use.
/// Anything unexpectedly long is cut so that no stretch of the file can be
/// echoed.
fn scan_message(info: &str) -> String {
    let info = info.trim();
    if info.chars().count() > 120 {
        "the file is not valid YAML".to_string()
    } else {
        info.to_string()
    }
}

/// A collection under construction.
enum Frame {
    Sequence {
        items: Vec<Value>,
        anchor: usize,
    },
    Mapping {
        entries: Map<String, Value>,
        /// Mappings pulled in with `<<`, in the order written.
        merges: Vec<Map<String, Value>>,
        /// The key waiting for its value.
        key: Option<String>,
        anchor: usize,
    },
}

/// What a value costs: to copy it for an alias, and to walk it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Weight {
    /// Scalars, collections and keys.
    nodes: usize,
    /// Bytes of text in strings and keys.
    bytes: usize,
    /// Levels of collections: 0 for a scalar.
    depth: usize,
}

#[derive(Default)]
struct Builder {
    scalars: Scalars,
    stack: Vec<Frame>,
    /// Anchored nodes with what they weigh.
    anchors: HashMap<usize, (Value, Weight)>,
    nodes: usize,
    bytes: usize,
    root: Option<Value>,
    done: bool,
    error: Option<YamlError>,
}

impl MarkedEventReceiver for Builder {
    fn on_event(&mut self, event: Event, mark: Marker) {
        if self.error.is_some() || self.done {
            return;
        }
        if let Err(message) = self.handle(event) {
            self.error = Some(YamlError {
                line: Some(mark.line()),
                message,
            });
        }
    }
}

impl Builder {
    fn handle(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::Nothing | Event::StreamStart | Event::StreamEnd | Event::DocumentStart => Ok(()),
            Event::DocumentEnd => {
                // Only the first document is the configuration.
                self.done = true;
                Ok(())
            }
            Event::SequenceStart(anchor, _) => {
                self.open()?;
                self.stack.push(Frame::Sequence {
                    items: Vec::new(),
                    anchor,
                });
                Ok(())
            }
            Event::MappingStart(anchor, _) => {
                self.open()?;
                self.stack.push(Frame::Mapping {
                    entries: Map::new(),
                    merges: Vec::new(),
                    key: None,
                    anchor,
                });
                Ok(())
            }
            Event::SequenceEnd => match self.stack.pop() {
                Some(Frame::Sequence { items, anchor }) => {
                    self.complete(Value::Array(items), anchor)
                }
                _ => Err("unexpected end of a sequence".to_string()),
            },
            Event::MappingEnd => match self.stack.pop() {
                Some(Frame::Mapping {
                    mut entries,
                    merges,
                    anchor,
                    ..
                }) => {
                    for merged in merges {
                        for (key, value) in merged {
                            if !entries.contains_key(&key) {
                                entries.insert(key, value);
                            }
                        }
                    }
                    self.complete(Value::Object(entries), anchor)
                }
                _ => Err("unexpected end of a mapping".to_string()),
            },
            Event::Scalar(text, style, anchor, tag) => {
                self.count(1, text.len())?;
                let plain = style == TScalarStyle::Plain;
                if self.expects_key() {
                    // A key is its text, whatever a value of that spelling
                    // would be: `007:` is not the key `7`.
                    if anchor > 0 {
                        let value = scalar(text.clone(), plain, tag.as_ref(), self.scalars);
                        self.remember(anchor, &value)?;
                    }
                    // Remembered with a marker so that only an unquoted
                    // `<<` merges.
                    let name = if plain && text == "<<" {
                        MERGE_KEY.to_string()
                    } else {
                        text
                    };
                    self.set_key(name);
                    return Ok(());
                }
                let value = scalar(text, plain, tag.as_ref(), self.scalars);
                self.complete(value, anchor)
            }
            Event::Alias(id) => {
                let (value, weight) = self
                    .anchors
                    .get(&id)
                    .ok_or_else(|| "an alias refers to an unknown anchor".to_string())?;
                let weight = *weight;
                // Aliases of aliases could otherwise build a tree deeper
                // than any single branch of the file.
                if self.stack.len().saturating_add(weight.depth) > MAX_DEPTH {
                    return Err(too_deep());
                }
                // Charged before it is copied.
                self.nodes = charge(self.nodes, weight.nodes, MAX_NODES, too_many_nodes)?;
                self.bytes = charge(self.bytes, weight.bytes, MAX_BYTES, too_many_bytes)?;
                let value = value.clone();
                self.place(value)
            }
        }
    }

    /// Accounts for a collection being opened.
    fn open(&mut self) -> Result<(), String> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(too_deep());
        }
        self.count(1, 0)
    }

    fn count(&mut self, nodes: usize, bytes: usize) -> Result<(), String> {
        self.nodes = charge(self.nodes, nodes, MAX_NODES, too_many_nodes)?;
        self.bytes = charge(self.bytes, bytes, MAX_BYTES, too_many_bytes)?;
        Ok(())
    }

    /// Whether the next node is the key of the mapping being read.
    fn expects_key(&self) -> bool {
        matches!(self.stack.last(), Some(Frame::Mapping { key: None, .. }))
    }

    fn set_key(&mut self, name: String) {
        if let Some(Frame::Mapping { key, .. }) = self.stack.last_mut() {
            *key = Some(name);
        }
    }

    /// Keeps a copy of an anchored node for the aliases that may follow.
    /// The copy is memory like any other, so it is charged like an alias:
    /// anchors nested in anchors would otherwise multiply what is inside
    /// them without a single alias.
    fn remember(&mut self, anchor: usize, value: &Value) -> Result<(), String> {
        let weight = weigh(value);
        self.count(weight.nodes, weight.bytes)?;
        self.anchors.insert(anchor, (value.clone(), weight));
        Ok(())
    }

    /// Registers a finished node under its anchor, if it has one, and hands
    /// it to its parent.
    fn complete(&mut self, value: Value, anchor: usize) -> Result<(), String> {
        if anchor > 0 {
            self.remember(anchor, &value)?;
        }
        self.place(value)
    }

    /// Hands a node to its parent (or makes it the root).
    fn place(&mut self, value: Value) -> Result<(), String> {
        match self.stack.last_mut() {
            None => {
                self.root = Some(value);
                Ok(())
            }
            Some(Frame::Sequence { items, .. }) => {
                items.push(value);
                Ok(())
            }
            Some(Frame::Mapping {
                entries,
                merges,
                key,
                ..
            }) => match key.take() {
                // A key that is not a scalar of its own: an alias, or a
                // collection.
                None => {
                    let name = match value {
                        Value::String(text) => text,
                        Value::Null => "null".to_string(),
                        Value::Bool(flag) => flag.to_string(),
                        Value::Number(number) => number.to_string(),
                        Value::Array(_) | Value::Object(_) => {
                            return Err(
                                "a mapping key is itself a mapping or a sequence".to_string()
                            );
                        }
                    };
                    *key = Some(name);
                    Ok(())
                }
                Some(name) if name == MERGE_KEY => {
                    match value {
                        Value::Object(map) => merges.push(map),
                        Value::Array(items) => {
                            for item in items {
                                match item {
                                    Value::Object(map) => merges.push(map),
                                    _ => {
                                        return Err(
                                            "a `<<` merge key needs mappings to merge".to_string()
                                        );
                                    }
                                }
                            }
                        }
                        _ => return Err("a `<<` merge key needs a mapping to merge".to_string()),
                    }
                    Ok(())
                }
                Some(name) => {
                    if entries.contains_key(&name) {
                        return Err(format!("the key `{}` appears twice", shown_key(&name)));
                    }
                    entries.insert(name, value);
                    Ok(())
                }
            },
        }
    }
}

/// Internal name of a merge key while its value is being read. Contains a
/// NUL, which a YAML key cannot.
const MERGE_KEY: &str = "\0<<";

/// Adds `amount` to a budget counter, or says that the budget is spent.
fn charge(
    used: usize,
    amount: usize,
    limit: usize,
    message: fn() -> String,
) -> Result<usize, String> {
    match used.checked_add(amount) {
        Some(total) if total <= limit => Ok(total),
        _ => Err(message()),
    }
}

fn too_many_nodes() -> String {
    format!("the document expands to more than {MAX_NODES} values")
}

fn too_many_bytes() -> String {
    format!(
        "the document expands to more than {} MiB of text",
        MAX_BYTES / (1024 * 1024)
    )
}

fn too_deep() -> String {
    format!("nested more than {MAX_DEPTH} levels deep")
}

fn shown_key(key: &str) -> String {
    if key.chars().count() <= LONGEST_KEY_SHOWN {
        key.to_string()
    } else {
        mask_secret(key)
    }
}

/// Weighs a finished value. Its depth is bounded by [`MAX_DEPTH`], which is
/// what bounds the recursion.
fn weigh(value: &Value) -> Weight {
    match value {
        Value::String(text) => Weight {
            nodes: 1,
            bytes: text.len(),
            depth: 0,
        },
        Value::Array(items) => {
            let mut weight = Weight {
                nodes: 1,
                bytes: 0,
                depth: 0,
            };
            for item in items {
                let inner = weigh(item);
                weight.nodes = weight.nodes.saturating_add(inner.nodes);
                weight.bytes = weight.bytes.saturating_add(inner.bytes);
                weight.depth = weight.depth.max(inner.depth);
            }
            weight.depth += 1;
            weight
        }
        Value::Object(map) => {
            let mut weight = Weight {
                nodes: 1,
                bytes: 0,
                depth: 0,
            };
            for (key, item) in map {
                let inner = weigh(item);
                weight.nodes = weight.nodes.saturating_add(inner.nodes).saturating_add(1);
                weight.bytes = weight
                    .bytes
                    .saturating_add(inner.bytes)
                    .saturating_add(key.len());
                weight.depth = weight.depth.max(inner.depth);
            }
            weight.depth += 1;
            weight
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => Weight {
            nodes: 1,
            bytes: 0,
            depth: 0,
        },
    }
}

/// The value of a scalar. Quoted and block scalars are strings, and so are
/// plain ones tagged `!!str`; other plain ones are read as `scalars` says.
fn scalar(text: String, plain: bool, tag: Option<&Tag>, scalars: Scalars) -> Value {
    if !plain {
        return Value::String(text);
    }
    if let Some(tag) = tag
        && tag.handle == "tag:yaml.org,2002:"
        && tag.suffix == "str"
    {
        return Value::String(text);
    }
    match (core_schema(&text), scalars) {
        (None, _) => Value::String(text),
        (Some(value), Scalars::Typed) => value,
        (Some(Value::Null), Scalars::Verbatim) => Value::Null,
        (Some(value), Scalars::Verbatim) => {
            let canonical = match &value {
                Value::Bool(flag) => flag.to_string() == text,
                Value::Number(number) => number.to_string() == text,
                _ => false,
            };
            if canonical {
                value
            } else {
                Value::String(text)
            }
        }
    }
}

/// What the YAML core schema makes of a plain scalar that is not a string:
/// null, a boolean or a number.
fn core_schema(text: &str) -> Option<Value> {
    match Yaml::from_str(text) {
        Yaml::Null => Some(Value::Null),
        Yaml::Boolean(flag) => Some(Value::Bool(flag)),
        Yaml::Integer(number) => Some(Value::Number(number.into())),
        // Infinities and NaN have no JSON number; they stay text.
        Yaml::Real(real) => real
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalars_are_typed() {
        let value = parse(
            "a: 1\nb: -2.5\nc: true\nd: null\ne: ~\nf: plain text\ng: \"8317\"\nh: '~'\ni:\nj: 0x10\nk: !!str 42\n",
        )
        .unwrap();
        assert_eq!(
            value,
            json!({
                "a": 1, "b": -2.5, "c": true, "d": null, "e": null, "f": "plain text",
                "g": "8317", "h": "~", "i": null, "j": 16, "k": "42"
            })
        );
        // What typing rewrites.
        assert_eq!(
            parse("a: 012345\nb: 1e3\nc: +5\nd: True\ne: 1.50\nf: .inf\n").unwrap(),
            json!({"a": 12345, "b": 1000.0, "c": 5, "d": true, "e": 1.5, "f": ".inf"})
        );
    }

    /// The verbatim reading types a plain scalar only when nothing is lost
    /// by it.
    #[test]
    fn verbatim_scalars_keep_their_text() {
        let value = parse_verbatim(
            "a: 1\nb: -2.5\nc: true\nd: null\ne: ~\nf: plain text\ng: \"8317\"\nh: '~'\ni:\n\
             j: 0x10\nk: !!str 42\nl: false\nm: 0\nn: 1.0\n",
        )
        .unwrap();
        assert_eq!(
            value,
            json!({
                "a": 1, "b": -2.5, "c": true, "d": null, "e": null, "f": "plain text",
                "g": "8317", "h": "~", "i": null, "j": "0x10", "k": "42", "l": false,
                "m": 0, "n": 1.0
            })
        );
        for written in [
            "012345",
            "00998877",
            "123456789012345678901234567890",
            "1e5",
            "0x1F",
            "0o17",
            "+5",
            "1.50",
            "1.",
            ".5",
            "-0",
            "True",
            "FALSE",
            "1e400",
            ".inf",
            ".NaN",
        ] {
            assert_eq!(
                parse_verbatim(&format!("key: {written}\n")).unwrap(),
                json!({"key": written}),
                "{written}"
            );
            assert_eq!(
                parse_verbatim(&format!("- {written}\n")).unwrap(),
                json!([written]),
                "{written}"
            );
        }
        // Whatever is typed reads back as the text of the file.
        for written in ["8317", "-12", "0", "2.5", "-0.25", "1.0", "true", "false"] {
            let value = parse_verbatim(&format!("key: {written}\n")).unwrap();
            assert!(!value["key"].is_string(), "{written}");
            assert_eq!(value["key"].to_string(), written);
        }
        // An alias gives the same text as its anchor.
        assert_eq!(
            parse_verbatim("a: &a 007\nb: *a\n").unwrap(),
            json!({"a": "007", "b": "007"})
        );
    }

    #[test]
    fn both_readings_have_the_same_shape() {
        let text = "\
base: &base
  n: 010
  list: [1, 0x2, three]
copy:
  <<: *base
  extra: 1e1
again: *base
";
        assert_eq!(
            parse(text).unwrap(),
            json!({
                "base": {"n": 10, "list": [1, 2, "three"]},
                "copy": {"extra": 10.0, "n": 10, "list": [1, 2, "three"]},
                "again": {"n": 10, "list": [1, 2, "three"]}
            })
        );
        assert_eq!(
            parse_verbatim(text).unwrap(),
            json!({
                "base": {"n": "010", "list": [1, "0x2", "three"]},
                "copy": {"extra": "1e1", "n": "010", "list": [1, "0x2", "three"]},
                "again": {"n": "010", "list": [1, "0x2", "three"]}
            })
        );
    }

    #[test]
    fn nesting_and_order_are_kept() {
        let value = parse(
            "server:\n  port: 8317\n  host: \"\"\nlist:\n  - one\n  - {name: x, alias: y}\n  - [1, 2]\nempty: []\nnone: {}\n",
        )
        .unwrap();
        assert_eq!(
            value,
            json!({
                "server": {"port": 8317, "host": ""},
                "list": ["one", {"name": "x", "alias": "y"}, [1, 2]],
                "empty": [],
                "none": {}
            })
        );
        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["server", "list", "empty", "none"]);
    }

    #[test]
    fn keys_are_text_as_written() {
        for read in [parse, parse_verbatim] {
            assert_eq!(
                read("8317: a\ntrue: b\n007: c\n0x10: d\n1.50: e\n\"9\": f\n").unwrap(),
                json!({"8317": "a", "true": "b", "007": "c", "0x10": "d", "1.50": "e", "9": "f"})
            );
            assert!(
                read("? [a, b]\n: c\n")
                    .unwrap_err()
                    .message
                    .contains("mapping key")
            );
            // A key that is written twice is a duplicate; two spellings of
            // one number are two keys.
            assert!(read("7: a\n7: b\n").is_err());
            assert_eq!(read("7: a\n07: b\n").unwrap(), json!({"7": "a", "07": "b"}));
            // A key from an alias, and an anchor on a key.
            assert_eq!(
                read("name: &n model\n*n : x\n").unwrap(),
                json!({"name": "model", "model": "x"})
            );
            assert_eq!(
                read("&k key: 1\nother: *k\n").unwrap(),
                json!({"key": 1, "other": "key"})
            );
        }
    }

    #[test]
    fn anchors_aliases_and_merge_keys() {
        let text = "\
base: &base
  proxy-url: direct
  priority: 1
other: &other
  priority: 9
  prefix: team
a:
  <<: *base
  priority: 2
b:
  <<: [*base, *other]
c: *base
quoted:
  \"<<\": literal
";
        let value = parse(text).unwrap();
        assert_eq!(value["a"], json!({"priority": 2, "proxy-url": "direct"}));
        // The earlier merged mapping wins.
        assert_eq!(
            value["b"],
            json!({"proxy-url": "direct", "priority": 1, "prefix": "team"})
        );
        assert_eq!(value["c"], value["base"]);
        assert_eq!(value["quoted"], json!({"<<": "literal"}));
        assert_eq!(parse_verbatim(text).unwrap(), value);
        assert!(parse("a:\n  <<: 3\n").is_err());
        assert!(parse("a: *nowhere\n").is_err());
    }

    #[test]
    fn duplicate_keys_are_refused_without_echoing_long_ones() {
        let error = parse("port: 1\nhost: x\nport: 2\n").unwrap_err();
        assert_eq!(error.message, "the key `port` appears twice");
        assert!(error.line.is_some());
        assert!(error.to_string().starts_with("line "));

        let long = "k".repeat(80);
        let error = parse(&format!("{long}: 1\n{long}: 2\n")).unwrap_err();
        assert!(!error.message.contains(&long), "{}", error.message);
    }

    #[test]
    fn only_the_first_document_is_read() {
        assert_eq!(parse("a: 1\n---\nb: 2\n").unwrap(), json!({"a": 1}));
        assert_eq!(parse("").unwrap(), Value::Null);
        assert_eq!(parse("# only a comment\n").unwrap(), Value::Null);
        assert_eq!(parse("\u{feff}a: 1\n").unwrap(), json!({"a": 1}));
        assert_eq!(parse_verbatim("").unwrap(), Value::Null);
        assert_eq!(
            parse_verbatim("\u{feff}a: 01\n").unwrap(),
            json!({"a": "01"})
        );
    }

    #[test]
    fn syntax_errors_have_a_line() {
        let error = parse("a: [1, 2\nb: 3\n").unwrap_err();
        assert!(error.line.is_some(), "{error:?}");
        assert!(!error.message.is_empty());
        assert!(parse("a: b: c: d\n").is_err());
        assert!(parse("\tx: 1\n  y: [}\n").is_err());
    }

    /// Nine levels of nine aliases each: 9^9 values when expanded.
    #[test]
    fn alias_bombs_are_stopped() {
        let mut text = String::from("a0: &a0 [x, x, x, x, x, x, x, x, x]\n");
        for level in 1..=9 {
            let previous = level - 1;
            let aliases = vec![format!("*a{previous}"); 9].join(", ");
            text.push_str(&format!("a{level}: &a{level} [{aliases}]\n"));
        }
        let error = parse(&text).unwrap_err();
        assert!(error.message.contains("expands to more than"), "{error}");
    }

    /// One node, many bytes: a long scalar referred to a thousand times is
    /// a thousand nodes and tens of megabytes.
    #[test]
    fn aliases_of_a_long_scalar_are_charged_by_the_byte() {
        const SCALAR: usize = 64 * 1024;
        let mut text = format!("big: &big \"{}\"\ncopies:\n", "k".repeat(SCALAR));
        for _ in 0..1000 {
            text.push_str("  - *big\n");
        }
        for read in [parse, parse_verbatim] {
            let error = read(&text).unwrap_err();
            assert_eq!(
                error.message,
                "the document expands to more than 16 MiB of text"
            );
            assert!(error.line.is_some());
            assert!(!error.to_string().contains("kkkk"));
        }
        // The same as a merged mapping and as a mapping key's value.
        let mut merged = format!("base: &base\n  text: \"{}\"\n", "k".repeat(SCALAR));
        for index in 0..1000 {
            merged.push_str(&format!("m{index}:\n  <<: *base\n"));
        }
        assert!(parse(&merged).unwrap_err().message.contains("MiB of text"));

        // A few references to a long scalar are an ordinary document.
        let mut fine = format!("big: &big \"{}\"\ncopies:\n", "k".repeat(SCALAR));
        for _ in 0..20 {
            fine.push_str("  - *big\n");
        }
        let value = parse(&fine).unwrap();
        assert_eq!(value["copies"].as_array().unwrap().len(), 20);
        assert_eq!(value["copies"][19].as_str().unwrap().len(), SCALAR);
    }

    /// The copy kept for an anchor is charged too: anchors nested in
    /// anchors multiply what is inside them without any alias.
    #[test]
    fn nested_anchors_cannot_multiply_a_long_scalar() {
        const SCALAR: usize = 512 * 1024;
        const LEVELS: usize = 60;
        let mut text = String::new();
        for level in 0..LEVELS {
            text.push_str(&format!("&a{level} ["));
        }
        text.push_str(&format!("\"{}\"", "k".repeat(SCALAR)));
        text.push_str(&"]".repeat(LEVELS));
        text.push('\n');
        let error = parse(&text).unwrap_err();
        assert!(error.message.contains("MiB of text"), "{error}");

        // Without the anchors the same document is fine, and so is a long
        // scalar with one anchor.
        let plain = format!(
            "{}\"{}\"{}\n",
            "[".repeat(LEVELS),
            "k".repeat(SCALAR),
            "]".repeat(LEVELS)
        );
        assert!(parse(&plain).is_ok());
        let once = format!("a: &a \"{}\"\nb: *a\n", "k".repeat(4 * 1024 * 1024));
        assert!(parse(&once).is_ok());
    }

    #[test]
    fn excessive_nesting_is_stopped() {
        let text = format!("{}1{}", "[".repeat(200), "]".repeat(200));
        let error = parse(&text).unwrap_err();
        assert!(error.message.contains("levels deep"), "{error}");
        let fine = format!("{}1{}", "[".repeat(30), "]".repeat(30));
        assert!(parse(&fine).is_ok());
    }

    /// Each line nests the previous one thirty levels further down: no
    /// branch of the file is deep, the expanded tree would be.
    #[test]
    fn aliases_cannot_stack_into_a_deep_tree() {
        let wrap = |inner: &str| format!("{}{inner}{}", "[".repeat(30), "]".repeat(30));
        let mut text = format!("a0: &a0 {}\n", wrap("x"));
        for level in 1..=40 {
            let previous = level - 1;
            text.push_str(&format!(
                "a{level}: &a{level} {}\n",
                wrap(&format!("*a{previous}"))
            ));
        }
        for read in [parse, parse_verbatim] {
            let error = read(&text).unwrap_err();
            assert!(error.message.contains("levels deep"), "{error}");
            // Refused at the first alias that goes too deep (line 3: 30
            // levels around 60).
            assert_eq!(error.line, Some(3));
        }
        // Up to the limit is fine: 30 levels around 30.
        let two = format!("a0: &a0 {}\na1: {}\n", wrap("x"), wrap("*a0"));
        assert!(parse(&two).is_ok());
    }

    #[test]
    fn weights_count_nodes_bytes_and_depth() {
        assert_eq!(
            weigh(&json!("four")),
            Weight {
                nodes: 1,
                bytes: 4,
                depth: 0
            }
        );
        assert_eq!(
            weigh(&json!({"key": ["ab", 1, {"k": null}]})),
            Weight {
                // Mapping, key, list, two scalars, mapping, key, null.
                nodes: 8,
                bytes: 3 + 2 + 1,
                depth: 3
            }
        );
        assert_eq!(weigh(&json!([])).depth, 1);
    }

    #[test]
    fn garbage_never_panics() {
        for text in [
            "\0\0\0",
            "{{{{",
            "- - - - :",
            "key: \"unterminated",
            "&a *a",
            "? ? ? ?",
            "%YAML 9.9\n---\nx",
            "a: !!binary |\n  zzzz",
            "\u{1}\u{2}: \u{3}",
            "&a : &a x\n*a : *a\n",
            "<<: 1\n",
            "- <<\n",
        ] {
            let _ = parse(text);
            let _ = parse_verbatim(text);
        }
    }
}
