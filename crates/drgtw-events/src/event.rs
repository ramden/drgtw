//! [`UsageEvent`] — the sole wire-format event emitted by the gateway.
//!
//! ## Privacy invariant
//!
//! **This struct must NEVER include request content, response content, or API
//! keys.** All fields are limited to metadata required for billing, auditing,
//! and observability.  If you find yourself adding a `body`, `prompt`, or
//! `secret` field here, stop — that is a privacy violation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A single LLM-proxy usage event.
///
/// Emitted once per upstream request, regardless of streaming or fallbacks.
///
/// # Privacy guarantee
///
/// This struct contains **no request/response content and no API keys**.
/// Token counts, latency, model name, and routing metadata only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageEvent {
    /// Unique ID for the gateway request (e.g. a UUID or ULID).
    pub request_id: String,

    /// The caller key ID (opaque identifier — **not** the key secret).
    pub key_id: String,

    /// Which API endpoint was called: `"chat_completions"` or `"messages"`.
    pub endpoint: String,

    /// Model name as reported by the upstream (e.g. `"gpt-4o"`, `"claude-3-5-sonnet"`).
    pub model: String,

    /// Name of the upstream connection / provider used.
    pub connection: String,

    /// HTTP status code returned to the caller.
    pub status: u16,

    /// Prompt / input token count, if reported by the upstream.
    pub input_tokens: Option<u64>,

    /// Completion / output token count, if reported by the upstream.
    pub output_tokens: Option<u64>,

    /// Calculated cost in USD, if a matching entry existed in the cost table.
    pub cost_usd: Option<f64>,

    /// End-to-end latency from request received to last byte forwarded, in ms.
    pub latency_ms: u64,

    /// Whether the PII detector flagged this request.
    pub pii: bool,

    /// Whether the response was streamed (SSE).
    pub streamed: bool,

    /// Number of fallback/retry attempts before a successful or final response.
    pub fallback_attempts: u32,

    /// Unix timestamp (milliseconds) when the event was created.
    pub ts_unix_ms: u64,

    /// Caller-supplied attribution metadata for per-agent / per-session cost
    /// accounting.
    ///
    /// Sourced from the request body's top-level `metadata` object and from
    /// `x-drgtw-meta-*` request headers (header wins on key collision). Values
    /// are plain strings; non-string body values are JSON-stringified. Caps:
    /// at most 16 keys, keys ≤ 64 chars, values ≤ 256 chars (values are
    /// truncated, and excess keys are dropped in sorted order).
    ///
    /// `None` when no metadata was supplied — the field is omitted from the
    /// serialized event (backward compatible with existing receivers).
    ///
    /// # Privacy note
    ///
    /// This is OPERATOR-CONTROLLED routing/attribution metadata (session ids,
    /// agent names), never request or response content. It must not be used to
    /// smuggle prompt/response text.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metadata: Option<BTreeMap<String, String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_event() -> UsageEvent {
        UsageEvent {
            request_id: "req-1".to_owned(),
            key_id: "key-1".to_owned(),
            endpoint: "chat_completions".to_owned(),
            model: "gpt-4o".to_owned(),
            connection: "openai-main".to_owned(),
            status: 200,
            input_tokens: Some(10),
            output_tokens: Some(5),
            cost_usd: Some(0.01),
            latency_ms: 42,
            pii: false,
            streamed: false,
            fallback_attempts: 0,
            ts_unix_ms: 1_700_000_000_000,
            metadata: None,
        }
    }

    #[test]
    fn metadata_absent_is_omitted_from_json() {
        let json = serde_json::to_string(&base_event()).expect("serialize");
        assert!(
            !json.contains("metadata"),
            "metadata: None must be skipped for backward compatibility, got: {json}"
        );
    }

    #[test]
    fn metadata_present_is_serialized() {
        let mut md = BTreeMap::new();
        md.insert("session-id".to_owned(), "abc".to_owned());
        let ev = UsageEvent { metadata: Some(md), ..base_event() };
        let json = serde_json::to_string(&ev).expect("serialize");
        assert!(json.contains("\"metadata\""));
        assert!(json.contains("\"session-id\":\"abc\""));
    }

    #[test]
    fn metadata_roundtrips_and_legacy_json_deserializes() {
        // Legacy event without a metadata field still deserializes (default None).
        let legacy = r#"{
            "request_id":"r","key_id":"k","endpoint":"messages","model":"m",
            "connection":"c","status":200,"input_tokens":null,"output_tokens":null,
            "cost_usd":null,"latency_ms":1,"pii":false,"streamed":false,
            "fallback_attempts":0,"ts_unix_ms":1
        }"#;
        let ev: UsageEvent = serde_json::from_str(legacy).expect("deserialize legacy");
        assert_eq!(ev.metadata, None);

        // Full round-trip with metadata present.
        let mut md = BTreeMap::new();
        md.insert("agent".to_owned(), "planner".to_owned());
        let ev = UsageEvent { metadata: Some(md), ..base_event() };
        let json = serde_json::to_string(&ev).expect("serialize");
        let back: UsageEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ev, back);
    }
}
