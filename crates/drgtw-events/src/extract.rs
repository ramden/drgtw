//! Pure token-extraction helpers for OpenAI and Anthropic response bodies.
//!
//! All functions are pure (no I/O, no allocation beyond parsing) and accept
//! a reference to a [`serde_json::Value`] already parsed from the response
//! body or a single SSE data payload.
//!
//! ## OpenAI usage
//!
//! Non-streaming: `usage.prompt_tokens` / `usage.completion_tokens` in the
//! top-level response object.
//!
//! Streaming (with `stream_options: { include_usage: true }`): the final
//! chunk carries a `usage` object with the same field names; `choices` may be
//! empty in that final chunk.  Pass each parsed SSE `data:` payload to
//! [`extract_usage_openai`] — it returns `Some` only for the chunk that
//! carries usage.
//!
//! ## Anthropic usage
//!
//! Non-streaming: `usage.input_tokens` / `usage.output_tokens` in the
//! top-level response object.
//!
//! Streaming:
//! - `message_start` event carries `message.usage.input_tokens` — use
//!   [`extract_usage_anthropic_stream_start`].
//! - `message_delta` event carries `usage.output_tokens` (cumulative) — use
//!   [`extract_usage_anthropic_stream_delta`].
//! - [`extract_usage_anthropic`] also works for `message_delta`-shaped payloads
//!   that happen to have both fields under `usage`.

use serde_json::Value;

// ── OpenAI ───────────────────────────────────────────────────────────────────

/// Extract `(input_tokens, output_tokens)` from an OpenAI response body or
/// a streaming chunk that carries `usage` (requires `stream_options: {include_usage: true}`).
///
/// Returns `None` if `usage` is absent or either field is missing/not a number.
///
/// Field mapping: `usage.prompt_tokens` → input, `usage.completion_tokens` → output.
pub fn extract_usage_openai(body: &Value) -> Option<(u64, u64)> {
    let usage = body.get("usage")?;
    let input = usage.get("prompt_tokens")?.as_u64()?;
    let output = usage.get("completion_tokens")?.as_u64()?;
    Some((input, output))
}

// ── Anthropic ────────────────────────────────────────────────────────────────

/// Extract `(input_tokens, output_tokens)` from an Anthropic non-streaming
/// response body.
///
/// Field mapping: `usage.input_tokens` → input, `usage.output_tokens` → output.
///
/// Also works for any payload where both fields live directly under `usage`.
pub fn extract_usage_anthropic(body: &Value) -> Option<(u64, u64)> {
    let usage = body.get("usage")?;
    let input = usage.get("input_tokens")?.as_u64()?;
    let output = usage.get("output_tokens")?.as_u64()?;
    Some((input, output))
}

/// Extract the **input** token count from an Anthropic streaming `message_start`
/// event payload.
///
/// Shape: `{ "type": "message_start", "message": { "usage": { "input_tokens": N } } }`
///
/// Returns `None` if the path or field is absent.
pub fn extract_usage_anthropic_stream_start(ev: &Value) -> Option<u64> {
    ev.get("message")?
        .get("usage")?
        .get("input_tokens")?
        .as_u64()
}

/// Extract the **cumulative output** token count from an Anthropic streaming
/// `message_delta` event payload.
///
/// Shape: `{ "type": "message_delta", "usage": { "output_tokens": N } }`
///
/// The value is cumulative (the final `message_delta` holds the total output
/// tokens for the stream).  Returns `None` if the path or field is absent.
pub fn extract_usage_anthropic_stream_delta(ev: &Value) -> Option<u64> {
    ev.get("usage")?.get("output_tokens")?.as_u64()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── OpenAI non-streaming ─────────────────────────────────────────────────

    #[test]
    fn openai_non_stream_basic() {
        let body = json!({
            "id": "chatcmpl-abc123",
            "object": "chat.completion",
            "choices": [{"message": {"role": "assistant", "content": "Hello"}}],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            }
        });
        assert_eq!(extract_usage_openai(&body), Some((100, 50)));
    }

    #[test]
    fn openai_non_stream_missing_usage() {
        let body = json!({ "id": "chatcmpl-abc123", "choices": [] });
        assert_eq!(extract_usage_openai(&body), None);
    }

    #[test]
    fn openai_non_stream_partial_usage() {
        // completion_tokens missing → None
        let body = json!({ "usage": { "prompt_tokens": 100 } });
        assert_eq!(extract_usage_openai(&body), None);
    }

    // ── OpenAI streaming final chunk (stream_options: {include_usage: true}) ─

    #[test]
    fn openai_stream_usage_chunk_include_usage_shape() {
        // Final chunk: choices may be empty, usage is present
        let chunk = json!({
            "id": "chatcmpl-xyz",
            "object": "chat.completion.chunk",
            "choices": [],
            "usage": {
                "prompt_tokens": 200,
                "completion_tokens": 80,
                "total_tokens": 280
            }
        });
        assert_eq!(extract_usage_openai(&chunk), Some((200, 80)));
    }

    #[test]
    fn openai_stream_normal_chunk_no_usage() {
        // Regular streaming chunk — no usage field
        let chunk = json!({
            "id": "chatcmpl-xyz",
            "object": "chat.completion.chunk",
            "choices": [{"delta": {"content": "Hello"}}]
        });
        assert_eq!(extract_usage_openai(&chunk), None);
    }

    // ── Anthropic non-streaming ───────────────────────────────────────────────

    #[test]
    fn anthropic_non_stream_basic() {
        let body = json!({
            "id": "msg_abc",
            "type": "message",
            "content": [{"type": "text", "text": "Hello"}],
            "usage": {
                "input_tokens": 300,
                "output_tokens": 120
            }
        });
        assert_eq!(extract_usage_anthropic(&body), Some((300, 120)));
    }

    #[test]
    fn anthropic_non_stream_missing_usage() {
        let body = json!({ "type": "message", "content": [] });
        assert_eq!(extract_usage_anthropic(&body), None);
    }

    #[test]
    fn anthropic_non_stream_partial_usage() {
        // output_tokens missing
        let body = json!({ "usage": { "input_tokens": 300 } });
        assert_eq!(extract_usage_anthropic(&body), None);
    }

    // ── Anthropic streaming: message_start ───────────────────────────────────

    #[test]
    fn anthropic_stream_start_basic() {
        let ev = json!({
            "type": "message_start",
            "message": {
                "id": "msg_abc",
                "type": "message",
                "usage": {
                    "input_tokens": 500,
                    "output_tokens": 0
                }
            }
        });
        assert_eq!(extract_usage_anthropic_stream_start(&ev), Some(500));
    }

    #[test]
    fn anthropic_stream_start_missing_message() {
        let ev = json!({ "type": "message_start" });
        assert_eq!(extract_usage_anthropic_stream_start(&ev), None);
    }

    #[test]
    fn anthropic_stream_start_missing_usage_field() {
        let ev = json!({
            "type": "message_start",
            "message": { "id": "msg_abc" }
        });
        assert_eq!(extract_usage_anthropic_stream_start(&ev), None);
    }

    // ── Anthropic streaming: message_delta ───────────────────────────────────

    #[test]
    fn anthropic_stream_delta_basic() {
        let ev = json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn" },
            "usage": { "output_tokens": 250 }
        });
        assert_eq!(extract_usage_anthropic_stream_delta(&ev), Some(250));
    }

    #[test]
    fn anthropic_stream_delta_missing_usage() {
        let ev = json!({ "type": "message_delta", "delta": {} });
        assert_eq!(extract_usage_anthropic_stream_delta(&ev), None);
    }

    #[test]
    fn anthropic_stream_delta_missing_output_tokens() {
        let ev = json!({ "type": "message_delta", "usage": {} });
        assert_eq!(extract_usage_anthropic_stream_delta(&ev), None);
    }

    // ── Zero-value edge cases ─────────────────────────────────────────────────

    #[test]
    fn openai_zero_tokens() {
        let body = json!({ "usage": { "prompt_tokens": 0, "completion_tokens": 0 } });
        assert_eq!(extract_usage_openai(&body), Some((0, 0)));
    }

    #[test]
    fn anthropic_zero_tokens() {
        let body = json!({ "usage": { "input_tokens": 0, "output_tokens": 0 } });
        assert_eq!(extract_usage_anthropic(&body), Some((0, 0)));
    }
}
