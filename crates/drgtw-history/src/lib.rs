//! Postgres-backed history/audit/session store for the gateway admin UI.
//!
//! # Feature gating
//!
//! The `postgres` feature is **off by default**. With it off, every public
//! type and function still compiles — mutating calls return
//! `Err(HistoryError::Disabled)` and query calls return empty results.
//! The gateway binary can therefore call this crate unconditionally; the
//! database becomes active only when the operator enables the feature and
//! supplies a `DATABASE_URL`.

pub mod error;
pub mod handle;
pub mod types;

// Implements drgtw_events::DeliveryLog for History (bridges events ↔ history
// without a dependency cycle: events does not import history).
pub mod delivery_log;

#[cfg(feature = "postgres")]
pub(crate) mod pg;

#[cfg(test)]
mod tests;

// Re-export the most-used surface so callers can `use drgtw_history::*;`.
pub use error::HistoryError;
pub use handle::History;
pub use types::{
    AuditEntry, Bucket, DimCount, PiiDetectionRow, UsageBucket, UsageSummary, UserRow,
    WebhookDeliveryRow,
};
