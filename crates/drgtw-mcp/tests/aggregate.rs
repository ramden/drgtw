//! Wiremock-backed integration tests for [`McpGateway::aggregate_tools`].

use drgtw_mcp::{McpGateway, UpstreamServer};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Mount a fully-working MCP server exposing the given tool names.
async fn mount_mcp_server(server: &MockServer, tools: &[&str]) {
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "initialize" })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "s")
                .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": {} })),
        )
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({ "method": "notifications/initialized" }),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(server)
        .await;

    let tool_objs: Vec<Value> = tools
        .iter()
        .map(|n| json!({ "name": n, "description": format!("the {n} tool") }))
        .collect();

    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/list" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 2, "result": { "tools": tool_objs }
        })))
        .mount(server)
        .await;
}

fn names(tools: &[Value]) -> Vec<String> {
    let mut v: Vec<String> = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

/// Two healthy upstreams merge into one prefixed tool list.
#[tokio::test]
async fn aggregates_two_servers_with_prefixed_names() {
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    mount_mcp_server(&a, &["search", "fetch"]).await;
    mount_mcp_server(&b, &["read"]).await;

    let gw = McpGateway::new(
        vec![
            UpstreamServer {
                name: "alpha".into(),
                url: a.uri(),
                headers: vec![],
                forward_headers: vec![],
            },
            UpstreamServer {
                name: "beta".into(),
                url: b.uri(),
                headers: vec![],
                forward_headers: vec![],
            },
        ],
        reqwest::Client::new(),
    );

    let tools = gw.aggregate_tools(&[]).await;
    assert_eq!(
        names(&tools),
        vec![
            "alpha-fetch".to_string(),
            "alpha-search".to_string(),
            "beta-read".to_string()
        ]
    );
    // Non-name fields are preserved.
    let search = tools.iter().find(|t| t["name"] == "alpha-search").unwrap();
    assert_eq!(search["description"], "the search tool");
}

/// A failing upstream (HTTP 500) is skipped; the healthy one still returns.
#[tokio::test]
async fn one_server_erroring_does_not_drop_the_other() {
    let good = MockServer::start().await;
    let bad = MockServer::start().await;
    mount_mcp_server(&good, &["ok"]).await;

    // bad server: everything → 500.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&bad)
        .await;

    let gw = McpGateway::new(
        vec![
            UpstreamServer {
                name: "good".into(),
                url: good.uri(),
                headers: vec![],
                forward_headers: vec![],
            },
            UpstreamServer {
                name: "bad".into(),
                url: bad.uri(),
                headers: vec![],
                forward_headers: vec![],
            },
        ],
        reqwest::Client::new(),
    );

    let tools = gw.aggregate_tools(&[]).await;
    assert_eq!(names(&tools), vec!["good-ok".to_string()]);
}
