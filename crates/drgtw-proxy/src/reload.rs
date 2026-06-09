//! Hot-reload seam: validate → write → swap the live bundle atomically.
//!
//! [`Reloader`] holds a weak reference to the shared [`ArcSwap<Live>`] and can
//! apply a new TOML document in-place. The swap is atomic — handlers that have
//! already called `state.live.load()` finish with the old config; new requests
//! pick up the new one. Rate-limit and budget counters for keys whose **secret**
//! is unchanged are preserved via `rebuild_from`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use drgtw_config::{Config, McpAuthType, McpServerConfig};
use drgtw_events::ModelCost;
use drgtw_keys::{BudgetTracker, KeyStore, RateLimiter};
use drgtw_mcp::{McpGateway, UpstreamServer};
use drgtw_pii::build_engine_with_ner;

use crate::Live;

/// Cloneable handle for applying config hot-reloads.
///
/// Created via [`ProxyState::reloader`]. Pass it to the UI router so the
/// config-editor page can call [`Reloader::apply`].
#[derive(Clone)]
pub struct Reloader {
    live: Arc<ArcSwap<Live>>,
    config_path: PathBuf,
    base_dir: PathBuf,
}

impl Reloader {
    pub(crate) fn new(
        live: Arc<ArcSwap<Live>>,
        config_path: PathBuf,
        base_dir: PathBuf,
    ) -> Self {
        Reloader { live, config_path, base_dir }
    }

    /// Apply a new TOML document as the live config.
    ///
    /// Steps:
    /// 1. Validate via [`drgtw_config::validate_str`] — rejects invalid TOML.
    /// 2. Write atomically to disk via [`drgtw_config::write_safe`].
    /// 3. Parse the new config (env-var resolution runs again).
    /// 4. Build a new [`Live`] bundle: fresh keys/pii/mcp/cost_tables, counters
    ///    preserved for unchanged secrets via `rebuild_from`.
    /// 5. Swap via [`ArcSwap::store`] — all future `load()` calls see the new bundle.
    ///
    /// # Errors
    /// Returns a `String` error if validation, write, parse, or PII engine build
    /// fails. The live config is unchanged on any error.
    pub fn apply(&self, new_doc_toml: &str) -> Result<(), String> {
        // 1. Validate.
        drgtw_config::validate_str(new_doc_toml).map_err(|errors| {
            let msgs: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
            format!("config validation failed: {}", msgs.join("; "))
        })?;

        // 2. Write safely (temp + fsync + rename).
        drgtw_config::write_safe(&self.config_path, new_doc_toml)
            .map_err(|e| format!("config write failed: {e}"))?;

        // 3. Parse (re-resolves env vars).
        let new_config =
            drgtw_config::load(&self.config_path).map_err(|e| format!("config load failed: {e}"))?;
        let new_config = Arc::new(new_config);

        // 4. Build the new live bundle, preserving live counters.
        let old_live = self.live.load();
        let new_live = build_live(
            Arc::clone(&new_config),
            &self.base_dir,
            Some(&old_live),
        )
        .map_err(|e| format!("live bundle build failed: {e}"))?;

        // 5. Atomic swap.
        self.live.store(Arc::new(new_live));
        Ok(())
    }

    /// Return the current live bundle (for UI snapshot reads).
    pub fn current(&self) -> arc_swap::Guard<Arc<Live>> {
        self.live.load()
    }
}

/// Build a [`Live`] bundle from `config`.
///
/// When `old` is `Some`, rate-limit and budget counters for keys whose secret
/// is unchanged are preserved via `rebuild_from`. Pass `None` at boot time.
pub(crate) fn build_live(
    config: Arc<Config>,
    base_dir: &std::path::Path,
    old: Option<&Live>,
) -> Result<Live, drgtw_pii::EngineBuildError> {
    let keys = KeyStore::new(&config);

    let (limiter, budget) = match old {
        Some(prev) => {
            let limiter = prev.limiter.rebuild_from(&prev.config, &config);
            let budget = prev.budget.rebuild_from(&prev.config, &config);
            (limiter, budget)
        }
        None => (RateLimiter::new(&config), BudgetTracker::new(&config)),
    };

    let pii = Arc::new(build_engine_with_ner(&config.pii, base_dir)?);

    // Build the shared reqwest client for MCP upstreams. At reload we build a
    // fresh client — the old one is dropped when the old Live is dropped.
    let mcp_client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("failed to build reqwest client for MCP");

    let mcp_servers: Vec<UpstreamServer> = config
        .mcp_servers
        .iter()
        .map(|(name, cfg)| build_upstream_server(name, cfg))
        .collect();
    let mcp = Arc::new(McpGateway::new(mcp_servers, mcp_client));

    let cost_tables = config
        .connections
        .iter()
        .map(|conn| {
            let table: HashMap<String, ModelCost> = conn
                .model_costs
                .iter()
                .map(|(model, mc)| {
                    (
                        model.clone(),
                        ModelCost {
                            input_per_1m: mc.input_per_1m,
                            output_per_1m: mc.output_per_1m,
                        },
                    )
                })
                .collect();
            (conn.name.clone(), table)
        })
        .collect();

    Ok(Live { config, keys, limiter, budget, pii, mcp, cost_tables })
}

/// Translate a configured MCP server into an [`UpstreamServer`].
fn build_upstream_server(name: &str, cfg: &McpServerConfig) -> UpstreamServer {
    let mut headers: Vec<(String, String)> = Vec::new();

    match cfg.auth_type {
        McpAuthType::None => {}
        McpAuthType::ApiKey => {
            if let Some(value) = &cfg.auth_value {
                headers.push(("X-API-Key".to_owned(), value.clone()));
            }
        }
        McpAuthType::Bearer => {
            if let Some(value) = &cfg.auth_value {
                headers.push(("Authorization".to_owned(), format!("Bearer {value}")));
            }
        }
    }

    for (header, value) in &cfg.extra_headers {
        headers.push((header.clone(), value.clone()));
    }

    // Normalise forward_headers to lowercase for O(1) lookup at request time.
    let forward_headers = cfg
        .forward_headers
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();

    UpstreamServer {
        name: name.to_owned(),
        url: cfg.url.clone(),
        headers,
        forward_headers,
    }
}
