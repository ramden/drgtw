//! Streaming usage capture (WP 8.3).
//!
//! Both proxied streaming paths need to record token usage for the usage-event
//! pipeline, *including* the plain (no-PII-restore) passthrough path that
//! previously forwarded upstream bytes untouched. This module provides a single
//! SSE-event-framing wrapper that:
//!
//! - splits the upstream byte stream into complete SSE events (reusing the same
//!   blank-line framing rules as [`crate::sse_restore`]);
//! - parses each event's `data:` JSON purely to read usage token counts —
//!   never mutating the bytes on the no-restore path;
//! - optionally runs PII restore through an [`SseRestorer`] on the restore
//!   path (in which case the restorer owns byte emission);
//! - fires a completion callback exactly once at stream end with the captured
//!   `(input_tokens, output_tokens)` (each `Option`, `None` when the upstream
//!   never reported usage — e.g. OpenAI without `stream_options.include_usage`).
//!
//! ## Usage extraction shapes
//! - OpenAI: the final chunk carries a top-level `usage` object
//!   (`prompt_tokens` / `completion_tokens`).
//! - Anthropic: `message_start` carries `message.usage.input_tokens`; the last
//!   `message_delta` carries the cumulative `usage.output_tokens`.
//!
//! ## Byte fidelity (no-restore path)
//! On the no-restore path the wrapper forwards each complete event's bytes
//! verbatim. Because SSE events are delimited by blank lines and we only ever
//! re-emit the exact bytes we buffered, the forwarded stream is byte-identical
//! to the upstream (the existing streaming-passthrough test still holds).

use bytes::Bytes;
use drgtw_events::{
    extract_usage_anthropic_stream_delta, extract_usage_anthropic_stream_start,
    extract_usage_openai,
};
use drgtw_pii::body::BodyFormat;
use futures::Stream;

use crate::sse_restore::SseRestorer;

/// Token usage captured while a stream flows.
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Accumulator that reads usage from each SSE event's JSON payload.
struct UsageAccumulator {
    format: BodyFormat,
    buf: Vec<u8>,
    usage: StreamUsage,
    done: bool,
}

impl UsageAccumulator {
    fn new(format: BodyFormat) -> Self {
        Self {
            format,
            buf: Vec::new(),
            usage: StreamUsage::default(),
            done: false,
        }
    }

    /// Feed raw upstream bytes; extract usage from any complete events.
    fn push(&mut self, chunk: &[u8]) {
        if self.done {
            return;
        }
        self.buf.extend_from_slice(chunk);
        while let Some(end) = find_event_boundary(&self.buf) {
            let event: Vec<u8> = self.buf.drain(..end).collect();
            self.scan_event(&event);
        }
    }

    /// Stream ended: scan any trailing buffered (delimiter-less) event.
    fn finish(&mut self) {
        if self.done {
            return;
        }
        if !self.buf.is_empty() {
            let leftover = std::mem::take(&mut self.buf);
            self.scan_event(&leftover);
        }
        self.done = true;
    }

    fn scan_event(&mut self, event: &[u8]) {
        let Ok(text) = std::str::from_utf8(event) else {
            return;
        };
        for line in text.lines() {
            let Some(payload) = data_payload(line) else {
                continue;
            };
            if payload.trim() == "[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
                continue;
            };
            match self.format {
                BodyFormat::OpenAiChat => {
                    if let Some((input, output)) = extract_usage_openai(&value) {
                        self.usage.input_tokens = Some(input);
                        self.usage.output_tokens = Some(output);
                    }
                }
                BodyFormat::AnthropicMessages => {
                    if let Some(input) = extract_usage_anthropic_stream_start(&value) {
                        self.usage.input_tokens = Some(input);
                    }
                    // message_delta carries cumulative output tokens; the last
                    // one wins.
                    if let Some(output) = extract_usage_anthropic_stream_delta(&value) {
                        self.usage.output_tokens = Some(output);
                    }
                }
            }
        }
    }
}

/// Build a stream that taps usage from the upstream SSE byte stream.
///
/// - When `restorer` is `Some`, PII restore is applied (the restorer owns byte
///   emission, exactly as the prior `restored_stream` did) AND usage is read.
/// - When `restorer` is `None`, bytes are forwarded verbatim and only usage is
///   read.
///
/// `on_complete` is invoked exactly once when the stream finishes, with the
/// captured usage. It must not block (used to emit a fire-and-forget event).
pub fn usage_tap_stream<S, F>(
    upstream: S,
    restorer: Option<SseRestorer>,
    format: BodyFormat,
    on_complete: F,
) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    F: FnOnce(StreamUsage) + Send + 'static,
{
    use futures::stream;
    use futures::StreamExt as _;

    struct State<S, F> {
        upstream: std::pin::Pin<Box<S>>,
        restorer: Option<SseRestorer>,
        acc: UsageAccumulator,
        on_complete: Option<F>,
        finished: bool,
    }

    let init = State {
        upstream: Box::pin(upstream),
        restorer,
        acc: UsageAccumulator::new(format),
        on_complete: Some(on_complete),
        finished: false,
    };

    stream::unfold(init, |mut st| async move {
        if st.finished {
            return None;
        }
        loop {
            match st.upstream.as_mut().next().await {
                None => {
                    st.finished = true;
                    // Flush restorer (emit any held-back synthetic event) and
                    // usage accumulator, then fire completion.
                    let tail = st
                        .restorer
                        .as_mut()
                        .map(|r| r.finish())
                        .unwrap_or_default();
                    st.acc.finish();
                    if let Some(cb) = st.on_complete.take() {
                        cb(st.acc.usage);
                    }
                    if tail.is_empty() {
                        return None;
                    }
                    return Some((Ok(Bytes::from(tail)), st));
                }
                Some(Err(_)) => {
                    // Upstream read error: stop, but still report whatever usage
                    // we captured so far.
                    st.finished = true;
                    st.acc.finish();
                    if let Some(cb) = st.on_complete.take() {
                        cb(st.acc.usage);
                    }
                    return None;
                }
                Some(Ok(chunk)) => {
                    // Always read usage.
                    st.acc.push(&chunk);
                    // Emit bytes: restored when a restorer is present, else
                    // verbatim.
                    let out = match st.restorer.as_mut() {
                        Some(r) => r.push(&chunk),
                        None => chunk.to_vec(),
                    };
                    if out.is_empty() {
                        // Restorer withheld the whole chunk (partial event /
                        // holdback); keep reading.
                        continue;
                    }
                    return Some((Ok(Bytes::from(out)), st));
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Framing helpers (mirror sse_restore's rules; kept local to avoid widening
// that module's public surface).
// ---------------------------------------------------------------------------

/// Find the index one-past the first event delimiter (`\n\n` or `\r\n\r\n`).
fn find_event_boundary(buf: &[u8]) -> Option<usize> {
    let mut lf = None;
    for i in 0..buf.len() {
        if buf[i] == b'\n' && i + 1 < buf.len() && buf[i + 1] == b'\n' {
            lf = Some(i + 2);
            break;
        }
    }
    let mut crlf = None;
    let win = b"\r\n\r\n";
    if buf.len() >= win.len() {
        for i in 0..=buf.len() - win.len() {
            if &buf[i..i + win.len()] == win {
                crlf = Some(i + win.len());
                break;
            }
        }
    }
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Extract the payload of an SSE `data:` line (single leading space stripped).
fn data_payload(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("data:")?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_usage_captured_from_final_chunk() {
        let mut acc = UsageAccumulator::new(BodyFormat::OpenAiChat);
        acc.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        acc.push(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":7}}\n\n");
        acc.push(b"data: [DONE]\n\n");
        acc.finish();
        assert_eq!(acc.usage.input_tokens, Some(12));
        assert_eq!(acc.usage.output_tokens, Some(7));
    }

    #[test]
    fn openai_no_usage_when_absent() {
        let mut acc = UsageAccumulator::new(BodyFormat::OpenAiChat);
        acc.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        acc.push(b"data: [DONE]\n\n");
        acc.finish();
        assert_eq!(acc.usage.input_tokens, None);
        assert_eq!(acc.usage.output_tokens, None);
    }

    #[test]
    fn anthropic_usage_from_start_and_delta() {
        let mut acc = UsageAccumulator::new(BodyFormat::AnthropicMessages);
        acc.push(
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":40,\"output_tokens\":0}}}\n\n",
        );
        acc.push(
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":3}}\n\n",
        );
        acc.push(
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":18}}\n\n",
        );
        acc.finish();
        assert_eq!(acc.usage.input_tokens, Some(40));
        // Cumulative: last delta wins.
        assert_eq!(acc.usage.output_tokens, Some(18));
    }

    #[test]
    fn usage_read_across_byte_splits() {
        let doc = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":9}}\n\ndata: [DONE]\n\n";
        for split in 1..doc.len() {
            let mut acc = UsageAccumulator::new(BodyFormat::OpenAiChat);
            acc.push(&doc[..split]);
            acc.push(&doc[split..]);
            acc.finish();
            assert_eq!(acc.usage.input_tokens, Some(5), "split {split}");
            assert_eq!(acc.usage.output_tokens, Some(9), "split {split}");
        }
    }
}
