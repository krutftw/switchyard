//! The canonical streaming model.
//!
//! A streamed response is a sequence of [`StreamEvent`]s. Stream decoders turn
//! an upstream's wire events into this sequence and stream encoders turn it
//! into a client's wire events, so the two sides never need to know about each
//! other.
//!
//! # Contract
//!
//! Every decoder must produce, and every encoder may rely on, a sequence with
//! this shape:
//!
//! ```text
//! Start
//! ( BlockStart(i) delta* BlockStop(i)  |  Usage )*      blocks never overlap; i = 0, 1, 2, …
//! Usage?
//! Finish
//! ```
//!
//! * `Start` comes first, exactly once. A decoder whose upstream fails before
//!   sending anything else still emits `Start` (with whatever id and model it
//!   knows, possibly empty) and then `Error`. Encoders, however, must also
//!   cope with a lone `Error` that was never preceded by `Start`, because the
//!   gateway can abort a stream before the upstream produced any event.
//! * Blocks are strictly sequential: a block is stopped before the next one
//!   starts, and indices increase by one from zero. Decoders for protocols
//!   that interleave (OpenAI parallel tool-call deltas) must buffer.
//! * Deltas only refer to the currently open block and match its kind:
//!   [`StreamEvent::TextDelta`] for text and refusal blocks,
//!   [`StreamEvent::ReasoningDelta`] / [`StreamEvent::ReasoningSignature`] for
//!   reasoning blocks, [`StreamEvent::ToolArgsDelta`] for tool-call blocks.
//! * `Usage` may appear any number of times anywhere after `Start`; each one
//!   is a running total (see [`Usage::merge`]).
//! * `Finish` comes last, exactly once, unless the stream dies, in which case
//!   an `Error` event ends it instead. A decoder whose upstream closes without
//!   a terminal event must close any open block and synthesise
//!   `Finish { reason: Error }` from `finish()`.

use crate::error::ApiError;
use crate::ir::{
    Citation, FinishReason, Part, Reasoning, RefusalPart, Response, Signature, TextPart, ToolCall,
    ToolCallKind,
};
use crate::usage::Usage;
use serde::{Deserialize, Serialize};

/// One step of a streamed response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// The response has begun.
    Start {
        /// Upstream response id (may be empty if the upstream has not sent one
        /// yet; encoders then mint their own).
        id: String,
        /// Model reported by the upstream.
        model: String,
        /// Unix seconds; `0` when unknown.
        #[serde(default)]
        created: i64,
    },
    /// A content block opens.
    BlockStart { index: u32, block: BlockStart },
    /// Text appended to the open text or refusal block.
    TextDelta { index: u32, text: String },
    /// Reasoning text appended to the open reasoning block.
    ReasoningDelta { index: u32, text: String },
    /// The signature / encrypted payload of the open reasoning block. Replaces
    /// any earlier signature for the same block.
    ReasoningSignature { index: u32, signature: Signature },
    /// A fragment of the open tool call's argument text.
    ToolArgsDelta { index: u32, fragment: String },
    /// A citation attached to the open text block.
    Citation { index: u32, citation: Citation },
    /// The open block is complete.
    BlockStop { index: u32 },
    /// Running token totals.
    Usage(Usage),
    /// Generation ended.
    Finish {
        reason: FinishReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_sequence: Option<String>,
    },
    /// The stream failed after it had started. Terminal.
    Error(ApiError),
}

/// What kind of block a [`StreamEvent::BlockStart`] opens.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "block", rename_all = "snake_case")]
pub enum BlockStart {
    Text,
    Reasoning {
        /// Provider item id, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// The provider withheld the text (Anthropic `redacted_thinking`); the
        /// payload arrives as a [`StreamEvent::ReasoningSignature`].
        #[serde(default)]
        redacted: bool,
    },
    ToolCall {
        id: String,
        name: String,
        #[serde(default)]
        kind: ToolCallKind,
        /// Signature bound to the call (Gemini `thoughtSignature`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<Signature>,
    },
    Refusal,
    /// A complete, non-incremental part (image output, provider-specific
    /// block). No deltas follow; the matching `BlockStop` comes next.
    Whole {
        part: Part,
    },
}

/// Folds a [`StreamEvent`] sequence back into a complete [`Response`].
///
/// Used wherever the gateway needs the whole answer of a streamed call: usage
/// accounting, request logs, the WebSocket transcript, and serving a
/// non-streaming client from a streaming upstream.
#[derive(Clone, Debug)]
pub struct Accumulator {
    response: Response,
    open: Option<Open>,
    started: bool,
    finished: bool,
    error: Option<ApiError>,
}

#[derive(Clone, Debug)]
enum Open {
    Text(TextPart),
    Reasoning(Reasoning),
    ToolCall(ToolCall),
    Refusal(RefusalPart),
    Whole(Part),
}

impl Default for Accumulator {
    fn default() -> Self {
        Accumulator::new()
    }
}

impl Accumulator {
    pub fn new() -> Self {
        Accumulator {
            response: Response::new("", ""),
            open: None,
            started: false,
            finished: false,
            error: None,
        }
    }

    /// Applies one event.
    pub fn push(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::Start { id, model, created } => {
                self.started = true;
                self.response.id = id.clone();
                self.response.model = model.clone();
                self.response.created = *created;
            }
            StreamEvent::BlockStart { block, .. } => {
                self.close_block();
                self.open = Some(match block {
                    BlockStart::Text => Open::Text(TextPart::default()),
                    BlockStart::Reasoning { id, redacted } => Open::Reasoning(Reasoning {
                        id: id.clone(),
                        redacted: *redacted,
                        ..Reasoning::default()
                    }),
                    BlockStart::ToolCall {
                        id,
                        name,
                        kind,
                        signature,
                    } => Open::ToolCall(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                        kind: *kind,
                        signature: signature.clone(),
                        cache_control: None,
                    }),
                    BlockStart::Refusal => Open::Refusal(RefusalPart::default()),
                    BlockStart::Whole { part } => Open::Whole(part.clone()),
                });
            }
            StreamEvent::TextDelta { text, .. } => match &mut self.open {
                Some(Open::Text(t)) => t.text.push_str(text),
                Some(Open::Refusal(r)) => r.text.push_str(text),
                _ => {}
            },
            StreamEvent::ReasoningDelta { text, .. } => {
                if let Some(Open::Reasoning(r)) = &mut self.open {
                    r.text.push_str(text);
                }
            }
            StreamEvent::ReasoningSignature { signature, .. } => {
                if let Some(Open::Reasoning(r)) = &mut self.open {
                    r.signature = Some(signature.clone());
                }
            }
            StreamEvent::ToolArgsDelta { fragment, .. } => {
                if let Some(Open::ToolCall(c)) = &mut self.open {
                    c.arguments.push_str(fragment);
                }
            }
            StreamEvent::Citation { citation, .. } => {
                if let Some(Open::Text(t)) = &mut self.open {
                    t.citations.push(citation.clone());
                }
            }
            StreamEvent::BlockStop { .. } => self.close_block(),
            StreamEvent::Usage(u) => self.response.usage.merge(u),
            StreamEvent::Finish {
                reason,
                stop_sequence,
            } => {
                self.close_block();
                self.finished = true;
                self.response.finish = reason.clone();
                self.response.stop_sequence = stop_sequence.clone();
            }
            StreamEvent::Error(err) => {
                self.close_block();
                self.finished = true;
                self.response.finish = FinishReason::Error;
                self.error = Some(err.clone());
            }
        }
    }

    fn close_block(&mut self) {
        if let Some(open) = self.open.take() {
            self.response.parts.push(match open {
                Open::Text(t) => Part::Text(t),
                Open::Reasoning(r) => Part::Reasoning(r),
                Open::ToolCall(c) => Part::ToolCall(c),
                Open::Refusal(r) => Part::Refusal(r),
                Open::Whole(p) => p,
            });
        }
    }

    /// Whether a `Start` event has been seen.
    pub fn started(&self) -> bool {
        self.started
    }

    /// Whether a terminal event (`Finish` or `Error`) has been seen.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// The error that terminated the stream, if it failed.
    pub fn error(&self) -> Option<&ApiError> {
        self.error.as_ref()
    }

    /// Usage totals seen so far.
    pub fn usage(&self) -> Usage {
        self.response.usage
    }

    /// A snapshot of the response as accumulated so far (the open block, if
    /// any, is included in its current state).
    pub fn snapshot(&self) -> Response {
        let mut clone = self.clone();
        clone.close_block();
        clone.response
    }

    /// Consumes the accumulator and returns the response.
    pub fn into_response(mut self) -> Response {
        self.close_block();
        self.response
    }
}

/// Replays a complete [`Response`] as a well-formed event sequence. Lets a
/// streaming client be served from a non-streaming upstream call, and gives
/// stream encoders a cheap way to be tested against whole responses.
pub fn response_to_events(response: &Response) -> Vec<StreamEvent> {
    let mut events = Vec::with_capacity(response.parts.len() * 3 + 3);
    events.push(StreamEvent::Start {
        id: response.id.clone(),
        model: response.model.clone(),
        created: response.created,
    });
    for (i, part) in response.parts.iter().enumerate() {
        let index = i as u32;
        match part {
            Part::Text(t) => {
                events.push(StreamEvent::BlockStart {
                    index,
                    block: BlockStart::Text,
                });
                if !t.text.is_empty() {
                    events.push(StreamEvent::TextDelta {
                        index,
                        text: t.text.clone(),
                    });
                }
                for c in &t.citations {
                    events.push(StreamEvent::Citation {
                        index,
                        citation: c.clone(),
                    });
                }
            }
            Part::Reasoning(r) => {
                events.push(StreamEvent::BlockStart {
                    index,
                    block: BlockStart::Reasoning {
                        id: r.id.clone(),
                        redacted: r.redacted,
                    },
                });
                if !r.text.is_empty() {
                    events.push(StreamEvent::ReasoningDelta {
                        index,
                        text: r.text.clone(),
                    });
                }
                if let Some(sig) = &r.signature {
                    events.push(StreamEvent::ReasoningSignature {
                        index,
                        signature: sig.clone(),
                    });
                }
            }
            Part::ToolCall(c) => {
                events.push(StreamEvent::BlockStart {
                    index,
                    block: BlockStart::ToolCall {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        kind: c.kind,
                        signature: c.signature.clone(),
                    },
                });
                if !c.arguments.is_empty() {
                    events.push(StreamEvent::ToolArgsDelta {
                        index,
                        fragment: c.arguments.clone(),
                    });
                }
            }
            Part::Refusal(r) => {
                events.push(StreamEvent::BlockStart {
                    index,
                    block: BlockStart::Refusal,
                });
                if !r.text.is_empty() {
                    events.push(StreamEvent::TextDelta {
                        index,
                        text: r.text.clone(),
                    });
                }
            }
            other => events.push(StreamEvent::BlockStart {
                index,
                block: BlockStart::Whole {
                    part: other.clone(),
                },
            }),
        }
        events.push(StreamEvent::BlockStop { index });
    }
    if !response.usage.is_empty() {
        events.push(StreamEvent::Usage(response.usage));
    }
    events.push(StreamEvent::Finish {
        reason: response.finish.clone(),
        stop_sequence: response.stop_sequence.clone(),
    });
    events
}

/// Checks a sequence against the contract in the module docs. Returns a
/// description of the first violation. Intended for tests of stream decoders.
pub fn validate_sequence(events: &[StreamEvent]) -> Result<(), String> {
    let mut started = false;
    let mut finished = false;
    let mut open: Option<(u32, BlockStart)> = None;
    let mut next_index = 0u32;
    for (n, ev) in events.iter().enumerate() {
        if finished {
            return Err(format!("event #{n} after terminal event: {ev:?}"));
        }
        if !started && !matches!(ev, StreamEvent::Start { .. }) {
            return Err(format!("event #{n} before Start: {ev:?}"));
        }
        match ev {
            StreamEvent::Start { .. } => {
                if started {
                    return Err(format!("event #{n}: second Start"));
                }
                started = true;
            }
            StreamEvent::BlockStart { index, block } => {
                if let Some((i, _)) = &open {
                    return Err(format!(
                        "event #{n}: block {index} starts while {i} is open"
                    ));
                }
                if *index != next_index {
                    return Err(format!(
                        "event #{n}: block index {index}, expected {next_index}"
                    ));
                }
                next_index += 1;
                open = Some((*index, block.clone()));
            }
            StreamEvent::TextDelta { index, .. } => match &open {
                Some((i, BlockStart::Text | BlockStart::Refusal)) if i == index => {}
                _ => {
                    return Err(format!(
                        "event #{n}: TextDelta without open text block {index}"
                    ));
                }
            },
            StreamEvent::Citation { index, .. } => match &open {
                Some((i, BlockStart::Text)) if i == index => {}
                _ => {
                    return Err(format!(
                        "event #{n}: Citation without open text block {index}"
                    ));
                }
            },
            StreamEvent::ReasoningDelta { index, .. }
            | StreamEvent::ReasoningSignature { index, .. } => match &open {
                Some((i, BlockStart::Reasoning { .. })) if i == index => {}
                _ => {
                    return Err(format!(
                        "event #{n}: reasoning event without open reasoning block {index}"
                    ));
                }
            },
            StreamEvent::ToolArgsDelta { index, .. } => match &open {
                Some((i, BlockStart::ToolCall { .. })) if i == index => {}
                _ => {
                    return Err(format!(
                        "event #{n}: ToolArgsDelta without open tool-call block {index}"
                    ));
                }
            },
            StreamEvent::BlockStop { index } => match open.take() {
                Some((i, _)) if i == *index => {}
                _ => {
                    return Err(format!(
                        "event #{n}: BlockStop {index} without matching start"
                    ));
                }
            },
            StreamEvent::Usage(_) => {}
            StreamEvent::Finish { .. } => {
                if let Some((i, _)) = &open {
                    return Err(format!("event #{n}: Finish while block {i} is open"));
                }
                finished = true;
            }
            StreamEvent::Error(_) => finished = true,
        }
    }
    if !started {
        return Err("no Start event".to_string());
    }
    if !finished {
        return Err("no terminal event".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Protocol;
    use pretty_assertions::assert_eq;

    fn sample() -> Response {
        let mut r = Response::new("resp_1", "model-x");
        r.created = 1_700_000_000;
        r.parts = vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "thinking…".into(),
                signature: Some(Signature::new(Protocol::Anthropic, "sig")),
                redacted: false,
            }),
            Part::text("Hello"),
            Part::tool_call("call_1", "lookup", "{\"q\":\"x\"}"),
        ];
        r.finish = FinishReason::ToolCalls;
        r.usage = Usage {
            input_tokens: 12,
            output_tokens: 34,
            reasoning_tokens: 5,
            ..Usage::default()
        };
        r
    }

    #[test]
    fn replay_then_accumulate_is_identity() {
        let r = sample();
        let events = response_to_events(&r);
        validate_sequence(&events).unwrap();
        let mut acc = Accumulator::new();
        for e in &events {
            acc.push(e);
        }
        assert!(acc.started() && acc.finished());
        assert_eq!(acc.into_response(), r);
    }

    #[test]
    fn snapshot_includes_open_block() {
        let mut acc = Accumulator::new();
        acc.push(&StreamEvent::Start {
            id: "x".into(),
            model: "m".into(),
            created: 0,
        });
        acc.push(&StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Text,
        });
        acc.push(&StreamEvent::TextDelta {
            index: 0,
            text: "par".into(),
        });
        assert_eq!(acc.snapshot().text(), "par");
        assert!(!acc.finished());
    }

    #[test]
    fn error_terminates() {
        let mut acc = Accumulator::new();
        acc.push(&StreamEvent::Start {
            id: "x".into(),
            model: "m".into(),
            created: 0,
        });
        acc.push(&StreamEvent::Error(ApiError::upstream("boom")));
        assert!(acc.finished());
        assert_eq!(acc.error().unwrap().message, "boom");
        assert_eq!(acc.into_response().finish, FinishReason::Error);
    }

    #[test]
    fn validation_catches_overlap_and_bad_indices() {
        let start = StreamEvent::Start {
            id: "x".into(),
            model: "m".into(),
            created: 0,
        };
        let finish = StreamEvent::Finish {
            reason: FinishReason::Stop,
            stop_sequence: None,
        };
        let bad = vec![
            start.clone(),
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text,
            },
            StreamEvent::BlockStart {
                index: 1,
                block: BlockStart::Text,
            },
            finish.clone(),
        ];
        assert!(validate_sequence(&bad).is_err());
        let bad = vec![
            start.clone(),
            StreamEvent::BlockStart {
                index: 1,
                block: BlockStart::Text,
            },
            StreamEvent::BlockStop { index: 1 },
            finish.clone(),
        ];
        assert!(validate_sequence(&bad).is_err());
        let bad = vec![start.clone()];
        assert!(validate_sequence(&bad).is_err());
        let bad = vec![
            start,
            StreamEvent::BlockStart {
                index: 0,
                block: BlockStart::Text,
            },
            StreamEvent::ToolArgsDelta {
                index: 0,
                fragment: "{".into(),
            },
            StreamEvent::BlockStop { index: 0 },
            finish,
        ];
        assert!(validate_sequence(&bad).is_err());
    }
}
