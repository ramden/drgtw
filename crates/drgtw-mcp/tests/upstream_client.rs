//! Wiremock-backed integration tests for [`UpstreamClient`].

use drgtw_mcp::{UpstreamClient, UpstreamError, UpstreamServer};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// JSON-RPC method name carried in a request body, if any.
fn rpc_method(req: &Request) -> Option<String> {
    let v: Value = serde_json::from_slice(&req.body).ok()?;
    v.get("method")?.as_str().map(str::to_string)
}

fn client_for(server: &MockServer) -> UpstreamClient {
    UpstreamClient::new(
        UpstreamServer {
            name: "ctx".to_string(),
            url: server.uri(),
            headers: vec![("Authorization".to_string(), "Bearer sekret".to_string())],
            forward_headers: vec![],
        },
        reqwest::Client::new(),
    )
}

/// initialize handshake sends protocolVersion, captures the session id, and
/// replays it on subsequent requests; static auth header sent on every request.
#[tokio::test]
async fn handshake_captures_session_and_sends_it_with_auth() {
    let server = MockServer::start().await;

    // initialize → return a session id header + auth header asserted.
    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("authorization", "Bearer sekret"))
        .and(body_partial_json(json!({ "method": "initialize" })))
        .and(body_partial_json(
            json!({ "params": { "protocolVersion": "2025-06-18" } }),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "sess-123")
                .insert_header("Content-Type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": { "protocolVersion": "2025-06-18", "capabilities": {} }
                })),
        )
        .mount(&server)
        .await;

    // notifications/initialized → 202.
    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("authorization", "Bearer sekret"))
        .and(body_partial_json(
            json!({ "method": "notifications/initialized" }),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    // tools/list → must carry the captured session id + auth header.
    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("authorization", "Bearer sekret"))
        .and(header("mcp-session-id", "sess-123"))
        .and(header("mcp-protocol-version", "2025-06-18"))
        .and(body_partial_json(json!({ "method": "tools/list" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 2,
            "result": { "tools": [ { "name": "search", "description": "find" } ] }
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let tools = client.list_tools(&[]).await.expect("list_tools ok");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "search");
}

/// call_tool returns the upstream `result` object.
#[tokio::test]
async fn call_tool_returns_result() {
    let server = MockServer::start().await;
    mount_handshake(&server, None).await;

    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/call" })))
        .and(body_partial_json(
            json!({ "params": { "name": "echo", "arguments": { "x": 1 } } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 2,
            "result": { "content": [ { "type": "text", "text": "hi" } ] }
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let result = client
        .call_tool("echo", json!({ "x": 1 }), &[])
        .await
        .expect("call ok");
    assert_eq!(result["content"][0]["text"], "hi");
}

/// An upstream JSON-RPC error object surfaces as `UpstreamError::Rpc`.
#[tokio::test]
async fn upstream_rpc_error_surfaces_as_rpc() {
    let server = MockServer::start().await;
    mount_handshake(&server, None).await;

    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/call" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 2,
            "error": { "code": -32000, "message": "boom" }
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let err = client.call_tool("x", json!({}), &[]).await.unwrap_err();
    match err {
        UpstreamError::Rpc { code, message } => {
            assert_eq!(code, -32000);
            assert_eq!(message, "boom");
        }
        other => panic!("expected Rpc, got {other:?}"),
    }
}

/// A `text/event-stream` response body is parsed for the matching id.
#[tokio::test]
async fn sse_response_body_is_parsed() {
    let server = MockServer::start().await;
    mount_handshake(&server, None).await;

    let sse = "event: message\n\
               data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"t\"}]}}\n\n";
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/list" })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse.as_bytes(), "text/event-stream"))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let tools = client.list_tools(&[]).await.expect("sse parsed");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "t");
}

/// A 404 after a session was established triggers re-init + a single retry.
#[tokio::test]
async fn http_404_after_session_triggers_reinit_and_retry() {
    let server = MockServer::start().await;
    mount_handshake(&server, Some("sess-A")).await;

    // First tools/list (with the original session) → 404 (session expired).
    Mock::given(method("POST"))
        .and(header("mcp-session-id", "sess-A"))
        .and(body_partial_json(json!({ "method": "tools/list" })))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    // Re-initialize → issues a NEW session id.
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "initialize" })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "sess-B")
                .set_body_json(json!({ "jsonrpc": "2.0", "id": 3, "result": {} })),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    // Retried tools/list with the NEW session → success.
    Mock::given(method("POST"))
        .and(header("mcp-session-id", "sess-B"))
        .and(body_partial_json(json!({ "method": "tools/list" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 4, "result": { "tools": [ { "name": "ok" } ] }
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let tools = client.list_tools(&[]).await.expect("retry succeeds");
    assert_eq!(tools[0]["name"], "ok");

    // Verify exactly one re-initialize occurred (initialize seen twice total).
    let inits = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| rpc_method(r).as_deref() == Some("initialize"))
        .count();
    assert_eq!(inits, 2, "expected exactly one re-init after 404");
}

/// An allowlisted inbound header is forwarded; a non-allowlisted one is not;
/// protocol headers are never overridden even if listed.
#[tokio::test]
async fn forward_headers_allowlisted_sent_non_allowlisted_skipped_protocol_not_overridden() {
    let server = MockServer::start().await;

    // initialize → accept any POST
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "initialize" })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "s1")
                .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": {} })),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "notifications/initialized" })))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    // tools/list: assert x-trace-id is forwarded AND content-type is not overridden.
    // We also assert x-secret is absent (not in allowlist).
    Mock::given(method("POST"))
        .and(header("x-trace-id", "trace-abc"))
        // content-type must remain application/json (not attacker-supplied value)
        .and(header("content-type", "application/json"))
        .and(body_partial_json(json!({ "method": "tools/list" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 2,
            "result": { "tools": [{ "name": "echo" }] }
        })))
        .mount(&server)
        .await;

    let client = UpstreamClient::new(
        UpstreamServer {
            name: "test".to_string(),
            url: server.uri(),
            headers: vec![("Authorization".to_string(), "Bearer tok".to_string())],
            forward_headers: vec!["x-trace-id".to_string()],
        },
        reqwest::Client::new(),
    );

    let inbound = vec![
        ("x-trace-id".to_string(), "trace-abc".to_string()),
        ("x-secret".to_string(), "should-not-forward".to_string()),
        // Attacker tries to override content-type — must be ignored.
        ("content-type".to_string(), "text/evil".to_string()),
    ];

    let tools = client.list_tools(&inbound).await.expect("list_tools ok");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "echo");

    // Verify x-secret was never sent.
    let reqs = server.received_requests().await.unwrap();
    let list_req = reqs
        .iter()
        .find(|r| {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            v.get("method").and_then(Value::as_str) == Some("tools/list")
        })
        .expect("tools/list request captured");

    assert!(
        !list_req.headers.contains_key("x-secret"),
        "x-secret must not be forwarded"
    );
    // content-type must be application/json, not the attacker value.
    let ct = list_req
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(ct, "application/json", "content-type must not be overridden");
}

/// When forward_headers is empty, no inbound headers are passed through.
#[tokio::test]
async fn no_forward_headers_when_allowlist_is_empty() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "initialize" })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": {} })),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "notifications/initialized" })))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/list" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 2,
            "result": { "tools": [] }
        })))
        .mount(&server)
        .await;

    let client = UpstreamClient::new(
        UpstreamServer {
            name: "test".to_string(),
            url: server.uri(),
            headers: vec![],
            forward_headers: vec![], // empty allowlist
        },
        reqwest::Client::new(),
    );

    let inbound = vec![("x-trace-id".to_string(), "trace-xyz".to_string())];
    let tools = client.list_tools(&inbound).await.expect("list_tools ok");
    assert!(tools.is_empty());

    let reqs = server.received_requests().await.unwrap();
    let list_req = reqs
        .iter()
        .find(|r| {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            v.get("method").and_then(Value::as_str) == Some("tools/list")
        })
        .expect("tools/list captured");

    assert!(
        !list_req.headers.contains_key("x-trace-id"),
        "x-trace-id must not be forwarded when allowlist is empty"
    );
}

/// Mount the initialize + notifications/initialized handshake responses.
/// When `session_id` is provided it is returned on initialize; the first
/// initialize is consumed (`up_to_n_times(1)`) so tests can mount a distinct
/// re-init response afterwards.
async fn mount_handshake(server: &MockServer, session_id: Option<&str>) {
    let mut init = ResponseTemplate::new(200).set_body_json(json!({
        "jsonrpc": "2.0", "id": 1, "result": {}
    }));
    if let Some(sid) = session_id {
        init = init.insert_header("Mcp-Session-Id", sid);
    }
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "initialize" })))
        .respond_with(init)
        .up_to_n_times(1)
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({ "method": "notifications/initialized" }),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(server)
        .await;
}
