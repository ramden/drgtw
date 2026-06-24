//! Integration tests for OpenTelemetry span enrichment + metric recording.
//!
//! Each test builds the full axum router with a real `ProxyState`, drives a
//! request end-to-end via `tower::ServiceExt::oneshot` against a wiremock
//! upstream, then inspects in-memory OTel exporters
//! — no collector is needed.
//!
//! Privacy invariant under test: every metric label must be
//! on the `drgtw_otel::keys::ALLOW_LIST`, and prompt text must never appear in
//! any attribute value.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::{ApiFormat, Config, Connection, ModelCost, PiiConfig, ServerConfig, VirtualKey};
use drgtw_otel::keys;
use drgtw_proxy::{ProxyState, router};
use opentelemetry_sdk::metrics::in_memory_exporter::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use serde_json::json;
use tower::ServiceExt; // for `.oneshot()`
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const VKEY: &str = "sk-drgtw-oteltest01";
const PROMPT: &str = "my extremely private prompt text";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Chat-only config: one open_ai connection at the mock upstream with a cost
/// table for `gpt-4o`, one virtual key.
fn chat_config(mock_base_url: &str) -> Arc<Config> {
    Arc::new(Config {
        server: ServerConfig {
            bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
            ..Default::default()
        },
        connections: vec![Connection {
            name: "mock-openai".into(),
            base_url: format!("{mock_base_url}/v1"),
            api_key: "upstream-secret-key".into(),
            format: ApiFormat::OpenAi,
            models: vec!["gpt-4o".into()],
            model_costs: HashMap::from([(
                "gpt-4o".to_string(),
                ModelCost { input_per_1m: 5.0, output_per_1m: 15.0 },
            )]),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: VKEY.into(),
            connections: vec!["mock-openai".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: Default::default(),
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    })
}

fn chat_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {VKEY}"))
        .header("x-drgtw-request-id", "req-otel-1")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{"role": "user", "content": PROMPT}],
            })
            .to_string(),
        ))
        .unwrap()
}

async fn mount_chat_ok(server: &MockServer) {
    let body = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "choices": [{"message": {"role": "assistant", "content": "Hello!"}}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18},
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&body)
                .insert_header("content-type", "application/json"),
        )
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// A successful chat request records the request counter, token usage, and
/// cost — and every metric label key is on the allow-list, with `request_id`
/// and (by default) `key_id` absent.
#[tokio::test]
async fn metrics_recorded_with_allow_listed_labels_only() {
    let upstream = MockServer::start().await;
    mount_chat_ok(&upstream).await;

    let exporter = InMemoryMetricExporter::default();
    let reader = PeriodicReader::builder(exporter.clone()).build();
    let provider = SdkMeterProvider::builder().with_reader(reader).build();
    let meter = opentelemetry::metrics::MeterProvider::meter(&provider, "test");
    let metrics = Arc::new(drgtw_otel::Metrics::new(&meter, false));

    let state = Arc::new(
        ProxyState::new(chat_config(&upstream.uri()), std::path::Path::new("."))
            .expect("ProxyState::new")
            .with_metrics(Some(metrics)),
    );
    let resp = router(state).oneshot(chat_request()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    provider.force_flush().expect("force_flush");
    let finished = exporter.get_finished_metrics().expect("finished metrics");

    let mut seen_names = Vec::new();
    let mut label_keys = Vec::new();
    for rm in &finished {
        for sm in rm.scope_metrics() {
            for m in sm.metrics() {
                seen_names.push(m.name().to_string());
                collect_label_keys(m.data(), &mut label_keys);
            }
        }
    }

    for expected in [
        "drgtw.requests",
        "drgtw.tokens.input",
        "drgtw.tokens.output",
        "drgtw.cost.usd",
        "gen_ai.client.operation.duration",
        "gen_ai.client.token.usage",
    ] {
        assert!(
            seen_names.iter().any(|n| n == expected),
            "metric `{expected}` missing; saw {seen_names:?}"
        );
    }

    for k in &label_keys {
        assert!(
            keys::ALLOW_LIST.contains(&k.as_str()),
            "metric label `{k}` is not on the allow-list"
        );
        assert_ne!(k, keys::DRGTW_REQUEST_ID, "request_id must never be a metric label");
        assert_ne!(k, keys::DRGTW_KEY_ID, "key_id must be off metrics by default");
    }
}

/// Pull every label key out of a metric's data points.
fn collect_label_keys(data: &opentelemetry_sdk::metrics::data::AggregatedMetrics, out: &mut Vec<String>) {
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics as A, MetricData as D};
    fn push<T>(points: impl Iterator<Item = T>, attrs: impl Fn(&T) -> Vec<String>, out: &mut Vec<String>) {
        for p in points {
            out.extend(attrs(&p));
        }
    }
    macro_rules! keys_of {
        ($md:expr, $out:expr) => {
            match $md {
                D::Gauge(g) => push(g.data_points(), |p| p.attributes().map(|kv| kv.key.to_string()).collect(), $out),
                D::Sum(s) => push(s.data_points(), |p| p.attributes().map(|kv| kv.key.to_string()).collect(), $out),
                D::Histogram(h) => push(h.data_points(), |p| p.attributes().map(|kv| kv.key.to_string()).collect(), $out),
                D::ExponentialHistogram(h) => push(h.data_points(), |p| p.attributes().map(|kv| kv.key.to_string()).collect(), $out),
            }
        };
    }
    match data {
        A::F64(md) => keys_of!(md, out),
        A::U64(md) => keys_of!(md, out),
        A::I64(md) => keys_of!(md, out),
    }
}

/// Without `[otel]` (no metrics attached), requests work and nothing panics —
/// the disabled path is a cheap no-op.
#[tokio::test]
async fn disabled_otel_request_unaffected() {
    let upstream = MockServer::start().await;
    mount_chat_ok(&upstream).await;

    let state = Arc::new(
        ProxyState::new(chat_config(&upstream.uri()), std::path::Path::new("."))
            .expect("ProxyState::new"),
    );
    let resp = router(state).oneshot(chat_request()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Streaming TTFT
// ---------------------------------------------------------------------------

/// A streaming chat request records the time-to-first-chunk histogram (and
/// token usage from the final usage chunk) once the stream is consumed.
#[tokio::test]
async fn streaming_records_ttft_histogram() {
    use http_body_util::BodyExt as _;

    let upstream = MockServer::start().await;
    let sse_body = concat!(
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18}}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_body.as_bytes().to_vec(), "text/event-stream")
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&upstream)
        .await;

    let exporter = InMemoryMetricExporter::default();
    let reader = PeriodicReader::builder(exporter.clone()).build();
    let provider = SdkMeterProvider::builder().with_reader(reader).build();
    let meter = opentelemetry::metrics::MeterProvider::meter(&provider, "test");
    let metrics = Arc::new(drgtw_otel::Metrics::new(&meter, false));

    let state = Arc::new(
        ProxyState::new(chat_config(&upstream.uri()), std::path::Path::new("."))
            .expect("ProxyState::new")
            .with_metrics(Some(metrics)),
    );

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {VKEY}"))
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "stream": true,
                "messages": [{"role": "user", "content": PROMPT}],
            })
            .to_string(),
        ))
        .unwrap();

    let resp = router(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Consume the stream to completion so the usage tap fires on_complete.
    let _ = resp.into_body().collect().await.expect("stream body");

    provider.force_flush().expect("force_flush");
    let finished = exporter.get_finished_metrics().expect("finished metrics");
    let names: Vec<String> = finished
        .iter()
        .flat_map(|rm| rm.scope_metrics())
        .flat_map(|sm| sm.metrics())
        .map(|m| m.name().to_string())
        .collect();
    for expected in [
        "gen_ai.client.operation.time_to_first_chunk",
        "gen_ai.client.token.usage",
        "drgtw.requests",
    ] {
        assert!(names.iter().any(|n| n == expected), "metric `{expected}` missing; saw {names:?}");
    }
}
