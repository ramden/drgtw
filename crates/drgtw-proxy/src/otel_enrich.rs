//! OpenTelemetry span enrichment + metric recording (0.0.2).
//!
//! This module is the single bridge between the proxy hot path and
//! `drgtw-otel`. It builds an allow-listed [`RequestTelemetry`] and:
//!   * sets GenAI-semconv + `drgtw.*` attributes on the current `tracing` span
//!     via [`OpenTelemetrySpanExt`] (dotted attribute keys the `info_span!`
//!     macro cannot express), and
//!   * records the request's metrics when `[otel] metrics` is enabled.
//!
//! Privacy: every value comes from the allow-listed [`RequestTelemetry`]; this
//! module has NO access to bodies, prompts, PII values, or secrets. When OTel
//! is disabled the span layer is not installed and `set_attribute` is a cheap
//! no-op; metric recording is gated on `state.metrics.is_some()`.

use drgtw_config::ApiFormat;
use drgtw_otel::{RequestTelemetry, keys};
use opentelemetry::trace::Status;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::ProxyState;

/// GenAI `gen_ai.provider.name` derived from the wire format. Bedrock serves the
/// Anthropic Messages shape but is its own provider for observability.
pub fn provider_name(format: ApiFormat) -> &'static str {
    match format {
        ApiFormat::OpenAi => "openai",
        ApiFormat::Anthropic => "anthropic",
        ApiFormat::Bedrock | ApiFormat::BedrockConverse => "aws.bedrock",
    }
}

/// Extract `(host, port)` from an upstream base URL, dropping path/query so no
/// secret-bearing URL component reaches a span. Returns `(None, None)` on parse
/// failure.
pub fn server_address(base_url: &str) -> (Option<String>, Option<u16>) {
    match url::Url::parse(base_url) {
        Ok(u) => {
            let host = u.host_str().map(str::to_owned);
            let port = u.port_or_known_default();
            (host, port)
        }
        Err(_) => (None, None),
    }
}

/// Set the allow-listed attributes on the current span + status on failure.
///
/// Uses [`OpenTelemetrySpanExt::set_attribute`] so dotted semconv keys are set
/// directly on the underlying OTel span. No-op when the OTel layer is absent.
pub fn enrich_span(t: &RequestTelemetry) {
    let span = Span::current();
    for kv in t.span_attrs() {
        span.set_attribute(kv.key, kv.value);
    }
    // Recording-Errors convention: set Error status (message omitted — class
    // only, per the privacy allow-list) when an error class is present.
    if let Some(class) = &t.error_type {
        span.set_status(Status::error(class.clone()));
    } else if matches!(t.status, Some(s) if s < 400) {
        span.set_status(Status::Ok);
    }
}

/// Record request + (optionally) PII-redaction metrics when metrics are on.
pub fn record_metrics(state: &ProxyState, t: &RequestTelemetry, pii_entities: u64) {
    if let Some(metrics) = &state.metrics {
        metrics.record_request(t);
        metrics.record_pii_redactions(pii_entities, t);
    }
}

/// Build a [`RequestTelemetry`] from the allow-listed request facts. Centralised
/// so every emit site produces the same content-free shape.
#[allow(clippy::too_many_arguments)]
pub fn telemetry(
    operation: &str,
    format: ApiFormat,
    request_model: &str,
    connection: &str,
    base_url: &str,
    key_id: &str,
    request_id: &str,
    status: Option<u16>,
    error_type: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cost_usd: Option<f64>,
    latency_s: Option<f64>,
    ttft_s: Option<f64>,
    pii_flag: bool,
    streaming: bool,
    fallback_attempts: u32,
) -> RequestTelemetry {
    let (server_addr, server_port) = server_address(base_url);
    RequestTelemetry {
        operation: operation.to_owned(),
        provider: provider_name(format).to_owned(),
        request_model: request_model.to_owned(),
        response_model: None,
        connection: connection.to_owned(),
        server_address: server_addr,
        server_port,
        key_id: key_id.to_owned(),
        request_id: request_id.to_owned(),
        status,
        error_type,
        input_tokens,
        output_tokens,
        cost_usd,
        latency_s,
        ttft_s,
        pii_flag,
        streaming,
        fallback_attempts,
    }
}

/// Map a `ProxyError`-style status to an `error.type` class string when the
/// status indicates failure. Class only — never a message.
pub fn error_class_for_status(status: u16) -> Option<String> {
    if status >= 500 {
        Some("upstream_error".to_owned())
    } else if status >= 400 {
        Some("client_error".to_owned())
    } else {
        None
    }
}

#[doc(hidden)]
pub const ALLOW_LIST: &[&str] = keys::ALLOW_LIST;
