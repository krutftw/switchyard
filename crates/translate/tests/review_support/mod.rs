//! Test doubles for the review tests: a trivial codec whose stream format is
//! one SSE event per canonical event (the `StreamEvent` as JSON in `data:`).
//! The crate's own fake codec is private to its unit tests, so the review
//! tests carry their own.

#![allow(dead_code)]

use serde_json::{Value, json};
use switchyard_core::codec::{
    ClientCtx, Codec, RequestMeta, RequestPath, StreamDecoder, StreamEncoder, UpstreamCtx,
};
use switchyard_core::error::{ApiError, CodecError, UpstreamErrorInfo};
use switchyard_core::ir::{FinishReason, Message, Part, Request, Response, Role};
use switchyard_core::model::ModelInfo;
use switchyard_core::protocol::Protocol;
use switchyard_core::reasoning::{Fitted, ReasoningConfig};
use switchyard_core::sse::SseEvent;
use switchyard_core::stream::StreamEvent;

/// Wire data that makes [`FakeDecoder`] report the stream as unusable.
pub const CORRUPT: &str = "!corrupt";

#[derive(Clone, Copy, Debug)]
pub struct FakeCodec {
    pub protocol: Protocol,
}

impl FakeCodec {
    pub fn new(protocol: Protocol) -> Self {
        FakeCodec { protocol }
    }
}

impl Codec for FakeCodec {
    fn protocol(&self) -> Protocol {
        self.protocol
    }

    fn request_meta(
        &self,
        body: &Value,
        _path: &RequestPath<'_>,
    ) -> Result<RequestMeta, CodecError> {
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| CodecError::invalid_param("model", "model is required"))?;
        Ok(RequestMeta {
            model: model.to_string(),
            stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    fn set_request_model(&self, body: &mut Value, model: &str) {
        if let Some(map) = body.as_object_mut() {
            map.insert("model".into(), json!(model));
        }
    }

    fn read_reasoning(&self, _body: &Value) -> ReasoningConfig {
        ReasoningConfig::default()
    }

    fn write_reasoning(&self, _body: &mut Value, _depth: Fitted, _ctx: &UpstreamCtx<'_>) {}

    fn rewrite_response_model(&self, payload: &mut Value, model: &str) {
        if payload.get("model").is_some_and(Value::is_string) {
            payload["model"] = json!(model);
        }
    }

    fn decode_request(&self, body: &Value, path: &RequestPath<'_>) -> Result<Request, CodecError> {
        let meta = self.request_meta(body, path)?;
        let mut request = Request::new(meta.model, self.protocol);
        request.stream = meta.stream;
        request
            .messages
            .push(Message::new(Role::User, vec![Part::text("hi")]));
        Ok(request)
    }

    fn encode_response(&self, response: &Response, ctx: &ClientCtx) -> Result<Value, CodecError> {
        let mut response = response.clone();
        response.model = ctx.model.clone();
        serde_json::to_value(&response).map_err(|e| CodecError::upstream(e.to_string()))
    }

    fn stream_encoder(&self, _ctx: &ClientCtx) -> Box<dyn StreamEncoder> {
        Box::new(FakeEncoder)
    }

    fn encode_error(&self, error: &ApiError) -> Value {
        json!({"error": {"message": error.message, "status": error.status}})
    }

    fn encode_models(&self, models: &[ModelInfo]) -> Value {
        json!({"models": models.iter().map(|m| m.id.clone()).collect::<Vec<_>>()})
    }

    fn encode_model(&self, model: &ModelInfo) -> Value {
        json!({"id": model.id})
    }

    fn encode_request(
        &self,
        request: &Request,
        _ctx: &UpstreamCtx<'_>,
    ) -> Result<Value, CodecError> {
        Ok(json!({"model": request.model, "stream": request.stream}))
    }

    fn decode_response(&self, body: &Value) -> Result<Response, CodecError> {
        serde_json::from_value(body.clone()).map_err(|e| CodecError::upstream(e.to_string()))
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
pub struct FakeEncoder;

impl StreamEncoder for FakeEncoder {
    fn encode(&mut self, event: &StreamEvent) -> Vec<SseEvent> {
        match serde_json::to_string(event) {
            Ok(data) => vec![SseEvent::data(data)],
            Err(_) => Vec::new(),
        }
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        vec![SseEvent::data("[DONE]")]
    }
}

/// Parses the fake stream format. Upholds the sequence contract on
/// `finish()`; reports [`CORRUPT`] data as an unusable stream.
#[derive(Default)]
pub struct FakeDecoder {
    open: Option<u32>,
    started: bool,
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
            return Ok(Vec::new());
        };
        match &parsed {
            StreamEvent::Start { .. } => self.started = true,
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
        if self.terminal {
            return out;
        }
        if !self.started {
            out.push(StreamEvent::Start {
                id: String::new(),
                model: String::new(),
                created: 0,
            });
        }
        if let Some(index) = self.open.take() {
            out.push(StreamEvent::BlockStop { index });
        }
        self.terminal = true;
        out.push(StreamEvent::Finish {
            reason: FinishReason::Error,
            stop_sequence: None,
        });
        out
    }
}

/// Serialises a canonical event the way the fake upstream would send it.
pub fn wire(event: &StreamEvent) -> SseEvent {
    SseEvent::data(serde_json::to_string(event).expect("serialisable event"))
}

/// The canonical events carried by fake wire events (`[DONE]` is skipped).
pub fn canonical(events: &[SseEvent]) -> Vec<StreamEvent> {
    events
        .iter()
        .filter_map(|e| serde_json::from_str(&e.data).ok())
        .collect()
}

pub fn start() -> StreamEvent {
    StreamEvent::Start {
        id: "resp_1".into(),
        model: "upstream-model".into(),
        created: 1_700_000_000,
    }
}

pub fn finish_stop() -> StreamEvent {
    StreamEvent::Finish {
        reason: FinishReason::Stop,
        stop_sequence: None,
    }
}
