//! Span-enrichment integration test for OpenTelemetry.
//!
//! Lives in its OWN test binary (= own process) deliberately: `tracing`
//! caches per-callsite interest process-wide, so when sibling tests hit the
//! `proxy_request` callsite before a subscriber exists, a later
//! `set_default`-scoped subscriber in the same process can race the interest
//! cache and see no spans. One process, one subscriber — deterministic.
//!
//! Privacy invariant under test: every dotted span attribute key must be on
//! the `drgtw_otel::keys::ALLOW_LIST`, and prompt text must never appear in
//! any attribute value.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::{ApiFormat, Config, Connection, ModelCost, PiiConfig, ServerConfig, VirtualKey};
use drgtw_otel::keys;
use drgtw_proxy::{ProxyState, router};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use serde_json::json;
use tower::ServiceExt; // for `.oneshot()`
use tracing_subscriber::layer::SubscriberExt as _;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const VKEY: &str = "sk-drgtw-oteltest01";
const PROMPT: &str = "my extremely private prompt text";

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
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: Default::default(),
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
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

/// The request span is exported with GenAI attributes, every dotted attribute
/// key is allow-listed, and no attribute value contains the prompt text.
#[tokio::test]
async fn span_enriched_with_allow_listed_attrs_only() {
    let upstream = MockServer::start().await;
    mount_chat_ok(&upstream).await;

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry().with(drgtw_otel::tracer_layer(&provider));
    let _guard = tracing::subscriber::set_default(subscriber);

    let state = Arc::new(
        ProxyState::new(chat_config(&upstream.uri()), std::path::Path::new("."))
            .expect("ProxyState::new"),
    );
    let resp = router(state).oneshot(chat_request()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    provider.force_flush().expect("force_flush");
    let spans = exporter.get_finished_spans().expect("finished spans");
    let req_span = spans
        .iter()
        .find(|s| s.name == "proxy_request")
        .expect("proxy_request span exported");

    // Dotted (set_attribute) keys must be allow-listed; `tracing` field keys
    // (no dots) come from the info_span! macro and are span-local context.
    let mut by_key: HashMap<String, String> = HashMap::new();
    for kv in &req_span.attributes {
        let k = kv.key.to_string();
        let v = format!("{:?}", kv.value);
        assert!(
            !v.contains(PROMPT),
            "span attribute `{k}` leaked prompt text"
        );
        if k.contains('.') {
            assert!(
                keys::ALLOW_LIST.contains(&k.as_str()),
                "span attr `{k}` is not on the allow-list"
            );
        }
        by_key.insert(k, v);
    }

    for expected in [
        keys::GEN_AI_OPERATION_NAME,
        keys::GEN_AI_PROVIDER_NAME,
        keys::GEN_AI_REQUEST_MODEL,
        keys::GEN_AI_USAGE_INPUT_TOKENS,
        keys::GEN_AI_USAGE_OUTPUT_TOKENS,
        keys::DRGTW_CONNECTION,
        keys::DRGTW_COST_USD,
        keys::STATUS,
    ] {
        assert!(by_key.contains_key(expected), "span attr `{expected}` missing: {by_key:?}");
    }
    assert!(by_key[keys::GEN_AI_REQUEST_MODEL].contains("gpt-4o"));
    assert!(by_key[keys::GEN_AI_PROVIDER_NAME].contains("openai"));
}
