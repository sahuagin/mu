//! Minimal SSE (Server-Sent Events) parser.
//!
//! SSE format (per the WHATWG spec, simplified for our needs):
//! - Each event is a sequence of `field: value\n` lines
//! - The blank line `\n\n` (or just `\n` after the last field's `\n`)
//!   terminates the event
//! - Fields we care about: `event` and `data`
//! - Multi-line `data` is concatenated with `\n`
//! - Lines starting with `:` are comments and ignored
//!
//! This implementation handles partial chunks: an SSE event may
//! span multiple `Bytes` from the underlying byte stream.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::stream::Stream;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// The concrete SSE stream both providers hold: byte source boxed with
/// its error pre-rendered to String, the SseStream itself left concrete
/// so [`SseStream::take_transport_error`] stays reachable after EOF
/// (boxing the outer stream erases it).
pub type ByteSse = SseStream<Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>>;

/// Wrap a stream of `Bytes` (e.g., reqwest's `bytes_stream()`) into a
/// stream of parsed `SseEvent`s. Errors from the underlying stream
/// terminate the SSE stream.
pub struct SseStream<S> {
    inner: S,
    buffer: String,
    /// Trailing bytes of an incomplete UTF-8 code point split across
    /// chunk boundaries, carried into the next chunk.
    pending_bytes: Vec<u8>,
    pending_event: Option<String>,
    pending_data: Vec<String>,
    done: bool,
    transport_error: Option<String>,
}

impl<S> SseStream<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: String::new(),
            pending_bytes: Vec::new(),
            pending_event: None,
            pending_data: Vec::new(),
            done: false,
            transport_error: None,
        }
    }

    /// A transport error terminates the SSE stream like EOF (the Item
    /// type carries no error channel), but the consumer must be able to
    /// tell a dropped connection from a clean close — a mid-stream drop
    /// otherwise masquerades as a complete-looking truncated turn.
    pub fn take_transport_error(&mut self) -> Option<String> {
        self.transport_error.take()
    }

    /// Parse complete events from the buffer; returns the next ready
    /// event and removes its bytes from the buffer.
    fn pop_event(&mut self) -> Option<SseEvent> {
        loop {
            // Find a complete line.
            let nl = self.buffer.find('\n')?;
            let line: String = self.buffer.drain(..=nl).collect();
            // Strip trailing \r\n or \n.
            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');

            if trimmed.is_empty() {
                // End of event. If we have any data, emit it.
                if !self.pending_data.is_empty() || self.pending_event.is_some() {
                    let event = SseEvent {
                        event: self.pending_event.take(),
                        data: self.pending_data.join("\n"),
                    };
                    self.pending_data.clear();
                    return Some(event);
                }
                continue;
            }
            if trimmed.starts_with(':') {
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("event:") {
                self.pending_event = Some(rest.trim_start().to_string());
            } else if let Some(rest) = trimmed.strip_prefix("data:") {
                self.pending_data.push(rest.trim_start().to_string());
            }
            // Other fields (id, retry) are ignored.
        }
    }
}

impl<S, E> Stream for SseStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    type Item = SseEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // Drain any complete events already buffered.
            if let Some(event) = self.pop_event() {
                return Poll::Ready(Some(event));
            }
            if self.done {
                return Poll::Ready(None);
            }
            // Pull more bytes.
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    // Chunks can split a multi-byte UTF-8 code point, so
                    // decode over carried + new bytes and keep any
                    // incomplete trailing code point for the next chunk.
                    self.pending_bytes.extend_from_slice(&bytes);
                    let pending = std::mem::take(&mut self.pending_bytes);
                    match std::str::from_utf8(&pending) {
                        Ok(s) => {
                            self.buffer.push_str(s);
                        }
                        Err(e) if e.error_len().is_none() => {
                            let valid = e.valid_up_to();
                            let s = std::str::from_utf8(&pending[..valid])
                                .expect("valid_up_to prefix is valid");
                            self.buffer.push_str(s);
                            self.pending_bytes = pending[valid..].to_vec();
                        }
                        Err(_) => {
                            // Genuinely invalid bytes are fatal — and must
                            // read as a transport failure, not a clean close.
                            self.transport_error = Some("invalid UTF-8 in SSE stream".to_string());
                            self.done = true;
                            return Poll::Ready(None);
                        }
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    self.transport_error = Some(e.to_string());
                    self.done = true;
                    return Poll::Ready(None);
                }
                Poll::Ready(None) => {
                    self.done = true;
                    if !self.pending_bytes.is_empty() {
                        // EOF mid-code-point: the stream was cut, not closed.
                        self.transport_error =
                            Some("stream ended mid-UTF-8 code point".to_string());
                        return Poll::Ready(None);
                    }
                    // Flush any trailing event without a blank-line terminator.
                    if !self.pending_data.is_empty() || self.pending_event.is_some() {
                        let event = SseEvent {
                            event: self.pending_event.take(),
                            data: self.pending_data.join("\n"),
                        };
                        self.pending_data.clear();
                        return Poll::Ready(Some(event));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, StreamExt};

    fn ok(b: &str) -> Result<Bytes, std::io::Error> {
        Ok(Bytes::copy_from_slice(b.as_bytes()))
    }

    #[tokio::test]
    async fn transport_error_terminates_and_is_retrievable() {
        let bytes = stream::iter(vec![
            ok("event: foo\ndata: x\n\n"),
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            )),
        ]);
        let mut sse = SseStream::new(Box::pin(bytes));
        assert!(sse.next().await.is_some(), "buffered event still yielded");
        assert!(sse.next().await.is_none(), "transport error ends stream");
        let err = sse.take_transport_error().expect("error recorded");
        assert!(err.contains("connection reset"));
        assert!(sse.take_transport_error().is_none(), "take drains");
    }

    #[tokio::test]
    async fn non_utf8_reads_as_transport_error_not_clean_eof() {
        let bytes = stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from_static(&[
            0xff, 0xfe, 0xfd,
        ]))]);
        let mut sse = SseStream::new(Box::pin(bytes));
        assert!(sse.next().await.is_none());
        let err = sse
            .take_transport_error()
            .expect("recorded as transport error");
        assert!(err.contains("invalid UTF-8"));
    }

    #[tokio::test]
    async fn multibyte_code_point_split_across_chunks_decodes() {
        // "é" (0xC3 0xA9) split across two chunks must decode, not
        // misread as invalid UTF-8.
        let bytes = stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"data: caf\xc3")),
            Ok(Bytes::from_static(b"\xa9\n\n")),
        ]);
        let mut sse = SseStream::new(Box::pin(bytes));
        let ev = sse.next().await.expect("event decodes");
        assert_eq!(ev.data, "café");
        assert!(sse.next().await.is_none());
        assert!(sse.take_transport_error().is_none(), "no spurious error");
    }

    #[tokio::test]
    async fn b3_multi_chunk_event() {
        // First chunk has the event line + start of data; second chunk
        // has the rest of data + the blank-line terminator.
        let bytes = stream::iter(vec![ok("event: foo\ndata: par"), ok("tial\n\n")]);
        let events: Vec<_> = SseStream::new(bytes).collect().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("foo"));
        assert_eq!(events[0].data, "partial");
    }

    #[tokio::test]
    async fn multiple_events_in_one_chunk() {
        let bytes = stream::iter(vec![ok("event: a\ndata: 1\n\nevent: b\ndata: 2\n\n")]);
        let events: Vec<_> = SseStream::new(bytes).collect().await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("a"));
        assert_eq!(events[0].data, "1");
        assert_eq!(events[1].event.as_deref(), Some("b"));
        assert_eq!(events[1].data, "2");
    }

    #[tokio::test]
    async fn comment_lines_ignored() {
        let bytes = stream::iter(vec![ok(": this is a heartbeat\nevent: x\ndata: y\n\n")]);
        let events: Vec<_> = SseStream::new(bytes).collect().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("x"));
        assert_eq!(events[0].data, "y");
    }

    #[tokio::test]
    async fn data_only_event() {
        let bytes = stream::iter(vec![ok("data: payload\n\n")]);
        let events: Vec<_> = SseStream::new(bytes).collect().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, None);
        assert_eq!(events[0].data, "payload");
    }
}
