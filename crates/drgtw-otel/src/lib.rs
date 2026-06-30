//! # drgtw-otel
//!
//! OpenTelemetry OTLP export (traces + metrics) for the DRGTW privacy gateway.
//!
//! ## Privacy invariant (authoritative — see `md/otel-design.md` §3)
//!
//! Spans and metrics carry **only** the allow-listed metadata below. There is
//! NO code path that puts prompt/response content, PII values, pseudonyms, or
//! secrets onto a span or metric, and no config switch to enable content
//! capture (`gen_ai.input.messages` / `gen_ai.output.messages` /
//! `gen_ai.system_instructions` are never emitted).
//!
//! The allow-list:
//! model (request + response), connection name, status / error class, input &
//! output token counts, cost USD, latency, ttft, key_id, pii_flag, request_id,
//! endpoint / operation, fallback attempts, pii entity *counts*.
//!
//! The [`RequestTelemetry`] struct is the ONLY input to the attribute builders.
//! It has no field capable of holding content, so the allow-list is enforced by
//! construction, not by review alone. A poison test in `tests/` asserts this.
//!
//! ## Runtime gating
//!
//! Always compiled; behaviour gated at runtime by `otel.enabled`. [`init`]
//! returns `Ok(None)` when disabled — no provider installed, the gateway's
//! stderr `fmt` subscriber and `[tracing]` JSONL writer are untouched.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context as _;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};

use drgtw_config::{OtelConfig, OtelProtocol};

// ---------------------------------------------------------------------------
// Attribute key constants (semconv + drgtw-private)
// ---------------------------------------------------------------------------

/// Semantic-convention + drgtw-private attribute keys. Centralised so the
/// allow-list guard test can enumerate exactly the keys that may be emitted.
pub mod keys {
    // GenAI semantic conventions (experimental).
    pub const GEN_AI_OPERATION_NAME: &str = "gen_ai.operation.name";
    pub const GEN_AI_PROVIDER_NAME: &str = "gen_ai.provider.name";
    pub const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
    pub const GEN_AI_RESPONSE_MODEL: &str = "gen_ai.response.model";
    pub const GEN_AI_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
    pub const GEN_AI_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
    pub const GEN_AI_TOKEN_TYPE: &str = "gen_ai.token.type";
    pub const SERVER_ADDRESS: &str = "server.address";
    pub const SERVER_PORT: &str = "server.port";
    pub const ERROR_TYPE: &str = "error.type";

    // drgtw-private (namespaced to avoid colliding with semconv).
    pub const DRGTW_CONNECTION: &str = "drgtw.connection";
    pub const DRGTW_KEY_ID: &str = "drgtw.key_id";
    pub const DRGTW_REQUEST_ID: &str = "drgtw.request_id";
    pub const DRGTW_PII_FLAG: &str = "drgtw.pii_flag";
    pub const DRGTW_COST_USD: &str = "drgtw.cost_usd";
    pub const DRGTW_FALLBACK_ATTEMPTS: &str = "drgtw.fallback_attempts";
    pub const DRGTW_STREAMING: &str = "drgtw.streaming";
    pub const STATUS: &str = "status";

    /// Forbidden keys that MUST NEVER be emitted. The poison test asserts none
    /// of these ever appear on a built attribute set.
    pub const FORBIDDEN: &[&str] = &[
        "gen_ai.input.messages",
        "gen_ai.output.messages",
        "gen_ai.system_instructions",
        "gen_ai.prompt",
        "gen_ai.completion",
    ];

    /// Full allow-list of keys the builders may emit. The guard test asserts the
    /// produced key set is a subset of this.
    pub const ALLOW_LIST: &[&str] = &[
        GEN_AI_OPERATION_NAME,
        GEN_AI_PROVIDER_NAME,
        GEN_AI_REQUEST_MODEL,
        GEN_AI_RESPONSE_MODEL,
        GEN_AI_USAGE_INPUT_TOKENS,
        GEN_AI_USAGE_OUTPUT_TOKENS,
        GEN_AI_TOKEN_TYPE,
        SERVER_ADDRESS,
        SERVER_PORT,
        ERROR_TYPE,
        DRGTW_CONNECTION,
        DRGTW_KEY_ID,
        DRGTW_REQUEST_ID,
        DRGTW_PII_FLAG,
        DRGTW_COST_USD,
        DRGTW_FALLBACK_ATTEMPTS,
        DRGTW_STREAMING,
        STATUS,
    ];
}

// ---------------------------------------------------------------------------
// RequestTelemetry — the ONLY input to the attribute builders (allow-list).
// ---------------------------------------------------------------------------

/// Allow-listed, content-free telemetry for one proxied request.
///
/// This is the single source of truth for span attributes and metric labels.
/// It deliberately has **no field** capable of holding prompt/response content,
/// PII values, pseudonyms, or secrets — the privacy allow-list is enforced by
/// construction. Adding such a field would be a privacy regression and is
/// guarded by the poison test.
#[derive(Debug, Clone, Default)]
pub struct RequestTelemetry {
    /// `"chat"` / `"embeddings"` / `"list_models"`.
    pub operation: String,
    /// Provider name derived from `ApiFormat` (`openai` / `anthropic` / `bedrock`).
    pub provider: String,
    /// Request model (allows alias-resolved name).
    pub request_model: String,
    /// Upstream response model, if known.
    pub response_model: Option<String>,
    /// Connection name that served the request.
    pub connection: String,
    /// Upstream host (no path/query).
    pub server_address: Option<String>,
    /// Upstream port.
    pub server_port: Option<u16>,
    /// Virtual-key id (name), never the secret. Span-only by default; on metrics
    /// only when `metrics_include_key_id`.
    pub key_id: String,
    /// Opaque correlation id. Span-only — NEVER a metric label.
    pub request_id: String,
    /// HTTP/logical status code.
    pub status: Option<u16>,
    /// `ProxyError` class name on failure (class only, never the message).
    pub error_type: Option<String>,
    /// Input token count.
    pub input_tokens: Option<u64>,
    /// Output token count.
    pub output_tokens: Option<u64>,
    /// Derived cost in USD.
    pub cost_usd: Option<f64>,
    /// End-to-end latency in seconds.
    pub latency_s: Option<f64>,
    /// Time-to-first-chunk in seconds (streaming only).
    pub ttft_s: Option<f64>,
    /// "Did we redact" — true when PII mode On and at least one entity matched.
    pub pii_flag: bool,
    /// Streaming response?
    pub streaming: bool,
    /// Number of fallback attempts (0 = primary served).
    pub fallback_attempts: u32,
}

impl RequestTelemetry {
    /// Build the **metric label** set. Cardinality-controlled:
    /// `request_id` is NEVER included; `key_id` only when `include_key_id`.
    pub fn metric_attrs(&self, include_key_id: bool) -> Vec<KeyValue> {
        let mut kv = vec![
            KeyValue::new(keys::GEN_AI_OPERATION_NAME, self.operation.clone()),
            KeyValue::new(keys::GEN_AI_PROVIDER_NAME, self.provider.clone()),
            KeyValue::new(keys::GEN_AI_REQUEST_MODEL, self.request_model.clone()),
            KeyValue::new(keys::DRGTW_CONNECTION, self.connection.clone()),
        ];
        if let Some(s) = self.status {
            kv.push(KeyValue::new(keys::STATUS, s as i64));
        }
        if let Some(e) = &self.error_type {
            kv.push(KeyValue::new(keys::ERROR_TYPE, e.clone()));
        }
        if include_key_id && !self.key_id.is_empty() {
            kv.push(KeyValue::new(keys::DRGTW_KEY_ID, self.key_id.clone()));
        }
        kv
    }

    /// Build the **span attribute** set (the wider allow-listed set; key_id and
    /// request_id are always present here — spans are not aggregated).
    pub fn span_attrs(&self) -> Vec<KeyValue> {
        let mut kv = vec![
            KeyValue::new(keys::GEN_AI_OPERATION_NAME, self.operation.clone()),
            KeyValue::new(keys::GEN_AI_PROVIDER_NAME, self.provider.clone()),
            KeyValue::new(keys::GEN_AI_REQUEST_MODEL, self.request_model.clone()),
            KeyValue::new(keys::DRGTW_CONNECTION, self.connection.clone()),
            KeyValue::new(keys::DRGTW_KEY_ID, self.key_id.clone()),
            KeyValue::new(keys::DRGTW_REQUEST_ID, self.request_id.clone()),
            KeyValue::new(keys::DRGTW_PII_FLAG, self.pii_flag),
            KeyValue::new(keys::DRGTW_STREAMING, self.streaming),
            KeyValue::new(
                keys::DRGTW_FALLBACK_ATTEMPTS,
                i64::from(self.fallback_attempts),
            ),
        ];
        if let Some(m) = &self.response_model {
            kv.push(KeyValue::new(keys::GEN_AI_RESPONSE_MODEL, m.clone()));
        }
        if let Some(a) = &self.server_address {
            kv.push(KeyValue::new(keys::SERVER_ADDRESS, a.clone()));
        }
        if let Some(p) = self.server_port {
            kv.push(KeyValue::new(keys::SERVER_PORT, i64::from(p)));
        }
        if let Some(t) = self.input_tokens {
            kv.push(KeyValue::new(keys::GEN_AI_USAGE_INPUT_TOKENS, t as i64));
        }
        if let Some(t) = self.output_tokens {
            kv.push(KeyValue::new(keys::GEN_AI_USAGE_OUTPUT_TOKENS, t as i64));
        }
        if let Some(c) = self.cost_usd {
            kv.push(KeyValue::new(keys::DRGTW_COST_USD, c));
        }
        if let Some(s) = self.status {
            kv.push(KeyValue::new(keys::STATUS, s as i64));
        }
        if let Some(e) = &self.error_type {
            kv.push(KeyValue::new(keys::ERROR_TYPE, e.clone()));
        }
        kv
    }
}

// ---------------------------------------------------------------------------
// Semconv histogram bucket boundaries
// ---------------------------------------------------------------------------

/// GenAI `gen_ai.client.operation.duration` / `time_to_first_chunk` buckets (s).
const DURATION_BUCKETS: &[f64] = &[
    0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
];

/// GenAI `gen_ai.client.token.usage` buckets (`{token}`).
const TOKEN_BUCKETS: &[f64] = &[
    1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0,
    16777216.0, 67108864.0,
];

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// All instruments, created once. Recording is a cheap method call; the
/// cardinality-controlled label set is built from [`RequestTelemetry`].
pub struct Metrics {
    /// Whether `key_id` is included on metric labels (config-gated, default off).
    include_key_id: bool,
    // Histograms (GenAI semconv).
    operation_duration: Histogram<f64>,
    token_usage: Histogram<u64>,
    time_to_first_chunk: Histogram<f64>,
    // Counters (drgtw-private).
    requests: Counter<u64>,
    tokens_input: Counter<u64>,
    tokens_output: Counter<u64>,
    cost_usd: Counter<f64>,
    pii_redactions: Counter<u64>,
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metrics")
            .field("include_key_id", &self.include_key_id)
            .finish_non_exhaustive()
    }
}

impl Metrics {
    /// Construct all instruments from a [`Meter`].
    pub fn new(meter: &Meter, include_key_id: bool) -> Self {
        let operation_duration = meter
            .f64_histogram("gen_ai.client.operation.duration")
            .with_unit("s")
            .with_description("GenAI client operation end-to-end duration")
            .with_boundaries(DURATION_BUCKETS.to_vec())
            .build();
        let token_usage = meter
            .u64_histogram("gen_ai.client.token.usage")
            .with_unit("{token}")
            .with_description("GenAI client token usage (input and output)")
            .with_boundaries(TOKEN_BUCKETS.to_vec())
            .build();
        let time_to_first_chunk = meter
            .f64_histogram("gen_ai.client.operation.time_to_first_chunk")
            .with_unit("s")
            .with_description("Time to first streamed chunk")
            .with_boundaries(DURATION_BUCKETS.to_vec())
            .build();
        let requests = meter
            .u64_counter("drgtw.requests")
            .with_unit("{request}")
            .with_description("Total proxied requests")
            .build();
        let tokens_input = meter
            .u64_counter("drgtw.tokens.input")
            .with_unit("{token}")
            .with_description("Total input tokens")
            .build();
        let tokens_output = meter
            .u64_counter("drgtw.tokens.output")
            .with_unit("{token}")
            .with_description("Total output tokens")
            .build();
        let cost_usd = meter
            .f64_counter("drgtw.cost.usd")
            .with_unit("{usd}")
            .with_description("Total cost in USD")
            .build();
        let pii_redactions = meter
            .u64_counter("drgtw.pii.redactions")
            .with_unit("{entity}")
            .with_description("Total PII entities redacted")
            .build();
        Self {
            include_key_id,
            operation_duration,
            token_usage,
            time_to_first_chunk,
            requests,
            tokens_input,
            tokens_output,
            cost_usd,
            pii_redactions,
        }
    }

    /// Record one completed request's metrics from the allow-listed telemetry.
    pub fn record_request(&self, t: &RequestTelemetry) {
        let attrs = t.metric_attrs(self.include_key_id);

        self.requests.add(1, &attrs);

        if let Some(secs) = t.latency_s {
            self.operation_duration.record(secs, &attrs);
        }
        if let Some(ttft) = t.ttft_s {
            self.time_to_first_chunk.record(ttft, &attrs);
        }
        if let Some(input) = t.input_tokens {
            let mut a = attrs.clone();
            a.push(KeyValue::new(keys::GEN_AI_TOKEN_TYPE, "input"));
            self.token_usage.record(input, &a);
            self.tokens_input.add(input, &attrs);
        }
        if let Some(output) = t.output_tokens {
            let mut a = attrs.clone();
            a.push(KeyValue::new(keys::GEN_AI_TOKEN_TYPE, "output"));
            self.token_usage.record(output, &a);
            self.tokens_output.add(output, &attrs);
        }
        if let Some(cost) = t.cost_usd {
            self.cost_usd.add(cost, &attrs);
        }
    }

    /// Record PII redaction count (entities detected this request).
    pub fn record_pii_redactions(&self, count: u64, t: &RequestTelemetry) {
        if count > 0 {
            self.pii_redactions.add(count, &t.metric_attrs(self.include_key_id));
        }
    }
}

// ---------------------------------------------------------------------------
// Init + guard + shutdown
// ---------------------------------------------------------------------------

/// Holds the SDK providers + the `tracing` layer plumbing so the binary can
/// install the layer and flush on graceful shutdown.
pub struct OtelGuard {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    /// Instruments, present when metrics export is on.
    pub metrics: Option<std::sync::Arc<Metrics>>,
    timeout: Duration,
}

impl std::fmt::Debug for OtelGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtelGuard")
            .field("traces", &self.tracer_provider.is_some())
            .field("metrics", &self.meter_provider.is_some())
            .finish()
    }
}

impl OtelGuard {
    /// Flush and shut down both providers, bounded by `export_timeout_ms`.
    /// Idempotent: takes the providers so a second call is a no-op.
    pub fn shutdown(mut self) {
        let _ = self.timeout; // deadline is applied by the exporter config.
        if let Some(tp) = self.tracer_provider.take()
            && let Err(e) = tp.shutdown()
        {
            tracing::warn!(error = %e, "otel tracer provider shutdown error");
        }
        if let Some(mp) = self.meter_provider.take()
            && let Err(e) = mp.shutdown()
        {
            tracing::warn!(error = %e, "otel meter provider shutdown error");
        }
    }

    /// Tracer provider handle (for installing the `tracing-opentelemetry`
    /// layer). `None` when traces are disabled.
    pub fn tracer_provider(&self) -> Option<&SdkTracerProvider> {
        self.tracer_provider.as_ref()
    }
}

/// Phoenix (Arize) routes spans to a project via this resource attribute.
/// Without it every span lands in Phoenix's `default` project.
const PHOENIX_PROJECT_KEY: &str = "openinference.project.name";

fn resource(cfg: &OtelConfig) -> Resource {
    let attrs = merged_resource_attributes(
        cfg,
        std::env::var("OTEL_RESOURCE_ATTRIBUTES").ok(),
        std::env::var("PHOENIX_PROJECT_NAME").ok(),
    );
    let mut builder = Resource::builder()
        .with_service_name(cfg.service_name.clone())
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")));
    for (k, v) in attrs {
        builder = builder.with_attribute(KeyValue::new(k, v));
    }
    builder.build()
}

/// Parse the standard `OTEL_RESOURCE_ATTRIBUTES` value — comma-separated
/// `key=value` pairs (W3C Baggage form) — into pairs. Keys/values are trimmed;
/// entries without `=` or with an empty key are skipped.
fn parse_otel_resource_attributes(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let k = k.trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_string(), v.trim().to_string()))
        })
        .collect()
}

/// Resolve the effective extra resource attributes. Precedence, lowest first:
/// `cfg.resource_attributes` < `OTEL_RESOURCE_ATTRIBUTES` (per key) <
/// `PHOENIX_PROJECT_NAME` (sets [`PHOENIX_PROJECT_KEY`]). Env args are passed in
/// so the merge is testable without mutating process env.
fn merged_resource_attributes(
    cfg: &OtelConfig,
    otel_env: Option<String>,
    phoenix_env: Option<String>,
) -> BTreeMap<String, String> {
    let mut attrs: BTreeMap<String, String> = cfg.resource_attributes.clone();
    if let Some(raw) = otel_env {
        for (k, v) in parse_otel_resource_attributes(&raw) {
            attrs.insert(k, v);
        }
    }
    if let Some(name) = phoenix_env {
        let name = name.trim();
        if !name.is_empty() {
            attrs.insert(PHOENIX_PROJECT_KEY.to_string(), name.to_string());
        }
    }
    attrs
}

/// Effective endpoint: `OTEL_EXPORTER_OTLP_ENDPOINT` env override, else config.
fn effective_endpoint(cfg: &OtelConfig) -> String {
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap_or_else(|_| cfg.endpoint.clone())
}

/// Append an OTLP/HTTP signal path (`/v1/traces`, `/v1/metrics`) to a base
/// endpoint, normalising any trailing slash.
///
/// The `opentelemetry-otlp` HTTP builder treats `with_endpoint` as the exact
/// per-signal URL (it does NOT auto-append the signal path the way the
/// `OTEL_EXPORTER_OTLP_ENDPOINT` env var does), so we append it ourselves to
/// keep `otel.endpoint` a plain base URL like `http://collector:4318`. The gRPC
/// path takes the base endpoint as-is.
pub fn otlp_signal_endpoint(base: &str, signal_path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), signal_path)
}

/// Build the tracer + meter providers and install them globally.
///
/// Returns `Ok(None)` when `cfg.enabled` is false — nothing is installed.
/// On success the global meter provider is set; the caller installs the tracer
/// layer via [`tracer_layer`].
pub fn init(cfg: &OtelConfig) -> anyhow::Result<Option<OtelGuard>> {
    if !cfg.enabled {
        return Ok(None);
    }
    let res = resource(cfg);
    let endpoint = effective_endpoint(cfg);
    let timeout = Duration::from_millis(cfg.export_timeout_ms);

    let tracer_provider = if cfg.traces {
        // Honour inbound W3C trace context (`traceparent`/`tracestate`) so the
        // proxy span can nest under an OTel-instrumented caller's span instead
        // of starting a fresh root. Extraction happens in the proxy handler.
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );
        Some(build_tracer_provider(cfg, &endpoint, timeout, res.clone())?)
    } else {
        None
    };

    let (meter_provider, metrics) = if cfg.metrics {
        let mp = build_meter_provider(cfg, &endpoint, timeout, res)?;
        opentelemetry::global::set_meter_provider(mp.clone());
        let meter = opentelemetry::global::meter("drgtw");
        let m = std::sync::Arc::new(Metrics::new(&meter, cfg.metrics_include_key_id));
        (Some(mp), Some(m))
    } else {
        (None, None)
    };

    Ok(Some(OtelGuard {
        tracer_provider,
        meter_provider,
        metrics,
        timeout,
    }))
}

fn build_tracer_provider(
    cfg: &OtelConfig,
    endpoint: &str,
    timeout: Duration,
    res: Resource,
) -> anyhow::Result<SdkTracerProvider> {
    let exporter = match cfg.protocol {
        OtelProtocol::Grpc => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .with_timeout(timeout)
            .build()
            .context("build OTLP gRPC span exporter")?,
        OtelProtocol::Http => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(otlp_signal_endpoint(endpoint, "/v1/traces"))
            .with_timeout(timeout)
            .build()
            .context("build OTLP HTTP span exporter")?,
    };

    // Parent-based ratio sampler: honour parent sampling decision, fall back to
    // a trace-id ratio at the root.
    let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(cfg.sample_ratio)));

    Ok(SdkTracerProvider::builder()
        .with_resource(res)
        .with_sampler(sampler)
        .with_batch_exporter(exporter)
        .build())
}

fn build_meter_provider(
    cfg: &OtelConfig,
    endpoint: &str,
    timeout: Duration,
    res: Resource,
) -> anyhow::Result<SdkMeterProvider> {
    let exporter = match cfg.protocol {
        OtelProtocol::Grpc => opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .with_timeout(timeout)
            .build()
            .context("build OTLP gRPC metric exporter")?,
        OtelProtocol::Http => opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(otlp_signal_endpoint(endpoint, "/v1/metrics"))
            .with_timeout(timeout)
            .build()
            .context("build OTLP HTTP metric exporter")?,
    };

    let reader = PeriodicReader::builder(exporter)
        .with_interval(Duration::from_millis(cfg.export_interval_ms))
        .build();

    Ok(SdkMeterProvider::builder()
        .with_resource(res)
        .with_reader(reader)
        .build())
}

/// Build a `tracing` layer that exports spans to the given tracer provider.
///
/// The binary adds this to its `registry()` only when `otel.enabled &&
/// otel.traces`. Generic over the subscriber so it composes with the existing
/// fmt layer.
pub fn tracer_layer<S>(
    provider: &SdkTracerProvider,
) -> tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::SdkTracer>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    let tracer = provider.tracer("drgtw");
    // Lean spans: suppress the layer's default `code.*` location, thread, and
    // idle/busy-time attributes — they add noise/cardinality and are not on
    // the allow-list. (They carry no content either; this is hygiene, not a
    // privacy requirement.)
    tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_location(false)
        .with_threads(false)
        .with_tracked_inactivity(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_telemetry() -> RequestTelemetry {
        RequestTelemetry {
            operation: "chat".into(),
            provider: "openai".into(),
            request_model: "gpt-4o".into(),
            response_model: Some("gpt-4o-2024".into()),
            connection: "openai".into(),
            server_address: Some("api.example.com".into()),
            server_port: Some(443),
            key_id: "team-a".into(),
            request_id: "req-123".into(),
            status: Some(200),
            error_type: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            cost_usd: Some(0.0025),
            latency_s: Some(1.5),
            ttft_s: Some(0.3),
            pii_flag: true,
            streaming: true,
            fallback_attempts: 1,
        }
    }

    #[test]
    fn signal_endpoint_appends_and_normalises_slash() {
        assert_eq!(
            otlp_signal_endpoint("http://collector:4318", "/v1/traces"),
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            otlp_signal_endpoint("http://collector:4318/", "/v1/metrics"),
            "http://collector:4318/v1/metrics"
        );
    }

    #[test]
    fn parse_resource_attrs_basic_trims_and_skips_malformed() {
        let got = parse_otel_resource_attributes(
            " openinference.project.name = my-proj , deployment.env=prod ,bad,=novalue,k= ",
        );
        assert_eq!(
            got,
            vec![
                ("openinference.project.name".to_string(), "my-proj".to_string()),
                ("deployment.env".to_string(), "prod".to_string()),
                ("k".to_string(), "".to_string()),
            ],
            "trims ws, keeps empty values, drops entries without `=` or empty key"
        );
    }

    fn cfg_with_attrs(pairs: &[(&str, &str)]) -> OtelConfig {
        OtelConfig {
            resource_attributes: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn merged_attrs_config_only_passthrough() {
        let cfg = cfg_with_attrs(&[(PHOENIX_PROJECT_KEY, "from-config")]);
        let got = merged_resource_attributes(&cfg, None, None);
        assert_eq!(got.get(PHOENIX_PROJECT_KEY).map(String::as_str), Some("from-config"));
    }

    #[test]
    fn merged_attrs_otel_env_overrides_config_per_key() {
        let cfg = cfg_with_attrs(&[(PHOENIX_PROJECT_KEY, "from-config"), ("keep", "kept")]);
        let got = merged_resource_attributes(
            &cfg,
            Some(format!("{PHOENIX_PROJECT_KEY}=from-env")),
            None,
        );
        assert_eq!(got.get(PHOENIX_PROJECT_KEY).map(String::as_str), Some("from-env"));
        assert_eq!(got.get("keep").map(String::as_str), Some("kept"), "untouched keys survive");
    }

    #[test]
    fn merged_attrs_phoenix_env_wins_over_all() {
        let cfg = cfg_with_attrs(&[(PHOENIX_PROJECT_KEY, "from-config")]);
        let got = merged_resource_attributes(
            &cfg,
            Some(format!("{PHOENIX_PROJECT_KEY}=from-otel-env")),
            Some("from-phoenix-env".to_string()),
        );
        assert_eq!(
            got.get(PHOENIX_PROJECT_KEY).map(String::as_str),
            Some("from-phoenix-env"),
            "PHOENIX_PROJECT_NAME has highest precedence"
        );
    }

    #[test]
    fn merged_attrs_blank_phoenix_env_ignored() {
        let cfg = cfg_with_attrs(&[(PHOENIX_PROJECT_KEY, "from-config")]);
        let got = merged_resource_attributes(&cfg, None, Some("   ".to_string()));
        assert_eq!(
            got.get(PHOENIX_PROJECT_KEY).map(String::as_str),
            Some("from-config"),
            "blank PHOENIX_PROJECT_NAME does not clobber config"
        );
    }

    #[test]
    fn init_disabled_returns_none() {
        let cfg = OtelConfig::default(); // enabled = false
        let guard = init(&cfg).expect("init must not error when disabled");
        assert!(guard.is_none(), "disabled otel installs nothing");
    }

    #[test]
    fn span_attrs_subset_of_allow_list() {
        let t = sample_telemetry();
        let attrs = t.span_attrs();
        for kv in &attrs {
            let k = kv.key.as_str();
            assert!(
                keys::ALLOW_LIST.contains(&k),
                "span attr `{k}` is not on the allow-list"
            );
        }
    }

    #[test]
    fn metric_attrs_subset_of_allow_list() {
        let t = sample_telemetry();
        for include_key_id in [false, true] {
            for kv in t.metric_attrs(include_key_id) {
                let k = kv.key.as_str();
                assert!(
                    keys::ALLOW_LIST.contains(&k),
                    "metric attr `{k}` not on allow-list"
                );
            }
        }
    }

    #[test]
    fn no_forbidden_keys_ever_emitted() {
        let t = sample_telemetry();
        let mut all: Vec<String> = t.span_attrs().iter().map(|k| k.key.to_string()).collect();
        all.extend(t.metric_attrs(true).iter().map(|k| k.key.to_string()));
        for forbidden in keys::FORBIDDEN {
            assert!(
                !all.iter().any(|k| k == forbidden),
                "forbidden key `{forbidden}` was emitted"
            );
        }
    }

    #[test]
    fn key_id_off_metrics_by_default_on_spans_always() {
        let t = sample_telemetry();
        // Spans always carry key_id.
        assert!(
            t.span_attrs().iter().any(|k| k.key.as_str() == keys::DRGTW_KEY_ID),
            "span must carry key_id"
        );
        // Metrics omit key_id by default.
        assert!(
            !t.metric_attrs(false)
                .iter()
                .any(|k| k.key.as_str() == keys::DRGTW_KEY_ID),
            "metrics must omit key_id by default"
        );
        // …and include it when explicitly enabled.
        assert!(
            t.metric_attrs(true).iter().any(|k| k.key.as_str() == keys::DRGTW_KEY_ID),
            "metrics include key_id when flag on"
        );
    }

    #[test]
    fn request_id_never_a_metric_label() {
        let t = sample_telemetry();
        for include_key_id in [false, true] {
            assert!(
                !t.metric_attrs(include_key_id)
                    .iter()
                    .any(|k| k.key.as_str() == keys::DRGTW_REQUEST_ID),
                "request_id must never be a metric label"
            );
        }
    }
}
