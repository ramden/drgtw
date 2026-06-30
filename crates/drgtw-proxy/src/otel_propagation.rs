//! Inbound W3C trace-context propagation.
//!
//! An OTel-instrumented caller injects a `traceparent` (and optional
//! `tracestate`) header on every request. We extract that remote context with
//! the globally-registered propagator (installed by `drgtw_otel::init` when
//! tracing is enabled) and reparent the `proxy_request` span under the caller's
//! span — so the priced gateway span joins the caller's trace and inherits its
//! baggage (e.g. `session.id`) instead of starting a fresh root.
//!
//! Fail-open and side-effect-free when there is nothing to honour: a missing,
//! malformed, or unregistered-propagator case yields `None`, leaving the span a
//! root exactly as before.

use axum::http::HeaderMap;
use opentelemetry::Context;
use opentelemetry::propagation::Extractor;
use opentelemetry::trace::TraceContextExt as _;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

/// Adapts an `http`/`axum` [`HeaderMap`] to the OTel [`Extractor`] trait so the
/// registered text-map propagator can read W3C headers from it.
struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Extract a remote OTel [`Context`] from the inbound W3C trace-context
/// headers. Returns `None` when no *valid* remote span context is present —
/// no `traceparent`, a malformed one, or no propagator registered — in which
/// case the caller keeps the span as a root.
fn remote_context(headers: &HeaderMap) -> Option<Context> {
    let cx = opentelemetry::global::get_text_map_propagator(|prop| {
        prop.extract(&HeaderExtractor(headers))
    });
    // An empty / no-op extraction yields the current (invalid) context; only
    // adopt a parent when the remote span context is actually valid.
    if cx.span().span_context().is_valid() {
        Some(cx)
    } else {
        None
    }
}

/// If the request carries a valid W3C trace context, reparent `span` under that
/// remote span so it joins the caller's trace. No-op otherwise.
pub(crate) fn set_remote_parent(span: &Span, headers: &HeaderMap) {
    if let Some(cx) = remote_context(headers) {
        let _ = span.set_parent(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::propagation::TraceContextPropagator;

    /// W3C example traceparent: version-00, the given 16-byte trace-id,
    /// an 8-byte parent span-id, sampled.
    const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    const TRACE_ID_HEX: &str = "0af7651916cd43dd8448eb211c80319c";

    /// The propagator is process-global; registering the same one repeatedly is
    /// idempotent, so each test can ensure it without ordering assumptions.
    fn ensure_propagator() {
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    }

    fn headers_with_traceparent(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("traceparent", value.parse().unwrap());
        h
    }

    #[test]
    fn extracts_remote_trace_id_from_traceparent() {
        ensure_propagator();
        let cx =
            remote_context(&headers_with_traceparent(TRACEPARENT)).expect("valid traceparent");
        assert_eq!(cx.span().span_context().trace_id().to_string(), TRACE_ID_HEX);
        assert!(cx.span().span_context().is_remote(), "parent is a remote span");
    }

    #[test]
    fn none_when_no_traceparent_present() {
        ensure_propagator();
        assert!(
            remote_context(&HeaderMap::new()).is_none(),
            "no traceparent => span stays a root"
        );
    }

    #[test]
    fn none_when_traceparent_malformed() {
        ensure_propagator();
        assert!(
            remote_context(&headers_with_traceparent("not-a-valid-traceparent")).is_none(),
            "malformed traceparent => span stays a root"
        );
    }
}
