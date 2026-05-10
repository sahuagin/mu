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

/// Wrap a stream of `Bytes` (e.g., reqwest's `bytes_stream()`) into a
/// stream of parsed `SseEvent`s. Errors from the underlying stream
/// terminate the SSE stream.
pub struct SseStream<S> {
    inner: S,
    buffer: String,
    pending_event: Option<String>,
    pending_data: Vec<String>,
    done: bool,
}

impl<S> SseStream<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: String::new(),
            pending_event: None,
            pending_data: Vec::new(),
            done: false,
        }
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
                    if let Ok(s) = std::str::from_utf8(&bytes) {
                        self.buffer.push_str(s);
                    } else {
                        // Non-UTF-8 in an SSE stream is fatal.
                        self.done = true;
                        return Poll::Ready(None);
                    }
                }
                Poll::Ready(Some(Err(_))) => {
                    self.done = true;
                    return Poll::Ready(None);
                }
                Poll::Ready(None) => {
                    self.done = true;
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
    async fn b3_multi_chunk_event() {
        // First chunk has the event line + start of data; second chunk
        // has the rest of data + the blank-line terminator.
        let bytes = stream::iter(vec![
            ok("event: foo\ndata: par"),
            ok("tial\n\n"),
        ]);
        let events: Vec<_> = SseStream::new(bytes).collect().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("foo"));
        assert_eq!(events[0].data, "partial");
    }

    #[tokio::test]
    async fn multiple_events_in_one_chunk() {
        let bytes = stream::iter(vec![ok(
            "event: a\ndata: 1\n\nevent: b\ndata: 2\n\n",
        )]);
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
