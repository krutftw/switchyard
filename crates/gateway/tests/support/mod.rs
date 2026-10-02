//! Shared test support: a gateway running against the scriptable fake
//! upstream, request builders for the four client protocols, and helpers
//! that read replies with the client protocol's own codec.
#![allow(dead_code)]

pub mod fake;

pub use fake::{Answer, Behaviour, Fake, Kind, Recorded, Wire};

use bytes::Bytes;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use switchyard_core::codec::RequestPath;
use switchyard_core::ir::{Request, Response};
use switchyard_core::stream::{Accumulator, StreamEvent, validate_sequence};
use switchyard_core::{Codec, Protocol, SseEvent};
use switchyard_gateway::{
    ClientIdentity, ClientRequest, Gateway, GatewayOptions, PresentedCredentials, Reply,
};
use switchyard_telemetry::RequestRecord;
use tempfile::TempDir;

/// The client key every test configuration defines.
pub const CLIENT_KEY: &str = "sy-test-key-0123456789";

/// The four protocols, as clients and as upstreams.
pub const PROTOCOLS: [Protocol; 4] = Protocol::ALL;

/// Settings every test configuration starts with: no proxy from the
/// developer's environment, one client key.
pub const PREAMBLE: &str = r#"
[upstream]
proxy = "direct"

[[auth.keys]]
key = "sy-test-key-0123456789"
name = "tester"
"#;

/// One provider per upstream protocol, each with one credential and one
/// model: `m-chat`, `m-responses`, `m-anthropic`, `m-gemini` (upstream ids
/// `up-chat`, `up-responses`, `claude-up`, `gemini-up`).
pub const FOUR_PROVIDERS: &str = r#"
[[providers]]
name = "chat"
kind = "openai-compat"
base_url = "{base}/v1"
api_keys = ["key-chat-1"]
[[providers.models]]
id = "up-chat"
alias = "m-chat"

[[providers]]
name = "responses"
kind = "openai"
wire_api = "responses"
base_url = "{base}/v1"
api_keys = ["key-responses-1"]
[[providers.models]]
id = "up-responses"
alias = "m-responses"

[[providers]]
name = "anthropic"
kind = "anthropic"
base_url = "{base}"
api_keys = ["key-anthropic-1"]
[[providers.models]]
id = "claude-up"
alias = "m-anthropic"

[[providers]]
name = "gemini"
kind = "gemini"
base_url = "{base}"
api_keys = ["key-gemini-1"]
[[providers.models]]
id = "gemini-up"
alias = "m-gemini"
"#;

/// The client-facing model served by the provider of `upstream` in
/// [`FOUR_PROVIDERS`], its upstream id and its credential's key.
pub fn route(upstream: Protocol) -> (&'static str, &'static str, &'static str) {
    match upstream {
        Protocol::OpenaiChat => ("m-chat", "up-chat", "key-chat-1"),
        Protocol::OpenaiResponses => ("m-responses", "up-responses", "key-responses-1"),
        Protocol::Anthropic => ("m-anthropic", "claude-up", "key-anthropic-1"),
        Protocol::Gemini => ("m-gemini", "gemini-up", "key-gemini-1"),
    }
}

/// The fake's wire for a protocol.
pub fn wire(protocol: Protocol) -> Wire {
    match protocol {
        Protocol::OpenaiChat => Wire::Chat,
        Protocol::OpenaiResponses => Wire::Responses,
        Protocol::Anthropic => Wire::Anthropic,
        Protocol::Gemini => Wire::Gemini,
    }
}

pub fn codec(protocol: Protocol) -> &'static dyn Codec {
    switchyard_codecs::codec(protocol)
}

/// A gateway wired to a fake upstream, with its configuration in a
/// temporary directory.
pub struct Harness {
    pub gateway: Gateway,
    pub fake: Fake,
    pub dir: TempDir,
    pub config_path: PathBuf,
}

impl Harness {
    /// Starts a fake upstream and a gateway configured with [`PREAMBLE`]
    /// plus `config`, in which `{base}` stands for the fake's address.
    pub async fn start(config: &str) -> Harness {
        Harness::start_raw(&format!("{PREAMBLE}\n{config}")).await
    }

    /// Like [`Harness::start`] without the preamble.
    pub async fn start_raw(config: &str) -> Harness {
        let fake = Fake::start().await;
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("switchyard.toml");
        std::fs::write(&config_path, config.replace("{base}", &fake.base())).unwrap();
        let gateway = Gateway::start(GatewayOptions::new(&config_path).watch(false))
            .await
            .unwrap_or_else(|error| panic!("the gateway must start: {error}"));
        Harness {
            gateway,
            fake,
            dir,
            config_path,
        }
    }

    /// The identity of [`CLIENT_KEY`].
    pub fn identity(&self) -> ClientIdentity {
        self.identity_of(CLIENT_KEY)
    }

    pub fn identity_of(&self, key: &str) -> ClientIdentity {
        self.gateway
            .authenticate(&PresentedCredentials {
                x_api_key: Some(key.to_string()),
                ..PresentedCredentials::default()
            })
            .expect("the test key must authenticate")
    }

    /// A request in `protocol` for `model` saying "hi".
    pub fn request(&self, protocol: Protocol, model: &str, stream: bool) -> ClientRequest {
        self.request_with(
            protocol,
            body(protocol, model, stream, false),
            model,
            stream,
        )
    }

    /// A request carrying `body`. `model` and `stream` are only used for
    /// Gemini, which names both in the URL.
    pub fn request_with(
        &self,
        protocol: Protocol,
        body: Value,
        model: &str,
        stream: bool,
    ) -> ClientRequest {
        request_as(self.identity(), protocol, body, model, stream)
    }

    /// A "hi" request made by `identity` instead of [`CLIENT_KEY`].
    pub fn request_by(
        &self,
        identity: &ClientIdentity,
        protocol: Protocol,
        model: &str,
        stream: bool,
    ) -> ClientRequest {
        request_as(
            identity.clone(),
            protocol,
            body(protocol, model, stream, false),
            model,
            stream,
        )
    }

    /// Sends a "hi" request and reads the whole reply.
    pub async fn ask(&self, protocol: Protocol, model: &str, stream: bool) -> Output {
        Output::read(
            self.gateway
                .generate(self.request(protocol, model, stream))
                .await,
        )
        .await
    }

    /// Sends a request with a custom body and reads the whole reply.
    pub async fn ask_with(
        &self,
        protocol: Protocol,
        body: Value,
        model: &str,
        stream: bool,
    ) -> Output {
        let request = self.request_with(protocol, body, model, stream);
        Output::read(self.gateway.generate(request).await).await
    }

    /// The record of a finished request.
    pub fn record(&self, request_id: &str) -> Arc<RequestRecord> {
        self.gateway
            .telemetry()
            .usage()
            .get(request_id)
            .unwrap_or_else(|| panic!("no record for request {request_id}"))
    }

    /// Edits the configuration file through the store and waits until the
    /// gateway has applied the result.
    pub async fn reconfigure(&self, text: &str) {
        let events = self.gateway.telemetry().subscribe();
        self.gateway
            .config_store()
            .replace_text(&text.replace("{base}", &self.fake.base()))
            .await
            .expect("the new configuration must be valid");
        Self::applied(events).await;
    }

    /// Has the gateway read its configuration file again, changed or not,
    /// and waits until it has applied it: everything that is checked when a
    /// configuration is applied (service-account files, say) is checked
    /// again.
    pub async fn reload(&self) {
        let events = self.gateway.telemetry().subscribe();
        self.gateway
            .config_store()
            .reload_from_disk()
            .await
            .expect("the configuration file must be valid");
        Self::applied(events).await;
    }

    /// Waits for the gateway to announce an applied configuration.
    async fn applied(mut events: tokio::sync::broadcast::Receiver<switchyard_telemetry::Event>) {
        let applied = async {
            loop {
                match events.recv().await {
                    Ok(event) if event.topic() == "config.reloaded" => return,
                    Ok(_) => {}
                    Err(error) => panic!("event bus closed: {error}"),
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(5), applied)
            .await
            .expect("the gateway must apply the new configuration");
    }
}

/// A request carrying `body`, made by `identity`. `model` and `stream` are
/// only used for Gemini, which names both in the URL.
pub fn request_as(
    identity: ClientIdentity,
    protocol: Protocol,
    body: Value,
    model: &str,
    stream: bool,
) -> ClientRequest {
    let mut request = ClientRequest::new(
        protocol,
        endpoint(protocol),
        Bytes::from(body.to_string()),
        identity,
    );
    if protocol == Protocol::Gemini {
        request.path_model = Some(model.to_string());
        request.path_stream = Some(stream);
    }
    request
}

/// The route label of a protocol's generation endpoint.
pub fn endpoint(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenaiChat => "POST /v1/chat/completions",
        Protocol::OpenaiResponses => "POST /v1/responses",
        Protocol::Anthropic => "POST /v1/messages",
        Protocol::Gemini => "POST /v1beta/models/{model}:generateContent",
    }
}

/// The JSON-schema of the test tool's arguments.
pub fn weather_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"city": {"type": "string"}},
        "required": ["city"]
    })
}

/// A tool declaration named `name` in `protocol`'s shape.
pub fn tool_declaration(protocol: Protocol, name: &str) -> Value {
    match protocol {
        Protocol::OpenaiChat => json!([{"type": "function", "function": {
            "name": name, "description": "Looks up the weather", "parameters": weather_schema()
        }}]),
        Protocol::OpenaiResponses => json!([{
            "type": "function", "name": name, "description": "Looks up the weather",
            "parameters": weather_schema()
        }]),
        Protocol::Anthropic => json!([{
            "name": name, "description": "Looks up the weather", "input_schema": weather_schema()
        }]),
        Protocol::Gemini => json!([{"functionDeclarations": [{
            "name": name, "description": "Looks up the weather", "parameters": weather_schema()
        }]}]),
    }
}

/// A minimal request body in `protocol`: one user turn saying "hi",
/// optionally offering the `get_weather` tool.
pub fn body(protocol: Protocol, model: &str, stream: bool, with_tool: bool) -> Value {
    let mut body = match protocol {
        Protocol::OpenaiChat => json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": stream
        }),
        Protocol::OpenaiResponses => json!({
            "model": model,
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "stream": stream
        }),
        Protocol::Anthropic => json!({
            "model": model,
            "max_tokens": 256,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": stream
        }),
        Protocol::Gemini => json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
        }),
    };
    if protocol == Protocol::OpenaiChat && stream {
        // A Chat client only gets the usage chunk when it asks for it.
        body["stream_options"] = json!({"include_usage": true});
    }
    if with_tool {
        body["tools"] = tool_declaration(protocol, "get_weather");
    }
    body
}

/// A reply read to its end.
#[derive(Debug)]
pub struct Output {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub request_id: String,
    /// Whether the gateway answered with a stream.
    pub streamed: bool,
    /// The body of a complete reply.
    pub body: Bytes,
    /// The events of a stream.
    pub events: Vec<SseEvent>,
}

impl Output {
    /// Reads a reply; a stream is drained until the gateway closes it.
    pub async fn read(reply: Reply) -> Output {
        match reply {
            Reply::Full(full) => Output {
                status: full.status,
                headers: full.headers,
                request_id: full.request_id,
                streamed: false,
                body: full.body,
                events: Vec::new(),
            },
            Reply::Stream(mut stream) => {
                let mut events = Vec::new();
                let drained = async {
                    while let Some(event) = stream.events.recv().await {
                        events.push(event);
                    }
                };
                tokio::time::timeout(Duration::from_secs(10), drained)
                    .await
                    .expect("the stream must end");
                Output {
                    status: 200,
                    headers: stream.headers,
                    request_id: stream.request_id,
                    streamed: true,
                    body: Bytes::new(),
                    events,
                }
            }
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {:?}", String::from_utf8_lossy(&self.body)))
    }

    /// The canonical events of a streamed reply, decoded with the client
    /// protocol's own stream decoder.
    pub fn canonical(&self, protocol: Protocol) -> Vec<StreamEvent> {
        assert!(self.streamed, "not a streamed reply");
        let mut decoder = codec(protocol).stream_decoder();
        let mut events = Vec::new();
        for event in &self.events {
            events.extend(
                decoder
                    .decode(event)
                    .unwrap_or_else(|e| panic!("client stream is not valid {protocol}: {e}")),
            );
        }
        events.extend(decoder.finish());
        events
    }

    /// The response the client received, read with the client protocol's
    /// own decoder (and, for a stream, its accumulator). A stream must
    /// follow the canonical sequence contract.
    pub fn response(&self, protocol: Protocol) -> Response {
        if !self.streamed {
            assert_eq!(self.status, 200, "{}", String::from_utf8_lossy(&self.body));
            return codec(protocol)
                .decode_response(&self.json())
                .unwrap_or_else(|e| panic!("client response is not valid {protocol}: {e}"));
        }
        let events = self.canonical(protocol);
        validate_sequence(&events)
            .unwrap_or_else(|e| panic!("invalid {protocol} stream: {e}\n{:#?}", self.events));
        let mut accumulator = Accumulator::new();
        for event in &events {
            accumulator.push(event);
        }
        assert!(
            accumulator.error().is_none(),
            "stream ended in an error: {:?}",
            accumulator.error()
        );
        accumulator.into_response()
    }

    /// The message of an error reply, read with the client protocol's
    /// error decoder.
    pub fn error_message(&self, protocol: Protocol) -> String {
        codec(protocol)
            .decode_error(self.status, &self.body)
            .message
    }

    /// The stream as wire text.
    pub fn wire_text(&self) -> String {
        self.events
            .iter()
            .map(|event| String::from_utf8_lossy(&event.to_bytes()).into_owned())
            .collect()
    }
}

/// Decodes an upstream request the fake recorded with the codec of the
/// protocol it arrived in.
pub fn decode_upstream(recorded: &Recorded, protocol: Protocol) -> Request {
    let stream = matches!(recorded.kind, Kind::Generate { stream: true });
    codec(protocol)
        .decode_request(
            &recorded.body,
            &RequestPath {
                model: Some(&recorded.model),
                stream: Some(stream),
            },
        )
        .unwrap_or_else(|e| panic!("upstream request is not valid {protocol}: {e}"))
}

/// The arguments of a tool call as JSON.
pub fn arguments(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|e| panic!("arguments are not JSON ({e}): {raw}"))
}

/// Waits until `check` holds, polling briefly.
pub async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..400 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for: {what}");
}
