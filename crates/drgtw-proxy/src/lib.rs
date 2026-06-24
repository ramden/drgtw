//! HTTP proxy core: routes, upstream dispatch, SSE streaming.
//!
//! Public API contract (Phase 1 / WP 1.2 + 1.3; Phase 2 / WP 2.1; Phase 3 / WP 3.4).
//! Frozen interface — extend, don't break.

use std::sync::Arc;

use std::collections::HashMap;

use arc_swap::ArcSwap;
use axum::routing::{get, post};
use drgtw_config::Config;
use drgtw_events::{EventSink, ModelCost, UsageEvent};
use drgtw_keys::{BudgetTracker, BudgetSnapshot, KeyStore, RateLimiter, RateLimiterSnapshot};
use drgtw_guardrails::GuardrailEngine;
use drgtw_pii::{EntityStore, PiiEngine};

mod converse;
mod error;
mod eventstream;
mod handlers;
mod mcp;
pub mod otel_enrich;
pub mod reload;
mod sigv4;
mod sse_restore;
mod state;
mod upstream;
mod usage_tap;

/// Hot-swappable live config bundle.
///
/// All fields here are rebuilt atomically on each hot-reload via [`ArcSwap`].
/// Infra fields that must NOT be rebuilt on reload (open DB handles, broadcast
/// channels, HTTP client pools) stay on [`ProxyState`] instead.
pub struct Live {
    /// Fully-validated, env-resolved config snapshot for this live generation.
    pub config: Arc<Config>,
    /// Virtual-key authentication and routing store.
    pub keys: KeyStore,
    /// Gateway-level rate limiter (WP 2.1).
    pub limiter: RateLimiter,
    /// Per-virtual-key spend budget tracker (WP 8.1 / 8.3).
    pub budget: BudgetTracker,
    /// PII engine (WP 3.4). Rebuilt when `pii` config changes.
    pub pii: Arc<PiiEngine>,
    /// Content-guardrail engine (v0.0.8). `Some` when `[guardrails]` has at
    /// least one rule; `None` disables the guardrail hooks (a cheap branch on
    /// the hot path). Rebuilt when `guardrails` config changes. Shares the PII
    /// engine `Arc` above for `contact_info` guardrails.
    pub guardrails: Option<Arc<GuardrailEngine>>,
    /// MCP gateway (WP-C). Rebuilt when `mcp_servers` config changes.
    pub mcp: Arc<drgtw_mcp::McpGateway>,
    /// Per-connection cost tables, pre-converted to `drgtw_events::ModelCost`
    /// and keyed by connection name. Rebuilt on each reload so the hot path
    /// never re-converts config.
    pub cost_tables: HashMap<String, HashMap<String, ModelCost>>,
}

/// Shared application state for all handlers.
///
/// Infra fields that survive hot-reload live here. Per-config fields live in
/// [`Live`] accessed via `self.live.load()`.
pub struct ProxyState {
    /// Streaming-safe HTTP client. Never rebuilt — pool continuity matters.
    pub client: reqwest::Client,
    /// Optional persistent entity store (WP 9.3). `Some` when `config.pii.vault`
    /// is configured — an opened, encrypted SQLite vault wrapped as an
    /// [`EntityStore`]. When `None`, placeholder mappings are per-request only
    /// (behaviour identical to pre-WP-9.3).
    pub entity_store: Option<Arc<dyn EntityStore>>,
    /// Usage-event sink (WP 8.3). `Some` when `config.events` is set.
    /// Not rebuilt on reload — the worker task and channel outlive config churn.
    pub events: Option<Arc<EventSink>>,
    /// Filesystem request tracer. `Some` when `config.tracing.enabled`; the dir
    /// is resolved against the config base dir at build time. `None` disables
    /// tracing entirely — every emit site is a cheap `Option` check.
    pub trace: Option<drgtw_trace::TraceWriter>,
    /// OpenTelemetry metric instruments (0.0.2). `Some` only when `[otel]` is
    /// enabled with `metrics = true`; recording is gated on this `Option` so the
    /// disabled path is a cheap branch.
    pub metrics: Option<Arc<drgtw_otel::Metrics>>,
    /// Live broadcast of every [`UsageEvent`] (capacity 1024). Always present.
    /// Subscribers created via [`ProxyState::subscribe_usage`]. A send with no
    /// receivers returns `Err` — that is normal and is always ignored.
    pub usage_broadcast: tokio::sync::broadcast::Sender<UsageEvent>,
    /// Optional persistent usage-history store (admin UI, WP-B). `Some` only when
    /// the binary connected to Postgres at boot. Recording is fire-and-forget:
    /// each emit site spawns a detached task so the response path never blocks on
    /// the database. `None` (the default, and tests / `--validate-config`) is a
    /// cheap branch that records nothing.
    pub history: Option<Arc<drgtw_history::History>>,
    /// Hot-swappable live config + key/limiter/budget/pii/mcp bundle.
    ///
    /// Handlers load a snapshot at the top of each request:
    /// ```rust,ignore
    /// let live = state.live.load();
    /// // live.keys, live.config, live.limiter, live.budget, ...
    /// ```
    pub live: Arc<ArcSwap<Live>>,
}

impl std::fmt::Debug for ProxyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyState")
            .field("live.config", &"<Arc<ArcSwap<Live>>>")
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

    /// Attach OpenTelemetry metric instruments, consuming and returning `self`.
    ///
    /// Builder-style so the binary can wire metrics after constructing state
    /// without changing [`ProxyState::new`]'s signature (tests and the
    /// `--validate-config` path build state without metrics).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Option<Arc<drgtw_otel::Metrics>>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Attach a connected history store, consuming and returning `self`.
    ///
    /// Builder-style, mirroring [`with_metrics`](Self::with_metrics): the binary
    /// wires the store after the boot connect succeeds, without changing
    /// [`ProxyState::new`]'s signature. When set, each usage emit site spawns a
    /// detached `record_usage` task (fire-and-forget — never blocks the response).
    ///
    /// When an [`EventSink`] is already configured, this also rebuilds it with
    /// the history as the `delivery_log` so every webhook POST attempt is
    /// persisted to `webhook_deliveries`. The old sink (delivery_log = None) is
    /// dropped; the new one is spawned inside the current Tokio runtime.
    #[must_use]
    pub fn with_history(mut self, history: Arc<drgtw_history::History>) -> Self {
        // Rebuild the event sink with a delivery log backed by this history,
        // if both an events config and a runtime are available.
        if self.events.is_some() {
            let config = self.live.load();
            if let Some(ev) = &config.config.events {
                let delivery_log: Option<Arc<dyn drgtw_events::sink::DeliveryLog>> =
                    Some(Arc::clone(&history) as Arc<dyn drgtw_events::sink::DeliveryLog>);
                self.events = Some(Arc::new(EventSink::new(
                    ev.url.clone(),
                    ev.auth_bearer.clone(),
                    ev.buffer_size,
                    ev.timeout_ms,
                    ev.signing_secret.clone(),
                    delivery_log,
                )));
            }
        }
        self.history = Some(history);
        self
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

    /// Subscribe to the live usage-event broadcast.
    ///
    /// Each call returns an independent [`tokio::sync::broadcast::Receiver`]
    /// that receives every [`UsageEvent`] emitted after the subscription is
    /// created. Lagging receivers that fall behind the channel capacity (1024)
    /// receive [`tokio::sync::broadcast::error::RecvError::Lagged`] — it is
    /// the caller's responsibility to handle or discard lagged events.
    pub fn subscribe_usage(&self) -> tokio::sync::broadcast::Receiver<UsageEvent> {
        self.usage_broadcast.subscribe()
    }

    /// Build a [`Reloader`] handle that can apply config edits to this state.
    ///
    /// `config_path` is the TOML file on disk; `base_dir` is used to resolve
    /// relative paths (NER model dir, vault path, trace dir).
    pub fn reloader(
        &self,
        config_path: std::path::PathBuf,
        base_dir: std::path::PathBuf,
    ) -> reload::Reloader {
        reload::Reloader::new(Arc::clone(&self.live), config_path, base_dir)
    }

    /// Return a snapshot of the rate-limit state for a key, for UI display.
    ///
    /// Delegates to the current live [`RateLimiter`]. Returns `None` when the
    /// key has no rate limit or the key_id is unknown.
    pub fn rate_limit_snapshot(&self, key_id: &str) -> Option<RateLimiterSnapshot> {
        self.live.load().limiter.snapshot(key_id)
    }

    /// Return a snapshot of the budget state for a key, for UI display.
    ///
    /// Delegates to the current live [`BudgetTracker`]. Returns `None` when the
    /// key has no budget or the key_id is unknown.
    pub fn budget_snapshot(&self, key_id: &str) -> Option<BudgetSnapshot> {
        self.live.load().budget.snapshot(key_id)
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
