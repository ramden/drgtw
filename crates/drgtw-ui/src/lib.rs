//! Embedded admin web UI (concept). Mounts under `/ui`.
//!
//! The basic tier runs with zero persistence: dashboard, read-only config
//! viewer, and locked history/audit pages. Reactivity (the live uptime ticker)
//! is driven by Datastar over a Server-Sent Events stream at `/ui/events`.
//!
//! State is a [`UiState`]: a process start [`Instant`], the shared [`Config`],
//! and the path to the TOML config file (for the editable config page).
//! The binary merges [`router`] only when `config.ui.enabled`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use axum::Router;
use axum::extract::{Query, State};
use axum::middleware;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
use axum::routing::{get, post};
use datastar::prelude::PatchSignals;
use drgtw_config::Config;
use drgtw_history::{Bucket, History};
use drgtw_keys::{BudgetSnapshot, RateLimiterSnapshot};
use drgtw_proxy::reload::Reloader;
use futures::Stream;
use serde::Deserialize;

pub mod auth;
mod assets;
pub mod csrf;
mod layout;
mod mask;
mod pages;

pub use pages::setup_page;

/// Mask a Postgres connection string for display (redacts any embedded
/// password, keeps `${ENV_VAR}` placeholders intact). Used by the binary to
/// build the [`PgGate::Unreachable`] message without leaking credentials.
#[must_use]
pub fn mask_pg_url(url: &str) -> String {
    mask::mask_url(url)
}

/// Outcome of the boot-time attempt to reach the Postgres history store.
///
/// Built once by the binary at startup (the only place that can `await`
/// [`History::connect`]) and threaded into [`UiState`]. The UI router branches
/// on it: [`PgGate::Connected`] unlocks the full product; every other variant
/// locks the UI to a single setup page that explains how to get connected.
///
/// `Clone` is cheap — the connected handle is behind an [`Arc`].
#[derive(Clone)]
pub enum PgGate {
    /// A live, migrated history store. The UI renders normally.
    Connected(Arc<History>),
    /// `[ui.history]` is absent from the config — nothing to connect to.
    NotConfigured,
    /// `[ui.history]` is set but the database could not be reached (connect
    /// error or timeout). `masked_url` has any password redacted for display.
    Unreachable { masked_url: String },
    /// The binary was built without the `ui`/`postgres` feature, so no live
    /// store can exist regardless of config.
    FeatureOff,
}

impl PgGate {
    /// The connected history handle, if any. `Some` only for [`PgGate::Connected`].
    #[must_use]
    pub fn history(&self) -> Option<Arc<History>> {
        match self {
            PgGate::Connected(h) => Some(Arc::clone(h)),
            _ => None,
        }
    }

    /// Whether the UI is unlocked (a live store is connected).
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(self, PgGate::Connected(_))
    }
}

/// Shared UI state. Cheap to clone — all fields are handles or cheap to copy.
#[derive(Clone)]
pub struct UiState {
    /// Process start, used to derive live uptime for the dashboard.
    pub start: Instant,
    /// Loaded, validated gateway configuration (as seen at boot).
    pub config: Arc<Config>,
    /// Absolute path to the TOML config file — read/written by the config editor.
    pub config_path: PathBuf,
    /// Boot-time history-store gate. Drives whether the router serves the full
    /// product or locks to the setup page.
    pub gate: PgGate,
    /// Live config hot-reload handle. `Some` when the binary wired it (always, in
    /// a real gateway run); `None` in standalone UI tests. Config mutations
    /// (keys CRUD, MCP servers, webhook secret) call [`Reloader::apply`] so they
    /// take effect without restarting the process, and the Rate Limits / Budgets
    /// / key-detail pages read live counters via [`Reloader::current`].
    pub reloader: Option<Reloader>,
}

impl UiState {
    /// Build UI state from the gateway start time, shared config, config path,
    /// and the boot-time history-store [`PgGate`]. The hot-reload handle is
    /// attached separately via [`UiState::with_reloader`] (the binary has the
    /// proxy state needed to build it; tests can omit it).
    pub fn new(start: Instant, config: Arc<Config>, config_path: PathBuf, gate: PgGate) -> Self {
        Self { start, config, config_path, gate, reloader: None }
    }

    /// Attach the live config hot-reload handle, consuming and returning `self`.
    /// Builder-style so [`UiState::new`]'s signature (and its test callers) stay
    /// unchanged.
    #[must_use]
    pub fn with_reloader(mut self, reloader: Reloader) -> Self {
        self.reloader = Some(reloader);
        self
    }

    /// The connected history handle, if the gate is [`PgGate::Connected`].
    #[must_use]
    pub fn history(&self) -> Option<Arc<History>> {
        self.gate.history()
    }

    /// The CURRENT gateway config — reflects live hot-reloads. After a mutation
    /// goes through [`Reloader::apply`], the proxy's `Live` holds the new config;
    /// `self.config` is only the boot snapshot. Pages that display config which
    /// can change at runtime (keys, limits, budgets, MCP, webhooks) must read
    /// through here so a freshly created/edited key shows immediately without a
    /// restart. Falls back to the boot config when no reloader is wired (tests).
    #[must_use]
    pub fn live_config(&self) -> Arc<Config> {
        match &self.reloader {
            Some(r) => Arc::clone(&r.current().config),
            None => Arc::clone(&self.config),
        }
    }

    /// Live rate-limit bucket snapshot for a `vk-{i}` key id, if a reloader is
    /// wired and the key has a rate limit configured. Reads the current [`Live`]
    /// limiter without blocking the request path.
    #[must_use]
    pub fn rate_limit_snapshot(&self, key_id: &str) -> Option<RateLimiterSnapshot> {
        self.reloader.as_ref()?.current().limiter.snapshot(key_id)
    }

    /// Live budget window snapshot for a `vk-{i}` key id, if a reloader is wired
    /// and the key has a budget configured.
    #[must_use]
    pub fn budget_snapshot(&self, key_id: &str) -> Option<BudgetSnapshot> {
        self.reloader.as_ref()?.current().budget.snapshot(key_id)
    }
}

/// Build the UI router, mounted by the binary under `/ui`.
///
/// Full product IA. Three states per nav entry: ● live (real config data),
/// ◐ coming soon (polished empty state), 🔒 requires PostgreSQL (`[ui.history]`).
///
/// Routes (all under the `/ui` mount point):
/// - Operate (●): `/` dashboard, `/config`, `/connections`, `/keys`
/// - Observe: `/analytics` 🔒, `/traces` 🔒, `/pii` ◐, `/audit` 🔒
/// - Govern (◐): `/budgets`, `/limits`, `/mcp`, `/webhooks`
/// - Admin: `/team` ◐, `/settings` ●
/// - `GET /events`   — Datastar SSE stream patching the `uptime` signal
/// - `GET /assets/*` — vendored fonts, Chart.js, Basecoat, Datastar
/// - `POST /config/save` — save a config section form (editable config page)
/// - `GET /login`    — login page (always public)
/// - `POST /login`   — verify credentials, set session cookie
/// - `POST /logout`  — clear cookie, redirect to login
///
/// When `[ui.auth]` is configured the `auth_layer` middleware gates all routes
/// except `/login` and `/assets/*`. Open mode (`[ui.auth]` absent) is a no-op
/// pass-through — existing behaviour is preserved.
pub fn router(state: UiState) -> Router {
    macro_rules! page {
        ($f:path) => {
            get(|State(s): State<UiState>| async move { Html($f(&s).into_string()) })
        };
    }

    // Locked mode: no live history store. Serve the setup page for every path
    // (so any deep link lands on the explanation) EXCEPT `/assets/*`, which must
    // keep serving CSS/JS so the setup page is styled. Login/config/dashboard
    // are NOT mounted — the UI is inert until Postgres is reachable.
    //
    // `/` is routed explicitly in addition to the catch-all `fallback`: under a
    // `nest("/ui", …)` mount the index path (`/ui` and `/ui/`) maps to the inner
    // `/`, which axum does not always route through a nested router's fallback.
    if !state.gate.is_connected() {
        let setup = get(|State(s): State<UiState>| async move {
            Html(pages::setup_page(&s).into_string())
        });
        return Router::new()
            .route("/", setup.clone())
            .route("/assets/{*path}", get(assets::serve))
            .fallback(setup)
            .with_state(state);
    }

    Router::new()
        // Auth routes (always public — middleware exempts /login and /assets/*).
        .route("/login", get(auth::get_login).post(auth::post_login))
        .route("/logout", post(auth::post_logout))
        // Operate (live)
        .route("/", get(pages::dashboard))
        .route("/config", page!(pages::config_view))
        .route("/config/save", post(pages::config_save))
        .route("/connections", page!(pages::connections))
        // Virtual keys: list (sync), per-key detail (async, reads history), CRUD
        // (POST → Reloader::apply, hot-reload).
        .route("/keys", page!(pages::virtual_keys))
        .route("/keys/new", post(pages::keys_create))
        .route("/keys/{idx}", get(pages::key_detail))
        .route("/keys/{idx}/edit", post(pages::keys_update))
        .route("/keys/{idx}/delete", post(pages::keys_delete))
        // Observe (Postgres-backed, async DB queries)
        .route("/analytics", get(pages::analytics))
        .route("/traces", get(pages::traces))
        .route("/pii", get(pages::pii_insights))
        .route("/audit", get(pages::audit_log))
        // JSON API (feeds the dashboard + analytics range toggles)
        .route("/api/timeseries", get(api_timeseries))
        // Govern
        .route("/budgets", get(pages::cost_budgets))
        .route("/limits", page!(pages::rate_limits))
        .route("/mcp", get(pages::mcp_servers))
        .route("/mcp/save", post(pages::mcp_save))
        .route("/mcp/{name}/delete", post(pages::mcp_delete))
        .route("/webhooks", get(pages::webhooks))
        .route("/webhooks/{id}/replay", post(pages::webhooks_replay))
        .route("/webhooks/rotate-secret", post(pages::webhooks_rotate))
        // Admin
        .route("/team", get(pages::team_access).post(pages::team_create))
        .route("/team/{id}/delete", post(pages::team_delete))
        .route("/settings", page!(pages::settings))
        // Infra
        .route("/events", get(events))
        .route("/assets/{*path}", get(assets::serve))
        // Auth middleware: runs after routing, before handlers.
        // The middleware itself exempts /login and /assets/*.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::auth_layer,
        ))
        .with_state(state)
}

/// SSE stream: patch the `uptime` signal once per second.
///
/// Proves end-to-end reactivity — the dashboard's uptime card re-renders on the
/// client with no polling. Each tick is a Datastar `PatchSignals` event carrying
/// `{"uptime": "<human-readable>"}`.
async fn events(
    State(state): State<UiState>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let start = state.start;
    let interval = tokio::time::interval(Duration::from_secs(1));
    let stream = futures::stream::unfold(interval, move |mut interval| async move {
        interval.tick().await;
        let signals = format!("{{\"uptime\": \"{}\"}}", humanize(start.elapsed().as_secs()));
        let event = Event::from(PatchSignals::new(signals));
        Some((Ok(event), interval))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Render a second count as a compact human string (e.g. `1h 02m 03s`).
fn humanize(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Current epoch time in milliseconds.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Map a `range` query string to a `(since_ms, until_ms, Bucket)` window.
///
/// `24h` → last 24h bucketed by hour; `7d` → last 7d by day; `30d` (and any
/// unrecognised value) → last 30d by day.
pub(crate) fn range_window(range: &str) -> (i64, i64, Bucket) {
    let now = now_ms();
    const DAY: i64 = 86_400_000;
    match range {
        "24h" => (now - DAY, now, Bucket::Hour),
        "7d" => (now - 7 * DAY, now, Bucket::Day),
        _ => (now - 30 * DAY, now, Bucket::Day),
    }
}

/// Query string for the timeseries JSON API.
#[derive(Deserialize)]
pub(crate) struct RangeQuery {
    #[serde(default)]
    range: Option<String>,
}

/// `GET /ui/api/timeseries?range=24h|7d|30d` — JSON arrays for chart rebuilds.
///
/// Returns parallel arrays keyed by bucket. On a missing history handle or a
/// query error, returns empty arrays (never 500). Used by the dashboard and
/// analytics range toggles to rebuild Chart.js without a full page reload.
async fn api_timeseries(
    State(state): State<UiState>,
    Query(q): Query<RangeQuery>,
) -> axum::Json<serde_json::Value> {
    let range = q.range.as_deref().unwrap_or("24h");
    let (since, until, bucket) = range_window(range);
    let buckets = match state.history() {
        Some(h) => h.usage_timeseries(since, until, bucket).await.unwrap_or_default(),
        None => Vec::new(),
    };
    axum::Json(pages::timeseries_json(&buckets, bucket))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_formats() {
        assert_eq!(humanize(0), "0s");
        assert_eq!(humanize(45), "45s");
        assert_eq!(humanize(125), "2m 05s");
        assert_eq!(humanize(3723), "1h 02m 03s");
    }
}
