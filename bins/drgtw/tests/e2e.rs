//! End-to-end integration tests for the drgtw binary router.
//!
//! These tests build the full router (health + proxy routes + request-ID
//! middleware) via `drgtw::server::router` using `tower::ServiceExt::oneshot`.
//! A wiremock server stands in for the upstream provider.
//!
//! Config is built by writing a temporary TOML file and loading it with
//! `drgtw_config::load`, which is the only public constructor.

use std::io::Write as _;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::load;
use serde_json::Value;
use tower::ServiceExt; // for `.oneshot()`
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Helper: load a config with PII on, including a custom recognizer.
fn load_pii_custom_config(mock_base_url: &str, custom_name: &str, custom_pattern: &str) -> Arc<drgtw_config::Config> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ2: AtomicU64 = AtomicU64::new(1000);
    let n = SEQ2.fetch_add(1, Ordering::Relaxed);

    // Use TOML literal strings (single-quoted) for the pattern so backslashes
    // are not interpreted as TOML escape sequences.
    let toml_content = format!(
        "
[[connections]]
name = \"mock-upstream\"
base_url = \"{mock_base_url}/v1\"
api_key = \"upstream-secret\"
format = \"open_ai\"
models = [\"gpt-4o\"]

[[virtual_keys]]
key = \"sk-drgtw-pii-e2e01\"
connections = [\"mock-upstream\"]
models = [\"gpt-4o\"]

[pii]
enabled_by_default = true

[[pii.custom_recognizers]]
name = \"{custom_name}\"
pattern = '{custom_pattern}'
"
    );

    let path = std::env::temp_dir().join(format!("drgtw-e2e-pii-{n}.toml"));
    let mut f = std::fs::File::create(&path).expect("create temp config");
    f.write_all(toml_content.as_bytes()).expect("write temp config");
    let cfg = load(&path).expect("load temp config");
    Arc::new(cfg)
}

/// Load a config TOML with an invalid custom recognizer regex; returns the
/// config (load succeeds — regex validation happens at PiiEngine boot time).
fn load_invalid_regex_config() -> Arc<drgtw_config::Config> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ3: AtomicU64 = AtomicU64::new(2000);
    let n = SEQ3.fetch_add(1, Ordering::Relaxed);

    let toml_content = r#"
[[connections]]
name = "dummy"
base_url = "http://127.0.0.1:1/v1"
api_key = "key"
format = "open_ai"
models = ["gpt-4o"]

[[virtual_keys]]
key = "sk-drgtw-invalid-re01"
connections = ["dummy"]
models = ["gpt-4o"]

[pii]
enabled_by_default = true

[[pii.custom_recognizers]]
name = "broken"
pattern = "[unclosed"
"#.to_owned();

    let path = std::env::temp_dir().join(format!("drgtw-e2e-invalid-{n}.toml"));
    let mut f = std::fs::File::create(&path).expect("create temp config");
    f.write_all(toml_content.as_bytes()).expect("write temp config");
    let cfg = load(&path).expect("load accepts the config — regex error at engine build");
    Arc::new(cfg)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write a temp TOML config file and load it.  Returns `Arc<Config>`.
///
/// The file is written to `std::env::temp_dir()` with a unique name derived
/// from the test thread id + a monotonic counter so parallel tests don't
/// collide.
fn load_test_config(mock_base_url: &str) -> Arc<drgtw_config::Config> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);

    let toml_content = format!(
        r#"
[[connections]]
name = "mock-upstream"
base_url = "{mock_base_url}/v1"
api_key = "upstream-secret"
format = "open_ai"
models = ["gpt-4o", "gpt-4o-mini"]

[[virtual_keys]]
key = "sk-drgtw-e2etest01"
connections = ["mock-upstream"]
models = ["gpt-4o", "gpt-4o-mini"]
"#
    );

    let path = std::env::temp_dir().join(format!("drgtw-e2e-{n}.toml"));
    let mut f = std::fs::File::create(&path).expect("create temp config");
    f.write_all(toml_content.as_bytes()).expect("write temp config");

    let cfg = load(&path).expect("load temp config");
    Arc::new(cfg)
}

/// Drain an axum response body to bytes.
async fn collect_body(resp: axum::response::Response) -> bytes::Bytes {
    use http_body_util::BodyExt;
    resp.into_body().collect().await.unwrap().to_bytes()
}

/// Convenience: build a POST /v1/chat/completions request.
fn chat_request(virtual_key: &str, json_body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {virtual_key}"))
        .header("Content-Type", "application/json")
        .body(Body::from(json_body.to_owned()))
        .unwrap()
}

// ---------------------------------------------------------------------------
// 1. /health returns 200 + {"status":"ok"}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_health_route() {
    // health doesn't need an upstream — pass a placeholder URL
    let cfg = load_test_config("http://127.0.0.1:1");
    let app = drgtw::server::router(cfg, std::path::Path::new("."), std::path::PathBuf::new()).expect("router build failed");

    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body_bytes = collect_body(resp).await;
    let body: Value =
        serde_json::from_slice(&body_bytes).expect("health body is JSON");
    assert_eq!(body["status"], "ok");
}

// ---------------------------------------------------------------------------
// 2. x-drgtw-request-id present on /health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_request_id_on_health() {
    let cfg = load_test_config("http://127.0.0.1:1");
    let app = drgtw::server::router(cfg, std::path::Path::new("."), std::path::PathBuf::new()).expect("router build failed");

    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let id = resp
        .headers()
        .get("x-drgtw-request-id")
        .expect("x-drgtw-request-id header must be present")
        .to_str()
        .expect("header is valid UTF-8");

    // Format: <16-hex>-<8-hex>
    assert!(!id.is_empty(), "request-id must not be empty");
    let parts: Vec<&str> = id.splitn(2, '-').collect();
    assert_eq!(parts.len(), 2, "request-id format: {id}");
    assert_eq!(parts[0].len(), 16, "nanos part: {id}");
    assert_eq!(parts[1].len(), 8, "counter part: {id}");
}

// ---------------------------------------------------------------------------
// 3. Two consecutive requests get distinct request IDs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_request_ids_are_distinct() {
    let cfg = load_test_config("http://127.0.0.1:1");

    let req_a = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let req_b = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    // Clone the app for two separate oneshot calls.
    let id_a = drgtw::server::router(Arc::clone(&cfg), std::path::Path::new("."), std::path::PathBuf::new())
        .expect("router build failed")
        .oneshot(req_a)
        .await
        .unwrap()
        .headers()
        .get("x-drgtw-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    let id_b = drgtw::server::router(Arc::clone(&cfg), std::path::Path::new("."), std::path::PathBuf::new())
        .expect("router build failed")
        .oneshot(req_b)
        .await
        .unwrap()
        .headers()
        .get("x-drgtw-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    assert_ne!(id_a, id_b, "consecutive requests must get different IDs");
}

// ---------------------------------------------------------------------------
// 4. POST /v1/chat/completions — full round-trip with virtual key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_chat_completions_round_trip() {
    let mock_server = MockServer::start().await;

    let upstream_body = serde_json::json!({
        "id": "chatcmpl-e2e",
        "object": "chat.completion",
        "choices": [{"message": {"role": "assistant", "content": "pong"}}],
    });

    // Mock expects the upstream key, NOT the virtual key.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer upstream-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&upstream_body)
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let cfg = load_test_config(&mock_server.uri());
    let app = drgtw::server::router(cfg, std::path::Path::new("."), std::path::PathBuf::new()).expect("router build failed");

    let req = chat_request(
        "sk-drgtw-e2etest01",
        r#"{"model":"gpt-4o","messages":[{"role":"user","content":"ping"}]}"#,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Request-ID must also be present on proxy responses.
    assert!(
        resp.headers().contains_key("x-drgtw-request-id"),
        "x-drgtw-request-id missing on proxy response"
    );

    let body_bytes = collect_body(resp).await;
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body, upstream_body);

    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 5. Invalid virtual key → 401
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_invalid_virtual_key_returns_401() {
    let mock_server = MockServer::start().await;
    let cfg = load_test_config(&mock_server.uri());
    let app = drgtw::server::router(cfg, std::path::Path::new("."), std::path::PathBuf::new()).expect("router build failed");

    let req = chat_request(
        "sk-drgtw-doesnotexist",
        r#"{"model":"gpt-4o","messages":[]}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let body_bytes = collect_body(resp).await;
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["error"]["code"], "invalid_api_key");
}

// ---------------------------------------------------------------------------
// 6. x-drgtw-request-id present even on error responses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_request_id_present_on_error_response() {
    let mock_server = MockServer::start().await;
    let cfg = load_test_config(&mock_server.uri());
    let app = drgtw::server::router(cfg, std::path::Path::new("."), std::path::PathBuf::new()).expect("router build failed");

    // No auth header → 401
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"model":"gpt-4o","messages":[]}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        resp.headers().contains_key("x-drgtw-request-id"),
        "x-drgtw-request-id must be present even on error responses"
    );
}

// ---------------------------------------------------------------------------
// 7. PII + custom recognizer e2e: WP 3.4 bin-level test
//    Custom recognizer `ticket` → pattern `TKT-\d+`; body contains TKT-9999;
//    upstream must receive TICKET_1, not TKT-9999.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_e2e_pii_custom_recognizer_full_flow() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "id": "chatcmpl-custom",
                    "choices": [{"message": {"role": "assistant", "content": "ticket TICKET_1 received"}}]
                }))
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let cfg = load_pii_custom_config(&mock_server.uri(), "ticket", r"TKT-\d+");
    let app = drgtw::server::router(cfg, std::path::Path::new("."), std::path::PathBuf::new()).expect("router build failed");

    let req_body =
        r#"{"model":"gpt-4o","messages":[{"role":"user","content":"handle TKT-9999 now"}]}"#;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", "Bearer sk-drgtw-pii-e2e01")
        .header("Content-Type", "application/json")
        .body(Body::from(req_body))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Client should have TKT-9999 restored in the response.
    let body_bytes = collect_body(resp).await;
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    let content = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        content.contains("TKT-9999"),
        "client must see restored ticket id, got: {content}"
    );
    assert!(
        !content.contains("TICKET_1"),
        "TICKET_1 placeholder must not leak, got: {content}"
    );

    // Upstream must have received the placeholder.
    let received = mock_server.received_requests().await.unwrap();
    let upstream: Value = serde_json::from_slice(&received[0].body).unwrap();
    let up_content = upstream["messages"][0]["content"].as_str().unwrap();
    assert!(
        up_content.contains("TICKET_1"),
        "upstream must receive TICKET_1, got: {up_content}"
    );
    assert!(
        !up_content.contains("TKT-9999"),
        "upstream must not receive raw ticket id, got: {up_content}"
    );

    mock_server.verify().await;
}

// ---------------------------------------------------------------------------
// 8. Invalid custom recognizer regex → server::router returns Err (boot fails)
// ---------------------------------------------------------------------------

#[test]
fn test_invalid_custom_regex_fails_boot() {
    let cfg = load_invalid_regex_config();
    // ProxyState::new (via server::router) must fail with a readable error.
    let result = drgtw::server::router(cfg, std::path::Path::new("."), std::path::PathBuf::new());
    assert!(
        result.is_err(),
        "router must fail to build with invalid custom regex"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("broken") || err_msg.contains("invalid regex") || err_msg.contains("regex"),
        "error message should mention the recognizer or regex, got: {err_msg}"
    );
}
