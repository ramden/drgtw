//! [`UsageEvent`] — the sole wire-format event emitted by the gateway.
//!
//! ## Privacy invariant
//!
//! **This struct must NEVER include request content, response content, or API
//! keys.** All fields are limited to metadata required for billing, auditing,
//! and observability.  If you find yourself adding a `body`, `prompt`, or
//! `secret` field here, stop — that is a privacy violation.

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
}
