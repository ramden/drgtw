//! ProxyState construction.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use drgtw_config::{Config, McpAuthType, McpServerConfig, VaultConfig};
use drgtw_events::{EventSink, ModelCost};
use drgtw_keys::{BudgetTracker, KeyStore, RateLimiter};
use drgtw_mcp::{McpGateway, UpstreamServer};
use drgtw_pii::{EngineBuildError, EntityStore, VaultStore, build_engine_with_ner};
use drgtw_trace::{TraceOptions, TraceWriter};
use drgtw_vault::Vault;

use crate::ProxyState;

impl ProxyState {
    /// Build shared proxy state from validated config.
    ///
    /// - Constructs a [`KeyStore`] from the config.
    /// - Constructs a [`RateLimiter`] from the config (WP 2.1).
    /// - Compiles the [`PiiEngine`] from config (WP 3.4). Returns `Err` if
    ///   any custom recognizer regex is invalid — invalid regexes must fail
    ///   boot rather than silently degrading at request time.
    /// - Builds a [`reqwest::Client`] with:
    ///   - default connection pool
    ///   - 10 s connect timeout
    ///   - NO response-body timeout (streaming responses must not be cut off)
    ///
    /// # Compression note
    /// The reqwest client is built without any compression feature flags
    /// (workspace Cargo.toml uses `default-features = false` for reqwest with
    /// rustls). As a result reqwest sends no `Accept-Encoding` header and
    /// upstream responses arrive as plain bytes — no decompression layer is
    /// needed before PII restore.
    pub fn build(
        config: Arc<Config>,
        base_dir: &Path,
    ) -> Result<Self, drgtw_pii::EngineBuildError> {
        let keys = KeyStore::new(&config);
        let limiter = RateLimiter::new(&config);
        let budget = BudgetTracker::new(&config);
        let pii = Arc::new(build_engine_with_ner(&config.pii, base_dir)?);

        // Persistent entity vault (WP 9.3). Built only when configured; any
        // failure here (bad key length, hex decode, wrong key, unopenable file)
        // fails boot rather than silently degrading to per-request mappings.
        let entity_store: Option<Arc<dyn EntityStore>> = match &config.pii.vault {
            Some(vault_cfg) => Some(open_vault_store(vault_cfg, base_dir)?),
            None => None,
        };

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Intentionally no .timeout() — streaming responses must flow freely.
            .build()
            .expect("failed to build reqwest client");

        // Usage-event sink (WP 8.3): only when configured. `EventSink::new`
        // spawns a Tokio worker, so `build` must be called inside a runtime
        // (it always is — `server::run` and every `#[tokio::test]` provide one).
        let events = config.events.as_ref().map(|ev| {
            Arc::new(EventSink::new(
                ev.url.clone(),
                ev.auth_bearer.clone(),
                ev.buffer_size,
                ev.timeout_ms,
            ))
        });

        // Pre-convert each connection's cost table into the events crate's
        // ModelCost shape, keyed by connection name. Done once here so the
        // request hot path performs no per-request conversion.
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

        // MCP gateway (WP-C). Map each configured upstream into an
        // `UpstreamServer`, pre-computing its static auth + extra headers, and
        // share the same streaming reqwest client. Always built — with zero
        // configured upstreams the gateway is an empty aggregator.
        let mcp_servers: Vec<UpstreamServer> = config
            .mcp_servers
            .iter()
            .map(|(name, cfg)| build_upstream_server(name, cfg))
            .collect();
        let mcp = Arc::new(McpGateway::new(mcp_servers, client.clone()));

        // Filesystem request tracer. Built only when enabled AND a Tokio runtime
        // is available (`TraceWriter::new` spawns a worker task). The serving
        // path always has a runtime; synchronous callers such as
        // `--validate-config` (and its unit tests) build state without one — in
        // that case tracing is silently skipped since no requests will be
        // served anyway. The trace dir resolves against base_dir for relative
        // paths (same convention as the vault / NER model dir).
        let runtime_available = tokio::runtime::Handle::try_current().is_ok();
        let trace = if config.tracing.enabled && runtime_available {
            let dir = {
                let p = Path::new(&config.tracing.dir);
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    base_dir.join(p)
                }
            };
            let opts = TraceOptions {
                retention_days: config.tracing.retention_days,
                rotate_max_bytes: config.tracing.rotate_max_bytes,
                archive_after_files: config.tracing.archive_after_files,
                ..TraceOptions::default()
            };
            Some(TraceWriter::new(opts, dir))
        } else {
            None
        };

        Ok(Self {
            keys,
            client,
            config,
            limiter,
            pii,
            entity_store,
            budget,
            events,
            cost_tables,
            mcp,
            trace,
            metrics: None,
        })
    }
}

/// Translate a configured MCP server into an [`UpstreamServer`], pre-computing
/// the static headers sent on every upstream request.
///
/// Header order: the auth header (derived from `auth_type` + `auth_value`)
/// comes first, then every `extra_headers` entry is appended. Config validation
/// guarantees `auth_value` is present when `auth_type != none`; a missing value
/// here is treated defensively as "no auth header".
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

    UpstreamServer {
        name: name.to_owned(),
        url: cfg.url.clone(),
        headers,
    }
}

/// Open the persistent entity vault from config and wrap it as an
/// [`EntityStore`] (WP 9.3).
///
/// Steps:
/// 1. Decode the 64-hex-char `key` into a `[u8; 32]`. Config validation already
///    enforces the 64-hex shape, but we re-check defensively so a malformed key
///    fails boot with a clear, key-safe error.
/// 2. Resolve `path`: absolute paths are used as-is; relative paths resolve
///    against `base_dir` (the config-file directory) — same convention as
///    `pii.ner.model_dir`.
/// 3. [`Vault::open`] the database (creates it if absent, verifies the key if it
///    exists). A wrong key surfaces as [`drgtw_vault::VaultError::BadKey`].
///
/// All failures map to [`EngineBuildError::Vault`] with a message that never
/// embeds key material.
fn open_vault_store(
    vault_cfg: &VaultConfig,
    base_dir: &Path,
) -> Result<Arc<dyn EntityStore>, EngineBuildError> {
    // 1. Decode the hex master key into 32 raw bytes.
    let key_bytes = hex::decode(&vault_cfg.key)
        .map_err(|_| EngineBuildError::Vault("vault key is not valid hex".to_owned()))?;
    let key: [u8; 32] = key_bytes.try_into().map_err(|_| {
        EngineBuildError::Vault(
            "vault key must decode to exactly 32 bytes (64 hex chars)".to_owned(),
        )
    })?;

    // 2. Resolve the database path against base_dir for relative paths.
    let path = {
        let p = Path::new(&vault_cfg.path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            base_dir.join(p)
        }
    };

    // 3. Open (or create) the encrypted vault. Wrong key → BadKey here.
    let vault = Vault::open(&path, &key)
        .map_err(|e| EngineBuildError::Vault(format!("{e} (path: {})", path.display())))?;

    Ok(Arc::new(VaultStore::new(Arc::new(vault))))
}
