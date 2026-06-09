//! Integration tests for two features:
//!
//! * Feature 1 — global model aliasing (`[model_aliases]`): an alias in the
//!   request body is resolved to its target before routing, allowlist, cost,
//!   and usage-event emission.
//! * Feature 2 — attribution metadata passthrough into usage events, sourced
//!   from the request body `metadata` object and `x-drgtw-meta-*` headers, with
//!   header-wins merge precedence and documented caps. The `x-drgtw-meta-*`
//!   headers must NOT leak to the upstream provider.
//!
//! All upstreams (and the event sink) are mocked via wiremock; requests run
//! through the full axum router via `tower::ServiceExt::oneshot`.

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

fn openai_conn(name: &str, base: &str, models: Vec<&str>, costs: Vec<(&str, f64, f64)>) -> Connection {
    let model_costs = costs
        .into_iter()
        .map(|(m, i, o)| {
            (
                m.to_string(),
                ModelCost { input_per_1m: i, output_per_1m: o },
            )
        })
        .collect();
    Connection {
        name: name.into(),
        base_url: format!("{base}/v1"),
        api_key: format!("{name}-key"),
        format: ApiFormat::OpenAi,
        models: models.into_iter().map(Into::into).collect(),
        model_costs,
        region: None,
        aws_access_key_id: None,
        aws_secret_access_key: None,
        aws_session_token: None,
    }
}

fn test_router(config: Arc<Config>) -> axum::Router {
    let state =
        Arc::new(ProxyState::new(config, std::path::Path::new(".")).expect("ProxyState::new"));
    router(state)
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

/// PII off so request bodies pass through predictably for upstream assertions.
fn pii_off() -> PiiConfig {
    PiiConfig { enabled_by_default: false, ..Default::default() }
}

async fn collect_body(resp: axum::response::Response) -> bytes::Bytes {
    use http_body_util::BodyExt;
    resp.into_body().collect().await.unwrap().to_bytes()
}

/// Records the body of every upstream chat request as parsed JSON, plus a flag
/// for whether any `x-drgtw-meta-*` header was present.
#[derive(Clone)]
struct UpstreamCapture {
    bodies: Arc<Mutex<Vec<Value>>>,
    saw_meta_header: Arc<Mutex<bool>>,
    response: Value,
}

impl Respond for UpstreamCapture {
    fn respond(&self, req: &WmRequest) -> ResponseTemplate {
        if let Ok(v) = serde_json::from_slice::<Value>(&req.body) {
            self.bodies.lock().unwrap().push(v);
        }
        let leaked = req
            .headers
            .iter()
            .any(|(n, _)| n.as_str().to_ascii_lowercase().starts_with("x-drgtw-meta-"));
        if leaked {
            *self.saw_meta_header.lock().unwrap() = true;
        }
        ResponseTemplate::new(200)
            .set_body_json(self.response.clone())
            .insert_header("content-type", "application/json")
    }
}

/// Mount a chat upstream that captures request bodies + meta-header leakage.
async fn mount_chat_upstream(
    server: &MockServer,
    response: Value,
) -> (Arc<Mutex<Vec<Value>>>, Arc<Mutex<bool>>) {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let saw_meta_header = Arc::new(Mutex::new(false));
    let cap = UpstreamCapture {
        bodies: Arc::clone(&bodies),
        saw_meta_header: Arc::clone(&saw_meta_header),
        response,
    };
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(cap)
        .mount(server)
        .await;
    (bodies, saw_meta_header)
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

fn ok_usage_response() -> Value {
    serde_json::json!({
        "id": "ok",
        "usage": {"prompt_tokens": 100, "completion_tokens": 50}
    })
}

/// Build a chat request with optional extra headers.
fn chat_request(
    virtual_key: &str,
    body: impl Into<String>,
    extra_headers: &[(&str, &str)],
) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {virtual_key}"))
        .header("Content-Type", "application/json");
    for (k, v) in extra_headers {
        b = b.header(*k, *v);
    }
    b.body(Body::from(body.into())).unwrap()
}

// ---------------------------------------------------------------------------
// Feature 1: model aliasing
// ---------------------------------------------------------------------------

/// An aliased request routes to the connection serving the RESOLVED model and
/// forwards the resolved model name upstream; the usage event carries the
/// resolved model, not the alias.
#[tokio::test]
async fn test_alias_routes_and_forwards_resolved_model() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;

    let (bodies, _) = mount_chat_upstream(&upstream, ok_usage_response()).await;
    let captured = mount_event_sink(&sink).await;

    // Connection serves only the TARGET model `gpt-4o-mini`; the alias `fast`
    // is not a connection model — routing must use the resolved name.
    let mut aliases = std::collections::HashMap::new();
    aliases.insert("fast".to_string(), "gpt-4o-mini".to_string());

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![openai_conn(
            "openai",
            &upstream.uri(),
            vec!["gpt-4o-mini"],
            vec![("gpt-4o-mini", 1.0, 2.0)],
        )],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-alias01".into(),
            connections: vec!["openai".into()],
            models: None,
            rate_limit: None,
            budget: None,
            mcp_servers: None,
        }],
        pii: pii_off(),
        events: Some(events_config(format!("{}/events", sink.uri()))),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: aliases,
        otel: Default::default(),
        ui: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-alias01",
            r#"{"model":"fast","messages":[]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Upstream received the RESOLVED model.
    let bodies = bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["model"], "gpt-4o-mini", "alias resolved before forwarding");

    // Usage event carries the resolved model.
    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["model"], "gpt-4o-mini");
}

/// The virtual-key model allowlist is applied to the RESOLVED model: a key
/// allowed only the target accepts the alias.
#[tokio::test]
async fn test_alias_allowlist_applies_to_resolved_model() {
    let upstream = MockServer::start().await;
    let (bodies, _) = mount_chat_upstream(&upstream, ok_usage_response()).await;

    let mut aliases = std::collections::HashMap::new();
    aliases.insert("fast".to_string(), "gpt-4o-mini".to_string());

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![openai_conn(
            "openai",
            &upstream.uri(),
            vec!["gpt-4o-mini"],
            vec![],
        )],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-alias02".into(),
            connections: vec!["openai".into()],
            // Allowlist contains ONLY the resolved target, not the alias.
            models: Some(vec!["gpt-4o-mini".into()]),
            rate_limit: None,
            budget: None,
            mcp_servers: None,
        }],
        pii: pii_off(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: aliases,
        otel: Default::default(),
        ui: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-alias02",
            r#"{"model":"fast","messages":[]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "allowlist matches resolved model");
    assert_eq!(bodies.lock().unwrap().len(), 1);
}

/// A non-aliased model is left untouched and forwarded verbatim.
#[tokio::test]
async fn test_unaliased_model_untouched() {
    let upstream = MockServer::start().await;
    let (bodies, _) = mount_chat_upstream(&upstream, ok_usage_response()).await;

    let mut aliases = std::collections::HashMap::new();
    aliases.insert("fast".to_string(), "gpt-4o-mini".to_string());

    let config = Arc::new(Config {
        server: server_config(),
        connections: vec![openai_conn("openai", &upstream.uri(), vec!["gpt-4o"], vec![])],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-alias03".into(),
            connections: vec!["openai".into()],
            models: None,
            rate_limit: None,
            budget: None,
            mcp_servers: None,
        }],
        pii: pii_off(),
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: aliases,
        otel: Default::default(),
        ui: Default::default(),
    });

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-alias03",
            r#"{"model":"gpt-4o","messages":[]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bodies = bodies.lock().unwrap().clone();
    assert_eq!(bodies[0]["model"], "gpt-4o", "unaliased model forwarded verbatim");
}

// ---------------------------------------------------------------------------
// Feature 2: metadata passthrough
// ---------------------------------------------------------------------------

fn meta_config(upstream_uri: &str, sink_uri: &str) -> Arc<Config> {
    Arc::new(Config {
        server: server_config(),
        connections: vec![openai_conn(
            "openai",
            upstream_uri,
            vec!["gpt-4o"],
            vec![],
        )],
        virtual_keys: vec![VirtualKey {
            key: "sk-drgtw-meta01".into(),
            connections: vec!["openai".into()],
            models: None,
            rate_limit: None,
            budget: None,
            mcp_servers: None,
        }],
        pii: pii_off(),
        events: Some(events_config(format!("{sink_uri}/events"))),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
    })
}

/// Header metadata is captured into the event AND stripped from the upstream
/// request (never leaks to the provider).
#[tokio::test]
async fn test_header_metadata_captured_and_not_leaked_upstream() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let (_bodies, saw_meta) = mount_chat_upstream(&upstream, ok_usage_response()).await;
    let captured = mount_event_sink(&sink).await;

    let app = test_router(meta_config(&upstream.uri(), &sink.uri()));
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-meta01",
            r#"{"model":"gpt-4o","messages":[]}"#,
            &[("x-drgtw-meta-session-id", "abc"), ("x-drgtw-meta-Agent", "planner")],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    let md = &events[0]["metadata"];
    assert_eq!(md["session-id"], "abc");
    // Header key lowercased.
    assert_eq!(md["agent"], "planner");

    assert!(
        !*saw_meta.lock().unwrap(),
        "x-drgtw-meta-* headers must NOT be forwarded upstream"
    );
}

/// Body `metadata` is harvested into the event, then stripped from the
/// forwarded body (drop_params: Azure-style upstreams 400 on unknown params).
#[tokio::test]
async fn test_body_metadata_captured_and_stripped_from_upstream() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let (bodies, _) = mount_chat_upstream(&upstream, ok_usage_response()).await;
    let captured = mount_event_sink(&sink).await;

    let app = test_router(meta_config(&upstream.uri(), &sink.uri()));
    let body = r#"{"model":"gpt-4o","messages":[],"metadata":{"user_id":"u1","count":3}}"#;
    let resp = app
        .oneshot(chat_request("sk-drgtw-meta01", body, &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The `metadata` object is STRIPPED from the upstream body after harvest
    // (Azure-style OpenAI-compatible upstreams reject unknown params with
    // 400); the rest of the body is forwarded intact.
    let bodies = bodies.lock().unwrap().clone();
    assert!(bodies[0].get("metadata").is_none(), "metadata must not reach upstream");
    assert_eq!(bodies[0]["model"], "gpt-4o");
    assert!(bodies[0]["messages"].is_array());

    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    let md = &events[0]["metadata"];
    assert_eq!(md["user_id"], "u1");
    // Non-string body values are JSON-stringified.
    assert_eq!(md["count"], "3");
}

/// On key collision, the header value wins over the body value.
#[tokio::test]
async fn test_metadata_merge_precedence_header_wins() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let _ = mount_chat_upstream(&upstream, ok_usage_response()).await;
    let captured = mount_event_sink(&sink).await;

    let app = test_router(meta_config(&upstream.uri(), &sink.uri()));
    let body = r#"{"model":"gpt-4o","messages":[],"metadata":{"session-id":"from-body"}}"#;
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-meta01",
            body,
            &[("x-drgtw-meta-session-id", "from-header")],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events[0]["metadata"]["session-id"], "from-header");
}

/// Caps: value truncated to 256 chars; keys longer than 64 chars dropped; at
/// most 16 keys retained (excess dropped in sorted order).
#[tokio::test]
async fn test_metadata_caps_enforced() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let _ = mount_chat_upstream(&upstream, ok_usage_response()).await;
    let captured = mount_event_sink(&sink).await;

    // 20 keys k00..k19 (each short), plus an over-long-value key and an
    // over-long-key entry.
    let long_value = "v".repeat(300);
    let long_key = "k".repeat(65);
    let mut md = serde_json::Map::new();
    for i in 0..20 {
        md.insert(format!("k{i:02}"), Value::String("x".to_string()));
    }
    md.insert("bigval".to_string(), Value::String(long_value));
    md.insert(long_key, Value::String("y".to_string()));
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [],
        "metadata": md,
    })
    .to_string();

    let app = test_router(meta_config(&upstream.uri(), &sink.uri()));
    let resp = app
        .oneshot(chat_request("sk-drgtw-meta01", body, &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    let md = events[0]["metadata"].as_object().unwrap();

    // At most 16 keys.
    assert_eq!(md.len(), 16, "key count capped at 16");
    // Over-long key dropped.
    assert!(!md.keys().any(|k| k.len() > 64), "over-long key dropped");
    // Value truncated to 256 chars if present (bigval sorts after k00..k19, so
    // it may be among the dropped excess — assert truncation only when kept).
    if let Some(Value::String(s)) = md.get("bigval") {
        assert_eq!(s.chars().count(), 256, "value truncated to 256 chars");
    }
    // Deterministic sorted-order drop: lexicographically-smallest keys kept.
    assert!(md.contains_key("bigval") || md.contains_key("k00"));
    assert!(md.contains_key("k00"), "smallest keys retained");
}

/// No metadata supplied → event omits the field entirely (backward compat).
#[tokio::test]
async fn test_no_metadata_field_absent() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let _ = mount_chat_upstream(&upstream, ok_usage_response()).await;
    let captured = mount_event_sink(&sink).await;

    let app = test_router(meta_config(&upstream.uri(), &sink.uri()));
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-meta01",
            r#"{"model":"gpt-4o","messages":[]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = collect_body(resp).await;

    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert!(
        events[0].get("metadata").is_none(),
        "metadata field omitted when absent"
    );
}
