//! The stream transcoder: upstream wire events in, client wire events out.
//!
//! One [`Transcoder`] serves one streamed upstream attempt. The gateway feeds
//! it every SSE event the upstream sends ([`Transcoder::push`]), tells it when
//! the upstream closed ([`Transcoder::finish`]) or when the gateway itself
//! gave up on the stream ([`Transcoder::fail`]), and writes whatever comes
//! back to the client. Meanwhile the transcoder folds the stream into an
//! [`Accumulator`], so usage, the complete response and the terminal state
//! are available for accounting whichever mode is used.
//!
//! # Modes
//!
//! * **Translation** ([`Transcoder::translate`]) — client and upstream speak
//!   different protocols. Each upstream event is decoded into canonical
//!   [`StreamEvent`]s by the upstream protocol's [`StreamDecoder`] and
//!   re-encoded by the client protocol's [`StreamEncoder`].
//! * **Passthrough** ([`Transcoder::passthrough`]) — same protocol on both
//!   sides. The *original* events are forwarded, with only the model name
//!   rewritten to the one the client asked for (the bytes of that one value
//!   are replaced; the rest of the payload is copied verbatim, see
//!   [`rewrite_model_text`]). The decoder still runs, but only on the side,
//!   to feed the accumulator; whatever it makes of the stream never changes
//!   what the client receives.
//!
//! # Terminal-event rules
//!
//! * A stream ends with exactly one terminal canonical event (`Finish` or
//!   `Error`). Canonical events a decoder produces after it are dropped (a
//!   late `Usage` is still counted), and nothing the transcoder does itself —
//!   [`Transcoder::fail`], a decode failure — adds a second one.
//! * In translation mode a stream that closes without a terminal event is
//!   completed by [`Transcoder::finish`]: the decoder closes its open block
//!   and emits `Finish { reason: Error }`; if it fails to, the transcoder
//!   does it itself, so the client always sees a well-formed ending.
//! * In passthrough mode nothing is added by [`Transcoder::finish`]: a
//!   truncated upstream stream reaches the client truncated, exactly as the
//!   upstream sent it, and [`Transcoder::truncated`] reports it afterwards.
//!   A caller that would rather tell the client opts in with
//!   [`Transcoder::report_truncation`]; `finish` then appends the protocol's
//!   in-stream error to a truncated stream.
//! * Whether a stream is complete is only known once [`Transcoder::finish`]
//!   has run. Decoders may hold the terminal event back until then (a Chat
//!   Completions stream ends with `[DONE]`, which some upstreams omit), so
//!   **do not** treat `!is_terminal()` at end of stream as a truncation and
//!   call [`Transcoder::fail`]: that would append an error to a complete
//!   response. Call `finish` when the upstream closes the stream and `fail`
//!   only when the gateway itself gives up on it.
//! * After [`Transcoder::finish`] or [`Transcoder::fail`] the transcoder is
//!   closed: further calls return nothing.
//!
//! # Tool names
//!
//! On the translation path the gateway may have rewritten tool names the
//! upstream would refuse ([`crate::toolnames::sanitize_tool_names`]). Give
//! the resulting map to the transcoder with [`Transcoder::with_tool_names`]
//! and every tool call the upstream opens is reported to the client — and
//! recorded in the accumulated response — under the name the client declared.
//!
//! # Secrets and unrequested events
//!
//! Two hooks let the gateway decide what of an upstream stream a client may
//! see, without the transcoder knowing any protocol:
//!
//! * [`Transcoder::with_scrubber`] — a function that removes the upstream
//!   credential from text. It is applied to what an upstream says **about a
//!   failure** (an in-stream error event, the description of an undecodable
//!   stream, the error given to [`Transcoder::fail`]), in both modes, before
//!   the client and [`Transcoder::error`] see it: careless upstreams quote
//!   the key they were called with. Ordinary content is never touched —
//!   self-hosted servers use keys such as `ollama` or `lm-studio`, and a
//!   model must stay free to write those words.
//! * [`Transcoder::with_passthrough_filter`] — a predicate that keeps an
//!   upstream event from being forwarded in passthrough mode (a usage chunk
//!   the gateway asked the upstream for but the client did not). The side
//!   decoder still reads such an event.
//!
//! # Bootstrap
//!
//! The gateway may retry a streamed attempt on another credential as long as
//! nothing has been sent to the client. To support that it can hold back the
//! output of [`Transcoder::push`] until [`Transcoder::saw_first_event`]
//! becomes true, and inspect [`Transcoder::error`] before releasing it: an
//! upstream that answers `200` and then immediately sends an in-stream error
//! shows up there.
//!
//! An upstream that closes the stream without ever producing an event never
//! opens that gate: `finish` then completes the (empty) stream, and it is the
//! caller's `saw_first_event()` check that tells such an attempt apart from
//! an answer.

use crate::splice::splice;
use crate::toolnames::ToolNames;
use serde_json::Value;
use std::fmt;
use std::sync::Arc;
use switchyard_core::codec::{ClientCtx, Codec, StreamDecoder, StreamEncoder};
use switchyard_core::error::{ApiError, CodecError};
use switchyard_core::ir::{FinishReason, Response};
use switchyard_core::sse::SseEvent;
use switchyard_core::stream::{Accumulator, StreamEvent};
use switchyard_core::usage::Usage;

/// A handle to a codec that is either a `'static` reference (the usual case:
/// codecs are unit structs) or shared ownership.
#[derive(Clone)]
pub enum CodecRef {
    /// A codec that lives for the whole program.
    Static(&'static dyn Codec),
    /// A reference-counted codec.
    Shared(Arc<dyn Codec>),
}

impl CodecRef {
    /// The codec.
    pub fn get(&self) -> &dyn Codec {
        match self {
            CodecRef::Static(codec) => *codec,
            CodecRef::Shared(codec) => codec.as_ref(),
        }
    }
}

impl fmt::Debug for CodecRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CodecRef({})", self.get().protocol())
    }
}

impl From<&'static dyn Codec> for CodecRef {
    fn from(codec: &'static dyn Codec) -> Self {
        CodecRef::Static(codec)
    }
}

impl<C: Codec> From<&'static C> for CodecRef {
    fn from(codec: &'static C) -> Self {
        CodecRef::Static(codec)
    }
}

impl From<Arc<dyn Codec>> for CodecRef {
    fn from(codec: Arc<dyn Codec>) -> Self {
        CodecRef::Shared(codec)
    }
}

impl<C: Codec> From<Arc<C>> for CodecRef {
    fn from(codec: Arc<C>) -> Self {
        CodecRef::Shared(codec)
    }
}

/// Removes secrets from text an upstream wrote. See
/// [`Transcoder::with_scrubber`].
type Scrub = Box<dyn Fn(&str) -> String + Send>;

/// Decides whether an upstream event is forwarded in passthrough mode. See
/// [`Transcoder::with_passthrough_filter`].
type Keep = Box<dyn Fn(&SseEvent) -> bool + Send>;

enum Mode {
    Translate {
        encoder: Box<dyn StreamEncoder>,
    },
    Passthrough {
        codec: CodecRef,
        client_model: Option<String>,
    },
}

/// Converts one upstream stream into one client stream. See the module docs.
pub struct Transcoder {
    decoder: Box<dyn StreamDecoder>,
    mode: Mode,
    accumulator: Accumulator,
    /// Index of the canonical block that is currently open, so the stream can
    /// be closed properly even if the decoder does not do it.
    open_block: Option<u32>,
    /// Model reported by the upstream's `Start` event.
    upstream_model: String,
    saw_first_event: bool,
    /// The terminal event was an `Error`, or a `Finish` with reason `Error`.
    ended_in_error: bool,
    first_error: Option<ApiError>,
    decode_error: Option<CodecError>,
    /// Passthrough only: tell the client when the upstream stream turns out
    /// to be truncated.
    report_truncation: bool,
    /// The upstream closed the stream before its terminal event.
    truncated: bool,
    /// `finish()` or `fail()` has run (or a decode failure ended the stream).
    closed: bool,
    /// Translation only: rewritten tool names to turn back into the client's
    /// own before events are accumulated and encoded.
    tool_names: Option<ToolNames>,
    /// Removes the upstream credential from failure descriptions.
    scrub: Option<Scrub>,
    /// Passthrough only: which upstream events are forwarded.
    keep: Option<Keep>,
    events_in: u64,
    events_out: u64,
}

impl fmt::Debug for Transcoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // State only: stream contents do not belong in logs.
        f.debug_struct("Transcoder")
            .field("passthrough", &self.is_passthrough())
            .field("saw_first_event", &self.saw_first_event)
            .field("terminal", &self.is_terminal())
            .field("closed", &self.closed)
            .field("events_in", &self.events_in)
            .field("events_out", &self.events_out)
            .finish_non_exhaustive()
    }
}

impl Transcoder {
    /// A transcoder for a translated stream: `decoder` belongs to the
    /// upstream's protocol (`Codec::stream_decoder`), `encoder` to the
    /// client's (`Codec::stream_encoder`, created with the client's
    /// `ClientCtx` so it reports the model name the client asked for).
    pub fn translate(decoder: Box<dyn StreamDecoder>, encoder: Box<dyn StreamEncoder>) -> Self {
        Transcoder::new(decoder, Mode::Translate { encoder })
    }

    /// A transcoder for a same-protocol stream.
    ///
    /// `decoder` and `codec` both belong to the shared protocol. `codec` is
    /// used to rewrite the model name in forwarded events
    /// (`Codec::rewrite_response_model`) and to render an in-stream error if
    /// [`Transcoder::fail`] is called. With `client_model == None` events are
    /// forwarded untouched.
    pub fn passthrough(
        decoder: Box<dyn StreamDecoder>,
        codec: impl Into<CodecRef>,
        client_model: Option<String>,
    ) -> Self {
        Transcoder::new(
            decoder,
            Mode::Passthrough {
                codec: codec.into(),
                client_model,
            },
        )
    }

    fn new(decoder: Box<dyn StreamDecoder>, mode: Mode) -> Self {
        Transcoder {
            decoder,
            mode,
            accumulator: Accumulator::new(),
            open_block: None,
            upstream_model: String::new(),
            saw_first_event: false,
            ended_in_error: false,
            first_error: None,
            decode_error: None,
            report_truncation: false,
            truncated: false,
            closed: false,
            tool_names: None,
            scrub: None,
            keep: None,
            events_in: 0,
            events_out: 0,
        }
    }

    /// Chooses what [`Transcoder::finish`] does with a **passthrough** stream
    /// that the upstream closed before its terminal event.
    ///
    /// Off (the default): nothing is added, the client receives the stream
    /// exactly as truncated as the upstream sent it. On: `finish` appends the
    /// protocol's in-stream error (`502`, rendered the way
    /// [`Transcoder::fail`] renders one, with the same limitations) so the
    /// client can tell a cut-off answer from a complete one, and records it
    /// as [`Transcoder::error`].
    ///
    /// Has no effect in translation mode, where a truncated stream is always
    /// completed with `Finish { reason: Error }` through the encoder, and
    /// none after a side-decoder failure, when the transcoder cannot tell
    /// whether the stream was complete.
    pub fn report_truncation(mut self, report: bool) -> Self {
        self.report_truncation = report;
        self
    }

    /// Restores the client's own tool names in a **translated** stream.
    ///
    /// `names` is the map [`crate::toolnames::sanitize_tool_names`] returned
    /// for the request this stream answers. Every canonical event that opens
    /// a tool call has its name looked up in it before the event is
    /// accumulated and encoded, so both the client and
    /// [`Transcoder::response_snapshot`] see the name the client declared.
    ///
    /// Has no effect in passthrough mode, where the upstream speaks the
    /// client's protocol and no name was rewritten.
    pub fn with_tool_names(mut self, names: ToolNames) -> Self {
        self.tool_names = (!names.is_empty() && !self.is_passthrough()).then_some(names);
        self
    }

    /// Removes secrets from what the upstream says about a failure.
    ///
    /// `scrub` returns its argument without the credentials the upstream
    /// call presented (the gateway passes `Target::redact`). It is applied
    ///
    /// * to the message, code and parameter of every upstream error event
    ///   before it is recorded ([`Transcoder::error`]) and encoded for the
    ///   client (translation);
    /// * to the data of a forwarded event that the side decoder read as an
    ///   error — an error event, or a terminal event with reason `Error` —
    ///   and, once the side decoder has given up on the stream and can no
    ///   longer tell errors from content, to every forwarded event
    ///   (passthrough);
    /// * to the description of a stream that could not be decoded and to the
    ///   error given to [`Transcoder::fail`].
    ///
    /// Content is left alone on purpose: see the module docs.
    pub fn with_scrubber(mut self, scrub: impl Fn(&str) -> String + Send + 'static) -> Self {
        self.scrub = Some(Box::new(scrub));
        self
    }

    /// Chooses which upstream events a **passthrough** stream forwards.
    ///
    /// `keep` is asked about every upstream event after the side decoder
    /// has read it; an event it answers `false` for is counted (usage, the
    /// accumulated response, [`Transcoder::saw_first_event`]) but not
    /// returned by [`Transcoder::push`]. The gateway uses it to hold back
    /// events that exist only because of something it added to the upstream
    /// request.
    ///
    /// Has no effect in translation mode, where the client's encoder decides
    /// what the client sees.
    pub fn with_passthrough_filter(
        mut self,
        keep: impl Fn(&SseEvent) -> bool + Send + 'static,
    ) -> Self {
        if self.is_passthrough() {
            self.keep = Some(Box::new(keep));
        }
        self
    }

    /// Feeds one upstream wire event and returns the wire events to send to
    /// the client, in order.
    ///
    /// * **Translation:** the event is decoded, accumulated and re-encoded.
    ///   Events the decoder does not understand produce nothing. If the
    ///   decoder reports the stream as unusable (`Err`), the client stream is
    ///   terminated exactly as [`Transcoder::fail`] would with a `502` error
    ///   (so no error event follows a terminal event that was already sent),
    ///   and the transcoder is closed.
    /// * **Passthrough:** the original event is returned, with the model name
    ///   rewritten when a client model was given and the event's data is JSON
    ///   that carries one. Data that is not JSON (`[DONE]`) is forwarded
    ///   byte for byte, and so is JSON the rewrite did not change. In a
    ///   changed payload only the bytes of the changed value are replaced
    ///   (see [`rewrite_model_text`]).
    ///   A decoder failure is swallowed — the stream is still forwarded, only
    ///   the side accounting stops (see [`Transcoder::decode_error`]) — and
    ///   counts as the first event (see [`Transcoder::saw_first_event`]).
    ///
    /// Returns nothing once the transcoder is closed.
    pub fn push(&mut self, event: &SseEvent) -> Vec<SseEvent> {
        if self.closed {
            return Vec::new();
        }
        self.events_in += 1;

        let decoded = if self.decode_error.is_some() {
            // The decoder declared the stream unusable earlier (passthrough
            // only: translation closes on that); do not feed it any more.
            Ok(Vec::new())
        } else {
            self.decoder.decode(event)
        };

        let out = if self.is_passthrough() {
            // Whether this event describes a failure — or, with the side
            // decoder out of action, might.
            let mut about_failure = self.decode_error.is_some();
            match decoded {
                Ok(mut events) => {
                    about_failure |= events.iter().any(is_failure);
                    self.scrub_errors(&mut events);
                    for canonical in &events {
                        self.absorb(canonical, true);
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "side decoder failed on a passthrough stream; usage accounting stops");
                    self.decode_error = Some(error);
                    about_failure = true;
                    // Nothing more will ever be learned from the decoder, and
                    // the stream is forwarded regardless: a caller holding
                    // output back for the first event must not wait forever.
                    self.saw_first_event = true;
                }
            }
            if self.keep.as_ref().is_some_and(|keep| !keep(event)) {
                Vec::new()
            } else {
                vec![self.forward(event, about_failure)]
            }
        } else {
            match decoded {
                Ok(mut events) => {
                    self.scrub_errors(&mut events);
                    self.restore_tool_names(&mut events);
                    self.encode_all(&events, true)
                }
                Err(error) => {
                    let api = ApiError::upstream(
                        self.scrubbed(&format!("upstream stream could not be decoded: {error}")),
                    );
                    self.decode_error = Some(error);
                    self.closed = true;
                    self.end_with(api)
                }
            }
        };
        self.events_out += out.len() as u64;
        out
    }

    /// Signals that the upstream closed the stream. Returns the events that
    /// complete the client stream.
    ///
    /// * **Translation:** runs `decoder.finish()` and encodes its tail. If no
    ///   terminal event has been produced by then, the transcoder closes the
    ///   open block and emits `Finish { reason: Error }` itself (or an
    ///   `Error` event when the stream never started). Finally
    ///   `encoder.finish()` adds protocol terminators such as `data: [DONE]`.
    /// * **Passthrough:** returns nothing — the upstream's own terminators
    ///   were already forwarded. `decoder.finish()` still runs so the
    ///   accumulated response is complete. The one exception is a truncated
    ///   stream when [`Transcoder::report_truncation`] is on: the protocol's
    ///   in-stream error is returned.
    ///
    /// Afterwards [`Transcoder::truncated`] tells whether the upstream closed
    /// the stream before its terminal event. A stream in which the upstream
    /// sent nothing at all is completed like any other truncated stream; a
    /// caller that wants to treat it as a failed attempt checks
    /// [`Transcoder::saw_first_event`].
    ///
    /// Idempotent; returns nothing if the transcoder is already closed.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        if self.closed {
            return Vec::new();
        }
        self.closed = true;
        let ended_by_upstream = self.accumulator.finished();

        let mut tail = if self.decode_error.is_some() {
            Vec::new()
        } else {
            self.decoder.finish()
        };
        self.scrub_errors(&mut tail);
        self.restore_tool_names(&mut tail);
        let mut out = self.encode_all(&tail, false);

        if !self.accumulator.finished() && self.decode_error.is_none() {
            // The decoder left the sequence open. Complete it here so that
            // the encoder (and the accumulated response) see a proper ending.
            let mut closing = Vec::with_capacity(2);
            if self.accumulator.started() {
                if let Some(index) = self.open_block {
                    closing.push(StreamEvent::BlockStop { index });
                }
                closing.push(StreamEvent::Finish {
                    reason: FinishReason::Error,
                    stop_sequence: None,
                });
            } else {
                closing.push(StreamEvent::Error(ApiError::upstream(
                    "upstream closed the stream without sending a response",
                )));
            }
            out.extend(self.encode_all(&closing, false));
        }

        // A decoder that held the real terminal event back until now (Chat
        // without `[DONE]`) has just delivered it with its own finish reason;
        // only an ending that had to be made up is a truncation. After a
        // side-decoder failure there is no telling.
        self.truncated = !ended_by_upstream && self.decode_error.is_none() && self.ended_in_error;

        if let Mode::Translate { encoder } = &mut self.mode {
            out.extend(encoder.finish());
        } else if self.truncated && self.report_truncation {
            let error = self.first_error.get_or_insert_with(|| {
                ApiError::upstream("upstream closed the stream before the response was complete")
            });
            let event = StreamEvent::Error(error.clone());
            out.extend(self.render_error(&event));
        }
        self.events_out += out.len() as u64;
        out
    }

    /// Terminates the client stream with an in-stream error: the idle timeout
    /// fired, the upstream connection broke, the gateway is shutting down.
    /// Returns the events to send; the transcoder is closed afterwards.
    ///
    /// * **Translation:** encodes `StreamEvent::Error(error)` and then calls
    ///   `encoder.finish()`. The encoder knows which blocks are open and
    ///   renders its protocol's error shape accordingly.
    /// * **Passthrough:** there is no client-side encoder (events were
    ///   forwarded verbatim), so the error event is rendered by a throwaway
    ///   encoder created from the codec. That encoder has not seen the
    ///   stream: it **cannot close a content block the upstream left open**
    ///   and any per-stream numbering it emits (Responses
    ///   `sequence_number`) starts from zero. Every protocol's error event
    ///   is self-contained, so clients treat it as the end of the stream.
    ///
    /// If the stream already ended with a terminal event, no error is added
    /// (it would follow the protocol's final event) and `error` is not
    /// recorded: translation mode returns only the encoder's terminators,
    /// passthrough mode returns nothing. Returns nothing if the transcoder is
    /// already closed.
    pub fn fail(&mut self, mut error: ApiError) -> Vec<SseEvent> {
        if self.closed {
            return Vec::new();
        }
        self.closed = true;
        self.scrub_error(&mut error);
        let out = self.end_with(error);
        self.events_out += out.len() as u64;
        out
    }

    /// The response as accumulated so far, including a block that is still
    /// open. `model` is the model the upstream reported.
    pub fn response_snapshot(&self) -> Response {
        self.accumulator.snapshot()
    }

    /// Consumes the transcoder and returns the accumulated response.
    pub fn into_response(self) -> Response {
        self.accumulator.into_response()
    }

    /// The accumulator the stream is folded into (for
    /// [`crate::reasoning_store::ReasoningStore::remember_accumulator`]).
    pub fn accumulator(&self) -> &Accumulator {
        &self.accumulator
    }

    /// Token usage reported by the upstream so far.
    pub fn usage(&self) -> Usage {
        self.accumulator.usage()
    }

    /// True once an upstream event has been decoded into at least one
    /// canonical event — the upstream has really started answering. Events
    /// the decoder skips (keep-alives, unknown types, `[DONE]`) do not count,
    /// and neither do events synthesised by [`Transcoder::finish`] or
    /// [`Transcoder::fail`].
    ///
    /// In passthrough mode it also becomes true when the side decoder gives
    /// up on the stream ([`Transcoder::decode_error`]): the events are
    /// forwarded regardless and the decoder will never report a first event,
    /// so a caller that holds output back until this turns true is released.
    pub fn saw_first_event(&self) -> bool {
        self.saw_first_event
    }

    /// True when the upstream closed the stream before its terminal event,
    /// so that the ending had to be made up. Known once
    /// [`Transcoder::finish`] has run; false before that, after
    /// [`Transcoder::fail`] (the gateway ended the stream, not the upstream),
    /// and after a passthrough side-decoder failure (unknowable).
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// True once a terminal canonical event (`Finish` or `Error`) has been
    /// seen or synthesised.
    pub fn is_terminal(&self) -> bool {
        self.accumulator.finished()
    }

    /// The error that ended the stream: an upstream in-stream error event, a
    /// decode failure in translation mode, the error passed to
    /// [`Transcoder::fail`], or one [`Transcoder::finish`] had to make up (a
    /// stream that closed before it started and that the decoder did not
    /// complete; with [`Transcoder::report_truncation`], a truncated
    /// passthrough stream). `None` for a stream that ended with a `Finish`
    /// event — including `Finish { reason: Error }`, see
    /// [`Transcoder::failed`] — and for an error that came too late to end
    /// the stream, after its terminal event.
    pub fn error(&self) -> Option<&ApiError> {
        self.first_error.as_ref()
    }

    /// True when the stream did not complete normally: it ended with an
    /// error event, or with `Finish { reason: Error }` (which is how a
    /// truncated stream is completed).
    pub fn failed(&self) -> bool {
        self.ended_in_error
    }

    /// The decoder's verdict that the upstream stream is unusable, if it
    /// reached one. In passthrough mode this only means that accounting
    /// stopped at that point.
    pub fn decode_error(&self) -> Option<&CodecError> {
        self.decode_error.as_ref()
    }

    /// Whether this transcoder forwards events verbatim.
    pub fn is_passthrough(&self) -> bool {
        matches!(self.mode, Mode::Passthrough { .. })
    }

    /// Whether [`Transcoder::finish`] or [`Transcoder::fail`] has run (or a
    /// decode failure ended a translated stream).
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Number of upstream wire events accepted by [`Transcoder::push`].
    pub fn events_in(&self) -> u64 {
        self.events_in
    }

    /// Number of client wire events returned so far by [`Transcoder::push`],
    /// [`Transcoder::finish`] and [`Transcoder::fail`].
    pub fn events_out(&self) -> u64 {
        self.events_out
    }

    /// Turns rewritten tool names back into the client's (translation mode
    /// with a name map only).
    fn restore_tool_names(&self, events: &mut [StreamEvent]) {
        if let Some(names) = &self.tool_names {
            for event in events {
                names.restore_event(event);
            }
        }
    }

    /// `text` without the upstream's credentials, as far as a scrubber was
    /// given.
    fn scrubbed(&self, text: &str) -> String {
        match &self.scrub {
            Some(scrub) => scrub(text),
            None => text.to_string(),
        }
    }

    /// Removes the upstream's credentials from an error's texts.
    fn scrub_error(&self, error: &mut ApiError) {
        let Some(scrub) = &self.scrub else {
            return;
        };
        error.message = scrub(&error.message);
        for text in [&mut error.code, &mut error.param].into_iter().flatten() {
            *text = scrub(text);
        }
    }

    /// [`Transcoder::scrub_error`] for every error event among `events`.
    fn scrub_errors(&self, events: &mut [StreamEvent]) {
        if self.scrub.is_none() {
            return;
        }
        for event in events {
            if let StreamEvent::Error(error) = event {
                self.scrub_error(error);
            }
        }
    }

    /// Records one canonical event. Returns whether it belongs to the stream
    /// (and should be encoded); events after the terminal one do not.
    fn absorb(&mut self, event: &StreamEvent, from_upstream: bool) -> bool {
        if self.accumulator.finished() {
            // Nothing may follow the terminal event on the wire, but a usage
            // report that arrives late is still worth counting.
            if matches!(event, StreamEvent::Usage(_)) {
                self.accumulator.push(event);
            }
            return false;
        }
        if from_upstream {
            self.saw_first_event = true;
        }
        match event {
            StreamEvent::Start { model, .. } => self.upstream_model = model.clone(),
            StreamEvent::BlockStart { index, .. } => self.open_block = Some(*index),
            StreamEvent::BlockStop { .. } => self.open_block = None,
            StreamEvent::Finish { reason, .. } => {
                self.open_block = None;
                self.ended_in_error = *reason == FinishReason::Error;
            }
            StreamEvent::Error(error) => {
                self.open_block = None;
                self.ended_in_error = true;
                if self.first_error.is_none() {
                    self.first_error = Some(error.clone());
                }
            }
            _ => {}
        }
        self.accumulator.push(event);
        true
    }

    /// Absorbs canonical events and, in translation mode, encodes them.
    fn encode_all(&mut self, events: &[StreamEvent], from_upstream: bool) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for event in events {
            if self.absorb(event, from_upstream)
                && let Mode::Translate { encoder } = &mut self.mode
            {
                out.extend(encoder.encode(event));
            }
        }
        out
    }

    /// Ends the client stream because of `error`. The one place that decides
    /// whether an error event may still be sent: if the stream already has
    /// its terminal event, an error would follow the protocol's final event,
    /// so only the encoder's terminators are returned and `error` is not
    /// recorded.
    fn end_with(&mut self, error: ApiError) -> Vec<SseEvent> {
        if self.accumulator.finished() {
            return match &mut self.mode {
                Mode::Translate { encoder } => encoder.finish(),
                Mode::Passthrough { .. } => Vec::new(),
            };
        }
        let event = StreamEvent::Error(error);
        self.absorb(&event, false);
        self.render_error(&event)
    }

    /// The wire form of an error event that ends the client stream, followed
    /// by the protocol's terminators.
    fn render_error(&mut self, event: &StreamEvent) -> Vec<SseEvent> {
        match &mut self.mode {
            Mode::Translate { encoder } => {
                let mut out = encoder.encode(event);
                out.extend(encoder.finish());
                out
            }
            Mode::Passthrough {
                codec,
                client_model,
            } => {
                let model = client_model
                    .clone()
                    .unwrap_or_else(|| self.upstream_model.clone());
                let mut encoder = codec.get().stream_encoder(&ClientCtx::new(model));
                let mut out = encoder.encode(event);
                out.extend(encoder.finish());
                out
            }
        }
    }

    /// The event to forward in passthrough mode: the original, with the
    /// model renamed and — when it is `about_failure` — the upstream's
    /// credentials removed.
    fn forward(&self, event: &SseEvent, about_failure: bool) -> SseEvent {
        let mut out = match &self.mode {
            Mode::Passthrough {
                codec,
                client_model: Some(model),
            } => match rewrite_model_text(codec.get(), &event.data, model) {
                Some(data) => SseEvent {
                    event: event.event.clone(),
                    data,
                },
                None => event.clone(),
            },
            _ => event.clone(),
        };
        if about_failure && let Some(scrub) = &self.scrub {
            out.data = scrub(&out.data);
        }
        out
    }
}

/// Whether a canonical event reports that the response failed: an error
/// event, or a terminal event with reason `Error` (whose wire form may carry
/// the upstream's description of what went wrong).
fn is_failure(event: &StreamEvent) -> bool {
    matches!(
        event,
        StreamEvent::Error(_)
            | StreamEvent::Finish {
                reason: FinishReason::Error,
                ..
            }
    )
}

/// Rewrites the model name inside a response payload given as JSON *text* —
/// the data of one stream event, or a complete response body — to `model`,
/// using `Codec::rewrite_response_model` to decide what to change.
///
/// Returns `None` when the payload is to be forwarded as it is: it is not
/// JSON, carries no model field the codec knows, or already names `model`.
///
/// Otherwise returns the payload with **only the bytes of the changed values
/// replaced**. Everything else is copied verbatim — whitespace, key order,
/// string escapes and above all numbers, which a parse-and-print round trip
/// through [`serde_json::Value`] does not preserve (integers beyond 64 bits
/// become floats, floats are reformatted). The gateway's non-streaming
/// passthrough path can use this on the upstream body for the same reason.
///
/// In the unusual case that the change cannot be expressed as in-place
/// replacements (the payload spells the model key twice in one object) the
/// rewritten value is printed compactly instead; this crate builds
/// `serde_json` with `float_roundtrip` so that even then every float keeps
/// its value.
pub fn rewrite_model_text(codec: &dyn Codec, data: &str, model: &str) -> Option<String> {
    if !may_carry_model(data) {
        return None;
    }
    let original: Value = serde_json::from_str(data).ok()?;
    let mut rewritten = original.clone();
    codec.rewrite_response_model(&mut rewritten, model);
    if rewritten == original {
        return None;
    }
    Some(splice(data, &original, &rewritten).unwrap_or_else(|| rewritten.to_string()))
}

/// Cheap pre-check that saves parsing most events of a stream: only a JSON
/// container can hold a model field, and every protocol spells that field
/// with the word "model" (`model`, `modelVersion`).
fn may_carry_model(data: &str) -> bool {
    let trimmed = data.trim_start();
    (trimmed.starts_with('{') || trimmed.starts_with('['))
        && (data.contains("model") || data.contains("Model"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{CORRUPT, FakeCodec, FakeDecoder, LazyDecoder, unwire, wire};
    use pretty_assertions::assert_eq;
    use switchyard_core::error::ErrorKind;
    use switchyard_core::ir::{Part, Reasoning, Signature};
    use switchyard_core::protocol::Protocol;
    use switchyard_core::stream::{BlockStart, response_to_events, validate_sequence};

    const UP: &str = "a";
    const DOWN: &str = "b";

    fn sample() -> Response {
        let mut response = Response::new("resp_1", "upstream-model");
        response.created = 1_700_000_000;
        response.parts = vec![
            Part::Reasoning(Reasoning {
                id: None,
                text: "let me think".into(),
                signature: Some(Signature::new(Protocol::Anthropic, "sig")),
                redacted: false,
            }),
            Part::text("Hello world"),
            Part::tool_call("call_1", "lookup", "{\"q\":\"x\"}"),
        ];
        response.finish = FinishReason::ToolCalls;
        response.usage = Usage {
            input_tokens: 12,
            output_tokens: 34,
            reasoning_tokens: 5,
            ..Usage::default()
        };
        response
    }

    fn start() -> StreamEvent {
        StreamEvent::Start {
            id: "resp_1".into(),
            model: "upstream-model".into(),
            created: 1_700_000_000,
        }
    }

    fn text_start(index: u32) -> StreamEvent {
        StreamEvent::BlockStart {
            index,
            block: BlockStart::Text,
        }
    }

    fn text_delta(index: u32, text: &str) -> StreamEvent {
        StreamEvent::TextDelta {
            index,
            text: text.into(),
        }
    }

    fn finish_stop() -> StreamEvent {
        StreamEvent::Finish {
            reason: FinishReason::Stop,
            stop_sequence: None,
        }
    }

    fn finish_error() -> StreamEvent {
        StreamEvent::Finish {
            reason: FinishReason::Error,
            stop_sequence: None,
        }
    }

    fn done() -> SseEvent {
        SseEvent::data("[DONE]")
    }

    fn translator() -> Transcoder {
        let client = FakeCodec::new(Protocol::Anthropic, DOWN);
        Transcoder::translate(
            Box::new(FakeDecoder::default()),
            client.stream_encoder(&ClientCtx::new("alias")),
        )
    }

    fn lazy_translator() -> Transcoder {
        let client = FakeCodec::new(Protocol::Anthropic, DOWN);
        Transcoder::translate(
            Box::new(LazyDecoder),
            client.stream_encoder(&ClientCtx::new("alias")),
        )
    }

    fn forwarder(client_model: Option<&str>) -> Transcoder {
        Transcoder::passthrough(
            Box::new(FakeDecoder::default()),
            Arc::new(FakeCodec::new(Protocol::OpenaiChat, UP)),
            client_model.map(str::to_string),
        )
    }

    fn push_all(transcoder: &mut Transcoder, events: &[StreamEvent]) -> Vec<SseEvent> {
        events
            .iter()
            .flat_map(|event| transcoder.push(&wire(UP, event)))
            .collect()
    }

    /// The canonical events carried by fake wire events (terminators such as
    /// `[DONE]` are skipped).
    fn canonical(events: &[SseEvent]) -> Vec<StreamEvent> {
        events.iter().filter_map(unwire).collect()
    }

    /// `events` as the client sees them: the encoder reports the alias.
    fn aliased(events: &[StreamEvent]) -> Vec<StreamEvent> {
        events
            .iter()
            .cloned()
            .map(|event| match event {
                StreamEvent::Start { id, created, .. } => StreamEvent::Start {
                    id,
                    model: "alias".into(),
                    created,
                },
                other => other,
            })
            .collect()
    }

    // ----- translation -----------------------------------------------------

    #[test]
    fn translates_a_complete_stream() {
        let events = response_to_events(&sample());
        let mut t = translator();
        let mut out = push_all(&mut t, &events);
        assert!(t.is_terminal());
        out.extend(t.finish());

        // One client event per canonical event, re-tagged, then the terminator.
        assert_eq!(out.len(), events.len() + 1);
        assert_eq!(out.last(), Some(&done()));
        assert!(
            out[..out.len() - 1]
                .iter()
                .all(|e| e.event.as_deref() == Some(DOWN))
        );
        let seen = canonical(&out);
        assert_eq!(seen, aliased(&events));
        validate_sequence(&seen).unwrap();

        // The accumulated response is the upstream's own.
        assert_eq!(t.response_snapshot(), sample());
        assert_eq!(t.usage(), sample().usage);
        assert!(t.saw_first_event());
        assert!(t.is_terminal());
        assert!(!t.failed());
        assert_eq!(t.error(), None);
        assert_eq!(t.decode_error(), None);
        assert_eq!(t.events_in(), events.len() as u64);
        assert_eq!(t.events_out(), events.len() as u64 + 1);
        assert!(!t.is_passthrough());
        assert!(t.is_closed());
        assert_eq!(t.into_response(), sample());
    }

    #[test]
    fn output_is_produced_incrementally() {
        let mut t = translator();
        assert_eq!(canonical(&t.push(&wire(UP, &start()))), aliased(&[start()]));
        assert_eq!(
            canonical(&t.push(&wire(UP, &text_start(0)))),
            vec![text_start(0)]
        );
        assert_eq!(
            canonical(&t.push(&wire(UP, &text_delta(0, "Hel")))),
            vec![text_delta(0, "Hel")]
        );
        // The open block is part of the snapshot.
        assert_eq!(t.response_snapshot().text(), "Hel");
        assert!(!t.is_terminal());
        assert_eq!(t.events_in(), 3);
        assert_eq!(t.events_out(), 3);
    }

    #[test]
    fn events_the_decoder_skips_produce_nothing() {
        let mut t = translator();
        for skipped in [
            done(),
            SseEvent::data(""),
            SseEvent::data("{\"type\":\"ping\"}"),
            SseEvent::named("ping", "{}"),
            SseEvent::data("plain text"),
        ] {
            assert_eq!(t.push(&skipped), Vec::new());
        }
        assert!(!t.saw_first_event());
        assert_eq!(t.events_in(), 5);
        assert_eq!(t.events_out(), 0);
        t.push(&wire(UP, &start()));
        assert!(t.saw_first_event());
    }

    #[test]
    fn upstream_done_marker_is_not_duplicated() {
        let mut t = translator();
        let mut out = push_all(&mut t, &[start(), finish_stop()]);
        out.extend(t.push(&done()));
        out.extend(t.finish());
        assert_eq!(out.iter().filter(|e| e.is_done_marker()).count(), 1);
    }

    #[test]
    fn truncated_stream_is_completed_by_the_decoder() {
        let mut t = translator();
        push_all(
            &mut t,
            &[start(), text_start(0), text_delta(0, "partial answ")],
        );
        assert!(!t.is_terminal());
        let tail = t.finish();
        assert_eq!(
            canonical(&tail),
            vec![StreamEvent::BlockStop { index: 0 }, finish_error()]
        );
        assert_eq!(tail.last(), Some(&done()));
        assert!(t.is_terminal());
        assert!(t.failed());
        // A truncation is not an error *event*.
        assert_eq!(t.error(), None);
        let response = t.response_snapshot();
        assert_eq!(response.text(), "partial answ");
        assert_eq!(response.finish, FinishReason::Error);
    }

    #[test]
    fn truncated_stream_is_completed_even_if_the_decoder_does_not() {
        let mut t = lazy_translator();
        let mut out = push_all(&mut t, &[start(), text_start(0), text_delta(0, "par")]);
        out.extend(t.finish());
        let seen = canonical(&out);
        assert_eq!(
            &seen[3..],
            &[StreamEvent::BlockStop { index: 0 }, finish_error()]
        );
        validate_sequence(&seen).unwrap();
        assert_eq!(out.last(), Some(&done()));
        assert!(t.failed());
        assert_eq!(t.response_snapshot().finish, FinishReason::Error);
    }

    #[test]
    fn truncated_between_blocks_needs_no_block_stop() {
        let mut t = lazy_translator();
        push_all(
            &mut t,
            &[
                start(),
                text_start(0),
                text_delta(0, "x"),
                StreamEvent::BlockStop { index: 0 },
            ],
        );
        assert_eq!(canonical(&t.finish()), vec![finish_error()]);
    }

    #[test]
    fn a_stream_that_never_started_ends_with_an_error_event() {
        let mut t = lazy_translator();
        let out = t.finish();
        let seen = canonical(&out);
        assert_eq!(seen.len(), 1);
        match &seen[0] {
            StreamEvent::Error(error) => {
                assert_eq!(error.kind, ErrorKind::Upstream);
                assert_eq!(error.status, 502);
            }
            other => panic!("expected an error event, got {other:?}"),
        }
        assert_eq!(out.last(), Some(&done()));
        assert!(!t.saw_first_event());
        assert!(t.is_terminal());
        assert!(t.failed());
        assert_eq!(t.error().map(|e| e.status), Some(502));
    }

    #[test]
    fn an_empty_stream_with_a_compliant_decoder_still_terminates() {
        let mut t = translator();
        let out = t.finish();
        assert_eq!(out.last(), Some(&done()));
        assert!(t.is_terminal());
        assert!(t.failed());
        assert!(!t.saw_first_event());
        assert_eq!(t.events_in(), 0);
    }

    #[test]
    fn upstream_error_event_is_translated_and_recorded() {
        let mut t = translator();
        let error = ApiError::rate_limit("slow down").with_code("rate_limited");
        let out = push_all(
            &mut t,
            &[
                start(),
                text_start(0),
                text_delta(0, "so"),
                StreamEvent::Error(error.clone()),
            ],
        );
        assert_eq!(
            canonical(&out).last(),
            Some(&StreamEvent::Error(error.clone()))
        );
        assert_eq!(t.error(), Some(&error));
        assert!(t.is_terminal());
        assert!(t.failed());
        // Only the protocol terminator is left to send.
        assert_eq!(t.finish(), vec![done()]);
        assert_eq!(t.response_snapshot().finish, FinishReason::Error);
        assert_eq!(t.response_snapshot().text(), "so");
    }

    #[test]
    fn an_error_as_the_very_first_event_is_visible_before_anything_is_sent() {
        // The bootstrap case: the gateway inspects `error()` and retries.
        let mut t = translator();
        let error = ApiError::unavailable("overloaded");
        let out = t.push(&wire(UP, &StreamEvent::Error(error.clone())));
        assert_eq!(out.len(), 1);
        assert!(t.saw_first_event());
        assert_eq!(t.error(), Some(&error));
        assert!(t.is_terminal());
    }

    #[test]
    fn only_the_first_error_is_kept() {
        let mut t = translator();
        let first = ApiError::upstream("first");
        push_all(&mut t, &[start(), StreamEvent::Error(first.clone())]);
        assert_eq!(t.fail(ApiError::timeout("second")), vec![done()]);
        assert_eq!(t.error(), Some(&first));
    }

    #[test]
    fn events_after_the_terminal_event_are_dropped() {
        let mut t = translator();
        push_all(&mut t, &[start(), finish_stop()]);
        let late = push_all(
            &mut t,
            &[
                text_start(0),
                text_delta(0, "ghost"),
                finish_error(),
                StreamEvent::Error(ApiError::upstream("late")),
            ],
        );
        assert_eq!(late, Vec::new());
        assert_eq!(t.response_snapshot().text(), "");
        assert_eq!(t.response_snapshot().finish, FinishReason::Stop);
        assert_eq!(t.error(), None);
        assert!(!t.failed());
    }

    #[test]
    fn late_usage_is_counted_but_not_sent() {
        let mut t = translator();
        push_all(&mut t, &[start(), finish_stop()]);
        let usage = Usage {
            input_tokens: 9,
            output_tokens: 3,
            ..Usage::default()
        };
        assert_eq!(t.push(&wire(UP, &StreamEvent::Usage(usage))), Vec::new());
        assert_eq!(t.usage(), usage);
        assert_eq!(t.finish(), vec![done()]);
    }

    #[test]
    fn usage_is_merged_as_running_totals() {
        let mut t = translator();
        push_all(
            &mut t,
            &[
                start(),
                StreamEvent::Usage(Usage {
                    input_tokens: 100,
                    cache_read_tokens: 20,
                    ..Usage::default()
                }),
                StreamEvent::Usage(Usage {
                    output_tokens: 7,
                    ..Usage::default()
                }),
                StreamEvent::Usage(Usage {
                    output_tokens: 15,
                    ..Usage::default()
                }),
            ],
        );
        assert_eq!(
            t.usage(),
            Usage {
                input_tokens: 100,
                cache_read_tokens: 20,
                output_tokens: 15,
                ..Usage::default()
            }
        );
    }

    #[test]
    fn an_undecodable_stream_ends_with_an_in_stream_error() {
        let mut t = translator();
        push_all(&mut t, &[start(), text_start(0), text_delta(0, "abc")]);
        let out = t.push(&SseEvent::data(CORRUPT));
        let seen = canonical(&out);
        assert_eq!(seen.len(), 1);
        match &seen[0] {
            StreamEvent::Error(error) => {
                assert_eq!(error.status, 502);
                assert_eq!(error.kind, ErrorKind::Upstream);
                assert!(error.message.contains("corrupt frame"), "{}", error.message);
            }
            other => panic!("expected an error event, got {other:?}"),
        }
        assert_eq!(out.last(), Some(&done()));
        assert_eq!(
            t.decode_error(),
            Some(&CodecError::upstream("corrupt frame"))
        );
        assert_eq!(t.error().map(|e| e.status), Some(502));
        assert!(t.is_terminal() && t.failed() && t.is_closed());
        // What was received before the failure is kept.
        assert_eq!(t.response_snapshot().text(), "abc");

        // The stream is over: nothing else comes out.
        assert_eq!(t.push(&wire(UP, &text_delta(0, "more"))), Vec::new());
        assert_eq!(t.finish(), Vec::new());
        assert_eq!(t.fail(ApiError::timeout("x")), Vec::new());
        assert_eq!(t.events_in(), 4);
        assert_eq!(t.events_out(), 5);
    }

    #[test]
    fn fail_terminates_a_translated_stream() {
        let mut t = translator();
        push_all(&mut t, &[start(), text_start(0), text_delta(0, "half")]);
        let error = ApiError::timeout("upstream sent nothing for 60s");
        let out = t.fail(error.clone());
        // The encoder receives the error with the block still open; closing
        // it is the encoder's business.
        assert_eq!(canonical(&out), vec![StreamEvent::Error(error.clone())]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event.as_deref(), Some(DOWN));
        assert_eq!(out[1], done());
        assert_eq!(t.error(), Some(&error));
        assert!(t.is_terminal() && t.failed());
        let response = t.response_snapshot();
        assert_eq!(response.text(), "half");
        assert_eq!(response.finish, FinishReason::Error);
        // Closed.
        assert_eq!(t.finish(), Vec::new());
        assert_eq!(t.fail(ApiError::internal("again")), Vec::new());
        assert_eq!(t.push(&wire(UP, &text_delta(0, "x"))), Vec::new());
        assert_eq!(t.events_out(), 5);
    }

    #[test]
    fn fail_before_any_event() {
        let mut t = translator();
        let error = ApiError::upstream("connection reset");
        let out = t.fail(error.clone());
        assert_eq!(canonical(&out), vec![StreamEvent::Error(error.clone())]);
        assert_eq!(out.last(), Some(&done()));
        // A locally generated error is not an upstream event.
        assert!(!t.saw_first_event());
        assert_eq!(t.error(), Some(&error));
    }

    #[test]
    fn fail_after_a_complete_stream_adds_no_error() {
        let mut t = translator();
        push_all(&mut t, &response_to_events(&sample()));
        assert_eq!(t.fail(ApiError::upstream("connection reset")), vec![done()]);
        assert_eq!(t.error(), None);
        assert!(!t.failed());
        assert_eq!(t.response_snapshot(), sample());
    }

    #[test]
    fn finish_is_idempotent_and_closes_the_transcoder() {
        let mut t = translator();
        push_all(&mut t, &response_to_events(&sample()));
        assert_eq!(t.finish(), vec![done()]);
        assert_eq!(t.finish(), Vec::new());
        assert_eq!(t.push(&wire(UP, &start())), Vec::new());
        let events = response_to_events(&sample()).len() as u64;
        assert_eq!(t.events_in(), events);
        assert_eq!(t.events_out(), events + 1);
    }

    // ----- passthrough -----------------------------------------------------

    #[test]
    fn passthrough_forwards_original_events() {
        let events = response_to_events(&sample());
        let mut t = forwarder(None);
        for event in &events {
            let wire_event = wire(UP, event);
            assert_eq!(t.push(&wire_event), vec![wire_event.clone()]);
        }
        let terminator = done();
        assert_eq!(t.push(&terminator), vec![terminator.clone()]);
        // Nothing extra at the end: the upstream's own terminator went out.
        assert_eq!(t.finish(), Vec::new());

        assert!(t.is_passthrough());
        assert_eq!(t.response_snapshot(), sample());
        assert_eq!(t.usage(), sample().usage);
        assert!(t.saw_first_event() && t.is_terminal() && !t.failed());
        assert_eq!(t.events_in(), events.len() as u64 + 1);
        assert_eq!(t.events_out(), events.len() as u64 + 1);
    }

    #[test]
    fn passthrough_rewrites_the_model_to_the_clients_name() {
        let mut t = forwarder(Some("alias"));
        let out = t.push(&wire(UP, &start()));
        assert_eq!(out.len(), 1);
        // The event name is kept, the data is compact JSON in the same order.
        assert_eq!(out[0].event.as_deref(), Some(UP));
        assert_eq!(
            out[0].data,
            r#"{"type":"start","id":"resp_1","model":"alias","created":1700000000}"#
        );
        // Accounting still sees what the upstream said.
        assert_eq!(t.response_snapshot().model, "upstream-model");
    }

    #[test]
    fn passthrough_leaves_events_without_a_model_byte_identical() {
        let mut t = forwarder(Some("alias"));
        t.push(&wire(UP, &start()));
        // Odd spacing and a text that happens to mention the word "model":
        // parsed, found unchanged, forwarded exactly as received.
        let spaced = SseEvent::named(
            UP,
            r#"{ "type": "text_delta",  "index": 0, "text": "which model?" }"#,
        );
        assert_eq!(t.push(&spaced), vec![spaced.clone()]);
        let plain = wire(UP, &text_start(0));
        assert_eq!(t.push(&plain), vec![plain.clone()]);
    }

    #[test]
    fn passthrough_does_not_reserialise_when_the_model_already_matches() {
        let mut t = forwarder(Some("alias"));
        let spaced =
            SseEvent::data(r#"{ "type": "start", "id": "x", "model": "alias", "created": 0 }"#);
        assert_eq!(t.push(&spaced), vec![spaced.clone()]);
    }

    #[test]
    fn passthrough_replaces_only_the_model_when_it_changed() {
        let mut t = forwarder(Some("alias"));
        let spaced = SseEvent::data(
            r#"{ "type": "start", "id": "x", "model": "gpt-upstream", "created": 0 }"#,
        );
        let out = t.push(&spaced);
        // The upstream's own formatting survives; only the name differs.
        assert_eq!(
            out,
            vec![SseEvent::data(
                r#"{ "type": "start", "id": "x", "model": "alias", "created": 0 }"#
            )]
        );
        // The side decoder still saw the upstream's event.
        assert_eq!(t.response_snapshot().model, "gpt-upstream");
    }

    #[test]
    fn passthrough_model_rewrite_does_not_touch_numbers() {
        // 1 ULP off is still a different number (and a 30-digit integer is
        // not a float): none of it may change because an alias is in use.
        let mut t = forwarder(Some("alias"));
        for number in [
            "-1.6596847772598267",
            "-9.643518239999999",
            "-12.504198559999999",
            "-19.701910220000002",
            "123456789012345678901234567890",
            "1.50",
            "1E5",
        ] {
            let event = SseEvent::data(format!(
                r#"{{"model":"upstream-model","avgLogprobs":{number}}}"#
            ));
            assert_eq!(
                t.push(&event),
                vec![SseEvent::data(format!(
                    r#"{{"model":"alias","avgLogprobs":{number}}}"#
                ))],
                "{number}"
            );
        }
    }

    #[test]
    fn passthrough_rewrite_keeps_multi_line_data_intact() {
        // An event whose JSON was spread over several `data:` lines.
        let mut t = forwarder(Some("alias"));
        let event = SseEvent::data("{\n\"type\": \"start\",\n\"model\": \"up\"\n}");
        assert_eq!(
            t.push(&event),
            vec![SseEvent::data(
                "{\n\"type\": \"start\",\n\"model\": \"alias\"\n}"
            )]
        );
    }

    #[test]
    fn passthrough_rewrites_nested_model_fields_the_codec_knows() {
        let mut t = forwarder(Some("alias"));
        let event = SseEvent::named(
            "response.completed",
            r#"{"type":"response.completed","response":{"id":"r","model":"up"}}"#,
        );
        let out = t.push(&event);
        assert_eq!(out[0].event.as_deref(), Some("response.completed"));
        assert_eq!(
            out[0].data,
            r#"{"type":"response.completed","response":{"id":"r","model":"alias"}}"#
        );
    }

    #[test]
    fn passthrough_forwards_non_json_data_untouched() {
        let mut t = forwarder(Some("alias"));
        for data in [
            "[DONE]",
            " [DONE] ",
            "",
            "model: not json",
            "{\"model\": truncated",
            "\"model\"",
            "12",
        ] {
            let event = SseEvent::data(data);
            assert_eq!(t.push(&event), vec![event.clone()], "{data:?}");
        }
        let named = SseEvent::named("ping", "");
        assert_eq!(t.push(&named), vec![named.clone()]);
    }

    #[test]
    fn passthrough_without_a_client_model_never_rewrites() {
        let mut t = forwarder(None);
        let event = wire(UP, &start());
        assert_eq!(t.push(&event), vec![event.clone()]);
    }

    #[test]
    fn passthrough_accepts_static_and_shared_codecs() {
        let leaked: &'static FakeCodec = Box::leak(Box::new(FakeCodec::new(Protocol::Gemini, UP)));
        let as_dyn: &'static dyn Codec = leaked;
        let shared: Arc<dyn Codec> = Arc::new(FakeCodec::new(Protocol::Gemini, UP));
        let transcoders = [
            Transcoder::passthrough(
                Box::new(FakeDecoder::default()),
                leaked,
                Some("alias".into()),
            ),
            Transcoder::passthrough(
                Box::new(FakeDecoder::default()),
                as_dyn,
                Some("alias".into()),
            ),
            Transcoder::passthrough(
                Box::new(FakeDecoder::default()),
                shared,
                Some("alias".into()),
            ),
        ];
        for mut t in transcoders {
            let out = t.push(&wire(UP, &start()));
            assert_eq!(canonical(&out), aliased(&[start()]));
        }
        assert_eq!(format!("{:?}", CodecRef::from(leaked)), "CodecRef(gemini)");
    }

    #[test]
    fn passthrough_swallows_decoder_failures_and_keeps_forwarding() {
        let mut t = forwarder(Some("alias"));
        push_all(
            &mut t,
            &[
                start(),
                StreamEvent::Usage(Usage {
                    input_tokens: 5,
                    ..Usage::default()
                }),
                text_start(0),
                text_delta(0, "seen"),
            ],
        );
        let corrupt = SseEvent::data(CORRUPT);
        assert_eq!(t.push(&corrupt), vec![corrupt.clone()]);
        assert_eq!(
            t.decode_error(),
            Some(&CodecError::upstream("corrupt frame"))
        );
        // Not a stream error: the client's stream is unaffected.
        assert_eq!(t.error(), None);
        assert!(!t.is_closed());

        // Later events are still forwarded (and still rewritten) …
        let later = wire(UP, &text_delta(0, " unseen"));
        assert_eq!(t.push(&later), vec![later.clone()]);
        let out = t.push(&wire(UP, &start()));
        assert_eq!(canonical(&out), aliased(&[start()]));
        // … but accounting stopped where the decoder gave up.
        assert_eq!(t.response_snapshot().text(), "seen");
        assert_eq!(t.usage().input_tokens, 5);
        assert_eq!(t.finish(), Vec::new());
        assert!(!t.is_terminal());
    }

    #[test]
    fn passthrough_truncated_stream_adds_nothing_but_accounts_for_it() {
        let mut t = forwarder(Some("alias"));
        push_all(&mut t, &[start(), text_start(0), text_delta(0, "cut of")]);
        assert!(!t.is_terminal());
        assert_eq!(t.finish(), Vec::new());
        assert!(t.is_terminal());
        assert!(t.failed());
        assert_eq!(t.error(), None);
        let response = t.response_snapshot();
        assert_eq!(response.text(), "cut of");
        assert_eq!(response.finish, FinishReason::Error);
        assert_eq!(t.events_out(), 3);
    }

    #[test]
    fn passthrough_upstream_error_event_is_forwarded_and_recorded() {
        let mut t = forwarder(Some("alias"));
        let error = ApiError::rate_limit("quota exceeded");
        let event = wire(UP, &StreamEvent::Error(error.clone()));
        assert_eq!(t.push(&event), vec![event.clone()]);
        assert!(t.saw_first_event());
        assert_eq!(t.error(), Some(&error));
        assert!(t.is_terminal() && t.failed());
    }

    #[test]
    fn passthrough_fail_renders_the_protocols_error_event() {
        let mut t = forwarder(Some("alias"));
        push_all(&mut t, &[start(), text_start(0), text_delta(0, "half")]);
        let error = ApiError::timeout("idle timeout");
        let out = t.fail(error.clone());
        // Rendered by a fresh encoder of the same protocol: the error event
        // and that protocol's terminator, and no attempt to close block 0.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event.as_deref(), Some(UP));
        assert_eq!(canonical(&out), vec![StreamEvent::Error(error.clone())]);
        assert_eq!(out[1], done());
        assert_eq!(t.error(), Some(&error));
        assert!(t.is_terminal() && t.failed() && t.is_closed());
        assert_eq!(t.response_snapshot().finish, FinishReason::Error);
        assert_eq!(t.fail(ApiError::internal("again")), Vec::new());
        assert_eq!(t.finish(), Vec::new());
        assert_eq!(t.events_out(), 5);
    }

    #[test]
    fn passthrough_fail_before_any_event_and_without_client_model() {
        let mut t = forwarder(None);
        let error = ApiError::upstream("connect failed");
        let out = t.fail(error.clone());
        assert_eq!(canonical(&out), vec![StreamEvent::Error(error)]);
        assert!(!t.saw_first_event());
    }

    #[test]
    fn passthrough_fail_after_the_upstream_finished_adds_nothing() {
        let mut t = forwarder(Some("alias"));
        push_all(&mut t, &response_to_events(&sample()));
        assert_eq!(t.fail(ApiError::upstream("connection reset")), Vec::new());
        assert_eq!(t.error(), None);
        assert!(!t.failed());
    }

    #[test]
    fn passthrough_fail_after_a_decoder_failure_still_reports_to_the_client() {
        let mut t = forwarder(None);
        push_all(&mut t, &[start()]);
        t.push(&SseEvent::data(CORRUPT));
        let out = t.fail(ApiError::timeout("idle"));
        assert_eq!(out.len(), 2);
        assert_eq!(t.error().map(|e| e.status), Some(504));
    }

    #[test]
    fn passthrough_first_event_is_a_decoded_event_not_a_keepalive() {
        let mut t = forwarder(Some("alias"));
        let ping = SseEvent::named("ping", "{\"type\":\"ping\"}");
        assert_eq!(t.push(&ping), vec![ping.clone()]);
        assert!(!t.saw_first_event());
        t.push(&wire(UP, &start()));
        assert!(t.saw_first_event());
    }

    #[test]
    fn passthrough_decoder_failure_opens_the_first_event_gate() {
        // The decoder gives up before it produced anything: the events are
        // forwarded anyway, so a caller waiting for the first event must be
        // released at once, not at the end of the stream.
        let mut t = forwarder(Some("alias"));
        let ping = SseEvent::named("ping", "{}");
        assert_eq!(t.push(&ping), vec![ping.clone()]);
        assert!(!t.saw_first_event());

        let corrupt = SseEvent::data(CORRUPT);
        assert_eq!(t.push(&corrupt), vec![corrupt.clone()]);
        assert!(t.saw_first_event());
        assert!(t.decode_error().is_some());
        // It is not an error of the stream, and nothing is known about it.
        assert_eq!(t.error(), None);
        assert!(!t.is_terminal() && !t.failed());

        let later = wire(UP, &text_start(0));
        assert_eq!(t.push(&later), vec![later.clone()]);
        assert!(t.saw_first_event());
        assert_eq!(t.finish(), Vec::new());
        assert!(!t.truncated());
    }

    // ----- one terminal event, whatever happens afterwards --------------------

    #[test]
    fn a_decode_failure_after_the_terminal_event_adds_no_error() {
        let mut t = translator();
        push_all(&mut t, &[start(), finish_stop()]);
        // Trailing garbage after a complete response: the client already has
        // the protocol's final event, only the terminator is left to send.
        assert_eq!(t.push(&SseEvent::data(CORRUPT)), vec![done()]);
        assert!(t.decode_error().is_some());
        assert_eq!(t.error(), None);
        assert!(!t.failed());
        assert!(t.is_closed());
        assert_eq!(t.response_snapshot().finish, FinishReason::Stop);
        // Closed: the terminator is not sent twice.
        assert_eq!(t.finish(), Vec::new());
        assert_eq!(t.fail(ApiError::timeout("late")), Vec::new());
        assert_eq!(t.events_out(), 3);
    }

    #[test]
    fn a_decode_failure_after_an_upstream_error_keeps_that_error() {
        let mut t = translator();
        let upstream = ApiError::rate_limit("slow down");
        push_all(&mut t, &[start(), StreamEvent::Error(upstream.clone())]);
        assert_eq!(t.push(&SseEvent::data(CORRUPT)), vec![done()]);
        assert_eq!(t.error(), Some(&upstream));
    }

    #[test]
    fn every_way_of_ending_a_translated_stream_yields_one_terminal_event() {
        type Ending = fn(&mut Transcoder) -> Vec<SseEvent>;
        let endings: [(&str, Ending); 4] = [
            ("finish", |t| t.finish()),
            ("fail", |t| t.fail(ApiError::timeout("idle"))),
            ("corrupt then finish", |t| {
                let mut out = t.push(&SseEvent::data(CORRUPT));
                out.extend(t.finish());
                out
            }),
            ("corrupt then fail", |t| {
                let mut out = t.push(&SseEvent::data(CORRUPT));
                out.extend(t.fail(ApiError::timeout("idle")));
                out
            }),
        ];
        let prefixes: [Vec<StreamEvent>; 5] = [
            vec![],
            vec![start()],
            vec![start(), text_start(0), text_delta(0, "x")],
            vec![start(), finish_stop()],
            vec![start(), StreamEvent::Error(ApiError::upstream("boom"))],
        ];
        for prefix in &prefixes {
            for (name, ending) in &endings {
                let mut t = translator();
                let mut out = push_all(&mut t, prefix);
                out.extend(ending(&mut t));
                let seen = canonical(&out);
                let terminals = seen
                    .iter()
                    .filter(|e| matches!(e, StreamEvent::Finish { .. } | StreamEvent::Error(_)))
                    .count();
                assert_eq!(terminals, 1, "{name} after {prefix:?}: {seen:?}");
                assert!(
                    matches!(
                        seen.last(),
                        Some(StreamEvent::Finish { .. } | StreamEvent::Error(_))
                    ),
                    "{name} after {prefix:?}: the terminal event is not last"
                );
                assert_eq!(
                    out.iter().filter(|e| e.is_done_marker()).count(),
                    1,
                    "{name} after {prefix:?}"
                );
                assert_eq!(out.last(), Some(&done()));
                assert!(t.is_terminal() && t.is_closed());
                // What the accessors say is what the client was told.
                let told_error = seen.iter().find_map(|e| match e {
                    StreamEvent::Error(error) => Some(error),
                    _ => None,
                });
                assert_eq!(t.error(), told_error, "{name} after {prefix:?}");
            }
        }
    }

    // ----- truncation ------------------------------------------------------------

    #[test]
    fn truncated_is_reported_after_finish() {
        // Translation: cut off in the middle of a block.
        let mut t = translator();
        push_all(&mut t, &[start(), text_start(0), text_delta(0, "cut")]);
        assert!(!t.truncated());
        t.finish();
        assert!(t.truncated());

        // A complete stream is not truncated …
        let mut t = translator();
        push_all(&mut t, &response_to_events(&sample()));
        t.finish();
        assert!(!t.truncated());

        // … and neither is one the upstream ended with an error of its own,
        let mut t = translator();
        push_all(
            &mut t,
            &[start(), StreamEvent::Error(ApiError::upstream("x"))],
        );
        t.finish();
        assert!(!t.truncated());

        // … one the upstream itself finished with reason `error`,
        let mut t = translator();
        push_all(&mut t, &[start(), finish_error()]);
        t.finish();
        assert!(t.failed());
        assert!(!t.truncated());

        // … or one the gateway gave up on.
        let mut t = translator();
        push_all(&mut t, &[start(), text_start(0)]);
        t.fail(ApiError::timeout("idle"));
        assert!(!t.truncated());
    }

    /// A decoder that, like the Chat Completions one, only knows the finish
    /// reason is final when the stream ends: it holds `Finish` back until
    /// `finish()`.
    #[derive(Default)]
    struct DeferringDecoder {
        inner: FakeDecoder,
        held: Option<StreamEvent>,
    }

    impl StreamDecoder for DeferringDecoder {
        fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, CodecError> {
            let mut events = self.inner.decode(event)?;
            if let Some(at) = events
                .iter()
                .position(|e| matches!(e, StreamEvent::Finish { .. }))
            {
                self.held = Some(events.remove(at));
            }
            Ok(events)
        }

        fn finish(&mut self) -> Vec<StreamEvent> {
            match self.held.take() {
                Some(finish) => vec![finish],
                None => self.inner.finish(),
            }
        }
    }

    fn deferring_forwarder(report: bool) -> Transcoder {
        Transcoder::passthrough(
            Box::new(DeferringDecoder::default()),
            Arc::new(FakeCodec::new(Protocol::OpenaiChat, UP)),
            Some("alias".into()),
        )
        .report_truncation(report)
    }

    #[test]
    fn a_terminal_event_the_decoder_held_back_is_not_a_truncation() {
        // The upstream sent its finish reason but no `[DONE]`: complete.
        for report in [false, true] {
            let mut t = deferring_forwarder(report);
            push_all(
                &mut t,
                &[
                    start(),
                    text_start(0),
                    text_delta(0, "all of it"),
                    StreamEvent::BlockStop { index: 0 },
                    finish_stop(),
                ],
            );
            // Not known to be complete until the stream ends.
            assert!(!t.is_terminal());
            assert_eq!(t.finish(), Vec::new(), "report = {report}");
            assert!(t.is_terminal());
            assert!(!t.truncated() && !t.failed());
            assert_eq!(t.error(), None);
            assert_eq!(t.response_snapshot().finish, FinishReason::Stop);
        }
    }

    #[test]
    fn passthrough_reports_a_truncated_stream_when_asked_to() {
        let mut t = forwarder(Some("alias")).report_truncation(true);
        push_all(&mut t, &[start(), text_start(0), text_delta(0, "cut of")]);
        let out = t.finish();
        // The protocol's error event and terminator, as `fail` renders them.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event.as_deref(), Some(UP));
        let seen = canonical(&out);
        match seen.as_slice() {
            [StreamEvent::Error(error)] => {
                assert_eq!(error.status, 502);
                assert_eq!(error.kind, ErrorKind::Upstream);
                assert_eq!(t.error(), Some(error));
            }
            other => panic!("expected one error event, got {other:?}"),
        }
        assert_eq!(out[1], done());
        assert!(t.truncated() && t.failed() && t.is_terminal());
        assert_eq!(t.response_snapshot().text(), "cut of");
        assert_eq!(t.events_out(), 5);
        assert_eq!(t.finish(), Vec::new());
    }

    #[test]
    fn passthrough_truncation_report_leaves_complete_streams_alone() {
        // Complete.
        let mut t = forwarder(Some("alias")).report_truncation(true);
        push_all(&mut t, &response_to_events(&sample()));
        assert_eq!(t.finish(), Vec::new());
        assert!(!t.truncated());

        // Ended by the upstream's own error event, which was forwarded.
        let mut t = forwarder(Some("alias")).report_truncation(true);
        let error = ApiError::rate_limit("quota");
        push_all(&mut t, &[start(), StreamEvent::Error(error.clone())]);
        assert_eq!(t.finish(), Vec::new());
        assert_eq!(t.error(), Some(&error));

        // The side decoder gave up: no way to tell, so nothing is claimed.
        let mut t = forwarder(Some("alias")).report_truncation(true);
        push_all(&mut t, &[start(), text_start(0)]);
        t.push(&SseEvent::data(CORRUPT));
        assert_eq!(t.finish(), Vec::new());
        assert!(!t.truncated());
        assert_eq!(t.error(), None);
    }

    #[test]
    fn passthrough_truncation_report_covers_a_stream_that_never_started() {
        let mut t = Transcoder::passthrough(
            Box::new(LazyDecoder),
            Arc::new(FakeCodec::new(Protocol::OpenaiChat, UP)),
            None,
        )
        .report_truncation(true);
        let out = t.finish();
        let seen = canonical(&out);
        assert_eq!(seen.len(), 1);
        assert_eq!(
            t.error(),
            match &seen[0] {
                StreamEvent::Error(error) => Some(error),
                other => panic!("expected an error event, got {other:?}"),
            }
        );
        assert!(t.truncated());
        assert!(!t.saw_first_event());
    }

    #[test]
    fn truncation_reporting_does_not_change_translation() {
        let mut plain = translator();
        let mut reporting = translator().report_truncation(true);
        let events = [start(), text_start(0), text_delta(0, "cut")];
        let mut a = push_all(&mut plain, &events);
        let mut b = push_all(&mut reporting, &events);
        a.extend(plain.finish());
        b.extend(reporting.finish());
        assert_eq!(a, b);
        assert_eq!(reporting.error(), None);
    }

    // ----- helpers ---------------------------------------------------------

    #[test]
    fn model_precheck() {
        assert!(may_carry_model(r#"{"model":"x"}"#));
        assert!(may_carry_model(r#"  {"modelVersion":"x"}"#));
        assert!(may_carry_model(r#"[{"Model":"x"}]"#));
        assert!(!may_carry_model(r#"{"type":"content_block_delta"}"#));
        assert!(!may_carry_model("[DONE]"));
        assert!(!may_carry_model("model"));
        assert!(!may_carry_model(""));
    }

    #[test]
    fn rewrite_model_reports_only_real_changes() {
        let codec = FakeCodec::new(Protocol::OpenaiChat, UP);
        assert_eq!(
            rewrite_model_text(&codec, r#"{"model": "up"}"#, "alias").as_deref(),
            Some(r#"{"model": "alias"}"#)
        );
        assert_eq!(
            rewrite_model_text(&codec, r#"{"model": "alias"}"#, "alias"),
            None
        );
        // A non-string model is not a model name.
        assert_eq!(rewrite_model_text(&codec, r#"{"model": 5}"#, "alias"), None);
        assert_eq!(
            rewrite_model_text(&codec, r#"{"other": "model"}"#, "alias"),
            None
        );
        assert_eq!(rewrite_model_text(&codec, r#"{"model": "#, "alias"), None);
    }

    #[test]
    fn rewrite_preserves_key_order_and_other_fields() {
        let codec = FakeCodec::new(Protocol::OpenaiChat, UP);
        let data = r#"{"z":1,"model":"up","a":{"y":[1,2,{"k":null}],"b":"é\n"},"n":1.5}"#;
        assert_eq!(
            rewrite_model_text(&codec, data, "alias").as_deref(),
            Some(r#"{"z":1,"model":"alias","a":{"y":[1,2,{"k":null}],"b":"é\n"},"n":1.5}"#)
        );
    }

    #[test]
    fn rewrite_changes_nothing_but_the_model() {
        // Everything a parse-and-print round trip would normalise: floats
        // with 17 digits, exponents, a 30-digit integer, `-0`, trailing
        // zeros, escapes, odd spacing, a duplicated key that is not ours.
        let codec = FakeCodec::new(Protocol::OpenaiChat, UP);
        let data = concat!(
            r#"{ "avgLogprobs": -1.6596847772598267, "p":-9.643518239999999,"#,
            r#" "e": 1E5, "big": 123456789012345678901234567890, "z": -0,"#,
            r#" "t": 0.10, "s": "é\/", "d": 1, "d": 2,"#,
            "\n\t",
            r#""model" : "up" , "response": {"model":"up","x": 1.0e-7} }"#
        );
        let expected = data
            .replace(r#""model" : "up""#, r#""model" : "alias""#)
            .replace(r#"{"model":"up","#, r#"{"model":"alias","#);
        assert_eq!(
            rewrite_model_text(&codec, data, "alias").as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn rewrite_of_a_duplicated_model_key_falls_back_to_reprinting() {
        // Which of two equal keys a client honours is undefined, so neither
        // may keep the upstream's name: the payload is reprinted, which
        // leaves a single key. Floats keep their value even then.
        let codec = FakeCodec::new(Protocol::OpenaiChat, UP);
        assert_eq!(
            rewrite_model_text(
                &codec,
                r#"{"model": "a", "model": "b", "avgLogprobs": -1.6596847772598267}"#,
                "alias"
            )
            .as_deref(),
            Some(r#"{"model":"alias","avgLogprobs":-1.6596847772598267}"#)
        );
    }

    #[test]
    fn the_fallback_reprint_keeps_float_values() {
        // The property `float_roundtrip` buys (see Cargo.toml): parse then
        // print never changes the value of a float.
        for number in [
            "-1.6596847772598267",
            "-9.643518239999999",
            "-12.504198559999999",
            "-19.701910220000002",
            "0.1",
            "2.2250738585072014e-308",
            "1.7976931348623157e308",
        ] {
            let parsed: Value = serde_json::from_str(number).unwrap();
            let printed: f64 = parsed.to_string().parse().unwrap();
            let exact: f64 = number.parse().unwrap();
            assert_eq!(printed.to_bits(), exact.to_bits(), "{number}");
        }
    }

    #[test]
    fn rewrite_works_on_a_complete_response_body() {
        let codec = FakeCodec::new(Protocol::OpenaiChat, UP);
        let body = "{\n  \"id\": \"r\",\n  \"model\": \"up\",\n  \"usage\": {\"total\": 3}\n}";
        assert_eq!(
            rewrite_model_text(&codec, body, "alias").as_deref(),
            Some("{\n  \"id\": \"r\",\n  \"model\": \"alias\",\n  \"usage\": {\"total\": 3}\n}")
        );
    }

    // ----- tool names --------------------------------------------------------

    fn tool_start(index: u32, name: &str) -> StreamEvent {
        StreamEvent::BlockStart {
            index,
            block: BlockStart::ToolCall {
                id: format!("call_{index}"),
                name: name.into(),
                kind: Default::default(),
                signature: None,
            },
        }
    }

    /// The map for a request that declared `mcp.server:get-data` and was
    /// sent to an upstream that only accepts `[a-zA-Z0-9_-]`.
    fn renamed() -> ToolNames {
        use switchyard_core::ir::{FunctionTool, Request, Tool};
        let mut request = Request::new("m", Protocol::Gemini);
        request.tools.push(Tool::Function(FunctionTool {
            name: "mcp.server:get-data".into(),
            description: None,
            parameters: Value::Null,
            strict: None,
            cache_control: None,
        }));
        let names = crate::toolnames::sanitize_tool_names(&mut request, Protocol::OpenaiChat);
        assert_eq!(
            names.original("mcp_server_get-data"),
            Some("mcp.server:get-data")
        );
        names
    }

    #[test]
    fn translated_tool_calls_carry_the_clients_names() {
        let mut t = translator().with_tool_names(renamed());
        let out = push_all(
            &mut t,
            &[
                start(),
                tool_start(0, "mcp_server_get-data"),
                StreamEvent::ToolArgsDelta {
                    index: 0,
                    fragment: "{\"q\":\"mcp_server_get-data\"}".into(),
                },
                StreamEvent::BlockStop { index: 0 },
                tool_start(1, "untouched"),
                StreamEvent::BlockStop { index: 1 },
                StreamEvent::Finish {
                    reason: FinishReason::ToolCalls,
                    stop_sequence: None,
                },
            ],
        );
        let seen = canonical(&out);
        assert_eq!(seen[1], tool_start(0, "mcp.server:get-data"));
        // Arguments are data, not names.
        assert_eq!(
            seen[2],
            StreamEvent::ToolArgsDelta {
                index: 0,
                fragment: "{\"q\":\"mcp_server_get-data\"}".into(),
            }
        );
        assert_eq!(seen[4], tool_start(1, "untouched"));
        validate_sequence(&seen).unwrap();
        // The accumulated response agrees with what the client saw.
        let names: Vec<String> = t
            .response_snapshot()
            .tool_calls()
            .map(|call| call.name.clone())
            .collect();
        assert_eq!(names, vec!["mcp.server:get-data", "untouched"]);
    }

    #[test]
    fn tool_names_in_the_decoders_tail_are_restored_too() {
        // A decoder that buffers a tool call until the stream closes hands
        // its BlockStart over from `finish()`.
        struct Buffering(Vec<StreamEvent>);
        impl StreamDecoder for Buffering {
            fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, CodecError> {
                if let Some(canonical) = unwire(event) {
                    self.0.push(canonical);
                }
                Ok(Vec::new())
            }
            fn finish(&mut self) -> Vec<StreamEvent> {
                std::mem::take(&mut self.0)
            }
        }
        let client = FakeCodec::new(Protocol::Anthropic, DOWN);
        let mut t = Transcoder::translate(
            Box::new(Buffering(Vec::new())),
            client.stream_encoder(&ClientCtx::new("alias")),
        )
        .with_tool_names(renamed());
        push_all(
            &mut t,
            &[
                start(),
                tool_start(0, "mcp_server_get-data"),
                StreamEvent::BlockStop { index: 0 },
                finish_stop(),
            ],
        );
        let seen = canonical(&t.finish());
        assert_eq!(seen[1], tool_start(0, "mcp.server:get-data"));
    }

    #[test]
    fn tool_names_are_never_rewritten_in_passthrough() {
        let mut t = forwarder(None).with_tool_names(renamed());
        let event = wire(UP, &tool_start(0, "mcp_server_get-data"));
        push_all(&mut t, &[start()]);
        assert_eq!(t.push(&event), vec![event.clone()]);
        let names: Vec<String> = t
            .response_snapshot()
            .tool_calls()
            .map(|call| call.name.clone())
            .collect();
        assert_eq!(names, vec!["mcp_server_get-data"]);
    }

    #[test]
    fn an_empty_name_map_changes_nothing() {
        let mut t = translator().with_tool_names(ToolNames::new());
        let out = push_all(&mut t, &[start(), tool_start(0, "mcp_server_get-data")]);
        assert_eq!(canonical(&out)[1], tool_start(0, "mcp_server_get-data"));
    }

    // ----- scrubbing and filtering ------------------------------------------

    const KEY: &str = "sk-upstream-key-0123456789";

    fn scrub(text: &str) -> String {
        text.replace(KEY, "[redacted]")
    }

    fn leaking_error() -> ApiError {
        let mut error = ApiError::upstream(format!("worker crashed while serving key {KEY}"));
        error.code = Some(format!("bad_key_{KEY}"));
        error
    }

    #[test]
    fn a_translated_upstream_error_is_scrubbed_for_the_client_and_the_record() {
        let mut t = translator().with_scrubber(scrub);
        let mut out = push_all(
            &mut t,
            &[
                start(),
                text_start(0),
                text_delta(0, "Hel"),
                StreamEvent::Error(leaking_error()),
            ],
        );
        out.extend(t.finish());
        let wire_text: String = out.iter().map(|event| event.data.clone()).collect();
        assert!(!wire_text.contains(KEY), "{wire_text}");
        assert!(wire_text.contains("worker crashed while serving key [redacted]"));
        let recorded = t.error().expect("the error is recorded");
        assert_eq!(
            recorded.message,
            "worker crashed while serving key [redacted]"
        );
        assert_eq!(recorded.code.as_deref(), Some("bad_key_[redacted]"));
    }

    #[test]
    fn content_is_never_scrubbed() {
        // A self-hosted upstream whose "key" is an ordinary word: the model
        // must stay free to write it.
        let hide = |text: &str| text.replace("ollama", "[redacted]");
        let events = [start(), text_start(0), text_delta(0, "ollama is a server")];

        let mut translated = translator().with_scrubber(hide);
        let out = push_all(&mut translated, &events);
        assert_eq!(canonical(&out)[2], text_delta(0, "ollama is a server"));

        let mut forwarded = forwarder(None).with_scrubber(hide);
        let wire_event = wire(UP, &events[2]);
        push_all(&mut forwarded, &events[..2]);
        assert_eq!(forwarded.push(&wire_event), vec![wire_event.clone()]);
    }

    #[test]
    fn a_forwarded_upstream_error_event_is_scrubbed() {
        let mut t = forwarder(Some("alias")).with_scrubber(scrub);
        push_all(&mut t, &[start(), text_start(0), text_delta(0, "Hel")]);
        let event = wire(UP, &StreamEvent::Error(leaking_error()));
        let out = t.push(&event);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event, event.event);
        assert!(!out[0].data.contains(KEY), "{}", out[0].data);
        assert_eq!(out[0].data, event.data.replace(KEY, "[redacted]"));
        assert!(!t.error().expect("recorded").message.contains(KEY));
    }

    #[test]
    fn a_forwarded_error_finish_is_scrubbed() {
        // Some protocols end a failed response with a terminal event that
        // carries the upstream's description (Responses `response.failed`).
        // The fake decoder reads any JSON with these fields as that event.
        let mut t = forwarder(None).with_scrubber(scrub);
        push_all(&mut t, &[start()]);
        let mut value = serde_json::to_value(finish_error()).unwrap();
        value["detail"] = serde_json::json!(format!("key {KEY} was refused"));
        let event = SseEvent::named(UP, value.to_string());
        let out = t.push(&event);
        assert_eq!(out.len(), 1);
        assert!(!out[0].data.contains(KEY), "{}", out[0].data);
        assert!(t.failed());
    }

    #[test]
    fn after_a_side_decoder_failure_every_forwarded_event_is_scrubbed() {
        // The decoder can no longer tell an error from content, so nothing
        // that quotes the credential may pass.
        let mut t = forwarder(None).with_scrubber(scrub);
        push_all(&mut t, &[start()]);
        assert_eq!(t.push(&SseEvent::data(CORRUPT)).len(), 1);
        let later = SseEvent::data(format!("{{\"oops\":\"{KEY}\"}}"));
        let out = t.push(&later);
        assert_eq!(out[0].data, "{\"oops\":\"[redacted]\"}");
    }

    #[test]
    fn the_gateways_own_failure_and_an_undecodable_stream_are_scrubbed() {
        let mut t = translator().with_scrubber(scrub);
        push_all(&mut t, &[start()]);
        let out = t.fail(ApiError::upstream(format!("read failed for {KEY}")));
        let text: String = out.iter().map(|event| event.data.clone()).collect();
        assert!(!text.contains(KEY), "{text}");
        assert_eq!(t.error().unwrap().message, "read failed for [redacted]");

        let mut t = forwarder(None).with_scrubber(scrub);
        push_all(&mut t, &[start()]);
        let out = t.fail(ApiError::upstream(format!("read failed for {KEY}")));
        assert!(out.iter().all(|event| !event.data.contains(KEY)));
    }

    #[test]
    fn without_a_scrubber_errors_pass_as_they_are() {
        let mut t = translator();
        let out = push_all(&mut t, &[start(), StreamEvent::Error(leaking_error())]);
        assert_eq!(canonical(&out)[1], StreamEvent::Error(leaking_error()));
    }

    #[test]
    fn a_passthrough_filter_holds_events_back_but_still_counts_them() {
        let usage = StreamEvent::Usage(Usage {
            input_tokens: 11,
            output_tokens: 7,
            ..Usage::default()
        });
        let hidden = wire(UP, &usage);
        let hidden_data = hidden.data.clone();
        let mut t = forwarder(None).with_passthrough_filter(move |event| event.data != hidden_data);
        let out = push_all(
            &mut t,
            &[
                start(),
                text_start(0),
                text_delta(0, "hi"),
                StreamEvent::BlockStop { index: 0 },
                usage.clone(),
                finish_stop(),
            ],
        );
        assert_eq!(out.len(), 5, "the usage event is not forwarded");
        assert!(out.iter().all(|event| *event != hidden));
        assert_eq!(t.usage().input_tokens, 11);
        assert_eq!(t.usage().output_tokens, 7);
        assert_eq!(t.events_in(), 6);
        assert_eq!(t.events_out(), 5);
    }

    #[test]
    fn a_held_back_first_event_still_opens_the_bootstrap_gate() {
        let mut t = forwarder(None).with_passthrough_filter(|_| false);
        assert_eq!(push_all(&mut t, &[start()]), Vec::new());
        assert!(t.saw_first_event());
    }

    #[test]
    fn a_passthrough_filter_does_nothing_in_translation_mode() {
        let mut t = translator().with_passthrough_filter(|_| false);
        assert_eq!(push_all(&mut t, &[start()]).len(), 1);
    }

    #[test]
    fn transcoder_is_send_and_debug_hides_content() {
        fn assert_send<T: Send>() {}
        assert_send::<Transcoder>();
        let mut t = translator();
        push_all(&mut t, &[start(), text_start(0), text_delta(0, "SECRET")]);
        let debug = format!("{t:?}");
        assert!(debug.starts_with("Transcoder"));
        assert!(!debug.contains("SECRET"));
    }
}
