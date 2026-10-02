//! The per-connection state of a Responses WebSocket and the rules that
//! turn each client message into a complete, stateless Responses request
//! (`docs/DESIGN.md` §10).
//!
//! A real Responses WebSocket upstream remembers the conversation: the
//! client sends only what is new and names the response it continues. The
//! gateway serves these connections through its normal (HTTP) pipeline, to
//! any provider, so it keeps the conversation itself and rebuilds the full
//! input on every turn. Like the vendor's connection-local cache it
//! remembers the latest response of each lane, and `previous_response_id`
//! alone says which of them — if any — a request continues. Everything
//! here is pure: no socket, no clock.

use serde_json::{Map, Value};
use switchyard_codecs::responses::{
    merge_transcript, prewarm_frames, repair_tool_pairs, ws_error_frame,
};

/// A message the gateway cannot act on, answered in-band; the connection
/// stays open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fault {
    pub(crate) status: u16,
    pub(crate) message: String,
    pub(crate) code: Option<&'static str>,
    pub(crate) param: Option<&'static str>,
}

impl Fault {
    pub(crate) fn invalid(message: impl Into<String>) -> Fault {
        Fault {
            status: 400,
            message: message.into(),
            code: None,
            param: None,
        }
    }

    fn with_param(mut self, param: &'static str) -> Fault {
        self.param = Some(param);
        self
    }

    fn with_code(mut self, code: &'static str) -> Fault {
        self.code = Some(code);
        self
    }

    /// The client continues a response this connection does not remember:
    /// any at all on a fresh socket (after a reconnect, typically), or one
    /// that is no longer the latest of its lane. It has to send the whole
    /// conversation again.
    fn previous_response_not_found() -> Fault {
        Fault {
            status: 409,
            message: "Previous response is not available on this websocket; resend the full \
                      conversation input without previous_response_id"
                .to_string(),
            code: Some("previous_response_not_found"),
            param: Some("previous_response_id"),
        }
    }

    /// The error frame for this fault.
    pub(crate) fn frame(&self) -> Value {
        ws_error_frame(self.status, &self.message, self.code, self.param)
    }
}

/// The message types a client may send.
pub(crate) fn is_request_type(kind: &str) -> bool {
    matches!(kind, "response.create" | "response.append")
}

/// The `type` of a client message, for an error message: the client's own
/// text, so kept short.
pub(crate) fn shown_type(message: &Map<String, Value>) -> String {
    match message.get("type") {
        Some(Value::String(kind)) => kind.chars().take(64).collect(),
        Some(_) => "(not a string)".to_string(),
        None => "(missing)".to_string(),
    }
}

/// What to do with a client message.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Prepared {
    /// Run this request through the pipeline. `request` is a complete
    /// Responses request: full input, `stream: true`, no WebSocket-only
    /// fields.
    Turn {
        request: Map<String, Value>,
        /// `request` as JSON, the body to send.
        body: Vec<u8>,
        lane: Option<String>,
    },
    /// A prewarm (`"generate": false`): answer with these two frames; no
    /// upstream is called.
    Prewarm {
        created: Value,
        completed: Value,
        lane: Option<String>,
    },
}

/// How many responses a connection remembers at most: one per lane, for the
/// vendor's 32 named lanes and the default one.
const MAX_REMEMBERED: usize = 33;

/// A response this connection can continue from.
#[derive(Clone, Debug)]
struct Remembered {
    /// The lane (`stream_id`) the response was made on; `None` is the
    /// default lane.
    lane: Option<String>,
    /// The request that produced it, as sent upstream (for a prewarm: as it
    /// would have been sent). Its `input` is the conversation up to, not
    /// including, the response's output.
    request: Map<String, Value>,
    /// `response.output`; empty for a prewarm.
    output: Vec<Value>,
    /// `response.id`: what a client names in `previous_response_id`. For a
    /// prewarm, the synthetic id it was answered with.
    response_id: Option<String>,
    /// The size of `request` and `output` as JSON.
    bytes: usize,
}

/// A turn that ran to a terminal event, for [`Transcript::commit`].
#[derive(Clone, Debug)]
pub(crate) struct Completed {
    /// The lane the request named.
    pub(crate) lane: Option<String>,
    /// The request as sent upstream.
    pub(crate) request: Map<String, Value>,
    /// The size of that request as JSON.
    pub(crate) request_bytes: usize,
    /// The response's output items.
    pub(crate) output: Vec<Value>,
    /// The response's id.
    pub(crate) response_id: Option<String>,
}

/// What a connection remembers between turns: the latest response of each
/// lane, oldest first — like the vendor's connection-local cache. The last
/// one is the latest response of the connection.
#[derive(Clone, Debug, Default)]
pub(crate) struct Transcript {
    remembered: Vec<Remembered>,
}

/// A non-empty string field.
fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn input_of(request: &Map<String, Value>) -> &[Value] {
    request
        .get("input")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// A `stream_id` names a lane of the vendor's protocol: 1 to 256 characters
/// of `[A-Za-z0-9_.-]`.
fn valid_lane(lane: &str) -> bool {
    (1..=256).contains(&lane.len())
        && lane
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A request as JSON, refused when it is larger than `max_bytes`.
fn encode(request: &Map<String, Value>, max_bytes: usize) -> Result<Vec<u8>, Fault> {
    let body = serde_json::to_vec(request)
        .map_err(|_| Fault::invalid("the request could not be encoded"))?;
    if body.len() > max_bytes {
        return Err(Fault {
            status: 413,
            message: "the conversation on this websocket has outgrown the request size limit; \
                      start over with a shorter input and without previous_response_id"
                .to_string(),
            code: Some("request_too_large"),
            param: None,
        });
    }
    Ok(body)
}

/// The lane a message names, when it names a valid one. For tagging an
/// error frame about the message; [`Transcript::prepare`] does the
/// validating.
pub(crate) fn lane_of(message: &Map<String, Value>) -> Option<String> {
    message
        .get("stream_id")
        .and_then(Value::as_str)
        .filter(|lane| valid_lane(lane))
        .map(str::to_string)
}

impl Transcript {
    /// The latest response of the connection.
    fn latest(&self) -> Option<&Remembered> {
        self.remembered.last()
    }

    /// The response a `previous_response_id` names.
    fn find(&self, id: &str) -> Option<&Remembered> {
        self.remembered
            .iter()
            .rev()
            .find(|response| response.response_id.as_deref() == Some(id))
            // A response the upstream gave no id cannot be named exactly;
            // whatever the client calls it, the latest one is meant.
            .or_else(|| {
                self.latest()
                    .filter(|response| response.response_id.is_none())
            })
    }

    /// What a `response.append` continues: the latest response of its lane,
    /// else of the connection.
    fn latest_of(&self, lane: Option<&str>) -> Option<&Remembered> {
        self.remembered
            .iter()
            .rev()
            .find(|response| response.lane.as_deref() == lane)
            .or_else(|| self.latest())
    }

    /// Remembers `response` as the latest of its lane, forgetting the
    /// oldest ones while there are too many or they take more than
    /// `max_bytes` together. The newest always stays.
    fn remember(&mut self, response: Remembered, max_bytes: usize) {
        self.remembered.retain(|old| old.lane != response.lane);
        self.remembered.push(response);
        while self.remembered.len() > 1
            && (self.remembered.len() > MAX_REMEMBERED
                || self
                    .remembered
                    .iter()
                    .fold(0usize, |sum, old| sum.saturating_add(old.bytes))
                    > max_bytes)
        {
            self.remembered.remove(0);
        }
    }

    /// Normalises one client message (`response.create` or
    /// `response.append`, already parsed as a JSON object) against what the
    /// connection remembers. `now` is the current time in unix seconds, used
    /// for a prewarm's answer.
    ///
    /// `previous_response_id` alone decides what a request continues, as in
    /// the vendor's protocol:
    ///
    /// * A `response.create` that names a response this connection
    ///   remembers — the latest of any lane, or a prewarm — continues it:
    ///   that response's input and output are put in front of the new
    ///   input, and its `model` and `instructions` are inherited when the
    ///   message has none.
    /// * One that names anything else is a 409
    ///   `previous_response_not_found`: the client has to resend its
    ///   context. That is every id on a fresh socket, and an earlier
    ///   response of a lane that has moved on.
    /// * One that names nothing is a request of its own. Its input is the
    ///   whole conversation, whatever came before on the connection; only
    ///   `model` is inherited (from the latest request), never
    ///   `instructions`: what an unrelated request told the model is not
    ///   this one's business.
    /// * The legacy `response.append` carries no id and continues the
    ///   latest response of its lane.
    ///
    /// The first request needs a `model`; `input` defaults to `[]` and must
    /// be an array. `type`, `generate`, `previous_response_id` and
    /// `stream_id` are removed, `stream` is forced on, and tool calls
    /// without output (and outputs without call) are dropped from the input.
    ///
    /// The rebuilt request must fit into `max_bytes` (the body limit), or
    /// the message is refused with a 413: a conversation only grows, and
    /// one that no longer fits has to be started over by the client.
    ///
    /// A prewarm is remembered at once; a turn changes nothing until
    /// [`commit`](Self::commit).
    pub(crate) fn prepare(
        &mut self,
        mut message: Map<String, Value>,
        now: i64,
        max_bytes: usize,
    ) -> Result<Prepared, Fault> {
        let kind = message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !is_request_type(&kind) {
            return Err(Fault::invalid(format!(
                "unsupported websocket request type: {}",
                shown_type(&message)
            )));
        }
        let append = kind == "response.append";

        let lane = match message.remove("stream_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(lane)) if valid_lane(&lane) => Some(lane),
            Some(_) => {
                return Err(Fault::invalid(
                    "stream_id must be 1 to 256 characters of letters, digits, `_`, `.` and `-`",
                )
                .with_code("invalid_stream_id")
                .with_param("stream_id"));
            }
        };

        let previous = text(&message, "previous_response_id").map(str::to_string);
        let prewarm = !append && message.get("generate") == Some(&Value::Bool(false));

        let input = match message.get_mut("input").map(Value::take) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items,
            Some(_) => {
                return Err(
                    Fault::invalid("websocket request requires array field: input")
                        .with_param("input"),
                );
            }
        };

        // The response this request continues, if it continues one.
        let base = match &previous {
            Some(id) => Some(
                self.find(id)
                    .ok_or_else(Fault::previous_response_not_found)?,
            ),
            None if append => Some(self.latest_of(lane.as_deref()).ok_or_else(|| {
                Fault::invalid("websocket request received before response.create")
            })?),
            None => None,
        };
        let input = match base {
            Some(base) => merge_transcript(input_of(&base.request), &base.output, &input),
            None => input,
        };

        if text(&message, "model").is_none() {
            let inherited = base
                .or_else(|| self.latest())
                .and_then(|response| text(&response.request, "model"))
                .map(str::to_string);
            match inherited {
                Some(model) => {
                    message.insert("model".to_string(), Value::String(model));
                }
                None => {
                    return Err(Fault::invalid("missing model in response.create request")
                        .with_param("model"));
                }
            }
        }
        if !message.contains_key("instructions")
            && let Some(instructions) = base.and_then(|base| base.request.get("instructions"))
        {
            message.insert("instructions".to_string(), instructions.clone());
        }

        // None of these may reach a stateless upstream.
        for key in ["type", "generate", "previous_response_id"] {
            message.remove(key);
        }
        message.insert("stream".to_string(), Value::Bool(true));

        if prewarm {
            // Not repaired: a call whose output the client has yet to send
            // is still part of what it told us.
            message.insert("input".to_string(), Value::Array(input));
            // What is remembered has to fit a request, too: prewarms cost
            // the client nothing, and must not cost the gateway its memory.
            let bytes = encode(&message, max_bytes)?.len();
            let model = text(&message, "model").unwrap_or("").to_string();
            let (created, completed) = prewarm_frames(&model, now);
            let response_id = created
                .pointer("/response/id")
                .and_then(Value::as_str)
                .map(str::to_string);
            self.remember(
                Remembered {
                    lane: lane.clone(),
                    request: message,
                    output: Vec::new(),
                    response_id,
                    bytes,
                },
                max_bytes,
            );
            return Ok(Prepared::Prewarm {
                created,
                completed,
                lane,
            });
        }

        message.insert("input".to_string(), Value::Array(repair_tool_pairs(input)));
        let body = encode(&message, max_bytes)?;
        Ok(Prepared::Turn {
            request: message,
            body,
            lane,
        })
    }

    /// Records a completed turn: its response is now the latest of its lane
    /// and can be continued from. `max_bytes` (the body limit) bounds what
    /// the connection remembers in all; the responses of the lanes used
    /// longest ago are forgotten first.
    pub(crate) fn commit(&mut self, turn: Completed, max_bytes: usize) {
        let output_bytes = serde_json::to_vec(&turn.output).map_or(0, |json| json.len());
        self.remember(
            Remembered {
                lane: turn.lane,
                request: turn.request,
                output: turn.output,
                response_id: turn.response_id,
                bytes: turn.request_bytes.saturating_add(output_bytes),
            },
            max_bytes,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    const NO_LIMIT: usize = usize::MAX;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn turn(transcript: &mut Transcript, message: Value) -> Map<String, Value> {
        match transcript.prepare(object(message), 0, NO_LIMIT) {
            Ok(Prepared::Turn { request, .. }) => request,
            other => panic!("expected a turn: {other:?}"),
        }
    }

    fn fault(transcript: &mut Transcript, message: Value) -> Fault {
        match transcript.prepare(object(message), 0, NO_LIMIT) {
            Err(fault) => fault,
            other => panic!("expected a fault: {other:?}"),
        }
    }

    /// Completes a turn on `lane` with `output` as response `id`.
    fn complete_on(
        transcript: &mut Transcript,
        lane: Option<&str>,
        request: Map<String, Value>,
        output: Vec<Value>,
        id: Option<&str>,
        max_bytes: usize,
    ) {
        let request_bytes = serde_json::to_vec(&request).unwrap().len();
        transcript.commit(
            Completed {
                lane: lane.map(str::to_string),
                request,
                request_bytes,
                output,
                response_id: id.map(str::to_string),
            },
            max_bytes,
        );
    }

    /// Completes a turn on the default lane.
    fn complete(
        transcript: &mut Transcript,
        request: Map<String, Value>,
        output: Vec<Value>,
        id: &str,
    ) {
        complete_on(transcript, None, request, output, Some(id), NO_LIMIT);
    }

    fn user(id: &str, text: &str) -> Value {
        json!({"type": "message", "role": "user", "id": id, "content": text})
    }

    fn answer(id: &str, text: &str) -> Value {
        json!({"type": "message", "role": "assistant", "id": id,
               "content": [{"type": "output_text", "text": text}]})
    }

    #[test]
    fn the_first_turn_is_cleaned_and_forced_to_stream() {
        let mut transcript = Transcript::default();
        let request = turn(
            &mut transcript,
            json!({
                "type": "response.create", "model": "m", "instructions": "sys",
                "input": [user("u1", "weather?")], "tools": [], "store": false,
                "stream": false, "generate": true, "stream_id": "main"
            }),
        );
        assert_eq!(
            Value::Object(request),
            json!({
                "model": "m", "instructions": "sys", "input": [user("u1", "weather?")],
                "tools": [], "store": false, "stream": true
            })
        );
        // Nothing is remembered until the turn completes.
        assert!(transcript.latest().is_none());
    }

    #[test]
    fn input_defaults_to_empty_and_must_be_an_array() {
        let mut transcript = Transcript::default();
        let request = turn(
            &mut transcript,
            json!({"type": "response.create", "model": "m"}),
        );
        assert_eq!(request["input"], json!([]));
        let refused = fault(
            &mut transcript,
            json!({"type": "response.create", "model": "m", "input": "hi"}),
        );
        assert_eq!(refused.status, 400);
        assert_eq!(
            refused.message,
            "websocket request requires array field: input"
        );
        assert_eq!(refused.frame()["error"]["param"], "input");
    }

    #[test]
    fn the_first_message_needs_a_model_and_must_be_a_create() {
        let mut transcript = Transcript::default();
        let refused = fault(
            &mut transcript,
            json!({"type": "response.create", "input": []}),
        );
        assert_eq!(refused.message, "missing model in response.create request");
        let refused = fault(
            &mut transcript,
            json!({"type": "response.append", "input": []}),
        );
        assert_eq!(
            refused.message,
            "websocket request received before response.create"
        );
        let refused = fault(
            &mut transcript,
            json!({"type": "session.update", "x": "y".repeat(500)}),
        );
        assert_eq!(
            refused.message,
            "unsupported websocket request type: session.update"
        );
    }

    #[test]
    fn a_follow_up_gets_the_whole_conversation() {
        let mut transcript = Transcript::default();
        let first = turn(
            &mut transcript,
            json!({"type": "response.create", "model": "m", "instructions": "sys",
                   "input": [user("u1", "weather?")], "tools": [{"type": "function", "name": "get"}]}),
        );
        let call = json!({"type": "function_call", "id": "fc1", "call_id": "c1", "name": "get", "arguments": "{}"});
        complete(&mut transcript, first, vec![call.clone()], "resp_1");
        assert_eq!(
            transcript.latest().unwrap().response_id.as_deref(),
            Some("resp_1")
        );

        let output = json!({"type": "function_call_output", "call_id": "c1", "output": "sunny"});
        let second = turn(
            &mut transcript,
            json!({"type": "response.create", "previous_response_id": "resp_1", "input": [output]}),
        );
        assert_eq!(
            Value::Object(second),
            json!({
                "input": [user("u1", "weather?"), call, output],
                "model": "m", "instructions": "sys", "stream": true
            }),
            "model and instructions are inherited, tools are not"
        );

        // An explicit `instructions`, even null, is the client's choice.
        let third = turn(
            &mut transcript,
            json!({"type": "response.create", "previous_response_id": "resp_1",
                   "input": [], "instructions": null}),
        );
        assert_eq!(third["instructions"], Value::Null);
    }

    #[test]
    fn append_continues_the_latest_response() {
        let mut transcript = Transcript::default();
        let first = turn(
            &mut transcript,
            json!({"type": "response.create", "model": "m", "instructions": "sys",
                   "input": [user("u1", "one")]}),
        );
        complete(&mut transcript, first, vec![answer("a1", "1")], "resp_1");

        let appended = turn(
            &mut transcript,
            json!({"type": "response.append", "input": [user("u2", "two")]}),
        );
        assert_eq!(
            appended["input"],
            json!([user("u1", "one"), answer("a1", "1"), user("u2", "two")])
        );
        assert_eq!(appended["model"], "m");
        assert_eq!(appended["instructions"], "sys");
    }

    #[test]
    fn a_create_that_names_no_response_is_a_request_of_its_own() {
        let mut transcript = Transcript::default();
        let first = turn(
            &mut transcript,
            json!({"type": "response.create", "model": "m", "instructions": "translate into French",
                   "input": [user("u1", "cat")]}),
        );
        complete(&mut transcript, first, vec![answer("a1", "chat")], "resp_1");

        // User messages only: not an increment to the conversation before,
        // whatever its input looks like.
        for previous in [json!(null), json!(""), json!("  ")] {
            let unrelated = turn(
                &mut transcript,
                json!({"type": "response.create", "previous_response_id": previous,
                       "input": [user("u2", "what is the capital of Peru?")]}),
            );
            assert_eq!(
                Value::Object(unrelated),
                json!({
                    "input": [user("u2", "what is the capital of Peru?")],
                    "model": "m", "stream": true
                }),
                "the model is inherited; the other request's instructions are not"
            );
        }

        // A replayed (compacted, rewritten) history is the same case.
        let compacted = json!([
            user("u9", "summary of before"),
            answer("a9", "ok"),
            user("u10", "next")
        ]);
        let replaced = turn(
            &mut transcript,
            json!({"type": "response.create", "model": "n", "instructions": "sys",
                   "input": compacted}),
        );
        assert_eq!(replaced["input"], compacted);
        assert_eq!(replaced["model"], "n");
        assert_eq!(replaced["instructions"], "sys");

        // Once it completes it is what the connection continues from.
        complete(
            &mut transcript,
            replaced,
            vec![answer("a10", "2")],
            "resp_2",
        );
        let next = turn(
            &mut transcript,
            json!({"type": "response.create", "previous_response_id": "resp_2",
                   "input": [user("u11", "more")]}),
        );
        assert_eq!(
            next["input"],
            json!([
                user("u9", "summary of before"),
                answer("a9", "ok"),
                user("u10", "next"),
                answer("a10", "2"),
                user("u11", "more")
            ])
        );
    }

    #[test]
    fn previous_response_id_without_history_is_a_409() {
        let mut transcript = Transcript::default();
        let refused = fault(
            &mut transcript,
            json!({"type": "response.create", "model": "m", "previous_response_id": "resp_gone",
                   "input": []}),
        );
        assert_eq!(refused.status, 409);
        assert_eq!(
            refused.frame(),
            json!({"type": "error", "status": 409, "error": {
                "message": "Previous response is not available on this websocket; resend the full conversation input without previous_response_id",
                "type": "invalid_request_error",
                "code": "previous_response_not_found",
                "param": "previous_response_id"
            }})
        );
        // An empty id is no id.
        turn(
            &mut transcript,
            json!({"type": "response.create", "model": "m", "previous_response_id": " "}),
        );
    }

    #[test]
    fn only_the_latest_response_of_a_lane_can_be_continued() {
        let mut transcript = Transcript::default();
        let first = turn(
            &mut transcript,
            json!({"type": "response.create", "model": "m", "input": [user("u1", "one")]}),
        );
        complete(&mut transcript, first, vec![answer("a1", "1")], "resp_1");
        let second = turn(
            &mut transcript,
            json!({"type": "response.create", "previous_response_id": "resp_1",
                   "input": [user("u2", "two")]}),
        );
        complete(&mut transcript, second, vec![answer("a2", "2")], "resp_2");

        // "Regenerate": the lane has moved on from resp_1. Continuing from
        // resp_2 instead would show the model the answer being discarded.
        for stale in ["resp_1", "resp_never_seen"] {
            let refused = fault(
                &mut transcript,
                json!({"type": "response.create", "previous_response_id": stale,
                       "input": [user("u3", "two, rephrased")]}),
            );
            assert_eq!(refused.status, 409, "{stale}");
            assert_eq!(refused.code, Some("previous_response_not_found"));
        }
        // Nothing was lost by asking.
        let third = turn(
            &mut transcript,
            json!({"type": "response.create", "previous_response_id": "resp_2",
                   "input": [user("u3", "three")]}),
        );
        assert_eq!(
            third["input"],
            json!([
                user("u1", "one"),
                answer("a1", "1"),
                user("u2", "two"),
                answer("a2", "2"),
                user("u3", "three")
            ])
        );
    }

    #[test]
    fn a_response_without_an_id_is_continued_under_any_name() {
        let mut transcript = Transcript::default();
        let first = turn(
            &mut transcript,
            json!({"type": "response.create", "model": "m", "input": [user("u1", "one")]}),
        );
        complete_on(
            &mut transcript,
            None,
            first,
            vec![answer("a1", "1")],
            None,
            NO_LIMIT,
        );
        let second = turn(
            &mut transcript,
            json!({"type": "response.create", "previous_response_id": "resp_from_created_event",
                   "input": [user("u2", "two")]}),
        );
        assert_eq!(
            second["input"],
            json!([user("u1", "one"), answer("a1", "1"), user("u2", "two")])
        );
    }

    #[test]
    fn lanes_keep_their_own_conversations_and_can_be_forked() {
        let mut transcript = Transcript::default();
        let a1 = turn(
            &mut transcript,
            json!({"type": "response.create", "model": "m", "instructions": "sys a",
                   "stream_id": "a", "input": [user("ua", "about a")]}),
        );
        complete_on(
            &mut transcript,
            Some("a"),
            a1,
            vec![answer("aa", "a!")],
            Some("resp_a1"),
            NO_LIMIT,
        );
        let b1 = turn(
            &mut transcript,
            json!({"type": "response.create", "model": "n", "instructions": "sys b",
                   "stream_id": "b", "input": [user("ub", "about b")]}),
        );
        complete_on(
            &mut transcript,
            Some("b"),
            b1,
            vec![answer("ab", "b!")],
            Some("resp_b1"),
            NO_LIMIT,
        );

        // Lane a continues its own response, although b's is the latest of
        // the connection.
        let a2 = turn(
            &mut transcript,
            json!({"type": "response.create", "stream_id": "a",
                   "previous_response_id": "resp_a1", "input": [user("ua2", "more a")]}),
        );
        assert_eq!(
            Value::Object(a2.clone()),
            json!({
                "input": [user("ua", "about a"), answer("aa", "a!"), user("ua2", "more a")],
                "model": "m", "instructions": "sys a", "stream": true
            })
        );

        // A fork: a's response continued on a new lane. Both stay usable.
        let fork = turn(
            &mut transcript,
            json!({"type": "response.create", "stream_id": "c",
                   "previous_response_id": "resp_a1", "input": [user("uc", "what if")]}),
        );
        assert_eq!(
            fork["input"],
            json!([
                user("ua", "about a"),
                answer("aa", "a!"),
                user("uc", "what if")
            ])
        );
        complete_on(
            &mut transcript,
            Some("c"),
            fork,
            vec![],
            Some("resp_c1"),
            NO_LIMIT,
        );
        complete_on(
            &mut transcript,
            Some("a"),
            a2,
            vec![answer("aa2", "a again")],
            Some("resp_a2"),
            NO_LIMIT,
        );
        assert_eq!(transcript.remembered.len(), 3);
        // Lane a has moved on; b and c are where they were.
        assert_eq!(
            fault(
                &mut transcript,
                json!({"type": "response.create", "previous_response_id": "resp_a1", "input": []})
            )
            .status,
            409
        );
        for id in ["resp_a2", "resp_b1", "resp_c1"] {
            turn(
                &mut transcript,
                json!({"type": "response.create", "previous_response_id": id, "input": []}),
            );
        }

        // `response.append` continues its lane's latest response.
        let appended = turn(
            &mut transcript,
            json!({"type": "response.append", "stream_id": "b", "input": [user("ub2", "more b")]}),
        );
        assert_eq!(
            appended["input"],
            json!([
                user("ub", "about b"),
                answer("ab", "b!"),
                user("ub2", "more b")
            ])
        );
        assert_eq!(appended["model"], "n");
    }

    #[test]
    fn what_is_remembered_is_bounded() {
        // By size: with room for one conversation, the lane used longest
        // ago is forgotten.
        let filler = "x".repeat(600);
        let limit = 1_500;
        let mut transcript = Transcript::default();
        for lane in ["a", "b"] {
            let request = turn(
                &mut transcript,
                json!({"type": "response.create", "model": "m", "stream_id": lane,
                       "input": [user("u", &filler)]}),
            );
            let id = format!("resp_{lane}");
            complete_on(
                &mut transcript,
                Some(lane),
                request,
                vec![answer("x", "ok")],
                Some(&id),
                limit,
            );
        }
        assert_eq!(transcript.remembered.len(), 1);
        assert_eq!(
            fault(
                &mut transcript,
                json!({"type": "response.create", "previous_response_id": "resp_a", "input": []})
            )
            .status,
            409
        );
        turn(
            &mut transcript,
            json!({"type": "response.create", "previous_response_id": "resp_b", "input": []}),
        );

        // The newest stays even when it alone is over the budget (a long
        // answer on top of a request that just fit).
        let request = turn(
            &mut transcript,
            json!({"type": "response.create", "model": "m", "input": [user("u", &filler)]}),
        );
        complete_on(
            &mut transcript,
            None,
            request,
            vec![answer("x", &"y".repeat(2_000))],
            Some("resp_big"),
            limit,
        );
        assert_eq!(transcript.remembered.len(), 1);
        assert_eq!(
            transcript.latest().unwrap().response_id.as_deref(),
            Some("resp_big")
        );

        // By number: one response per lane, and no more lanes than the
        // vendor has.
        let mut transcript = Transcript::default();
        for n in 0..(MAX_REMEMBERED + 5) {
            let lane = format!("lane-{n}");
            let request = turn(
                &mut transcript,
                json!({"type": "response.create", "model": "m", "stream_id": lane, "input": []}),
            );
            let id = format!("resp_{n}");
            complete_on(
                &mut transcript,
                Some(&lane),
                request,
                vec![],
                Some(&id),
                NO_LIMIT,
            );
        }
        assert_eq!(transcript.remembered.len(), MAX_REMEMBERED);
        assert_eq!(
            transcript.remembered[0].response_id.as_deref(),
            Some("resp_5")
        );
    }

    #[test]
    fn a_prewarm_is_answered_locally_and_becomes_the_root() {
        let mut transcript = Transcript::default();
        let prepared = transcript
            .prepare(
                object(json!({"type": "response.create", "model": "m", "generate": false,
                              "instructions": "sys", "tools": [], "input": [user("u0", "context")]})),
                1_767_225_600,
                NO_LIMIT,
            )
            .unwrap();
        let Prepared::Prewarm {
            created, completed, ..
        } = prepared
        else {
            panic!("expected a prewarm");
        };
        let id = created["response"]["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("resp_prewarm_"));
        assert_eq!(created["type"], "response.created");
        assert_eq!(completed["type"], "response.completed");
        assert_eq!(completed["response"]["id"], id.as_str());
        assert_eq!(completed["response"]["output"], json!([]));
        assert_eq!(completed["response"]["usage"]["total_tokens"], 0);
        assert_eq!(created["response"]["created_at"], 1_767_225_600);

        // Another id is not known here.
        let refused = fault(
            &mut transcript,
            json!({"type": "response.create", "previous_response_id": "resp_other", "input": []}),
        );
        assert_eq!(refused.status, 409);

        // The prewarm's id continues from its input.
        let follow_up = turn(
            &mut transcript,
            json!({"type": "response.create", "previous_response_id": id, "input": [user("u1", "go")]}),
        );
        assert_eq!(
            Value::Object(follow_up.clone()),
            json!({
                "input": [user("u0", "context"), user("u1", "go")],
                "model": "m", "instructions": "sys", "stream": true
            })
        );

        // Without an id, what follows a prewarm stands on its own.
        let fresh = turn(
            &mut transcript,
            json!({"type": "response.create", "input": [user("u2", "fresh")]}),
        );
        assert_eq!(fresh["input"], json!([user("u2", "fresh")]));

        // Once a generating turn completes, the prewarm is history.
        complete(&mut transcript, follow_up, vec![], "resp_real");
        let next = turn(
            &mut transcript,
            json!({"type": "response.create", "previous_response_id": "resp_real",
                   "input": [user("u3", "more")]}),
        );
        assert_eq!(
            next["input"],
            json!([user("u0", "context"), user("u1", "go"), user("u3", "more")])
        );
        let refused = fault(
            &mut transcript,
            json!({"type": "response.create", "previous_response_id": id, "input": []}),
        );
        assert_eq!(refused.status, 409);
    }

    #[test]
    fn a_second_prewarm_starts_over() {
        let mut transcript = Transcript::default();
        let first = turn(
            &mut transcript,
            json!({"type": "response.create", "model": "m", "input": [user("u1", "one")]}),
        );
        complete(&mut transcript, first, vec![], "resp_1");
        let prepared = transcript
            .prepare(
                object(json!({"type": "response.create", "generate": false,
                              "input": [user("p1", "root")]})),
                0,
                NO_LIMIT,
            )
            .unwrap();
        assert!(matches!(prepared, Prepared::Prewarm { .. }));
        assert_eq!(transcript.remembered.len(), 1);
        let root = transcript.latest().unwrap();
        assert_eq!(root.request["input"], json!([user("p1", "root")]));
        assert_eq!(root.request["model"], "m");
        assert!(!root.request.contains_key("generate"));
        assert!(
            root.response_id
                .as_deref()
                .is_some_and(|id| id.starts_with("resp_prewarm_"))
        );
    }

    #[test]
    fn orphaned_tool_items_are_dropped_from_what_goes_upstream() {
        let mut transcript = Transcript::default();
        let request = turn(
            &mut transcript,
            json!({"type": "response.create", "model": "m", "input": [
                user("u1", "hi"),
                {"type": "function_call", "id": "fc1", "call_id": "c1", "name": "get", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c2", "output": "lost"},
                {"type": "function_call", "id": "fc3", "call_id": "c3", "name": "get", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c3", "output": "kept"}
            ]}),
        );
        assert_eq!(
            request["input"],
            json!([
                user("u1", "hi"),
                {"type": "function_call", "id": "fc3", "call_id": "c3", "name": "get", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c3", "output": "kept"}
            ])
        );
    }

    #[test]
    fn lanes_are_validated_and_never_sent_upstream() {
        let mut transcript = Transcript::default();
        let prepared = transcript
            .prepare(
                object(json!({"type": "response.create", "model": "m", "stream_id": "lane-1.a_b"})),
                0,
                NO_LIMIT,
            )
            .unwrap();
        let Prepared::Turn {
            request,
            body,
            lane,
        } = prepared
        else {
            panic!("expected a turn");
        };
        assert_eq!(lane.as_deref(), Some("lane-1.a_b"));
        assert!(!request.contains_key("stream_id"));
        // The body is the request, encoded.
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            Value::Object(request)
        );
        for bad in [
            json!(""),
            json!("has space"),
            json!(7),
            json!("x".repeat(257)),
        ] {
            let refused = fault(
                &mut transcript,
                json!({"type": "response.create", "model": "m", "stream_id": bad}),
            );
            assert_eq!(refused.code, Some("invalid_stream_id"));
        }
    }

    #[test]
    fn a_conversation_that_outgrows_the_limit_is_refused_and_left_as_it_was() {
        let limit = 2_000;
        let filler = "x".repeat(600);
        let mut transcript = Transcript::default();

        // Prewarms are free for the client; they must not be able to pile
        // up here for nothing.
        let prewarm = |n: usize, previous: Option<&str>| {
            let mut message = json!({"type": "response.create", "model": "m", "generate": false,
                                     "input": [user(&format!("u{n}"), &filler)]});
            if let Some(previous) = previous {
                message["previous_response_id"] = json!(previous);
            }
            object(message)
        };
        let mut id: Option<String> = None;
        let mut accepted = 0;
        let refused = loop {
            match transcript.prepare(prewarm(accepted, id.as_deref()), 0, limit) {
                Ok(Prepared::Prewarm { created, .. }) => {
                    accepted += 1;
                    id = created["response"]["id"].as_str().map(str::to_string);
                }
                Ok(other) => panic!("expected a prewarm: {other:?}"),
                Err(fault) => break fault,
            }
            assert!(accepted < 10, "the transcript grew without bound");
        };
        assert_eq!(accepted, 2);
        assert_eq!(refused.status, 413);
        assert_eq!(refused.code, Some("request_too_large"));
        // What was there is still there, and still usable.
        assert_eq!(transcript.remembered.len(), 1);
        let root = transcript.latest().unwrap();
        assert_eq!(input_of(&root.request).len(), 2);
        assert_eq!(root.response_id, id);

        // A turn that would not fit is refused the same way …
        let big = transcript
            .prepare(
                object(
                    json!({"type": "response.create", "previous_response_id": id,
                              "input": [user("t1", &filler)]}),
                ),
                0,
                limit,
            )
            .unwrap_err();
        assert_eq!(big.status, 413);
        // … and one that fits is not.
        let small = transcript.prepare(
            object(
                json!({"type": "response.create", "previous_response_id": id,
                          "input": [user("t2", "short")]}),
            ),
            0,
            limit,
        );
        assert!(matches!(small, Ok(Prepared::Turn { .. })));
    }
}
