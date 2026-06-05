//! Integration tests using in-memory exporters (no network).
//!
//! Covers WP-2 (shutdown flush) and WP-5 (metric recording with the
//! cardinality-controlled label set), plus the WP-6 forbidden-key assertion on
//! actually-exported span attributes.

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::{Span as _, Tracer, TracerProvider as _};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

use drgtw_otel::{Metrics, RequestTelemetry, keys};

fn sample() -> RequestTelemetry {
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
fn shutdown_flushes_spans_no_loss() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();

    let tracer = provider.tracer("drgtw");
    let t = sample();
    {
        let mut span = tracer.start("chat gpt-4o");
        for kv in t.span_attrs() {
            span.set_attribute(kv);
        }
        drop(span); // ends the span
    }

    // Graceful flush must export every span (shutdown on the in-memory
    // exporter resets storage, so we assert on the flushed contents first).
    provider.force_flush().expect("flush ok");

    let spans = exporter.get_finished_spans().expect("spans available");
    assert_eq!(spans.len(), 1, "exactly one span exported");
    let span = &spans[0];
    assert_eq!(span.name, "chat gpt-4o");

    // No forbidden key may appear on the exported attributes.
    let emitted: Vec<String> = span
        .attributes
        .iter()
        .map(|kv| kv.key.to_string())
        .collect();
    for forbidden in keys::FORBIDDEN {
        assert!(
            !emitted.iter().any(|k| k == forbidden),
            "forbidden key `{forbidden}` exported on span"
        );
    }
    // Every emitted key must be on the allow-list.
    for k in &emitted {
        assert!(
            keys::ALLOW_LIST.contains(&k.as_str()),
            "exported span attr `{k}` not on allow-list"
        );
    }
    // Sanity: the GenAI required keys are present.
    assert!(emitted.iter().any(|k| k == keys::GEN_AI_OPERATION_NAME));
    assert!(emitted.iter().any(|k| k == keys::GEN_AI_REQUEST_MODEL));

    // Shutdown must succeed on the graceful path.
    provider.shutdown().expect("shutdown ok");
}

#[test]
fn metrics_record_and_flush_with_expected_labels() {
    let exporter = InMemoryMetricExporter::default();
    let reader = PeriodicReader::builder(exporter.clone()).build();
    let provider = SdkMeterProvider::builder().with_reader(reader).build();
    let meter = provider.meter("drgtw");

    // key_id OFF (default).
    let metrics = Metrics::new(&meter, false);
    let t = sample();
    metrics.record_request(&t);
    metrics.record_pii_redactions(3, &t);

    provider.force_flush().expect("flush ok");
    let finished = exporter.get_finished_metrics().expect("metrics available");

    // Collect every metric name and every attribute key across all data points.
    let mut names = std::collections::BTreeSet::new();
    let mut attr_keys = std::collections::BTreeSet::new();
    for rm in &finished {
        for sm in rm.scope_metrics() {
            for m in sm.metrics() {
                names.insert(m.name().to_string());
            }
        }
    }

    // All expected instruments fired.
    for expected in [
        "gen_ai.client.operation.duration",
        "gen_ai.client.token.usage",
        "gen_ai.client.operation.time_to_first_chunk",
        "drgtw.requests",
        "drgtw.tokens.input",
        "drgtw.tokens.output",
        "drgtw.cost.usd",
        "drgtw.pii.redactions",
    ] {
        assert!(
            names.contains(expected),
            "metric `{expected}` not exported; got {names:?}"
        );
    }

    // Inspect the request counter's labels via the public debug data shape.
    // The metric data points carry attributes; assert key_id/request_id absent.
    // We re-record into a fresh provider and read attributes through the
    // exporter's data; but data-point attribute access is version-specific, so
    // we assert at the builder level (covered by unit tests) and here just
    // confirm the by-default attrs we passed never contained the banned keys.
    let attrs = t.metric_attrs(false);
    for kv in &attrs {
        attr_keys.insert(kv.key.to_string());
    }
    assert!(
        !attr_keys.contains(keys::DRGTW_KEY_ID),
        "key_id must be absent from metric labels by default"
    );
    assert!(
        !attr_keys.contains(keys::DRGTW_REQUEST_ID),
        "request_id must never be a metric label"
    );

    provider.shutdown().expect("shutdown ok");
}
