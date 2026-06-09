use serde::{Deserialize, Serialize};

/// Time-series aggregation granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Hour,
    Day,
}

impl Bucket {
    /// Postgres `date_trunc` argument.
    #[cfg(feature = "postgres")]
    pub(crate) fn pg_trunc(&self) -> &'static str {
        match self {
            Bucket::Hour => "hour",
            Bucket::Day => "day",
        }
    }

    /// Milliseconds per bucket (used for pure-Rust helpers).
    pub fn ms(&self) -> u64 {
        match self {
            Bucket::Hour => 3_600_000,
            Bucket::Day => 86_400_000,
        }
    }
}

/// One aggregated usage bucket returned by `usage_timeseries`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageBucket {
    pub ts_ms: i64,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    pub avg_latency_ms: f64,
}

/// A single-row aggregate summary returned by `usage_summary`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    pub avg_latency_ms: f64,
    pub pii_count: i64,
    pub error_count: i64,
}

/// One grouped row returned by `usage_by_*` (model/connection/endpoint).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimCount {
    pub label: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

/// A single audit-log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts_unix_ms: i64,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: serde_json::Value,
}

/// A user row returned by `find_user` / `list_users`.
#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
}

/// One PII detection record (one entity kind detected in a single request).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiiDetectionRow {
    pub request_id: String,
    pub key_id: String,
    pub entity_kind: String,
    pub count: i32,
    pub ts_unix_ms: i64,
}

/// One webhook delivery attempt recorded by [`EventSink`].
///
/// `id` is `None` before insertion (BIGSERIAL assigned by Postgres).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDeliveryRow {
    /// Postgres-assigned delivery id; `None` before first insert.
    pub id: Option<i64>,
    /// The gateway request this delivery corresponds to.
    pub request_id: String,
    /// Delivery attempt timestamp (ms since Unix epoch).
    pub ts_unix_ms: i64,
    /// HTTP status code returned by the upstream endpoint, if any.
    pub status_code: Option<i32>,
    /// `true` when the delivery was accepted (2xx response).
    pub ok: bool,
    /// Transport or serialisation error message, if delivery failed.
    pub error: Option<String>,
    /// 1-based attempt number (first try = 1).
    pub attempt: i32,
    /// The JSON payload that was sent to the upstream endpoint.
    pub payload: serde_json::Value,
}
