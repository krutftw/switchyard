//! Streaming: the bootstrap that decides whether an upstream stream is good
//! enough to commit to, and the pump task that then carries it to the
//! client.
//!
//! ```text
//! upstream bytes ─► SseParser ─► Transcoder ─► bounded channel ─► server ─► client
//! ```
//!
//! Until the transcoder has produced the first client-visible event nothing
//! has been sent, so a stream that dies or reports an error in that window
//! is just a failed attempt and another credential can be tried
//! ([`bootstrap`]). From the first event on the stream belongs to the
//! client: later failures are rendered in-band and the outcome is reported
//! when the stream ends ([`Pump`]).

use crate::gateway::Inner;
use crate::recorder::{CaptureBuf, DropOutcome, Recorder};
use crate::target::{bad_gateway, stream_failure};
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use switchyard_core::codec::StreamDecoder;
use switchyard_core::{CodecError, SseEvent, SseParser, StreamEvent, UpstreamError};
use switchyard_scheduler::{Lease, Outcome};
use switchyard_telemetry::{GaugeGuard, RecordError, error_kind_name};
use switchyard_translate::Transcoder;
use switchyard_upstream::Target;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Events buffered between the pump and the server. Small on purpose: when
/// the client reads slowly the pump stops reading from the upstream, and
/// TCP back-pressure does the rest.
pub(crate) const STREAM_CHANNEL_CAPACITY: usize = 64;

/// `streaming.idle_timeout_secs` as a limit; zero disables it.
pub(crate) fn idle_limit(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// The failure for an upstream that went silent.
pub(crate) fn idle_timeout(limit: Duration) -> UpstreamError {
    let mut error = UpstreamError::transport(format!(
        "timeout: the upstream sent nothing for {} s",
        limit.as_secs()
    ));
    // Reported to clients as a gateway timeout.
    error.status = 408;
    error
}

/// Whether a Chat Completions client asked for the usage chunk of its
/// stream (`stream_options.include_usage: true`).
pub(crate) fn asked_for_usage_chunk(body: &Value) -> bool {
    body.get("stream_options")
        .and_then(|options| options.get("include_usage"))
        == Some(&Value::Bool(true))
}

/// Whether a Chat Completions stream event is the usage-only chunk: the one
/// with `"choices": []` and the `usage` object, which the API sends just
/// before `[DONE]` to clients that set `stream_options.include_usage`.
///
/// The gateway asks every Chat upstream for it (it needs the numbers), so on
/// the passthrough path it has to be kept from clients that did not ask:
/// they are entitled to index `choices[0]` on every chunk.
pub(crate) fn is_usage_only_chunk(event: &SseEvent) -> bool {
    has_empty_choices(&event.data)
        && serde_json::from_str::<Value>(&event.data).is_ok_and(|chunk| {
            chunk
                .get("choices")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
                && chunk.get("usage").is_some_and(Value::is_object)
        })
}

/// Cheap pre-check for [`is_usage_only_chunk`], so that the ordinary chunks
/// of a stream are not parsed a second time: does the text contain
/// `"choices"` followed by an empty array?
fn has_empty_choices(data: &str) -> bool {
    const KEY: &str = "\"choices\"";
    data.match_indices(KEY).any(|(at, _)| {
        let mut rest = data[at + KEY.len()..]
            .chars()
            .filter(|c| !c.is_ascii_whitespace());
        rest.next() == Some(':') && rest.next() == Some('[') && rest.next() == Some(']')
    })
}

/// A decoder for canonical events carried as JSON, one per wire event. It
/// lets the mock provider — which produces canonical events directly — run
/// through the same [`Transcoder`] as a real upstream stream.
pub(crate) struct CanonicalDecoder;

impl StreamDecoder for CanonicalDecoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, CodecError> {
        serde_json::from_str::<StreamEvent>(&event.data)
            .map(|event| vec![event])
            .map_err(|error| CodecError::upstream(format!("not a canonical event: {error}")))
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        Vec::new()
    }
}

/// Where a stream's events come from.
pub(crate) enum Feed {
    /// A real upstream: bytes framed as server-sent events.
    Bytes {
        stream: BoxStream<'static, Result<Bytes, UpstreamError>>,
        parser: SseParser,
    },
    /// The mock provider: canonical events, wrapped for [`CanonicalDecoder`].
    Canonical(BoxStream<'static, StreamEvent>),
}

/// One step of a [`Feed`].
pub(crate) enum FeedItem {
    /// Wire events, with the raw bytes they were parsed from.
    Events(Vec<SseEvent>, Option<Bytes>),
    /// The source ended; these events were still buffered.
    End(Vec<SseEvent>),
    /// The source failed.
    Error(UpstreamError),
}

impl Feed {
    pub(crate) fn bytes(stream: BoxStream<'static, Result<Bytes, UpstreamError>>) -> Self {
        Feed::Bytes {
            stream,
            parser: SseParser::new(),
        }
    }

    /// The next step. Must not be called again after `End` or `Error`.
    pub(crate) async fn next(&mut self) -> FeedItem {
        match self {
            Feed::Bytes { stream, parser } => match stream.next().await {
                Some(Ok(chunk)) => match parser.push(&chunk) {
                    Ok(events) => FeedItem::Events(events, Some(chunk)),
                    Err(error) => FeedItem::Error(bad_gateway(format!(
                        "the upstream stream is unusable: {error}"
                    ))),
                },
                Some(Err(error)) => FeedItem::Error(error),
                None => FeedItem::End(parser.finish().into_iter().collect()),
            },
            Feed::Canonical(stream) => match stream.next().await {
                Some(event) => match serde_json::to_string(&event) {
                    Ok(data) => FeedItem::Events(vec![SseEvent::data(data)], None),
                    Err(error) => FeedItem::Error(bad_gateway(format!(
                        "a mock event could not be serialised: {error}"
                    ))),
                },
                None => FeedItem::End(Vec::new()),
            },
        }
    }
}

/// [`Feed::next`] under the idle limit; `None` when the limit expired.
async fn next_within(feed: &mut Feed, idle: Option<Duration>) -> Option<FeedItem> {
    match idle {
        Some(limit) => tokio::time::timeout(limit, feed.next()).await.ok(),
        None => Some(feed.next().await),
    }
}

/// What is said about a stream the upstream closed before its terminal
/// event.
const TRUNCATED: &str = "the upstream closed the stream before the response was complete";

/// How a bootstrap ended.
pub(crate) enum Boot {
    /// The stream produced its first client-visible event: `pending` holds
    /// everything to send so far, and `ended` says whether the upstream has
    /// already closed the stream (a short answer that arrived in one piece).
    Committed { pending: Vec<SseEvent>, ended: bool },
    /// The stream failed before anything could be sent: a failed attempt.
    Failed(UpstreamError),
    /// The client went away.
    Cancelled,
}

/// Reads from `feed` until the transcoder has something for the client or
/// the stream turns out to be no good.
///
/// A failed attempt is: a transport error, an upstream error event (or a
/// stream the decoder cannot make sense of) before anything was sent, the
/// upstream going silent for `idle`, or the stream ending without a single
/// event or without completing.
pub(crate) async fn bootstrap(
    feed: &mut Feed,
    transcoder: &mut Transcoder,
    cancel: &CancellationToken,
    idle: Option<Duration>,
    capture: &mut Option<CaptureBuf>,
    target: Option<&Target>,
) -> Boot {
    let mut pending: Vec<SseEvent> = Vec::new();
    loop {
        let item = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Boot::Cancelled,
            item = next_within(feed, idle) => item,
        };
        match item {
            None => return Boot::Failed(idle_timeout(idle.unwrap_or_default())),
            Some(FeedItem::Error(error)) => return Boot::Failed(error),
            Some(FeedItem::Events(events, raw)) => {
                if let (Some(buffer), Some(raw)) = (capture.as_mut(), raw.as_ref()) {
                    buffer.push(raw);
                }
                for event in &events {
                    pending.extend(transcoder.push(event));
                }
                if let Some(error) = transcoder.error() {
                    return Boot::Failed(stream_failure(error, target));
                }
                if transcoder.saw_first_event() && !pending.is_empty() {
                    return Boot::Committed {
                        pending,
                        ended: false,
                    };
                }
            }
            Some(FeedItem::End(events)) => {
                for event in &events {
                    pending.extend(transcoder.push(event));
                }
                if let Some(error) = transcoder.error() {
                    return Boot::Failed(stream_failure(error, target));
                }
                if !transcoder.saw_first_event() {
                    return Boot::Failed(bad_gateway(
                        "the upstream closed the stream without sending a response",
                    ));
                }
                pending.extend(transcoder.finish());
                if let Some(error) = transcoder.error() {
                    return Boot::Failed(stream_failure(error, target));
                }
                if transcoder.truncated() {
                    return Boot::Failed(bad_gateway(TRUNCATED));
                }
                return Boot::Committed {
                    pending,
                    ended: true,
                };
            }
        }
    }
}

/// How a committed stream ended.
enum End {
    /// The upstream closed the stream (whether it was complete is the
    /// transcoder's verdict).
    Completed,
    /// The client stopped listening.
    ClientGone,
    /// The upstream connection failed or went silent.
    Failed(UpstreamError),
}

/// The task that carries a committed stream to the client and settles the
/// request when it ends. Dropping it (a panic, runtime shutdown) releases
/// the gauges and still publishes the request record.
pub(crate) struct Pump {
    pub inner: Arc<Inner>,
    pub recorder: Recorder,
    pub lease: Lease,
    /// Scope the stream's reasoning is remembered under.
    pub scope: String,
    pub feed: Feed,
    pub transcoder: Transcoder,
    /// Events produced during the bootstrap, sent first.
    pub pending: Vec<SseEvent>,
    /// The upstream already closed the stream during the bootstrap.
    pub ended: bool,
    pub tx: mpsc::Sender<SseEvent>,
    pub cancel: CancellationToken,
    pub idle: Option<Duration>,
    /// Time from sending the request to the first event, reported to the
    /// scheduler as the attempt's latency.
    pub latency_ms: u64,
    /// For removing the credential from messages; `None` for the mock.
    pub target: Option<Target>,
    pub upstream_capture: Option<CaptureBuf>,
    pub client_capture: Option<CaptureBuf>,
    pub stream_gauge: GaugeGuard,
    /// The client has been sent a complete response (its terminal event
    /// included). Set by the pump; start with `false`.
    pub delivered: bool,
}

impl Pump {
    /// Spawns the pump.
    pub(crate) fn spawn(mut self) {
        self.recorder.on_drop(DropOutcome::TASK_ENDED);
        tokio::spawn(async move {
            let end = self.forward().await;
            self.settle(end);
        });
    }

    /// Sends events to the client, in order, waiting when the channel is
    /// full. False when the client is gone. Notes when what has been sent
    /// amounts to a complete response.
    async fn send(&mut self, events: Vec<SseEvent>) -> bool {
        for event in events {
            if let Some(buffer) = self.client_capture.as_mut() {
                buffer.push(&event.to_bytes());
            }
            let delivered = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => false,
                sent = self.tx.send(event) => sent.is_ok(),
            };
            if !delivered {
                return false;
            }
        }
        if self.transcoder.is_terminal() && !self.transcoder.failed() {
            self.delivered = true;
        }
        true
    }

    async fn forward(&mut self) -> End {
        let first = std::mem::take(&mut self.pending);
        if !self.send(first).await {
            return End::ClientGone;
        }
        if self.ended {
            return End::Completed;
        }
        loop {
            let item = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return End::ClientGone,
                _ = self.tx.closed() => return End::ClientGone,
                item = next_within(&mut self.feed, self.idle) => item,
            };
            let failure = match item {
                Some(FeedItem::Events(events, raw)) => {
                    if let (Some(buffer), Some(raw)) = (self.upstream_capture.as_mut(), raw) {
                        buffer.push(&raw);
                    }
                    let mut out = Vec::new();
                    for event in &events {
                        out.extend(self.transcoder.push(event));
                    }
                    if !self.send(out).await {
                        return End::ClientGone;
                    }
                    if self.transcoder.is_closed() {
                        // The stream could not be decoded any further; the
                        // transcoder has told the client.
                        return End::Completed;
                    }
                    continue;
                }
                Some(FeedItem::End(events)) => {
                    let mut out = Vec::new();
                    for event in &events {
                        out.extend(self.transcoder.push(event));
                    }
                    out.extend(self.transcoder.finish());
                    return if self.send(out).await {
                        End::Completed
                    } else {
                        End::ClientGone
                    };
                }
                Some(FeedItem::Error(error)) => error,
                None => idle_timeout(self.idle.unwrap_or_default()),
            };
            // The connection broke or went silent. If the response was
            // already complete that changes nothing for the client.
            let complete = self.transcoder.is_terminal() && !self.transcoder.failed();
            let out = self.transcoder.fail(failure.to_api_error());
            if !self.send(out).await {
                return End::ClientGone;
            }
            return if complete {
                End::Completed
            } else {
                End::Failed(failure)
            };
        }
    }

    /// Reports the outcome to the scheduler, remembers reasoning, and
    /// publishes the request record.
    fn settle(self, end: End) {
        let Pump {
            inner,
            mut recorder,
            lease,
            scope,
            feed,
            transcoder,
            latency_ms,
            target,
            upstream_capture,
            client_capture,
            stream_gauge,
            tx,
            delivered,
            ..
        } = self;
        // Closes the upstream connection before anything else.
        drop(feed);
        // A client that leaves once it has the whole response (many stop
        // reading at the terminal event) was served, not lost.
        let end = match end {
            End::ClientGone if delivered => End::Completed,
            other => other,
        };
        let status = match end {
            End::ClientGone => {
                // The credential did its job; the client leaving says
                // nothing about it.
                inner.report(&lease, Outcome::Success { latency_ms });
                recorder.builder().set_error(RecordError::new(
                    "client_disconnect",
                    "the client closed the stream before it ended",
                ));
                crate::reply::CLIENT_CLOSED_REQUEST
            }
            End::Failed(error) => {
                inner.report(&lease, Outcome::Failure(&error));
                recorder.builder().set_error(
                    RecordError::new(
                        error_kind_name(error.to_api_error().kind),
                        &error.info.message,
                    )
                    .with_upstream_status(error.status),
                );
                200
            }
            End::Completed => {
                if let Some(error) = transcoder.error() {
                    let failure = stream_failure(error, target.as_ref());
                    inner.report(&lease, Outcome::Failure(&failure));
                    recorder.builder().set_error(
                        RecordError::new(error_kind_name(error.kind), &failure.info.message)
                            .with_upstream_status(failure.status),
                    );
                } else if transcoder.truncated() {
                    let failure = bad_gateway(TRUNCATED);
                    inner.report(&lease, Outcome::Failure(&failure));
                    recorder
                        .builder()
                        .set_error(RecordError::new("upstream", &failure.info.message));
                } else if transcoder.failed() {
                    // The upstream itself ended the response with an error
                    // finish reason: the credential worked, the generation
                    // did not.
                    inner.report(&lease, Outcome::Success { latency_ms });
                    recorder.builder().set_error(RecordError::new(
                        "upstream",
                        "the upstream ended the response with an error",
                    ));
                } else {
                    inner.report(&lease, Outcome::Success { latency_ms });
                    inner
                        .reasoning
                        .remember_accumulator(transcoder.accumulator(), &scope);
                }
                200
            }
        };
        recorder.set_usage(transcoder.usage());
        // The raw upstream events are kept for the operator only; even so
        // they are stored without the credential an error may have quoted.
        let upstream_text = upstream_capture
            .and_then(CaptureBuf::into_text)
            .map(|text| match &target {
                Some(target) => target.redact(&text),
                None => text,
            });
        recorder.capture_upstream_response_text(upstream_text);
        recorder.capture_client_response_text(client_capture.and_then(CaptureBuf::into_text));
        drop(stream_gauge);
        recorder.finish(status);
        // Closed last: when the server sees the stream end, the request has
        // been settled and its record published.
        drop(tx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::codec::ClientCtx;
    use switchyard_core::ir::FinishReason;
    use switchyard_core::{ApiError, FailureClass, Protocol};

    fn chat_transcoder() -> Transcoder {
        let codec = switchyard_codecs::codec(Protocol::OpenaiChat);
        Transcoder::translate(
            Box::new(CanonicalDecoder),
            codec.stream_encoder(&ClientCtx::new("m")),
        )
    }

    fn canonical(events: Vec<StreamEvent>) -> Feed {
        Feed::Canonical(futures::stream::iter(events).boxed())
    }

    fn start() -> StreamEvent {
        StreamEvent::Start {
            id: "r1".into(),
            model: "m".into(),
            created: 1,
        }
    }

    fn text(index: u32, text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::BlockStart {
                index,
                block: switchyard_core::BlockStart::Text,
            },
            StreamEvent::TextDelta {
                index,
                text: text.into(),
            },
            StreamEvent::BlockStop { index },
        ]
    }

    fn finish() -> StreamEvent {
        StreamEvent::Finish {
            reason: FinishReason::Stop,
            stop_sequence: None,
        }
    }

    async fn boot(feed: &mut Feed, transcoder: &mut Transcoder) -> Boot {
        bootstrap(
            feed,
            transcoder,
            &CancellationToken::new(),
            Some(Duration::from_secs(5)),
            &mut None,
            None,
        )
        .await
    }

    #[test]
    fn the_canonical_decoder_round_trips_events() {
        let mut decoder = CanonicalDecoder;
        let event = start();
        let wire = SseEvent::data(serde_json::to_string(&event).unwrap());
        assert_eq!(decoder.decode(&wire).unwrap(), vec![event]);
        assert!(decoder.decode(&SseEvent::data("nonsense")).is_err());
        assert!(decoder.finish().is_empty());
    }

    #[tokio::test]
    async fn a_stream_commits_at_its_first_client_visible_event() {
        let mut events = vec![start()];
        events.extend(text(0, "hello"));
        events.push(finish());
        let mut feed = canonical(events);
        let mut transcoder = chat_transcoder();
        match boot(&mut feed, &mut transcoder).await {
            Boot::Committed { pending, ended } => {
                assert!(!pending.is_empty());
                assert!(!ended, "the rest of the stream is still to come");
            }
            _ => panic!("expected the stream to commit"),
        }
        assert!(transcoder.saw_first_event());
    }

    #[tokio::test]
    async fn an_error_event_before_content_is_a_failed_attempt() {
        // Both events arrive in one read, as they do when an upstream
        // answers 200 and fails at once: the error is seen before anything
        // is committed.
        let wire: String = [
            start(),
            StreamEvent::Error(ApiError::unavailable("overloaded").with_status(529)),
        ]
        .iter()
        .map(|event| format!("data: {}\n\n", serde_json::to_string(event).unwrap()))
        .collect();
        let mut feed = Feed::bytes(futures::stream::iter(vec![Ok(Bytes::from(wire))]).boxed());
        let mut transcoder = chat_transcoder();
        match boot(&mut feed, &mut transcoder).await {
            Boot::Failed(error) => {
                assert_eq!(error.class, FailureClass::Server);
                assert_eq!(error.status, 529);
                assert_eq!(error.info.message, "overloaded");
            }
            _ => panic!("expected a failed attempt"),
        }
    }

    #[tokio::test]
    async fn an_empty_stream_is_a_failed_attempt() {
        let mut feed = canonical(Vec::new());
        let mut transcoder = chat_transcoder();
        match boot(&mut feed, &mut transcoder).await {
            Boot::Failed(error) => {
                assert_eq!(error.class, FailureClass::Server);
                assert_eq!(error.status, 502);
            }
            _ => panic!("expected a failed attempt"),
        }
    }

    #[tokio::test]
    async fn a_transport_error_is_a_failed_attempt() {
        let stream = futures::stream::iter(vec![Err(UpstreamError::transport(
            "read: connection reset",
        ))])
        .boxed();
        let mut feed = Feed::bytes(stream);
        let mut transcoder = chat_transcoder();
        match boot(&mut feed, &mut transcoder).await {
            Boot::Failed(error) => assert_eq!(error.class, FailureClass::Transport),
            _ => panic!("expected a failed attempt"),
        }
    }

    #[tokio::test]
    async fn silence_is_a_timeout() {
        let mut feed = Feed::bytes(futures::stream::pending().boxed());
        let mut transcoder = chat_transcoder();
        let outcome = bootstrap(
            &mut feed,
            &mut transcoder,
            &CancellationToken::new(),
            Some(Duration::from_millis(30)),
            &mut None,
            None,
        )
        .await;
        match outcome {
            Boot::Failed(error) => {
                assert_eq!(error.class, FailureClass::Transport);
                assert_eq!(error.status, 408);
                assert_eq!(error.to_api_error().status, 504);
            }
            _ => panic!("expected a timeout"),
        }
    }

    #[tokio::test]
    async fn cancellation_ends_the_bootstrap() {
        let mut feed = Feed::bytes(futures::stream::pending().boxed());
        let mut transcoder = chat_transcoder();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = bootstrap(&mut feed, &mut transcoder, &cancel, None, &mut None, None).await;
        assert!(matches!(outcome, Boot::Cancelled));
    }

    #[tokio::test]
    async fn a_panicking_pump_releases_its_gauges_and_still_publishes_the_record() {
        use crate::{Gateway, GatewayOptions};
        use switchyard_scheduler::PickRequest;
        use switchyard_telemetry::RequestStart;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switchyard.toml");
        std::fs::write(
            &path,
            "[[providers]]
name = \"demo\"
kind = \"mock\"
",
        )
        .unwrap();
        let gateway = Gateway::start(GatewayOptions::new(&path).watch(false))
            .await
            .unwrap();
        let inner = Arc::clone(gateway.inner());
        let resolved = inner.scheduler.resolve("mock-echo").unwrap();
        let lease = inner
            .scheduler
            .pick(&PickRequest {
                resolved: &resolved,
                tried: &[],
                session: None,
                client_protocol: Protocol::OpenaiChat,
                now: inner.scheduler.now(),
            })
            .unwrap();
        let start = RequestStart::new(
            Protocol::OpenaiChat,
            "POST /v1/chat/completions",
            "mock-echo",
            switchyard_core::util::now_unix_ms(),
        );
        let recorder = Recorder::begin(&inner.telemetry, inner.store.current(), start);
        let id = recorder.id().to_string();
        let (tx, mut rx) = mpsc::channel(4);
        // A source that blows up as soon as the pump reads from it.
        let exploding = futures::stream::poll_fn(|_| -> std::task::Poll<Option<StreamEvent>> {
            panic!("the feed exploded")
        });
        Pump {
            inner: Arc::clone(&inner),
            recorder,
            lease,
            scope: "test".to_string(),
            feed: Feed::Canonical(exploding.boxed()),
            transcoder: chat_transcoder(),
            pending: vec![SseEvent::data("first")],
            ended: false,
            tx,
            cancel: CancellationToken::new(),
            idle: None,
            latency_ms: 1,
            target: None,
            upstream_capture: None,
            client_capture: None,
            stream_gauge: inner.telemetry.track_stream(),
            delivered: false,
        }
        .spawn();

        // The client got what was ready, then the stream just ends.
        assert_eq!(
            rx.recv().await.map(|event| event.data),
            Some("first".to_string())
        );
        assert_eq!(rx.recv().await, None);
        let gauges = inner.telemetry.gauges();
        for _ in 0..200 {
            if gauges.in_flight() == 0 && gauges.active_streams() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!((gauges.in_flight(), gauges.active_streams()), (0, 0));
        let record = inner
            .telemetry
            .usage()
            .get(&id)
            .expect("the record was published");
        assert_eq!(record.status, 500);
        assert!(!record.ok);
        assert_eq!(
            record.error.as_ref().map(|e| e.kind.as_str()),
            Some("internal")
        );
        gateway.shutdown().await;
    }

    #[test]
    fn the_usage_only_chunk_is_recognised_and_nothing_else() {
        let usage_only = [
            r#"{"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
            r#"{"choices" : [ ] , "usage": {"prompt_tokens": 1}}"#,
            "{\"usage\":{\"total_tokens\":3},\n \"choices\":\n []}",
        ];
        for data in usage_only {
            assert!(is_usage_only_chunk(&SseEvent::data(data)), "{data}");
        }
        let kept = [
            // An ordinary chunk of a stream with usage switched on.
            r#"{"choices":[{"index":0,"delta":{"content":"hi"}}],"usage":null}"#,
            // Usage on the last content chunk, as some servers send it.
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"total_tokens":3}}"#,
            // Azure's opening chunk: no choices, but no usage either.
            r#"{"choices":[],"prompt_filter_results":[{"prompt_index":0}]}"#,
            r#"{"choices":[],"usage":null}"#,
            // The words inside content prove nothing.
            r#"{"choices":[{"index":0,"delta":{"content":"\"choices\": [] and \"usage\": {}"}}]}"#,
            r#"{"error":{"message":"boom"}}"#,
            "[DONE]",
            "",
        ];
        for data in kept {
            assert!(!is_usage_only_chunk(&SseEvent::data(data)), "{data}");
        }
    }

    #[test]
    fn only_an_explicit_true_asks_for_the_usage_chunk() {
        use serde_json::json;
        assert!(asked_for_usage_chunk(
            &json!({"stream_options": {"include_usage": true}})
        ));
        for body in [
            json!({}),
            json!({"stream_options": null}),
            json!({"stream_options": {}}),
            json!({"stream_options": {"include_usage": false}}),
            json!({"stream_options": {"include_usage": "true"}}),
        ] {
            assert!(!asked_for_usage_chunk(&body), "{body}");
        }
    }

    #[test]
    fn idle_limit_zero_disables() {
        assert_eq!(idle_limit(0), None);
        assert_eq!(idle_limit(300), Some(Duration::from_secs(300)));
    }
}
