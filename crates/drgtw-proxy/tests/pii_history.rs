//! Integration tests for the two history-wiring gaps:
//!
//! - GAP 1: EventSink signs webhook POSTs with HMAC-SHA256 when
//!   `EventsConfig.signing_secret` is set, and the header
//!   `X-Drgtw-Signature: sha256=<hex>` is present on every delivery.
//! - GAP 2: PII detection counts are recorded per entity kind when a
//!   PII-bearing request is proxied (verified via `UsageEvent.pii = true`
//!   and by checking the `pii_kind_counts` helper indirectly via the
//!   integration path).
//!
//! All upstreams and the event sink are mocked via wiremock.
//! Requests run through the full axum router via `tower::ServiceExt::oneshot`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::{ApiFormat, Config, Connection, EventsConfig, PiiConfig, ServerConfig, VirtualKey};
use drgtw_proxy::{ProxyState, router};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as WmRequest, Respond, ResponseTemplate};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn server_cfg() -> ServerConfig {
    ServerConfig {
        bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
        ..Default::default()
    }
}

fn openai_conn(name: &str, base: &str) -> Connection {
    Connection {
        name: name.into(),
        base_url: format!("{base}/v1"),
        api_key: format!("{name}-key"),
        format: ApiFormat::OpenAi,
        models: vec!["gpt-4o".into()],
        model_costs: Default::default(),
        region: None,
        aws_access_key_id: None,
        aws_secret_access_key: None,
        aws_session_token: None,
    }
}

fn chat_request(virtual_key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {virtual_key}"))
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}]
        }).to_string()))
        .unwrap()
}

fn chat_request_with_pii(virtual_key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {virtual_key}"))
        .header("Content-Type", "application/json")
        .header("x-drgtw-pii", "on")
        .body(Body::from(serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content":
                "Email max.mustermann@example.com, phone +49 89 1234567, \
                 IBAN DE89370400440532013000, card 4111 1111 1111 1111."
            }]
        }).to_string()))
        .unwrap()
}

fn ok_upstream_response() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(serde_json::json!({
            "id": "chatcmpl-test",
            "choices": [{"message": {"role": "assistant", "content": "Hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 20, "completion_tokens": 5}
        }))
        .insert_header("content-type", "application/json")
}

async fn poll_until<F: Fn() -> bool>(f: F) {
    for _ in 0..100 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// GAP 1 — signing header
//
// An EventSink built with `signing_secret` must include
// `X-Drgtw-Signature: sha256=<lowercase hex>` on every webhook POST.
// We verify the header is present and non-empty; the HMAC value itself is
// verified by the drgtw-events unit tests.
// ---------------------------------------------------------------------------

/// Responder that records every raw wiremock request.
#[derive(Clone)]
struct RequestCapture(Arc<Mutex<Vec<WmRequest>>>);

impl Respond for RequestCapture {
    fn respond(&self, req: &WmRequest) -> ResponseTemplate {
        self.0.lock().unwrap().push(req.clone());
        ResponseTemplate::new(200)
    }
}

#[tokio::test]
async fn event_sink_adds_signature_header_when_secret_set() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ok_upstream_response())
        .mount(&upstream)
        .await;

    let captured = Arc::new(Mutex::new(Vec::<WmRequest>::new()));
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(RequestCapture(Arc::clone(&captured)))
        .mount(&sink)
        .await;

    let config = Arc::new(Config {
        server: server_cfg(),
        connections: vec![openai_conn("mock", &upstream.uri())],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-signtest01".into(),
            connections: vec!["mock".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
        }],
        pii: PiiConfig::default(),
        events: Some(EventsConfig {
            url: format!("{}/events", sink.uri()),
            auth_bearer: None,
            buffer_size: 64,
            timeout_ms: 5_000,
            signing_secret: Some("test-signing-secret-abc".into()),
        }),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let state = Arc::new(
        ProxyState::new(Arc::clone(&config), std::path::Path::new("."))
            .expect("ProxyState::new"),
    );
    let app = router(state);
    let resp = app.oneshot(chat_request("sk-drgtw-signtest01")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    poll_until(|| !captured.lock().unwrap().is_empty()).await;

    let reqs = captured.lock().unwrap().clone();
    assert!(!reqs.is_empty(), "event sink received at least one POST");

    let sig_header = reqs[0]
        .headers
        .get("x-drgtw-signature")
        .map(|v| v.to_str().unwrap_or(""))
        .unwrap_or("");

    assert!(
        sig_header.starts_with("sha256="),
        "X-Drgtw-Signature must start with 'sha256=', got: {sig_header:?}"
    );
    // hex after 'sha256=' must be non-empty lowercase hex (64 chars for SHA-256)
    let hex_part = &sig_header["sha256=".len()..];
    assert_eq!(hex_part.len(), 64, "SHA-256 hex must be 64 chars, got {}", hex_part.len());
    assert!(
        hex_part.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
        "SHA-256 hex must be lowercase, got: {hex_part}"
    );
}

#[tokio::test]
async fn event_sink_no_signature_header_when_secret_absent() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ok_upstream_response())
        .mount(&upstream)
        .await;

    let captured = Arc::new(Mutex::new(Vec::<WmRequest>::new()));
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(RequestCapture(Arc::clone(&captured)))
        .mount(&sink)
        .await;

    let config = Arc::new(Config {
        server: server_cfg(),
        connections: vec![openai_conn("mock", &upstream.uri())],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-nosigtest01".into(),
            connections: vec!["mock".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
        }],
        pii: PiiConfig::default(),
        events: Some(EventsConfig {
            url: format!("{}/events", sink.uri()),
            auth_bearer: None,
            buffer_size: 64,
            timeout_ms: 5_000,
            signing_secret: None, // no secret
        }),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let state = Arc::new(
        ProxyState::new(Arc::clone(&config), std::path::Path::new("."))
            .expect("ProxyState::new"),
    );
    let app = router(state);
    let resp = app.oneshot(chat_request("sk-drgtw-nosigtest01")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    poll_until(|| !captured.lock().unwrap().is_empty()).await;

    let reqs = captured.lock().unwrap().clone();
    assert!(!reqs.is_empty(), "event sink received at least one POST");
    assert!(
        reqs[0].headers.get("x-drgtw-signature").is_none(),
        "X-Drgtw-Signature must be absent when no secret configured"
    );
}

// ---------------------------------------------------------------------------
// GAP 2 — PII event flag
//
// A request containing PII (email + phone + IBAN + card) must result in
// a UsageEvent with `pii = true` emitted to the event sink. The per-kind
// detection rows are fire-and-forget to a history store (which is not
// connected in unit tests), so we verify the event flag as the observable
// proxy of the PII pipeline firing correctly.
// ---------------------------------------------------------------------------

/// Capture event bodies as JSON values.
#[derive(Clone)]
struct BodyCapture(Arc<Mutex<Vec<serde_json::Value>>>);

impl Respond for BodyCapture {
    fn respond(&self, req: &WmRequest) -> ResponseTemplate {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&req.body) {
            self.0.lock().unwrap().push(v);
        }
        ResponseTemplate::new(200)
    }
}

#[tokio::test]
async fn pii_request_sets_pii_flag_in_usage_event() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ok_upstream_response())
        .mount(&upstream)
        .await;

    let captured = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(BodyCapture(Arc::clone(&captured)))
        .mount(&sink)
        .await;

    let config = Arc::new(Config {
        server: server_cfg(),
        connections: vec![openai_conn("mock", &upstream.uri())],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-piihistory01".into(),
            connections: vec!["mock".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
        }],
        pii: PiiConfig {
            enabled_by_default: true,
            disabled_recognizers: vec![],
            custom_recognizers: vec![],
            entities: None,
            ner: None,
            vault: None,
            embeddings_require_vault: false,
            require_ner: false,
        },
        events: Some(EventsConfig {
            url: format!("{}/events", sink.uri()),
            auth_bearer: None,
            buffer_size: 64,
            timeout_ms: 5_000,
            signing_secret: None,
        }),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let state = Arc::new(
        ProxyState::new(Arc::clone(&config), std::path::Path::new("."))
            .expect("ProxyState::new"),
    );
    let app = router(state);
    let resp = app.oneshot(chat_request_with_pii("sk-drgtw-piihistory01")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    poll_until(|| !captured.lock().unwrap().is_empty()).await;

    let events = captured.lock().unwrap().clone();
    assert!(!events.is_empty(), "event sink received at least one event");
    let pii_flag = events[0].get("pii").and_then(|v| v.as_bool()).unwrap_or(false);
    assert!(pii_flag, "UsageEvent.pii must be true for a PII-bearing request; event: {}", events[0]);
}

#[tokio::test]
async fn non_pii_request_pii_flag_false() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ok_upstream_response())
        .mount(&upstream)
        .await;

    let captured = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(BodyCapture(Arc::clone(&captured)))
        .mount(&sink)
        .await;

    let config = Arc::new(Config {
        server: server_cfg(),
        connections: vec![openai_conn("mock", &upstream.uri())],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-nopii01".into(),
            connections: vec!["mock".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
        }],
        pii: PiiConfig {
            enabled_by_default: true,
            ..Default::default()
        },
        events: Some(EventsConfig {
            url: format!("{}/events", sink.uri()),
            auth_bearer: None,
            buffer_size: 64,
            timeout_ms: 5_000,
            signing_secret: None,
        }),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let state = Arc::new(
        ProxyState::new(Arc::clone(&config), std::path::Path::new("."))
            .expect("ProxyState::new"),
    );
    let app = router(state);
    // Plain request with no PII.
    let resp = app.oneshot(chat_request("sk-drgtw-nopii01")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    poll_until(|| !captured.lock().unwrap().is_empty()).await;

    let events = captured.lock().unwrap().clone();
    assert!(!events.is_empty(), "event sink received at least one event");
    let pii_flag = events[0].get("pii").and_then(|v| v.as_bool()).unwrap_or(true);
    assert!(!pii_flag, "UsageEvent.pii must be false when no PII detected; event: {}", events[0]);
}
