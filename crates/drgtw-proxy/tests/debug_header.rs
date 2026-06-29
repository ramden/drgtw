//! Debug-header feature tests (demo support).
//!
//! `x-drgtw-debug: on` on a NON-streaming chat/messages request with PII mode
//! ON makes the response JSON gain a top-level `"drgtw_debug"` object:
//!
//! ```json
//! "drgtw_debug": {
//!   "pseudonymized_request": <rewritten request body sent upstream>,
//!   "raw_response_text": ["<assistant text BEFORE restore, per choice/block>"],
//!   "entities": <number of entities in the request map>
//! }
//! ```
//!
//! The block is a no-op (and absent) when streaming, when PII is off, on
//! embeddings, on errors, or when the header is absent.
//!
//! Coverage:
//!  1. OpenAI non-streaming + debug on → drgtw_debug present, pseudonymized
//!     request carries EMAIL_1, raw_response_text carries EMAIL_1, main content
//!     restored, entities >= 1.
//!  2. Header absent → response byte-identical to the no-debug baseline (no
//!     drgtw_debug key).
//!  3. Streaming + debug on → no drgtw_debug, stream restored as usual.
//!  4. PII off + debug on → no drgtw_debug.

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
            key: "sk-drgtw-dbgtest001".into(),
            connections: vec!["mock-openai".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: PiiConfig {
            enabled_by_default: pii_enabled,
            disabled_recognizers: vec![],
            custom_recognizers: vec![],
            entities: None,
            ner: None,
            vault: None,
            embeddings_require_vault: false,
            require_ner: false,
        },
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    })
}

fn test_router(config: Arc<Config>) -> axum::Router {
    let state = Arc::new(
        ProxyState::new(config, std::path::Path::new(".")).expect("ProxyState::new failed"),
    );
    router(state)
}

/// Build an OpenAI chat request, optionally with `x-drgtw-debug` and `stream`.
fn openai_request(virtual_key: &str, body: &str, debug: bool) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {virtual_key}"))
        .header("Content-Type", "application/json");
    if debug {
        b = b.header("x-drgtw-debug", "on");
    }
    b.body(Body::from(body.to_owned())).unwrap()
}

async fn collect_body(resp: axum::response::Response) -> bytes::Bytes {
    use http_body_util::BodyExt;
    resp.into_body().collect().await.unwrap().to_bytes()
}

/// Standard non-streaming OpenAI mock that echoes EMAIL_1 in the content.
async fn mount_openai_echo(mock_server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "id": "chatcmpl-dbg",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            // Echo the placeholder — proxy restores it in the
                            // main body but raw_response_text keeps it.
                            "content": "I got your message about EMAIL_1"
                        }
                    }]
                }))
                .insert_header("content-type", "application/json"),
        )
        .mount(mock_server)
        .await;
}

// ---------------------------------------------------------------------------
// 1. OpenAI non-streaming + debug on → drgtw_debug present and well-formed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_debug_on_adds_drgtw_debug_block() {
    let mock_server = MockServer::start().await;
    mount_openai_echo(&mock_server).await;

    let cfg = openai_pii_config(&mock_server.uri(), true);
    let app = test_router(cfg);

    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "mail max@example.com"}]
    })
    .to_string();
    let req = openai_request("sk-drgtw-dbgtest001", &body, true);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = collect_body(resp).await;
    let v: Value = serde_json::from_slice(&bytes).unwrap();

    // Main content restored to the original email (placeholder gone).
    let content = v["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        content.contains("max@example.com"),
        "main content must be restored, got: {content}"
    );
    assert!(
        !content.contains("EMAIL_1"),
        "placeholder must not leak in main content"
    );

    // drgtw_debug block present.
    let dbg = &v["drgtw_debug"];
    assert!(dbg.is_object(), "drgtw_debug must be present, got: {v}");

    // entities count >= 1 (at least the email).
    assert!(
        dbg["entities"].as_u64().unwrap() >= 1,
        "entities must be >= 1, got: {}",
        dbg["entities"]
    );

    // pseudonymized_request carries the placeholder, NOT the raw email.
    let pseudo = &dbg["pseudonymized_request"];
    let pseudo_content = pseudo["messages"][0]["content"].as_str().unwrap();
    assert!(
        pseudo_content.contains("EMAIL_1"),
        "pseudonymized_request must contain EMAIL_1, got: {pseudo_content}"
    );
    assert!(
        !pseudo_content.contains("max@example.com"),
        "pseudonymized_request must NOT contain raw email, got: {pseudo_content}"
    );

    // raw_response_text carries the pre-restore placeholder text.
    let raw = dbg["raw_response_text"].as_array().unwrap();
    assert!(!raw.is_empty(), "raw_response_text must be non-empty");
    assert!(
        raw.iter()
            .any(|t| t.as_str().unwrap_or("").contains("EMAIL_1")),
        "raw_response_text must contain EMAIL_1, got: {raw:?}"
    );

    // The mapping itself is NEVER emitted.
    assert!(dbg.get("map").is_none(), "entity map must not be emitted");
    assert!(
        dbg.get("entities_map").is_none(),
        "entity map must not be emitted"
    );
}

// ---------------------------------------------------------------------------
// 2. Header absent → byte-identical to baseline, no drgtw_debug key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_debug_absent_is_byte_identical() {
    let mock_server = MockServer::start().await;
    mount_openai_echo(&mock_server).await;

    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "mail max@example.com"}]
    })
    .to_string();

    // Baseline: no debug header.
    let app1 = test_router(openai_pii_config(&mock_server.uri(), true));
    let resp1 = app1
        .oneshot(openai_request("sk-drgtw-dbgtest001", &body, false))
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let bytes1 = collect_body(resp1).await;

    // Same request again, still no debug header.
    let app2 = test_router(openai_pii_config(&mock_server.uri(), true));
    let resp2 = app2
        .oneshot(openai_request("sk-drgtw-dbgtest001", &body, false))
        .await
        .unwrap();
    let bytes2 = collect_body(resp2).await;

    assert_eq!(
        bytes1, bytes2,
        "two no-debug responses must be byte-identical"
    );

    let v: Value = serde_json::from_slice(&bytes1).unwrap();
    assert!(
        v.get("drgtw_debug").is_none(),
        "no drgtw_debug key when header absent, got: {v}"
    );
}

// ---------------------------------------------------------------------------
// 3. Streaming + debug on → no drgtw_debug; stream still restored
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_debug_on_streaming_is_noop() {
    let mock_server = MockServer::start().await;

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
        .mount(&mock_server)
        .await;

    let app = test_router(openai_pii_config(&mock_server.uri(), true));
    let body = serde_json::json!({
        "model": "gpt-4o",
        "stream": true,
        "messages": [{"role": "user", "content": "contact max@example.com"}]
    })
    .to_string();
    let resp = app
        .oneshot(openai_request("sk-drgtw-dbgtest001", &body, true))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = collect_body(resp).await;
    let s = std::str::from_utf8(&bytes).unwrap();

    // Stream restored as usual.
    assert!(
        s.contains("max@example.com"),
        "stream must restore email, got: {s}"
    );
    assert!(
        !s.contains("EMAIL_1"),
        "placeholder must not leak in stream"
    );
    // No debug block injected into the SSE stream.
    assert!(
        !s.contains("drgtw_debug"),
        "streaming must not gain drgtw_debug"
    );
}

// ---------------------------------------------------------------------------
// 4. PII off + debug on → no drgtw_debug
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_debug_on_pii_off_is_noop() {
    let mock_server = MockServer::start().await;
    mount_openai_echo(&mock_server).await;

    // PII disabled by default and no x-drgtw-pii header → PII off.
    let app = test_router(openai_pii_config(&mock_server.uri(), false));
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "mail max@example.com"}]
    })
    .to_string();
    let resp = app
        .oneshot(openai_request("sk-drgtw-dbgtest001", &body, true))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = collect_body(resp).await;
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        v.get("drgtw_debug").is_none(),
        "PII off must not gain drgtw_debug, got: {v}"
    );
}

// ---------------------------------------------------------------------------
// 5. PII on, debug on, but ZERO entities detected → drgtw_debug still present
//    (regression: demo panels must not go blank when nothing was detected).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_debug_on_zero_entities_still_attaches_block() {
    let mock_server = MockServer::start().await;
    // Response with no placeholders — nothing to restore.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "id": "chatcmpl-noent",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "Nice to meet you!"}
                    }]
                }))
                .insert_header("content-type", "application/json"),
        )
        .mount(&mock_server)
        .await;

    let app = test_router(openai_pii_config(&mock_server.uri(), true));
    // No deterministic PII in this text (a bare first name needs NER, which is
    // not configured here) → zero entities.
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hello there"}]
    })
    .to_string();
    let resp = app
        .oneshot(openai_request("sk-drgtw-dbgtest001", &body, true))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let v: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    let dbg = &v["drgtw_debug"];
    assert!(
        dbg.is_object(),
        "drgtw_debug must attach even at 0 entities, got: {v}"
    );
    assert_eq!(dbg["entities"].as_u64().unwrap(), 0, "expected 0 entities");
    // pseudonymized_request equals the (unchanged) original text.
    let pseudo = dbg["pseudonymized_request"]["messages"][0]["content"]
        .as_str()
        .unwrap();
    assert_eq!(pseudo, "hello there");
    // Normal response still delivered.
    assert_eq!(
        v["choices"][0]["message"]["content"].as_str().unwrap(),
        "Nice to meet you!"
    );
}
