//! Normalisation of JSON Schemas that were written for another protocol
//! before they are sent to a Chat Completions upstream.
//!
//! Anthropic, Gemini and MCP tool schemas are looser than what OpenAI (and
//! the stricter compatible servers) accept as function `parameters`:
//!
//! * an object schema without `properties` (what every argument-less MCP
//!   tool declares: `{"type":"object"}`) is rejected as
//!   `invalid_function_parameters`, so `"properties": {}` is added;
//! * a boolean subschema `true` ("anything") is not understood and becomes
//!   the equivalent `{}`. `false` is kept, and so are the booleans of
//!   `additionalProperties`, which strict mode requires verbatim;
//! * `pattern` strings using Unicode property classes (`\p{…}`, `\P{…}`) or
//!   `\0` cannot be compiled by upstream validators and are removed (a
//!   constraint is lost, the request is not); `patternProperties` entries
//!   keyed by such a pattern are removed likewise.
//!
//! * the root of a function's `parameters` has to be an object schema
//!   ("schema must be a JSON Schema of 'type: \"object\"'"). Gemini accepts
//!   declarations whose root names no type, so a root without `type` is
//!   given `"type": "object"`, and a root that declares another type (which
//!   no function call could satisfy) becomes the empty object schema.
//!
//! Only schema positions are visited. Data positions (`default`, `enum`,
//! `const`, `examples`) are never touched, so a default value that happens to
//! contain a `pattern` key survives.

use serde_json::{Map, Value};
use switchyard_core::ir::empty_object_schema;

/// Keywords whose value is a single subschema (or, for the draft-04 tuple
/// form of `items`, an array of subschemas).
const SINGLE: &[&str] = &[
    "items",
    "contains",
    "additionalProperties",
    "propertyNames",
    "unevaluatedProperties",
    "unevaluatedItems",
    "additionalItems",
    "contentSchema",
    "not",
    "if",
    "then",
    "else",
];

/// Keywords whose value maps names to subschemas.
const MAPS: &[&str] = &[
    "properties",
    "$defs",
    "definitions",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
];

/// Keywords whose value is an array of subschemas.
const LISTS: &[&str] = &["prefixItems", "anyOf", "oneOf", "allOf"];

/// Normalises a function tool's parameter schema for a Chat upstream.
/// `Value::Null` (no parameters) and anything that is not a schema object
/// become the canonical empty object schema.
pub(crate) fn normalize_parameters(schema: &Value) -> Value {
    let Value::Object(root) = schema else {
        return empty_object_schema();
    };
    if declares_object(root) {
        return normalize_schema(schema);
    }
    match root.get("type") {
        None | Some(Value::Null) => {
            let mut typed = Map::new();
            typed.insert("type".into(), Value::String("object".into()));
            typed.extend(
                root.iter()
                    .filter(|(key, _)| key.as_str() != "type")
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
            normalize_schema(&Value::Object(typed))
        }
        Some(_) => empty_object_schema(),
    }
}

/// Normalises any schema (see the module documentation). Non-object roots
/// are returned unchanged.
pub(crate) fn normalize_schema(schema: &Value) -> Value {
    let mut out = schema.clone();
    walk(&mut out);
    out
}

/// A regular expression the upstream's validator cannot compile.
fn unsupported_pattern(pattern: &str) -> bool {
    pattern.contains("\\p{") || pattern.contains("\\P{") || pattern.contains("\\0")
}

fn declares_object(schema: &Map<String, Value>) -> bool {
    match schema.get("type") {
        Some(Value::String(kind)) => kind == "object",
        Some(Value::Array(kinds)) => kinds.iter().any(|k| k.as_str() == Some("object")),
        _ => false,
    }
}

/// Normalises a value in a subschema position. `keep_bool` is set for
/// `additionalProperties`, whose boolean form is meaningful to strict mode.
fn subschema(value: &mut Value, keep_bool: bool) {
    match value {
        Value::Bool(true) if !keep_bool => *value = Value::Object(Map::new()),
        Value::Object(_) => walk(value),
        _ => {}
    }
}

fn walk(schema: &mut Value) {
    let Value::Object(map) = schema else {
        return;
    };
    if map
        .get("pattern")
        .and_then(Value::as_str)
        .is_some_and(unsupported_pattern)
    {
        map.shift_remove("pattern");
    }
    if let Some(Value::Object(patterns)) = map.get_mut("patternProperties") {
        patterns.retain(|pattern, _| !unsupported_pattern(pattern));
    }
    if declares_object(map) && !map.contains_key("properties") {
        map.insert("properties".into(), Value::Object(Map::new()));
    }
    for (key, value) in map.iter_mut() {
        let key = key.as_str();
        if SINGLE.contains(&key) {
            match value {
                Value::Array(items) => items.iter_mut().for_each(|item| subschema(item, false)),
                other => subschema(other, key == "additionalProperties"),
            }
        } else if MAPS.contains(&key) {
            if let Value::Object(entries) = value {
                // `dependencies` may also map to arrays of property names;
                // `subschema` leaves those alone.
                entries
                    .values_mut()
                    .for_each(|entry| subschema(entry, false));
            }
        } else if LISTS.contains(&key)
            && let Value::Array(items) = value
        {
            items.iter_mut().for_each(|item| subschema(item, false));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn a_parameters_root_is_always_an_object_schema() {
        assert_eq!(
            normalize_parameters(&json!({})),
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            normalize_parameters(
                &json!({"properties": {"q": {"type": "string"}}, "required": ["q"]})
            ),
            json!({"type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"]})
        );
        assert_eq!(
            normalize_parameters(&json!({"type": null, "properties": {"q": {}}})),
            json!({"type": "object", "properties": {"q": {}}})
        );
        assert_eq!(
            normalize_parameters(&json!({"type": "string"})),
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            normalize_parameters(&json!({"type": ["object", "null"]})),
            json!({"type": ["object", "null"], "properties": {}})
        );
    }

    #[test]
    fn missing_and_non_object_parameters_become_the_empty_object_schema() {
        let empty = json!({"type": "object", "properties": {}});
        assert_eq!(normalize_parameters(&Value::Null), empty);
        assert_eq!(normalize_parameters(&json!(true)), empty);
        assert_eq!(normalize_parameters(&json!("object")), empty);
        assert_eq!(normalize_parameters(&json!([])), empty);
    }

    #[test]
    fn key_order_is_preserved_and_properties_is_appended() {
        let out = normalize_parameters(&json!({"description": "d", "type": "object"}));
        assert_eq!(
            out.to_string(),
            r#"{"description":"d","type":"object","properties":{}}"#
        );
    }

    #[test]
    fn nullable_object_types_get_properties_too() {
        assert_eq!(
            normalize_schema(&json!({"type": ["object", "null"]})),
            json!({"type": ["object", "null"], "properties": {}})
        );
        // A schema that does not say it is an object is left alone.
        assert_eq!(normalize_schema(&json!({})), json!({}));
        assert_eq!(
            normalize_schema(&json!({"type": "string"})),
            json!({"type": "string"})
        );
    }

    #[test]
    fn every_subschema_keyword_is_visited() {
        let input = json!({
            "type": "object",
            "properties": {"a": {"type": "object"}},
            "$defs": {"d": true, "e": {"type": "object"}},
            "definitions": {"d": {"type": "object"}},
            "patternProperties": {"^x": {"type": "object"}},
            "dependentSchemas": {"a": {"type": "object"}},
            "dependencies": {"a": ["b"], "b": {"type": "object"}},
            "items": [{"type": "object"}, true],
            "prefixItems": [{"type": "object"}],
            "contains": {"type": "object"},
            "propertyNames": {"pattern": "^\\p{L}+$"},
            "unevaluatedProperties": {"type": "object"},
            "anyOf": [true, {"type": "object"}],
            "oneOf": [{"type": "object"}],
            "allOf": [{"type": "object"}],
            "not": {"type": "object"},
            "if": {"type": "object"},
            "then": {"type": "object"},
            "else": true
        });
        let object = json!({"type": "object", "properties": {}});
        assert_eq!(
            normalize_schema(&input),
            json!({
                "type": "object",
                "properties": {"a": object},
                "$defs": {"d": {}, "e": object},
                "definitions": {"d": object},
                "patternProperties": {"^x": object},
                "dependentSchemas": {"a": object},
                "dependencies": {"a": ["b"], "b": object},
                "items": [object, {}],
                "prefixItems": [object],
                "contains": object,
                "propertyNames": {},
                "unevaluatedProperties": object,
                "anyOf": [{}, object],
                "oneOf": [object],
                "allOf": [object],
                "not": object,
                "if": object,
                "then": object,
                "else": {}
            })
        );
    }

    #[test]
    fn false_subschemas_and_additional_properties_booleans_are_kept() {
        let input = json!({
            "type": "object",
            "properties": {"never": false, "any": true},
            "additionalProperties": true,
            "items": false
        });
        assert_eq!(
            normalize_schema(&input),
            json!({
                "type": "object",
                "properties": {"never": false, "any": {}},
                "additionalProperties": true,
                "items": false
            })
        );
    }

    #[test]
    fn data_positions_are_never_rewritten() {
        let input = json!({
            "type": "object",
            "properties": {
                "flag": {"type": "boolean", "default": true, "enum": [true, false], "const": true},
                "cfg": {
                    "type": "object",
                    "properties": {},
                    "default": {"type": "object", "pattern": "\\p{L}", "items": true},
                    "examples": [{"type": "object"}, true]
                },
                // A property that is merely *named* like a keyword.
                "pattern": {"type": "string"},
                "default": {"type": "object"}
            }
        });
        let mut expected = input.clone();
        expected["properties"]["default"]["properties"] = json!({});
        assert_eq!(normalize_schema(&input), expected);
    }

    #[test]
    fn uncompilable_patterns_are_removed_and_ordinary_ones_kept() {
        let input = json!({
            "type": "object",
            "properties": {
                "unicode": {"type": "string", "pattern": "^[\\p{L}]+$"},
                "negated": {"type": "string", "pattern": "\\P{Lu}"},
                "nul": {"type": "string", "pattern": "[^\\0]"},
                "lookahead": {"type": "string", "pattern": "^(?=.*\\d).+$"},
                "numeric": {"type": "string", "pattern": 7}
            },
            "patternProperties": {
                "^\\p{L}+$": {"type": "string"},
                "^[a-z]+$": {"type": "string"}
            }
        });
        assert_eq!(
            normalize_schema(&input),
            json!({
                "type": "object",
                "properties": {
                    "unicode": {"type": "string"},
                    "negated": {"type": "string"},
                    "nul": {"type": "string"},
                    "lookahead": {"type": "string", "pattern": "^(?=.*\\d).+$"},
                    "numeric": {"type": "string", "pattern": 7}
                },
                "patternProperties": {"^[a-z]+$": {"type": "string"}}
            })
        );
    }
}
