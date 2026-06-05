//! Integration tests for the Anthropic `/v1/messages` endpoint (WP 2.1).
//!
//! These tests mirror the patterns from tests/proxy.rs but exercise:
//!   - POST /v1/messages non-streaming and streaming round-trips
//!   - Anthropic-style error body shape
//!   - Format enforcement (wrong endpoint for a given model)
//!   - anthropic-version passthrough
//!   - Rate-limit headers (TODO: blocked on drgtw-keys RateLimiter landing)
//!
//! NOTE: Tests that call through to KeyStore::authenticate / ResolvedKey
//! methods depend on the drgtw-keys implementation. They compile today and
//! pass once that crate is implemented.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::{ApiFormat, Config, Connection, PiiConfig, ServerConfig, VirtualKey};
use drgtw_proxy::{ProxyState, router};
use serde_json::Value;
use tower::ServiceExt; // for `.oneshot()`
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn default_server_config() -> ServerConfig {
    ServerConfig {
        bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
        ..Default::default()
    }
}

/// Build an Anthropic config with a tiny max_body_bytes limit (32 bytes).
fn tiny_max_body_anthropic_config(mock_base_url: &str) -> Arc<Config> {
    Arc::new(Config {
        server: ServerConfig {
            bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
            max_body_bytes: 32,
        },
        connections: vec![Connection {
            name: "mock-anthropic".into(),
            base_url: mock_base_url.to_owned(),
            api_key: "upstream-anthropic-key".into(),
            format: ApiFormat::Anthropic,
            models: vec!["claude-3-5-sonnet".into()],
            model_costs: Default::default(),
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-bodylimit-ant".into(),
            connections: vec!["mock-anthropic".into()],
            models: Some(vec!["claude-3-5-sonnet".into()]),
            rate_limit: None,
            budget: None,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
    })
}

/// Build a Config with one Anthropic connection and one virtual key.
/// base_url follows Anthropic convention: NO trailing /v1.
fn anthropic_config(mock_base_url: &str) -> Arc<Config> {
    Arc::new(Config {
        server: default_server_config(),
        connections: vec![Connection {
            name: "mock-anthropic".into(),
            // Anthropic base_url: no /v1 — the proxy appends /v1/messages itself.
            base_url: mock_base_url.to_owned(),
            api_key: "upstream-anthropic-key".into(),
            format: ApiFormat::Anthropic,
            models: vec!["claude-3-5-sonnet".into(), "claude-3-haiku".into()],
            model_costs: Default::default(),
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-anthropickey001".into(),
            connections: vec!["mock-anthropic".into()],
            models: Some(vec!["claude-3-5-sonnet".into(), "claude-3-haiku".into()]),
            rate_limit: None,
            budget: None,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
    })
}

/// Config with BOTH an OpenAI and an Anthropic connection, two virtual keys.
fn dual_config(openai_base: &str, anthropic_base: &str) -> Arc<Config> {
    Arc::new(Config {
        server: default_server_config(),
        connections: vec![
            Connection {
                name: "mock-openai".into(),
                base_url: format!("{openai_base}/v1"),
                api_key: "upstream-openai-key".into(),
                format: ApiFormat::OpenAi,
                models: vec!["gpt-4o".into()],
                model_costs: Default::default(),
            },
            Connection {
                name: "mock-anthropic".into(),
                base_url: anthropic_base.to_owned(),
                api_key: "upstream-anthropic-key".into(),
                format: ApiFormat::Anthropic,
                models: vec!["claude-3-5-sonnet".into()],
                model_costs: Default::default(),
            },
        ],
        virtual_keys: vec![
            VirtualKey {
                key: "sk-drgtw-openaikey001".into(),
                connections: vec!["mock-openai".into()],
                models: None,
                rate_limit: None,
                budget: None,
            },
            VirtualKey {
                key: "sk-drgtw-dualkey001".into(),
                connections: vec!["mock-openai".into(), "mock-anthropic".into()],
                models: None,
                rate_limit: None,
                budget: None,
            },
        ],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
    })
}

fn test_router(config: Arc<Config>) -> axum::Router {
    let state = Arc::new(
        ProxyState::new(config, std::path::Path::new(".")).expect("ProxyState::new failed in test"),
    );
    router(state)
}

/// POST /v1/messages with x-api-key auth.
fn messages_request_x_api_key(virtual_key: &str, body: impl Into<String>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", virtual_key)
        .header("Content-Type", "application/json")
        .body(Body::from(body.into()))
        .unwrap()
}

/// POST /v1/messages with Bearer auth.
fn messages_request_bearer(virtual_key: &str, body: impl Into<String>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("Authorization", format!("Bearer {virtual_key}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.into()))
        .unwrap()
}

/// Drain response body into bytes.
async fn collect_body(resp: axum::response::Response) -> bytes::Bytes {
    use http_body_util::BodyExt;
    resp.into_body().collect().await.unwrap().to_bytes()
}

// ---------------------------------------------------------------------------
// 1. Non-streaming round-trip via x-api-key auth
// ---------------------------------------------------------------------------

/// Mock asserts:
///   - upstream receives `x-api-key: upstream-anthropic-key` (NOT the virtual key)
///   - upstream receives `anthropic-version` header
///   - upstream does NOT receive `Authorization` or `x-api-key` from the client
#[tokio::test]
async fn test_messages_non_stream_round_trip_x_api_key_auth() {
    let mock_server = MockServer::start().await;

    let upstream_body = serde_json::json!({
        "id": "msg_abc123",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "Hello!"}],
        "model": "claude-3-5-sonnet",
        "stop_reason": "end_turn",
    });

    // Only matches if the upstream key is sent AND anthropic-version is present.
    // Virtual key would not match here — it starts with "sk-drgtw-".
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "upstream-anthropic-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&upstream_body)
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let app = test_router(anthropic_config(&mock_server.uri()));
    let req = messages_request_x_api_key(
        "sk-drgtw-anthropickey001",
        r#"{"model":"claude-3-5-sonnet","messages":[{"role":"user","content":"hi"}],"max_tokens":1024}"#,
    );

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = collect_body(resp).await;
    let got: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(got, upstream_body);

    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 2. Virtual key must NOT be forwarded to upstream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_messages_virtual_key_not_forwarded() {
    let mock_server = MockServer::start().await;

    // Match on the correct upstream key — passes.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "upstream-anthropic-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"ok","type":"message"}"#)
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    // If the virtual key leaked via x-api-key → fails.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-drgtw-anthropickey001"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mock_server)
        .await;

    let app = test_router(anthropic_config(&mock_server.uri()));
    let req = messages_request_x_api_key(
        "sk-drgtw-anthropickey001",
        r#"{"model":"claude-3-5-sonnet","messages":[],"max_tokens":1}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 3. Streaming byte-identical relay (text/event-stream Anthropic framing)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_messages_streaming_byte_identical_relay() {
    let mock_server = MockServer::start().await;

    // Anthropic SSE framing sample.
    let sse_body = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-3-5-sonnet","stop_reason":null}}"#,
        "\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello!"}}"#,
        "\n\n",
        "event: message_stop\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_body.as_bytes().to_vec(), "text/event-stream")
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let app = test_router(anthropic_config(&mock_server.uri()));
    let req = messages_request_x_api_key(
        "sk-drgtw-anthropickey001",
        r#"{"model":"claude-3-5-sonnet","messages":[],"max_tokens":10,"stream":true}"#,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("text/event-stream"), "content-type was: {ct}");

    let body_bytes = collect_body(resp).await;
    assert_eq!(body_bytes.as_ref(), sse_body.as_bytes());

    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 4. Missing model → 400 Anthropic-style body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_messages_missing_model_returns_400_anthropic_body() {
    let mock_server = MockServer::start().await;
    let app = test_router(anthropic_config(&mock_server.uri()));

    let req = messages_request_x_api_key(
        "sk-drgtw-anthropickey001",
        r#"{"messages":[{"role":"user","content":"hi"}],"max_tokens":10}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    // Anthropic shape: {"type":"error","error":{"type":"...","message":"..."}}
    assert_eq!(body["type"], "error", "body was: {body}");
    assert_eq!(
        body["error"]["type"], "invalid_request_error",
        "body was: {body}"
    );
    assert!(
        body["error"]["message"].as_str().unwrap().contains("model"),
        "message should mention 'model', body was: {body}"
    );
}

// ---------------------------------------------------------------------------
// 5. Format mismatch: OpenAI model via /v1/messages → 400
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_messages_format_mismatch_openai_model_via_messages_endpoint() {
    let mock_server = MockServer::start().await;
    // dual_config: gpt-4o is OpenAI, claude-3-5-sonnet is Anthropic.
    let app = test_router(dual_config(&mock_server.uri(), &mock_server.uri()));

    // Request gpt-4o (OpenAI format) via /v1/messages (Anthropic endpoint).
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("Authorization", "Bearer sk-drgtw-dualkey001")
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"max_tokens":1}"#,
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    // Must be Anthropic-style error from the /v1/messages handler.
    assert_eq!(body["type"], "error", "body was: {body}");
    assert_eq!(
        body["error"]["type"], "invalid_request_error",
        "body was: {body}"
    );
    // Message must mention the model and the correct endpoint.
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("gpt-4o"),
        "message should mention model, got: {msg}"
    );
    assert!(
        msg.contains("/v1/chat/completions"),
        "message should suggest correct endpoint, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// 6. Format mismatch: Anthropic model via /v1/chat/completions → 400
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_chat_completions_format_mismatch_anthropic_model() {
    let mock_server = MockServer::start().await;
    // dual_config: claude-3-5-sonnet is Anthropic, gpt-4o is OpenAI.
    let app = test_router(dual_config(&mock_server.uri(), &mock_server.uri()));

    // Request claude-3-5-sonnet (Anthropic format) via /v1/chat/completions (OpenAI endpoint).
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", "Bearer sk-drgtw-dualkey001")
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"model":"claude-3-5-sonnet","messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    // Must be OpenAI-style error from the /v1/chat/completions handler.
    assert!(body.get("error").is_some(), "body was: {body}");
    assert!(body["error"].get("message").is_some(), "body was: {body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("claude-3-5-sonnet"),
        "message should mention model, got: {msg}"
    );
    assert!(
        msg.contains("/v1/messages"),
        "message should suggest correct endpoint, got: {msg}"
    );
    // Must NOT be Anthropic shape (no top-level "type":"error").
    assert_ne!(
        body["type"], "error",
        "must use OpenAI shape, body was: {body}"
    );
}

// ---------------------------------------------------------------------------
// 7. 401 on /v1/messages has Anthropic-style error body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_messages_missing_key_returns_401_anthropic_body() {
    let mock_server = MockServer::start().await;
    let app = test_router(anthropic_config(&mock_server.uri()));

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        // Intentionally no auth header.
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"model":"claude-3-5-sonnet","messages":[],"max_tokens":1}"#,
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    // Anthropic shape.
    assert_eq!(body["type"], "error", "body was: {body}");
    assert_eq!(
        body["error"]["type"], "authentication_error",
        "body was: {body}"
    );
}

#[tokio::test]
async fn test_messages_unknown_key_returns_401_anthropic_body() {
    let mock_server = MockServer::start().await;
    let app = test_router(anthropic_config(&mock_server.uri()));

    let req = messages_request_x_api_key(
        "sk-drgtw-doesnotexist",
        r#"{"model":"claude-3-5-sonnet","messages":[],"max_tokens":1}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["type"], "error", "body was: {body}");
    assert_eq!(
        body["error"]["type"], "authentication_error",
        "body was: {body}"
    );
}

// ---------------------------------------------------------------------------
// 8. anthropic-version passthrough: client-supplied value forwarded as-is
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_messages_anthropic_version_passthrough() {
    let mock_server = MockServer::start().await;

    // Mock expects the client-supplied anthropic-version, not the default.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "upstream-anthropic-key"))
        .and(header("anthropic-version", "2024-01-01"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"ok","type":"message"}"#)
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let app = test_router(anthropic_config(&mock_server.uri()));

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", "sk-drgtw-anthropickey001")
        .header("Content-Type", "application/json")
        .header("anthropic-version", "2024-01-01")
        .body(Body::from(
            r#"{"model":"claude-3-5-sonnet","messages":[],"max_tokens":1}"#,
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 9. Rate-limit: gateway 429 + Anthropic error body + retry-after header
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_messages_rate_limit_exhausted_returns_429_anthropic_body() {
    let mock_server = MockServer::start().await;

    // Config: rate_limit = 1 req per 60 s on the virtual key.
    let config = Arc::new(Config {
        server: default_server_config(),
        connections: vec![Connection {
            name: "mock-anthropic-rl".into(),
            base_url: mock_server.uri(),
            api_key: "upstream-rl-key".into(),
            format: ApiFormat::Anthropic,
            models: vec!["claude-3-haiku".into()],
            model_costs: Default::default(),
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-ratelimitkey".into(),
            connections: vec!["mock-anthropic-rl".into()],
            models: None,
            rate_limit: Some(drgtw_config::RateLimit {
                requests: 1,
                per_seconds: 60,
            }),
            budget: None,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
    });

    // First request — should succeed (mock provides a response).
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"ok","type":"message"}"#)
                .insert_header("content-type", "application/json"),
        )
        .mount(&mock_server)
        .await;

    let app = test_router(config);
    let body_str = r#"{"model":"claude-3-haiku","messages":[],"max_tokens":1}"#;

    // First request.
    let req1 = messages_request_x_api_key("sk-drgtw-ratelimitkey", body_str);
    let resp1 = app.clone().oneshot(req1).await.unwrap();

    // Second request — should be rate-limited once RateLimiter is implemented.
    let req2 = messages_request_x_api_key("sk-drgtw-ratelimitkey", body_str);
    let resp2 = app.oneshot(req2).await.unwrap();

    assert_eq!(resp1.status(), StatusCode::OK);
    assert_eq!(resp2.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry = resp2
        .headers()
        .get("retry-after")
        .expect("retry-after header");
    assert!(retry.to_str().unwrap().parse::<u64>().is_ok());
    let rl_limit = resp2
        .headers()
        .get("x-ratelimit-limit")
        .expect("x-ratelimit-limit");
    assert_eq!(rl_limit.to_str().unwrap(), "1");
    let rl_rem = resp2
        .headers()
        .get("x-ratelimit-remaining")
        .expect("x-ratelimit-remaining");
    assert_eq!(rl_rem.to_str().unwrap(), "0");
    let body: Value = serde_json::from_slice(&collect_body(resp2).await).unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "rate_limit_error");
}

// ---------------------------------------------------------------------------
// 10. Upstream URL join: proxy must call /v1/messages on the upstream
// ---------------------------------------------------------------------------

/// Verifies the URL join: base_url = mock_server.uri() (no /v1),
/// upstream receives request at /v1/messages.
#[tokio::test]
async fn test_messages_upstream_url_join() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"url-ok","type":"message"}"#)
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    // base_url = mock server root (no /v1) — proxy must append /v1/messages.
    let app = test_router(anthropic_config(&mock_server.uri()));
    let req = messages_request_bearer(
        "sk-drgtw-anthropickey001",
        r#"{"model":"claude-3-5-sonnet","messages":[],"max_tokens":1}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 11. Body size limit (WP 6.3) — Anthropic endpoint
// ---------------------------------------------------------------------------

/// Oversized body → 413 with Anthropic-format error body.
#[tokio::test]
async fn test_body_too_large_anthropic_returns_413() {
    let mock_server = MockServer::start().await;
    // max_body_bytes = 32; this body is well over that.
    let big_body = "x".repeat(1024);

    let app = test_router(tiny_max_body_anthropic_config(&mock_server.uri()));

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", "sk-drgtw-bodylimit-ant")
        .header("Content-Type", "application/json")
        .body(Body::from(big_body))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    // Anthropic error shape: {"type":"error","error":{"type":"invalid_request_error",...}}
    assert_eq!(body["type"], "error", "body: {body}");
    assert_eq!(
        body["error"]["type"], "invalid_request_error",
        "body: {body}"
    );
}
