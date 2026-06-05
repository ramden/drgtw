//! DRGTW configuration: TOML schema, env-var resolution, validation.
//!
//! Public API contract (Phase 0 / WP 0.2). The types below are the agreed
//! cross-crate interface — extend, but do not break, without a lead decision.

use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;

use serde::Deserialize;
use url::Url;

/// Prefix every virtual key must carry.
pub const VIRTUAL_KEY_PREFIX: &str = "sk-drgtw-";

/// Fully loaded, env-resolved, validated configuration.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub connections: Vec<Connection>,
    #[serde(default)]
    pub virtual_keys: Vec<VirtualKey>,
    #[serde(default)]
    pub pii: PiiConfig,
    /// Optional event-streaming sink (WP 8.1).
    #[serde(default)]
    pub events: Option<EventsConfig>,
    /// Fallback routing behaviour (WP 8.1).
    #[serde(default)]
    pub fallback: FallbackConfig,
    /// Global model aliases: alias name → target model name. TOML shape
    /// `[model_aliases]` with `alias = "target"` entries. Resolution is
    /// ONE LEVEL only — if a target is itself an alias key, it is NOT
    /// re-resolved (no recursive chains). Defaults to empty (no aliases).
    #[serde(default)]
    pub model_aliases: HashMap<String, String>,
    /// Aggregated upstream MCP servers, keyed by name (WP-A). TOML shape
    /// `[mcp_servers.<name>]`. Defaults to empty (no MCP servers configured).
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// Filesystem request tracing. On by default; logrotate-style.
    #[serde(default)]
    pub tracing: TracingConfig,
    /// OpenTelemetry OTLP export (traces + metrics). Off by default; additive
    /// to `[tracing]` (filesystem JSONL) which is unrelated and untouched.
    #[serde(default)]
    pub otel: OtelConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Address the gateway listens on.
    #[serde(default = "default_bind_addr")]
    pub bind_addr: SocketAddr,
    /// Maximum allowed request body size in bytes.
    /// Requests exceeding this are rejected with HTTP 413.
    /// Default: 10 MiB (10_485_760). Must be > 0.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            max_body_bytes: default_max_body_bytes(),
        }
    }
}

fn default_bind_addr() -> SocketAddr {
    "127.0.0.1:8080".parse().expect("valid default bind addr")
}

fn default_max_body_bytes() -> usize {
    10_485_760 // 10 MiB
}

/// Per-model cost entry for a connection (WP 8.1).
///
/// Both prices are in USD per 1 million tokens.
/// Keys may use the same trailing-`*` wildcard syntax as the `models` list.
/// Keys need not appear in the connection's `models` list (cost for wildcard-served models).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ModelCost {
    /// USD per 1M input tokens. Must be finite and >= 0.
    pub input_per_1m: f64,
    /// USD per 1M output tokens. Must be finite and >= 0.
    pub output_per_1m: f64,
}

/// An upstream provider connection.
#[derive(Debug, Clone, Deserialize)]
pub struct Connection {
    /// Unique name referenced by virtual keys.
    pub name: String,
    /// Absolute http(s) base URL of the upstream provider.
    pub base_url: String,
    /// Upstream API key. Supports `${ENV_VAR}` references, resolved at load.
    pub api_key: String,
    /// Wire format this upstream speaks.
    pub format: ApiFormat,
    /// Model names served by this connection.
    #[serde(default)]
    pub models: Vec<String>,
    /// Per-model cost overrides (WP 8.1). Keys may be exact or wildcard patterns.
    /// Defaults to empty (no cost tracking for this connection).
    #[serde(default)]
    pub model_costs: HashMap<String, ModelCost>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiFormat {
    OpenAi,
    Anthropic,
    /// Native AWS Bedrock `InvokeModel` (non-streaming), Anthropic-shaped body,
    /// bearer auth. The URL builder appends `/model/{model}/invoke`, so the
    /// `base_url` carries NO `/v1` suffix
    /// (e.g. `https://bedrock-runtime.eu-central-1.amazonaws.com`).
    Bedrock,
}

/// Per-virtual-key spend budget (WP 8.1).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Budget {
    /// Maximum spend in USD for the window. Must be finite and > 0.
    pub max_usd: f64,
    /// Window size in seconds. Must be > 0.
    pub per_seconds: u32,
}

/// A virtual API key handed to downstream callers.
#[derive(Debug, Clone, Deserialize)]
pub struct VirtualKey {
    /// The key value. Must start with [`VIRTUAL_KEY_PREFIX`].
    pub key: String,
    /// Names of connections this key may use. Must be non-empty and resolve.
    pub connections: Vec<String>,
    /// Optional model allowlist. `None` = all models of allowed connections.
    /// Entries may end with `*` for prefix-match (e.g. `"gpt-*"`).
    #[serde(default)]
    pub models: Option<Vec<String>>,
    /// Optional per-key rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
    /// Optional per-key USD spend budget (WP 8.1).
    #[serde(default)]
    pub budget: Option<Budget>,
}

/// Per-virtual-key token-bucket rate limit configuration.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RateLimit {
    /// Maximum requests in the window.
    pub requests: u32,
    /// Window size in seconds.
    pub per_seconds: u32,
}

/// PII pipeline settings. Placeholder in Phase 0; grows in Phase 3+.
#[derive(Debug, Clone, Deserialize)]
pub struct PiiConfig {
    /// Default mode when the caller sends no `x-drgtw-pii` header.
    /// Defaults to `true` — privacy-first: callers must opt OUT.
    #[serde(default = "default_pii_enabled")]
    pub enabled_by_default: bool,
    /// Built-in recognizers to disable (by name: "email", "phone", "iban",
    /// "credit_card"). Empty = all built-ins active.
    #[serde(default)]
    pub disabled_recognizers: Vec<String>,
    /// Additional regex-based recognizers.
    #[serde(default)]
    pub custom_recognizers: Vec<CustomRecognizer>,
    /// Optional NER (named-entity recognition) configuration. When absent,
    /// NER is not loaded. When present, `model_dir` is required.
    #[serde(default)]
    pub ner: Option<NerConfig>,
    /// Optional persistent encrypted entity vault (WP 9.1). When absent, the
    /// vault is off and placeholder mappings are per-request only. When present,
    /// both `path` and `key` are required.
    #[serde(default)]
    pub vault: Option<VaultConfig>,
}

/// Persistent encrypted entity-vault configuration (WP 9.1).
///
/// The vault stores stable entity→placeholder mappings in a SQLite database,
/// with original values encrypted at rest. Required fields: `path`, `key`.
#[derive(Debug, Clone, Deserialize)]
pub struct VaultConfig {
    /// Path to the SQLite database file. Required, non-empty. A relative path is
    /// resolved against the config-file directory by the consumer (the same
    /// convention as `pii.ner.model_dir`), not here.
    pub path: String,
    /// Master key. Supports `${ENV_VAR}` substitution (resolved at `load()`
    /// time). After resolution it must be exactly 64 hex characters (32 bytes).
    pub key: String,
}

impl Default for PiiConfig {
    fn default() -> Self {
        Self {
            enabled_by_default: default_pii_enabled(),
            disabled_recognizers: Vec::new(),
            custom_recognizers: Vec::new(),
            ner: None,
            vault: None,
        }
    }
}

fn default_pii_enabled() -> bool {
    true
}

/// Behaviour when NER inference fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FailMode {
    /// On error, log a warning and return no detections (open gate). Default.
    #[default]
    Open,
    /// On error, propagate the error to the caller (closed gate).
    Closed,
}

/// NER model and worker-pool configuration. Required fields: `model_dir`.
/// All other fields have defaults.
#[derive(Debug, Clone, Deserialize)]
pub struct NerConfig {
    /// Path to the model directory. Supports `${ENV_VAR}` substitution
    /// (resolved at `load()` time). Existence is NOT checked here — it is
    /// checked by the engine builder on the machine that actually runs inference.
    pub model_dir: String,
    /// Minimum per-span score to emit a detection. Range `0.0..=1.0`.
    #[serde(default = "default_score_threshold")]
    pub score_threshold: f32,
    /// Behaviour when NER inference errors. Default: `open`.
    #[serde(default)]
    pub fail_mode: FailMode,
    /// Per-request timeout in milliseconds. Must be `> 0`.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Number of NER worker threads. Must be `> 0`.
    #[serde(default = "default_workers")]
    pub workers: usize,
    /// Max pending requests in the NER queue. Must be `> 0`.
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: usize,
}

fn default_score_threshold() -> f32 {
    0.5
}
fn default_timeout_ms() -> u64 {
    5000
}
fn default_workers() -> usize {
    2
}
fn default_queue_capacity() -> usize {
    64
}

/// Event-streaming sink configuration (WP 8.1).
///
/// When present, the gateway will POST cost/usage events to `url`.
#[derive(Debug, Clone, Deserialize)]
pub struct EventsConfig {
    /// Absolute http(s) URL of the event sink. Required. Supports `${ENV_VAR}`.
    pub url: String,
    /// Optional Bearer token for the event sink. Supports `${ENV_VAR}`.
    #[serde(default)]
    pub auth_bearer: Option<String>,
    /// In-memory queue capacity before back-pressure is applied. Default 1024. Must be > 0.
    #[serde(default = "default_events_buffer_size")]
    pub buffer_size: usize,
    /// Per-request timeout in milliseconds when posting an event. Default 5000. Must be > 0.
    #[serde(default = "default_events_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_events_buffer_size() -> usize {
    1024
}
fn default_events_timeout_ms() -> u64 {
    5000
}

/// Fallback routing behaviour (WP 8.1).
///
/// When `enabled` is `true` (the default) the gateway will try the next
/// available connection if the primary one returns an error.
#[derive(Debug, Clone, Deserialize)]
pub struct FallbackConfig {
    /// Enable connection-level fallback. Default `true`.
    #[serde(default = "default_fallback_enabled")]
    pub enabled: bool,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_fallback_enabled() -> bool {
    true
}

/// Filesystem request-tracing configuration.
///
/// Tracing is **on by default**. When `enabled` is `false` the gateway writes
/// no trace files. Paths are resolved against the config-file directory by the
/// consumer (same convention as `pii.ner.model_dir`), not here.
#[derive(Debug, Clone, Deserialize)]
pub struct TracingConfig {
    /// Master switch. Default `true`; set `false` to disable tracing entirely.
    #[serde(default = "default_tracing_enabled")]
    pub enabled: bool,
    /// Directory for trace files, relative to the config base dir. Default `"traces"`.
    #[serde(default = "default_tracing_dir")]
    pub dir: String,
    /// Archives/rotated files older than this many days are deleted. Default `90`.
    #[serde(default = "default_tracing_retention_days")]
    pub retention_days: u64,
    /// The active trace file rotates once it reaches this size in bytes.
    /// Default `52428800` (50 MiB).
    #[serde(default = "default_tracing_rotate_max_bytes")]
    pub rotate_max_bytes: u64,
    /// Rotated files are bundled into a tar.gz once this many exist. Default `10`.
    #[serde(default = "default_tracing_archive_after_files")]
    pub archive_after_files: u64,
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            enabled: default_tracing_enabled(),
            dir: default_tracing_dir(),
            retention_days: default_tracing_retention_days(),
            rotate_max_bytes: default_tracing_rotate_max_bytes(),
            archive_after_files: default_tracing_archive_after_files(),
        }
    }
}

fn default_tracing_enabled() -> bool {
    true
}
fn default_tracing_dir() -> String {
    "traces".to_string()
}
fn default_tracing_retention_days() -> u64 {
    90
}
fn default_tracing_rotate_max_bytes() -> u64 {
    52_428_800
}
fn default_tracing_archive_after_files() -> u64 {
    10
}

/// OTLP transport protocol.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OtelProtocol {
    /// gRPC (OTLP/gRPC), conventional port 4317. Default.
    #[default]
    Grpc,
    /// HTTP/protobuf (OTLP/HTTP), conventional port 4318.
    Http,
}

/// OpenTelemetry OTLP export configuration.
///
/// **Default = disabled.** When `enabled` is `false` no provider is installed,
/// no exporter is created, and the gateway's `[tracing]` JSONL writer plus the
/// stderr `fmt` subscriber behave byte-identically to before.
///
/// Privacy: spans and metrics carry ONLY the allow-listed metadata (model,
/// connection, status, token counts, cost, latency, ttft, key_id, pii_flag,
/// request_id, endpoint, error class, fallback attempts). Prompt/response
/// content, PII values, pseudonyms, and secrets are NEVER exported — there is
/// no config switch to enable content capture.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OtelConfig {
    /// Master switch. Default `false`. Everything below is inert until `true`.
    pub enabled: bool,
    /// OTLP endpoint URL. gRPC conventionally `:4317`, HTTP `:4318`.
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` env var, if set, overrides this at init.
    pub endpoint: String,
    /// Transport protocol: `grpc` (default) or `http`.
    pub protocol: OtelProtocol,
    /// `service.name` resource attribute. Default `"drgtw"`.
    pub service_name: String,
    /// Export spans. Default `true` (only takes effect when `enabled`).
    pub traces: bool,
    /// Export metrics. Default `true` (only takes effect when `enabled`).
    pub metrics: bool,
    /// Parent-based trace ratio sampler ratio, `0.0..=1.0`. Default `1.0`.
    pub sample_ratio: f64,
    /// Periodic metric reader push interval in milliseconds. Default `10000`.
    pub export_interval_ms: u64,
    /// Per-export deadline in milliseconds (traces batch + metrics). Default `5000`.
    pub export_timeout_ms: u64,
    /// Include `drgtw.key_id` as a **metric** label. Default `false` — key_id
    /// multiplies metric cardinality (keys × models × connections × status).
    /// Spans always carry key_id (spans are not aggregated); this flag controls
    /// metrics only.
    pub metrics_include_key_id: bool,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: default_otel_endpoint(),
            protocol: OtelProtocol::default(),
            service_name: default_otel_service_name(),
            traces: true,
            metrics: true,
            sample_ratio: default_otel_sample_ratio(),
            export_interval_ms: default_otel_export_interval_ms(),
            export_timeout_ms: default_otel_export_timeout_ms(),
            metrics_include_key_id: false,
        }
    }
}

fn default_otel_endpoint() -> String {
    "http://localhost:4317".to_string()
}
fn default_otel_service_name() -> String {
    "drgtw".to_string()
}
fn default_otel_sample_ratio() -> f64 {
    1.0
}
fn default_otel_export_interval_ms() -> u64 {
    10_000
}
fn default_otel_export_timeout_ms() -> u64 {
    5_000
}

/// Upstream authentication scheme for an MCP server (WP-A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthType {
    /// No upstream auth header is sent. Default.
    #[default]
    None,
    /// Sends `X-API-Key: <auth_value>`.
    ApiKey,
    /// Sends `Authorization: Bearer <auth_value>`.
    Bearer,
}

/// A single aggregated upstream MCP server (WP-A).
///
/// Keyed by name in [`Config::mcp_servers`]. The name is the map key, not a
/// field. `url`, `auth_value`, and every `extra_headers` value support
/// `${ENV_VAR}` substitution (resolved at `load()` time).
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    /// Absolute http(s) URL of the upstream MCP endpoint. Required.
    pub url: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// Upstream auth scheme. Default: `none`.
    #[serde(default)]
    pub auth_type: McpAuthType,
    /// Auth credential. Required iff `auth_type != none`; must be absent when
    /// `auth_type == none`. Supports `${ENV_VAR}`.
    #[serde(default)]
    pub auth_value: Option<String>,
    /// Optional static headers sent on every upstream request. Values support
    /// `${ENV_VAR}`.
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
}

/// A user-defined regex recognizer. The regex is compiled by the PII engine
/// at startup; compile errors fail boot, not config load.
#[derive(Debug, Clone, Deserialize)]
pub struct CustomRecognizer {
    /// Entity kind name, used as placeholder prefix (uppercased): `name = "ticket"` → `TICKET_1`.
    pub name: String,
    /// Regex pattern (Rust `regex` crate syntax).
    pub pattern: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid TOML in `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("environment variable `{var}` referenced by `{field}` is not set")]
    MissingEnvVar { var: String, field: String },
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    /// Resolve a model name through the global `[model_aliases]` table.
    ///
    /// Resolution is **one level only**: if `name` is an alias, its target is
    /// returned even when that target is itself an alias key (no recursive
    /// chains are followed). When `name` is not an alias it is returned
    /// unchanged.
    pub fn resolve_model_alias<'a>(&'a self, name: &'a str) -> &'a str {
        self.model_aliases
            .get(name)
            .map(String::as_str)
            .unwrap_or(name)
    }
}

/// Load, env-resolve, and validate a config file.
///
/// Validation rules (WP 0.2):
/// - connection names unique and non-empty
/// - `base_url` is an absolute http(s) URL
/// - virtual keys start with [`VIRTUAL_KEY_PREFIX`], are unique,
///   and reference only existing connections (non-empty list)
/// - `${VAR}` references in `api_key` resolved from the environment;
///   unset vars are a hard error
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let path_str = path.display().to_string();

    // 1. Read file.
    let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path_str.clone(),
        source: e,
    })?;

    // 2. Parse TOML.
    let mut config: Config = toml::from_str(&raw).map_err(|e| ConfigError::Parse {
        path: path_str.clone(),
        source: e,
    })?;

    // 3. Env-var resolution on connections.
    for conn in &mut config.connections {
        let field_api_key = format!("connections[{}].api_key", conn.name);
        conn.api_key = resolve_env_vars(&conn.api_key, &field_api_key)?;

        let field_base_url = format!("connections[{}].base_url", conn.name);
        conn.base_url = resolve_env_vars(&conn.base_url, &field_base_url)?;
    }

    // 3b. Env-var resolution on pii.ner.model_dir.
    if let Some(ner) = &mut config.pii.ner {
        ner.model_dir = resolve_env_vars(&ner.model_dir, "pii.ner.model_dir")?;
    }

    // 3b'. Env-var resolution on pii.vault.key (and path, for symmetry).
    if let Some(vault) = &mut config.pii.vault {
        vault.path = resolve_env_vars(&vault.path, "pii.vault.path")?;
        vault.key = resolve_env_vars(&vault.key, "pii.vault.key")?;
    }

    // 3c. Env-var resolution on events.url and events.auth_bearer.
    if let Some(events) = &mut config.events {
        events.url = resolve_env_vars(&events.url, "events.url")?;
        if let Some(bearer) = &events.auth_bearer {
            let resolved = resolve_env_vars(bearer, "events.auth_bearer")?;
            events.auth_bearer = Some(resolved);
        }
    }

    // 3d. Env-var resolution on mcp_servers.<name>.{url, auth_value, extra_headers.*}.
    for (name, server) in &mut config.mcp_servers {
        let field_url = format!("mcp_servers[{name}].url");
        server.url = resolve_env_vars(&server.url, &field_url)?;

        if let Some(auth_value) = &server.auth_value {
            let field = format!("mcp_servers[{name}].auth_value");
            let resolved = resolve_env_vars(auth_value, &field)?;
            server.auth_value = Some(resolved);
        }

        for (header, value) in &mut server.extra_headers {
            let field = format!("mcp_servers[{name}].extra_headers[{header}]");
            *value = resolve_env_vars(value, &field)?;
        }
    }

    // 4. Validate.
    validate(&config)?;

    Ok(config)
}

/// Resolve `${VAR}` references in a string from environment variables.
///
/// Literal `$` not followed by `{...}` passes through unchanged.
/// Returns `ConfigError::MissingEnvVar` if any referenced variable is unset.
fn resolve_env_vars(value: &str, field: &str) -> Result<String, ConfigError> {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' {
            if chars.peek() == Some(&'{') {
                // Consume the '{'
                chars.next();
                // Collect until '}'
                let mut var_name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    var_name.push(c);
                }
                if !closed {
                    // Malformed `${...` without closing brace — treat as literal.
                    result.push('$');
                    result.push('{');
                    result.push_str(&var_name);
                } else {
                    let val = std::env::var(&var_name).map_err(|_| ConfigError::MissingEnvVar {
                        var: var_name.clone(),
                        field: field.to_owned(),
                    })?;
                    result.push_str(&val);
                }
            } else {
                // Literal `$` without `{` — pass through unchanged.
                result.push('$');
            }
        } else {
            result.push(ch);
        }
    }

    Ok(result)
}

/// Validate the fully env-resolved config.
fn validate(config: &Config) -> Result<(), ConfigError> {
    // --- Server ---
    if config.server.max_body_bytes == 0 {
        return Err(ConfigError::Invalid(
            "server.max_body_bytes must be > 0".to_owned(),
        ));
    }

    // --- Connections ---
    let mut conn_names: HashSet<&str> = HashSet::new();
    for conn in &config.connections {
        // Non-empty name.
        if conn.name.is_empty() {
            return Err(ConfigError::Invalid(
                "connection name must not be empty".to_owned(),
            ));
        }
        // Unique name.
        if !conn_names.insert(conn.name.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "duplicate connection name `{}`",
                conn.name
            )));
        }
        // Absolute http(s) URL, no query or fragment.
        validate_base_url(&conn.base_url, &conn.name)?;

        // api_key non-empty after resolution.
        if conn.api_key.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "connections[{}].api_key must not be empty",
                conn.name
            )));
        }

        // Models: valid patterns, no duplicates within a connection.
        let mut model_names: HashSet<&str> = HashSet::new();
        for model in &conn.models {
            let ctx = format!("connections[{}].models", conn.name);
            validate_model_pattern(model, &ctx)?;
            if !model_names.insert(model.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "connections[{}].models contains duplicate `{}`",
                    conn.name, model
                )));
            }
        }

        // model_costs: keys are valid patterns; values are finite and >= 0.
        for (key, cost) in &conn.model_costs {
            let ctx = format!("connections[{}].model_costs", conn.name);
            if key.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "{ctx}: model cost key must not be empty"
                )));
            }
            validate_model_pattern(key, &ctx)?;
            if !cost.input_per_1m.is_finite() || cost.input_per_1m < 0.0 {
                return Err(ConfigError::Invalid(format!(
                    "{ctx}[\"{key}\"].input_per_1m must be a finite value >= 0"
                )));
            }
            if !cost.output_per_1m.is_finite() || cost.output_per_1m < 0.0 {
                return Err(ConfigError::Invalid(format!(
                    "{ctx}[\"{key}\"].output_per_1m must be a finite value >= 0"
                )));
            }
        }
    }

    // --- Virtual keys ---
    let mut vk_keys: HashSet<&str> = HashSet::new();
    for vk in &config.virtual_keys {
        // Must start with prefix and be longer than just the prefix.
        if !vk.key.starts_with(VIRTUAL_KEY_PREFIX) || vk.key.len() <= VIRTUAL_KEY_PREFIX.len() {
            return Err(ConfigError::Invalid(format!(
                "virtual key `{}` must start with `{}` and have additional characters",
                vk.key, VIRTUAL_KEY_PREFIX
            )));
        }
        // Unique.
        if !vk_keys.insert(vk.key.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "duplicate virtual key `{}`",
                vk.key
            )));
        }
        // connections list non-empty.
        if vk.connections.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "virtual key `{}` has an empty connections list",
                vk.key
            )));
        }
        // Every named connection resolves.
        for conn_name in &vk.connections {
            if !conn_names.contains(conn_name.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "virtual key `{}` references unknown connection `{}`",
                    vk.key, conn_name
                )));
            }
        }
        // If models allowlist present, must be non-empty, valid patterns.
        if let Some(models) = &vk.models {
            if models.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "virtual key `{}` has an empty models allowlist; omit the field to allow all",
                    vk.key
                )));
            }
            let ctx = format!("virtual key `{}`  models allowlist", vk.key);
            for pattern in models {
                validate_model_pattern(pattern, &ctx)?;
            }
        }

        // Rate limit: if present, both fields must be > 0.
        if let Some(rl) = &vk.rate_limit {
            if rl.requests == 0 {
                return Err(ConfigError::Invalid(format!(
                    "virtual key `{}` rate_limit.requests must be > 0",
                    vk.key
                )));
            }
            if rl.per_seconds == 0 {
                return Err(ConfigError::Invalid(format!(
                    "virtual key `{}` rate_limit.per_seconds must be > 0",
                    vk.key
                )));
            }
        }

        // Budget: if present, max_usd must be finite and > 0; per_seconds must be > 0.
        if let Some(budget) = &vk.budget {
            if !budget.max_usd.is_finite() || budget.max_usd <= 0.0 {
                return Err(ConfigError::Invalid(format!(
                    "virtual key `{}` budget.max_usd must be a finite value > 0",
                    vk.key
                )));
            }
            if budget.per_seconds == 0 {
                return Err(ConfigError::Invalid(format!(
                    "virtual key `{}` budget.per_seconds must be > 0",
                    vk.key
                )));
            }
        }
    }

    // --- PII custom recognizers ---
    let mut rec_names: HashSet<&str> = HashSet::new();
    for rec in &config.pii.custom_recognizers {
        if rec.name.is_empty()
            || !rec
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(ConfigError::Invalid(format!(
                "pii.custom_recognizers name `{}` must be non-empty ascii alphanumeric/underscore",
                rec.name
            )));
        }
        if !rec_names.insert(rec.name.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "duplicate pii.custom_recognizers name `{}`",
                rec.name
            )));
        }
        if rec.pattern.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "pii.custom_recognizers `{}` has an empty pattern",
                rec.name
            )));
        }
    }

    // --- PII NER ---
    if let Some(ner) = &config.pii.ner {
        if ner.model_dir.is_empty() {
            return Err(ConfigError::Invalid(
                "pii.ner.model_dir must not be empty".to_owned(),
            ));
        }
        if !(0.0..=1.0).contains(&ner.score_threshold) {
            return Err(ConfigError::Invalid(format!(
                "pii.ner.score_threshold `{}` must be in the range 0.0..=1.0",
                ner.score_threshold
            )));
        }
        if ner.timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "pii.ner.timeout_ms must be > 0".to_owned(),
            ));
        }
        if ner.workers == 0 {
            return Err(ConfigError::Invalid(
                "pii.ner.workers must be > 0".to_owned(),
            ));
        }
        if ner.queue_capacity == 0 {
            return Err(ConfigError::Invalid(
                "pii.ner.queue_capacity must be > 0".to_owned(),
            ));
        }
    }

    // --- PII vault (WP 9.1) ---
    if let Some(vault) = &config.pii.vault {
        if vault.path.is_empty() {
            return Err(ConfigError::Invalid(
                "pii.vault.path must not be empty".to_owned(),
            ));
        }
        // Key must be exactly 64 hex characters (32 bytes) after env resolution.
        // NEVER echo the key material in the error message.
        let key_ok = vault.key.len() == 64 && vault.key.chars().all(|c| c.is_ascii_hexdigit());
        if !key_ok {
            return Err(ConfigError::Invalid(
                "pii.vault.key must be 64 hex characters".to_owned(),
            ));
        }
    }

    // --- Events ---
    if let Some(events) = &config.events {
        validate_absolute_http_url(&events.url, "events.url")?;
        if events.buffer_size == 0 {
            return Err(ConfigError::Invalid(
                "events.buffer_size must be > 0".to_owned(),
            ));
        }
        if events.timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "events.timeout_ms must be > 0".to_owned(),
            ));
        }
    }

    // --- MCP servers (WP-A) ---
    for (name, server) in &config.mcp_servers {
        // Server name (map key): non-empty, ascii alphanumeric / `_` / `-`.
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(ConfigError::Invalid(format!(
                "mcp_servers name `{name}` must be non-empty ascii alphanumeric, `_`, or `-`"
            )));
        }

        // url: absolute http(s), no query/fragment.
        validate_absolute_http_url(&server.url, &format!("mcp_servers[{name}].url"))?;

        // auth_type / auth_value coupling.
        match server.auth_type {
            McpAuthType::None => {
                if server.auth_value.is_some() {
                    return Err(ConfigError::Invalid(format!(
                        "mcp_servers[{name}].auth_value must be absent when auth_type is none"
                    )));
                }
            }
            _ => {
                let ok = server
                    .auth_value
                    .as_deref()
                    .map(|v| !v.is_empty())
                    .unwrap_or(false);
                if !ok {
                    return Err(ConfigError::Invalid(format!(
                        "mcp_servers[{name}].auth_value must be present and non-empty when auth_type is not none"
                    )));
                }
            }
        }

        // extra_headers: keys non-empty, valid HTTP header name chars.
        for header in server.extra_headers.keys() {
            if header.is_empty()
                || !header
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                return Err(ConfigError::Invalid(format!(
                    "mcp_servers[{name}].extra_headers header name `{header}` must be non-empty ascii alphanumeric or `-`"
                )));
            }
        }

        // Header values flow into HTTP requests: reject control chars (\r, \n,
        // \0) to prevent header injection. Never echo the value (may be secret).
        if let Some(auth_value) = &server.auth_value
            && auth_value.chars().any(|c| c.is_ascii_control())
        {
            return Err(ConfigError::Invalid(format!(
                "mcp_servers[{name}].auth_value must not contain control characters"
            )));
        }
        for (header, value) in &server.extra_headers {
            if value.chars().any(|c| c.is_ascii_control()) {
                return Err(ConfigError::Invalid(format!(
                    "mcp_servers[{name}].extra_headers[{header}] must not contain control characters"
                )));
            }
        }
    }

    // --- OTel ---
    // Only validate when enabled: a disabled section may carry placeholder
    // defaults and must never fail boot.
    if config.otel.enabled {
        if config.otel.endpoint.is_empty() {
            return Err(ConfigError::Invalid(
                "otel.endpoint must not be empty when otel.enabled is true".to_owned(),
            ));
        }
        // Endpoint must be a valid absolute http(s) URL. (No query/fragment —
        // OTLP endpoints are bare host:port roots.)
        validate_absolute_http_url(&config.otel.endpoint, "otel.endpoint")?;
        if !(0.0..=1.0).contains(&config.otel.sample_ratio) {
            return Err(ConfigError::Invalid(format!(
                "otel.sample_ratio must be in 0.0..=1.0, got {}",
                config.otel.sample_ratio
            )));
        }
        if config.otel.export_interval_ms == 0 {
            return Err(ConfigError::Invalid(
                "otel.export_interval_ms must be > 0".to_owned(),
            ));
        }
        if config.otel.export_timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "otel.export_timeout_ms must be > 0".to_owned(),
            ));
        }
    }

    Ok(())
}

/// Validate a single model pattern entry.
///
/// Rules:
/// - Must not be empty.
/// - `*` is only allowed as the **last** character.
/// - At most one `*` in the entire string.
fn validate_model_pattern(pattern: &str, context: &str) -> Result<(), ConfigError> {
    if pattern.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "{context} contains an empty string"
        )));
    }
    let star_count = pattern.chars().filter(|&c| c == '*').count();
    if star_count > 1 {
        return Err(ConfigError::Invalid(format!(
            "{context}: model pattern `{pattern}` contains more than one `*`"
        )));
    }
    if star_count == 1 && !pattern.ends_with('*') {
        return Err(ConfigError::Invalid(format!(
            "{context}: model pattern `{pattern}` has `*` in non-terminal position; `*` may only appear at the end"
        )));
    }
    Ok(())
}

/// Check that a base_url is an absolute http(s) URL with no query or fragment.
fn validate_base_url(url_str: &str, conn_name: &str) -> Result<(), ConfigError> {
    let field = format!("connections[{}].base_url", conn_name);
    validate_absolute_http_url(url_str, &field)
}

/// Check that a URL string is an absolute http(s) URL with no query or fragment.
/// `field` is used in error messages (e.g. `"events.url"`).
fn validate_absolute_http_url(url_str: &str, field: &str) -> Result<(), ConfigError> {
    let url = Url::parse(url_str)
        .map_err(|_| ConfigError::Invalid(format!("{field} `{url_str}` is not a valid URL",)))?;
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(ConfigError::Invalid(format!(
            "{field} `{url_str}` must use http or https scheme",
        )));
    }
    if !url.host_str().map(|h| !h.is_empty()).unwrap_or(false) {
        return Err(ConfigError::Invalid(format!(
            "{field} `{url_str}` must be an absolute URL with a host",
        )));
    }
    if url.query().is_some() {
        return Err(ConfigError::Invalid(format!(
            "{field} `{url_str}` must not contain a query string",
        )));
    }
    if url.fragment().is_some() {
        return Err(ConfigError::Invalid(format!(
            "{field} `{url_str}` must not contain a fragment",
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // Helper: write TOML to a temp file and call load().
    fn load_toml(content: &str) -> Result<Config, ConfigError> {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(content.as_bytes()).expect("write");
        load(f.path())
    }

    // -----------------------------------------------------------------------
    // Happy path
    // -----------------------------------------------------------------------

    #[test]
    fn test_happy_path_full_config() {
        // SAFETY: tests run sequentially via unique var names; set_var is
        // unsafe in edition 2024 because it is not thread-safe, but each test
        // uses unique var names so parallel test threads cannot collide on the
        // same var.
        unsafe {
            std::env::set_var("DRGTW_TEST_HAPPY_KEY", "sk-live-abc123");
        }

        let toml = r#"
[server]
bind_addr = "0.0.0.0:9090"

[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "${DRGTW_TEST_HAPPY_KEY}"
format = "open_ai"
models = ["gpt-4o", "gpt-4o-mini"]

[[connections]]
name = "anthropic"
base_url = "https://api.anthropic.com"
api_key = "literal-key-value"
format = "anthropic"

[[virtual_keys]]
key = "sk-drgtw-testkey001"
connections = ["openai"]
models = ["gpt-4o"]

[[virtual_keys]]
key = "sk-drgtw-testkey002"
connections = ["openai", "anthropic"]

[pii]
enabled_by_default = true
"#;
        let cfg = load_toml(toml).expect("should load");
        assert_eq!(cfg.server.bind_addr.to_string(), "0.0.0.0:9090");
        assert_eq!(cfg.connections.len(), 2);
        assert_eq!(cfg.connections[0].api_key, "sk-live-abc123");
        assert_eq!(cfg.connections[1].api_key, "literal-key-value");
        assert_eq!(cfg.virtual_keys.len(), 2);
        assert!(cfg.pii.enabled_by_default);

        unsafe {
            std::env::remove_var("DRGTW_TEST_HAPPY_KEY");
        }
    }

    #[test]
    fn test_minimal_config_defaults() {
        // Empty TOML — all fields should get their defaults.
        let cfg = load_toml("").expect("minimal config");
        assert_eq!(cfg.server.bind_addr.to_string(), "127.0.0.1:8080");
        assert_eq!(cfg.server.max_body_bytes, 10_485_760);
        assert!(cfg.connections.is_empty());
        assert!(cfg.virtual_keys.is_empty());
        assert!(
            cfg.pii.enabled_by_default,
            "pii on by default — privacy-first"
        );
    }

    // -----------------------------------------------------------------------
    // Model aliases (Feature 1)
    // -----------------------------------------------------------------------

    #[test]
    fn test_model_aliases_absent_defaults_empty() {
        let cfg = load_toml("").expect("minimal config");
        assert!(
            cfg.model_aliases.is_empty(),
            "model_aliases defaults to empty when the table is absent"
        );
        // An unknown name resolves to itself.
        assert_eq!(cfg.resolve_model_alias("gpt-4o"), "gpt-4o");
    }

    #[test]
    fn test_model_aliases_parsed() {
        let toml = r#"
[model_aliases]
fast = "gpt-4o-mini"
smart = "gpt-4o"
"#;
        let cfg = load_toml(toml).expect("should load");
        assert_eq!(cfg.model_aliases.len(), 2);
        assert_eq!(cfg.resolve_model_alias("fast"), "gpt-4o-mini");
        assert_eq!(cfg.resolve_model_alias("smart"), "gpt-4o");
        // Non-alias name passes through unchanged.
        assert_eq!(cfg.resolve_model_alias("gpt-4o"), "gpt-4o");
    }

    #[test]
    fn test_model_aliases_one_level_only() {
        // `a` points at `b`, and `b` is itself an alias. Resolution is one
        // level: `a` resolves to `b` (NOT to `c`).
        let toml = r#"
[model_aliases]
a = "b"
b = "c"
"#;
        let cfg = load_toml(toml).expect("should load");
        assert_eq!(
            cfg.resolve_model_alias("a"),
            "b",
            "one-level resolution: alias chains are NOT followed"
        );
        assert_eq!(cfg.resolve_model_alias("b"), "c");
    }

    // -----------------------------------------------------------------------
    // Bedrock native format (0.0.2, Option A2)
    // -----------------------------------------------------------------------

    #[test]
    fn test_bedrock_format_deserializes() {
        // `format = "bedrock"` round-trips to ApiFormat::Bedrock. base_url has
        // NO /v1 suffix — the URL builder appends /model/{model}/invoke.
        let toml = r#"
[[connections]]
name = "bedrock-native-eu"
base_url = "https://bedrock-runtime.eu-central-1.amazonaws.com"
api_key = "bedrock-bearer-token"
format = "bedrock"
models = ["eu.anthropic.claude-sonnet-4-6"]
"#;
        let cfg = load_toml(toml).expect("bedrock format should load");
        assert_eq!(cfg.connections[0].format, ApiFormat::Bedrock);
        assert_eq!(
            cfg.connections[0].base_url,
            "https://bedrock-runtime.eu-central-1.amazonaws.com"
        );
    }

    #[test]
    fn test_bedrock_invalid_base_url_rejected() {
        // The same absolute-http(s) URL validation applies to bedrock
        // connections; a non-URL base_url is rejected with ConfigError::Invalid.
        let toml = r#"
[[connections]]
name = "bedrock-bad"
base_url = "not a url at all"
api_key = "bedrock-bearer-token"
format = "bedrock"
"#;
        let err = load_toml(toml).expect_err("bad bedrock base_url");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("connections[bedrock-bad].base_url"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_max_body_bytes_default_applied() {
        let cfg = load_toml("").expect("minimal config");
        assert_eq!(
            cfg.server.max_body_bytes, 10_485_760,
            "default must be 10 MiB"
        );
    }

    #[test]
    fn test_max_body_bytes_custom_value() {
        let toml = r#"
[server]
max_body_bytes = 1024
"#;
        let cfg = load_toml(toml).expect("load");
        assert_eq!(cfg.server.max_body_bytes, 1024);
    }

    #[test]
    fn test_max_body_bytes_zero_rejected() {
        let toml = r#"
[server]
max_body_bytes = 0
"#;
        let err = load_toml(toml).expect_err("zero must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("max_body_bytes"),
            "error should mention field: {msg}"
        );
    }

    #[test]
    fn test_empty_lists_are_valid() {
        // Spec: empty connections/virtual_keys lists are VALID.
        let toml = r#"
[server]
bind_addr = "127.0.0.1:8080"
"#;
        load_toml(toml).expect("empty lists are valid");
    }

    // -----------------------------------------------------------------------
    // Env-var resolution
    // -----------------------------------------------------------------------

    #[test]
    fn test_env_resolution_single_ref() {
        unsafe {
            std::env::set_var("DRGTW_TEST_SINGLE_KEY", "resolved-key");
        }
        let toml = r#"
[[connections]]
name = "test-single"
base_url = "https://api.example.com"
api_key = "${DRGTW_TEST_SINGLE_KEY}"
format = "open_ai"
"#;
        let cfg = load_toml(toml).expect("load");
        assert_eq!(cfg.connections[0].api_key, "resolved-key");
        unsafe {
            std::env::remove_var("DRGTW_TEST_SINGLE_KEY");
        }
    }

    #[test]
    fn test_env_resolution_multiple_refs_in_one_string() {
        unsafe {
            std::env::set_var("DRGTW_TEST_MULTI_A", "hello");
            std::env::set_var("DRGTW_TEST_MULTI_B", "world");
        }
        let toml = r#"
[[connections]]
name = "test-multi"
base_url = "https://api.example.com"
api_key = "${DRGTW_TEST_MULTI_A}-${DRGTW_TEST_MULTI_B}"
format = "open_ai"
"#;
        let cfg = load_toml(toml).expect("load");
        assert_eq!(cfg.connections[0].api_key, "hello-world");
        unsafe {
            std::env::remove_var("DRGTW_TEST_MULTI_A");
            std::env::remove_var("DRGTW_TEST_MULTI_B");
        }
    }

    #[test]
    fn test_env_resolution_literal_dollar_passthrough() {
        // A `$` not followed by `{` should pass through unchanged.
        let toml = r#"
[[connections]]
name = "test-literal-dollar"
base_url = "https://api.example.com"
api_key = "price-is-$5"
format = "open_ai"
"#;
        let cfg = load_toml(toml).expect("load");
        assert_eq!(cfg.connections[0].api_key, "price-is-$5");
    }

    #[test]
    fn test_env_resolution_base_url() {
        unsafe {
            std::env::set_var("DRGTW_TEST_BASE_HOST", "https://api.example.com");
        }
        let toml = r#"
[[connections]]
name = "test-url-env"
base_url = "${DRGTW_TEST_BASE_HOST}"
api_key = "some-key"
format = "open_ai"
"#;
        let cfg = load_toml(toml).expect("load");
        assert_eq!(cfg.connections[0].base_url, "https://api.example.com");
        unsafe {
            std::env::remove_var("DRGTW_TEST_BASE_HOST");
        }
    }

    #[test]
    fn test_env_resolution_missing_var() {
        // Make absolutely sure this var is not set.
        unsafe {
            std::env::remove_var("DRGTW_TEST_DEFINITELY_NOT_SET_XYZ123");
        }
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "${DRGTW_TEST_DEFINITELY_NOT_SET_XYZ123}"
format = "open_ai"
"#;
        let err = load_toml(toml).expect_err("should fail with missing env var");
        match err {
            ConfigError::MissingEnvVar { var, field } => {
                assert_eq!(var, "DRGTW_TEST_DEFINITELY_NOT_SET_XYZ123");
                assert_eq!(field, "connections[openai].api_key");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // Validation failures
    // -----------------------------------------------------------------------

    #[test]
    fn test_invalid_io_missing_file() {
        let err = load(Path::new("/tmp/drgtw-test-no-such-file-12345.toml"))
            .expect_err("should fail with io error");
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    #[test]
    fn test_invalid_toml_parse_error() {
        let err = load_toml("this is not valid {{ toml }}").expect_err("should fail");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn test_invalid_empty_connection_name() {
        let toml = r#"
[[connections]]
name = ""
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"
"#;
        let err = load_toml(toml).expect_err("empty name");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("name must not be empty"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_duplicate_connection_name() {
        let toml = r#"
[[connections]]
name = "dup"
base_url = "https://api.example.com"
api_key = "key1"
format = "open_ai"

[[connections]]
name = "dup"
base_url = "https://api.example.com"
api_key = "key2"
format = "open_ai"
"#;
        let err = load_toml(toml).expect_err("dup name");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("duplicate connection name"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_base_url_not_url() {
        let toml = r#"
[[connections]]
name = "bad-url"
base_url = "not a url at all"
api_key = "key"
format = "open_ai"
"#;
        let err = load_toml(toml).expect_err("bad url");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("connections[bad-url].base_url"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_base_url_wrong_scheme() {
        let toml = r#"
[[connections]]
name = "ftp-conn"
base_url = "ftp://files.example.com/path"
api_key = "key"
format = "open_ai"
"#;
        let err = load_toml(toml).expect_err("ftp scheme");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("http or https"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_base_url_has_query() {
        let toml = r#"
[[connections]]
name = "query-url"
base_url = "https://api.example.com/v1?foo=bar"
api_key = "key"
format = "open_ai"
"#;
        let err = load_toml(toml).expect_err("query in url");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("query"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_base_url_has_fragment() {
        let toml = r#"
[[connections]]
name = "frag-url"
base_url = "https://api.example.com/v1#section"
api_key = "key"
format = "open_ai"
"#;
        let err = load_toml(toml).expect_err("fragment in url");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("fragment"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_empty_api_key() {
        let toml = r#"
[[connections]]
name = "no-key"
base_url = "https://api.example.com"
api_key = ""
format = "open_ai"
"#;
        let err = load_toml(toml).expect_err("empty api_key");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("connections[no-key].api_key"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_duplicate_model_in_connection() {
        let toml = r#"
[[connections]]
name = "dup-models"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"
models = ["gpt-4", "gpt-4"]
"#;
        let err = load_toml(toml).expect_err("dup model");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("duplicate"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_empty_string_in_models() {
        let toml = r#"
[[connections]]
name = "empty-model"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"
models = ["gpt-4", ""]
"#;
        let err = load_toml(toml).expect_err("empty model string");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("empty string"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_virtual_key_missing_prefix() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-notdrgtw-something"
connections = ["conn"]
"#;
        let err = load_toml(toml).expect_err("bad vk prefix");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("sk-drgtw-"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_virtual_key_prefix_only() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-"
connections = ["conn"]
"#;
        let err = load_toml(toml).expect_err("prefix-only key");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("additional characters"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_duplicate_virtual_key() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-dupkey"
connections = ["conn"]

[[virtual_keys]]
key = "sk-drgtw-dupkey"
connections = ["conn"]
"#;
        let err = load_toml(toml).expect_err("dup vk");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("duplicate virtual key"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_virtual_key_empty_connections_list() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-emptyconn"
connections = []
"#;
        let err = load_toml(toml).expect_err("empty connections");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("empty connections list"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_virtual_key_unknown_connection() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-unknown"
connections = ["conn", "does-not-exist"]
"#;
        let err = load_toml(toml).expect_err("unknown conn");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("unknown connection `does-not-exist`"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_virtual_key_empty_models_allowlist() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-emptymodels"
connections = ["conn"]
models = []
"#;
        let err = load_toml(toml).expect_err("empty models");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("empty models allowlist"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_valid_virtual_key_no_models_allowlist() {
        // models = None means allow all — must be valid.
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-allmodels"
connections = ["conn"]
"#;
        let cfg = load_toml(toml).expect("no models = all models is valid");
        assert!(cfg.virtual_keys[0].models.is_none());
    }

    #[test]
    fn test_valid_virtual_key_with_models_allowlist() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-withmodels"
connections = ["conn"]
models = ["gpt-4o"]
"#;
        let cfg = load_toml(toml).expect("non-empty models allowlist is valid");
        let models = cfg.virtual_keys[0]
            .models
            .as_deref()
            .expect("models present");
        assert_eq!(models, &["gpt-4o"]);
    }

    // -----------------------------------------------------------------------
    // WP 2.2 — wildcard model patterns
    // -----------------------------------------------------------------------

    #[test]
    fn test_valid_wildcard_in_connection_models() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "key"
format = "open_ai"
models = ["gpt-4o", "gpt-*"]
"#;
        let cfg = load_toml(toml).expect("trailing wildcard is valid");
        assert_eq!(cfg.connections[0].models, vec!["gpt-4o", "gpt-*"]);
    }

    #[test]
    fn test_valid_match_all_wildcard_in_connection_models() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "key"
format = "open_ai"
models = ["*"]
"#;
        let cfg = load_toml(toml).expect("match-all wildcard is valid");
        assert_eq!(cfg.connections[0].models, vec!["*"]);
    }

    #[test]
    fn test_invalid_wildcard_non_terminal_in_connection_models() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "key"
format = "open_ai"
models = ["g*t"]
"#;
        let err = load_toml(toml).expect_err("non-terminal * rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("non-terminal position"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_wildcard_multiple_stars_in_connection_models() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "key"
format = "open_ai"
models = ["gpt-**"]
"#;
        let err = load_toml(toml).expect_err("double * rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("more than one"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_valid_wildcard_in_vk_models_allowlist() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-wildcardvk"
connections = ["conn"]
models = ["gpt-4o", "gpt-*"]
"#;
        let cfg = load_toml(toml).expect("wildcard in VK allowlist is valid");
        assert_eq!(
            cfg.virtual_keys[0].models.as_deref().unwrap(),
            &["gpt-4o", "gpt-*"]
        );
    }

    #[test]
    fn test_invalid_wildcard_non_terminal_in_vk_models() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-badwild"
connections = ["conn"]
models = ["g*t-4o"]
"#;
        let err = load_toml(toml).expect_err("non-terminal * in VK rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("non-terminal position"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // WP 2.3 — rate_limit config
    // -----------------------------------------------------------------------

    #[test]
    fn test_valid_rate_limit_config() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-ratelimited"
connections = ["conn"]

[virtual_keys.rate_limit]
requests = 100
per_seconds = 60
"#;
        let cfg = load_toml(toml).expect("valid rate_limit");
        let rl = cfg.virtual_keys[0]
            .rate_limit
            .as_ref()
            .expect("rate_limit present");
        assert_eq!(rl.requests, 100);
        assert_eq!(rl.per_seconds, 60);
    }

    #[test]
    fn test_valid_no_rate_limit() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-nolimit"
connections = ["conn"]
"#;
        let cfg = load_toml(toml).expect("no rate_limit is valid");
        assert!(cfg.virtual_keys[0].rate_limit.is_none());
    }

    #[test]
    fn test_invalid_rate_limit_requests_zero() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-zerorequests"
connections = ["conn"]

[virtual_keys.rate_limit]
requests = 0
per_seconds = 60
"#;
        let err = load_toml(toml).expect_err("requests=0 rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("rate_limit.requests must be > 0"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_invalid_rate_limit_per_seconds_zero() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-zeroseconds"
connections = ["conn"]

[virtual_keys.rate_limit]
requests = 10
per_seconds = 0
"#;
        let err = load_toml(toml).expect_err("per_seconds=0 rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("rate_limit.per_seconds must be > 0"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // WP 4.3 — pii.ner config
    // -----------------------------------------------------------------------

    #[test]
    fn test_ner_absent_by_default() {
        let cfg = load_toml("").expect("empty config");
        assert!(cfg.pii.ner.is_none(), "ner should be absent by default");
    }

    #[test]
    fn test_ner_happy_path_full() {
        let toml = r#"
[pii.ner]
model_dir = "models/ner-multilingual"
score_threshold = 0.7
fail_mode = "closed"
timeout_ms = 3000
workers = 4
queue_capacity = 128
"#;
        let cfg = load_toml(toml).expect("full ner config");
        let ner = cfg.pii.ner.as_ref().expect("ner present");
        assert_eq!(ner.model_dir, "models/ner-multilingual");
        assert!((ner.score_threshold - 0.7).abs() < 1e-6);
        assert_eq!(ner.fail_mode, FailMode::Closed);
        assert_eq!(ner.timeout_ms, 3000);
        assert_eq!(ner.workers, 4);
        assert_eq!(ner.queue_capacity, 128);
    }

    #[test]
    fn test_ner_defaults_applied() {
        // Only model_dir required; everything else uses defaults.
        let toml = r#"
[pii.ner]
model_dir = "models/ner"
"#;
        let cfg = load_toml(toml).expect("ner defaults");
        let ner = cfg.pii.ner.as_ref().expect("ner present");
        assert_eq!(ner.model_dir, "models/ner");
        assert!(
            (ner.score_threshold - 0.5).abs() < 1e-6,
            "default threshold 0.5"
        );
        assert_eq!(ner.fail_mode, FailMode::Open, "default fail_mode Open");
        assert_eq!(ner.timeout_ms, 5000, "default timeout_ms 5000");
        assert_eq!(ner.workers, 2, "default workers 2");
        assert_eq!(ner.queue_capacity, 64, "default queue_capacity 64");
    }

    #[test]
    fn test_ner_fail_mode_open_default_deserialization() {
        let toml = r#"
[pii.ner]
model_dir = "models/ner"
fail_mode = "open"
"#;
        let cfg = load_toml(toml).expect("fail_mode=open");
        let ner = cfg.pii.ner.as_ref().unwrap();
        assert_eq!(ner.fail_mode, FailMode::Open);
    }

    #[test]
    fn test_ner_model_dir_empty_rejected() {
        let toml = r#"
[pii.ner]
model_dir = ""
"#;
        let err = load_toml(toml).expect_err("empty model_dir rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("model_dir must not be empty"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_ner_score_threshold_below_zero_rejected() {
        let toml = r#"
[pii.ner]
model_dir = "models/ner"
score_threshold = -0.1
"#;
        let err = load_toml(toml).expect_err("negative threshold rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("score_threshold"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_ner_score_threshold_above_one_rejected() {
        let toml = r#"
[pii.ner]
model_dir = "models/ner"
score_threshold = 1.1
"#;
        let err = load_toml(toml).expect_err("threshold > 1.0 rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("score_threshold"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_ner_score_threshold_boundaries_valid() {
        // 0.0 and 1.0 are both valid boundaries.
        for threshold in ["0.0", "1.0"] {
            let toml =
                format!("[pii.ner]\nmodel_dir = \"models/ner\"\nscore_threshold = {threshold}\n");
            load_toml(&toml)
                .unwrap_or_else(|e| panic!("threshold={threshold} should be valid: {e}"));
        }
    }

    #[test]
    fn test_ner_timeout_ms_zero_rejected() {
        let toml = r#"
[pii.ner]
model_dir = "models/ner"
timeout_ms = 0
"#;
        let err = load_toml(toml).expect_err("timeout_ms=0 rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("timeout_ms must be > 0"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_ner_workers_zero_rejected() {
        let toml = r#"
[pii.ner]
model_dir = "models/ner"
workers = 0
"#;
        let err = load_toml(toml).expect_err("workers=0 rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("workers must be > 0"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_ner_queue_capacity_zero_rejected() {
        let toml = r#"
[pii.ner]
model_dir = "models/ner"
queue_capacity = 0
"#;
        let err = load_toml(toml).expect_err("queue_capacity=0 rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("queue_capacity must be > 0"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_ner_model_dir_env_var_resolved() {
        unsafe {
            std::env::set_var("DRGTW_TEST_NER_MODEL_DIR", "/data/models/ner-multilingual");
        }
        let toml = r#"
[pii.ner]
model_dir = "${DRGTW_TEST_NER_MODEL_DIR}"
"#;
        let cfg = load_toml(toml).expect("env var resolution in model_dir");
        let ner = cfg.pii.ner.as_ref().unwrap();
        assert_eq!(ner.model_dir, "/data/models/ner-multilingual");
        unsafe {
            std::env::remove_var("DRGTW_TEST_NER_MODEL_DIR");
        }
    }

    #[test]
    fn test_ner_model_dir_missing_env_var_rejected() {
        unsafe {
            std::env::remove_var("DRGTW_TEST_NER_MISSING_VAR_XYZ");
        }
        let toml = r#"
[pii.ner]
model_dir = "${DRGTW_TEST_NER_MISSING_VAR_XYZ}"
"#;
        let err = load_toml(toml).expect_err("missing env var for model_dir rejected");
        match err {
            ConfigError::MissingEnvVar { var, field } => {
                assert_eq!(var, "DRGTW_TEST_NER_MISSING_VAR_XYZ");
                assert_eq!(field, "pii.ner.model_dir");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // WP 8.1 — model_costs
    // -----------------------------------------------------------------------

    #[test]
    fn test_model_costs_absent_by_default() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "key"
format = "open_ai"
"#;
        let cfg = load_toml(toml).expect("load");
        assert!(
            cfg.connections[0].model_costs.is_empty(),
            "model_costs should default to empty"
        );
    }

    #[test]
    fn test_model_costs_exact_key_happy_path() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "key"
format = "open_ai"
models = ["gpt-4o-mini"]

[connections.model_costs."gpt-4o-mini"]
input_per_1m = 0.15
output_per_1m = 0.60
"#;
        let cfg = load_toml(toml).expect("load");
        let cost = cfg.connections[0]
            .model_costs
            .get("gpt-4o-mini")
            .expect("cost entry present");
        assert!((cost.input_per_1m - 0.15).abs() < 1e-9);
        assert!((cost.output_per_1m - 0.60).abs() < 1e-9);
    }

    #[test]
    fn test_model_costs_wildcard_key_valid() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "key"
format = "open_ai"

[connections.model_costs."gpt-*"]
input_per_1m = 0.10
output_per_1m = 0.40
"#;
        let cfg = load_toml(toml).expect("wildcard cost key valid");
        assert!(cfg.connections[0].model_costs.contains_key("gpt-*"));
    }

    #[test]
    fn test_model_costs_key_not_required_in_models_list() {
        // Cost for a model key that is NOT in the models list is valid (covers wildcard-served models).
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "key"
format = "open_ai"
models = ["gpt-4o"]

[connections.model_costs."gpt-4o-mini"]
input_per_1m = 0.15
output_per_1m = 0.60
"#;
        load_toml(toml).expect("cost key not in models list is valid");
    }

    #[test]
    fn test_model_costs_zero_values_valid() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "key"
format = "open_ai"

[connections.model_costs."free-model"]
input_per_1m = 0.0
output_per_1m = 0.0
"#;
        load_toml(toml).expect("zero cost values are valid");
    }

    #[test]
    fn test_model_costs_negative_input_rejected() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "key"
format = "open_ai"

[connections.model_costs."gpt-4o"]
input_per_1m = -0.01
output_per_1m = 0.60
"#;
        let err = load_toml(toml).expect_err("negative input_per_1m rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("input_per_1m"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_model_costs_negative_output_rejected() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "key"
format = "open_ai"

[connections.model_costs."gpt-4o"]
input_per_1m = 0.15
output_per_1m = -1.0
"#;
        let err = load_toml(toml).expect_err("negative output_per_1m rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("output_per_1m"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_model_costs_invalid_wildcard_pattern_rejected() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "key"
format = "open_ai"

[connections.model_costs."g*t"]
input_per_1m = 0.15
output_per_1m = 0.60
"#;
        let err = load_toml(toml).expect_err("non-terminal wildcard in cost key rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("non-terminal position"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_model_costs_multiple_entries() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "key"
format = "open_ai"

[connections.model_costs."gpt-4o"]
input_per_1m = 2.50
output_per_1m = 10.0

[connections.model_costs."gpt-4o-mini"]
input_per_1m = 0.15
output_per_1m = 0.60
"#;
        let cfg = load_toml(toml).expect("multiple cost entries");
        assert_eq!(cfg.connections[0].model_costs.len(), 2);
    }

    // -----------------------------------------------------------------------
    // WP 8.1 — [events]
    // -----------------------------------------------------------------------

    #[test]
    fn test_events_absent_by_default() {
        let cfg = load_toml("").expect("empty config");
        assert!(cfg.events.is_none(), "events should be absent by default");
    }

    #[test]
    fn test_events_happy_path_full() {
        let toml = r#"
[events]
url = "https://events.example.com/ingest"
auth_bearer = "tok-abc123"
buffer_size = 2048
timeout_ms = 3000
"#;
        let cfg = load_toml(toml).expect("valid events config");
        let ev = cfg.events.as_ref().expect("events present");
        assert_eq!(ev.url, "https://events.example.com/ingest");
        assert_eq!(ev.auth_bearer.as_deref(), Some("tok-abc123"));
        assert_eq!(ev.buffer_size, 2048);
        assert_eq!(ev.timeout_ms, 3000);
    }

    #[test]
    fn test_events_defaults_applied() {
        let toml = r#"
[events]
url = "https://events.example.com/ingest"
"#;
        let cfg = load_toml(toml).expect("events defaults");
        let ev = cfg.events.as_ref().expect("events present");
        assert_eq!(ev.buffer_size, 1024, "default buffer_size = 1024");
        assert_eq!(ev.timeout_ms, 5000, "default timeout_ms = 5000");
        assert!(ev.auth_bearer.is_none(), "auth_bearer defaults to None");
    }

    #[test]
    fn test_events_url_env_var_resolved() {
        unsafe {
            std::env::set_var("DRGTW_TEST_EVENTS_URL", "https://events.example.com");
        }
        let toml = r#"
[events]
url = "${DRGTW_TEST_EVENTS_URL}"
"#;
        let cfg = load_toml(toml).expect("env var in events.url");
        let ev = cfg.events.as_ref().unwrap();
        assert_eq!(ev.url, "https://events.example.com");
        unsafe {
            std::env::remove_var("DRGTW_TEST_EVENTS_URL");
        }
    }

    #[test]
    fn test_events_auth_bearer_env_var_resolved() {
        unsafe {
            std::env::set_var("DRGTW_TEST_EVENTS_TOKEN", "secret-token-xyz");
        }
        let toml = r#"
[events]
url = "https://events.example.com"
auth_bearer = "${DRGTW_TEST_EVENTS_TOKEN}"
"#;
        let cfg = load_toml(toml).expect("env var in events.auth_bearer");
        let ev = cfg.events.as_ref().unwrap();
        assert_eq!(ev.auth_bearer.as_deref(), Some("secret-token-xyz"));
        unsafe {
            std::env::remove_var("DRGTW_TEST_EVENTS_TOKEN");
        }
    }

    #[test]
    fn test_events_url_non_http_rejected() {
        let toml = r#"
[events]
url = "ftp://events.example.com"
"#;
        let err = load_toml(toml).expect_err("non-http events url rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("http or https"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_events_url_invalid_rejected() {
        let toml = r#"
[events]
url = "not a url"
"#;
        let err = load_toml(toml).expect_err("invalid events url rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("events.url"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_events_buffer_size_zero_rejected() {
        let toml = r#"
[events]
url = "https://events.example.com"
buffer_size = 0
"#;
        let err = load_toml(toml).expect_err("buffer_size=0 rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("buffer_size must be > 0"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_events_timeout_ms_zero_rejected() {
        let toml = r#"
[events]
url = "https://events.example.com"
timeout_ms = 0
"#;
        let err = load_toml(toml).expect_err("timeout_ms=0 rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("events.timeout_ms must be > 0"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // WP 8.1 — [fallback]
    // -----------------------------------------------------------------------

    #[test]
    fn test_fallback_default_enabled_true() {
        let cfg = load_toml("").expect("empty config");
        assert!(
            cfg.fallback.enabled,
            "fallback.enabled must default to true"
        );
    }

    #[test]
    fn test_fallback_can_be_disabled() {
        let toml = r#"
[fallback]
enabled = false
"#;
        let cfg = load_toml(toml).expect("fallback disabled");
        assert!(!cfg.fallback.enabled);
    }

    #[test]
    fn test_fallback_explicit_true() {
        let toml = r#"
[fallback]
enabled = true
"#;
        let cfg = load_toml(toml).expect("fallback explicit true");
        assert!(cfg.fallback.enabled);
    }

    // -----------------------------------------------------------------------
    // WP 8.1 — virtual key budget
    // -----------------------------------------------------------------------

    #[test]
    fn test_budget_absent_by_default() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-nobudget"
connections = ["conn"]
"#;
        let cfg = load_toml(toml).expect("load");
        assert!(
            cfg.virtual_keys[0].budget.is_none(),
            "budget should default to None"
        );
    }

    #[test]
    fn test_budget_happy_path() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-withbudget"
connections = ["conn"]

[virtual_keys.budget]
max_usd = 10.0
per_seconds = 86400
"#;
        let cfg = load_toml(toml).expect("valid budget config");
        let budget = cfg.virtual_keys[0].budget.as_ref().expect("budget present");
        assert!((budget.max_usd - 10.0).abs() < 1e-9);
        assert_eq!(budget.per_seconds, 86400);
    }

    #[test]
    fn test_budget_max_usd_zero_rejected() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-zerobudget"
connections = ["conn"]

[virtual_keys.budget]
max_usd = 0.0
per_seconds = 3600
"#;
        let err = load_toml(toml).expect_err("max_usd=0 rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("budget.max_usd"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_budget_max_usd_negative_rejected() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-negbudget"
connections = ["conn"]

[virtual_keys.budget]
max_usd = -5.0
per_seconds = 3600
"#;
        let err = load_toml(toml).expect_err("negative max_usd rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("budget.max_usd"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_budget_per_seconds_zero_rejected() {
        let toml = r#"
[[connections]]
name = "conn"
base_url = "https://api.example.com"
api_key = "key"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-zerosec"
connections = ["conn"]

[virtual_keys.budget]
max_usd = 10.0
per_seconds = 0
"#;
        let err = load_toml(toml).expect_err("per_seconds=0 rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("budget.per_seconds"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // WP 9.1 — pii.vault config
    // -----------------------------------------------------------------------

    #[test]
    fn test_vault_absent_by_default() {
        let cfg = load_toml("").expect("empty config");
        assert!(cfg.pii.vault.is_none(), "vault should be absent by default");
    }

    #[test]
    fn test_vault_happy_path() {
        let key = "a".repeat(64);
        let toml = format!("[pii.vault]\npath = \"vault.db\"\nkey = \"{key}\"\n");
        let cfg = load_toml(&toml).expect("valid vault config");
        let vault = cfg.pii.vault.as_ref().expect("vault present");
        assert_eq!(vault.path, "vault.db");
        assert_eq!(vault.key, key);
    }

    #[test]
    fn test_vault_key_env_var_resolved() {
        let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        unsafe {
            std::env::set_var("DRGTW_TEST_VAULT_KEY_OK", key);
        }
        let toml = r#"
[pii.vault]
path = "vault.db"
key = "${DRGTW_TEST_VAULT_KEY_OK}"
"#;
        let cfg = load_toml(toml).expect("env-resolved vault key");
        assert_eq!(cfg.pii.vault.as_ref().unwrap().key, key);
        unsafe {
            std::env::remove_var("DRGTW_TEST_VAULT_KEY_OK");
        }
    }

    #[test]
    fn test_vault_key_missing_env_var_rejected() {
        unsafe {
            std::env::remove_var("DRGTW_TEST_VAULT_MISSING_XYZ");
        }
        let toml = r#"
[pii.vault]
path = "vault.db"
key = "${DRGTW_TEST_VAULT_MISSING_XYZ}"
"#;
        let err = load_toml(toml).expect_err("missing env var rejected");
        match err {
            ConfigError::MissingEnvVar { var, field } => {
                assert_eq!(var, "DRGTW_TEST_VAULT_MISSING_XYZ");
                assert_eq!(field, "pii.vault.key");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_vault_path_empty_rejected() {
        let key = "a".repeat(64);
        let toml = format!("[pii.vault]\npath = \"\"\nkey = \"{key}\"\n");
        let err = load_toml(&toml).expect_err("empty path rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("pii.vault.path"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_vault_key_wrong_length_rejected() {
        // 63 hex chars — one short.
        let key = "a".repeat(63);
        let toml = format!("[pii.vault]\npath = \"vault.db\"\nkey = \"{key}\"\n");
        let err = load_toml(&toml).expect_err("short key rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("64 hex characters"), "{msg}");
                // Must NOT echo the key material.
                assert!(!msg.contains(&key), "error must not leak key material");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_vault_key_non_hex_rejected() {
        // 64 chars but contains non-hex characters.
        let key = "z".repeat(64);
        let toml = format!("[pii.vault]\npath = \"vault.db\"\nkey = \"{key}\"\n");
        let err = load_toml(&toml).expect_err("non-hex key rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("64 hex characters"), "{msg}");
                assert!(!msg.contains(&key), "error must not leak key material");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_vault_key_uppercase_hex_valid() {
        let key = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789";
        let toml = format!("[pii.vault]\npath = \"vault.db\"\nkey = \"{key}\"\n");
        let cfg = load_toml(&toml).expect("uppercase hex is valid");
        assert_eq!(cfg.pii.vault.as_ref().unwrap().key, key);
    }

    // -----------------------------------------------------------------------
    // WP-A — mcp_servers config
    // -----------------------------------------------------------------------

    #[test]
    fn test_mcp_servers_absent_by_default() {
        let cfg = load_toml("").expect("empty config");
        assert!(
            cfg.mcp_servers.is_empty(),
            "mcp_servers should default to empty"
        );
    }

    #[test]
    fn test_mcp_servers_happy_path_full() {
        unsafe {
            std::env::set_var("DRGTW_TEST_MCP_TOKEN", "ctx7-secret");
            std::env::set_var("DRGTW_TEST_MCP_HEADER", "header-val");
        }
        let toml = r#"
[mcp_servers.context7]
url = "https://mcp.context7.com/mcp"
description = "library docs"
auth_type = "bearer"
auth_value = "${DRGTW_TEST_MCP_TOKEN}"

[mcp_servers.context7.extra_headers]
X-Tenant = "${DRGTW_TEST_MCP_HEADER}"
X-Static = "literal"

[mcp_servers.internal-tools]
url = "https://tools.example.com/mcp"
auth_type = "api_key"
auth_value = "plain-key"
"#;
        let cfg = load_toml(toml).expect("should load");
        assert_eq!(cfg.mcp_servers.len(), 2);

        let ctx7 = cfg.mcp_servers.get("context7").expect("context7 present");
        assert_eq!(ctx7.url, "https://mcp.context7.com/mcp");
        assert_eq!(ctx7.description.as_deref(), Some("library docs"));
        assert_eq!(ctx7.auth_type, McpAuthType::Bearer);
        assert_eq!(ctx7.auth_value.as_deref(), Some("ctx7-secret"));
        assert_eq!(
            ctx7.extra_headers.get("X-Tenant").map(String::as_str),
            Some("header-val")
        );
        assert_eq!(
            ctx7.extra_headers.get("X-Static").map(String::as_str),
            Some("literal")
        );

        let internal = cfg
            .mcp_servers
            .get("internal-tools")
            .expect("internal-tools present");
        assert_eq!(internal.auth_type, McpAuthType::ApiKey);
        assert_eq!(internal.auth_value.as_deref(), Some("plain-key"));
        assert!(internal.description.is_none());
        assert!(internal.extra_headers.is_empty());

        unsafe {
            std::env::remove_var("DRGTW_TEST_MCP_TOKEN");
            std::env::remove_var("DRGTW_TEST_MCP_HEADER");
        }
    }

    #[test]
    fn test_mcp_servers_auth_type_defaults_to_none() {
        let toml = r#"
[mcp_servers.public]
url = "https://mcp.example.com/mcp"
"#;
        let cfg = load_toml(toml).expect("auth_type defaults to none");
        let server = cfg.mcp_servers.get("public").unwrap();
        assert_eq!(server.auth_type, McpAuthType::None);
        assert!(server.auth_value.is_none());
    }

    #[test]
    fn test_mcp_servers_invalid_name_chars_rejected() {
        let toml = r#"
[mcp_servers."bad name!"]
url = "https://mcp.example.com/mcp"
"#;
        let err = load_toml(toml).expect_err("invalid name chars rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("mcp_servers name"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_mcp_servers_invalid_url_rejected() {
        let toml = r#"
[mcp_servers.bad-url]
url = "not a url"
"#;
        let err = load_toml(toml).expect_err("invalid url rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("mcp_servers[bad-url].url"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_mcp_servers_bearer_without_auth_value_rejected() {
        let toml = r#"
[mcp_servers.needsauth]
url = "https://mcp.example.com/mcp"
auth_type = "bearer"
"#;
        let err = load_toml(toml).expect_err("bearer without auth_value rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("auth_value"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_mcp_servers_bearer_empty_auth_value_rejected() {
        let toml = r#"
[mcp_servers.needsauth]
url = "https://mcp.example.com/mcp"
auth_type = "bearer"
auth_value = ""
"#;
        let err = load_toml(toml).expect_err("empty auth_value rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("auth_value"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_mcp_servers_auth_value_with_none_rejected() {
        let toml = r#"
[mcp_servers.public]
url = "https://mcp.example.com/mcp"
auth_type = "none"
auth_value = "should-not-be-here"
"#;
        let err = load_toml(toml).expect_err("auth_value with none rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("auth_value"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_mcp_servers_bad_header_name_rejected() {
        let toml = r#"
[mcp_servers.srv]
url = "https://mcp.example.com/mcp"

[mcp_servers.srv.extra_headers]
"Bad Header" = "value"
"#;
        let err = load_toml(toml).expect_err("bad header name rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("Bad Header"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_mcp_servers_missing_env_var_in_auth_value() {
        unsafe {
            std::env::remove_var("DRGTW_TEST_MCP_MISSING_XYZ");
        }
        let toml = r#"
[mcp_servers.srv]
url = "https://mcp.example.com/mcp"
auth_type = "bearer"
auth_value = "${DRGTW_TEST_MCP_MISSING_XYZ}"
"#;
        let err = load_toml(toml).expect_err("missing env var in auth_value rejected");
        match err {
            ConfigError::MissingEnvVar { var, field } => {
                assert_eq!(var, "DRGTW_TEST_MCP_MISSING_XYZ");
                assert_eq!(field, "mcp_servers[srv].auth_value");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_mcp_servers_auth_value_with_control_char_rejected() {
        let toml = r#"
[mcp_servers.srv]
url = "https://mcp.example.com/mcp"
auth_type = "bearer"
auth_value = "abc\ndef"
"#;
        let err = load_toml(toml).expect_err("auth_value with control char rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("mcp_servers[srv].auth_value"), "{msg}");
                assert!(msg.contains("control characters"), "{msg}");
                assert!(!msg.contains("abc"), "must not echo secret value: {msg}");
                assert!(!msg.contains("def"), "must not echo secret value: {msg}");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_mcp_servers_extra_header_value_with_control_char_rejected() {
        let toml = r#"
[mcp_servers.srv]
url = "https://mcp.example.com/mcp"

[mcp_servers.srv.extra_headers]
X-Tenant = "good\rbad"
"#;
        let err = load_toml(toml).expect_err("extra_headers value with control char rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(
                    msg.contains("mcp_servers[srv].extra_headers[X-Tenant]"),
                    "{msg}"
                );
                assert!(msg.contains("control characters"), "{msg}");
                assert!(!msg.contains("good"), "must not echo value: {msg}");
                assert!(!msg.contains("bad"), "must not echo value: {msg}");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // Tracing config
    // -----------------------------------------------------------------------

    #[test]
    fn test_tracing_defaults_when_absent() {
        let cfg = load_toml("").expect("empty config");
        assert!(cfg.tracing.enabled, "tracing.enabled must default to true");
        assert_eq!(cfg.tracing.dir, "traces");
        assert_eq!(cfg.tracing.retention_days, 90);
        assert_eq!(cfg.tracing.rotate_max_bytes, 52_428_800);
        assert_eq!(cfg.tracing.archive_after_files, 10);
    }

    #[test]
    fn test_tracing_explicit_values_parse() {
        let toml = r#"
[tracing]
enabled = false
retention_days = 7
"#;
        let cfg = load_toml(toml).expect("tracing section parses");
        assert!(!cfg.tracing.enabled);
        assert_eq!(cfg.tracing.retention_days, 7);
        // unspecified fields keep their defaults
        assert_eq!(cfg.tracing.dir, "traces");
        assert_eq!(cfg.tracing.rotate_max_bytes, 52_428_800);
        assert_eq!(cfg.tracing.archive_after_files, 10);
    }

    // -----------------------------------------------------------------------
    // OTel config (0.0.2)
    // -----------------------------------------------------------------------

    #[test]
    fn test_otel_defaults_when_absent() {
        let cfg = load_toml("").expect("empty config");
        assert!(!cfg.otel.enabled, "otel.enabled must default to false");
        assert_eq!(cfg.otel.endpoint, "http://localhost:4317");
        assert_eq!(cfg.otel.protocol, OtelProtocol::Grpc);
        assert_eq!(cfg.otel.service_name, "drgtw");
        assert!(cfg.otel.traces);
        assert!(cfg.otel.metrics);
        assert_eq!(cfg.otel.sample_ratio, 1.0);
        assert_eq!(cfg.otel.export_interval_ms, 10_000);
        assert_eq!(cfg.otel.export_timeout_ms, 5_000);
        assert!(
            !cfg.otel.metrics_include_key_id,
            "key_id must be off metrics by default (cardinality)"
        );
    }

    #[test]
    fn test_otel_full_section_parses() {
        let toml = r#"
[otel]
enabled = true
endpoint = "http://otel-collector.example.com:4317"
protocol = "grpc"
service_name = "example-gateway"
traces = true
metrics = false
sample_ratio = 0.25
export_interval_ms = 2000
export_timeout_ms = 1000
metrics_include_key_id = true
"#;
        let cfg = load_toml(toml).expect("full otel section parses");
        assert!(cfg.otel.enabled);
        assert_eq!(cfg.otel.endpoint, "http://otel-collector.example.com:4317");
        assert_eq!(cfg.otel.protocol, OtelProtocol::Grpc);
        assert_eq!(cfg.otel.service_name, "example-gateway");
        assert!(cfg.otel.traces);
        assert!(!cfg.otel.metrics);
        assert_eq!(cfg.otel.sample_ratio, 0.25);
        assert_eq!(cfg.otel.export_interval_ms, 2000);
        assert_eq!(cfg.otel.export_timeout_ms, 1000);
        assert!(cfg.otel.metrics_include_key_id);
    }

    #[test]
    fn test_otel_protocol_http_round_trips() {
        let toml = r#"
[otel]
protocol = "http"
"#;
        let cfg = load_toml(toml).expect("http protocol parses");
        assert_eq!(cfg.otel.protocol, OtelProtocol::Http);
    }

    #[test]
    fn test_otel_disabled_invalid_endpoint_ok() {
        // A disabled section must never fail boot, even with a bogus endpoint.
        let toml = r#"
[otel]
enabled = false
endpoint = "not a url"
sample_ratio = 9.0
"#;
        let cfg = load_toml(toml).expect("disabled otel never validated");
        assert!(!cfg.otel.enabled);
    }

    #[test]
    fn test_otel_enabled_sample_ratio_too_high_rejected() {
        let toml = r#"
[otel]
enabled = true
endpoint = "http://otel-collector.example.com:4317"
sample_ratio = 1.5
"#;
        let err = load_toml(toml).expect_err("sample_ratio > 1.0 rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("sample_ratio"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_otel_enabled_sample_ratio_negative_rejected() {
        let toml = r#"
[otel]
enabled = true
endpoint = "http://otel-collector.example.com:4317"
sample_ratio = -0.1
"#;
        let err = load_toml(toml).expect_err("negative sample_ratio rejected");
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn test_otel_enabled_empty_endpoint_rejected() {
        let toml = r#"
[otel]
enabled = true
endpoint = ""
"#;
        let err = load_toml(toml).expect_err("empty endpoint rejected when enabled");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("endpoint"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_otel_enabled_invalid_endpoint_rejected() {
        let toml = r#"
[otel]
enabled = true
endpoint = "ftp://nope"
"#;
        let err = load_toml(toml).expect_err("non-http endpoint rejected when enabled");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("otel.endpoint"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_otel_enabled_zero_interval_rejected() {
        let toml = r#"
[otel]
enabled = true
endpoint = "http://otel-collector.example.com:4317"
export_interval_ms = 0
"#;
        let err = load_toml(toml).expect_err("zero interval rejected");
        assert!(matches!(err, ConfigError::Invalid(_)));
    }
}
