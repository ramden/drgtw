//! Integration tests for the MCP gateway handler (WP-C).
//!
//! These build the full axum router with a real `ProxyState` and drive
//! `POST /mcp` end-to-end via `tower::ServiceExt::oneshot`. A single wiremock
//! mock plays the upstream MCP server: it inspects the JSON-RPC `method` in each
//! request body and answers `initialize`, `notifications/initialized`,
//! `tools/list`, and `tools/call` appropriately, capturing the upstream auth
//! header so the auth-forwarding test can assert on it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::{Config, McpAuthType, McpServerConfig, PiiConfig, ServerConfig, VirtualKey};
use drgtw_proxy::{ProxyState, router};
use serde_json::{Value, json};
use tower::ServiceExt; // for `.oneshot()`
use wiremock::{Mock, MockServer, Request as WmRequest, Respond, ResponseTemplate};

const VKEY: &str = "sk-drgtw-mcptest0001";
const UPSTREAM_BEARER: &str = "upstream-mcp-secret";

// ---------------------------------------------------------------------------
// Upstream MCP mock
// ---------------------------------------------------------------------------

/// A wiremock responder that answers MCP JSON-RPC requests by method.
///
/// * `initialize` → result + `Mcp-Session-Id` header.
/// * `notifications/initialized` (notification) → 202, empty body.
/// * `tools/list` → one tool named `search`.
/// * `tools/call` → echoes the bare tool name + arguments in the result, so the
///   gateway-side test can confirm the upstream received the *un*-prefixed name.
/// * anything else → JSON-RPC method-not-found.
struct McpUpstream;

impl Respond for McpUpstream {
    fn respond(&self, request: &WmRequest) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let method = body.get("method").and_then(Value::as_str).unwrap_or("");
        let id = body.get("id").cloned();

        // Notifications carry no id and expect no response body.
        if id.is_none() {
            return ResponseTemplate::new(202);
        }

        let result = match method {
            "initialize" => {
                let result = json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "mock-upstream", "version": "0.0.0" },
                });
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("mcp-session-id", "test-session-123")
                    .set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result,
                    }));
            }
            "tools/list" => json!({
                "tools": [
                    {
                        "name": "search",
                        "description": "search the docs",
                        "inputSchema": { "type": "object" },
                    }
                ]
            }),
            "tools/call" => {
                let params = body.get("params").cloned().unwrap_or(Value::Null);
                json!({
                    "content": [
                        { "type": "text", "text": "ok" }
                    ],
                    "echo": params,
                })
            }
            _ => {
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": "method not found" },
                    }));
            }
        };

        ResponseTemplate::new(200)
            .insert_header("content-type", "application/json")
            .set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }))
    }
}

// ---------------------------------------------------------------------------
// Config / router helpers
// ---------------------------------------------------------------------------

fn base_config(mcp_servers: HashMap<String, McpServerConfig>) -> Arc<Config> {
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
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: PiiConfig::default(),
        events: None,
        fallback: Default::default(),
        mcp_servers,
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    })
}

/// Config with a single upstream MCP server "ctx" reachable at `upstream_url`,
/// authenticated with a bearer token.
fn config_with_ctx(upstream_url: &str) -> Arc<Config> {
    let mut servers = HashMap::new();
    servers.insert(
        "ctx".to_string(),
        McpServerConfig {
            url: upstream_url.to_string(),
            description: None,
            auth_type: McpAuthType::Bearer,
            auth_value: Some(UPSTREAM_BEARER.to_string()),
            extra_headers: HashMap::new(),
            forward_headers: vec![],
        },
    );
    base_config(servers)
}

fn test_router(config: Arc<Config>) -> axum::Router {
    let state = Arc::new(
        ProxyState::new(config, std::path::Path::new(".")).expect("ProxyState::new failed in test"),
    );
    router(state)
}

/// POST /mcp with the virtual-key Authorization header and a JSON body.
fn mcp_request(virtual_key: Option<&str>, body: impl Into<String>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Content-Type", "application/json");
    if let Some(k) = virtual_key {
        builder = builder.header("Authorization", format!("Bearer {k}"));
    }
    builder.body(Body::from(body.into())).unwrap()
}

async fn collect_body(resp: axum::response::Response) -> bytes::Bytes {
    use http_body_util::BodyExt;
    resp.into_body().collect().await.unwrap().to_bytes()
}

async fn mount_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("POST"))
        .respond_with(McpUpstream)
        .mount(&server)
        .await;
    server
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_key_is_unauthorized() {
    let app = test_router(base_config(HashMap::new()));
    let req = mcp_request(None, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bad_key_is_unauthorized() {
    let app = test_router(base_config(HashMap::new()));
    let req = mcp_request(
        Some("sk-drgtw-wrong"),
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn initialize_returns_protocol_and_session_header() {
    let app = test_router(base_config(HashMap::new()));
    let req = mcp_request(
        Some(VKEY),
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().contains_key("mcp-session-id"),
        "initialize must issue an Mcp-Session-Id header"
    );
    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(body["result"]["serverInfo"]["name"], "drgtw");
    assert_eq!(
        body["result"]["capabilities"]["tools"]["listChanged"],
        json!(false)
    );
}

#[tokio::test]
async fn notification_returns_202_empty() {
    let app = test_router(base_config(HashMap::new()));
    let req = mcp_request(
        Some(VKEY),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = collect_body(resp).await;
    assert!(
        body.is_empty(),
        "notification response must have an empty body"
    );
}

#[tokio::test]
async fn ping_returns_empty_result() {
    let app = test_router(base_config(HashMap::new()));
    let req = mcp_request(Some(VKEY), r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["id"], json!(7));
    assert_eq!(body["result"], json!({}));
}

#[tokio::test]
async fn tools_list_prefixes_server_name() {
    let upstream = mount_upstream().await;
    let app = test_router(config_with_ctx(&upstream.uri()));
    let req = mcp_request(
        Some(VKEY),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    let tools = body["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "ctx-search");
}

#[tokio::test]
async fn tools_call_routes_to_upstream_with_bare_name_and_auth() {
    let upstream = mount_upstream().await;
    let app = test_router(config_with_ctx(&upstream.uri()));
    let req = mcp_request(
        Some(VKEY),
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ctx-search","arguments":{"q":"rust"}}}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();

    // Result passed through verbatim from upstream; the upstream echoed the
    // params it received, which must carry the BARE tool name and arguments.
    assert_eq!(body["result"]["echo"]["name"], "search");
    assert_eq!(body["result"]["echo"]["arguments"]["q"], "rust");
    assert_eq!(body["result"]["content"][0]["text"], "ok");

    // The bearer auth header derived from config must have reached the upstream.
    let received = upstream.received_requests().await.unwrap();
    assert!(
        received.iter().any(|r| {
            r.headers.get("authorization").and_then(|v| v.to_str().ok())
                == Some(&format!("Bearer {UPSTREAM_BEARER}"))
        }),
        "upstream must receive the configured bearer auth header"
    );
}

#[tokio::test]
async fn tools_call_unknown_tool_is_invalid_params() {
    let upstream = mount_upstream().await;
    let app = test_router(config_with_ctx(&upstream.uri()));
    let req = mcp_request(
        Some(VKEY),
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope-missing","arguments":{}}}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["error"]["code"], json!(-32602));
}

#[tokio::test]
async fn unknown_method_is_method_not_found() {
    let app = test_router(base_config(HashMap::new()));
    let req = mcp_request(
        Some(VKEY),
        r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["error"]["code"], json!(-32601));
}

#[tokio::test]
async fn malformed_body_is_parse_error() {
    let app = test_router(base_config(HashMap::new()));
    let req = mcp_request(Some(VKEY), "this is not json{");
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["error"]["code"], json!(-32700));
    assert_eq!(body["id"], Value::Null);
}

#[tokio::test]
async fn valid_json_missing_method_is_invalid_request() {
    let app = test_router(base_config(HashMap::new()));
    let req = mcp_request(Some(VKEY), r#"{"foo":1}"#);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["error"]["code"], json!(-32600));
    assert_eq!(body["id"], Value::Null);
}

#[tokio::test]
async fn zero_servers_tools_list_is_empty() {
    let app = test_router(base_config(HashMap::new()));
    let req = mcp_request(
        Some(VKEY),
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/list"}"#,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    let tools = body["result"]["tools"].as_array().expect("tools array");
    assert!(
        tools.is_empty(),
        "zero configured servers → empty tools list"
    );
}
