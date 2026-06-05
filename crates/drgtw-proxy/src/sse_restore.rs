//! SSE-event-aware streaming PII restore.
//!
//! ## Why this exists (the bug it fixes)
//! The first streaming-restore implementation fed the **raw SSE bytes** of the
//! upstream response straight into [`drgtw_pii::StreamRestorer`]. That worked
//! only when a placeholder happened to land inside a single delta, contiguous
//! in the byte stream. Real upstreams (verified against Azure OpenAI) tokenise
//! placeholders across *multiple* SSE events: the reply "write to EMAIL_1
//! today" arrived as six separate `data:` events with delta contents
//! `"write"`, `" to"`, `" EMAIL"`, `"_"`, `"1"`, `" today"`. In the raw byte
//! stream those fragments are separated by JSON + SSE framing
//! (`EMAIL"}...\n\ndata: {"...":"_1`), so byte-level holdback can never join
//! `EMAIL` + `_` + `1` into `EMAIL_1`. The client received the literal
//! placeholder.
//!
//! ## The fix
//! Parse the SSE stream into events, pull the **text field** out of each data
//! event, and run a single [`StreamRestorer`] over the *concatenated text
//! stream* (not the raw bytes). The restorer's cross-chunk holdback now sees
//! `EMAIL` then `_` then `1` as consecutive text and reassembles `EMAIL_1`
//! before restoring it. Each event is re-serialised with its (possibly
//! held-back, possibly restored) text and emitted; non-text events and
//! non-data lines pass through untouched. Trailing holdback is flushed as a
//! synthetic text event before the terminator.
//!
//! ## Framing rules
//! - Events are delimited by a blank line: `\n\n` (also tolerating
//!   `\r\n\r\n`). Partial trailing events stay buffered until completed.
//! - UTF-8: incoming bytes are buffered raw; we only split on the ASCII
//!   delimiter bytes, so multi-byte sequences are never severed. JSON parsing
//!   of a complete event always sees valid UTF-8 (a complete SSE event is a
//!   complete JSON document).
//! - Within an event, lines starting `data:` carry the payload; everything
//!   else (`event:`, comments `:`, blanks) is preserved positionally.
//! - `data: [DONE]` (OpenAI) / `event: message_stop` (Anthropic) → flush the
//!   restorer's holdback as a synthetic text event *before* the terminator,
//!   then emit the terminator verbatim.

use drgtw_pii::{body::BodyFormat, StreamRestorer};

/// Stateful SSE transformer that restores PII placeholders across event
/// boundaries.
pub struct SseRestorer {
    restorer: StreamRestorer,
    format: BodyFormat,
    /// Incoming bytes not yet split into a complete event.
    buf: Vec<u8>,
    /// Set once the terminator has been emitted (or stream ended); further
    /// `push`/`finish` calls become no-ops to avoid double flush.
    flushed: bool,
    /// Last seen OpenAI chunk `id` (for the synthetic flush event).
    last_id: Option<String>,
    /// Last seen OpenAI chunk `model` (for the synthetic flush event).
    last_model: Option<String>,
    /// Last seen Anthropic `content_block_delta` index (for the synthetic
    /// flush event).
    last_index: u64,
}

impl SseRestorer {
    pub fn new(restorer: StreamRestorer, format: BodyFormat) -> Self {
        Self {
            restorer,
            format,
            buf: Vec::new(),
            flushed: false,
            last_id: None,
            last_model: None,
            last_index: 0,
        }
    }

    /// Feed a chunk of raw upstream bytes; return bytes ready to forward.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.flushed {
            // Terminator already emitted; relay any trailing bytes verbatim.
            return chunk.to_vec();
        }
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(event_end) = find_event_boundary(&self.buf) {
            // `event_end` is the index one past the blank-line delimiter.
            let event: Vec<u8> = self.buf.drain(..event_end).collect();
            self.process_event(&event, &mut out);
            if self.flushed {
                // Everything still buffered after the terminator is relayed
                // verbatim (e.g. bytes that arrived in the same chunk).
                out.append(&mut self.buf);
                break;
            }
        }
        out
    }

    /// Stream ended. Process any buffered partial event, then flush the
    /// restorer holdback as a final synthetic event if non-empty.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.flushed {
            return Vec::new();
        }
        let mut out = Vec::new();
        // Process whatever remains as a final (possibly delimiter-less) event.
        if !self.buf.is_empty() {
            let leftover: Vec<u8> = std::mem::take(&mut self.buf);
            self.process_event(&leftover, &mut out);
        }
        if !self.flushed {
            // No explicit terminator seen: flush holdback as a final synthetic
            // text event appended after everything else.
            let rest = self.restorer.finish();
            if !rest.is_empty() {
                out.extend_from_slice(self.synthetic_event(&rest).as_bytes());
            }
            self.flushed = true;
        }
        out
    }

    // -----------------------------------------------------------------------
    // Event processing
    // -----------------------------------------------------------------------

    /// Process one complete SSE event (its bytes include the trailing
    /// delimiter when one was present). Appends emitted bytes to `out`.
    fn process_event(&mut self, event: &[u8], out: &mut Vec<u8>) {
        // A complete SSE event is a complete UTF-8 document.
        let Ok(text) = std::str::from_utf8(event) else {
            // Should not happen for well-formed SSE; relay verbatim.
            out.extend_from_slice(event);
            return;
        };

        // Detect the OpenAI terminator and the Anthropic terminator.
        if is_done_event(text) {
            self.emit_flush_then(text, out);
            return;
        }
        if self.format == BodyFormat::AnthropicMessages && is_message_stop(text) {
            self.emit_flush_then(text, out);
            return;
        }

        // Walk the event line by line, transforming only `data:` JSON lines.
        let mut rebuilt = String::with_capacity(text.len());
        for line in split_keep_newlines(text) {
            let (content, newline) = split_trailing_newline(line);
            if let Some(payload) = data_payload(content) {
                let transformed = self.transform_data_payload(payload);
                rebuilt.push_str("data: ");
                rebuilt.push_str(&transformed);
            } else {
                rebuilt.push_str(content);
            }
            rebuilt.push_str(newline);
        }
        out.extend_from_slice(rebuilt.as_bytes());
    }

    /// Emit the holdback flush (synthetic event) then the terminator verbatim.
    fn emit_flush_then(&mut self, terminator: &str, out: &mut Vec<u8>) {
        let rest = self.restorer.finish();
        if !rest.is_empty() {
            out.extend_from_slice(self.synthetic_event(&rest).as_bytes());
        }
        out.extend_from_slice(terminator.as_bytes());
        self.flushed = true;
    }

    /// Transform a single JSON `data:` payload, returning the re-serialised
    /// JSON (or the original string when it is not JSON / has no text field).
    fn transform_data_payload(&mut self, payload: &str) -> String {
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(payload) else {
            // Not JSON (should not occur for these formats) — feed nothing,
            // pass through unchanged.
            return payload.to_owned();
        };

        match self.format {
            BodyFormat::OpenAiChat => self.transform_openai(&mut value),
            BodyFormat::AnthropicMessages => self.transform_anthropic(&mut value),
        }

        serde_json::to_string(&value).unwrap_or_else(|_| payload.to_owned())
    }

    /// OpenAI chat chunk: text lives at `choices[0].delta.content`.
    fn transform_openai(&mut self, value: &mut serde_json::Value) {
        // Remember id/model for a possible synthetic flush event.
        if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
            self.last_id = Some(id.to_owned());
        }
        if let Some(model) = value.get("model").and_then(|v| v.as_str()) {
            self.last_model = Some(model.to_owned());
        }

        // Only touch the content field when it is a present string.
        let Some(content) = value
            .get_mut("choices")
            .and_then(|c| c.get_mut(0))
            .and_then(|c| c.get_mut("delta"))
            .and_then(|d| d.get_mut("content"))
        else {
            return; // role-only / finish_reason / usage chunk: untouched.
        };
        if let Some(text) = content.as_str() {
            let released = self.restorer.feed(text);
            *content = serde_json::Value::String(released);
        }
    }

    /// Anthropic event: text lives in `content_block_delta` with
    /// `delta.type == "text_delta"` at `delta.text`.
    fn transform_anthropic(&mut self, value: &mut serde_json::Value) {
        let is_text_delta = value.get("type").and_then(|v| v.as_str())
            == Some("content_block_delta")
            && value
                .get("delta")
                .and_then(|d| d.get("type"))
                .and_then(|t| t.as_str())
                == Some("text_delta");
        if !is_text_delta {
            return; // everything else untouched.
        }
        if let Some(index) = value.get("index").and_then(|v| v.as_u64()) {
            self.last_index = index;
        }
        if let Some(text_field) = value.get_mut("delta").and_then(|d| d.get_mut("text"))
            && let Some(text) = text_field.as_str()
        {
            let released = self.restorer.feed(text);
            *text_field = serde_json::Value::String(released);
        }
    }

    /// Build a synthetic `data:` event carrying restored holdback text, shaped
    /// for the active body format and terminated with a blank line.
    fn synthetic_event(&self, rest: &str) -> String {
        match self.format {
            BodyFormat::OpenAiChat => {
                let id = self.last_id.as_deref().unwrap_or("drgtw");
                let mut obj = serde_json::json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "choices": [{
                        "index": 0,
                        "delta": {"content": rest},
                        "finish_reason": serde_json::Value::Null,
                    }],
                });
                if let Some(model) = &self.last_model {
                    obj.as_object_mut()
                        .unwrap()
                        .insert("model".to_owned(), serde_json::Value::String(model.clone()));
                }
                format!("data: {}\n\n", serde_json::to_string(&obj).unwrap())
            }
            BodyFormat::AnthropicMessages => {
                let obj = serde_json::json!({
                    "type": "content_block_delta",
                    "index": self.last_index,
                    "delta": {"type": "text_delta", "text": rest},
                });
                format!(
                    "event: content_block_delta\ndata: {}\n\n",
                    serde_json::to_string(&obj).unwrap()
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Framing helpers
// ---------------------------------------------------------------------------

/// Find the index one-past the first event delimiter (`\n\n` or `\r\n\r\n`).
/// Returns `None` when no complete event is buffered yet.
fn find_event_boundary(buf: &[u8]) -> Option<usize> {
    let mut lf = None; // index past first `\n\n`
    let mut crlf = None; // index past first `\r\n\r\n`
    for i in 0..buf.len() {
        if buf[i] == b'\n' && i + 1 < buf.len() && buf[i + 1] == b'\n' {
            lf = Some(i + 2);
            break;
        }
    }
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

/// Split a string into lines, each line *including* its trailing newline
/// (`\n` or `\r\n`). The final line may have no newline.
fn split_keep_newlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            out.push(&s[start..=i]);
            start = i + 1;
        }
        i += 1;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Split a line into (content, trailing-newline) where the newline is `\r\n`,
/// `\n`, or empty.
fn split_trailing_newline(line: &str) -> (&str, &str) {
    if let Some(stripped) = line.strip_suffix("\r\n") {
        (stripped, "\r\n")
    } else if let Some(stripped) = line.strip_suffix('\n') {
        (stripped, "\n")
    } else {
        (line, "")
    }
}

/// If `line` is an SSE `data:` line, return its payload (the part after
/// `data: ` / `data:`, trimmed of a single leading space per the SSE spec).
fn data_payload(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("data:")?;
    // SSE: a single leading space after the colon is stripped.
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// Does this event contain an OpenAI `data: [DONE]` line?
fn is_done_event(event: &str) -> bool {
    event
        .lines()
        .filter_map(data_payload)
        .any(|p| p.trim() == "[DONE]")
}

/// Does this Anthropic event carry `event: message_stop` or a
/// `"type":"message_stop"` data payload?
fn is_message_stop(event: &str) -> bool {
    for line in event.lines() {
        if line.strip_prefix("event:").map(str::trim) == Some("message_stop") {
            return true;
        }
        if let Some(payload) = data_payload(line)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(payload)
            && v.get("type").and_then(|t| t.as_str()) == Some("message_stop")
        {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Unit tests for the transformer (no network, deterministic via FakeMap).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use drgtw_pii::stream::testutil::{restorer_from_fake, FakeMap};

    fn fake_restorer(pairs: &[(&str, &str)]) -> StreamRestorer {
        let mut fake = FakeMap::new();
        for (ph, orig) in pairs {
            fake.insert(*ph, *orig);
        }
        restorer_from_fake(&fake)
    }

    /// Concatenate all `delta.content` strings across the emitted OpenAI stream.
    fn join_openai_content(bytes: &[u8]) -> String {
        let text = std::str::from_utf8(bytes).unwrap();
        let mut joined = String::new();
        for line in text.lines() {
            if let Some(payload) = data_payload(line) {
                if payload.trim() == "[DONE]" {
                    continue;
                }
                let v: serde_json::Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(c) = v["choices"][0]["delta"]["content"].as_str() {
                    joined.push_str(c);
                }
            }
        }
        joined
    }

    fn join_anthropic_text(bytes: &[u8]) -> String {
        let text = std::str::from_utf8(bytes).unwrap();
        let mut joined = String::new();
        for line in text.lines() {
            if let Some(payload) = data_payload(line) {
                let v: serde_json::Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v["type"] == "content_block_delta"
                    && v["delta"]["type"] == "text_delta"
                    && let Some(t) = v["delta"]["text"].as_str()
                {
                    joined.push_str(t);
                }
            }
        }
        joined
    }

    /// The Azure regression: placeholder split across six events.
    #[test]
    fn openai_placeholder_split_across_events() {
        let mut t = SseRestorer::new(
            fake_restorer(&[("EMAIL_1", "max.mustermann@example.com")]),
            BodyFormat::OpenAiChat,
        );
        let frags = ["write", " to", " EMAIL", "_", "1", " today"];
        let mut out = Vec::new();
        for frag in frags {
            let ev = format!(
                "data: {{\"id\":\"c1\",\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
                serde_json::to_string(frag).unwrap()
            );
            out.extend_from_slice(&t.push(ev.as_bytes()));
        }
        out.extend_from_slice(&t.push(b"data: [DONE]\n\n"));
        out.extend_from_slice(&t.finish());

        let joined = join_openai_content(&out);
        assert_eq!(joined, "write to max.mustermann@example.com today");
        assert!(!joined.contains("EMAIL_1"));
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("[DONE]"));
    }

    /// Placeholder whole in one event still works.
    #[test]
    fn openai_placeholder_whole_in_one_event() {
        let mut t = SseRestorer::new(
            fake_restorer(&[("EMAIL_1", "a@b.com")]),
            BodyFormat::OpenAiChat,
        );
        let mut out =
            t.push(b"data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"Hi EMAIL_1!\"}}]}\n\n");
        out.extend_from_slice(&t.push(b"data: [DONE]\n\n"));
        out.extend_from_slice(&t.finish());
        assert_eq!(join_openai_content(&out), "Hi a@b.com!");
    }

    /// Byte-split invariance: feeding the same 2-event doc split at *every*
    /// byte position must produce identical output.
    #[test]
    fn output_independent_of_byte_chunking() {
        let doc = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\" EMAIL\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"_1 done\"}}]}\n\n",
            "data: [DONE]\n\n",
        )
        .as_bytes();

        // Reference: whole document in one push.
        let mut ref_t = SseRestorer::new(
            fake_restorer(&[("EMAIL_1", "x@y.com")]),
            BodyFormat::OpenAiChat,
        );
        let mut reference = ref_t.push(doc);
        reference.extend_from_slice(&ref_t.finish());

        for split in 1..doc.len() {
            let mut t = SseRestorer::new(
                fake_restorer(&[("EMAIL_1", "x@y.com")]),
                BodyFormat::OpenAiChat,
            );
            let mut out = t.push(&doc[..split]);
            out.extend_from_slice(&t.push(&doc[split..]));
            out.extend_from_slice(&t.finish());
            assert_eq!(
                join_openai_content(&out),
                join_openai_content(&reference),
                "byte split at {split} changed restored text"
            );
        }
    }

    /// Role-only, finish_reason, and usage chunks pass through with fields
    /// preserved.
    #[test]
    fn openai_non_content_chunks_preserved() {
        let mut t = SseRestorer::new(
            fake_restorer(&[("EMAIL_1", "a@b.com")]),
            BodyFormat::OpenAiChat,
        );
        let role = b"data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n";
        let finish = b"data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let usage = b"data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"total_tokens\":5}}\n\n";
        let mut out = t.push(role);
        out.extend_from_slice(&t.push(finish));
        out.extend_from_slice(&t.push(usage));
        out.extend_from_slice(&t.push(b"data: [DONE]\n\n"));
        out.extend_from_slice(&t.finish());

        let s = std::str::from_utf8(&out).unwrap();
        // Semantics preserved: parse each event and check fields survive.
        let mut saw_role = false;
        let mut saw_finish = false;
        let mut saw_usage = false;
        for line in s.lines() {
            if let Some(p) = data_payload(line) {
                if p.trim() == "[DONE]" {
                    continue;
                }
                let v: serde_json::Value = serde_json::from_str(p).unwrap();
                if v["choices"][0]["delta"]["role"] == "assistant" {
                    saw_role = true;
                }
                if v["choices"][0]["finish_reason"] == "stop" {
                    saw_finish = true;
                }
                if v["usage"]["total_tokens"] == 5 {
                    saw_usage = true;
                }
            }
        }
        assert!(saw_role && saw_finish && saw_usage, "fields lost: {s}");
    }

    /// Anthropic: split text_delta restored, flushed on message_stop.
    #[test]
    fn anthropic_split_restored_and_message_stop_flush() {
        let mut t = SseRestorer::new(
            fake_restorer(&[("EMAIL_1", "a@b.com")]),
            BodyFormat::AnthropicMessages,
        );
        let mk = |txt: &str| {
            format!(
                "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}\n\n",
                serde_json::to_string(txt).unwrap()
            )
        };
        let mut out = Vec::new();
        for frag in ["see ", "EMAIL", "_", "1"] {
            out.extend_from_slice(&t.push(mk(frag).as_bytes()));
        }
        out.extend_from_slice(&t.push(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
        out.extend_from_slice(&t.finish());

        let joined = join_anthropic_text(&out);
        assert_eq!(joined, "see a@b.com");
        assert!(!joined.contains("EMAIL_1"));
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("message_stop"));
    }

    /// Trailing holdback (longest-match ambiguity) flushed via synthetic event
    /// before [DONE].
    #[test]
    fn trailing_holdback_flushed_before_done() {
        // EMAIL_1 and EMAIL_12 both known: after "EMAIL_1" the restorer holds
        // back because the next char could be "2" → EMAIL_12.
        let mut t = SseRestorer::new(
            fake_restorer(&[("EMAIL_1", "one@x.com"), ("EMAIL_12", "twelve@x.com")]),
            BodyFormat::OpenAiChat,
        );
        let mut out =
            t.push(b"data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hi EMAIL_1\"}}]}\n\n");
        // At this point the restorer holds back "EMAIL_1" (could become _12).
        out.extend_from_slice(&t.push(b"data: [DONE]\n\n"));
        out.extend_from_slice(&t.finish());

        let joined = join_openai_content(&out);
        assert_eq!(joined, "hi one@x.com");
        let s = std::str::from_utf8(&out).unwrap();
        // Synthetic flush event must appear before [DONE].
        let synth_pos = s.find("one@x.com").expect("synthetic flush missing");
        let done_pos = s.find("[DONE]").expect("DONE missing");
        assert!(synth_pos < done_pos, "flush must precede DONE: {s}");
    }
}
