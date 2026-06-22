use bytes::Bytes;
use futures::Stream;
use std::{
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SseError {
    #[error("byte stream error: {0}")]
    Bytes(String),
    #[error("invalid utf-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),
}

pub struct SseStream<S> {
    inner: S,
    buf: Vec<u8>,
    done: bool,
}
impl<S> SseStream<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            done: false,
        }
    }
}

impl<S, E> Stream for SseStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    type Item = Result<SseEvent, SseError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some((pos, len)) = find_frame_boundary(&self.buf) {
                let frame = self.buf.drain(..pos + len).collect::<Vec<_>>();
                return Poll::Ready(Some(parse_frame(&frame)));
            }
            if self.done {
                if self.buf.is_empty() {
                    return Poll::Ready(None);
                }
                let frame = std::mem::take(&mut self.buf);
                return Poll::Ready(Some(parse_frame(&frame)));
            }
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => self.buf.extend_from_slice(&bytes),
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(SseError::Bytes(e.to_string()))))
                }
                Poll::Ready(None) => self.done = true,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
fn find_frame_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len() {
        if buf[i..].starts_with(b"\r\n\r\n") {
            return Some((i, 4));
        }
        if buf[i..].starts_with(b"\n\n") {
            return Some((i, 2));
        }
    }
    None
}
fn parse_frame(frame: &[u8]) -> Result<SseEvent, SseError> {
    let s = std::str::from_utf8(frame)?.trim_end_matches(['\r', '\n']);
    let mut event = None;
    let mut data = String::new();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim_start().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }
    Ok(SseEvent { event, data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn parses_lf_delimited_frame() {
        let bytes = futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(
            b"event: message\ndata: {\"x\":1}\n\n",
        ))]);
        let ev = SseStream::new(bytes).next().await.unwrap().unwrap();
        assert_eq!(ev.event.as_deref(), Some("message"));
        assert_eq!(ev.data, "{\"x\":1}");
    }

    #[tokio::test]
    async fn parses_crlf_delimited_frame() {
        let bytes = futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(
            b"event: message\r\ndata: {\"x\":1}\r\n\r\n",
        ))]);
        let ev = SseStream::new(bytes).next().await.unwrap().unwrap();
        assert_eq!(ev.event.as_deref(), Some("message"));
        assert_eq!(ev.data, "{\"x\":1}");
    }
}
