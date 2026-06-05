//! HTTP proxy core: routes, upstream dispatch, SSE streaming.
//!
//! Public API contract (Phase 1 / WP 1.2 + 1.3; Phase 2 / WP 2.1; Phase 3 / WP 3.4).
//! Frozen interface — extend, don't break.

use std::sync::Arc;

use std::collections::HashMap;

use axum::routing::{get, post};
use drgtw_config::Config;
use drgtw_events::{EventSink, ModelCost};
use drgtw_keys::{BudgetTracker, KeyStore, RateLimiter};
use drgtw_pii::{EntityStore, PiiEngine};

mod error;
mod handlers;
mod mcp;
mod sse_restore;
mod state;
mod upstream;
mod usage_tap;

/// Shared application state for all handlers.
pub struct ProxyState {
    pub keys: KeyStore,
    pub client: reqwest::Client,
    pub config: Arc<Config>,
    /// Gateway-level rate limiter (WP 2.1). Built from config at startup.
    pub limiter: RateLimiter,
    /// PII engine (WP 3.4). Built once at startup; shared across requests.
    pub pii: Arc<PiiEngine>,
    /// Optional persistent entity store (WP 9.3). `Some` when `config.pii.vault`
    /// is configured — an opened, encrypted SQLite vault wrapped as an
    /// [`EntityStore`]. When `None`, placeholder mappings are per-request only
    /// (behaviour identical to pre-WP-9.3).
    pub entity_store: Option<Arc<dyn EntityStore>>,
    /// Per-virtual-key spend budget tracker (WP 8.1 / 8.3). Built from config.
    pub budget: BudgetTracker,
    /// Usage-event sink (WP 8.3). `Some` when `config.events` is set.
    pub events: Option<Arc<EventSink>>,
    /// Per-connection cost tables, pre-converted to `drgtw_events::ModelCost`
    /// and keyed by connection name. Built once at startup so the request hot
    /// path never re-converts the config map.
    pub cost_tables: HashMap<String, HashMap<String, ModelCost>>,
    /// MCP gateway (WP-C). Always built — with zero configured upstreams it is
    /// an empty aggregator: `POST /mcp` still serves `initialize`/`ping` and
    /// returns an empty `tools/list`.
    pub mcp: Arc<drgtw_mcp::McpGateway>,
    /// Filesystem request tracer. `Some` when `config.tracing.enabled`; the dir
    /// is resolved against the config base dir at build time. `None` disables
    /// tracing entirely — every emit site is a cheap `Option` check.
    pub trace: Option<drgtw_trace::TraceWriter>,
}

impl std::fmt::Debug for ProxyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyState")
            .field("keys", &self.keys)
            .field("config", &self.config)
            .field("pii", &self.pii)
            .finish_non_exhaustive()
    }
}

impl ProxyState {
    /// Construct proxy state: builds the [`KeyStore`], [`RateLimiter`],
    /// [`PiiEngine`], and a streaming-safe [`reqwest::Client`].
    ///
    /// # Errors
    /// Returns [`drgtw_pii::EngineError`] when a custom recognizer regex is
    /// invalid — this must fail boot, not silently degrade at request time.
    pub fn new(
        config: Arc<Config>,
        base_dir: &std::path::Path,
    ) -> Result<Self, drgtw_pii::EngineBuildError> {
        Self::build(config, base_dir)
    }

    /// Clone the trace-writer handle, if tracing is enabled.
    ///
    /// Intended for deterministic test flushing: a test can hold this clone,
    /// drop the router/state so the state-held sender is released, then call
    /// [`drgtw_trace::TraceWriter::shutdown`] on the clone to await the worker
    /// draining every pending entry to disk.
    pub fn trace_handle(&self) -> Option<drgtw_trace::TraceWriter> {
        self.trace.clone()
    }
}

/// Build the proxy router. The bin merges this with its own routes (/health).
///
/// Routes:
/// - `POST /v1/chat/completions` — OpenAI format, streaming + non-streaming
/// - `POST /v1/messages`         — Anthropic Messages API, streaming + non-streaming (WP 2.1)
/// - `POST /v1/embeddings`       — OpenAI embeddings, non-streaming (WP 9.3)
/// - `GET  /v1/models`           — models visible to the authenticated key
/// - `POST /mcp`                 — MCP gateway (streamable HTTP); `GET`/`DELETE` → 405 (WP-C)
pub fn router(state: Arc<ProxyState>) -> axum::Router {
    axum::Router::new()
        .route("/v1/chat/completions", post(handlers::chat_completions))
        .route("/v1/messages", post(handlers::messages))
        .route("/v1/embeddings", post(handlers::embeddings))
        .route("/v1/models", get(handlers::list_models))
        .route(
            "/mcp",
            post(mcp::handle_post)
                .get(mcp::method_not_allowed)
                .delete(mcp::method_not_allowed),
        )
        .with_state(state)
}
