//! ProxyState construction.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use drgtw_config::{Config, VaultConfig};
use drgtw_events::EventSink;
use drgtw_pii::{EngineBuildError, EntityStore, VaultStore};
use drgtw_trace::{TraceOptions, TraceWriter};
use drgtw_vault::Vault;

use crate::reload::build_live;
use crate::ProxyState;

impl ProxyState {
    /// Build shared proxy state from validated config.
    ///
    /// - Constructs the initial [`Live`] bundle (keys, limiter, budget, pii,
    ///   mcp, cost_tables) via [`build_live`].
    /// - Wraps the live bundle in an [`ArcSwap`] for zero-downtime hot-reload.
    /// - Builds infra fields that survive hot-reload: the HTTP client, the
    ///   optional entity vault, the event sink, the trace writer, and the
    ///   usage-event broadcast channel.
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
        // Build the initial live bundle (no old state → fresh counters).
        let live_initial = build_live(Arc::clone(&config), base_dir, None)?;

        // Persistent entity vault (WP 9.3). Built only when configured; any
        // failure here (bad key length, hex decode, wrong key, unopenable file)
        // fails boot rather than silently degrading to per-request mappings.
        let entity_store: Option<Arc<dyn EntityStore>> = match &config.pii.vault {
            Some(vault_cfg) => Some(open_vault_store(vault_cfg, base_dir)?),
            None => None,
        };

        // Embeddings vault posture (WP 9.4). When no vault is configured the
        // embeddings placeholders are per-request counters — fine for one-off
        // requests, but they break embedding-index/RAG consistency because the
        // same entity maps to a different placeholder on each request. If the
        // operator demanded the vault (`pii.embeddings_require_vault = true`)
        // this is a config contradiction and must fail boot.
        check_embeddings_vault_posture(&config)?;

        // Shared HTTP client for proxy upstream requests. Never rebuilt on
        // hot-reload — pool continuity and keep-alive are important here.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Intentionally no .timeout() — streaming responses must flow freely.
            .build()
            .expect("failed to build reqwest client");

        // Usage-event sink (WP 8.3): only when configured. `EventSink::new`
        // spawns a Tokio worker, so `build` must be called inside a runtime
        // (it always is — `server::run` and every `#[tokio::test]` provide one).
        // `signing_secret` is wired here; `delivery_log` is injected later via
        // `with_history` (History isn't available at build time) which rebuilds
        // the sink with a live delivery log.
        let events = config.events.as_ref().map(|ev| {
            Arc::new(EventSink::new(
                ev.url.clone(),
                ev.auth_bearer.clone(),
                ev.buffer_size,
                ev.timeout_ms,
                ev.signing_secret.clone(),
                None, // delivery_log injected in with_history
            ))
        });

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

        let (usage_broadcast, _) = tokio::sync::broadcast::channel(1024);

        let live = Arc::new(ArcSwap::from_pointee(live_initial));

        Ok(Self {
            client,
            entity_store,
            events,
            trace,
            metrics: None,
            usage_broadcast,
            history: None,
            live,
        })
    }
}

/// Open the persistent entity vault from config and wrap it as an
/// [`EntityStore`] (WP 9.3).
fn open_vault_store(
    vault_cfg: &VaultConfig,
    base_dir: &Path,
) -> Result<Arc<dyn EntityStore>, EngineBuildError> {
    let key_bytes = hex::decode(&vault_cfg.key)
        .map_err(|_| EngineBuildError::Vault("vault key is not valid hex".to_owned()))?;
    let key: [u8; 32] = key_bytes.try_into().map_err(|_| {
        EngineBuildError::Vault(
            "vault key must decode to exactly 32 bytes (64 hex chars)".to_owned(),
        )
    })?;

    let path = {
        let p = Path::new(&vault_cfg.path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            base_dir.join(p)
        }
    };

    let vault = Vault::open(&path, &key)
        .map_err(|e| EngineBuildError::Vault(format!("{e} (path: {})", path.display())))?;

    Ok(Arc::new(VaultStore::new(Arc::new(vault))))
}

/// Decide the boot-time posture for `/v1/embeddings` placeholder stability
/// (WP 9.4) and act on it.
fn check_embeddings_vault_posture(config: &Config) -> Result<(), EngineBuildError> {
    if config.pii.vault.is_some() {
        return Ok(());
    }
    if config.pii.embeddings_require_vault {
        return Err(EngineBuildError::Vault(
            "pii.embeddings_require_vault is set but no [pii.vault] is configured: \
             /v1/embeddings cannot guarantee stable placeholders without a persistent \
             vault — configure [pii.vault] or unset pii.embeddings_require_vault"
                .to_owned(),
        ));
    }
    tracing::warn!(
        "no [pii.vault] configured: /v1/embeddings will use per-request placeholders; \
         cross-request vector consistency is not guaranteed. Configure [pii.vault] for \
         stable placeholders, or set pii.embeddings_require_vault to reject embeddings \
         requests instead of serving inconsistent placeholders."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use drgtw_config::{PiiConfig, VaultConfig};

    fn config_with_pii(vault: Option<VaultConfig>, require: bool) -> Config {
        Config {
            pii: PiiConfig {
                vault,
                embeddings_require_vault: require,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn dummy_vault() -> VaultConfig {
        VaultConfig {
            path: "vault.db".to_owned(),
            key: "a".repeat(64),
        }
    }

    #[test]
    fn posture_ok_when_no_vault_and_flag_unset() {
        let config = config_with_pii(None, false);
        assert!(check_embeddings_vault_posture(&config).is_ok());
    }

    #[test]
    fn posture_ok_when_vault_present_and_flag_set() {
        let config = config_with_pii(Some(dummy_vault()), true);
        assert!(check_embeddings_vault_posture(&config).is_ok());
    }

    #[test]
    fn posture_fails_boot_when_required_but_no_vault() {
        let config = config_with_pii(None, true);
        match check_embeddings_vault_posture(&config) {
            Err(EngineBuildError::Vault(msg)) => {
                assert!(msg.contains("embeddings_require_vault"), "{msg}");
                assert!(msg.contains("[pii.vault]"), "{msg}");
            }
            other => panic!("expected Vault error, got {other:?}"),
        }
    }
}
