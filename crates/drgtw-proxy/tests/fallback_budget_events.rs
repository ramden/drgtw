//! Integration tests for WP 8.3: connection fallback, per-key budgets, and
//! usage-event emission.
//!
//! All upstreams (and the event sink) are mocked via wiremock. Requests run
//! through the full axum router via `tower::ServiceExt::oneshot`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::{
    ApiFormat, Budget, Config, Connection, EventsConfig, ModelCost, PiiConfig, ServerConfig,
    VirtualKey,
};
use drgtw_proxy::{ProxyState, router};
use serde_json::Value;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as WmRequest, Respond, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn server_config() -> ServerConfig {
    ServerConfig {
        bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
        ..Default::default()
    }
}

fn openai_conn(name: &str, base: &str, costs: Vec<(&str, f64, f64)>) -> Connection {
    let model_costs = costs
        .into_iter()
        .map(|(m, i, o)| {
            (
                m.to_string(),
                ModelCost {
                    input_per_1m: i,
                    output_per_1m: o,
                },
            )
        })
        .collect();
    Connection {
        name: name.into(),
        base_url: format!("{base}/v1"),
        api_key: format!("{name}-key"),
        format: ApiFormat::OpenAi,
        models: vec!["gpt-4o".into()],
        model_costs,
        region: None,
        aws_access_key_id: None,
        aws_secret_access_key: None,
        aws_session_token: None,
    }
}

fn test_router(config: Arc<Config>) -> axum::Router {
    let state =
        Arc::new(ProxyState::new(config, std::path::Path::new(".")).expect("ProxyState::new"));
    router(state)
}

fn chat_request(virtual_key: &str, body: impl Into<String>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {virtual_key}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.into()))
        .unwrap()
}

async fn collect_body(resp: axum::response::Response) -> bytes::Bytes {
    use http_body_util::BodyExt;
    resp.into_body().collect().await.unwrap().to_bytes()
}

/// wiremock responder that records every received event body as parsed JSON.
#[derive(Clone)]
struct EventCapture(Arc<Mutex<Vec<Value>>>);

impl Respond for EventCapture {
    fn respond(&self, req: &WmRequest) -> ResponseTemplate {
        if let Ok(v) = serde_json::from_slice::<Value>(&req.body) {
            self.0.lock().unwrap().push(v);
        }
        ResponseTemplate::new(200)
    }
}

/// Mount an event-sink receiver on `server` at `/events`; return captured-events handle.
async fn mount_event_sink(server: &MockServer) -> Arc<Mutex<Vec<Value>>> {
    let captured = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(EventCapture(Arc::clone(&captured)))
        .mount(server)
        .await;
    captured
}

/// Poll until at least `n` events have been captured, or timeout.
async fn wait_for_events(captured: &Arc<Mutex<Vec<Value>>>, n: usize) {
    for _ in 0..50 {
        if captured.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn events_config(sink_url: String) -> EventsConfig {
    EventsConfig {
        url: sink_url,
        auth_bearer: None,
        buffer_size: 64,
        timeout_ms: 5_000,
        signing_secret: None,
    }
}

// ---------------------------------------------------------------------------
// 1. Fallback: candidate1 503 → candidate2 200, event records attempt + conn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_fallback_503_then_200_records_attempt_and_connection() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;
    let sink = MockServer::start().await;

    // Primary always 503 (retriable).
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&primary)
        .await;

    // Secondary 200 with usage.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "id": "ok",
                    "usage": {"prompt_tokens": 100, "completion_tokens": 50}
                }))
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&secondary)
        .await;

    let captured = mount_event_sink(&sink).await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![
            openai_conn("primary", &primary.uri(), vec![("gpt-4o", 1.0, 2.0)]),
            openai_conn("secondary", &secondary.uri(), vec![("gpt-4o", 1.0, 2.0)]),
        ],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-fb01".into(),
            connections: vec!["primary".into(), "secondary".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: PiiConfig::default(),
        events: Some(events_config(format!("{}/events", sink.uri()))),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-fb01",
            r#"{"model":"gpt-4o","messages":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    primary.verify().await;
    secondary.verify().await;

    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events.len(), 1, "exactly one usage event");
    let ev = &events[0];
    assert_eq!(
        ev["fallback_attempts"], 1,
        "one retriable attempt before success"
    );
    assert_eq!(ev["connection"], "secondary");
    assert_eq!(ev["input_tokens"], 100);
    assert_eq!(ev["output_tokens"], 50);
    // cost = 100/1e6 * 1.0 + 50/1e6 * 2.0 = 0.0001 + 0.0001 = 0.0002
    let cost = ev["cost_usd"].as_f64().unwrap();
    assert!((cost - 0.0002).abs() < 1e-9, "cost={cost}");
    assert_eq!(ev["status"], 200);
    assert_eq!(ev["streamed"], false);
}

// ---------------------------------------------------------------------------
// 2. Fallback disabled → 503 relayed, no second attempt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_fallback_disabled_relays_503_no_second_attempt() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&primary)
        .await;

    // Must NOT be hit when fallback is disabled.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&secondary)
        .await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![
            openai_conn("primary", &primary.uri(), vec![]),
            openai_conn("secondary", &secondary.uri(), vec![]),
        ],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-fb02".into(),
            connections: vec!["primary".into(), "secondary".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: drgtw_config::FallbackConfig { enabled: false },
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-fb02",
            r#"{"model":"gpt-4o","messages":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    primary.verify().await;
    secondary.verify().await;
}

// ---------------------------------------------------------------------------
// 3. Non-retriable 400 → relayed, no failover
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_non_retriable_400_no_failover() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string(r#"{"error":{"message":"bad"}}"#)
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&primary)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&secondary)
        .await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![
            openai_conn("primary", &primary.uri(), vec![]),
            openai_conn("secondary", &secondary.uri(), vec![]),
        ],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-fb03".into(),
            connections: vec!["primary".into(), "secondary".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-fb03",
            r#"{"model":"gpt-4o","messages":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    primary.verify().await;
    secondary.verify().await;
}

// ---------------------------------------------------------------------------
// 4. All candidates fail (503, 503) → last error relayed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_all_candidates_fail_relays_last() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&primary)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(504)
                .set_body_string("gateway timeout")
                .insert_header("content-type", "text/plain"),
        )
        .expect(1)
        .mount(&secondary)
        .await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![
            openai_conn("primary", &primary.uri(), vec![]),
            openai_conn("secondary", &secondary.uri(), vec![]),
        ],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-fb04".into(),
            connections: vec!["primary".into(), "secondary".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-fb04",
            r#"{"model":"gpt-4o","messages":[]}"#,
        ))
        .await
        .unwrap();
    // Last candidate's 504 is relayed verbatim (it is the final candidate).
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);

    primary.verify().await;
    secondary.verify().await;
}

// ---------------------------------------------------------------------------
// 5. Budget: first request consumes over the cap → second 429 insufficient_budget
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_budget_exhausted_after_first_request() {
    let upstream = MockServer::start().await;

    // Usage makes cost = 1M/1e6 * 10 + 1M/1e6 * 30 = 10 + 30 = 40 USD,
    // which blows a 0.01 USD budget after the first request.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "id": "ok",
                    "usage": {"prompt_tokens": 1000000, "completion_tokens": 1000000}
                }))
                .insert_header("content-type", "application/json"),
        )
        // Only the first request reaches upstream; the second is blocked by budget.
        .expect(1)
        .mount(&upstream)
        .await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![openai_conn(
            "openai",
            &upstream.uri(),
            vec![("gpt-4o", 10.0, 30.0)],
        )],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-budget01".into(),
            connections: vec!["openai".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: Some(Budget {
                max_usd: 0.01,
                per_seconds: 3600,
            }),
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);

    // First request: succeeds and records spend well over the cap.
    let resp1 = app
        .clone()
        .oneshot(chat_request(
            "sk-drgtw-budget01",
            r#"{"model":"gpt-4o","messages":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);

    // Second request: budget now exhausted → 429 insufficient_budget.
    let resp2 = app
        .oneshot(chat_request(
            "sk-drgtw-budget01",
            r#"{"model":"gpt-4o","messages":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        resp2.headers().contains_key("retry-after"),
        "retry-after header present"
    );

    let body: Value = serde_json::from_slice(&collect_body(resp2).await).unwrap();
    assert_eq!(body["error"]["code"], "insufficient_budget", "body: {body}");
    assert_eq!(body["error"]["type"], "rate_limit_error", "body: {body}");

    upstream.verify().await;
}

// ---------------------------------------------------------------------------
// 6. Events: non-stream event JSON with correct tokens/cost/model/key_id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_event_non_stream_fields() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "id": "ok",
                    "usage": {"prompt_tokens": 200, "completion_tokens": 80}
                }))
                .insert_header("content-type", "application/json"),
        )
        .mount(&upstream)
        .await;

    let captured = mount_event_sink(&sink).await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![openai_conn(
            "openai",
            &upstream.uri(),
            vec![("gpt-4o", 2.5, 10.0)],
        )],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-ev01".into(),
            connections: vec!["openai".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: PiiConfig::default(),
        events: Some(events_config(format!("{}/events", sink.uri()))),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-ev01",
            r#"{"model":"gpt-4o","messages":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev["key_id"], "vk-0");
    assert_eq!(ev["endpoint"], "chat_completions");
    assert_eq!(ev["model"], "gpt-4o");
    assert_eq!(ev["connection"], "openai");
    assert_eq!(ev["input_tokens"], 200);
    assert_eq!(ev["output_tokens"], 80);
    assert_eq!(ev["fallback_attempts"], 0);
    assert_eq!(ev["streamed"], false);
    // cost = 200/1e6*2.5 + 80/1e6*10 = 0.0005 + 0.0008 = 0.0013
    let cost = ev["cost_usd"].as_f64().unwrap();
    assert!((cost - 0.0013).abs() < 1e-9, "cost={cost}");
}

// ---------------------------------------------------------------------------
// 7. Events: streaming OpenAI (include_usage final chunk) captured at end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_event_stream_openai_usage() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;

    let sse = concat!(
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n",
        "data: [DONE]\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse.as_bytes().to_vec(), "text/event-stream")
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&upstream)
        .await;

    let captured = mount_event_sink(&sink).await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![openai_conn(
            "openai",
            &upstream.uri(),
            vec![("gpt-4o", 1.0, 1.0)],
        )],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-ev02".into(),
            connections: vec!["openai".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: PiiConfig::default(),
        events: Some(events_config(format!("{}/events", sink.uri()))),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-ev02",
            r#"{"model":"gpt-4o","messages":[],"stream":true}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Drain the stream so the tap reaches completion and emits.
    let body = collect_body(resp).await;
    assert_eq!(
        body.as_ref(),
        sse.as_bytes(),
        "stream must pass through byte-identical"
    );

    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev["streamed"], true);
    assert_eq!(ev["input_tokens"], 11);
    assert_eq!(ev["output_tokens"], 4);
    let cost = ev["cost_usd"].as_f64().unwrap();
    assert!((cost - 0.000015).abs() < 1e-12, "cost={cost}");
}

// ---------------------------------------------------------------------------
// 8. Events: streaming Anthropic (message_start + message_delta) captured
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_event_stream_anthropic_usage() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;

    let sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":30,\"output_tokens\":0}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":12}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse.as_bytes().to_vec(), "text/event-stream")
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&upstream)
        .await;

    let captured = mount_event_sink(&sink).await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![Connection {
            name: "anthropic".into(),
            base_url: upstream.uri(), // Anthropic: no /v1 suffix
            api_key: "anthropic-key".into(),
            format: ApiFormat::Anthropic,
            models: vec!["claude-3-5-sonnet".into()],
            model_costs: [(
                "claude-3-5-sonnet".to_string(),
                ModelCost {
                    input_per_1m: 3.0,
                    output_per_1m: 15.0,
                },
            )]
            .into_iter()
            .collect(),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-ev03".into(),
            connections: vec!["anthropic".into()],
            models: Some(vec!["claude-3-5-sonnet".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: PiiConfig::default(),
        events: Some(events_config(format!("{}/events", sink.uri()))),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("Authorization", "Bearer sk-drgtw-ev03")
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"model":"claude-3-5-sonnet","messages":[],"stream":true}"#,
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let _ = collect_body(resp).await; // drain to drive tap completion

    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev["endpoint"], "messages");
    assert_eq!(ev["streamed"], true);
    assert_eq!(ev["input_tokens"], 30);
    assert_eq!(ev["output_tokens"], 12);
    // cost = 30/1e6*3 + 12/1e6*15 = 0.00009 + 0.00018 = 0.00027
    let cost = ev["cost_usd"].as_f64().unwrap();
    assert!((cost - 0.00027).abs() < 1e-12, "cost={cost}");
}

// ---------------------------------------------------------------------------
// 9. Auth failure emits NO event
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_failure_emits_no_event() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;

    // Upstream should never be hit on an auth failure.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;

    // Sink should receive zero events.
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&sink)
        .await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![openai_conn(
            "openai",
            &upstream.uri(),
            vec![("gpt-4o", 1.0, 1.0)],
        )],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-ev04".into(),
            connections: vec!["openai".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: PiiConfig::default(),
        events: Some(events_config(format!("{}/events", sink.uri()))),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    // Unknown virtual key → 401 before any upstream/event activity.
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-doesnotexist",
            r#"{"model":"gpt-4o","messages":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Give any (erroneous) event a chance to be delivered before verifying.
    tokio::time::sleep(Duration::from_millis(150)).await;
    upstream.verify().await;
    sink.verify().await;
}

// ---------------------------------------------------------------------------
// 10. Upstream error path (500) still emits an event (post-auth)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_upstream_error_emits_event() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;

    // 500 is non-retriable → relayed; event emitted with status 500, no tokens.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_string("boom")
                .insert_header("content-type", "text/plain"),
        )
        .mount(&upstream)
        .await;

    let captured = mount_event_sink(&sink).await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![openai_conn(
            "openai",
            &upstream.uri(),
            vec![("gpt-4o", 1.0, 1.0)],
        )],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-ev05".into(),
            connections: vec!["openai".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: PiiConfig::default(),
        events: Some(events_config(format!("{}/events", sink.uri()))),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-ev05",
            r#"{"model":"gpt-4o","messages":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev["status"], 500);
    assert!(ev["input_tokens"].is_null());
    assert!(ev["cost_usd"].is_null());
}
