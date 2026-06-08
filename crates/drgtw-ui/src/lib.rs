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
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::middleware;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
use axum::routing::{get, post};
use datastar::prelude::PatchSignals;
use drgtw_config::Config;
use futures::Stream;

pub mod auth;
mod assets;
mod layout;
mod mask;
mod pages;

/// Shared UI state. Cheap to clone — all fields are handles or cheap to copy.
#[derive(Clone)]
pub struct UiState {
    /// Process start, used to derive live uptime for the dashboard.
    pub start: Instant,
    /// Loaded, validated gateway configuration (as seen at boot).
    pub config: Arc<Config>,
    /// Absolute path to the TOML config file — read/written by the config editor.
    pub config_path: PathBuf,
}

impl UiState {
    /// Build UI state from the gateway start time, shared config, and config path.
    pub fn new(start: Instant, config: Arc<Config>, config_path: PathBuf) -> Self {
        Self { start, config, config_path }
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

    Router::new()
        // Auth routes (always public — middleware exempts /login and /assets/*).
        .route("/login", get(auth::get_login).post(auth::post_login))
        .route("/logout", post(auth::post_logout))
        // Operate (live)
        .route("/", page!(pages::dashboard))
        .route("/config", page!(pages::config_view))
        .route("/config/save", post(pages::config_save))
        .route("/connections", page!(pages::connections))
        .route("/keys", page!(pages::virtual_keys))
        // Observe
        .route("/analytics", page!(pages::analytics))
        .route("/traces", page!(pages::traces))
        .route("/pii", page!(pages::pii_insights))
        .route("/audit", page!(pages::audit_log))
        // Govern
        .route("/budgets", page!(pages::cost_budgets))
        .route("/limits", page!(pages::rate_limits))
        .route("/mcp", page!(pages::mcp_servers))
        .route("/webhooks", page!(pages::webhooks))
        // Admin
        .route("/team", page!(pages::team_access))
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
