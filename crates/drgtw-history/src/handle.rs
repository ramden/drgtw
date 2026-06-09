//! `History` handle — identical public surface under both feature states.

use drgtw_events::UsageEvent;

use crate::error::HistoryError;
use crate::types::{
    AuditEntry, Bucket, DimCount, PiiDetectionRow, UsageBucket, UsageSummary, UserRow,
    WebhookDeliveryRow,
};

// ── Inner state ───────────────────────────────────────────────────────────────

enum Inner {
    Disabled,
    #[cfg(feature = "postgres")]
    Pg(sqlx::PgPool),
}

// ── Public handle ─────────────────────────────────────────────────────────────

/// Opaque handle to the history store.
///
/// Construct with [`History::connect`] (requires `postgres` feature) or
/// [`History::disabled`] (always available, returns `Err(HistoryError::Disabled)`
/// from every mutating call and empty slices from queries).
pub struct History {
    inner: Inner,
}

impl History {
    /// Always-available constructor that produces a disabled, no-op handle.
    pub fn disabled() -> Self {
        History {
            inner: Inner::Disabled,
        }
    }

    /// Connect to Postgres, run embedded migrations, and return a live handle.
    ///
    /// Available only with `--features postgres`.
    #[cfg(feature = "postgres")]
    pub async fn connect(url: &str) -> Result<Self, HistoryError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect(url)
            .await?;
        crate::pg::run_migrations(&pool).await?;
        Ok(History {
            inner: Inner::Pg(pool),
        })
    }

    // ── Usage events ──────────────────────────────────────────────────────────

    pub async fn record_usage(&self, ev: &UsageEvent) -> Result<(), HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = ev;
                Err(HistoryError::Disabled)
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::record_usage(pool, ev).await,
        }
    }

    pub async fn record_usage_batch(&self, evs: &[UsageEvent]) -> Result<(), HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = evs;
                Err(HistoryError::Disabled)
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::record_usage_batch(pool, evs).await,
        }
    }

    pub async fn recent_usage(&self, limit: u32) -> Result<Vec<UsageEvent>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = limit;
                Ok(vec![])
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::recent_usage(pool, limit as i64).await,
        }
    }

    pub async fn usage_timeseries(
        &self,
        since_ms: i64,
        until_ms: i64,
        bucket: Bucket,
    ) -> Result<Vec<UsageBucket>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = (since_ms, until_ms, bucket);
                Ok(vec![])
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => {
                crate::pg::usage_timeseries(pool, since_ms, until_ms, bucket).await
            }
        }
    }

    pub async fn usage_summary(
        &self,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<UsageSummary, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = (since_ms, until_ms);
                Ok(UsageSummary {
                    requests: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cost_usd: 0.0,
                    avg_latency_ms: 0.0,
                    pii_count: 0,
                    error_count: 0,
                })
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::usage_summary(pool, since_ms, until_ms).await,
        }
    }

    pub async fn usage_by_model(
        &self,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<DimCount>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = (since_ms, until_ms);
                Ok(vec![])
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::usage_by(pool, since_ms, until_ms, "model").await,
        }
    }

    pub async fn usage_by_connection(
        &self,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<DimCount>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = (since_ms, until_ms);
                Ok(vec![])
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => {
                crate::pg::usage_by(pool, since_ms, until_ms, "connection").await
            }
        }
    }

    pub async fn usage_by_endpoint(
        &self,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<DimCount>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = (since_ms, until_ms);
                Ok(vec![])
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::usage_by(pool, since_ms, until_ms, "endpoint").await,
        }
    }

    // ── Audit log ─────────────────────────────────────────────────────────────

    pub async fn append_audit(&self, entry: &AuditEntry) -> Result<(), HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = entry;
                Err(HistoryError::Disabled)
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::append_audit(pool, entry).await,
        }
    }

    pub async fn recent_audit(&self, limit: u32) -> Result<Vec<AuditEntry>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = limit;
                Ok(vec![])
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::recent_audit(pool, limit as i64).await,
        }
    }

    // ── Users ─────────────────────────────────────────────────────────────────

    pub async fn create_user(
        &self,
        username: &str,
        password_hash: &str,
    ) -> Result<i64, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = (username, password_hash);
                Err(HistoryError::Disabled)
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::create_user(pool, username, password_hash).await,
        }
    }

    pub async fn find_user(&self, username: &str) -> Result<Option<UserRow>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = username;
                Ok(None)
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::find_user(pool, username).await,
        }
    }

    // ── Sessions ──────────────────────────────────────────────────────────────

    pub async fn create_session(
        &self,
        session_id: &str,
        user_id: i64,
        expires_ms: i64,
    ) -> Result<(), HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = (session_id, user_id, expires_ms);
                Err(HistoryError::Disabled)
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => {
                crate::pg::create_session(pool, session_id, user_id, expires_ms).await
            }
        }
    }

    pub async fn get_session(
        &self,
        session_id: &str,
    ) -> Result<Option<(i64, i64)>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = session_id;
                Ok(None)
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::get_session(pool, session_id).await,
        }
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<(), HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = session_id;
                Err(HistoryError::Disabled)
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::delete_session(pool, session_id).await,
        }
    }

    // ── Per-key usage queries ─────────────────────────────────────────────────

    pub async fn usage_summary_by_key(
        &self,
        key_id: &str,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<UsageSummary, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = (key_id, since_ms, until_ms);
                Ok(UsageSummary {
                    requests: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cost_usd: 0.0,
                    avg_latency_ms: 0.0,
                    pii_count: 0,
                    error_count: 0,
                })
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => {
                crate::pg::usage_summary_by_key(pool, key_id, since_ms, until_ms).await
            }
        }
    }

    pub async fn usage_timeseries_by_key(
        &self,
        key_id: &str,
        since_ms: i64,
        until_ms: i64,
        bucket: Bucket,
    ) -> Result<Vec<UsageBucket>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = (key_id, since_ms, until_ms, bucket);
                Ok(vec![])
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => {
                crate::pg::usage_timeseries_by_key(pool, key_id, since_ms, until_ms, bucket).await
            }
        }
    }

    /// Aggregate usage grouped by `key_id` across `[since_ms, until_ms)`.
    pub async fn usage_by_key(
        &self,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<DimCount>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = (since_ms, until_ms);
                Ok(vec![])
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::usage_by_key(pool, since_ms, until_ms).await,
        }
    }

    // ── User management ───────────────────────────────────────────────────────

    pub async fn list_users(&self) -> Result<Vec<UserRow>, HistoryError> {
        match &self.inner {
            Inner::Disabled => Ok(vec![]),
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::list_users(pool).await,
        }
    }

    pub async fn delete_user(&self, id: i64) -> Result<(), HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = id;
                Err(HistoryError::Disabled)
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::delete_user(pool, id).await,
        }
    }

    // ── PII detections ────────────────────────────────────────────────────────

    pub async fn record_pii_detections(
        &self,
        rows: &[PiiDetectionRow],
    ) -> Result<(), HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = rows;
                Err(HistoryError::Disabled)
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::record_pii_detections(pool, rows).await,
        }
    }

    pub async fn pii_by_kind(
        &self,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<DimCount>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = (since_ms, until_ms);
                Ok(vec![])
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::pii_by_kind(pool, since_ms, until_ms).await,
        }
    }

    pub async fn pii_timeseries(
        &self,
        since_ms: i64,
        until_ms: i64,
        bucket: Bucket,
    ) -> Result<Vec<UsageBucket>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = (since_ms, until_ms, bucket);
                Ok(vec![])
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::pii_timeseries(pool, since_ms, until_ms, bucket).await,
        }
    }

    // ── Webhook deliveries ────────────────────────────────────────────────────

    pub async fn record_webhook_delivery(
        &self,
        row: &WebhookDeliveryRow,
    ) -> Result<i64, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = row;
                Err(HistoryError::Disabled)
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::record_webhook_delivery(pool, row).await,
        }
    }

    pub async fn recent_webhook_deliveries(
        &self,
        limit: u32,
    ) -> Result<Vec<WebhookDeliveryRow>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = limit;
                Ok(vec![])
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::recent_webhook_deliveries(pool, limit).await,
        }
    }

    pub async fn get_webhook_delivery(
        &self,
        id: i64,
    ) -> Result<Option<WebhookDeliveryRow>, HistoryError> {
        match &self.inner {
            Inner::Disabled => {
                let _ = id;
                Ok(None)
            }
            #[cfg(feature = "postgres")]
            Inner::Pg(pool) => crate::pg::get_webhook_delivery(pool, id).await,
        }
    }
}
