//! The event bus: a `tokio::sync::broadcast` channel carrying everything the
//! live dashboard shows. Publishing never blocks and never fails; a
//! subscriber that falls behind loses the oldest events (it sees
//! `RecvError::Lagged`) while the gateway carries on.

use crate::logs::LogLine;
use crate::record::{RequestRecord, RequestStart};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Default number of events buffered per subscriber before it starts
/// losing the oldest ones.
pub const DEFAULT_BUS_CAPACITY: usize = 1024;

/// Topics an [`Event`] can have, in the order they are documented for the
/// admin WebSocket.
pub const TOPICS: [&str; 5] = [
    "request.started",
    "request.finished",
    "log",
    "credential",
    "config.reloaded",
];

/// Something that happened in the gateway.
#[derive(Clone, Debug)]
pub enum Event {
    RequestStarted(RequestStart),
    RequestFinished(Arc<RequestRecord>),
    Log(Arc<LogLine>),
    /// A credential changed state (cooldown started or ended, enabled,
    /// disabled). The payload is produced by the scheduler's owner and
    /// forwarded as is.
    Credential(Value),
    ConfigReloaded {
        /// Unix milliseconds.
        at: i64,
        ok: bool,
        message: String,
    },
}

impl Event {
    /// Topic name, also the `type` of the frame sent to dashboard clients.
    pub const fn topic(&self) -> &'static str {
        match self {
            Event::RequestStarted(_) => "request.started",
            Event::RequestFinished(_) => "request.finished",
            Event::Log(_) => "log",
            Event::Credential(_) => "credential",
            Event::ConfigReloaded { .. } => "config.reloaded",
        }
    }

    /// The payload alone.
    pub fn data(&self) -> Value {
        match self {
            Event::RequestStarted(start) => serde_json::to_value(start).unwrap_or(Value::Null),
            Event::RequestFinished(record) => {
                serde_json::to_value(record.as_ref()).unwrap_or(Value::Null)
            }
            Event::Log(line) => serde_json::to_value(line.as_ref()).unwrap_or(Value::Null),
            Event::Credential(value) => value.clone(),
            Event::ConfigReloaded { at, ok, message } => {
                json!({ "at": at, "ok": ok, "message": message })
            }
        }
    }

    /// The JSON frame pushed over the admin WebSocket:
    /// `{"type": <topic>, "data": <payload>}`.
    pub fn to_frame(&self) -> Value {
        json!({ "type": self.topic(), "data": self.data() })
    }
}

/// Fan-out of [`Event`]s to any number of subscribers. Cloning is cheap and
/// every clone publishes to the same subscribers.
#[derive(Clone, Debug)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl Default for EventBus {
    fn default() -> Self {
        EventBus::new(DEFAULT_BUS_CAPACITY)
    }
}

impl EventBus {
    /// A bus whose subscribers each buffer up to `capacity` events
    /// (at least one).
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        EventBus { tx }
    }

    /// Publishes an event and returns how many subscribers it was queued
    /// for. Never blocks; with no subscribers the event is simply dropped.
    pub fn publish(&self, event: Event) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// A new subscriber. It receives events published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::RecordBuilder;
    use pretty_assertions::assert_eq;
    use switchyard_core::protocol::Protocol;
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};

    fn start(id: &str) -> RequestStart {
        RequestStart::new(
            Protocol::OpenaiChat,
            "POST /v1/chat/completions",
            "gpt-5",
            1_000,
        )
        .with_id(id)
    }

    fn reloaded(at: i64) -> Event {
        Event::ConfigReloaded {
            at,
            ok: true,
            message: "applied".to_string(),
        }
    }

    #[test]
    fn publish_without_subscribers_is_fine() {
        let bus = EventBus::default();
        assert_eq!(bus.subscriber_count(), 0);
        assert_eq!(bus.publish(reloaded(1)), 0);
    }

    #[test]
    fn topics_and_frames() {
        let record = Arc::new(RecordBuilder::new(start("r1")).finish(200, 1_500));
        let line = Arc::new(LogLine::new(7, "info", "switchyard::test", "hello"));
        let events = [
            Event::RequestStarted(start("r1")),
            Event::RequestFinished(record),
            Event::Log(line),
            Event::Credential(json!({"id": "c1", "state": "cooldown"})),
            reloaded(9),
        ];
        let topics: Vec<&str> = events.iter().map(Event::topic).collect();
        assert_eq!(topics, TOPICS);

        let frame = events[0].to_frame();
        assert_eq!(frame["type"], "request.started");
        assert_eq!(frame["data"]["id"], "r1");
        assert_eq!(frame["data"]["client_protocol"], "openai-chat");
        assert_eq!(frame["data"]["transport"], "http");

        let frame = events[1].to_frame();
        assert_eq!(frame["type"], "request.finished");
        assert_eq!(frame["data"]["status"], 200);
        assert_eq!(frame["data"]["duration_ms"], 500);

        let frame = events[2].to_frame();
        assert_eq!(
            frame,
            json!({"type": "log", "data": {
                "seq": 0, "at": 7, "level": "info", "target": "switchyard::test",
                "message": "hello", "fields": {}
            }})
        );

        assert_eq!(
            events[3].to_frame(),
            json!({"type": "credential", "data": {"id": "c1", "state": "cooldown"}})
        );
        assert_eq!(
            events[4].to_frame(),
            json!({"type": "config.reloaded", "data": {"at": 9, "ok": true, "message": "applied"}})
        );
    }

    #[tokio::test]
    async fn fan_out_reaches_every_subscriber() {
        let bus = EventBus::default();
        let mut a = bus.subscribe();
        let mut b = bus.clone().subscribe();
        assert_eq!(bus.subscriber_count(), 2);
        assert_eq!(bus.publish(Event::RequestStarted(start("r1"))), 2);
        assert_eq!(bus.publish(reloaded(2)), 2);
        for rx in [&mut a, &mut b] {
            assert_eq!(rx.recv().await.unwrap().topic(), "request.started");
            assert_eq!(rx.recv().await.unwrap().topic(), "config.reloaded");
            assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        }
    }

    #[tokio::test]
    async fn subscriber_only_sees_events_after_subscribing() {
        let bus = EventBus::default();
        bus.publish(reloaded(1));
        let mut rx = bus.subscribe();
        bus.publish(reloaded(2));
        match rx.recv().await.unwrap() {
            Event::ConfigReloaded { at, .. } => assert_eq!(at, 2),
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[tokio::test]
    async fn lagging_subscriber_loses_events_but_never_blocks_the_publisher() {
        let bus = EventBus::new(4);
        let mut slow = bus.subscribe();
        // Nobody is reading; every publish must still return immediately.
        for at in 0..100 {
            assert_eq!(bus.publish(reloaded(at)), 1);
        }
        match slow.recv().await {
            Err(RecvError::Lagged(missed)) => assert_eq!(missed, 96),
            other => panic!("expected a lag notice, got {other:?}"),
        }
        // After the notice the subscriber continues with the newest events.
        let mut seen = Vec::new();
        while let Ok(Event::ConfigReloaded { at, .. }) = slow.try_recv() {
            seen.push(at);
        }
        assert_eq!(seen, vec![96, 97, 98, 99]);
    }

    #[tokio::test]
    async fn dropped_subscriber_is_forgotten() {
        let bus = EventBus::default();
        let rx = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);
        drop(rx);
        assert_eq!(bus.subscriber_count(), 0);
        assert_eq!(bus.publish(reloaded(1)), 0);
    }
}
