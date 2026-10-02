//! Test doubles shared by this crate's unit tests: a deliberately trivial
//! codec whose wire format is a thin JSON wrapper around the IR.
//!
//! * request: `{"fmt":<tag>,"model":…,"stream":…,"effort":…,"messages":[{"role":…,"text":…}]}`
//! * response: `{"fmt":<tag>,"model":…,"response":<ir::Response as JSON>}`
//! * stream: one SSE event per canonical event, `event: <tag>` and the
//!   [`StreamEvent`] serialised as JSON in `data:`; the encoder ends the
//!   stream with `data: [DONE]`.
//!
//! Two instances with different tags stand in for two different protocols.

use serde_json::{Value, json};
use switchyard_core::codec::{
    ClientCtx, Codec, RequestMeta, RequestPath, StreamDecoder, StreamEncoder, UpstreamCtx,
};
use switchyard_core::error::{ApiError, CodecError, UpstreamErrorInfo};
use switchyard_core::ir::{FinishReason, Message, Part, Request, Response, Role};
use switchyard_core::model::ModelInfo;
use switchyard_core::protocol::Protocol;
use switchyard_core::reasoning::{Fitted, ReasoningConfig, depth_from_effort_str};
use switchyard_core::sse::SseEvent;
use switchyard_core::stream::StreamEvent;

/// A codec for an imaginary protocol.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FakeCodec {
    protocol: Protocol,
    tag: &'static str,
}

impl FakeCodec {
    pub(crate) fn new(protocol: Protocol, tag: &'static str) -> Self {
        FakeCodec { protocol, tag }
    }
}

impl Codec for FakeCodec {
    fn protocol(&self) -> Protocol {
        self.protocol
    }

    fn request_meta(
        &self,
        body: &Value,
        path: &RequestPath<'_>,
    ) -> Result<RequestMeta, CodecError> {
        let model = path
            .model
            .or_else(|| body.get("model").and_then(Value::as_str))
            .ok_or_else(|| CodecError::invalid_param("model", "model is required"))?;
        Ok(RequestMeta {
            model: model.to_string(),
            stream: path
                .stream
                .or_else(|| body.get("stream").and_then(Value::as_bool))
                .unwrap_or(false),
        })
    }

    fn set_request_model(&self, body: &mut Value, model: &str) {
        if let Some(map) = body.as_object_mut() {
            map.insert("model".into(), json!(model));
        }
    }

    fn read_reasoning(&self, body: &Value) -> ReasoningConfig {
        ReasoningConfig {
            depth: body
                .get("effort")
                .and_then(Value::as_str)
                .and_then(depth_from_effort_str),
            summary: None,
        }
    }

    fn write_reasoning(&self, body: &mut Value, depth: Fitted, _ctx: &UpstreamCtx<'_>) {
        let Some(map) = body.as_object_mut() else {
            return;
        };
        match depth {
            Fitted::Use(depth) => {
                map.insert("effort".into(), json!(depth.label()));
            }
            Fitted::Strip => {
                map.shift_remove("effort");
            }
        }
    }

    fn rewrite_response_model(&self, payload: &mut Value, model: &str) {
        if payload.get("model").is_some_and(Value::is_string) {
            payload["model"] = json!(model);
        }
        if payload
            .get("response")
            .and_then(|r| r.get("model"))
            .is_some_and(Value::is_string)
        {
            payload["response"]["model"] = json!(model);
        }
    }

    fn decode_request(&self, body: &Value, path: &RequestPath<'_>) -> Result<Request, CodecError> {
        let meta = self.request_meta(body, path)?;
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| CodecError::invalid_param("messages", "messages is required"))?;
        let mut request = Request::new(meta.model, self.protocol);
        request.stream = meta.stream;
        for message in messages {
            let role = match message.get("role").and_then(Value::as_str) {
                Some("assistant") => Role::Assistant,
                Some("system") => Role::System,
                _ => Role::User,
            };
            let text = message.get("text").and_then(Value::as_str).unwrap_or("");
            request
                .messages
                .push(Message::new(role, vec![Part::text(text)]));
        }
        let reasoning = self.read_reasoning(body);
        if !reasoning.is_empty() {
            request.reasoning = Some(reasoning);
        }
        Ok(request)
    }

    fn encode_response(&self, response: &Response, ctx: &ClientCtx) -> Result<Value, CodecError> {
        let mut response = response.clone();
        response.model = ctx.model.clone();
        let ir =
            serde_json::to_value(&response).map_err(|e| CodecError::upstream(e.to_string()))?;
        Ok(json!({"fmt": self.tag, "model": ctx.model, "response": ir}))
    }

    fn stream_encoder(&self, ctx: &ClientCtx) -> Box<dyn StreamEncoder> {
        Box::new(FakeEncoder {
            tag: self.tag,
            model: ctx.model.clone(),
        })
    }

    fn encode_error(&self, error: &ApiError) -> Value {
        json!({"fmt": self.tag, "error": {"message": error.message, "status": error.status}})
    }

    fn encode_models(&self, models: &[ModelInfo]) -> Value {
        json!({"fmt": self.tag, "models": models.iter().map(|m| m.id.clone()).collect::<Vec<_>>()})
    }

    fn encode_model(&self, model: &ModelInfo) -> Value {
        json!({"fmt": self.tag, "id": model.id})
    }

    fn encode_request(
        &self,
        request: &Request,
        _ctx: &UpstreamCtx<'_>,
    ) -> Result<Value, CodecError> {
        if request.messages.is_empty() {
            return Err(CodecError::Unsupported(
                "the fake protocol needs at least one message".into(),
            ));
        }
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::Assistant => "assistant",
                    Role::System => "system",
                    Role::User => "user",
                };
                json!({"role": role, "text": m.text()})
            })
            .collect();
        let mut body = json!({
            "fmt": self.tag,
            "model": request.model,
            "stream": request.stream,
            "messages": messages,
        });
        if let Some(depth) = request.reasoning.as_ref().and_then(|r| r.depth) {
            body["effort"] = json!(depth.label());
        }
        Ok(body)
    }

    fn decode_response(&self, body: &Value) -> Result<Response, CodecError> {
        if body.get("fmt").and_then(Value::as_str) != Some(self.tag) {
            return Err(CodecError::upstream(format!(
                "not a `{}` response",
                self.tag
            )));
        }
        let ir = body
            .get("response")
            .cloned()
            .ok_or_else(|| CodecError::upstream("missing response"))?;
        serde_json::from_value(ir).map_err(|e| CodecError::upstream(e.to_string()))
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(FakeDecoder::default())
    }

    fn decode_error(&self, _status: u16, body: &[u8]) -> UpstreamErrorInfo {
        UpstreamErrorInfo {
            message: String::from_utf8_lossy(body).into_owned(),
            ..UpstreamErrorInfo::default()
        }
    }
}

/// Renders each canonical event as one SSE event and ends with `[DONE]`.
pub(crate) struct FakeEncoder {
    tag: &'static str,
    model: String,
}

impl StreamEncoder for FakeEncoder {
    fn encode(&mut self, event: &StreamEvent) -> Vec<SseEvent> {
        let mut event = event.clone();
        if let StreamEvent::Start { model, .. } = &mut event
            && !self.model.is_empty()
        {
            *model = self.model.clone();
        }
        match serde_json::to_string(&event) {
            Ok(data) => vec![SseEvent::named(self.tag, data)],
            Err(_) => Vec::new(),
        }
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        vec![SseEvent::data("[DONE]")]
    }
}

/// Wire data that makes [`FakeDecoder`] report the stream as unusable.
pub(crate) const CORRUPT: &str = "!corrupt";

/// Parses the fake stream format and upholds the sequence contract on
/// `finish()` the way real decoders must.
#[derive(Default)]
pub(crate) struct FakeDecoder {
    open: Option<u32>,
    terminal: bool,
}

impl StreamDecoder for FakeDecoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, CodecError> {
        if event.data == CORRUPT {
            return Err(CodecError::upstream("corrupt frame"));
        }
        if event.is_done_marker() {
            return Ok(Vec::new());
        }
        let Ok(parsed) = serde_json::from_str::<StreamEvent>(&event.data) else {
            // Unknown events are skipped, never an error.
            return Ok(Vec::new());
        };
        match &parsed {
            StreamEvent::BlockStart { index, .. } => self.open = Some(*index),
            StreamEvent::BlockStop { .. } => self.open = None,
            StreamEvent::Finish { .. } | StreamEvent::Error(_) => {
                self.open = None;
                self.terminal = true;
            }
            _ => {}
        }
        Ok(vec![parsed])
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if let Some(index) = self.open.take() {
            out.push(StreamEvent::BlockStop { index });
        }
        if !self.terminal {
            self.terminal = true;
            out.push(StreamEvent::Finish {
                reason: FinishReason::Error,
                stop_sequence: None,
            });
        }
        out
    }
}

/// A decoder that parses events but does nothing on `finish()` — a decoder
/// that breaks the contract, for testing the transcoder's safety net.
#[derive(Default)]
pub(crate) struct LazyDecoder;

impl StreamDecoder for LazyDecoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, CodecError> {
        Ok(serde_json::from_str::<StreamEvent>(&event.data)
            .map(|e| vec![e])
            .unwrap_or_default())
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        Vec::new()
    }
}

/// Serialises a canonical event the way the fake upstream would send it.
pub(crate) fn wire(tag: &str, event: &StreamEvent) -> SseEvent {
    SseEvent::named(
        tag,
        serde_json::to_string(event).expect("serialisable event"),
    )
}

/// Parses a fake wire event back into a canonical event.
pub(crate) fn unwire(event: &SseEvent) -> Option<StreamEvent> {
    serde_json::from_str(&event.data).ok()
}
