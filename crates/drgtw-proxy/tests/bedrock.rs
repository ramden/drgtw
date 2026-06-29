//! Integration tests for AWS Bedrock upstream support (0.0.2).
//!
//! Two deployment shapes are exercised, both with bearer auth:
//!
//! * **Option A — OpenAI-compat.** A stock `open_ai`-format connection pointed
//!   at a Bedrock-shaped base URL (`.../v1`). Bedrock model ids
//!   (`us.anthropic.claude-sonnet-4-6`, `openai.gpt-oss-120b`) route to
//!   `POST /v1/chat/completions` with `Authorization: Bearer <key>`, stream as
//!   standard SSE, and bill via `model_costs` keyed on the Bedrock id
//!   (exact + wildcard).
//!
//! * **Option A2 — native InvokeModel.** A new `bedrock`-format connection
//!   pointed at a base URL with NO `/v1`. The gateway dispatches to
//!   `POST /model/{model}/invoke` with bearer auth, strips the `model` field
//!   into the URL path, injects `anthropic_version` (unless the client supplied
//!   one), reads usage from the Anthropic-shaped response, and rejects
//!   `stream:true` with a clean 400 (no upstream call).
//!
//! All upstreams + the event sink are mocked via wiremock; requests run through
//! the full axum router via `tower::ServiceExt::oneshot`. Neutral names only
//! (Example Corp / example.com).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::{
    ApiFormat, Config, Connection, EventsConfig, ModelCost, PiiConfig, ServerConfig, VirtualKey,
};
use drgtw_proxy::{router, ProxyState};
use serde_json::Value;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as WmRequest, Respond, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn server_config() -> ServerConfig {
    ServerConfig {
        bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
        ..Default::default()
    }
}

/// PII off so request bodies pass through predictably for upstream assertions.
fn pii_off() -> PiiConfig {
    PiiConfig { enabled_by_default: false, ..Default::default() }
}

fn events_config(sink_url: String) -> EventsConfig {
    EventsConfig {
        url: sink_url,
        auth_bearer: None,
        buffer_size: 64,
        timeout_ms: 5_000,
        signing_secret: None,
    }
}

fn test_router(config: Arc<Config>) -> axum::Router {
    let state =
        Arc::new(ProxyState::new(config, std::path::Path::new(".")).expect("ProxyState::new"));
    router(state)
}

fn model_costs(entries: Vec<(&str, f64, f64)>) -> std::collections::HashMap<String, ModelCost> {
    entries
        .into_iter()
        .map(|(m, i, o)| (m.to_string(), ModelCost { input_per_1m: i, output_per_1m: o }))
        .collect()
}

async fn collect_body(resp: axum::response::Response) -> bytes::Bytes {
    use http_body_util::BodyExt;
    resp.into_body().collect().await.unwrap().to_bytes()
}

/// What an upstream request carried: parsed body + the `Authorization` header.
#[derive(Clone)]
struct CapturedReq {
    body: Value,
    authorization: Option<String>,
}

/// Captures every upstream request it receives (body + auth header).
#[derive(Clone)]
struct UpstreamCapture {
    reqs: Arc<Mutex<Vec<CapturedReq>>>,
    response: Value,
}

impl Respond for UpstreamCapture {
    fn respond(&self, req: &WmRequest) -> ResponseTemplate {
        let body = serde_json::from_slice::<Value>(&req.body).unwrap_or(Value::Null);
        let authorization = req
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());
        self.reqs.lock().unwrap().push(CapturedReq { body, authorization });
        ResponseTemplate::new(200)
            .set_body_json(self.response.clone())
            .insert_header("content-type", "application/json")
    }
}

/// Mount an upstream at `mount_path` that captures requests and returns
/// `response`. Returns the shared capture buffer.
async fn mount_capture(
    server: &MockServer,
    mount_path: &str,
    response: Value,
) -> Arc<Mutex<Vec<CapturedReq>>> {
    let reqs = Arc::new(Mutex::new(Vec::new()));
    let cap = UpstreamCapture { reqs: Arc::clone(&reqs), response };
    Mock::given(method("POST"))
        .and(path(mount_path.to_owned()))
        .respond_with(cap)
        .mount(server)
        .await;
    reqs
}

#[derive(Clone)]
struct EventCapture(Arc<Mutex<Vec<Value>>>);

impl Respond for EventCapture {
    fn respond(&self, req: &WmRequest) -> ResponseTemplate {
        if let Ok(v) = serde_json::from_slice::<Value>(&req.body) {
            self.0.lock().unwrap().push(v);
        }
        ResponseTemplate::new(200)
    }
}

async fn mount_event_sink(server: &MockServer) -> Arc<Mutex<Vec<Value>>> {
    let captured = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/events"))
        .respond_with(EventCapture(Arc::clone(&captured)))
        .mount(server)
        .await;
    captured
}

async fn wait_for_events(captured: &Arc<Mutex<Vec<Value>>>, n: usize) {
    for _ in 0..50 {
        if captured.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// An OpenAI-shaped non-streaming response with a usage block.
fn openai_usage_response() -> Value {
    serde_json::json!({
        "id": "chatcmpl-bedrock",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}}],
        "usage": {"prompt_tokens": 1000, "completion_tokens": 500}
    })
}

/// An Anthropic Messages-shaped non-streaming response (what native Bedrock
/// InvokeModel returns for Anthropic models). 40 in / 18 out.
fn anthropic_usage_response() -> Value {
    serde_json::json!({
        "id": "msg_bedrock",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 40, "output_tokens": 18}
    })
}

fn chat_request(virtual_key: &str, body: impl Into<String>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {virtual_key}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.into()))
        .unwrap()
}

fn messages_request(virtual_key: &str, body: impl Into<String>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", virtual_key)
        .header("Content-Type", "application/json")
        .body(Body::from(body.into()))
        .unwrap()
}

// ===========================================================================
// Option A — Bedrock via the OpenAI-compat endpoint + bearer key (WP-1)
// ===========================================================================

/// A stock `open_ai` connection pointed at a Bedrock-shaped base URL serves
/// Bedrock model ids: routes to `/v1/chat/completions`, sends bearer auth, and
/// bills from a `model_costs` entry keyed on the Bedrock id.
#[tokio::test]
async fn bedrock_openai_compat_routes_and_bills() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let reqs = mount_capture(&upstream, "/v1/chat/completions", openai_usage_response()).await;
    let captured = mount_event_sink(&sink).await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![Connection {
            name: "bedrock-openai".into(),
            // Bedrock OpenAI-compat base URL carries the /v1 suffix.
            base_url: format!("{}/v1", upstream.uri()),
            api_key: "bedrock-bearer-token".into(),
            format: ApiFormat::OpenAi,
            models: vec!["us.anthropic.claude-sonnet-4-6".into()],
            model_costs: model_costs(vec![("us.anthropic.claude-sonnet-4-6", 3.0, 15.0)]),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-bedrockoai01".into(),
            connections: vec!["bedrock-openai".into()],
            models: None,
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: pii_off(),
        events: Some(events_config(format!("{}/events", sink.uri()))),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-bedrockoai01",
            r#"{"model":"us.anthropic.claude-sonnet-4-6","messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Upstream saw the Bedrock model id verbatim, with bearer auth.
    let reqs = reqs.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].body["model"], "us.anthropic.claude-sonnet-4-6");
    assert_eq!(reqs[0].authorization.as_deref(), Some("Bearer bedrock-bearer-token"));

    // Usage event billed against the Bedrock-id cost key:
    // 1000/1e6*3 + 500/1e6*15 = 0.003 + 0.0075 = 0.0105.
    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["model"], "us.anthropic.claude-sonnet-4-6");
    assert_eq!(events[0]["input_tokens"], 1000);
    assert_eq!(events[0]["output_tokens"], 500);
    let cost = events[0]["cost_usd"].as_f64().expect("cost present");
    assert!((cost - 0.0105).abs() < 1e-9, "cost was {cost}");
}

/// Streaming SSE passes through byte-identical and usage is captured by the tap
/// (mirrors the OpenAI streaming path — Bedrock OpenAI-compat returns standard
/// SSE chunks with `stream_options.include_usage`).
#[tokio::test]
async fn bedrock_openai_compat_streams_sse() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let captured = mount_event_sink(&sink).await;

    let sse_body = concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":7}}\n\n",
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
        .mount(&upstream)
        .await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![Connection {
            name: "bedrock-openai".into(),
            base_url: format!("{}/v1", upstream.uri()),
            api_key: "bedrock-bearer-token".into(),
            format: ApiFormat::OpenAi,
            models: vec!["openai.gpt-oss-120b".into()],
            model_costs: model_costs(vec![("openai.gpt-oss-120b", 0.15, 0.60)]),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-bedrockoai02".into(),
            connections: vec!["bedrock-openai".into()],
            models: None,
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: pii_off(),
        events: Some(events_config(format!("{}/events", sink.uri()))),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-bedrockoai02",
            r#"{"model":"openai.gpt-oss-120b","messages":[],"stream":true,"stream_options":{"include_usage":true}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.contains("text/event-stream"), "content-type was: {ct}");

    let body_bytes = collect_body(resp).await;
    assert_eq!(body_bytes.as_ref(), sse_body.as_bytes(), "SSE relayed byte-identical");

    // Usage captured by the tap: 12 in / 7 out.
    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["input_tokens"], 12);
    assert_eq!(events[0]["output_tokens"], 7);
    assert_eq!(events[0]["streamed"], true);
}

/// A wildcard `model_costs` key (`us.anthropic.claude-*`) matches a concrete
/// Bedrock id and bills correctly.
#[tokio::test]
async fn bedrock_cost_wildcard_key_matches() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let reqs = mount_capture(&upstream, "/v1/chat/completions", openai_usage_response()).await;
    let captured = mount_event_sink(&sink).await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![Connection {
            name: "bedrock-openai".into(),
            base_url: format!("{}/v1", upstream.uri()),
            api_key: "bedrock-bearer-token".into(),
            format: ApiFormat::OpenAi,
            // Connection serves the concrete id via its own wildcard model entry.
            models: vec!["us.anthropic.claude-*".into()],
            // Cost keyed by wildcard — must match the concrete id below.
            model_costs: model_costs(vec![("us.anthropic.claude-*", 3.0, 15.0)]),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-bedrockoai03".into(),
            connections: vec!["bedrock-openai".into()],
            models: None,
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: pii_off(),
        events: Some(events_config(format!("{}/events", sink.uri()))),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-bedrockoai03",
            r#"{"model":"us.anthropic.claude-sonnet-4-6","messages":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(reqs.lock().unwrap().len(), 1);

    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    let cost = events[0]["cost_usd"].as_f64().expect("wildcard cost present");
    assert!((cost - 0.0105).abs() < 1e-9, "wildcard cost was {cost}");
}

// ===========================================================================
// Option A2 — native Bedrock InvokeModel (bearer, non-streaming) (WP-4)
// ===========================================================================

/// A `bedrock`-format connection dispatches to `/model/{model}/invoke` with
/// bearer auth, strips `model` from the body into the URL path, injects
/// `anthropic_version`, and bills from the Anthropic-shaped usage block.
#[tokio::test]
async fn bedrock_invoke_non_streaming_routes_and_bills() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    // Model id with a `:0` revision suffix → path-encoded to `%3A0`.
    let reqs = mount_capture(
        &upstream,
        "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/invoke",
        anthropic_usage_response(),
    )
    .await;
    let captured = mount_event_sink(&sink).await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![Connection {
            name: "bedrock-native".into(),
            // Native base URL: NO /v1 suffix.
            base_url: upstream.uri(),
            api_key: "bedrock-bearer-token".into(),
            format: ApiFormat::Bedrock,
            models: vec!["anthropic.claude-3-5-sonnet-20241022-v2:0".into()],
            model_costs: model_costs(vec![(
                "anthropic.claude-3-5-sonnet-20241022-v2:0",
                3.0,
                15.0,
            )]),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-bedrocknat01".into(),
            connections: vec!["bedrock-native".into()],
            models: None,
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: pii_off(),
        events: Some(events_config(format!("{}/events", sink.uri()))),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(messages_request(
            "sk-drgtw-bedrocknat01",
            r#"{"model":"anthropic.claude-3-5-sonnet-20241022-v2:0","messages":[{"role":"user","content":"hi"}],"max_tokens":64}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Path routing + bearer auth proven by the mount path + captured header;
    // body transform: no `model`, anthropic_version injected, rest preserved.
    let reqs = reqs.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].authorization.as_deref(), Some("Bearer bedrock-bearer-token"));
    assert!(reqs[0].body.get("model").is_none(), "model stripped into URL path");
    assert_eq!(reqs[0].body["anthropic_version"], "bedrock-2023-05-31");
    assert_eq!(reqs[0].body["max_tokens"], 64, "rest of body preserved");

    // Usage from the Anthropic-shaped block: 40 in / 18 out.
    // cost = 40/1e6*3 + 18/1e6*15 = 0.00012 + 0.00027 = 0.00039.
    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["input_tokens"], 40);
    assert_eq!(events[0]["output_tokens"], 18);
    let cost = events[0]["cost_usd"].as_f64().expect("cost present");
    assert!((cost - 0.00039).abs() < 1e-9, "cost was {cost}");
}

/// When the client supplies its own `anthropic_version`, the gateway preserves
/// it rather than overwriting with the Bedrock default.
#[tokio::test]
async fn bedrock_invoke_preserves_client_anthropic_version() {
    let upstream = MockServer::start().await;
    let reqs = mount_capture(
        &upstream,
        "/model/eu.anthropic.claude-sonnet-4-6/invoke",
        anthropic_usage_response(),
    )
    .await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![Connection {
            name: "bedrock-native".into(),
            base_url: upstream.uri(),
            api_key: "bedrock-bearer-token".into(),
            format: ApiFormat::Bedrock,
            models: vec!["eu.anthropic.claude-sonnet-4-6".into()],
            model_costs: Default::default(),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-bedrocknat02".into(),
            connections: vec!["bedrock-native".into()],
            models: None,
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: pii_off(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(messages_request(
            "sk-drgtw-bedrocknat02",
            r#"{"model":"eu.anthropic.claude-sonnet-4-6","anthropic_version":"custom-version","messages":[],"max_tokens":8}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let reqs = reqs.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].body["anthropic_version"], "custom-version",
        "client-supplied anthropic_version is preserved"
    );
    assert!(reqs[0].body.get("model").is_none(), "model still stripped");
}

/// The model id is moved into the URL path (not duplicated in the body) — a
/// plain id with no `:` suffix routes to `/model/{id}/invoke`.
#[tokio::test]
async fn bedrock_invoke_strips_model_into_path() {
    let upstream = MockServer::start().await;
    // The mount path encodes the expected URL: a request reaching the capture
    // proves the model was moved into `/model/{id}/invoke`.
    let reqs = mount_capture(
        &upstream,
        "/model/eu.anthropic.claude-sonnet-4-6/invoke",
        anthropic_usage_response(),
    )
    .await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![Connection {
            name: "bedrock-native".into(),
            base_url: upstream.uri(),
            api_key: "bedrock-bearer-token".into(),
            format: ApiFormat::Bedrock,
            models: vec!["eu.anthropic.claude-sonnet-4-6".into()],
            model_costs: Default::default(),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-bedrocknat03".into(),
            connections: vec!["bedrock-native".into()],
            models: None,
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: pii_off(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(messages_request(
            "sk-drgtw-bedrocknat03",
            r#"{"model":"eu.anthropic.claude-sonnet-4-6","messages":[],"max_tokens":8}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let reqs = reqs.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1, "request reached the /model/{{id}}/invoke path");
    assert!(reqs[0].body.get("model").is_none());
}

/// A `stream:true` request against a `bedrock` connection is rejected with a
/// clean 400 (Anthropic error shape) and NO upstream call is made.
#[tokio::test]
async fn bedrock_streaming_request_rejected_no_upstream_call() {
    let upstream = MockServer::start().await;
    // If the guard fails to fire, this capture would record the dispatched call.
    let reqs = mount_capture(
        &upstream,
        "/model/eu.anthropic.claude-sonnet-4-6/invoke",
        anthropic_usage_response(),
    )
    .await;

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![Connection {
            name: "bedrock-native".into(),
            base_url: upstream.uri(),
            api_key: "bedrock-bearer-token".into(),
            format: ApiFormat::Bedrock,
            models: vec!["eu.anthropic.claude-sonnet-4-6".into()],
            model_costs: Default::default(),
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-bedrocknat04".into(),
            connections: vec!["bedrock-native".into()],
            models: None,
            rate_limit: None,
            budget: None,
            mcp_servers: None,
            allow_pii_bypass: false,
        }],
        pii: pii_off(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
        guardrails: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(messages_request(
            "sk-drgtw-bedrocknat04",
            r#"{"model":"eu.anthropic.claude-sonnet-4-6","messages":[],"max_tokens":8,"stream":true}"#,
        ))
        .await
        .unwrap();

    // Clean 400 with the Anthropic error body shape.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = collect_body(resp).await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("streaming"),
        "message mentions the streaming limitation: {v}"
    );

    // No upstream request was dispatched.
    assert_eq!(reqs.lock().unwrap().len(), 0, "no upstream call on stream guard");
}
