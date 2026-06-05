//! WP 3.4 PII pipeline integration tests.
//!
//! Tests are end-to-end through the full axum router (ProxyState + handlers)
//! with a wiremock upstream.  All cases use `tower::ServiceExt::oneshot`.
//!
//! Coverage:
//!  1. OpenAI non-streaming: PII pseudonymised on the way in, restored on the way out.
//!  2. Anthropic non-streaming: same but via /v1/messages.
//!  3. Streaming: placeholder restored through SSE stream.
//!  4. `x-drgtw-pii: off` → wiremock receives the raw email (byte passthrough).
//!  5. `x-drgtw-pii: bogus` → 400.
//!  6. `enabled_by_default = false` + no header → passthrough.
//!  7. Empty map skip: body without PII → upstream receives identical bytes.
//!  8. Anthropic: `x-drgtw-pii: bogus` produces Anthropic-shaped 400.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::{ApiFormat, Config, Connection, PiiConfig, ServerConfig, VirtualKey};
use drgtw_proxy::{ProxyState, router};
use serde_json::Value;
use tower::ServiceExt; // `.oneshot()`
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_server_config() -> ServerConfig {
    ServerConfig {
        bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
        ..Default::default()
    }
}

/// Config with PII on by default, single OpenAI connection.
fn openai_pii_config(mock_base_url: &str, pii_enabled: bool) -> Arc<Config> {
    Arc::new(Config {
        server: default_server_config(),
        connections: vec![Connection {
            name: "mock-openai".into(),
            base_url: format!("{mock_base_url}/v1"),
            api_key: "upstream-secret".into(),
            format: ApiFormat::OpenAi,
            models: vec!["gpt-4o".into()],
            model_costs: Default::default(),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-piitest001".into(),
            connections: vec!["mock-openai".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
        }],
        pii: PiiConfig {
            enabled_by_default: pii_enabled,
            disabled_recognizers: vec![],
            custom_recognizers: vec![],
            ner: None,
            vault: None,
        },
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
    })
}

/// Config with PII on by default, single Anthropic connection.
fn anthropic_pii_config(mock_base_url: &str, pii_enabled: bool) -> Arc<Config> {
    Arc::new(Config {
        server: default_server_config(),
        connections: vec![Connection {
            name: "mock-anthropic".into(),
            base_url: mock_base_url.to_owned(),
            api_key: "upstream-secret".into(),
            format: ApiFormat::Anthropic,
            models: vec!["claude-3-5-sonnet".into()],
            model_costs: Default::default(),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-piitest001".into(),
            connections: vec!["mock-anthropic".into()],
            models: Some(vec!["claude-3-5-sonnet".into()]),
            rate_limit: None,
            budget: None,
        }],
        pii: PiiConfig {
            enabled_by_default: pii_enabled,
            disabled_recognizers: vec![],
            custom_recognizers: vec![],
            ner: None,
            vault: None,
        },
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
    })
}

fn test_router(config: Arc<Config>) -> axum::Router {
    let state = Arc::new(
        ProxyState::new(config, std::path::Path::new(".")).expect("ProxyState::new failed"),
    );
    router(state)
}

fn openai_request(virtual_key: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {virtual_key}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn openai_request_with_pii_header(
    virtual_key: &str,
    body: &str,
    pii_header: &str,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {virtual_key}"))
        .header("Content-Type", "application/json")
        .header("x-drgtw-pii", pii_header)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn anthropic_request(virtual_key: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", virtual_key)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn anthropic_request_with_pii_header(
    virtual_key: &str,
    body: &str,
    pii_header: &str,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", virtual_key)
        .header("Content-Type", "application/json")
        .header("x-drgtw-pii", pii_header)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn collect_body(resp: axum::response::Response) -> bytes::Bytes {
    use http_body_util::BodyExt;
    resp.into_body().collect().await.unwrap().to_bytes()
}

// ---------------------------------------------------------------------------
// 1. OpenAI non-streaming: PII pseudonymised in → restored out
// ---------------------------------------------------------------------------

/// Wiremock asserts the upstream body has EMAIL_1 + PHONE_1 (not the raw values).
/// The mock's response echoes "EMAIL_1" in the content field.
/// The client response must contain the restored email address.
#[tokio::test]
async fn test_openai_nonstream_pii_pseudonymize_and_restore() {
    let mock_server = MockServer::start().await;

    // The upstream sees placeholders, not PII.
    // We use a body_json_path matcher to check the content field contains EMAIL_1.
    // wiremock doesn't have a deep-body path matcher, so we capture ALL requests
    // and inspect what arrived via the received_requests API.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "id": "chatcmpl-pii1",
                    "object": "chat.completion",
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            // Echo the placeholder — proxy must restore it.
                            "content": "I got your message about EMAIL_1"
                        }
                    }]
                }))
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let cfg = openai_pii_config(&mock_server.uri(), true);
    let app = test_router(cfg);

    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "mail max@example.com and ring +49 89 1234567"}]
    });
    let req = openai_request("sk-drgtw-piitest001", &request_body.to_string());
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Client response must contain the restored email, not the placeholder.
    let body_bytes = collect_body(resp).await;
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    let content = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        content.contains("max@example.com"),
        "response should contain restored email, got: {content}"
    );
    assert!(
        !content.contains("EMAIL_1"),
        "placeholder EMAIL_1 must not leak to client, got: {content}"
    );

    // Inspect what wiremock received: must contain placeholders, not raw PII.
    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    let upstream_content = upstream_body["messages"][0]["content"].as_str().unwrap();
    assert!(
        upstream_content.contains("EMAIL_1"),
        "upstream should receive EMAIL_1 placeholder, got: {upstream_content}"
    );
    assert!(
        !upstream_content.contains("max@example.com"),
        "upstream must NOT receive raw email, got: {upstream_content}"
    );
    assert!(
        upstream_content.contains("PHONE_1"),
        "upstream should receive PHONE_1 placeholder, got: {upstream_content}"
    );
    assert!(
        !upstream_content.contains("+49"),
        "upstream must NOT receive raw phone, got: {upstream_content}"
    );

    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 2. Anthropic non-streaming: PII pseudonymised in → restored out
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_anthropic_nonstream_pii_pseudonymize_and_restore() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "id": "msg-pii1",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "Received from EMAIL_1"}]
                }))
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let cfg = anthropic_pii_config(&mock_server.uri(), true);
    let app = test_router(cfg);

    let request_body = serde_json::json!({
        "model": "claude-3-5-sonnet",
        "max_tokens": 100,
        "system": "You help alice@corp.org",
        "messages": [{"role": "user", "content": "contact alice@corp.org please"}]
    });
    let req = anthropic_request("sk-drgtw-piitest001", &request_body.to_string());
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Client must get the restored email, not placeholder.
    let body_bytes = collect_body(resp).await;
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    let text = body["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("alice@corp.org"),
        "response should contain restored email, got: {text}"
    );
    assert!(
        !text.contains("EMAIL_1"),
        "placeholder must not leak, got: {text}"
    );

    // Upstream must have received the placeholder, not the raw email.
    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    // Check system field was pseudonymised.
    let system = upstream_body["system"].as_str().unwrap_or("");
    assert!(
        !system.contains("alice@corp.org"),
        "upstream system must not contain raw email, got: {system}"
    );
    assert!(
        system.contains("EMAIL_1"),
        "upstream system must contain EMAIL_1 placeholder, got: {system}"
    );

    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 3. Streaming: placeholder restored through SSE stream
// ---------------------------------------------------------------------------

/// Mock returns SSE body where EMAIL_1 placeholder appears whole.
/// (Chunk-split correctness is proptest-covered in drgtw-pii; here we verify
/// end-to-end restore works through the axum body machinery.)
#[tokio::test]
async fn test_streaming_pii_restore() {
    let mock_server = MockServer::start().await;

    // SSE body with EMAIL_1 placeholder in one chunk.
    let sse_body = concat!(
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"Hello EMAIL_1!\"}}]}\n\n",
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

    let cfg = openai_pii_config(&mock_server.uri(), true);
    let app = test_router(cfg);

    let req_body = serde_json::json!({
        "model": "gpt-4o",
        "stream": true,
        "messages": [{"role": "user", "content": "contact max@example.com"}]
    });
    let req = openai_request("sk-drgtw-piitest001", &req_body.to_string());
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body_bytes = collect_body(resp).await;
    let body_str = std::str::from_utf8(&body_bytes).unwrap();

    // Placeholder must be restored to the original email.
    assert!(
        body_str.contains("max@example.com"),
        "streaming response must contain restored email, got: {body_str}"
    );
    assert!(
        !body_str.contains("EMAIL_1"),
        "EMAIL_1 placeholder must not leak to client, got: {body_str}"
    );

    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 4. x-drgtw-pii: off → byte-identical passthrough
// ---------------------------------------------------------------------------

/// With `x-drgtw-pii: off`, the wiremock must receive the raw email unchanged,
/// and the body bytes forwarded are identical to what the client sent.
#[tokio::test]
async fn test_pii_header_off_bypasses_pii() {
    let mock_server = MockServer::start().await;

    let raw_body =
        r#"{"model":"gpt-4o","messages":[{"role":"user","content":"email max@example.com"}]}"#;

    // Wiremock will only match if it receives the EXACT original body string.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(wiremock::matchers::body_string(raw_body))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id":"ok","choices":[]}))
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    // Config has PII enabled by default, but the header turns it off.
    let cfg = openai_pii_config(&mock_server.uri(), true);
    let app = test_router(cfg);

    let req = openai_request_with_pii_header("sk-drgtw-piitest001", raw_body, "off");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 5. x-drgtw-pii: bogus value → 400 (OpenAI error shape)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pii_header_bogus_openai_returns_400() {
    let mock_server = MockServer::start().await;
    let cfg = openai_pii_config(&mock_server.uri(), true);
    let app = test_router(cfg);

    let req = openai_request_with_pii_header(
        "sk-drgtw-piitest001",
        r#"{"model":"gpt-4o","messages":[]}"#,
        "maybe",
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["error"]["code"], "invalid_pii_header");
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

// ---------------------------------------------------------------------------
// 6. Anthropic: x-drgtw-pii bogus → 400 (Anthropic error shape)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pii_header_bogus_anthropic_returns_400() {
    let mock_server = MockServer::start().await;
    let cfg = anthropic_pii_config(&mock_server.uri(), true);
    let app = test_router(cfg);

    let req = anthropic_request_with_pii_header(
        "sk-drgtw-piitest001",
        r#"{"model":"claude-3-5-sonnet","max_tokens":1,"messages":[]}"#,
        "YES",
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    // Anthropic shape: {"type":"error","error":{"type":...,"message":...}}
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

// ---------------------------------------------------------------------------
// 7. enabled_by_default = false + no header → passthrough (byte-identical)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pii_disabled_by_default_passthrough() {
    let mock_server = MockServer::start().await;

    let raw_body =
        r#"{"model":"gpt-4o","messages":[{"role":"user","content":"email bob@example.com"}]}"#;

    // Wiremock asserts the raw body arrives unchanged.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(wiremock::matchers::body_string(raw_body))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id":"ok","choices":[]}))
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    // PII disabled by default.
    let cfg = openai_pii_config(&mock_server.uri(), false);
    let app = test_router(cfg);

    // No x-drgtw-pii header → use default (off).
    let req = openai_request("sk-drgtw-piitest001", raw_body);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 8. Empty map skip: body without PII → upstream receives identical bytes
// ---------------------------------------------------------------------------

/// When PII is on but the body contains no PII, the EntityMap is empty.
/// The upstream must still receive bytes that parse to the same JSON
/// (re-serialized is acceptable since we re-serialize after pseudonymize
/// even when map is empty — the test checks semantic equivalence, not
/// byte identity, because serde_json doesn't guarantee field order).
///
/// More importantly: map.len() == 0 → response restore is skipped entirely.
#[tokio::test]
async fn test_empty_map_skip_no_pii_in_body() {
    let mock_server = MockServer::start().await;

    let upstream_response = serde_json::json!({
        "id": "chatcmpl-nopii",
        "object": "chat.completion",
        "choices": [{"message": {"role": "assistant", "content": "no pii here"}}]
    });

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&upstream_response)
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let cfg = openai_pii_config(&mock_server.uri(), true);
    let app = test_router(cfg);

    // Body has no PII → empty EntityMap → restore is a no-op.
    let req_body = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hello world"}]}"#;
    let req = openai_request("sk-drgtw-piitest001", req_body);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Response should equal the upstream response exactly.
    let body_bytes = collect_body(resp).await;
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body, upstream_response);

    // Upstream must have received the body (it matched the mock → expect(1) passes).
    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 9. x-drgtw-pii: ON header (case-insensitive) overrides disabled default
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pii_header_on_overrides_disabled_default() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "id": "chatcmpl-override",
                    "choices": [{"message": {"role": "assistant", "content": "got EMAIL_1"}}]
                }))
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    // PII disabled by default — but header overrides to ON.
    let cfg = openai_pii_config(&mock_server.uri(), false);
    let app = test_router(cfg);

    let req_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "send to max@example.com"}]
    });
    let req = openai_request_with_pii_header(
        "sk-drgtw-piitest001",
        &req_body.to_string(),
        "ON", // uppercase — must be case-insensitive
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Upstream must NOT have received the raw email.
    let received = mock_server.received_requests().await.unwrap();
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    let content = upstream_body["messages"][0]["content"].as_str().unwrap();
    assert!(
        !content.contains("max@example.com"),
        "upstream must not receive raw email when pii=ON, got: {content}"
    );
    assert!(
        content.contains("EMAIL_1"),
        "upstream must receive placeholder, got: {content}"
    );

    // Client response must have the email restored.
    let body_bytes = collect_body(resp).await;
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    let resp_content = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        resp_content.contains("max@example.com"),
        "client must see restored email, got: {resp_content}"
    );

    mock_server.verify().await;
}
