//! Integration tests for the usage-event broadcast channel.
//!
//! Verifies that [`ProxyState::subscribe_usage`] receives a [`UsageEvent`]
//! on every completed request — including when no webhook `[events]` sink is
//! configured — and that the existing webhook-sink path is unaffected.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::{
    ApiFormat, Config, Connection, EventsConfig, ModelCost, PiiConfig, ServerConfig, VirtualKey,
};
use drgtw_proxy::{router, ProxyState};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn server_config() -> ServerConfig {
    ServerConfig {
        bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
        ..Default::default()
    }
}

fn openai_conn(name: &str, base: &str) -> Connection {
    Connection {
        name: name.to_owned(),
        base_url: format!("{base}/v1"),
        api_key: format!("{name}-key"),
        format: ApiFormat::OpenAi,
        models: vec!["gpt-4o".into()],
        model_costs: [(
            "gpt-4o".to_string(),
            ModelCost { input_per_1m: 1.0, output_per_1m: 2.0 },
        )]
        .into_iter()
        .collect(),
        region: None,
        aws_access_key_id: None,
        aws_secret_access_key: None,
        aws_session_token: None,
    }
}

fn make_config(upstream_base: &str, events: Option<EventsConfig>) -> Arc<Config> {
    let conn = openai_conn("openai", upstream_base);
    Arc::new(Config {
        server: server_config(),
        connections: vec![conn],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-bc01".into(),
            connections: vec!["openai".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: PiiConfig { enabled_by_default: false, ..Default::default() },
        events,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    })
}

fn events_config(sink_url: String) -> EventsConfig {
    EventsConfig { url: sink_url, auth_bearer: None, buffer_size: 64, timeout_ms: 5_000, signing_secret: None }
}

fn chat_request(virtual_key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {virtual_key}"))
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"model":"gpt-4o","messages":[]}"#))
        .unwrap()
}

fn ok_chat_response() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(serde_json::json!({
            "id": "r1",
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}
        }))
        .insert_header("content-type", "application/json")
}

/// Poll a broadcast receiver until one message arrives, with a short timeout.
async fn recv_with_timeout(
    rx: &mut tokio::sync::broadcast::Receiver<drgtw_events::UsageEvent>,
) -> drgtw_events::UsageEvent {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for UsageEvent")
        .expect("broadcast channel closed unexpectedly")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The broadcast fires even when no webhook `[events]` sink is configured.
#[tokio::test]
async fn test_broadcast_fires_without_webhook_sink() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ok_chat_response())
        .expect(1)
        .mount(&upstream)
        .await;

    // events: None — no webhook sink.
    let config = make_config(&upstream.uri(), None);
    let state = Arc::new(ProxyState::new(config, std::path::Path::new(".")).expect("state"));
    let mut rx = state.subscribe_usage();
    let app = router(state);

    let resp = app.oneshot(chat_request("sk-drgtw-bc01")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let ev = recv_with_timeout(&mut rx).await;
    assert_eq!(ev.endpoint, "chat_completions");
    // key_id is an opaque index-based identifier ("vk-{n}"), not the bearer token.
    assert!(ev.key_id.starts_with("vk-"), "unexpected key_id format: {}", ev.key_id);
    assert_eq!(ev.status, 200);
    assert_eq!(ev.input_tokens, Some(5));
    assert_eq!(ev.output_tokens, Some(3));

    upstream.verify().await;
}

/// The broadcast also fires when a webhook sink IS configured, and the webhook
/// sink still receives the same event (existing behavior unchanged).
#[tokio::test]
async fn test_broadcast_fires_alongside_webhook_sink() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ok_chat_response())
        .expect(1)
        .mount(&upstream)
        .await;

    // Webhook sink must receive exactly one POST.
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&sink)
        .await;

    let config =
        make_config(&upstream.uri(), Some(events_config(format!("{}/events", sink.uri()))));
    let state = Arc::new(ProxyState::new(config, std::path::Path::new(".")).expect("state"));
    let mut rx = state.subscribe_usage();
    let app = router(state);

    let resp = app.oneshot(chat_request("sk-drgtw-bc01")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let ev = recv_with_timeout(&mut rx).await;
    assert_eq!(ev.endpoint, "chat_completions");
    assert!(ev.key_id.starts_with("vk-"), "unexpected key_id format: {}", ev.key_id);
    assert_eq!(ev.status, 200);

    // Give the async webhook sink a moment to deliver, then verify count.
    tokio::time::sleep(Duration::from_millis(150)).await;
    sink.verify().await;
    upstream.verify().await;
}

/// Multiple independent subscribers each receive the same event.
#[tokio::test]
async fn test_multiple_subscribers_each_receive_event() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ok_chat_response())
        .expect(1)
        .mount(&upstream)
        .await;

    let config = make_config(&upstream.uri(), None);
    let state = Arc::new(ProxyState::new(config, std::path::Path::new(".")).expect("state"));
    let mut rx1 = state.subscribe_usage();
    let mut rx2 = state.subscribe_usage();
    let app = router(state);

    let resp = app.oneshot(chat_request("sk-drgtw-bc01")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let ev1 = recv_with_timeout(&mut rx1).await;
    let ev2 = recv_with_timeout(&mut rx2).await;
    assert_eq!(ev1.request_id, ev2.request_id);
    assert_eq!(ev1.status, 200);

    upstream.verify().await;
}
