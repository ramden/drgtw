//! `GET /info` — unauthenticated, cheap, no-LLM service status for ops
//! dashboards, plus `GET /health/ready` readiness for probes.
//!
//! Contract (schema_version = 1): the response is **versioned** — fields are
//! only ever added, never removed or repurposed, so consumers can pin to the
//! shape. It reports version/build provenance, uptime, the *effective* (live,
//! hot-reload-aware) operational config, and a `config_fingerprint` for drift
//! detection across replicas.
//!
//! SECURITY: this endpoint never emits a secret. No API keys, no base URLs, no
//! AWS creds, no vault/session keys, no MCP/event/OTLP endpoints, no guardrail
//! or recognizer regex patterns, no bind address. Only names, booleans, counts,
//! numeric knobs, and a one-way `config_fingerprint`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde::Serialize;

use drgtw_config::{ApiFormat, FailMode, GuardrailAction, GuardrailKind, GuardrailPhase};
use drgtw_proxy::ProxyState;

/// Short commit hash the binary was built from (baked by `build.rs`).
const GIT_SHA: &str = env!("DRGTW_GIT_SHA");
/// RFC3339 UTC build timestamp (baked by `build.rs`).
const BUILT_AT: &str = env!("DRGTW_BUILT_AT");
/// Response schema version. Bump only on a breaking shape change (never expected
/// — fields are additive).
const SCHEMA_VERSION: u32 = 1;

/// State shared with the `/info` + `/health/ready` handlers.
///
/// Holds the same `ProxyState` the proxy router runs on, so `/info` reports the
/// **live** config after a hot reload (not a boot-time snapshot). `started_at`
/// is wall-clock for uptime reporting.
#[derive(Clone)]
pub(crate) struct InfoState {
    pub proxy: Arc<ProxyState>,
    pub started_at: SystemTime,
}

#[derive(Serialize)]
pub(crate) struct InfoResponse {
    service: &'static str,
    schema_version: u32,
    version: &'static str,
    build: Build,
    started_at: String,
    uptime_seconds: u64,
    models: Vec<ModelInfo>,
    model_aliases: std::collections::BTreeMap<String, String>,
    pii: Pii,
    guardrails: Guardrails,
    mcp: Mcp,
    otel: Otel,
    events: Toggle,
    tracing: Toggle,
    config_fingerprint: String,
}

#[derive(Serialize)]
struct Build {
    git_sha: &'static str,
    built_at: &'static str,
}

#[derive(Serialize)]
struct ModelInfo {
    model: String,
    connection: String,
    format: ApiFormat,
}

#[derive(Serialize)]
struct Pii {
    enabled_by_default: bool,
    require_ner: bool,
    embeddings_disable_pii: bool,
    embeddings_require_vault: bool,
    /// `null` = every detected entity kind kept.
    entities: Option<Vec<String>>,
    disabled_recognizers: Vec<String>,
    /// Names only — never the regex patterns.
    custom_recognizers: Vec<String>,
    /// Presence of an encrypted entity vault — never its path or key.
    vault_configured: bool,
    /// `null` when no `[pii.ner]` model is configured.
    ner: Option<Ner>,
}

#[derive(Serialize)]
struct Ner {
    /// `true` when a NER model is configured (and therefore was loaded at boot —
    /// an unloadable model fails boot).
    loaded: bool,
    /// Model directory *basename* only (e.g. `ner-multilingual`), never the path.
    model: String,
    score_threshold: f32,
    fail_mode: FailMode,
    timeout_ms: u64,
    workers: usize,
    queue_capacity: usize,
    /// `null` = NER runs on every role. The customer's #1 drift signal.
    scan_roles: Option<Vec<String>>,
    cache_capacity: usize,
}

#[derive(Serialize)]
struct Guardrails {
    enabled: bool,
    rules: Vec<GuardrailInfo>,
}

#[derive(Serialize)]
struct GuardrailInfo {
    name: String,
    kind: GuardrailKind,
    phase: GuardrailPhase,
    action: GuardrailAction,
}

#[derive(Serialize)]
struct Mcp {
    enabled: bool,
    server_count: usize,
    /// Names + optional descriptions only — never URLs or auth.
    servers: Vec<McpServerInfo>,
}

#[derive(Serialize)]
struct McpServerInfo {
    name: String,
    description: Option<String>,
}

#[derive(Serialize)]
struct Otel {
    enabled: bool,
    traces: bool,
    metrics: bool,
}

#[derive(Serialize)]
struct Toggle {
    enabled: bool,
}

/// Build the `/info` + `/health/ready` sub-router, state pre-applied so it
/// merges into the state-less app router.
pub(crate) fn routes(state: InfoState) -> axum::Router {
    use axum::routing::get;
    axum::Router::new()
        .route("/info", get(handle))
        .route("/health/ready", get(ready))
        .with_state(state)
}

/// `GET /info` — versioned status JSON. Gated by `x-health-token` iff
/// `[server] status_token` is configured; otherwise open like `/health`.
pub(crate) async fn handle(
    State(st): State<InfoState>,
    headers: HeaderMap,
) -> Result<Json<InfoResponse>, StatusCode> {
    let live = st.proxy.live.load();
    let cfg = &live.config;

    // Optional token gate.
    if let Some(expected) = cfg.server.status_token.as_deref() {
        let provided = headers.get("x-health-token").and_then(|v| v.to_str().ok()).unwrap_or("");
        if !ct_eq(expected.as_bytes(), provided.as_bytes()) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    let models = cfg
        .connections
        .iter()
        .flat_map(|c| {
            c.models.iter().map(move |m| ModelInfo {
                model: m.clone(),
                connection: c.name.clone(),
                format: c.format,
            })
        })
        .collect();

    let pii = &cfg.pii;
    let ner = pii.ner.as_ref().map(|n| Ner {
        loaded: true,
        model: basename(&n.model_dir),
        score_threshold: n.score_threshold,
        fail_mode: n.fail_mode,
        timeout_ms: n.timeout_ms,
        workers: n.workers,
        queue_capacity: n.queue_capacity,
        scan_roles: n.scan_roles.clone(),
        cache_capacity: n.cache_capacity,
    });

    let resp = InfoResponse {
        service: "drgtw",
        schema_version: SCHEMA_VERSION,
        version: env!("CARGO_PKG_VERSION"),
        build: Build { git_sha: GIT_SHA, built_at: BUILT_AT },
        started_at: epoch_to_rfc3339(system_time_secs(st.started_at)),
        uptime_seconds: SystemTime::now()
            .duration_since(st.started_at)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        models,
        model_aliases: cfg.model_aliases.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        pii: Pii {
            enabled_by_default: pii.enabled_by_default,
            require_ner: pii.require_ner,
            embeddings_disable_pii: pii.embeddings_disable_pii,
            embeddings_require_vault: pii.embeddings_require_vault,
            entities: pii.entities.clone(),
            disabled_recognizers: pii.disabled_recognizers.clone(),
            custom_recognizers: pii.custom_recognizers.iter().map(|r| r.name.clone()).collect(),
            vault_configured: pii.vault.is_some(),
            ner,
        },
        guardrails: Guardrails {
            enabled: !cfg.guardrails.is_empty(),
            rules: cfg
                .guardrails
                .rules
                .iter()
                .map(|r| GuardrailInfo {
                    name: r.name.clone(),
                    kind: r.kind,
                    phase: r.phase,
                    action: r.action,
                })
                .collect(),
        },
        mcp: Mcp {
            enabled: !cfg.mcp_servers.is_empty(),
            server_count: cfg.mcp_servers.len(),
            servers: {
                // Sorted by name for stable output across `HashMap` iteration.
                let mut s: Vec<McpServerInfo> = cfg
                    .mcp_servers
                    .iter()
                    .map(|(name, srv)| McpServerInfo {
                        name: name.clone(),
                        description: srv.description.clone(),
                    })
                    .collect();
                s.sort_by(|a, b| a.name.cmp(&b.name));
                s
            },
        },
        otel: Otel {
            enabled: cfg.otel.enabled,
            traces: cfg.otel.traces,
            metrics: cfg.otel.metrics,
        },
        events: Toggle { enabled: cfg.events.is_some() },
        tracing: Toggle { enabled: cfg.tracing.enabled },
        config_fingerprint: cfg.fingerprint(),
    };

    Ok(Json(resp))
}

#[derive(Serialize)]
pub(crate) struct ReadyResponse {
    status: &'static str,
    ner_loaded: bool,
}

/// `GET /health/ready` — readiness probe. Always unauthenticated (probes must
/// not carry secrets). Returns `200` once the gateway is serving; reports
/// whether the NER model is loaded so orchestrators can distinguish
/// "up but PII-degraded" from fully ready.
pub(crate) async fn ready(State(st): State<InfoState>) -> Json<ReadyResponse> {
    let live = st.proxy.live.load();
    Json(ReadyResponse {
        status: "ready",
        ner_loaded: live.config.pii.ner.is_some(),
    })
}

/// Compare the status token without a per-byte early exit, so a wrong token of
/// the correct length cannot be recovered prefix-by-prefix via timing. The
/// length-mismatch shortcut below is *not* constant-time — it can leak the
/// expected token's length — which is an accepted trade-off for a low-value
/// health token (an empty token is rejected at config load, so the compared
/// slices are always non-empty).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

fn system_time_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// RFC3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`) from unix seconds. Mirrors the helper in
/// `build.rs`; kept dependency-free (no `chrono`).
fn epoch_to_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as i64;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}
