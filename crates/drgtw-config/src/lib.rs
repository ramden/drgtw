//! DRGTW configuration: TOML schema, env-var resolution, validation.
//!
//! Public API contract (Phase 0 / WP 0.2). The types below are the agreed
//! cross-crate interface — extend, but do not break, without a lead decision.

pub mod edit;
pub use edit::{
    read_document, restart_required_changes, set_value, validate_str, write_safe, FieldError,
};

use std::collections::{BTreeMap, HashMap};
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
    /// Embedded admin web UI (concept). Off by default; mounted at `/ui` by the
    /// binary only when `enabled`. Presence of `[ui.history]` unlocks the
    /// history/audit nav — the concept opens no Postgres connection.
    #[serde(default)]
    pub ui: UiConfig,
    /// Content guardrails applied to request/response text (pre/post upstream).
    /// Defaults to empty (no guardrails). See [`GuardrailsConfig`].
    #[serde(default)]
    pub guardrails: GuardrailsConfig,
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

    // --- SigV4 credentials (only meaningful for `bedrock_converse`) ---
    /// AWS region, e.g. `"eu-central-1"`. Required when SigV4 creds are present.
    /// Supports `${ENV_VAR}` references, resolved at load.
    #[serde(default)]
    pub region: Option<String>,
    /// SigV4 access key id. `${ENV_VAR}` expanded at load.
    #[serde(default)]
    pub aws_access_key_id: Option<String>,
    /// SigV4 secret access key. `${ENV_VAR}` expanded at load.
    #[serde(default)]
    pub aws_secret_access_key: Option<String>,
    /// Optional STS session token. `${ENV_VAR}` expanded at load.
    #[serde(default)]
    pub aws_session_token: Option<String>,
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
    /// AWS Bedrock Converse / ConverseStream over the OpenAI client surface
    /// (`/v1/chat/completions`). Auth is SigV4 (static creds) or bearer
    /// (`api_key`). The URL builder appends `/model/{model}/converse[-stream]`,
    /// so the `base_url` carries NO `/v1` suffix.
    BedrockConverse,
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
    /// Optional MCP server allowlist for this key. `None` = all configured servers.
    /// Each entry must name a key in `config.mcp_servers`; validated at load time.
    #[serde(default)]
    pub mcp_servers: Option<Vec<String>>,
    /// When true, this key may bypass PII scanning per request by sending
    /// `x-drgtw-pii: off`. Keys without this flag have the bypass header
    /// ignored (fail-closed: PII still scans). Defaults to `false`.
    #[serde(default)]
    pub allow_pii_bypass: bool,
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
    /// Optional allow-list of entity kinds to keep. Names are presidio-style and
    /// case-insensitive: `PERSON`, `LOCATION`, `ORGANIZATION`, `EMAIL_ADDRESS`
    /// (alias `EMAIL`), `PHONE_NUMBER` (alias `PHONE`), `CREDIT_CARD` (alias
    /// `CC`), `IBAN_CODE` (alias `IBAN`), `IP_ADDRESS`, `DATE_TIME` (alias
    /// `DATE`), `NATIONAL_ID`, `NRP`, plus any `custom_recognizers` name.
    ///
    /// When absent (`None`), every detected kind is kept (backward compatible).
    /// When present, detections are filtered to the listed kinds **after** the
    /// recognizers run — recognizers still execute, only their output is
    /// filtered. An empty list is rejected at validation (use `disabled_pii`
    /// semantics instead of an empty allow-list).
    #[serde(default)]
    pub entities: Option<Vec<String>>,
    /// Optional NER (named-entity recognition) configuration. When absent,
    /// NER is not loaded. When present, `model_dir` is required.
    #[serde(default)]
    pub ner: Option<NerConfig>,
    /// Optional persistent encrypted entity vault (WP 9.1). When absent, the
    /// vault is off and placeholder mappings are per-request only. When present,
    /// both `path` and `key` are required.
    #[serde(default)]
    pub vault: Option<VaultConfig>,
    /// Require the persistent vault for `/v1/embeddings`. When `true` and no
    /// `[pii.vault]` is configured, embeddings requests that engage the PII
    /// pipeline are rejected rather than served with per-request placeholders
    /// (which are inconsistent across requests and break embedding-index/RAG
    /// consistency). Defaults to `false` — the vault may be intentionally
    /// absent in development.
    #[serde(default)]
    pub embeddings_require_vault: bool,
    /// Fail boot when the PII pipeline is enabled but no `[pii.ner]` model is
    /// configured. Defaults to `false` (a boot *warning* is logged instead).
    ///
    /// Set `true` for GDPR-grade deployments that must never start in a state
    /// where person/organization/location names reach the upstream provider in
    /// clear text: with `enabled_by_default = true` and no NER model, boot is
    /// rejected rather than silently masking only structured identifiers. This
    /// is the hard-fail counterpart to [`PiiConfig::names_leak_without_ner`].
    #[serde(default)]
    pub require_ner: bool,
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

/// Embedded admin UI configuration (concept).
///
/// The basic UI tier runs with zero persistence. The optional `[ui.history]`
/// section unlocks the history/audit nav — its presence is the only signal the
/// UI uses; the concept never opens a Postgres connection.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UiConfig {
    /// Mount the UI under `/ui`. Defaults to `false` — opt-in, like `[otel]`.
    #[serde(default)]
    pub enabled: bool,
    /// Optional request-history backend. When present, the history/audit pages
    /// appear unlocked; when absent they render a locked empty state.
    #[serde(default)]
    pub history: Option<UiHistoryConfig>,
    /// Optional login/session auth for the admin UI.
    ///
    /// When absent, the UI is open (current behaviour). When present, all `/ui`
    /// routes except `/ui/login` and `/ui/assets/*` require a valid session cookie.
    #[serde(default)]
    pub auth: Option<UiAuthConfig>,
}

/// Login + session-cookie auth for the admin UI.
///
/// Presence of this section enables authentication on every `/ui` route
/// except the login page and static assets. All fields are required.
///
/// `session_key` is resolved from `${ENV_VAR}` at `load()` time; the literal
/// `${ENV_VAR}` form is accepted by the UI-mode validator without resolving.
#[derive(Debug, Clone, Deserialize)]
pub struct UiAuthConfig {
    /// Login username shown in the sidebar footer.
    pub username: String,
    /// Argon2id PHC string produced by `drgtw hash-password`. Must start with `$argon2`.
    pub password_hash: String,
    /// HMAC key used to sign session tokens and CSRF tokens.
    /// Must be non-empty after env-var resolution. Supports `${ENV_VAR}`.
    pub session_key: String,
    /// Session lifetime in hours. Default 24.
    #[serde(default = "default_session_ttl_hours")]
    pub session_ttl_hours: u64,
}

fn default_session_ttl_hours() -> u64 {
    24
}

/// Request-history backend for the UI (concept).
///
/// Required field: `postgres_url`. Follows the `VaultConfig` convention —
/// `${ENV_VAR}` references resolve at `load()` time and the value must be
/// non-empty after resolution. The concept does not connect to Postgres; the
/// section's presence merely unlocks the history/audit nav.
#[derive(Debug, Clone, Deserialize)]
pub struct UiHistoryConfig {
    /// PostgreSQL connection string. Supports `${ENV_VAR}` substitution
    /// (resolved at `load()` time). Must be non-empty after resolution.
    pub postgres_url: String,
}

impl Default for PiiConfig {
    fn default() -> Self {
        Self {
            enabled_by_default: default_pii_enabled(),
            disabled_recognizers: Vec::new(),
            custom_recognizers: Vec::new(),
            entities: None,
            ner: None,
            vault: None,
            embeddings_require_vault: false,
            require_ner: false,
        }
    }
}

impl PiiConfig {
    /// `true` when the PII pipeline is on by default but no NER model is
    /// configured — so person/organization/location names pass through to the
    /// upstream provider in clear text. The binary logs a warning on this at
    /// boot, since for GDPR-grade deployments it is almost always a
    /// misconfiguration (the most common personal data — names + locations —
    /// is the data that leaks).
    pub fn names_leak_without_ner(&self) -> bool {
        self.enabled_by_default && self.ner.is_none()
    }
}

fn default_pii_enabled() -> bool {
    true
}

/// Canonical built-in PII entity names accepted in `pii.entities` and a
/// `contact_info` guardrail's `entities` list. These are the presidio-style
/// names a deployment writes in TOML.
pub const KNOWN_PII_ENTITY_NAMES: &[&str] = &[
    "PERSON",
    "LOCATION",
    "ORGANIZATION",
    "EMAIL_ADDRESS",
    "PHONE_NUMBER",
    "CREDIT_CARD",
    "IBAN_CODE",
    "IP_ADDRESS",
    "DATE_TIME",
    "NATIONAL_ID",
    "NRP",
];

/// Map a user-supplied entity name to its canonical form (case-insensitive,
/// alias-aware), or `None` if it is not a known built-in entity.
///
/// Aliases: `EMAIL`→`EMAIL_ADDRESS`, `PHONE`→`PHONE_NUMBER`,
/// `CC`/`CARD`→`CREDIT_CARD`, `IBAN`→`IBAN_CODE`, `ORG`→`ORGANIZATION`,
/// `LOC`→`LOCATION`, `IP`→`IP_ADDRESS`, `DATE`/`DATETIME`→`DATE_TIME`.
///
/// Custom-recognizer names are validated separately (they are not built-ins).
pub fn canonical_pii_entity_name(name: &str) -> Option<&'static str> {
    let canon = match name.trim().to_ascii_uppercase().as_str() {
        "PERSON" => "PERSON",
        "LOCATION" | "LOC" => "LOCATION",
        "ORGANIZATION" | "ORG" => "ORGANIZATION",
        "EMAIL_ADDRESS" | "EMAIL" => "EMAIL_ADDRESS",
        "PHONE_NUMBER" | "PHONE" => "PHONE_NUMBER",
        "CREDIT_CARD" | "CC" | "CARD" => "CREDIT_CARD",
        "IBAN_CODE" | "IBAN" => "IBAN_CODE",
        "IP_ADDRESS" | "IP" => "IP_ADDRESS",
        "DATE_TIME" | "DATETIME" | "DATE" => "DATE_TIME",
        "NATIONAL_ID" => "NATIONAL_ID",
        "NRP" => "NRP",
        _ => return None,
    };
    Some(canon)
}

/// Content-guardrail configuration. TOML shape:
///
/// ```toml
/// [[guardrails.rules]]
/// name = "no-jailbreaks"
/// kind = "prompt_injection"   # prompt_injection | banned_content | contact_info
/// phase = "pre"               # pre | post | both   (default: kind-appropriate)
/// action = "block"            # block | redact | flag
/// patterns = ["(?i)ignore .* instructions"]   # extra regexes (banned_content/prompt_injection)
/// entities = ["EMAIL_ADDRESS", "PHONE_NUMBER"] # which kinds to act on (contact_info)
/// ```
///
/// Absent or empty `rules` = no guardrails (backward compatible).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct GuardrailsConfig {
    /// Ordered list of guardrail rules. Evaluated in order; the first `Block`
    /// short-circuits.
    #[serde(default)]
    pub rules: Vec<GuardrailRule>,
}

impl GuardrailsConfig {
    /// `true` when no guardrails are configured (engine build is skipped).
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// A single guardrail rule.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct GuardrailRule {
    /// Operator-facing name (appears in logs/traces when the rule fires).
    pub name: String,
    /// Which built-in guardrail backs this rule.
    pub kind: GuardrailKind,
    /// Whether the rule runs on the request, the response, or both.
    #[serde(default)]
    pub phase: GuardrailPhase,
    /// What to do when the rule matches.
    #[serde(default)]
    pub action: GuardrailAction,
    /// Extra regex patterns. Used by `prompt_injection` (appended to the
    /// built-in heuristics) and `banned_content` (the blocklist). Ignored by
    /// `contact_info`.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Entity kinds to act on. Used by `contact_info` (presidio-style names, as
    /// in [`PiiConfig::entities`]). Ignored by the other kinds. Empty = a
    /// sensible default set chosen by the guardrail.
    #[serde(default)]
    pub entities: Vec<String>,
}

/// Built-in guardrail kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailKind {
    /// Heuristic prompt-injection / jailbreak detection on request text.
    PromptInjection,
    /// Operator-defined blocklist (regex) for NSFW / disallowed content.
    BannedContent,
    /// Contact-info / national-identifier detection (reuses the PII detectors).
    ContactInfo,
}

/// When a guardrail rule runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailPhase {
    /// On the request, before the upstream call. Default.
    #[default]
    Pre,
    /// On the (non-streaming) response, after the upstream call.
    Post,
    /// On both request and response.
    Both,
}

/// What a guardrail does when it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailAction {
    /// Reject the request/response with a content-filter error. Default.
    #[default]
    Block,
    /// Replace the matching spans with a placeholder and continue.
    Redact,
    /// Log/trace the match and continue unchanged.
    Flag,
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
    /// Restrict NER scanning to chat messages of these roles. Role names are
    /// matched case-insensitively against the message `role` field
    /// (`system`, `user`, `assistant`, `developer`, `tool`). For the Anthropic
    /// top-level `system` field the role is treated as `system`.
    ///
    /// When absent (`None`, the default), NER runs on every role — backward
    /// compatible. When present, the NER model is only invoked for the listed
    /// roles; the cheap regex recognizers (email, phone, IBAN, …) still run on
    /// **every** role regardless of this setting. This lets deployments skip
    /// NER on a large, static, PII-free system prompt without weakening
    /// structured-identifier masking anywhere.
    ///
    /// An empty list is rejected at validation (it would disable NER on every
    /// role — use `[pii]` `disabled_recognizers`/omit `[pii.ner]` for that).
    #[serde(default)]
    pub scan_roles: Option<Vec<String>>,
    /// Capacity of the in-memory NER verdict cache, counted in distinct input
    /// texts. `0` (the default) disables the cache. When `> 0`, NER results for
    /// byte-identical input text are reused across requests, up to this many
    /// entries, with least-recently-used eviction.
    ///
    /// The cache key is a 128-bit hash of the input text; **no plaintext is
    /// retained** — only span offsets/kinds/scores are stored. This makes the
    /// cache safe to enable for repeated content such as a static system prompt
    /// or a large unchanged conversation prefix.
    #[serde(default = "default_ner_cache_capacity")]
    pub cache_capacity: usize,
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
fn default_ner_cache_capacity() -> usize {
    0
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
    /// Optional HMAC signing secret for webhook delivery. When set, the sink
    /// adds a `X-Drgtw-Signature` header to each POST. Supports `${ENV_VAR}`.
    #[serde(default)]
    pub signing_secret: Option<String>,
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
    /// Extra resource attributes merged into the exported OTLP `Resource`,
    /// in addition to `service.name`/`service.version`. Use this to set
    /// vendor attributes such as `openinference.project.name` (Phoenix routes
    /// spans to a project via that key). At init these are overridden, per key,
    /// by the standard `OTEL_RESOURCE_ATTRIBUTES` env (`k=v,k2=v2`); and
    /// `PHOENIX_PROJECT_NAME`, if set, overrides `openinference.project.name`.
    /// Default empty.
    pub resource_attributes: BTreeMap<String, String>,
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
            resource_attributes: BTreeMap::new(),
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
    /// Inbound request header names (case-insensitive) to forward to this
    /// upstream. Empty by default — nothing is forwarded (safe default).
    /// Names are stored lowercased. Example: `["x-trace-id", "x-tenant"]`.
    #[serde(default)]
    pub forward_headers: Vec<String>,
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
    load_strict(path, false)
}

/// Like [`load`], but controls how **unknown** TOML keys are handled.
///
/// drgtw's config is not `deny_unknown_fields`, so a misplaced or misspelled
/// key (e.g. a `[ner]` table written instead of `[pii.ner]`, or `score_threshold`
/// at the `[pii]` level instead of under `[pii.ner]`) would otherwise be
/// silently ignored — a dangerous failure mode for PII settings, where a
/// dropped key means data leaks unmasked.
///
/// - `strict = false` (the default via [`load`]): unknown keys are logged as a
///   `warn!` and loading proceeds.
/// - `strict = true` (the `--strict-config` flag): any unknown key is a hard
///   [`ConfigError::Invalid`] and boot fails.
///
/// Key paths are reported dotted, e.g. `pii.scrore_threshold` or `ner.model_dir`.
pub fn load_strict(path: &Path, strict: bool) -> Result<Config, ConfigError> {
    let path_str = path.display().to_string();

    // 1. Read file.
    let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path_str.clone(),
        source: e,
    })?;

    // 2. Parse TOML, collecting any keys that no field consumed.
    let mut unknown_keys: Vec<String> = Vec::new();
    let de = toml::Deserializer::new(&raw);
    let mut config: Config = serde_ignored::deserialize(de, |key_path| {
        unknown_keys.push(key_path.to_string());
    })
    .map_err(|e| ConfigError::Parse {
        path: path_str.clone(),
        source: e,
    })?;

    // 2b. Surface unknown keys. Warn by default; hard-fail under --strict-config.
    if !unknown_keys.is_empty() {
        let joined = unknown_keys.join(", ");
        if strict {
            return Err(ConfigError::Invalid(format!(
                "unknown config key(s) in `{path_str}`: {joined} (rejected by --strict-config)"
            )));
        }
        tracing::warn!(
            keys = %joined,
            "ignoring unknown config key(s) — check for typos or misplaced sections; \
             run with --strict-config to make this a hard error"
        );
    }

    // 3. Env-var resolution on connections.
    for conn in &mut config.connections {
        let field_api_key = format!("connections[{}].api_key", conn.name);
        conn.api_key = resolve_env_vars(&conn.api_key, &field_api_key)?;

        let field_base_url = format!("connections[{}].base_url", conn.name);
        conn.base_url = resolve_env_vars(&conn.base_url, &field_base_url)?;

        // SigV4 credential fields (only meaningful for bedrock_converse).
        if let Some(region) = &conn.region {
            let field = format!("connections[{}].region", conn.name);
            conn.region = Some(resolve_env_vars(region, &field)?);
        }
        if let Some(akid) = &conn.aws_access_key_id {
            let field = format!("connections[{}].aws_access_key_id", conn.name);
            conn.aws_access_key_id = Some(resolve_env_vars(akid, &field)?);
        }
        if let Some(secret) = &conn.aws_secret_access_key {
            let field = format!("connections[{}].aws_secret_access_key", conn.name);
            conn.aws_secret_access_key = Some(resolve_env_vars(secret, &field)?);
        }
        if let Some(token) = &conn.aws_session_token {
            let field = format!("connections[{}].aws_session_token", conn.name);
            conn.aws_session_token = Some(resolve_env_vars(token, &field)?);
        }
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

    // 3e. Env-var resolution on ui.history.postgres_url.
    if let Some(history) = &mut config.ui.history {
        history.postgres_url =
            resolve_env_vars(&history.postgres_url, "ui.history.postgres_url")?;
    }

    // 3f. Env-var resolution on ui.auth.session_key.
    if let Some(auth) = &mut config.ui.auth {
        auth.session_key = resolve_env_vars(&auth.session_key, "ui.auth.session_key")?;
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
///
/// Delegates to [`edit::validate_inner`] with `ui_mode = false` so the full
/// set of checks (including the 64-hex vault key rule) is enforced.
fn validate(config: &Config) -> Result<(), ConfigError> {
    edit::validate_inner(config, false)
}

/// Validate a single model pattern entry.
///
/// Rules:
/// - Must not be empty.
/// - `*` is only allowed as the **last** character.
/// - At most one `*` in the entire string.
#[allow(dead_code)]
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
#[allow(dead_code)]
fn validate_base_url(url_str: &str, conn_name: &str) -> Result<(), ConfigError> {
    let field = format!("connections[{}].base_url", conn_name);
    validate_absolute_http_url(url_str, &field)
}

/// Check that a URL string is an absolute http(s) URL with no query or fragment.
/// `field` is used in error messages (e.g. `"events.url"`).
#[allow(dead_code)]
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

    // Helper: write TOML to a temp file and call load_strict().
    fn load_toml_strict(content: &str, strict: bool) -> Result<Config, ConfigError> {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(content.as_bytes()).expect("write");
        load_strict(f.path(), strict)
    }

    // -----------------------------------------------------------------------
    // PII entities allow-list (v0.0.8)
    // -----------------------------------------------------------------------

    #[test]
    fn test_pii_entities_absent_defaults_none() {
        let cfg = load_toml("[pii]\nenabled_by_default = true\n").expect("valid");
        assert!(cfg.pii.entities.is_none());
    }

    #[test]
    fn test_pii_entities_valid_subset_with_aliases() {
        let cfg = load_toml(
            "[pii]\nentities = [\"PERSON\", \"EMAIL\", \"ip_address\", \"DATE\"]\n",
        )
        .expect("valid");
        assert_eq!(
            cfg.pii.entities.as_deref(),
            Some(["PERSON", "EMAIL", "ip_address", "DATE"].map(String::from).as_slice())
        );
    }

    #[test]
    fn test_pii_entities_invalid_name_rejected() {
        let err = load_toml("[pii]\nentities = [\"PERSON\", \"NONSENSE\"]\n").unwrap_err();
        match err {
            ConfigError::Invalid(m) => assert!(m.contains("NONSENSE"), "{m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn test_pii_entities_empty_list_rejected() {
        let err = load_toml("[pii]\nentities = []\n").unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn test_pii_entities_accepts_custom_recognizer_name() {
        let toml = "[pii]\nentities = [\"PERSON\", \"ticket\"]\n\
                    [[pii.custom_recognizers]]\nname = \"ticket\"\npattern = \"TKT-\\\\d+\"\n";
        let cfg = load_toml(toml).expect("custom recognizer name is a valid entity");
        assert!(cfg.pii.entities.is_some());
    }

    #[test]
    fn test_canonical_pii_entity_name_aliases() {
        assert_eq!(canonical_pii_entity_name("email"), Some("EMAIL_ADDRESS"));
        assert_eq!(canonical_pii_entity_name("CC"), Some("CREDIT_CARD"));
        assert_eq!(canonical_pii_entity_name("org"), Some("ORGANIZATION"));
        assert_eq!(canonical_pii_entity_name("IP"), Some("IP_ADDRESS"));
        assert_eq!(canonical_pii_entity_name("date_time"), Some("DATE_TIME"));
        assert_eq!(canonical_pii_entity_name("unknown"), None);
    }

    // -----------------------------------------------------------------------
    // require_ner (v0.0.8)
    // -----------------------------------------------------------------------

    #[test]
    fn test_require_ner_without_ner_block_rejected() {
        let err = load_toml("[pii]\nenabled_by_default = true\nrequire_ner = true\n").unwrap_err();
        match err {
            ConfigError::Invalid(m) => assert!(m.contains("require_ner"), "{m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn test_require_ner_with_ner_block_ok() {
        let toml = "[pii]\nrequire_ner = true\n[pii.ner]\nmodel_dir = \"models/ner\"\n";
        let cfg = load_toml(toml).expect("require_ner satisfied by [pii.ner]");
        assert!(cfg.pii.require_ner);
    }

    #[test]
    fn test_names_leak_without_ner_flag() {
        let leaky = load_toml("[pii]\nenabled_by_default = true\n").unwrap();
        assert!(leaky.pii.names_leak_without_ner());
        let safe = load_toml("[pii]\nenabled_by_default = false\n").unwrap();
        assert!(!safe.pii.names_leak_without_ner());
    }

    // -----------------------------------------------------------------------
    // Strict-config / unknown keys (v0.0.8)
    // -----------------------------------------------------------------------

    #[test]
    fn test_unknown_key_warns_by_default() {
        // Misplaced `score_threshold` at [pii] level (belongs under [pii.ner]).
        let cfg = load_toml_strict("[pii]\nscore_threshold = 0.7\n", false)
            .expect("unknown key only warns when not strict");
        // The stray key is ignored; the rest loads.
        assert!(cfg.pii.enabled_by_default);
    }

    #[test]
    fn test_unknown_key_rejected_in_strict_mode() {
        let err = load_toml_strict("[pii]\nscore_threshold = 0.7\n", true).unwrap_err();
        match err {
            ConfigError::Invalid(m) => {
                assert!(m.contains("unknown config key"), "{m}");
                assert!(m.contains("score_threshold"), "{m}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn test_unknown_section_rejected_in_strict_mode() {
        // The real leak path: a misplaced `[ner]` table instead of `[pii.ner]`.
        let err = load_toml_strict("[ner]\nmodel_dir = \"models/ner\"\n", true).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn test_known_config_passes_strict_mode() {
        let toml = "[pii]\nenabled_by_default = true\n[pii.ner]\nmodel_dir = \"models/ner\"\n";
        load_toml_strict(toml, true).expect("all-known config passes strict mode");
    }

    // -----------------------------------------------------------------------
    // Guardrails (v0.0.8)
    // -----------------------------------------------------------------------

    #[test]
    fn test_guardrails_absent_defaults_empty() {
        let cfg = load_toml("[pii]\nenabled_by_default = true\n").unwrap();
        assert!(cfg.guardrails.is_empty());
    }

    #[test]
    fn test_guardrails_rules_parse() {
        let toml = "\
[[guardrails.rules]]
name = \"block-jailbreaks\"
kind = \"prompt_injection\"
phase = \"pre\"
action = \"block\"

[[guardrails.rules]]
name = \"redact-contact\"
kind = \"contact_info\"
phase = \"post\"
action = \"redact\"
entities = [\"EMAIL_ADDRESS\", \"PHONE_NUMBER\"]
";
        let cfg = load_toml(toml).expect("valid guardrails");
        assert_eq!(cfg.guardrails.rules.len(), 2);
        assert_eq!(cfg.guardrails.rules[0].kind, GuardrailKind::PromptInjection);
        assert_eq!(cfg.guardrails.rules[0].phase, GuardrailPhase::Pre);
        assert_eq!(cfg.guardrails.rules[0].action, GuardrailAction::Block);
        assert_eq!(cfg.guardrails.rules[1].kind, GuardrailKind::ContactInfo);
        assert_eq!(cfg.guardrails.rules[1].action, GuardrailAction::Redact);
    }

    #[test]
    fn test_guardrails_defaults_phase_pre_action_block() {
        let toml = "[[guardrails.rules]]\nname = \"g\"\nkind = \"banned_content\"\npatterns = [\"x\"]\n";
        let cfg = load_toml(toml).unwrap();
        assert_eq!(cfg.guardrails.rules[0].phase, GuardrailPhase::Pre);
        assert_eq!(cfg.guardrails.rules[0].action, GuardrailAction::Block);
    }

    #[test]
    fn test_guardrails_empty_name_rejected() {
        let toml = "[[guardrails.rules]]\nname = \"\"\nkind = \"prompt_injection\"\n";
        assert!(matches!(load_toml(toml), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn test_guardrails_duplicate_name_rejected() {
        let toml = "\
[[guardrails.rules]]
name = \"dup\"
kind = \"prompt_injection\"
[[guardrails.rules]]
name = \"dup\"
kind = \"banned_content\"
patterns = [\"x\"]
";
        assert!(matches!(load_toml(toml), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn test_guardrails_contact_info_unknown_entity_rejected() {
        let toml = "[[guardrails.rules]]\nname = \"g\"\nkind = \"contact_info\"\nentities = [\"WAT\"]\n";
        assert!(matches!(load_toml(toml), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn test_guardrails_unknown_kind_rejected() {
        // Unknown enum variant → TOML parse error (serde rejects it).
        let toml = "[[guardrails.rules]]\nname = \"g\"\nkind = \"telepathy\"\n";
        assert!(matches!(load_toml(toml), Err(ConfigError::Parse { .. })));
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
    fn test_virtual_key_allow_pii_bypass_defaults_false() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "literal-key-value"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-nobypass"
connections = ["openai"]
"#;
        let cfg = load_toml(toml).expect("should load");
        assert!(
            !cfg.virtual_keys[0].allow_pii_bypass,
            "allow_pii_bypass must default to false (privacy-first)"
        );
    }

    #[test]
    fn test_virtual_key_allow_pii_bypass_parsed() {
        let toml = r#"
[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "literal-key-value"
format = "open_ai"

[[virtual_keys]]
key = "sk-drgtw-analyzer"
connections = ["openai"]
allow_pii_bypass = true
"#;
        let cfg = load_toml(toml).expect("should load");
        assert!(
            cfg.virtual_keys[0].allow_pii_bypass,
            "allow_pii_bypass = true must parse"
        );
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
        assert!(ner.scan_roles.is_none(), "default scan_roles is None (all roles)");
        assert_eq!(ner.cache_capacity, 0, "default cache_capacity 0 (disabled)");
    }

    #[test]
    fn test_ner_scan_roles_and_cache_parsed() {
        let toml = r#"
[pii.ner]
model_dir = "models/ner"
scan_roles = ["user", "assistant"]
cache_capacity = 512
"#;
        let cfg = load_toml(toml).expect("ner scoping + cache config");
        let ner = cfg.pii.ner.as_ref().expect("ner present");
        assert_eq!(
            ner.scan_roles.as_deref(),
            Some(["user".to_string(), "assistant".to_string()].as_slice())
        );
        assert_eq!(ner.cache_capacity, 512);
    }

    #[test]
    fn test_ner_empty_scan_roles_rejected() {
        let toml = r#"
[pii.ner]
model_dir = "models/ner"
scan_roles = []
"#;
        let err = load_toml(toml).expect_err("empty scan_roles must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("scan_roles"), "error should mention scan_roles: {msg}");
    }

    #[test]
    fn test_ner_blank_scan_role_rejected() {
        let toml = r#"
[pii.ner]
model_dir = "models/ner"
scan_roles = ["user", "  "]
"#;
        let err = load_toml(toml).expect_err("blank scan_roles entry must be rejected");
        assert!(err.to_string().contains("scan_roles"));
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
    // UI (concept)
    // -----------------------------------------------------------------------

    #[test]
    fn test_ui_defaults_disabled_no_history() {
        let cfg = load_toml("").expect("empty config");
        assert!(!cfg.ui.enabled, "ui disabled by default");
        assert!(cfg.ui.history.is_none(), "ui.history absent by default");
    }

    #[test]
    fn test_ui_enabled_without_history() {
        let toml = "[ui]\nenabled = true\n";
        let cfg = load_toml(toml).expect("valid ui config");
        assert!(cfg.ui.enabled);
        assert!(cfg.ui.history.is_none());
    }

    #[test]
    fn test_ui_history_present_unlocks() {
        let toml = "[ui]\nenabled = true\n\n[ui.history]\npostgres_url = \"postgres://localhost/drgtw\"\n";
        let cfg = load_toml(toml).expect("valid ui history config");
        let history = cfg.ui.history.as_ref().expect("history present");
        assert_eq!(history.postgres_url, "postgres://localhost/drgtw");
    }

    #[test]
    fn test_ui_history_postgres_url_env_var_resolved() {
        let url = "postgres://user:pw@db.example.com:5432/drgtw";
        unsafe {
            std::env::set_var("DRGTW_TEST_UI_PG_OK", url);
        }
        let toml = r#"
[ui.history]
postgres_url = "${DRGTW_TEST_UI_PG_OK}"
"#;
        let cfg = load_toml(toml).expect("env-resolved postgres_url");
        assert_eq!(cfg.ui.history.as_ref().unwrap().postgres_url, url);
        unsafe {
            std::env::remove_var("DRGTW_TEST_UI_PG_OK");
        }
    }

    #[test]
    fn test_ui_history_postgres_url_missing_env_var_rejected() {
        unsafe {
            std::env::remove_var("DRGTW_TEST_UI_PG_MISSING");
        }
        let toml = r#"
[ui.history]
postgres_url = "${DRGTW_TEST_UI_PG_MISSING}"
"#;
        let err = load_toml(toml).expect_err("missing env var rejected");
        match err {
            ConfigError::MissingEnvVar { var, field } => {
                assert_eq!(var, "DRGTW_TEST_UI_PG_MISSING");
                assert_eq!(field, "ui.history.postgres_url");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_ui_history_postgres_url_empty_rejected() {
        let toml = "[ui.history]\npostgres_url = \"\"\n";
        let err = load_toml(toml).expect_err("empty postgres_url rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("ui.history.postgres_url"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_embeddings_require_vault_default_false() {
        let cfg = load_toml("").expect("empty config");
        assert!(
            !cfg.pii.embeddings_require_vault,
            "embeddings_require_vault should default to false"
        );
    }

    #[test]
    fn test_embeddings_require_vault_parsed_without_vault() {
        // Flag may be set even when no `[pii.vault]` is present — this is not a
        // validation error (the contradiction is resolved at boot, not here).
        let toml = "[pii]\nembeddings_require_vault = true\n";
        let cfg = load_toml(toml).expect("flag without vault still parses");
        assert!(cfg.pii.embeddings_require_vault);
        assert!(cfg.pii.vault.is_none(), "vault still absent");
    }

    #[test]
    fn test_embeddings_require_vault_parsed_with_vault() {
        let key = "a".repeat(64);
        let toml = format!(
            "[pii]\nembeddings_require_vault = true\n\n[pii.vault]\npath = \"vault.db\"\nkey = \"{key}\"\n"
        );
        let cfg = load_toml(&toml).expect("flag with vault parses");
        assert!(cfg.pii.embeddings_require_vault);
        assert!(cfg.pii.vault.is_some(), "vault present");
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

    #[test]
    fn test_mcp_servers_forward_headers_parsed_and_defaulted() {
        // With explicit forward_headers list.
        let toml = r#"
[mcp_servers.srv]
url = "https://mcp.example.com/mcp"
forward_headers = ["X-Trace-Id", "X-Tenant"]
"#;
        let cfg = load_toml(toml).expect("forward_headers parsed");
        let srv = cfg.mcp_servers.get("srv").unwrap();
        assert_eq!(srv.forward_headers, vec!["X-Trace-Id", "X-Tenant"]);

        // Default: absent = empty vec.
        let toml2 = r#"
[mcp_servers.srv]
url = "https://mcp.example.com/mcp"
"#;
        let cfg2 = load_toml(toml2).expect("forward_headers defaults to empty");
        let srv2 = cfg2.mcp_servers.get("srv").unwrap();
        assert!(srv2.forward_headers.is_empty());
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

    // -----------------------------------------------------------------------
    // Bedrock Converse format + SigV4 credentials (0.0.3)
    // -----------------------------------------------------------------------

    #[test]
    fn test_bedrock_converse_format_deserializes() {
        let toml = r#"
[[connections]]
name = "bedrock-converse-bearer"
base_url = "https://bedrock-runtime.us-east-1.amazonaws.com"
api_key = "bedrock-bearer-token"
format = "bedrock_converse"
region = "us-east-1"
models = ["us.amazon.titan-text-premier-v1:0"]
"#;
        let cfg = load_toml(toml).expect("bedrock_converse should load");
        assert_eq!(cfg.connections[0].format, ApiFormat::BedrockConverse);
        assert_eq!(cfg.connections[0].region.as_deref(), Some("us-east-1"));
    }

    #[test]
    fn test_bedrock_converse_sigv4_creds_deserialize() {
        let toml = r#"
[[connections]]
name = "bc-sigv4"
base_url = "https://bedrock-runtime.eu-central-1.amazonaws.com"
api_key = ""
format = "bedrock_converse"
region = "eu-central-1"
aws_access_key_id = "AKIDEXAMPLE"
aws_secret_access_key = "secret-value"
models = ["eu.amazon.nova-pro-v1:0"]
"#;
        let cfg = load_toml(toml).expect("sigv4-only converse should load");
        assert_eq!(
            cfg.connections[0].aws_access_key_id.as_deref(),
            Some("AKIDEXAMPLE")
        );
        assert_eq!(
            cfg.connections[0].aws_secret_access_key.as_deref(),
            Some("secret-value")
        );
        assert!(cfg.connections[0].aws_session_token.is_none());
    }

    #[test]
    fn test_bedrock_converse_sigv4_env_expansion() {
        unsafe {
            std::env::set_var("DRGTW_TEST_AWS_AKID", "AKIDFROMENV");
            std::env::set_var("DRGTW_TEST_AWS_SECRET", "SECRETFROMENV");
            std::env::set_var("DRGTW_TEST_AWS_TOKEN", "TOKENFROMENV");
            std::env::set_var("DRGTW_TEST_AWS_REGION", "eu-west-1");
        }
        let toml = r#"
[[connections]]
name = "bc-env"
base_url = "https://bedrock-runtime.eu-west-1.amazonaws.com"
api_key = ""
format = "bedrock_converse"
region = "${DRGTW_TEST_AWS_REGION}"
aws_access_key_id = "${DRGTW_TEST_AWS_AKID}"
aws_secret_access_key = "${DRGTW_TEST_AWS_SECRET}"
aws_session_token = "${DRGTW_TEST_AWS_TOKEN}"
"#;
        let cfg = load_toml(toml).expect("env expansion should load");
        assert_eq!(cfg.connections[0].region.as_deref(), Some("eu-west-1"));
        assert_eq!(
            cfg.connections[0].aws_access_key_id.as_deref(),
            Some("AKIDFROMENV")
        );
        assert_eq!(
            cfg.connections[0].aws_secret_access_key.as_deref(),
            Some("SECRETFROMENV")
        );
        assert_eq!(
            cfg.connections[0].aws_session_token.as_deref(),
            Some("TOKENFROMENV")
        );
        unsafe {
            std::env::remove_var("DRGTW_TEST_AWS_AKID");
            std::env::remove_var("DRGTW_TEST_AWS_SECRET");
            std::env::remove_var("DRGTW_TEST_AWS_TOKEN");
            std::env::remove_var("DRGTW_TEST_AWS_REGION");
        }
    }

    #[test]
    fn test_bedrock_converse_no_auth_rejected() {
        // Neither SigV4 creds nor a non-empty api_key.
        let toml = r#"
[[connections]]
name = "bc-noauth"
base_url = "https://bedrock-runtime.us-east-1.amazonaws.com"
api_key = ""
format = "bedrock_converse"
region = "us-east-1"
"#;
        let err = load_toml(toml).expect_err("no auth must be rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("bedrock_converse requires either"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_bedrock_converse_partial_sigv4_creds_rejected() {
        // Only access key, no secret.
        let toml = r#"
[[connections]]
name = "bc-partial"
base_url = "https://bedrock-runtime.us-east-1.amazonaws.com"
api_key = ""
format = "bedrock_converse"
region = "us-east-1"
aws_access_key_id = "AKIDEXAMPLE"
"#;
        let err = load_toml(toml).expect_err("partial creds rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("must be set together"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_bedrock_converse_sigv4_without_region_rejected() {
        let toml = r#"
[[connections]]
name = "bc-noregion"
base_url = "https://bedrock-runtime.us-east-1.amazonaws.com"
api_key = ""
format = "bedrock_converse"
aws_access_key_id = "AKIDEXAMPLE"
aws_secret_access_key = "secret-value"
"#;
        let err = load_toml(toml).expect_err("missing region rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("region is required for SigV4"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_bedrock_converse_session_token_without_keys_rejected() {
        let toml = r#"
[[connections]]
name = "bc-tokenonly"
base_url = "https://bedrock-runtime.us-east-1.amazonaws.com"
api_key = "bearer-token"
format = "bedrock_converse"
region = "us-east-1"
aws_session_token = "orphan-token"
"#;
        let err = load_toml(toml).expect_err("session token without keys rejected");
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("aws_session_token requires"), "{msg}"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn test_bedrock_converse_sigv4_only_empty_api_key_ok() {
        // The universal non-empty api_key rule is relaxed for sigv4-only
        // bedrock_converse connections.
        let toml = r#"
[[connections]]
name = "bc-sigv4only"
base_url = "https://bedrock-runtime.eu-central-1.amazonaws.com"
api_key = ""
format = "bedrock_converse"
region = "eu-central-1"
aws_access_key_id = "AKIDEXAMPLE"
aws_secret_access_key = "secret-value"
"#;
        let cfg = load_toml(toml).expect("sigv4-only empty api_key is valid");
        assert_eq!(cfg.connections[0].format, ApiFormat::BedrockConverse);
        assert!(cfg.connections[0].api_key.is_empty());
    }

    #[test]
    fn test_openai_still_requires_nonempty_api_key() {
        // The relaxation is scoped to bedrock_converse: open_ai still rejects
        // an empty api_key.
        let toml = r#"
[[connections]]
name = "oai-empty"
base_url = "https://api.example.com/v1"
api_key = ""
format = "open_ai"
"#;
        let err = load_toml(toml).expect_err("open_ai empty key rejected");
        match err {
            ConfigError::Invalid(msg) => {
                assert!(msg.contains("connections[oai-empty].api_key"), "{msg}")
            }
            other => panic!("unexpected: {other}"),
        }
    }
}
