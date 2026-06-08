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

/// A single audit-log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts_unix_ms: i64,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: serde_json::Value,
}

/// A user row returned by `find_user`.
#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
}
