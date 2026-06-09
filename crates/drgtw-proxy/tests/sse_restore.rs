//! End-to-end SSE streaming PII-restore regression tests (the Azure bug).
//!
//! The bug: when the upstream LLM tokenises a placeholder across MULTIPLE SSE
//! `data:` events (e.g. `" EMAIL"`, `"_"`, `"1"`), the old raw-byte streaming
//! restorer could never reassemble it because JSON+SSE framing sat between the
//! fragments. These tests drive the full axum router with a wiremock upstream
//! emitting fragmented SSE and assert the client receives fully restored text.

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

fn openai_pii_config(mock_base_url: &str) -> Arc<Config> {
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
            key: "sk-drgtw-ssetest001".into(),
            connections: vec!["mock-openai".into()],
            models: Some(vec!["gpt-4o".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
        }],
        pii: PiiConfig {
            enabled_by_default: true,
            disabled_recognizers: vec![],
            custom_recognizers: vec![],
            ner: None,
            vault: None,
            embeddings_require_vault: false,
        },
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
    })
}

fn anthropic_pii_config(mock_base_url: &str) -> Arc<Config> {
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
            key: "sk-drgtw-ssetest001".into(),
            connections: vec!["mock-anthropic".into()],
            models: Some(vec!["claude-3-5-sonnet".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
        }],
        pii: PiiConfig {
            enabled_by_default: true,
            disabled_recognizers: vec![],
            custom_recognizers: vec![],
            ner: None,
            vault: None,
            embeddings_require_vault: false,
        },
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
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

fn anthropic_request(virtual_key: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", virtual_key)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn collect_body(resp: axum::response::Response) -> bytes::Bytes {
    use http_body_util::BodyExt;
    resp.into_body().collect().await.unwrap().to_bytes()
}

/// Join all `choices[0].delta.content` strings across an emitted OpenAI SSE
/// stream, skipping `[DONE]` and non-content events.
fn join_openai_content(bytes: &[u8]) -> String {
    let text = std::str::from_utf8(bytes).unwrap();
    let mut joined = String::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.strip_prefix(' ').unwrap_or(rest).trim();
        if payload == "[DONE]" || payload.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if let Some(c) = v["choices"][0]["delta"]["content"].as_str() {
            joined.push_str(c);
        }
    }
    joined
}

fn join_anthropic_text(bytes: &[u8]) -> String {
    let text = std::str::from_utf8(bytes).unwrap();
    let mut joined = String::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.strip_prefix(' ').unwrap_or(rest).trim();
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if v["type"] == "content_block_delta"
            && v["delta"]["type"] == "text_delta"
            && let Some(t) = v["delta"]["text"].as_str()
        {
            joined.push_str(t);
        }
    }
    joined
}

// ---------------------------------------------------------------------------
// 1. REGRESSION: placeholder split across six SSE events (the Azure case)
// ---------------------------------------------------------------------------

/// Request contains an email → pseudonymised to EMAIL_1. The upstream streams
/// the reply "write to EMAIL_1 today" as six separate data events, splitting
/// the placeholder into `" EMAIL"`, `"_"`, `"1"`. The client must receive the
/// fully restored email, and the joined content must never contain "EMAIL_1".
#[tokio::test]
async fn regression_openai_placeholder_split_across_events() {
    let mock_server = MockServer::start().await;

    // Six fragments, EMAIL_1 split across three of them.
    let frags = ["write", " to", " EMAIL", "_", "1", " today"];
    let mut sse_body = String::new();
    for frag in frags {
        sse_body.push_str(&format!(
            "data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\n",
            serde_json::to_string(frag).unwrap()
        ));
    }
    sse_body.push_str("data: [DONE]\n\n");

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_body.into_bytes(), "text/event-stream")
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let app = test_router(openai_pii_config(&mock_server.uri()));
    let req_body = serde_json::json!({
        "model": "gpt-4o",
        "stream": true,
        "messages": [{"role": "user", "content": "write to max.mustermann@example.com today"}]
    });
    let resp = app
        .oneshot(openai_request("sk-drgtw-ssetest001", &req_body.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = collect_body(resp).await;
    let joined = join_openai_content(&body);

    assert_eq!(
        joined, "write to max.mustermann@example.com today",
        "split placeholder must be restored; got: {joined}"
    );
    assert!(
        !joined.contains("EMAIL_1"),
        "placeholder must not leak in joined content: {joined}"
    );
    let raw = std::str::from_utf8(&body).unwrap();
    assert!(
        !raw.contains("EMAIL_1"),
        "placeholder must not leak in raw stream: {raw}"
    );
}

// ---------------------------------------------------------------------------
// 2. Placeholder whole in a single event (old behaviour preserved)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openai_placeholder_whole_in_one_event() {
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
        .expect(1)
        .mount(&mock_server)
        .await;

    let app = test_router(openai_pii_config(&mock_server.uri()));
    let req_body = serde_json::json!({
        "model": "gpt-4o",
        "stream": true,
        "messages": [{"role": "user", "content": "contact max.mustermann@example.com"}]
    });
    let resp = app
        .oneshot(openai_request("sk-drgtw-ssetest001", &req_body.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = collect_body(resp).await;
    let joined = join_openai_content(&body);
    assert_eq!(joined, "Hello max.mustermann@example.com!");
    assert!(!joined.contains("EMAIL_1"));
}

// ---------------------------------------------------------------------------
// 3. Non-content chunks (role-only, finish_reason, usage) preserved
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openai_non_content_chunks_pass_through() {
    let mock_server = MockServer::start().await;

    let sse_body = concat!(
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi EMAIL_1\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"total_tokens\":7}}\n\n",
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

    let app = test_router(openai_pii_config(&mock_server.uri()));
    let req_body = serde_json::json!({
        "model": "gpt-4o",
        "stream": true,
        "messages": [{"role": "user", "content": "contact max.mustermann@example.com"}]
    });
    let resp = app
        .oneshot(openai_request("sk-drgtw-ssetest001", &req_body.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = collect_body(resp).await;
    let s = std::str::from_utf8(&body).unwrap();

    // Content restored.
    assert_eq!(join_openai_content(&body), "hi max.mustermann@example.com");

    // Non-content fields survive.
    let mut saw_role = false;
    let mut saw_finish = false;
    let mut saw_usage = false;
    for line in s.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.strip_prefix(' ').unwrap_or(rest).trim();
        if payload == "[DONE]" || payload.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(payload).unwrap();
        if v["choices"][0]["delta"]["role"] == "assistant" {
            saw_role = true;
        }
        if v["choices"][0]["finish_reason"] == "stop" {
            saw_finish = true;
        }
        if v["usage"]["total_tokens"] == 7 {
            saw_usage = true;
        }
    }
    assert!(saw_role, "role chunk lost: {s}");
    assert!(saw_finish, "finish_reason chunk lost: {s}");
    assert!(saw_usage, "usage chunk lost: {s}");
}

// ---------------------------------------------------------------------------
// 4. Anthropic: content_block_delta split across events, message_stop flush
// ---------------------------------------------------------------------------

#[tokio::test]
async fn regression_anthropic_placeholder_split_across_events() {
    let mock_server = MockServer::start().await;

    let frags = ["reach ", "EMAIL", "_", "1", " now"];
    let mut sse_body = String::new();
    sse_body.push_str(
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    );
    for frag in frags {
        sse_body.push_str(&format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}\n\n",
            serde_json::to_string(frag).unwrap()
        ));
    }
    sse_body.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_body.into_bytes(), "text/event-stream")
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let app = test_router(anthropic_pii_config(&mock_server.uri()));
    let req_body = serde_json::json!({
        "model": "claude-3-5-sonnet",
        "stream": true,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "reach max.mustermann@example.com now"}]
    });
    let resp = app
        .oneshot(anthropic_request(
            "sk-drgtw-ssetest001",
            &req_body.to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = collect_body(resp).await;
    let joined = join_anthropic_text(&body);
    assert_eq!(joined, "reach max.mustermann@example.com now");
    assert!(!joined.contains("EMAIL_1"));
    let s = std::str::from_utf8(&body).unwrap();
    assert!(s.contains("message_stop"), "terminator must survive: {s}");
}
