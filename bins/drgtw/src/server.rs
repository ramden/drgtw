//! Server bootstrap: builds the full axum router, wires the proxy, applies
//! request-ID middleware, and drives the graceful-shutdown lifecycle.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::{Router, routing::get};
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tracing::info;

use drgtw_config::Config;
use drgtw_pii::EngineBuildError;
use drgtw_proxy::ProxyState;
use drgtw_ui::UiState;

use crate::middleware::request_id::RequestIdLayer;
use crate::routes;

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
    let state = Arc::new(ProxyState::new(Arc::clone(&config), base_dir)?);

    let proxy_routes = drgtw_proxy::router(state);
    let health_route = Router::new().route("/health", get(routes::health::handle));

    let mut app = Router::new().merge(proxy_routes).merge(health_route);

    // Mount the admin UI under `/ui` only when enabled. The concept exposes no
    // auth — see the run() construction site for the TODO gating non-localhost.
    if config.ui.enabled {
        app = app.nest("/ui", drgtw_ui::router(UiState::new(Instant::now(), config, config_path)));
    }

    Ok(app.layer(ServiceBuilder::new().layer(RequestIdLayer)))
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
    let state = Arc::new(ProxyState::new(config, base_dir)?.with_metrics(metrics));

    // Hold a trace-writer handle so we can flush JSONL on graceful shutdown.
    let trace_handle = state.trace_handle();

    let proxy_routes = drgtw_proxy::router(state);
    let health_route = Router::new().route("/health", get(routes::health::handle));
    let mut app = Router::new().merge(proxy_routes).merge(health_route);

    // Mount the admin UI under `/ui` only when enabled.
    // TODO(ui-auth): admin token + signed cookie session before any non-localhost exposure
    if ui_enabled {
        app = app.nest("/ui", drgtw_ui::router(UiState::new(Instant::now(), ui_config, config_path)));
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
