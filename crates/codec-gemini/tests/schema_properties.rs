//! Properties of the schema cleaner that must hold for *any* input, checked
//! on a few thousand generated schemas (deterministic generator, no I/O):
//!
//! * cleaning is idempotent;
//! * the result never contains a keyword Gemini rejects;
//! * a node with an `enum` is a string enum;
//! * the result is never more than a small multiple of the input, and never
//!   deeper than a JSON parser accepts.

use serde_json::{Map, Value, json};
use switchyard_codec_gemini::{sanitize_schema, sanitize_schema_legacy};

/// xorshift64*: small, deterministic, good enough to shuffle schema shapes.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<'a>(&mut self, options: &[&'a str]) -> &'a str {
        options[self.below(options.len() as u64) as usize]
    }
}

const TYPES: [&str; 7] = [
    "string", "integer", "number", "boolean", "object", "array", "null",
];
const DEFS: [&str; 4] = ["A", "B", "C", "D"];

fn scalar(rng: &mut Rng) -> Value {
    match rng.below(5) {
        0 => json!(rng.below(100)),
        1 => json!(rng.chance(50)),
        2 => Value::Null,
        3 => json!(1.5),
        _ => json!(rng.pick(&["a", "b", "red", "x y"])),
    }
}

fn type_value(rng: &mut Rng) -> Value {
    if rng.chance(20) {
        let count = 1 + rng.below(3);
        Value::Array((0..count).map(|_| json!(rng.pick(&TYPES))).collect())
    } else {
        json!(rng.pick(&TYPES))
    }
}

fn schema(rng: &mut Rng, depth: u32) -> Value {
    if depth == 0 || rng.chance(15) {
        return match rng.below(4) {
            0 => json!({"$ref": format!("#/$defs/{}", rng.pick(&DEFS))}),
            1 => json!(true),
            2 => json!({}),
            _ => json!({"type": rng.pick(&TYPES)}),
        };
    }
    let mut node = Map::new();
    if rng.chance(75) {
        node.insert("type".into(), type_value(rng));
    }
    if rng.chance(30) {
        node.insert(
            "description".into(),
            json!(rng.pick(&["A thing", "Other", ""])),
        );
    }
    if rng.chance(35) {
        let mut properties = Map::new();
        for _ in 0..rng.below(4) {
            let name = rng.pick(&["a", "b", "type", "enum", "items", "$ref", "x-k", "_"]);
            properties.insert(name.into(), schema(rng, depth - 1));
        }
        node.insert("properties".into(), Value::Object(properties));
        if rng.chance(50) {
            let required = match rng.below(3) {
                0 => json!(["a", "zzz"]),
                1 => json!(["b", "type"]),
                _ => json!(true),
            };
            node.insert("required".into(), required);
        }
    }
    if rng.chance(20) {
        node.insert("items".into(), schema(rng, depth - 1));
    }
    if rng.chance(20) {
        let count = 1 + rng.below(4);
        node.insert(
            "enum".into(),
            Value::Array((0..count).map(|_| scalar(rng)).collect()),
        );
    }
    if rng.chance(10) {
        node.insert("const".into(), scalar(rng));
    }
    for union in ["anyOf", "oneOf", "allOf"] {
        if rng.chance(15) {
            let count = 1 + rng.below(3);
            node.insert(
                union.into(),
                Value::Array((0..count).map(|_| schema(rng, depth - 1)).collect()),
            );
        }
    }
    for branch in ["then", "else", "not", "additionalProperties"] {
        if rng.chance(8) {
            node.insert(branch.into(), schema(rng, depth - 1));
        }
    }
    if rng.chance(10) {
        // A stray object-valued key: a bare property map.
        node.insert("stray".into(), schema(rng, depth - 1));
    }
    if rng.chance(15) {
        node.insert("$ref".into(), json!(format!("#/$defs/{}", rng.pick(&DEFS))));
    }
    for (keyword, value) in [
        ("nullable", json!(true)),
        ("title", json!("T")),
        ("format", json!("date-time")),
        ("minLength", json!(1)),
        ("default", json!({"$ref": "#/keep", "const": 1})),
        ("pattern", json!("^a")),
        ("x-vendor", json!({"a": 1})),
        ("$schema", json!("http://json-schema.org/draft-07/schema#")),
        ("propertyNames", json!({"pattern": "^a"})),
    ] {
        if rng.chance(8) {
            node.insert(keyword.into(), value);
        }
    }
    Value::Object(node)
}

fn generate(seed: u64) -> Value {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut root = match schema(&mut rng, 4) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let mut defs = Map::new();
    for name in DEFS {
        if rng.chance(80) {
            defs.insert(name.into(), schema(&mut rng, 3));
        }
    }
    root.insert("$defs".into(), Value::Object(defs));
    Value::Object(root)
}

fn depth_of(value: &Value) -> usize {
    match value {
        Value::Array(list) => 1 + list.iter().map(depth_of).max().unwrap_or(0),
        Value::Object(map) => 1 + map.values().map(depth_of).max().unwrap_or(0),
        _ => 0,
    }
}

/// Walks the schema positions of a cleaned schema and checks the invariants.
fn check_node(node: &Value, input: &Value) {
    let Value::Object(map) = node else {
        return;
    };
    for rejected in [
        "$ref",
        "$defs",
        "definitions",
        "$schema",
        "const",
        "nullable",
        "title",
        "allOf",
        "then",
        "else",
        "if",
        "propertyNames",
        "patternProperties",
    ] {
        assert!(
            !map.contains_key(rejected),
            "`{rejected}` survived in {node} (input {input})"
        );
    }
    assert!(
        !map.keys().any(|key| key.starts_with("x-")),
        "an extension keyword survived in {node} (input {input})"
    );
    if let Some(Value::Array(values)) = map.get("enum") {
        assert!(
            values.iter().all(Value::is_string),
            "non-string enum value in {node} (input {input})"
        );
        assert_eq!(
            map.get("type"),
            Some(&json!("string")),
            "enum node is not a string in {node} (input {input})"
        );
    }
    if let Some(ty) = map.get("type") {
        assert!(ty.is_string(), "`type` is not a single name in {node}");
    }
    if let Some(Value::Object(properties)) = map.get("properties") {
        properties
            .values()
            .for_each(|property| check_node(property, input));
    }
    match map.get("items") {
        Some(Value::Array(tuple)) => tuple.iter().for_each(|item| check_node(item, input)),
        Some(single) => check_node(single, input),
        None => {}
    }
    for union in ["anyOf", "oneOf"] {
        if let Some(Value::Array(branches)) = map.get(union) {
            branches.iter().for_each(|branch| check_node(branch, input));
        }
    }
}

/// Checks idempotence over the generated schemas and reports the smallest
/// counter-example, which is the one worth reading.
fn assert_idempotent(clean: fn(&Value) -> Value) {
    let mut smallest: Option<(usize, u64, Value, Value, Value)> = None;
    for seed in 1..=SEEDS {
        let input = generate(seed);
        let once = clean(&input);
        let twice = clean(&once);
        if once != twice {
            let size = input.to_string().len();
            if smallest.as_ref().is_none_or(|(best, ..)| size < *best) {
                smallest = Some((size, seed, input, once, twice));
            }
        }
    }
    if let Some((_, seed, input, once, twice)) = smallest {
        panic!("not idempotent for seed {seed}\n input: {input}\n once:  {once}\n twice: {twice}");
    }
}

const SEEDS: u64 = 1500;

#[test]
fn generated_schemas_are_cleaned_idempotently() {
    assert_idempotent(sanitize_schema);
}

#[test]
fn generated_schemas_are_cleaned_idempotently_in_the_legacy_dialect() {
    assert_idempotent(sanitize_schema_legacy);
}

#[test]
fn generated_schemas_come_out_valid_for_gemini() {
    for seed in 1..=SEEDS {
        let input = generate(seed);
        check_node(&sanitize_schema(&input), &input);
        check_node(&sanitize_schema_legacy(&input), &input);
    }
}

#[test]
fn generated_schemas_never_grow_or_deepen_out_of_proportion() {
    for seed in 1..=SEEDS {
        let input = generate(seed);
        let cleaned = sanitize_schema(&input);
        let size_in = input.to_string().len();
        let size_out = cleaned.to_string().len();
        assert!(
            size_out <= size_in * 8 + 256,
            "seed {seed}: {size_in} bytes became {size_out}"
        );
        assert!(depth_of(&cleaned) <= 100, "seed {seed}: too deep");
    }
}
