//! Integration tests for drgtw-proxy.
//!
//! These tests construct the full axum router with a real ProxyState.
//! The upstream is mocked via wiremock. The tests exercise the full
//! request/response path via tower::ServiceExt::oneshot.
//!
//! NOTE: Tests that call through to KeyStore::authenticate / ResolvedKey
//! methods will panic with `todo!("WP 1.1")` until the drgtw-keys parallel
//! agent lands. They compile fully today and will pass once that crate is
//! implemented. No tests are marked #[ignore].

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

/// Build a config with a tiny max_body_bytes (32 bytes) for body-size tests.
fn tiny_max_body_config(mock_base_url: &str) -> Arc<Config> {
    Arc::new(Config {
        server: ServerConfig {
            bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
            max_body_bytes: 32,
        },
        connections: vec![Connection {
            name: "mock-openai".into(),
            base_url: format!("{}/v1", mock_base_url),
            api_key: "upstream-secret-key".into(),
            format: ApiFormat::OpenAi,
            models: vec!["gpt-4o".into()],
            model_costs: Default::default(),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-bodylimit".into(),
            connections: vec!["mock-openai".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
    })
}

/// Build a Config with two virtual keys:
///   - sk-drgtw-testkey001: allowed gpt-4o, gpt-4o-mini
///   - sk-drgtw-testkey002: allowed gpt-4o only
fn test_config(mock_base_url: &str) -> Arc<Config> {
    Arc::new(Config {
        server: default_server_config(),
        connections: vec![Connection {
            name: "mock-openai".into(),
            base_url: format!("{}/v1", mock_base_url),
            api_key: "upstream-secret-key".into(),
            format: ApiFormat::OpenAi,
            models: vec!["gpt-4o".into(), "gpt-4o-mini".into()],
            model_costs: Default::default(),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![
            VirtualKey {
                key: "sk-drgtw-testkey001".into(),
                connections: vec!["mock-openai".into()],
                models: Some(vec!["gpt-4o".into(), "gpt-4o-mini".into()]),
                rate_limit: None,
                budget: None,
            },
            VirtualKey {
                key: "sk-drgtw-testkey002".into(),
                connections: vec!["mock-openai".into()],
                models: Some(vec!["gpt-4o".into()]),
                rate_limit: None,
                budget: None,
            },
        ],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
    })
}

/// Build the router for testing.
fn test_router(config: Arc<Config>) -> axum::Router {
    let state = Arc::new(
        ProxyState::new(config, std::path::Path::new(".")).expect("ProxyState::new failed in test"),
    );
    router(state)
}

/// POST /v1/chat/completions with Authorization header.
fn chat_request(virtual_key: &str, body: impl Into<String>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
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
// 1. Non-streaming round-trip
// ---------------------------------------------------------------------------

/// Mock asserts: receives Authorization: Bearer <upstream-key>; proxy must NOT
/// forward the virtual key.
#[tokio::test]
async fn test_non_stream_round_trip() {
    let mock_server = MockServer::start().await;

    let upstream_body = serde_json::json!({
        "id": "chatcmpl-abc123",
        "object": "chat.completion",
        "choices": [{"message": {"role": "assistant", "content": "Hello!"}}],
    });

    // Only matches if upstream-secret-key is sent — virtual key would miss this.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer upstream-secret-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&upstream_body)
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let app = test_router(test_config(&mock_server.uri()));
    let req = chat_request(
        "sk-drgtw-testkey001",
        r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = collect_body(resp).await;
    let got: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(got, upstream_body);

    mock_server.verify().await;
}

/// Virtual key must not leak to upstream. A mock that matches on the virtual
/// key is registered with expect(0) — any hit causes verify() to fail.
#[tokio::test]
async fn test_virtual_key_not_leaked_to_upstream() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer upstream-secret-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"ok"}"#)
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    // If the virtual key leaked, this mock would catch it → expect(0) fails.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-drgtw-testkey001"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mock_server)
        .await;

    let app = test_router(test_config(&mock_server.uri()));
    let req = chat_request("sk-drgtw-testkey001", r#"{"model":"gpt-4o","messages":[]}"#);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 2. Auth errors → 401
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_missing_key_returns_401() {
    let mock_server = MockServer::start().await;
    let app = test_router(test_config(&mock_server.uri()));

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        // Intentionally no Authorization header.
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"model":"gpt-4o","messages":[]}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["error"]["code"], "invalid_api_key");
}

#[tokio::test]
async fn test_unknown_key_returns_401() {
    let mock_server = MockServer::start().await;
    let app = test_router(test_config(&mock_server.uri()));

    let req = chat_request(
        "sk-drgtw-doesnotexist",
        r#"{"model":"gpt-4o","messages":[]}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["error"]["code"], "invalid_api_key");
}

// ---------------------------------------------------------------------------
// 3. Routing errors → 404 / 403
// ---------------------------------------------------------------------------

/// A model unknown to any connection (no connection serves it) → 404.
/// Uses a key with no model allowlist so we bypass the allowlist check and
/// reach the UnknownModel branch.
#[tokio::test]
async fn test_unknown_model_returns_404() {
    let mock_server = MockServer::start().await;

    // Build a config with a key that has NO model allowlist so the allowlist
    // check is skipped and we reach the UnknownModel (404) branch.
    let config = Arc::new(Config {
        server: default_server_config(),
        connections: vec![Connection {
            name: "mock-openai".into(),
            base_url: format!("{}/v1", mock_server.uri()),
            api_key: "upstream-secret-key".into(),
            format: ApiFormat::OpenAi,
            models: vec!["gpt-4o".into()],
            model_costs: Default::default(),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-noallowlist404".into(),
            connections: vec!["mock-openai".into()],
            models: None, // no allowlist — all models of the connection are allowed
            rate_limit: None,
            budget: None,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
    });
    let app = test_router(config);

    // Request a model that does not exist on any connection.
    let req = chat_request(
        "sk-drgtw-noallowlist404",
        r#"{"model":"gpt-999-nonexistent","messages":[]}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["error"]["code"], "model_not_found");
}

/// testkey002 is allowed only gpt-4o; requesting gpt-4o-mini must → 403.
#[tokio::test]
async fn test_model_not_in_allowlist_returns_403() {
    let mock_server = MockServer::start().await;
    let app = test_router(test_config(&mock_server.uri()));

    let req = chat_request(
        "sk-drgtw-testkey002",
        r#"{"model":"gpt-4o-mini","messages":[]}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["error"]["code"], "model_not_allowed");
}

// ---------------------------------------------------------------------------
// 4. Missing model field → 400
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_missing_model_field_returns_400() {
    let mock_server = MockServer::start().await;
    let app = test_router(test_config(&mock_server.uri()));

    let req = chat_request(
        "sk-drgtw-testkey001",
        r#"{"messages":[{"role":"user","content":"hello"}]}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["error"]["code"], "missing_model");
}

// ---------------------------------------------------------------------------
// 5. Streaming: byte-identical SSE passthrough
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_streaming_round_trip() {
    let mock_server = MockServer::start().await;

    let sse_body = concat!(
        "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
        "data: [DONE]\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_body.as_bytes().to_vec(), "text/event-stream")
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let app = test_router(test_config(&mock_server.uri()));
    let req = chat_request(
        "sk-drgtw-testkey001",
        r#"{"model":"gpt-4o","messages":[],"stream":true}"#,
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
// 6. GET /v1/models — exact model list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_models() {
    let mock_server = MockServer::start().await;
    let app = test_router(test_config(&mock_server.uri()));

    let req = Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header("Authorization", "Bearer sk-drgtw-testkey001")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["object"], "list");

    let data = body["data"].as_array().unwrap();
    let mut ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["gpt-4o", "gpt-4o-mini"], "got: {ids:?}");
}

#[tokio::test]
async fn test_list_models_respects_allowlist() {
    let mock_server = MockServer::start().await;
    let app = test_router(test_config(&mock_server.uri()));

    // testkey002 is restricted to gpt-4o only.
    let req = Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header("Authorization", "Bearer sk-drgtw-testkey002")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    let data = body["data"].as_array().unwrap();
    let ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["gpt-4o"], "got: {ids:?}");
}

// ---------------------------------------------------------------------------
// 7. Upstream 429 relayed verbatim with body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_upstream_429_relayed() {
    let mock_server = MockServer::start().await;

    let upstream_error = serde_json::json!({
        "error": {
            "message": "Rate limit exceeded",
            "type": "requests",
            "code": "rate_limit_exceeded",
        }
    });

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_json(&upstream_error)
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let app = test_router(test_config(&mock_server.uri()));
    let req = chat_request("sk-drgtw-testkey001", r#"{"model":"gpt-4o","messages":[]}"#);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["error"]["code"], "rate_limit_exceeded");

    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 8. Upstream network error → 502
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_upstream_connect_error_returns_502() {
    // Point at a port that no process is listening on.
    let dead_base = "http://127.0.0.1:19999";

    let config = Arc::new(Config {
        server: default_server_config(),
        connections: vec![Connection {
            name: "dead".into(),
            base_url: format!("{dead_base}/v1"),
            api_key: "some-key".into(),
            format: ApiFormat::OpenAi,
            models: vec!["gpt-4o".into()],
            model_costs: Default::default(),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-connecterr".into(),
            connections: vec!["dead".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
    });

    let app = test_router(config);
    let req = chat_request("sk-drgtw-connecterr", r#"{"model":"gpt-4o","messages":[]}"#);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["error"]["type"], "upstream_error");
}

// ---------------------------------------------------------------------------
// 9. Body forwarded verbatim (opaque passthrough — unknown fields preserved)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_body_forwarded_verbatim() {
    let mock_server = MockServer::start().await;

    // Include a custom field that a typed deserialiser would strip.
    let original_body =
        r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"custom_field":true}"#;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(wiremock::matchers::body_string(original_body))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"ok"}"#)
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    // Byte-identity requires PII off: default is ON since Phase 9, so opt out
    // explicitly for this passthrough test.
    let mut config = (*test_config(&mock_server.uri())).clone();
    config.pii.enabled_by_default = false;
    let app = test_router(Arc::new(config));
    let req = chat_request("sk-drgtw-testkey001", original_body);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 10. Body size limit (WP 6.3)
// ---------------------------------------------------------------------------

/// Oversized body → 413 with OpenAI-format error (type=invalid_request_error,
/// code=request_too_large).
#[tokio::test]
async fn test_body_too_large_openai_returns_413() {
    let mock_server = MockServer::start().await;
    // max_body_bytes = 32; this body is well over that.
    let big_body = "x".repeat(1024);

    let app = test_router(tiny_max_body_config(&mock_server.uri()));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", "Bearer sk-drgtw-bodylimit")
        .header("Content-Type", "application/json")
        .body(Body::from(big_body))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(
        body["error"]["type"], "invalid_request_error",
        "body: {body}"
    );
    assert_eq!(body["error"]["code"], "request_too_large", "body: {body}");
}

/// Normal-size body with tiny limit still accepted.
#[tokio::test]
async fn test_body_within_limit_openai_proceeds() {
    let mock_server = MockServer::start().await;

    // A body small enough to fit in 32 bytes — just needs to clear the limit.
    // We let the mock return 400 (bad JSON) — we only care it reaches upstream.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"ok"}"#)
                .insert_header("content-type", "application/json"),
        )
        .mount(&mock_server)
        .await;

    let big_limit_config = Arc::new(Config {
        server: ServerConfig {
            bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
            max_body_bytes: 10_485_760, // 10 MiB — default
        },
        connections: vec![Connection {
            name: "mock-openai".into(),
            base_url: format!("{}/v1", mock_server.uri()),
            api_key: "upstream-secret-key".into(),
            format: ApiFormat::OpenAi,
            models: vec!["gpt-4o".into()],
            model_costs: Default::default(),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-biglimit".into(),
            connections: vec!["mock-openai".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
    });

    let app = test_router(big_limit_config);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", "Bearer sk-drgtw-biglimit")
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    // 200 means it made it past the body-size check and upstream responded.
    assert_eq!(resp.status(), StatusCode::OK);
}
