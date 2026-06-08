//! `History` handle — identical public surface under both feature states.

use drgtw_events::UsageEvent;

use crate::error::HistoryError;
use crate::types::{AuditEntry, Bucket, UsageBucket, UserRow};

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
}
