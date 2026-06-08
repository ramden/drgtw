use thiserror::Error;

#[derive(Debug, Error)]
pub enum HistoryError {
    /// The history store is disabled (compiled without `postgres` feature, or
    /// constructed via `History::disabled()`).
    #[error("history store is disabled")]
    Disabled,

    /// A database-level error (only reachable with `postgres` feature ON).
    #[cfg(feature = "postgres")]
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    /// Migration failed.
    #[cfg(feature = "postgres")]
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// JSON serialisation error (metadata column).
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
