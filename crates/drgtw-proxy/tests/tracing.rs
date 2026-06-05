//! Integration tests for the filesystem request tracer wired into the proxy.
//!
//! Each test builds the full axum router with a real `ProxyState` whose
//! `[tracing]` section points at a fresh tempdir, drives a request end-to-end
//! via `tower::ServiceExt::oneshot`, then flushes deterministically: the test
//! holds a `TraceWriter` clone (via `ProxyState::trace_handle`), drops the
//! router so the state-held sender is released, and awaits
//! `TraceWriter::shutdown` so every queued entry is on disk before assertions.
//!
//! Privacy invariant under test: LLM-endpoint entries carry metadata only —
//! the request prompt text must never appear in the trace file.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::{
    ApiFormat, Config, Connection, McpAuthType, McpServerConfig, PiiConfig, ServerConfig,
    TracingConfig, VirtualKey,
};
use drgtw_proxy::{ProxyState, router};
use serde_json::{json, Value};
use tower::ServiceExt; // for `.oneshot()`
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as WmRequest, Respond, ResponseTemplate};

const VKEY: &str = "sk-drgtw-tracetest01";
const TRACE_FILE: &str = "drgtw-trace.jsonl";

// ---------------------------------------------------------------------------
// Config helpers
// ---------------------------------------------------------------------------

/// A `[tracing]` section pointing at `dir`, enabled or disabled per `enabled`.
fn tracing_config(dir: &std::path::Path, enabled: bool) -> TracingConfig {
    TracingConfig {
        enabled,
        dir: dir.to_string_lossy().into_owned(),
        ..TracingConfig::default()
    }
}

/// Chat-only config: one open_ai connection pointed at the mock upstream, one
/// virtual key allowed `gpt-4o`, tracing enabled/disabled per `tracing`.
fn chat_config(mock_base_url: &str, tracing: TracingConfig) -> Arc<Config> {
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
            model_costs: Default::default(),
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
        tracing,
        model_aliases: Default::default(),
        otel: Default::default(),
    })
}

/// MCP config: one upstream MCP server "ctx", tracing enabled at `dir`.
fn mcp_config(upstream_url: &str, dir: &std::path::Path) -> Arc<Config> {
    let mut servers = HashMap::new();
    servers.insert(
        "ctx".to_string(),
        McpServerConfig {
            url: upstream_url.to_string(),
            description: None,
            auth_type: McpAuthType::None,
            auth_value: None,
            extra_headers: HashMap::new(),
        },
    );
    Arc::new(Config {
        server: ServerConfig {
            bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
            ..Default::default()
        },
        connections: vec![],
        virtual_keys: vec![VirtualKey {
            key: VKEY.into(),
            connections: vec![],
            models: Some(vec![]),
            rate_limit: None,
            budget: None,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers: servers,
        tracing: tracing_config(dir, true),
        model_aliases: Default::default(),
        otel: Default::default(),
    })
}

/// Build the router and return it alongside the trace-writer handle (if any),
/// so a test can flush deterministically after dropping the router.
fn build(config: Arc<Config>) -> (axum::Router, Option<drgtw_trace::TraceWriter>) {
    let state = Arc::new(
        ProxyState::new(config, std::path::Path::new("."))
            .expect("ProxyState::new failed in test"),
    );
    let handle = state.trace_handle();
    (router(state), handle)
}

/// Flush the tracer deterministically: drop the router (releasing the
/// state-held sender) then await shutdown on the held clone.
async fn flush(app: axum::Router, handle: Option<drgtw_trace::TraceWriter>) {
    drop(app);
    if let Some(h) = handle {
        h.shutdown().await;
    }
}

fn chat_request(body: impl Into<String>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {VKEY}"))
        .header("x-drgtw-request-id", "req-trace-1")
        .body(Body::from(body.into()))
        .unwrap()
}

fn mcp_request(body: impl Into<String>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {VKEY}"))
        .header("x-drgtw-request-id", "req-mcp-1")
        .body(Body::from(body.into()))
        .unwrap()
}

/// Read all parsed JSONL trace entries from `<dir>/drgtw-trace.jsonl`.
fn read_entries(dir: &std::path::Path) -> Vec<Value> {
    let path = dir.join(TRACE_FILE);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("trace file {} unreadable: {e}", path.display()));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("each trace line must be valid JSON"))
        .collect()
}

// ---------------------------------------------------------------------------
// LLM mock upstream
// ---------------------------------------------------------------------------

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

async fn mount_chat_429(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_json(json!({"error": {"message": "slow down"}}))
                .insert_header("content-type", "application/json"),
        )
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// MCP mock upstream (answers tools/call by echoing params)
// ---------------------------------------------------------------------------

struct McpUpstream;

impl Respond for McpUpstream {
    fn respond(&self, request: &WmRequest) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let id = body.get("id").cloned();
        let method = body.get("method").and_then(Value::as_str).unwrap_or("");
        if id.is_none() {
            return ResponseTemplate::new(202);
        }
        let result = match method {
            "tools/list" => json!({
                "tools": [
                    {"name": "search", "description": "search", "inputSchema": {"type": "object"}}
                ]
            }),
            "tools/call" => json!({
                "content": [{"type": "text", "text": "ok"}],
            }),
            _ => {
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "method not found"},
                    }));
            }
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/json")
            .set_body_json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }
}

async fn mount_mcp(server: &MockServer) {
    Mock::given(method("POST"))
        .respond_with(McpUpstream)
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_round_trip_writes_one_metadata_entry() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = MockServer::start().await;
    mount_chat_ok(&upstream).await;

    let (app, handle) = build(chat_config(&upstream.uri(), tracing_config(dir.path(), true)));
    // A unique prompt string we assert NEVER lands in the trace file.
    let prompt = "SENTINEL_PROMPT_should_not_be_traced";
    let req = chat_request(format!(
        r#"{{"model":"gpt-4o","messages":[{{"role":"user","content":"{prompt}"}}]}}"#
    ));
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    flush(app, handle).await;

    let entries = read_entries(dir.path());
    assert_eq!(entries.len(), 1, "exactly one trace entry expected");
    let e = &entries[0];
    assert_eq!(e["kind"], "chat");
    assert_eq!(e["request_id"], "req-trace-1");
    assert_eq!(e["status"], 200);
    assert_eq!(e["model"], "gpt-4o");
    assert_eq!(e["connection"], "mock-openai");
    assert_eq!(e["input_tokens"], 11);
    assert_eq!(e["output_tokens"], 7);
    // Virtual key identifier present, but never the raw secret.
    assert!(e["virtual_key"].is_string());
    assert_ne!(e["virtual_key"], VKEY);

    // No body content: the prompt text must be absent from the whole file.
    let raw = std::fs::read_to_string(dir.path().join(TRACE_FILE)).unwrap();
    assert!(
        !raw.contains(prompt),
        "prompt text leaked into trace file: {raw}"
    );
    assert!(!raw.contains(VKEY), "raw virtual key secret leaked into trace");
}

#[tokio::test]
async fn mcp_tools_call_round_trip_traces_args_and_output() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = MockServer::start().await;
    mount_mcp(&upstream).await;

    let (app, handle) = build(mcp_config(&upstream.uri(), dir.path()));
    let req = mcp_request(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ctx-search","arguments":{"q":"rust"}}}"#,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    flush(app, handle).await;

    let entries = read_entries(dir.path());
    let call = entries
        .iter()
        .find(|e| e["kind"] == "mcp" && e["method"] == "tools/call")
        .expect("an mcp tools/call entry must be present");
    assert_eq!(call["request_id"], "req-mcp-1");
    assert_eq!(call["tool"], "ctx-search");
    assert_eq!(call["server"], "ctx");
    assert_eq!(call["status"], 200);
    assert_eq!(call["arguments"]["q"], "rust");
    assert_eq!(call["output"]["content"][0]["text"], "ok");
}

#[tokio::test]
async fn tracing_disabled_creates_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = MockServer::start().await;
    mount_chat_ok(&upstream).await;

    let (app, handle) = build(chat_config(&upstream.uri(), tracing_config(dir.path(), false)));
    assert!(handle.is_none(), "disabled tracing must yield no TraceWriter");
    let req = chat_request(r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    flush(app, handle).await;

    // No active trace file should exist (the writer was never constructed).
    assert!(
        !dir.path().join(TRACE_FILE).exists(),
        "no trace file must be created when tracing is disabled"
    );
}

#[tokio::test]
async fn upstream_429_is_traced_with_status_429() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = MockServer::start().await;
    mount_chat_429(&upstream).await;

    let (app, handle) = build(chat_config(&upstream.uri(), tracing_config(dir.path(), true)));
    let req = chat_request(r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    flush(app, handle).await;

    let entries = read_entries(dir.path());
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e["kind"], "chat");
    assert_eq!(e["status"], 429);
    assert!(e["error"].is_string(), "non-2xx relay should record an error");
}
