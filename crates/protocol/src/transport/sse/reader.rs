//! Background reader task for the SSE transport.
//!
//! Owns the spawned task that parses SSE events off the wire and forwards
//! `message` payloads onto the mpsc channel feeding `SseTransport::request`.
//! Pure mechanics — no public API surface here; everything is `pub(super)`.

use std::fmt::Display;
use std::mem;
use std::sync::{Arc, Mutex};

use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::TransportError;

/// Maximum decoded `data:` bytes in one SSE event.
///
/// This is enforced while chunks are consumed, before a complete event is
/// materialized as a `String`.
pub(super) const MAX_SSE_EVENT_DATA_BYTES: usize = 16 * 1024 * 1024;

/// Aggregate bytes allowed in the inbound channel and id-mismatch buffer.
///
/// Permits travel with frames, so moving a frame from the channel to
/// `SseTransport::pending` does not release its charge.
pub(super) const INBOUND_BYTE_BUDGET: usize = 32 * 1024 * 1024;

const MAX_SSE_LINE_BYTES: usize = MAX_SSE_EVENT_DATA_BYTES + 16;
const MAX_EVENT_TYPE_BYTES: usize = 1024;
const UTF8_BOM: &[u8; 3] = b"\xef\xbb\xbf";

/// Secret-safe terminal failures emitted by the bounded reader.
///
/// Variants carry no server-controlled strings, URLs, or response bodies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReaderTerminalError {
    StreamRead,
    EventDataLimit,
    LineLimit,
    EventTypeLimit,
    EventDataUtf8,
    EventTypeUtf8,
    UnexpectedEof,
    AggregateBudget,
    AccountingOverflow,
}

impl std::fmt::Display for ReaderTerminalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StreamRead => formatter.write_str("sse stream read/parse failure"),
            Self::EventDataLimit => write!(
                formatter,
                "sse event data exceeds {MAX_SSE_EVENT_DATA_BYTES}-byte limit"
            ),
            Self::LineLimit => {
                write!(
                    formatter,
                    "sse line exceeds {MAX_SSE_LINE_BYTES}-byte limit"
                )
            }
            Self::EventTypeLimit => write!(
                formatter,
                "sse event type exceeds {MAX_EVENT_TYPE_BYTES}-byte limit"
            ),
            Self::EventDataUtf8 => formatter.write_str("sse event data is not valid UTF-8"),
            Self::EventTypeUtf8 => formatter.write_str("sse event type is not valid UTF-8"),
            Self::UnexpectedEof => formatter.write_str("sse peer stream closed unexpectedly"),
            Self::AggregateBudget => write!(
                formatter,
                "sse inbound frames exceed {INBOUND_BYTE_BUDGET}-byte aggregate budget"
            ),
            Self::AccountingOverflow => {
                formatter.write_str("sse inbound frame accounting overflow")
            }
        }
    }
}

impl ReaderTerminalError {
    pub(super) fn into_transport_error(self) -> TransportError {
        TransportError::Other(self.to_string())
    }
}

#[derive(Default)]
pub(super) struct TerminalErrorLatch(Mutex<Option<ReaderTerminalError>>);

impl TerminalErrorLatch {
    pub(super) fn set(&self, error: ReaderTerminalError) {
        match self.0.lock() {
            Ok(mut terminal) => *terminal = Some(error),
            Err(poisoned) => *poisoned.into_inner() = Some(error),
        }
    }

    pub(super) fn get(&self) -> Option<ReaderTerminalError> {
        match self.0.lock() {
            Ok(terminal) => *terminal,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

#[derive(Debug)]
pub(super) struct BoundedEvent {
    pub(super) event: String,
    pub(super) data: String,
}

/// A frame charged against the shared inbound-byte budget.
pub(super) struct InboundFrame {
    data: String,
    _permit: OwnedSemaphorePermit,
}

impl InboundFrame {
    fn new(data: String, permit: OwnedSemaphorePermit) -> Self {
        Self {
            data,
            _permit: permit,
        }
    }

    pub(super) fn as_str(&self) -> &str {
        &self.data
    }

    pub(super) fn into_string(self) -> String {
        self.data
    }
}

fn inbound_frame_charge(data: &String) -> Result<u32, ReaderTerminalError> {
    let retained_bytes = data
        .capacity()
        .saturating_add(std::mem::size_of::<InboundFrame>())
        .max(1);
    u32::try_from(retained_bytes).map_err(|_| ReaderTerminalError::AccountingOverflow)
}

/// Incremental SSE parser with bounded line and event-data accumulation.
///
/// `eventsource-stream` builds unbounded `String`s internally, so checking its
/// returned event would be too late. This parser applies the limit as each
/// network chunk is consumed.
pub(super) struct BoundedSseParser<S, B> {
    stream: S,
    chunk: Option<B>,
    chunk_offset: usize,
    line: Vec<u8>,
    event_type: String,
    data: Vec<u8>,
    has_data: bool,
    bom_prefix: Vec<u8>,
    bom_resolved: bool,
    skip_lf_after_cr: bool,
    eof: bool,
}

impl<S, B> BoundedSseParser<S, B> {
    pub(super) fn new(stream: S) -> Self {
        Self {
            stream,
            chunk: None,
            chunk_offset: 0,
            line: Vec::new(),
            event_type: String::new(),
            data: Vec::new(),
            has_data: false,
            bom_prefix: Vec::with_capacity(UTF8_BOM.len()),
            bom_resolved: false,
            skip_lf_after_cr: false,
            eof: false,
        }
    }

    fn reset_event(&mut self) {
        self.event_type.clear();
        self.data.clear();
        self.has_data = false;
    }

    fn process_line(&mut self, line: &[u8]) -> Result<Option<BoundedEvent>, ReaderTerminalError> {
        if line.is_empty() {
            if !self.has_data {
                self.reset_event();
                return Ok(None);
            }
            let data = String::from_utf8(mem::take(&mut self.data))
                .map_err(|_| ReaderTerminalError::EventDataUtf8)?;
            let event = if self.event_type.is_empty() {
                "message".to_owned()
            } else {
                mem::take(&mut self.event_type)
            };
            self.has_data = false;
            return Ok(Some(BoundedEvent { event, data }));
        }
        if line[0] == b':' {
            return Ok(None);
        }

        let (field, mut value) = match line.iter().position(|byte| *byte == b':') {
            Some(index) => (&line[..index], &line[index + 1..]),
            None => (line, &[][..]),
        };
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }

        match field {
            b"event" => {
                if value.len() > MAX_EVENT_TYPE_BYTES {
                    return Err(ReaderTerminalError::EventTypeLimit);
                }
                self.event_type = std::str::from_utf8(value)
                    .map_err(|_| ReaderTerminalError::EventTypeUtf8)?
                    .to_owned();
            }
            b"data" => {
                let separator = usize::from(self.has_data);
                if self
                    .data
                    .len()
                    .saturating_add(separator)
                    .saturating_add(value.len())
                    > MAX_SSE_EVENT_DATA_BYTES
                {
                    return Err(ReaderTerminalError::EventDataLimit);
                }
                if separator != 0 {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(value);
                self.has_data = true;
            }
            _ => {}
        }
        Ok(None)
    }

    fn process_wire_byte(&mut self, byte: u8) -> Result<Option<BoundedEvent>, ReaderTerminalError> {
        if self.skip_lf_after_cr {
            self.skip_lf_after_cr = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }

        if matches!(byte, b'\r' | b'\n') {
            self.skip_lf_after_cr = byte == b'\r';
            let line = mem::take(&mut self.line);
            return self.process_line(&line);
        }

        if self.line.len() >= MAX_SSE_LINE_BYTES {
            return Err(ReaderTerminalError::LineLimit);
        }
        self.line.push(byte);
        Ok(None)
    }

    fn replay_bom_prefix(&mut self) -> Result<Option<BoundedEvent>, ReaderTerminalError> {
        let prefix = mem::take(&mut self.bom_prefix);
        for byte in prefix {
            if let Some(event) = self.process_wire_byte(byte)? {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }
}

impl<S, B, E> BoundedSseParser<S, B>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: Display,
{
    pub(super) async fn next_event(&mut self) -> Result<Option<BoundedEvent>, ReaderTerminalError> {
        loop {
            if let Some(chunk) = self.chunk.as_ref() {
                let bytes = chunk.as_ref();
                let chunk_len = bytes.len();
                if self.chunk_offset == chunk_len {
                    self.chunk = None;
                    self.chunk_offset = 0;
                    continue;
                }

                if !self.bom_resolved {
                    let byte = bytes[self.chunk_offset];
                    self.chunk_offset += 1;
                    self.bom_prefix.push(byte);
                    let prefix_index = self.bom_prefix.len() - 1;
                    if byte != UTF8_BOM[prefix_index] {
                        self.bom_resolved = true;
                        if let Some(event) = self.replay_bom_prefix()? {
                            return Ok(Some(event));
                        }
                    } else if self.bom_prefix.len() == UTF8_BOM.len() {
                        // EventSource ignores exactly one UTF-8 BOM at stream
                        // start, even when its bytes cross network chunks.
                        self.bom_prefix.clear();
                        self.bom_resolved = true;
                    }
                    if self.chunk_offset == chunk_len {
                        self.chunk = None;
                        self.chunk_offset = 0;
                    }
                    continue;
                }

                let remaining = &bytes[self.chunk_offset..];
                if self.skip_lf_after_cr && remaining.first() == Some(&b'\n') {
                    self.skip_lf_after_cr = false;
                    self.chunk_offset += 1;
                    if self.chunk_offset == chunk_len {
                        self.chunk = None;
                        self.chunk_offset = 0;
                    }
                    continue;
                }
                self.skip_lf_after_cr = false;

                let delimiter = remaining
                    .iter()
                    .position(|byte| matches!(*byte, b'\r' | b'\n'));
                let segment_len = delimiter.unwrap_or(remaining.len());
                if self.line.len().saturating_add(segment_len) > MAX_SSE_LINE_BYTES {
                    return Err(ReaderTerminalError::LineLimit);
                }
                self.line.extend_from_slice(&remaining[..segment_len]);
                self.chunk_offset += segment_len;
                if delimiter.is_some() {
                    let delimiter_byte = bytes[self.chunk_offset];
                    self.chunk_offset += 1;
                    let line = mem::take(&mut self.line);
                    self.skip_lf_after_cr = delimiter_byte == b'\r';
                    if self.chunk_offset == chunk_len {
                        self.chunk = None;
                        self.chunk_offset = 0;
                    }
                    if let Some(event) = self.process_line(&line)? {
                        return Ok(Some(event));
                    }
                } else {
                    self.chunk = None;
                    self.chunk_offset = 0;
                }
                continue;
            }

            if self.eof {
                if !self.bom_resolved {
                    self.bom_resolved = true;
                    if let Some(event) = self.replay_bom_prefix()? {
                        return Ok(Some(event));
                    }
                }
                if !self.line.is_empty() {
                    let line = mem::take(&mut self.line);
                    if let Some(event) = self.process_line(&line)? {
                        return Ok(Some(event));
                    }
                }
                // Dispatch a final complete event at EOF, per SSE parsing
                // semantics, without requiring a trailing blank line.
                return self.process_line(&[]);
            }

            match self.stream.next().await {
                Some(Ok(chunk)) => {
                    self.chunk = Some(chunk);
                    self.chunk_offset = 0;
                }
                Some(Err(_)) => {
                    return Err(ReaderTerminalError::StreamRead);
                }
                None => self.eof = true,
            }
        }
    }
}

/// Minimal shape we peel off an incoming frame to extract the JSON-RPC `id`.
/// Notifications (no `id`) are tolerated and surface as `None`.
#[derive(Deserialize)]
pub(super) struct IdProbe {
    pub(super) id: Option<serde_json::Value>,
}

/// Peel the JSON-RPC `id` out of a frame. Returns `None` for notifications or
/// frames without a numeric/string id. The id is returned in its raw JSON
/// form so we can compare against the outbound one with no coercion games.
pub(super) fn extract_id(frame: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<IdProbe>(frame)
        .ok()
        .and_then(|p| p.id)
}

/// Spawn the background reader task.
///
/// `event_stream` is the already-handshaked SSE event source (the initial
/// `endpoint` event has been consumed before this is called). The task
/// forwards `message` payloads onto `tx` and shuts down on either
/// `cancel` firing or the stream ending.
pub(super) fn spawn_reader<S, B, E>(
    mut event_stream: BoundedSseParser<S, B>,
    tx: mpsc::Sender<Result<InboundFrame, TransportError>>,
    byte_budget: Arc<Semaphore>,
    terminal_error: Arc<TerminalErrorLatch>,
    cancel: CancellationToken,
) -> JoinHandle<()>
where
    S: Stream<Item = Result<B, E>> + Send + Unpin + 'static,
    B: AsRef<[u8]> + Send + 'static,
    E: Display + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                next = event_stream.next_event() => match next {
                    Ok(Some(ev)) => {
                        // Only `message` events carry JSON-RPC payloads;
                        // ping / comment / re-emitted endpoint events are
                        // ignored on purpose so they don't confuse the
                        // `request` id-correlation buffer.
                        if ev.event == "message" {
                            let charge = match inbound_frame_charge(&ev.data) {
                                Ok(charge) => charge,
                                Err(error) => {
                                    terminal_error.set(error);
                                    tokio::select! {
                                        _ = cancel.cancelled() => {}
                                        _ = tx.send(Err(error.into_transport_error())) => {}
                                    }
                                    break;
                                }
                            };
                            let permit = match byte_budget.clone().try_acquire_many_owned(charge) {
                                Ok(permit) => permit,
                                Err(_) => {
                                    let error = ReaderTerminalError::AggregateBudget;
                                    terminal_error.set(error);
                                    tokio::select! {
                                        _ = cancel.cancelled() => {}
                                        _ = tx.send(Err(error.into_transport_error())) => {}
                                    }
                                    break;
                                }
                            };
                            let send_result = tokio::select! {
                                _ = cancel.cancelled() => break,
                                result = tx.send(Ok(InboundFrame::new(ev.data, permit))) => result,
                            };
                            if send_result.is_err() {
                                // Consumer dropped — nothing more to do.
                                break;
                            }
                        }
                    }
                    Ok(None) => {
                        if cancel.is_cancelled() {
                            break;
                        }
                        let error = ReaderTerminalError::UnexpectedEof;
                        terminal_error.set(error);
                        tokio::select! {
                            _ = cancel.cancelled() => {}
                            _ = tx.send(Err(TransportError::Closed)) => {}
                        }
                        break;
                    }
                    Err(error) => {
                        terminal_error.set(error);
                        tokio::select! {
                            _ = cancel.cancelled() => {}
                            _ = tx.send(Err(error.into_transport_error())) => {}
                        }
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use futures_util::stream;

    use super::*;

    #[tokio::test]
    async fn parser_accepts_exact_data_boundary() {
        let mut wire = b"event: message\ndata: ".to_vec();
        wire.extend(std::iter::repeat_n(b'x', MAX_SSE_EVENT_DATA_BYTES));
        wire.extend_from_slice(b"\n\n");
        let chunks = wire
            .chunks(4093)
            .map(|chunk| Ok::<_, &'static str>(chunk.to_vec()))
            .collect::<Vec<_>>();
        let mut parser = BoundedSseParser::new(stream::iter(chunks));

        let event = parser
            .next_event()
            .await
            .expect("boundary event parses")
            .expect("one event");
        assert_eq!(event.event, "message");
        assert_eq!(event.data.len(), MAX_SSE_EVENT_DATA_BYTES);
    }

    #[tokio::test]
    async fn parser_rejects_chunked_data_over_boundary_without_echoing_body() {
        const SENTINEL: &str = "oversized-sse-secret-sentinel";
        let mut wire = b"event: message\ndata: ".to_vec();
        wire.extend(std::iter::repeat_n(b'x', MAX_SSE_EVENT_DATA_BYTES));
        wire.extend_from_slice(SENTINEL.as_bytes());
        let chunks = wire
            .chunks(3079)
            .map(|chunk| Ok::<_, &'static str>(chunk.to_vec()))
            .collect::<Vec<_>>();
        let mut parser = BoundedSseParser::new(stream::iter(chunks));

        let error = parser
            .next_event()
            .await
            .expect_err("oversized event must fail");
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("limit"), "{diagnostic}");
        assert!(!diagnostic.contains(SENTINEL), "{diagnostic}");
    }

    #[tokio::test]
    async fn parser_ignores_one_bom_split_across_chunks() {
        let chunks = vec![
            Ok::<_, &'static str>(vec![0xef]),
            Ok(vec![0xbb]),
            Ok(vec![0xbf]),
            Ok(b"data: \xef".to_vec()),
            Ok(vec![0xbb]),
            Ok(b"\xbfx\n\n".to_vec()),
        ];
        let mut parser = BoundedSseParser::new(stream::iter(chunks));

        let event = parser
            .next_event()
            .await
            .expect("BOM-prefixed stream parses")
            .expect("one event");
        assert_eq!(event.event, "message");
        assert_eq!(
            event.data, "\u{feff}x",
            "only the stream-start BOM is ignored"
        );
    }

    #[tokio::test]
    async fn parser_accepts_cr_only_lines_across_chunks() {
        let chunks = vec![
            Ok::<_, &'static str>(b"event: message\r".to_vec()),
            Ok(b"data: one\r".to_vec()),
            Ok(b"data: two\r".to_vec()),
            Ok(b"\r".to_vec()),
        ];
        let mut parser = BoundedSseParser::new(stream::iter(chunks));

        let event = parser
            .next_event()
            .await
            .expect("CR-only stream parses")
            .expect("one event");
        assert_eq!(event.event, "message");
        assert_eq!(event.data, "one\ntwo");
    }

    #[tokio::test]
    async fn parser_treats_split_crlf_as_one_line_ending() {
        let chunks = vec![
            Ok::<_, &'static str>(b"event: custom\r".to_vec()),
            Ok(b"\ndata: value\r".to_vec()),
            Ok(b"\n\r".to_vec()),
            Ok(b"\n".to_vec()),
        ];
        let mut parser = BoundedSseParser::new(stream::iter(chunks));

        let event = parser
            .next_event()
            .await
            .expect("split CRLF stream parses")
            .expect("one event");
        assert_eq!(
            event.event, "custom",
            "the LF half must not act as an extra blank line"
        );
        assert_eq!(event.data, "value");
    }

    #[test]
    fn frame_charge_uses_retained_capacity_and_inline_storage() {
        let mut data = String::with_capacity(4096);
        data.push_str("{}");
        let charge = inbound_frame_charge(&data).expect("small frame charge fits");
        assert_eq!(
            charge as usize,
            data.capacity() + std::mem::size_of::<InboundFrame>()
        );
        assert!(charge as usize > data.len());
    }
}
