//! Server bootstrap: builds the full axum router, wires the proxy, applies
//! request-ID middleware, and drives the graceful-shutdown lifecycle.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::{Router, routing::get};
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tracing::info;
#[cfg(feature = "ui")]
use tracing::warn;

use drgtw_config::Config;
use drgtw_pii::EngineBuildError;
use drgtw_proxy::ProxyState;
use drgtw_ui::{PgGate, UiState};

use crate::middleware::request_id::RequestIdLayer;
use crate::routes;

/// Attempt the boot-time Postgres connect for the admin-UI history store.
///
/// Returns the [`PgGate`] describing the outcome — never errors out of boot:
/// an unreachable database leaves the gateway proxying normally with the UI
/// locked to the setup page.
///
/// Variants:
/// - feature off → [`PgGate::FeatureOff`] (compiled without `ui`/`postgres`).
/// - no `[ui.history]` → [`PgGate::NotConfigured`].
/// - connect ok within 5s → [`PgGate::Connected`].
/// - connect error or timeout → [`PgGate::Unreachable`] (URL masked for display).
async fn build_pg_gate(config: &Config) -> PgGate {
    #[cfg(feature = "ui")]
    {
        use std::time::Duration;
        match &config.ui.history {
            None => PgGate::NotConfigured,
            Some(h) => {
                let connect = drgtw_history::History::connect(&h.postgres_url);
                match tokio::time::timeout(Duration::from_secs(5), connect).await {
                    Ok(Ok(history)) => PgGate::Connected(Arc::new(history)),
                    Ok(Err(e)) => {
                        warn!(error = %e, "history store unreachable — UI locked to setup page");
                        PgGate::Unreachable { masked_url: drgtw_ui::mask_pg_url(&h.postgres_url) }
                    }
                    Err(_timeout) => {
                        warn!("history store connect timed out — UI locked to setup page");
                        PgGate::Unreachable { masked_url: drgtw_ui::mask_pg_url(&h.postgres_url) }
                    }
                }
            }
        }
    }
    #[cfg(not(feature = "ui"))]
    {
        let _ = config;
        PgGate::FeatureOff
    }
}

/// Build the full application router.
///
/// Merges the proxy routes (`POST /v1/chat/completions`, `GET /v1/models`)
/// with the binary-owned `/health` route and wraps everything in the
/// request-ID middleware.
///
/// `config_path` is threaded into `UiState` for the editable config page.
/// Pass an empty `PathBuf` when the path is not known (e.g. unit tests that
/// exercise pages other than the config editor).
///
/// # Errors
/// Returns an error when a custom PII recognizer regex in the config is
/// invalid.  Invalid regex must fail boot, not silently degrade at request
/// time.
pub fn router(
    config: Arc<Config>,
    base_dir: &std::path::Path,
    config_path: std::path::PathBuf,
) -> Result<Router, EngineBuildError> {
    // This sync entry point cannot `await`, so it derives the gate from config
    // alone (no live connect): `[ui.history]` present → a connected gate backed
    // by a no-op disabled handle (unlocks the full UI for rendering/tests
    // without standing up Postgres); absent → NotConfigured (locked setup page).
    // The async `run()` path below does the real connect and overrides this.
    let gate = sync_gate_from_config(&config);
    router_with_gate(config, base_dir, config_path, gate)
}

/// Build the full router with an explicit [`PgGate`].
///
/// The async `run()` path constructs the gate via a real boot connect; the sync
/// `router()` derives it from config. Both funnel through here so the UI and the
/// proxy share the same connected history handle.
pub fn router_with_gate(
    config: Arc<Config>,
    base_dir: &std::path::Path,
    config_path: std::path::PathBuf,
    gate: PgGate,
) -> Result<Router, EngineBuildError> {
    let mut proxy_state = ProxyState::new(Arc::clone(&config), base_dir)?;
    if let Some(history) = gate.history() {
        proxy_state = proxy_state.with_history(history);
    }
    let state = Arc::new(proxy_state);

    // Hot-reload handle for the UI: lets config mutations apply live and the UI
    // read live rate-limit/budget counters. Built from the shared proxy state so
    // both point at the same `ArcSwap<Live>`.
    let reloader = state.reloader(config_path.clone(), base_dir.to_path_buf());

    let proxy_routes = drgtw_proxy::router(state);
    let health_route = Router::new().route("/health", get(routes::health::handle));

    let mut app = Router::new().merge(proxy_routes).merge(health_route);

    // Mount the admin UI under `/ui` only when enabled. The concept exposes no
    // auth — see the run() construction site for the TODO gating non-localhost.
    if config.ui.enabled {
        app = app.nest(
            "/ui",
            drgtw_ui::router(
                UiState::new(Instant::now(), config, config_path, gate).with_reloader(reloader),
            ),
        );
    }

    Ok(app.layer(ServiceBuilder::new().layer(RequestIdLayer)))
}

/// Derive a [`PgGate`] from config without connecting (sync `router()` path).
///
/// `[ui.history]` present → `Connected` backed by a disabled no-op handle, so
/// the full UI renders and existing page-level tests (which key the unlocked
/// state on `config.ui.history.is_some()`) keep passing without a live database.
/// Absent → `NotConfigured`.
fn sync_gate_from_config(config: &Config) -> PgGate {
    if config.ui.history.is_some() {
        PgGate::Connected(Arc::new(drgtw_history::History::disabled()))
    } else {
        PgGate::NotConfigured
    }
}

/// Bind, serve, and gracefully shut down the gateway.
///
/// `base_dir` is the directory relative model paths resolve against —
/// conventionally the config file's parent directory.
/// `config_path` is the absolute path to the TOML file used to bootstrap the
/// gateway — threaded into `UiState` so the editable config page can read and
/// write it via the drgtw-config safe-edit API.
pub async fn run(
    config: Config,
    base_dir: &std::path::Path,
    config_path: std::path::PathBuf,
    otel_guard: Option<drgtw_otel::OtelGuard>,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = config.server.bind_addr;

    info!(
        bind_addr = %addr,
        connections = config.connections.len(),
        virtual_keys = config.virtual_keys.len(),
        ner = config.pii.ner.is_some(),
        otel = otel_guard.is_some(),
        "starting gateway"
    );

    // Build proxy state, attaching OTel metric instruments (if any) before
    // wrapping it for the router. Spans are exported via the global subscriber
    // layer, so only metrics need to live in state.
    let metrics = otel_guard.as_ref().and_then(|g| g.metrics.clone());
    let config = Arc::new(config);
    let ui_enabled = config.ui.enabled;
    // Clone the shared config for the UI before it moves into proxy state.
    let ui_config = Arc::clone(&config);

    // Boot-time history-store connect (admin UI, WP-B). Done once here — the
    // only place that can `await` — and the resulting handle is shared into both
    // the proxy state (fire-and-forget usage recording) and the UI (gate). An
    // unreachable database never fails boot: the gateway proxies normally and
    // the UI locks to the setup page.
    let gate = if ui_enabled {
        build_pg_gate(&config).await
    } else {
        PgGate::NotConfigured
    };

    let mut proxy_state = ProxyState::new(config, base_dir)?.with_metrics(metrics);
    if let Some(history) = gate.history() {
        proxy_state = proxy_state.with_history(history);
    }
    let state = Arc::new(proxy_state);

    // Hold a trace-writer handle so we can flush JSONL on graceful shutdown.
    let trace_handle = state.trace_handle();

    // Hot-reload handle for the UI (live config apply + live counter reads),
    // built from the shared proxy state before it moves into the proxy router.
    let reloader = state.reloader(config_path.clone(), base_dir.to_path_buf());

    let proxy_routes = drgtw_proxy::router(state);
    let health_route = Router::new().route("/health", get(routes::health::handle));
    let mut app = Router::new().merge(proxy_routes).merge(health_route);

    // Mount the admin UI under `/ui` only when enabled.
    // TODO(ui-auth): admin token + signed cookie session before any non-localhost exposure
    if ui_enabled {
        app = app.nest(
            "/ui",
            drgtw_ui::router(
                UiState::new(Instant::now(), ui_config, config_path, gate).with_reloader(reloader),
            ),
        );
    }

    let app = app.layer(ServiceBuilder::new().layer(RequestIdLayer));

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Shutdown order (per design §5.4): stop serving → flush drgtw-trace JSONL →
    // flush OTel → exit.
    if let Some(trace) = trace_handle {
        trace.shutdown().await;
    }
    if let Some(guard) = otel_guard {
        // The OTel batch processor / periodic reader run on their own threads
        // and drive async exporters; flush on a blocking thread so the runtime
        // reactor stays available to those export futures.
        let _ = tokio::task::spawn_blocking(move || guard.shutdown()).await;
    }

    info!("gateway stopped");
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to listen for SIGINT");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
