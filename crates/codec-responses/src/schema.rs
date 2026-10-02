//! Making a function tool's parameter schema acceptable to a Responses
//! upstream.
//!
//! Tool schemas written for other protocols are routinely rejected here with
//! a 400 that fails the whole request: a root without `type: "object"`, an
//! object root without `properties` (the usual shape of a tool that takes no
//! arguments), dialect markers left in by schema generators, and regular
//! expressions the vendor's validator cannot compile. None of that changes
//! what arguments the tool takes, so it is repaired rather than passed on.
//!
//! Only schemas that arrived through another protocol are touched. A
//! Responses client's own schema is forwarded as written.

use serde_json::{Map, Value, json};
use switchyard_core::ir::empty_object_schema;

/// Keywords whose value is a single subschema.
const SCHEMA_KEYWORDS: &[&str] = &[
    "items",
    "additionalProperties",
    "additionalItems",
    "unevaluatedProperties",
    "unevaluatedItems",
    "contains",
    "propertyNames",
    "not",
    "if",
    "then",
    "else",
];

/// Keywords whose value is a list of subschemas.
const SCHEMA_LIST_KEYWORDS: &[&str] = &["anyOf", "oneOf", "allOf", "prefixItems"];

/// Keywords whose value maps names to subschemas.
const SCHEMA_MAP_KEYWORDS: &[&str] = &[
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
];

/// Whether a regular expression uses constructs the vendor's validator
/// cannot compile: Unicode property classes and NUL escapes.
fn unsupported_pattern(pattern: &str) -> bool {
    pattern.contains("\\p{") || pattern.contains("\\P{") || pattern.contains("\\0")
}

/// Cleans one schema node and everything below it. The walk follows schema
/// keywords only, so data that merely looks like a schema (`default`,
/// `enum`, `const`, `examples`) is never altered.
fn clean(schema: &mut Value) {
    match schema {
        Value::Object(node) => clean_node(node),
        // `items` may be a list of schemas in older drafts.
        Value::Array(list) => list.iter_mut().for_each(clean),
        _ => {}
    }
}

fn clean_node(node: &mut Map<String, Value>) {
    node.remove("$schema");
    node.remove("$id");
    if node
        .get("pattern")
        .and_then(Value::as_str)
        .is_some_and(unsupported_pattern)
    {
        node.remove("pattern");
    }
    if let Some(Value::Object(patterns)) = node.get_mut("patternProperties") {
        patterns.retain(|pattern, _| !unsupported_pattern(pattern));
    }
    for (keyword, value) in node.iter_mut() {
        let keyword = keyword.as_str();
        if SCHEMA_KEYWORDS.contains(&keyword) {
            clean(value);
        } else if SCHEMA_LIST_KEYWORDS.contains(&keyword) {
            if let Value::Array(list) = value {
                list.iter_mut().for_each(clean);
            }
        } else if SCHEMA_MAP_KEYWORDS.contains(&keyword)
            && let Value::Object(map) = value
        {
            map.values_mut().for_each(clean);
        }
    }
}

/// The parameter schema of a function tool that came from another protocol,
/// in a form this API accepts:
///
/// * anything that is not a JSON object becomes the empty object schema;
/// * a root without a `type` is an object; an object root gets `properties`
///   when it has none;
/// * `$schema` and `$id` are removed throughout;
/// * `pattern` values and `patternProperties` keys with Unicode property
///   classes (`\p{…}`, `\P{…}`) or `\0` are removed throughout.
pub(crate) fn portable_parameters(parameters: &Value) -> Value {
    let Value::Object(root) = parameters else {
        return empty_object_schema();
    };
    let mut root = root.clone();
    clean_node(&mut root);
    let is_object = match root.get("type") {
        None | Some(Value::Null) => true,
        Some(Value::String(kind)) => kind.is_empty() || kind == "object",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "object"),
        Some(_) => false,
    };
    if is_object {
        if !root.get("type").is_some_and(|kind| kind.is_array()) {
            root.insert("type".into(), json!("object"));
        }
        if !root.get("properties").is_some_and(Value::is_object) {
            root.insert("properties".into(), json!({}));
        }
    }
    Value::Object(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_that_are_not_object_schemas_are_repaired() {
        let empty = json!({"type": "object", "properties": {}});
        assert_eq!(portable_parameters(&Value::Null), empty);
        assert_eq!(portable_parameters(&json!("nonsense")), empty);
        assert_eq!(portable_parameters(&json!({})), empty);
        assert_eq!(portable_parameters(&json!({"type": "object"})), empty);
        assert_eq!(portable_parameters(&json!({"type": ""})), empty);
        assert_eq!(
            portable_parameters(&json!({"type": "object", "properties": null})),
            empty
        );
        assert_eq!(
            portable_parameters(
                &json!({"properties": {"q": {"type": "string"}}, "required": ["q"]})
            ),
            json!({"properties": {"q": {"type": "string"}}, "required": ["q"], "type": "object"})
        );
        assert_eq!(
            portable_parameters(&json!({"type": ["object", "null"]})),
            json!({"type": ["object", "null"], "properties": {}})
        );
        // Not an object schema: left for the upstream to judge.
        assert_eq!(
            portable_parameters(&json!({"type": "string"})),
            json!({"type": "string"})
        );
    }

    #[test]
    fn a_well_formed_schema_is_left_exactly_as_it_is() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "pattern": "^[a-z/]+$", "description": "Where"},
                "mode": {"enum": ["r", "w"], "default": "r"},
                "tags": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["path"],
            "additionalProperties": false
        });
        assert_eq!(portable_parameters(&schema), schema);
    }

    #[test]
    fn dialect_markers_and_uncompilable_patterns_are_removed_throughout() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$id": "https://example.com/tool.json",
            "type": "object",
            "properties": {
                "name": {"type": "string", "pattern": "^\\p{L}+$"},
                "code": {"type": "string", "pattern": "^[A-Z]{3}$"},
                "nested": {
                    "$id": "#nested",
                    "type": "object",
                    "patternProperties": {
                        "^\\P{Cc}+$": {"type": "string"},
                        "^x-": {"type": "string", "pattern": "a\\0b"}
                    },
                    "additionalProperties": {"type": "string", "pattern": "\\p{Lu}"}
                },
                "list": {"type": "array", "items": {"anyOf": [
                    {"type": "string", "pattern": "\\p{N}"},
                    {"$schema": "x", "type": "number"}
                ]}}
            },
            "$defs": {"word": {"type": "string", "pattern": "\\p{L}"}}
        });
        assert_eq!(
            portable_parameters(&schema),
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "code": {"type": "string", "pattern": "^[A-Z]{3}$"},
                    "nested": {
                        "type": "object",
                        "patternProperties": {"^x-": {"type": "string"}},
                        "additionalProperties": {"type": "string"}
                    },
                    "list": {"type": "array", "items": {"anyOf": [
                        {"type": "string"},
                        {"type": "number"}
                    ]}}
                },
                "$defs": {"word": {"type": "string"}}
            })
        );
    }

    #[test]
    fn data_that_looks_like_a_schema_is_not_touched() {
        let schema = json!({
            "type": "object",
            "properties": {
                // A property that happens to be called `pattern` / `$id`.
                "pattern": {"type": "string", "default": "\\p{L}"},
                "$id": {"type": "string"},
                "filter": {
                    "type": "object",
                    "default": {"$schema": "kept", "pattern": "\\p{L}"},
                    "examples": [{"$id": "kept", "pattern": "\\p{L}"}],
                    "const": {"$id": "kept"},
                    "enum": [{"pattern": "\\p{L}"}]
                }
            }
        });
        assert_eq!(portable_parameters(&schema), schema);
    }
}
