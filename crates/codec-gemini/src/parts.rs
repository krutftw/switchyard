//! Conversions between Gemini `Part` objects and canonical parts that are
//! shared by the request, response and stream code.

use crate::util::{pick, pick_str, sanitize_function_name};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use serde_json::{Map, Value, json};
use std::borrow::Cow;
use switchyard_core::ir::{
    MediaPart, MediaSource, OpaquePart, Part, Signature, ToolResult, parse_data_uri,
};
use switchyard_core::{Protocol, sig};

/// The value Google documents for skipping thought-signature validation on a
/// `functionCall` part that did not originate from Gemini.
pub const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// The second documented bypass literal. Recognised, never emitted.
const ALT_SKIP_SIGNATURE: &str = "context_engineering_is_the_way_to_go";

/// Whether `signature` is one of the documented validation-bypass literals
/// rather than a blob issued by the model.
pub(crate) fn is_bypass_signature(signature: &str) -> bool {
    signature == SKIP_SIGNATURE || signature == ALT_SKIP_SIGNATURE
}

/// The raw thought signature of a part, wherever a client may have put it.
/// The canonical place is a top-level `thoughtSignature`; older clients and
/// OpenAI-compat shims nest it.
pub(crate) fn raw_signature(part: &Value) -> Option<&str> {
    const SPELLINGS: [&str; 2] = ["thoughtSignature", "thought_signature"];
    let direct = pick_str(part, &SPELLINGS);
    let nested = || {
        [
            "functionCall",
            "function_call",
            "functionResponse",
            "function_response",
        ]
        .iter()
        .find_map(|key| part.get(*key).and_then(|inner| pick_str(inner, &SPELLINGS)))
    };
    let extra = || {
        part.get("extra_content")
            .and_then(|e| e.get("google"))
            .and_then(|g| g.get("thought_signature"))
            .and_then(Value::as_str)
    };
    direct
        .or_else(nested)
        .or_else(extra)
        .filter(|s| !s.is_empty())
}

/// What every armoured signature starts with: the base64 of `sy1.` (the
/// first three bytes and the top bits of the dot), which is the same in the
/// standard and the URL-safe alphabet.
const ARMOUR_PREFIX: &str = "c3kxL";

/// Marker put in front of a foreign redacted-reasoning payload before it is
/// tagged and armoured for a client. A Gemini part has one opaque slot
/// (`thoughtSignature`) and nothing that says "the provider withheld this
/// reasoning; the blob *is* the reasoning" (Anthropic `redacted_thinking`, a
/// Chat relay's `reasoning.encrypted`), so that fact has to ride inside the
/// blob or the payload comes back as the signature of an empty thought, which
/// the vendor that issued it refuses. Base64 never contains `:`, so the
/// marker cannot collide with a blob.
const REDACTED_MARKER: &str = "redacted:";

/// Renders a signature for a Gemini client. `redacted` says the signature
/// is the payload of withheld reasoning (see [`REDACTED_MARKER`]).
///
/// `Part.thoughtSignature` is a protobuf `bytes` field, so on the wire it
/// must be base64: typed SDKs (Go, Python) decode it while parsing and fail
/// the whole response otherwise. A Gemini blob is base64 already and is
/// delivered as it is. A blob of another vendor is first tagged with its
/// origin by [`sig::encode_for_client`] (`sy1.<tag>.<blob>`); the dots in
/// that are not base64 characters, so the tagged string is armoured as
/// standard base64 on top. [`signature_from_client`] undoes both layers and
/// [`split_redacted`] the marker.
pub(crate) fn signature_for_client(signature: &Signature, redacted: bool) -> String {
    if signature.valid_for(Protocol::Gemini) {
        return sig::encode_for_client(signature, Protocol::Gemini);
    }
    let tagged = if redacted {
        let marked = Signature::new(
            signature.origin,
            format!("{REDACTED_MARKER}{}", signature.data),
        );
        sig::encode_for_client(&marked, Protocol::Gemini)
    } else {
        sig::encode_for_client(signature, Protocol::Gemini)
    };
    STANDARD.encode(tagged)
}

/// Takes the [`REDACTED_MARKER`] off a signature read from a client request.
/// The flag says the signature is the payload of withheld reasoning. Only a
/// blob of another vendor can carry the marker.
pub(crate) fn split_redacted(mut signature: Signature) -> (Signature, bool) {
    if !signature.valid_for(Protocol::Gemini)
        && let Some(payload) = signature.data.strip_prefix(REDACTED_MARKER)
    {
        signature.data = payload.to_string();
        return (signature, true);
    }
    (signature, false)
}

/// The tagged signature (`sy1.<tag>.<blob>`) inside an armoured one, or
/// `None` when `raw` is not an armoured signature. Clients re-encode `bytes`
/// fields their own way, so the standard and the URL-safe alphabet are both
/// accepted, padded or not.
fn dearmour(raw: &str) -> Option<String> {
    if !raw.starts_with(ARMOUR_PREFIX) {
        return None;
    }
    let body = raw.trim_end_matches('=');
    let bytes = if body.contains(['-', '_']) {
        URL_SAFE_NO_PAD.decode(body)
    } else {
        STANDARD_NO_PAD.decode(body)
    }
    .ok()?;
    let tagged = String::from_utf8(bytes).ok()?;
    let mut rest = tagged.strip_prefix(sig::PREFIX)?.chars();
    let tag = rest.next().and_then(Protocol::from_tag);
    (tag.is_some() && rest.next() == Some('.')).then_some(tagged)
}

/// Parses a signature string received from a Gemini client: an armoured or a
/// plainly tagged blob of another vendor keeps its real origin, anything else
/// is Gemini's own.
pub(crate) fn signature_from_client(raw: &str) -> Signature {
    match dearmour(raw) {
        Some(tagged) => sig::decode_from_client(&tagged, Protocol::Gemini),
        None => sig::decode_from_client(raw, Protocol::Gemini),
    }
}

/// The signature of a part in a **client request**. Bypass literals are not
/// signatures; wrapped foreign blobs keep their real origin.
pub(crate) fn request_signature(part: &Value) -> Option<Signature> {
    raw_signature(part)
        .filter(|s| !is_bypass_signature(s))
        .map(signature_from_client)
}

/// Removes the thought signature of a part from every place
/// [`raw_signature`] reads it from.
pub(crate) fn remove_signature(part: &mut Map<String, Value>) {
    const SPELLINGS: [&str; 2] = ["thoughtSignature", "thought_signature"];
    for key in SPELLINGS {
        part.shift_remove(key);
    }
    for holder in [
        "functionCall",
        "function_call",
        "functionResponse",
        "function_response",
    ] {
        if let Some(Value::Object(inner)) = part.get_mut(holder) {
            for key in SPELLINGS {
                inner.shift_remove(key);
            }
        }
    }
    if let Some(Value::Object(extra)) = part.get_mut("extra_content") {
        if let Some(Value::Object(google)) = extra.get_mut("google") {
            google.shift_remove("thought_signature");
            if google.is_empty() {
                extra.shift_remove("google");
            }
        }
        if extra.is_empty() {
            part.shift_remove("extra_content");
        }
    }
}

/// The signature of a part in an **upstream response**.
pub(crate) fn response_signature(part: &Value) -> Option<Signature> {
    raw_signature(part)
        .filter(|s| !is_bypass_signature(s))
        .map(|s| Signature::new(Protocol::Gemini, s))
}

/// The blob to replay to a Gemini upstream, if `signature` may go there.
pub(crate) fn upstream_signature(signature: Option<&Signature>) -> Option<&str> {
    signature
        .filter(|s| {
            s.valid_for(Protocol::Gemini) && !s.data.is_empty() && !is_bypass_signature(&s.data)
        })
        .map(|s| s.data.as_str())
}

/// Explicit call id of a `functionCall` / `functionResponse` object.
pub(crate) fn explicit_id(call: &Value) -> Option<&str> {
    pick_str(call, &["id", "call_id", "callId"])
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

/// `name` of a `functionCall` / `functionResponse` object (may be empty).
pub(crate) fn call_name(call: &Value) -> &str {
    pick_str(call, &["name"]).map(str::trim).unwrap_or("")
}

/// The arguments of a `functionCall` as JSON text. Absent arguments are `{}`.
pub(crate) fn call_args_text(call: &Value) -> String {
    match pick(call, &["args", "arguments"]) {
        None => "{}".to_string(),
        // Some compatible servers send the arguments pre-serialised.
        Some(Value::String(text)) if text.trim().is_empty() => "{}".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

fn media_kind(mime: &str, media: MediaPart) -> Part {
    let mime = mime.to_ascii_lowercase();
    if mime.starts_with("image/") {
        Part::Image(media)
    } else if mime.starts_with("audio/") {
        Part::Audio(media)
    } else {
        Part::Document(media)
    }
}

/// MIME type suggested by a file name or URL path.
fn mime_from_extension(name: &str) -> Option<&'static str> {
    let path = name.split(['?', '#']).next().unwrap_or(name);
    let (_, extension) = path.rsplit_once('.')?;
    Some(match extension.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "json" => "application/json",
        "xml" => "text/xml",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "m4a" => "audio/mp4",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mpeg" | "mpg" => "video/mpeg",
        _ => return None,
    })
}

/// Brings the base64 of a `bytes` field into the canonical form: standard
/// alphabet, padded.
///
/// Gemini's JSON parser also takes the URL-safe alphabet and unpadded input,
/// and Google's Python SDK sends exactly that (`-` / `_`). The canonical model
/// promises standard base64, which is all the other vendors accept. Data that
/// is not plain base64 (line breaks, stray characters) is passed on untouched.
fn standard_base64(data: &str) -> Cow<'_, str> {
    let mut url_safe = false;
    for byte in data.bytes() {
        match byte {
            b'-' | b'_' => url_safe = true,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' | b'=' => {}
            _ => return Cow::Borrowed(data),
        }
    }
    let missing = match data.len() % 4 {
        2 => 2,
        3 => 1,
        _ => 0,
    };
    if !url_safe && missing == 0 {
        return Cow::Borrowed(data);
    }
    let mut fixed = String::with_capacity(data.len() + missing);
    fixed.extend(data.chars().map(|c| match c {
        '-' => '+',
        '_' => '/',
        other => other,
    }));
    fixed.extend(std::iter::repeat_n('=', missing));
    Cow::Owned(fixed)
}

/// `inlineData {mimeType, data}` -> media part. `None` when there is no data.
pub(crate) fn decode_inline_data(inline: &Value) -> Option<Part> {
    let data = pick_str(inline, &["data"]).filter(|d| !d.is_empty())?;
    let mime = pick_str(inline, &["mimeType", "mime_type"])
        .filter(|m| !m.is_empty())
        .unwrap_or("application/octet-stream");
    let mut media = MediaPart::base64(mime, standard_base64(data));
    media.filename = pick_str(inline, &["displayName", "display_name"]).map(str::to_owned);
    Some(media_kind(mime, media))
}

/// A `fileUri` that only means something to Google: a Files API handle or a
/// Cloud Storage object. Plain web URLs are portable.
fn is_provider_uri(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    let web = lower.starts_with("http://") || lower.starts_with("https://");
    !web || (lower.contains("generativelanguage.googleapis.com/") && lower.contains("/files/"))
}

/// `fileData {mimeType, fileUri}` -> media part. `None` without a URI.
pub(crate) fn decode_file_data(file: &Value) -> Option<Part> {
    let uri = pick_str(file, &["fileUri", "file_uri"]).filter(|u| !u.is_empty())?;
    let mime = pick_str(file, &["mimeType", "mime_type"]).filter(|m| !m.is_empty());
    let source = if is_provider_uri(uri) {
        MediaSource::FileRef {
            id: uri.to_string(),
        }
    } else {
        MediaSource::Url {
            url: uri.to_string(),
        }
    };
    let media = MediaPart {
        source,
        media_type: mime.map(str::to_owned),
        filename: pick_str(file, &["displayName", "display_name"]).map(str::to_owned),
        detail: None,
        cache_control: None,
    };
    let kind = mime
        .or_else(|| mime_from_extension(uri))
        .unwrap_or("application/octet-stream");
    Some(media_kind(kind, media))
}

/// Media part -> `inlineData` / `fileData` part.
///
/// Provider file handles are only emitted when `native` is true (the request
/// came from a Gemini client, so the handle is Google's); a handle issued by
/// another vendor means nothing here and the part is dropped.
///
/// A URL whose MIME type is neither declared nor visible in its extension:
/// for a `native` part nothing is invented and `fileData` goes out without a
/// `mimeType`, exactly as the Gemini client wrote it (the documented way to
/// pass a YouTube link); the part's canonical kind says nothing there, since
/// it was itself derived from the missing type. For other vendors' parts the
/// kind is real information (an OpenAI `image_url`, an Anthropic `document`)
/// and its usual type is used.
pub(crate) fn encode_media(part: &Part, native: bool) -> Option<Value> {
    let (media, fallback) = match part {
        Part::Image(media) => (media, "image/jpeg"),
        Part::Audio(media) => (media, "audio/mpeg"),
        Part::Document(media) => (media, "application/pdf"),
        _ => return None,
    };
    let declared = media.media_type.as_deref().filter(|m| !m.is_empty());
    let from_name = || media.filename.as_deref().and_then(mime_from_extension);
    match &media.source {
        MediaSource::Base64 { data } => {
            let mime = declared.or_else(from_name).unwrap_or(fallback);
            Some(json!({"inlineData": {"mimeType": mime, "data": data}}))
        }
        MediaSource::Url { url } => {
            if let Some((mime, data)) = parse_data_uri(url) {
                return Some(json!({"inlineData": {"mimeType": mime, "data": data}}));
            }
            let mime = declared
                .or_else(|| mime_from_extension(url))
                .or_else(from_name)
                .or((!native).then_some(fallback));
            let mut file = Map::new();
            if let Some(mime) = mime {
                file.insert("mimeType".to_string(), Value::String(mime.to_string()));
            }
            file.insert("fileUri".to_string(), Value::String(url.clone()));
            Some(json!({"fileData": file}))
        }
        MediaSource::FileRef { id } => {
            if !native {
                return None;
            }
            let mut file = Map::new();
            if let Some(mime) = declared.or_else(|| mime_from_extension(id)) {
                file.insert("mimeType".to_string(), Value::String(mime.to_string()));
            }
            file.insert("fileUri".to_string(), Value::String(id.clone()));
            Some(json!({"fileData": file}))
        }
    }
}

fn plain(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Decodes the payload of a `functionResponse` into tool-result content.
///
/// Google's convention is `{"output": …}` / `{"error": …}`; this gateway (and
/// the project it descends from) writes `{"result": …}`. A response object
/// that is exactly one of those wrappers is unwrapped, anything else is kept
/// as JSON text. Multimodal `functionResponse.parts` become media parts.
pub(crate) fn decode_function_response(response: &Value) -> (Vec<Part>, bool) {
    let mut is_error = false;
    let text = match pick(response, &["response"]) {
        Some(Value::Object(object)) if object.len() == 1 => {
            if let Some(value) = object.get("result").or_else(|| object.get("output")) {
                plain(value)
            } else if let Some(value) = object.get("error") {
                is_error = true;
                plain(value)
            } else {
                Value::Object(object.clone()).to_string()
            }
        }
        Some(other) => plain(other),
        None => String::new(),
    };
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(Part::text(text));
    }
    if let Some(Value::Array(parts)) = pick(response, &["parts"]) {
        for part in parts {
            if let Some(media) =
                pick(part, &["inlineData", "inline_data"]).and_then(decode_inline_data)
            {
                content.push(media);
            } else if let Some(media) =
                pick(part, &["fileData", "file_data"]).and_then(decode_file_data)
            {
                content.push(media);
            }
        }
    }
    (content, is_error)
}

/// Whether `value` contains, at any depth, a string-valued `$ref` key. Gemini
/// reads `$ref` inside a function response as a reference to a media part and
/// rejects the request, so such results are sent as text.
fn contains_ref(value: &Value) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| (key == "$ref" && value.is_string()) || contains_ref(value)),
        Value::Array(list) => list.iter().any(contains_ref),
        _ => false,
    }
}

/// The `response` object of a `functionResponse`: a result that is itself a
/// JSON object is passed as that object, everything else is wrapped.
pub(crate) fn response_object(text: &str, is_error: bool) -> Value {
    if is_error {
        return json!({"error": text});
    }
    if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(text.trim()) {
        // An object that looks like one of the wrappers would be unwrapped by
        // the next reader; keep it unambiguous by wrapping it once more.
        let wrapper_like = object.len() == 1
            && ["result", "output", "error"]
                .iter()
                .any(|k| object.contains_key(*k));
        let value = Value::Object(object);
        if !wrapper_like && !contains_ref(&value) {
            return value;
        }
    }
    json!({"result": text})
}

/// Encodes a tool result as a `functionResponse` part, followed by one
/// `inlineData` / `fileData` part per image or document it carries.
///
/// Media travels as sibling parts rather than `functionResponse.parts`
/// because only the newest models accept the nested form.
pub(crate) fn encode_function_response(
    name: &str,
    result: &ToolResult,
    id: Option<&str>,
    native: bool,
) -> Vec<Value> {
    let mut response = Map::new();
    response.insert(
        "name".to_string(),
        Value::String(sanitize_function_name(name)),
    );
    response.insert(
        "response".to_string(),
        response_object(&result.text(), result.is_error),
    );
    if let Some(id) = id {
        response.insert("id".to_string(), Value::String(id.to_string()));
    }
    let mut out = vec![json!({"functionResponse": response})];
    out.extend(
        result
            .content
            .iter()
            .filter_map(|part| encode_media(part, native)),
    );
    out
}

/// Keys of a part that are metadata rather than its payload.
fn is_part_metadata(key: &str) -> bool {
    matches!(
        key,
        "thought"
            | "thoughtSignature"
            | "thought_signature"
            | "partMetadata"
            | "part_metadata"
            | "mediaResolution"
            | "media_resolution"
            | "videoMetadata"
            | "video_metadata"
    )
}

/// True when a part object carries no payload field at all (only a signature
/// or other metadata).
pub(crate) fn is_metadata_only(part: &Map<String, Value>) -> bool {
    part.iter()
        .all(|(key, value)| is_part_metadata(key) || value.is_null())
}

/// A part the canonical model has no slot for (`executableCode`,
/// `codeExecutionResult`, `toolCall`, …), kept verbatim for Gemini peers.
pub(crate) fn opaque(raw: &Value) -> Part {
    Part::Opaque(OpaquePart {
        origin: Protocol::Gemini,
        raw: raw.clone(),
    })
}

/// Candidate-level metadata objects that are preserved as opaque parts and
/// re-attached to the candidate when rendering for a Gemini client.
pub(crate) const CANDIDATE_METADATA: [&str; 3] = [
    "groundingMetadata",
    "citationMetadata",
    "urlContextMetadata",
];

/// If `raw` is a single-key object holding candidate-level metadata, returns
/// the key and the metadata.
pub(crate) fn candidate_metadata(raw: &Value) -> Option<(&'static str, &Value)> {
    let map = raw.as_object()?;
    if map.len() != 1 {
        return None;
    }
    CANDIDATE_METADATA
        .iter()
        .find_map(|key| map.get(*key).map(|value| (*key, value)))
}
