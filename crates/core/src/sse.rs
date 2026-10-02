//! Server-Sent Events: an incremental parser for upstream streams and a
//! serializer for client streams.

use bytes::{BufMut, Bytes, BytesMut};

/// One SSE event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, if any.
    pub event: Option<String>,
    /// The `data:` field. Multiple `data:` lines are joined with `\n`.
    pub data: String,
}

impl SseEvent {
    /// An event with only a `data:` field.
    pub fn data(data: impl Into<String>) -> Self {
        SseEvent {
            event: None,
            data: data.into(),
        }
    }

    /// An event with both `event:` and `data:` fields.
    pub fn named(event: impl Into<String>, data: impl Into<String>) -> Self {
        SseEvent {
            event: Some(event.into()),
            data: data.into(),
        }
    }

    /// An event whose data is `value` serialised as compact JSON.
    pub fn json(event: Option<&str>, value: &serde_json::Value) -> Self {
        SseEvent {
            event: event.map(str::to_string),
            data: value.to_string(),
        }
    }

    /// True for OpenAI's `data: [DONE]` terminator.
    pub fn is_done_marker(&self) -> bool {
        self.data.trim() == "[DONE]"
    }

    /// Serialises the event in wire form, terminated by a blank line.
    pub fn to_bytes(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(self.data.len() + 32);
        self.write_to(&mut out);
        out.freeze()
    }

    /// Appends the wire form of the event to `out`.
    pub fn write_to(&self, out: &mut BytesMut) {
        if let Some(event) = &self.event {
            out.put_slice(b"event: ");
            out.put_slice(event.as_bytes());
            out.put_u8(b'\n');
        }
        // A data payload containing newlines must be split across data lines.
        for line in self.data.split('\n') {
            out.put_slice(b"data: ");
            out.put_slice(line.as_bytes());
            out.put_u8(b'\n');
        }
        out.put_u8(b'\n');
    }
}

/// An SSE comment line (`: text`), used as a keep-alive that every compliant
/// client ignores.
pub fn comment(text: &str) -> Bytes {
    let mut out = BytesMut::with_capacity(text.len() + 4);
    out.put_slice(b": ");
    out.put_slice(text.as_bytes());
    out.put_slice(b"\n\n");
    out.freeze()
}

/// Default cap on a single buffered event (64 MiB). Image generation streams
/// carry whole base64 images in one event, so the limit is generous.
pub const DEFAULT_MAX_EVENT_BYTES: usize = 64 * 1024 * 1024;

/// Raised when an upstream sends an event larger than the parser's limit.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("SSE event exceeds the {limit} byte limit")]
pub struct EventTooLarge {
    pub limit: usize,
}

/// Incremental SSE parser. Feed it arbitrary byte chunks; it yields complete
/// events as their terminating blank line arrives.
///
/// Follows the WHATWG event-stream rules for the fields this gateway cares
/// about: lines end with `\n`, `\r\n` or `\r`; a leading UTF-8 BOM is ignored;
/// lines starting with `:` are comments; `id:` and `retry:` fields are
/// ignored; one optional space after the colon is stripped; an event with no
/// `data:` lines is dropped unless it carries an `event:` name.
#[derive(Debug)]
pub struct SseParser {
    buf: Vec<u8>,
    /// Scan position within `buf`: everything before it is known to contain no
    /// line terminator.
    scanned: usize,
    event: Option<String>,
    data: String,
    has_data: bool,
    max_event_bytes: usize,
    bom_checked: bool,
    /// The previous chunk ended with `\r`; a `\n` starting the next chunk
    /// belongs to the same line terminator.
    pending_cr: bool,
}

impl Default for SseParser {
    fn default() -> Self {
        SseParser::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        SseParser::with_limit(DEFAULT_MAX_EVENT_BYTES)
    }

    pub fn with_limit(max_event_bytes: usize) -> Self {
        SseParser {
            buf: Vec::new(),
            scanned: 0,
            event: None,
            data: String::new(),
            has_data: false,
            max_event_bytes,
            bom_checked: false,
            pending_cr: false,
        }
    }

    /// Feeds a chunk of bytes and returns the events completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, EventTooLarge> {
        let mut chunk = chunk;
        if self.pending_cr {
            self.pending_cr = false;
            if let Some(rest) = chunk.strip_prefix(b"\n") {
                chunk = rest;
            }
        }
        self.buf.extend_from_slice(chunk);

        if !self.bom_checked && self.buf.len() >= 3 {
            self.bom_checked = true;
            if self.buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
                self.buf.drain(..3);
            }
        }

        let mut events = Vec::new();
        let mut line_start = 0usize;
        let mut i = self.scanned;
        while i < self.buf.len() {
            let b = self.buf[i];
            if b == b'\n' || b == b'\r' {
                let line_end = i;
                let mut next = i + 1;
                if b == b'\r' {
                    if next < self.buf.len() {
                        if self.buf[next] == b'\n' {
                            next += 1;
                        }
                    } else {
                        self.pending_cr = true;
                    }
                }
                let line = std::str::from_utf8(&self.buf[line_start..line_end])
                    .map(str::to_owned)
                    .unwrap_or_else(|_| {
                        String::from_utf8_lossy(&self.buf[line_start..line_end]).into_owned()
                    });
                if let Some(ev) = self.process_line(&line) {
                    events.push(ev);
                }
                line_start = next;
                i = next;
            } else {
                i += 1;
            }
        }
        self.buf.drain(..line_start);
        self.scanned = self.buf.len();

        if self.buf.len() + self.data.len() > self.max_event_bytes {
            return Err(EventTooLarge {
                limit: self.max_event_bytes,
            });
        }
        Ok(events)
    }

    /// Signals end of input. Returns a final event if the stream ended without
    /// the terminating blank line (some upstreams omit it on the last event).
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            self.scanned = 0;
            if let Some(ev) = self.process_line(&line) {
                return Some(ev);
            }
        }
        self.dispatch()
    }

    fn process_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => self.event = Some(value.to_string()),
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        let has_data = std::mem::take(&mut self.has_data);
        let data = std::mem::take(&mut self.data);
        if !has_data && event.is_none() {
            return None;
        }
        Some(SseEvent { event, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut p = SseParser::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(p.push(c).unwrap());
        }
        out.extend(p.finish());
        out
    }

    #[test]
    fn basic_events() {
        let evs = parse_all(&[b"event: message_start\ndata: {\"a\":1}\n\ndata: [DONE]\n\n"]);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0], SseEvent::named("message_start", "{\"a\":1}"));
        assert!(evs[1].is_done_marker());
    }

    #[test]
    fn split_across_chunks_at_every_byte() {
        let wire = b"event: a\r\ndata: one\r\ndata: two\r\n\r\n: comment\r\ndata:three\r\n\r\n";
        let mut expected = None;
        for cut in 0..wire.len() {
            let evs = parse_all(&[&wire[..cut], &wire[cut..]]);
            assert_eq!(evs.len(), 2, "cut at {cut}");
            assert_eq!(evs[0], SseEvent::named("a", "one\ntwo"));
            assert_eq!(evs[1], SseEvent::data("three"));
            if let Some(prev) = &expected {
                assert_eq!(prev, &evs);
            }
            expected = Some(evs);
        }
    }

    #[test]
    fn bare_cr_line_endings() {
        let evs = parse_all(&[b"data: x\r\rdata: y\r\r"]);
        assert_eq!(evs, vec![SseEvent::data("x"), SseEvent::data("y")]);
    }

    #[test]
    fn missing_final_blank_line() {
        let evs = parse_all(&[b"data: {\"x\":1}\n\ndata: tail"]);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[1].data, "tail");
    }

    #[test]
    fn bom_comments_and_unknown_fields_are_ignored() {
        let evs = parse_all(&[b"\xEF\xBB\xBF: hi\nid: 7\nretry: 100\ndata: ok\n\n"]);
        assert_eq!(evs, vec![SseEvent::data("ok")]);
    }

    #[test]
    fn empty_data_field_is_an_event() {
        let evs = parse_all(&[b"data:\n\n"]);
        assert_eq!(evs, vec![SseEvent::data("")]);
    }

    #[test]
    fn size_limit() {
        let mut p = SseParser::with_limit(16);
        assert!(p.push(b"data: 0123456789abcdefghij").is_err());
    }

    #[test]
    fn serialisation_round_trip() {
        let ev = SseEvent::named("delta", "line1\nline2");
        let wire = ev.to_bytes();
        assert_eq!(&wire[..], b"event: delta\ndata: line1\ndata: line2\n\n");
        assert_eq!(parse_all(&[&wire]), vec![ev]);
        assert_eq!(&comment("keep-alive")[..], b": keep-alive\n\n");
    }
}
