//! Gemini `generateContent` / `streamGenerateContent`, per notes 15 §6,
//! 07 §2 (schema subset) and 09 §7 (thought signatures).

use super::{
    Errs, Report, event_json, is_base64, is_wrapped_blob, kind, object, only_keys, opt_bool,
    opt_number_in, opt_str, opt_uint, req_nonempty, req_str, req_uint,
};
use serde_json::{Map, Value};
use switchyard_core::SseEvent;

/// Top-level request fields (notes 15 §6.2). `model` and `stream` are not
/// among them: both travel in the URL.
const REQUEST_KEYS: &[&str] = &[
    "contents",
    "systemInstruction",
    "tools",
    "toolConfig",
    "safetySettings",
    "generationConfig",
    "cachedContent",
    "labels",
    "serviceTier",
    "store",
];

const GENERATION_KEYS: &[&str] = &[
    "stopSequences",
    "responseMimeType",
    "responseSchema",
    "responseJsonSchema",
    "_responseJsonSchema",
    "responseModalities",
    "candidateCount",
    "maxOutputTokens",
    "temperature",
    "topP",
    "topK",
    "seed",
    "presencePenalty",
    "frequencyPenalty",
    "responseLogprobs",
    "logprobs",
    "thinkingConfig",
    "speechConfig",
    "imageConfig",
    "mediaResolution",
    "responseFormat",
];

const FINISH_REASONS: &[&str] = &[
    "FINISH_REASON_UNSPECIFIED",
    "STOP",
    "MAX_TOKENS",
    "SAFETY",
    "RECITATION",
    "LANGUAGE",
    "OTHER",
    "BLOCKLIST",
    "PROHIBITED_CONTENT",
    "SPII",
    "MALFORMED_FUNCTION_CALL",
    "IMAGE_SAFETY",
    "IMAGE_PROHIBITED_CONTENT",
    "IMAGE_OTHER",
    "NO_IMAGE",
    "IMAGE_RECITATION",
    "UNEXPECTED_TOOL_CALL",
    "TOO_MANY_TOOL_CALLS",
    "MISSING_THOUGHT_SIGNATURE",
    "MALFORMED_RESPONSE",
    "ESCALATION",
    "PUP_LIMITED_DISABLED",
];

/// The literals Google documents for skipping thought-signature validation.
const BYPASS_SIGNATURES: &[&str] = &[
    "skip_thought_signature_validator",
    "context_engineering_is_the_way_to_go",
];

/// JSON Schema keywords a function declaration must not carry (notes 07 §2.4
/// rules 16–18, and the unions and type arrays rules 13–15 flatten away).
const UNSUPPORTED_SCHEMA_KEYWORDS: &[&str] = &[
    "$schema",
    "$defs",
    "definitions",
    "const",
    "$ref",
    "$id",
    "id",
    "$anchor",
    "$vocabulary",
    "$dynamicRef",
    "$dynamicAnchor",
    "propertyNames",
    "patternProperties",
    "if",
    "then",
    "else",
    "$comment",
    "enumDescriptions",
    "enumTitles",
    "prefill",
    "deprecated",
    "encrypted",
    "additionalItems",
    "unevaluatedProperties",
    "unevaluatedItems",
    "contentSchema",
    "nullable",
    "title",
    "allOf",
    "anyOf",
    "oneOf",
];

/// Data fields of a `Part`; a part has exactly one.
const DATA_FIELDS: &[&str] = &[
    "text",
    "inlineData",
    "fileData",
    "functionCall",
    "functionResponse",
    "executableCode",
    "codeExecutionResult",
    "toolCall",
    "toolResponse",
];
const PART_METADATA: &[&str] = &[
    "thought",
    "thoughtSignature",
    "partMetadata",
    "mediaResolution",
    "videoMetadata",
];

/// Gemini function names: a letter or underscore first, then letters, digits,
/// `_ . : -`, at most 64 characters in all.
fn is_function_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && name.len() <= 64
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// Checks a function-declaration schema against the supported subset.
fn check_schema(schema: &Value, path: &str, errs: &mut Errs) {
    let Some(schema) = schema.as_object() else {
        // `additionalProperties: false`, `items: {}` and friends.
        return;
    };
    for (key, value) in schema {
        if UNSUPPORTED_SCHEMA_KEYWORDS.contains(&key.as_str()) || key.starts_with("x-") {
            errs.push(path, format!("unsupported schema keyword `{key}`"));
        }
        match key.as_str() {
            "type" if !value.is_string() => {
                errs.push(path, "`type` must be a single type name");
            }
            "enum"
                if !value
                    .as_array()
                    .is_some_and(|values| values.iter().all(Value::is_string)) =>
            {
                errs.push(path, "enum values must be strings");
            }
            "type" | "enum" => {}
            // Children of `properties` are names, not keywords.
            "properties" => {
                if let Some(properties) = value.as_object() {
                    for (name, child) in properties {
                        check_schema(child, &format!("{path}.properties.{name}"), errs);
                    }
                }
            }
            "items" | "additionalProperties" | "not" | "contains" => {
                check_schema(value, &format!("{path}.{key}"), errs);
            }
            "prefixItems" => {
                for (i, child) in value.as_array().into_iter().flatten().enumerate() {
                    check_schema(child, &format!("{path}.prefixItems[{i}]"), errs);
                }
            }
            _ => {}
        }
    }
    let type_name = schema
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    if schema.contains_key("items") && type_name.as_deref() != Some("array") {
        errs.push(path, "`items` on a schema that is not an array");
    }
    if type_name.as_deref() == Some("array") && !schema.contains_key("items") {
        errs.push(path, "array schema without `items`");
    }
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        let properties = schema.get("properties").and_then(Value::as_object);
        for name in required.iter().filter_map(Value::as_str) {
            if !properties.is_some_and(|p| p.contains_key(name)) {
                errs.push(
                    path,
                    format!("`required` names `{name}`, which is not a property"),
                );
            }
        }
    }
}

/// What kind of part this is, with the pieces the pairing rules need.
enum PartKind {
    Text,
    Call {
        name: String,
        id: Option<String>,
        signed: bool,
    },
    Response {
        name: String,
        id: Option<String>,
    },
    Other,
}

fn check_part(part: &Value, request: bool, path: &str, errs: &mut Errs) -> PartKind {
    let Some(map) = object(part, path, errs) else {
        return PartKind::Other;
    };
    let data: Vec<&str> = DATA_FIELDS
        .iter()
        .copied()
        .filter(|field| map.contains_key(*field))
        .collect();
    for key in map.keys() {
        if !DATA_FIELDS.contains(&key.as_str()) && !PART_METADATA.contains(&key.as_str()) {
            // Notes 07 REC: camelCase on the wire; `inline_data`,
            // `thought_signature`, `mime_type` are tolerated spellings the
            // gateway has no reason to write.
            errs.push(path, format!("unknown part field `{key}`"));
        }
    }
    if data.len() != 1 {
        errs.push(
            path,
            format!("a part needs exactly one data field, found {data:?}"),
        );
    }
    let signature = match map.get("thoughtSignature") {
        None => None,
        Some(Value::String(signature)) => {
            if BYPASS_SIGNATURES.contains(&signature.as_str()) {
                if !request {
                    errs.push(path, "a validation-bypass literal in a response");
                }
            } else if request && is_wrapped_blob(signature) {
                errs.push(path, "a gateway-wrapped signature reached the upstream");
            } else if !is_base64(signature) {
                // The field is protobuf `bytes`: typed SDKs fail on anything
                // that is not base64.
                errs.push(
                    path,
                    format!("thoughtSignature is not base64: {signature:.40}"),
                );
            }
            Some(signature.as_str())
        }
        Some(other) => {
            errs.push(
                path,
                format!("thoughtSignature must be a string, found {}", kind(other)),
            );
            None
        }
    };
    opt_bool(map, "thought", path, errs);
    match data.first().copied() {
        Some("text") => {
            if !map.get("text").is_some_and(Value::is_string) {
                errs.push(path, "text must be a string");
            }
            PartKind::Text
        }
        Some("inlineData") => {
            let path = format!("{path}.inlineData");
            if let Some(inline) = map.get("inlineData").and_then(|v| object(v, &path, errs)) {
                only_keys(inline, &["mimeType", "data", "displayName"], &path, errs);
                req_nonempty(inline, "mimeType", &path, errs);
                if let Some(data) = req_nonempty(inline, "data", &path, errs)
                    && !is_base64(data)
                {
                    errs.push(&path, "data is not base64");
                }
            }
            PartKind::Other
        }
        Some("fileData") => {
            let path = format!("{path}.fileData");
            if let Some(file) = map.get("fileData").and_then(|v| object(v, &path, errs)) {
                only_keys(file, &["mimeType", "fileUri", "displayName"], &path, errs);
                req_nonempty(file, "fileUri", &path, errs);
            }
            PartKind::Other
        }
        Some("functionCall") => {
            let path = format!("{path}.functionCall");
            let Some(call) = map.get("functionCall").and_then(|v| object(v, &path, errs)) else {
                return PartKind::Other;
            };
            only_keys(call, &["name", "args", "id"], &path, errs);
            let name = req_str(call, "name", &path, errs).unwrap_or("");
            if !is_function_name(name) {
                errs.push(&path, format!("invalid function name `{name}`"));
            }
            match call.get("args") {
                Some(Value::Object(_)) | None => {}
                Some(other) => errs.push(
                    &path,
                    format!("args must be an object, found {}", kind(other)),
                ),
            }
            PartKind::Call {
                name: name.to_string(),
                id: opt_str(call, "id", &path, errs).map(str::to_string),
                signed: signature.is_some(),
            }
        }
        Some("functionResponse") => {
            let path = format!("{path}.functionResponse");
            let Some(response) = map
                .get("functionResponse")
                .and_then(|v| object(v, &path, errs))
            else {
                return PartKind::Other;
            };
            only_keys(
                response,
                &[
                    "name",
                    "response",
                    "id",
                    "parts",
                    "willContinue",
                    "scheduling",
                ],
                &path,
                errs,
            );
            let name = req_str(response, "name", &path, errs).unwrap_or("");
            if !is_function_name(name) {
                errs.push(&path, format!("invalid function name `{name}`"));
            }
            if !response.get("response").is_some_and(Value::is_object) {
                errs.push(&path, "response must be an object");
            }
            if signature.is_some() {
                errs.push(
                    &path,
                    "a functionResponse part must not carry a thought signature",
                );
            }
            PartKind::Response {
                name: name.to_string(),
                id: opt_str(response, "id", &path, errs).map(str::to_string),
            }
        }
        _ => PartKind::Other,
    }
}

/// One content, reduced to what the turn rules look at.
struct Turn {
    role: String,
    calls: Vec<(String, Option<String>)>,
    responses: Vec<(String, Option<String>)>,
}

fn check_contents(contents: &[Value], errs: &mut Errs) {
    let mut turns: Vec<Turn> = Vec::new();
    for (i, content) in contents.iter().enumerate() {
        let path = format!("contents[{i}]");
        let Some(content) = object(content, &path, errs) else {
            continue;
        };
        only_keys(content, &["role", "parts"], &path, errs);
        let role = req_str(content, "role", &path, errs)
            .unwrap_or("")
            .to_string();
        if role != "user" && role != "model" {
            errs.push(&path, format!("role must be user or model, found `{role}`"));
        }
        let mut turn = Turn {
            role,
            calls: Vec::new(),
            responses: Vec::new(),
        };
        let Some(parts) = content.get("parts").and_then(Value::as_array) else {
            errs.push(&path, "parts must be an array");
            turns.push(turn);
            continue;
        };
        if parts.is_empty() {
            errs.push(&path, "parts must not be empty");
        }
        let mut seen_response = false;
        for (j, part) in parts.iter().enumerate() {
            let path = format!("{path}.parts[{j}]");
            match check_part(part, true, &path, errs) {
                PartKind::Text => {
                    // Vertex answers a function response followed by text in
                    // the same turn with a 400 (notes 09 §1.3).
                    if seen_response {
                        errs.push(&path, "text after a functionResponse in the same turn");
                    }
                }
                PartKind::Call { name, id, signed } => {
                    if turn.role != "model" {
                        errs.push(&path, "functionCall outside a model turn");
                    }
                    // Gemini 3 requires the first call of a model step to be
                    // signed (a bypass literal counts); notes 15 §6.5.
                    if turn.calls.is_empty() && !signed {
                        errs.push(
                            &path,
                            "the first functionCall of a model turn carries no thoughtSignature",
                        );
                    }
                    turn.calls.push((name, id));
                }
                PartKind::Response { name, id } => {
                    if turn.role != "user" {
                        errs.push(&path, "functionResponse outside a user turn");
                    }
                    seen_response = true;
                    turn.responses.push((name, id));
                }
                PartKind::Other => {}
            }
        }
        turns.push(turn);
    }
    if turns.first().is_some_and(|turn| turn.role != "user") {
        errs.push(
            "contents[0]",
            "the conversation must start with a user turn",
        );
    }
    for (i, turn) in turns.iter().enumerate() {
        let path = format!("contents[{i}]");
        if i > 0 && turns[i - 1].role == turn.role {
            errs.push(&path, format!("two consecutive `{}` turns", turn.role));
        }
        if !turn.calls.is_empty() {
            // `FC1, FC2` must be answered by `FR1, FR2` together, in order.
            match turns.get(i + 1) {
                Some(next) if next.role == "user" => {
                    if next.responses.len() != turn.calls.len() {
                        errs.push(
                            &path,
                            format!(
                                "{} function call(s) but {} response(s) in the next turn",
                                turn.calls.len(),
                                next.responses.len()
                            ),
                        );
                    }
                    for ((call, call_id), (response, response_id)) in
                        turn.calls.iter().zip(&next.responses)
                    {
                        if call != response {
                            errs.push(
                                &path,
                                format!(
                                    "call `{call}` is answered by a response named `{response}`"
                                ),
                            );
                        }
                        if call_id.is_some() && call_id != response_id {
                            errs.push(
                                &path,
                                format!("call id {call_id:?} is answered with id {response_id:?}"),
                            );
                        }
                    }
                }
                _ => errs.push(&path, "function calls are not answered by the next turn"),
            }
        }
        if !turn.responses.is_empty() {
            let answers_calls = i
                .checked_sub(1)
                .is_some_and(|previous| !turns[previous].calls.is_empty());
            if !answers_calls {
                errs.push(&path, "function responses without calls in the turn before");
            }
        }
    }
}

fn declared_names(root: &Map<String, Value>, errs: &mut Errs) -> Vec<String> {
    let mut names = Vec::new();
    let Some(tools) = root.get("tools") else {
        return names;
    };
    let Some(tools) = tools.as_array() else {
        errs.push("tools", "must be an array");
        return names;
    };
    let mut builtin = false;
    for (i, tool) in tools.iter().enumerate() {
        let path = format!("tools[{i}]");
        let Some(tool) = object(tool, &path, errs) else {
            continue;
        };
        if tool.is_empty() {
            errs.push(&path, "empty tool object");
        }
        for (key, value) in tool {
            if key != "functionDeclarations" {
                builtin = true;
                continue;
            }
            let Some(declarations) = value.as_array() else {
                errs.push(&path, "functionDeclarations must be an array");
                continue;
            };
            if declarations.is_empty() {
                errs.push(&path, "functionDeclarations must not be empty");
            }
            for (j, declaration) in declarations.iter().enumerate() {
                let path = format!("{path}.functionDeclarations[{j}]");
                let Some(declaration) = object(declaration, &path, errs) else {
                    continue;
                };
                only_keys(
                    declaration,
                    &[
                        "name",
                        "description",
                        "parameters",
                        "parametersJsonSchema",
                        "response",
                        "responseJsonSchema",
                        "behavior",
                    ],
                    &path,
                    errs,
                );
                if let Some(name) = req_str(declaration, "name", &path, errs) {
                    if !is_function_name(name) {
                        errs.push(&path, format!("invalid function name `{name}`"));
                    }
                    if names.iter().any(|n| n == name) {
                        errs.push(&path, format!("duplicate declaration of `{name}`"));
                    }
                    names.push(name.to_string());
                }
                if declaration.contains_key("parameters")
                    && declaration.contains_key("parametersJsonSchema")
                {
                    errs.push(
                        &path,
                        "parameters and parametersJsonSchema are mutually exclusive",
                    );
                }
                if let Some(schema) = declaration.get("parametersJsonSchema") {
                    if !schema.is_object() {
                        errs.push(&path, "parametersJsonSchema must be an object");
                    }
                    check_schema(schema, &format!("{path}.parametersJsonSchema"), errs);
                }
            }
        }
    }
    if builtin && !names.is_empty() {
        // Most models reject built-in tools next to function calling.
        errs.push(
            "tools",
            "built-in tools are combined with function declarations",
        );
    }
    names
}

/// Validates a `generateContent` request body as sent to an upstream.
pub fn validate_gemini_request(body: &Value) -> Report {
    let mut errs = Errs::default();
    let Some(root) = object(body, "$", &mut errs) else {
        return errs.finish();
    };
    only_keys(root, REQUEST_KEYS, "$", &mut errs);
    match root.get("contents").and_then(Value::as_array) {
        Some(contents) => {
            if contents.is_empty() {
                errs.push("contents", "must not be empty");
            }
            check_contents(contents, &mut errs);
        }
        None => errs.push("contents", "must be an array"),
    }
    if let Some(system) = root.get("systemInstruction")
        && let Some(system) = object(system, "systemInstruction", &mut errs)
    {
        only_keys(system, &["role", "parts"], "systemInstruction", &mut errs);
        match system.get("parts").and_then(Value::as_array) {
            Some(parts) if !parts.is_empty() => {
                for (i, part) in parts.iter().enumerate() {
                    let text_only = part.as_object().is_some_and(|p| {
                        p.len() == 1 && p.get("text").is_some_and(Value::is_string)
                    });
                    if !text_only {
                        errs.push(
                            &format!("systemInstruction.parts[{i}]"),
                            "system instructions are text only",
                        );
                    }
                }
            }
            _ => errs.push("systemInstruction", "parts must be a non-empty array"),
        }
    }
    let declared = declared_names(root, &mut errs);
    if let Some(config) = root.get("toolConfig")
        && let Some(config) = object(config, "toolConfig", &mut errs)
    {
        let path = "toolConfig.functionCallingConfig";
        if let Some(calling) = config
            .get("functionCallingConfig")
            .and_then(|c| object(c, path, &mut errs))
        {
            only_keys(calling, &["mode", "allowedFunctionNames"], path, &mut errs);
            let mode = calling.get("mode").and_then(Value::as_str).unwrap_or("");
            if !["AUTO", "ANY", "NONE", "VALIDATED"].contains(&mode) {
                errs.push(path, format!("invalid mode `{mode}`"));
            }
            if declared.is_empty() {
                errs.push(path, "a calling mode without function declarations");
            }
            if let Some(allowed) = calling.get("allowedFunctionNames") {
                if mode != "ANY" {
                    errs.push(path, "allowedFunctionNames requires mode ANY");
                }
                for name in allowed.as_array().into_iter().flatten() {
                    match name.as_str() {
                        Some(name) if declared.iter().any(|d| d == name) => {}
                        other => {
                            errs.push(path, format!("allows {other:?}, which is not declared"))
                        }
                    }
                }
            }
        }
    }
    if let Some(config) = root.get("generationConfig")
        && let Some(config) = object(config, "generationConfig", &mut errs)
    {
        let path = "generationConfig";
        only_keys(config, GENERATION_KEYS, path, &mut errs);
        if config.is_empty() {
            errs.push(path, "must not be an empty object");
        }
        opt_number_in(config, "temperature", 0.0, 2.0, path, &mut errs);
        opt_number_in(config, "topP", 0.0, 1.0, path, &mut errs);
        opt_uint(config, "topK", path, &mut errs);
        if opt_uint(config, "maxOutputTokens", path, &mut errs) == Some(0) {
            errs.push(path, "maxOutputTokens must be positive");
        }
        opt_uint(config, "candidateCount", path, &mut errs);
        if let Some(seed) = config.get("seed")
            && seed.as_i64().is_none_or(|s| i32::try_from(s).is_err())
        {
            errs.push(path, "seed must be a 32-bit integer");
        }
        if let Some(stops) = config.get("stopSequences") {
            match stops.as_array() {
                Some(stops) => {
                    if stops.is_empty() || stops.len() > 5 {
                        errs.push(path, "stopSequences holds between 1 and 5 entries");
                    }
                    if !stops
                        .iter()
                        .all(|s| s.as_str().is_some_and(|s| !s.is_empty()))
                    {
                        errs.push(path, "stop sequences must be non-empty strings");
                    }
                }
                None => errs.push(path, "stopSequences must be an array"),
            }
        }
        let mime = opt_str(config, "responseMimeType", path, &mut errs);
        if let Some(mime) = mime
            && !["application/json", "text/plain", "text/x.enum"].contains(&mime)
        {
            errs.push(path, format!("invalid responseMimeType `{mime}`"));
        }
        let schemas = [
            "responseSchema",
            "responseJsonSchema",
            "_responseJsonSchema",
        ]
        .iter()
        .filter(|key| config.contains_key(**key))
        .count();
        if schemas > 1 {
            errs.push(path, "more than one response schema");
        }
        if schemas > 0 && mime != Some("application/json") && mime != Some("text/x.enum") {
            errs.push(
                path,
                "a response schema needs responseMimeType application/json",
            );
        }
        if let Some(thinking) = config.get("thinkingConfig")
            && let Some(thinking) = object(thinking, "generationConfig.thinkingConfig", &mut errs)
        {
            let path = "generationConfig.thinkingConfig";
            only_keys(
                thinking,
                &["includeThoughts", "thinkingBudget", "thinkingLevel"],
                path,
                &mut errs,
            );
            if thinking.is_empty() {
                errs.push(path, "must not be an empty object");
            }
            // Level and budget "can't be used at the same time" (notes 15 §6.4).
            if thinking.contains_key("thinkingBudget") && thinking.contains_key("thinkingLevel") {
                errs.push(path, "thinkingBudget and thinkingLevel are both set");
            }
            if let Some(budget) = thinking.get("thinkingBudget")
                && budget.as_i64().is_none_or(|b| b < -1)
            {
                errs.push(path, format!("invalid thinkingBudget {budget}"));
            }
            if let Some(level) = opt_str(thinking, "thinkingLevel", path, &mut errs)
                && !["minimal", "low", "medium", "high"]
                    .contains(&level.to_ascii_lowercase().as_str())
            {
                errs.push(path, format!("invalid thinkingLevel `{level}`"));
            }
            opt_bool(thinking, "includeThoughts", path, &mut errs);
        }
    }
    opt_bool(root, "store", "$", &mut errs);
    errs.finish()
}

/// Checks `usageMetadata`: thoughts are counted next to the candidates, and
/// the total is the sum of all three (notes 15 §6.3).
fn check_usage(usage: &Value, path: &str, complete: bool, errs: &mut Errs) {
    let Some(usage) = object(usage, path, errs) else {
        return;
    };
    let prompt = opt_uint(usage, "promptTokenCount", path, errs).unwrap_or(0);
    let cached = opt_uint(usage, "cachedContentTokenCount", path, errs).unwrap_or(0);
    let candidates = opt_uint(usage, "candidatesTokenCount", path, errs).unwrap_or(0);
    let thoughts = opt_uint(usage, "thoughtsTokenCount", path, errs).unwrap_or(0);
    let tool_use = opt_uint(usage, "toolUsePromptTokenCount", path, errs).unwrap_or(0);
    if cached > prompt {
        errs.push(
            path,
            format!("cachedContentTokenCount {cached} exceeds promptTokenCount {prompt}"),
        );
    }
    if complete {
        let total = req_uint(usage, "totalTokenCount", path, errs).unwrap_or(0);
        let sum = prompt + candidates + thoughts + tool_use;
        if total != sum {
            errs.push(
                path,
                format!("totalTokenCount {total} != {prompt} + {candidates} + {thoughts}"),
            );
        }
    }
}

/// Checks a candidate; returns whether it carries a `finishReason`.
fn check_candidate(candidate: &Value, path: &str, errs: &mut Errs) -> bool {
    let Some(candidate) = object(candidate, path, errs) else {
        return false;
    };
    if let Some(content) = candidate.get("content") {
        let path = format!("{path}.content");
        if let Some(content) = object(content, &path, errs) {
            if content.get("role").and_then(Value::as_str) != Some("model") {
                errs.push(&path, "role must be `model`");
            }
            match content.get("parts") {
                Some(Value::Array(parts)) => {
                    for (i, part) in parts.iter().enumerate() {
                        let path = format!("{path}.parts[{i}]");
                        if let PartKind::Response { .. } = check_part(part, false, &path, errs) {
                            errs.push(&path, "functionResponse in a model answer");
                        }
                    }
                }
                None => {}
                Some(other) => errs.push(
                    &path,
                    format!("parts must be an array, found {}", kind(other)),
                ),
            }
        }
    }
    match candidate.get("finishReason") {
        None => false,
        Some(Value::String(reason)) => {
            if !FINISH_REASONS.contains(&reason.as_str()) {
                errs.push(path, format!("invalid finishReason `{reason}`"));
            }
            true
        }
        Some(other) => {
            errs.push(
                path,
                format!("finishReason must be a string, found {}", kind(other)),
            );
            false
        }
    }
}

/// Validates a complete `GenerateContentResponse`.
pub fn validate_gemini_response(body: &Value) -> Report {
    let mut errs = Errs::default();
    let Some(root) = object(body, "$", &mut errs) else {
        return errs.finish();
    };
    match root.get("candidates").and_then(Value::as_array) {
        Some(candidates) if !candidates.is_empty() => {
            for (i, candidate) in candidates.iter().enumerate() {
                let path = format!("candidates[{i}]");
                if !check_candidate(candidate, &path, &mut errs) {
                    errs.push(&path, "a complete response needs a finishReason");
                }
                if candidate.get("content").is_none() {
                    errs.push(&path, "missing content");
                }
            }
        }
        // A blocked prompt is the one answer without candidates.
        _ => {
            if !root.contains_key("promptFeedback") {
                errs.push(
                    "candidates",
                    "must be a non-empty array (or promptFeedback must explain why not)",
                );
            }
        }
    }
    match root.get("usageMetadata") {
        Some(usage) => check_usage(usage, "usageMetadata", true, &mut errs),
        None => errs.push("usageMetadata", "missing"),
    }
    req_nonempty(root, "modelVersion", "$", &mut errs);
    req_nonempty(root, "responseId", "$", &mut errs);
    errs.finish()
}

/// Validates a `streamGenerateContent?alt=sse` stream (notes 15 §6.1, §6.3):
/// data-only events, each a `GenerateContentResponse`; the last one carries
/// the finish reason and the final usage. A stream may instead end with an
/// `{"error":{…}}` chunk (notes 15 §6.6).
pub fn validate_gemini_stream(events: &[SseEvent]) -> Report {
    let mut errs = Errs::default();
    if events.is_empty() {
        errs.push("stream", "no events");
        return errs.finish();
    }
    let mut finished = false;
    let mut failed = false;
    let last = events.len() - 1;
    for (i, event) in events.iter().enumerate() {
        let path = format!("event[{i}]");
        if event.event.is_some() {
            errs.push(&path, "Gemini streams use data-only events");
        }
        if event.data.trim() == "[DONE]" {
            errs.push(&path, "Gemini streams have no [DONE] sentinel");
            continue;
        }
        if finished || failed {
            errs.push(&path, "chunk after the end of the stream");
            continue;
        }
        let Some(chunk) = event_json(event, i, &mut errs) else {
            continue;
        };
        let Some(chunk) = object(&chunk, &path, &mut errs) else {
            continue;
        };
        if let Some(error) = chunk.get("error") {
            failed = true;
            if !error.get("code").is_some_and(Value::is_u64) {
                errs.push(&path, "error.code must be an integer");
            }
            if error
                .get("message")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                errs.push(&path, "error.message is missing");
            }
            if error
                .get("status")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                errs.push(&path, "error.status is missing");
            }
            continue;
        }
        let mut carries_finish = false;
        let mut carries_parts = false;
        match chunk.get("candidates") {
            Some(Value::Array(candidates)) => {
                for (j, candidate) in candidates.iter().enumerate() {
                    carries_finish |=
                        check_candidate(candidate, &format!("{path}.candidates[{j}]"), &mut errs);
                    carries_parts |= candidate
                        .get("content")
                        .and_then(|c| c.get("parts"))
                        .and_then(Value::as_array)
                        .is_some_and(|parts| !parts.is_empty());
                }
            }
            None => {}
            Some(other) => errs.push(
                &path,
                format!("candidates must be an array, found {}", kind(other)),
            ),
        }
        let usage = chunk.get("usageMetadata");
        if let Some(usage) = usage {
            check_usage(
                usage,
                &format!("{path}.usageMetadata"),
                carries_finish,
                &mut errs,
            );
        }
        if !carries_finish
            && !carries_parts
            && usage.is_none()
            && !chunk.contains_key("promptFeedback")
        {
            errs.push(
                &path,
                "chunk carries nothing (no parts, finish reason or usage)",
            );
        }
        if carries_finish || chunk.contains_key("promptFeedback") {
            finished = true;
            if i != last {
                errs.push(&path, "the finish reason is not on the last chunk");
            }
            if usage.is_none() {
                errs.push(&path, "the final chunk carries no usageMetadata");
            }
            req_nonempty(chunk, "responseId", &path, &mut errs);
            req_nonempty(chunk, "modelVersion", &path, &mut errs);
        }
    }
    if !finished && !failed {
        errs.push("stream", "no chunk carries a finishReason");
    }
    errs.finish()
}
