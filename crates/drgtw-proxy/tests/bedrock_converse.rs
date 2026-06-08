//! Integration tests for AWS Bedrock Converse / ConverseStream support (0.0.3).
//!
//! A `bedrock_converse`-format connection serves the **`/v1/chat/completions`
//! endpoint surface** (callers use the OpenAI body) and dispatches to Bedrock's
//! normalised Converse / ConverseStream APIs:
//!
//! * The gateway translates the OpenAI request body into a Converse body
//!   (`messages[].content[].text`, `system[]`, `inferenceConfig`), lifts the
//!   model id into the URL path (`POST /model/{id}/converse[-stream]`), and
//!   either SigV4-signs the request (when AWS creds are present) or sends the
//!   Bedrock API key as `Authorization: Bearer`.
//! * Non-streaming Converse JSON is translated back into an OpenAI
//!   `chat.completion`, so usage extraction + cost + PII restore run unchanged.
//! * ConverseStream returns a binary `application/vnd.amazon.eventstream` body;
//!   the gateway re-frames it into OpenAI SSE chunks, so `stream: true` now
//!   WORKS on a Bedrock connection (the 0.0.2 limitation is lifted).
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
// Config helpers
// ---------------------------------------------------------------------------

fn server_config() -> ServerConfig {
    ServerConfig {
        bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
        ..Default::default()
    }
}

fn pii_off() -> PiiConfig {
    PiiConfig { enabled_by_default: false, ..Default::default() }
}

fn pii_on() -> PiiConfig {
    PiiConfig { enabled_by_default: true, ..Default::default() }
}

fn events_config(sink_url: String) -> EventsConfig {
    EventsConfig {
        url: sink_url,
        auth_bearer: None,
        buffer_size: 64,
        timeout_ms: 5_000,
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

/// Build a `bedrock_converse` connection. `creds` is `Some((akid, secret,
/// session))` for SigV4, or `None` for bearer-only (`api_key`).
fn converse_connection(
    name: &str,
    base_url: String,
    models: Vec<&str>,
    costs: Vec<(&str, f64, f64)>,
    creds: Option<(&str, &str, Option<&str>)>,
) -> Connection {
    let (akid, secret, session) = match creds {
        Some((a, s, t)) => (Some(a.to_owned()), Some(s.to_owned()), t.map(str::to_owned)),
        None => (None, None, None),
    };
    Connection {
        name: name.into(),
        base_url,
        api_key: if creds.is_some() { String::new() } else { "bedrock-bearer-token".into() },
        format: ApiFormat::BedrockConverse,
        models: models.into_iter().map(str::to_owned).collect(),
        model_costs: model_costs(costs),
        region: Some("eu-central-1".into()),
        aws_access_key_id: akid,
        aws_secret_access_key: secret,
        aws_session_token: session,
    }
}

fn base_config(connection: Connection, virtual_key: &str, sink: &MockServer, pii: PiiConfig) -> Config {
    Config {
        server: server_config(),
        connections: vec![connection],
        virtual_keys: vec![VirtualKey {
            key: virtual_key.into(),
            connections: vec!["bedrock-converse".into()],
            models: None,
            rate_limit: None,
            budget: None,
        }],
        pii,
        events: Some(events_config(format!("{}/events", sink.uri()))),
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
        ui: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// Upstream capture (body + Authorization + a few AWS headers)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CapturedReq {
    body: Value,
    authorization: Option<String>,
    amz_date: Option<String>,
    amz_security_token: Option<String>,
}

#[derive(Clone)]
struct ConverseCapture {
    reqs: Arc<Mutex<Vec<CapturedReq>>>,
    response: Value,
}

impl Respond for ConverseCapture {
    fn respond(&self, req: &WmRequest) -> ResponseTemplate {
        let body = serde_json::from_slice::<Value>(&req.body).unwrap_or(Value::Null);
        let hdr = |name: &str| {
            req.headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_owned())
        };
        self.reqs.lock().unwrap().push(CapturedReq {
            body,
            authorization: hdr("authorization"),
            amz_date: hdr("x-amz-date"),
            amz_security_token: hdr("x-amz-security-token"),
        });
        ResponseTemplate::new(200)
            .set_body_json(self.response.clone())
            .insert_header("content-type", "application/json")
    }
}

async fn mount_converse(
    server: &MockServer,
    mount_path: &str,
    response: Value,
) -> Arc<Mutex<Vec<CapturedReq>>> {
    let reqs = Arc::new(Mutex::new(Vec::new()));
    let cap = ConverseCapture { reqs: Arc::clone(&reqs), response };
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

/// A Converse-shaped non-streaming response. 11 in / 7 out.
fn converse_response(text: &str, stop_reason: &str) -> Value {
    serde_json::json!({
        "output": { "message": { "role": "assistant", "content": [{ "text": text }] }},
        "stopReason": stop_reason,
        "usage": { "inputTokens": 11, "outputTokens": 7, "totalTokens": 18 }
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

async fn collect_body(resp: axum::response::Response) -> bytes::Bytes {
    use http_body_util::BodyExt;
    resp.into_body().collect().await.unwrap().to_bytes()
}

// ---------------------------------------------------------------------------
// Binary eventstream fixture builder (mirrors the eventstream.rs unit helper)
// ---------------------------------------------------------------------------

/// Encode one string header: `u8 name_len`, name, `u8 type=7`, `u16 BE len`, value.
fn string_header(name: &str, value: &str) -> Vec<u8> {
    let mut h = Vec::new();
    h.push(name.len() as u8);
    h.extend_from_slice(name.as_bytes());
    h.push(7u8); // string type
    h.extend_from_slice(&(value.len() as u16).to_be_bytes());
    h.extend_from_slice(value.as_bytes());
    h
}

/// Assemble a complete eventstream message from a headers block + payload,
/// computing both CRC32s (big-endian on the wire) the way the decoder validates.
fn frame(headers: &[u8], payload: &[u8]) -> Vec<u8> {
    let headers_len = headers.len() as u32;
    // total = prelude(12) + headers + payload + trailing CRC(4)
    let total_length = 12 + headers_len + payload.len() as u32 + 4;

    let mut msg = Vec::new();
    msg.extend_from_slice(&total_length.to_be_bytes());
    msg.extend_from_slice(&headers_len.to_be_bytes());
    let prelude_crc = crc32fast::hash(&msg[0..8]);
    msg.extend_from_slice(&prelude_crc.to_be_bytes());
    msg.extend_from_slice(headers);
    msg.extend_from_slice(payload);
    let message_crc = crc32fast::hash(&msg);
    msg.extend_from_slice(&message_crc.to_be_bytes());
    msg
}

/// A standard Converse event frame (`:message-type=event`, `:content-type=json`).
fn event_frame(event_type: &str, payload: Value) -> Vec<u8> {
    let mut headers = Vec::new();
    headers.extend_from_slice(&string_header(":event-type", event_type));
    headers.extend_from_slice(&string_header(":content-type", "application/json"));
    headers.extend_from_slice(&string_header(":message-type", "event"));
    let payload = serde_json::to_vec(&payload).unwrap();
    frame(&headers, &payload)
}

/// Build the full ConverseStream binary body for a single response.
/// `deltas` are emitted as separate `contentBlockDelta` text frames.
fn converse_stream_body(deltas: &[&str], stop_reason: &str, input: u64, output: u64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(event_frame("messageStart", serde_json::json!({ "role": "assistant" })));
    for (i, d) in deltas.iter().enumerate() {
        out.extend(event_frame(
            "contentBlockDelta",
            serde_json::json!({ "contentBlockIndex": i, "delta": { "text": d } }),
        ));
    }
    out.extend(event_frame("contentBlockStop", serde_json::json!({ "contentBlockIndex": 0 })));
    out.extend(event_frame("messageStop", serde_json::json!({ "stopReason": stop_reason })));
    out.extend(event_frame(
        "metadata",
        serde_json::json!({ "usage": { "inputTokens": input, "outputTokens": output, "totalTokens": input + output } }),
    ));
    out
}

/// Join all `choices[0].delta.content` strings across an OpenAI SSE byte stream.
fn join_sse_content(bytes: &[u8]) -> String {
    let text = std::str::from_utf8(bytes).unwrap();
    let mut joined = String::new();
    for line in text.lines() {
        let Some(payload) = line.strip_prefix("data: ") else { continue };
        if payload.trim() == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(payload) else { continue };
        if let Some(c) = v["choices"][0]["delta"]["content"].as_str() {
            joined.push_str(c);
        }
    }
    joined
}

// ===========================================================================
// Non-streaming: Converse round-trip + cost + usage event
// ===========================================================================

/// A `bedrock_converse` connection translates an OpenAI request into a Converse
/// body, translates the Converse response back to OpenAI, and bills the usage.
/// `stopReason: max_tokens` maps to `finish_reason: length`.
#[tokio::test]
async fn converse_non_streaming_round_trip() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let reqs = mount_converse(
        &upstream,
        "/model/eu.amazon.nova-pro-v1%3A0/converse",
        converse_response("Hello from Nova", "max_tokens"),
    )
    .await;
    let captured = mount_event_sink(&sink).await;

    let conn = converse_connection(
        "bedrock-converse",
        upstream.uri(),
        vec!["eu.amazon.nova-pro-v1:0"],
        vec![("eu.amazon.nova-pro-v1:0", 0.8, 3.2)],
        None, // bearer
    );
    let config = Arc::new(base_config(conn, "sk-drgtw-converse01", &sink, pii_off()));

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-converse01",
            r#"{"model":"eu.amazon.nova-pro-v1:0","messages":[{"role":"system","content":"Be brief."},{"role":"user","content":"hi"}],"max_tokens":64}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Upstream saw a translated Converse body (system lifted, text blocks).
    let reqs = reqs.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].body["system"][0]["text"], "Be brief.");
    assert_eq!(reqs[0].body["messages"][0]["role"], "user");
    assert_eq!(reqs[0].body["messages"][0]["content"][0]["text"], "hi");
    assert_eq!(reqs[0].body["inferenceConfig"]["maxTokens"], 64);
    // No top-level `model` leaks into the Converse body.
    assert!(reqs[0].body.get("model").is_none());

    // Client got an OpenAI-shaped response with mapped finish_reason + usage.
    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "Hello from Nova");
    assert_eq!(body["choices"][0]["finish_reason"], "length");
    assert_eq!(body["usage"]["prompt_tokens"], 11);
    assert_eq!(body["usage"]["completion_tokens"], 7);

    // Usage event billed: 11/1e6*0.8 + 7/1e6*3.2.
    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["input_tokens"], 11);
    assert_eq!(events[0]["output_tokens"], 7);
    let cost = events[0]["cost_usd"].as_f64().expect("cost present");
    let expected = 11.0 / 1e6 * 0.8 + 7.0 / 1e6 * 3.2;
    assert!((cost - expected).abs() < 1e-12, "cost was {cost}, expected {expected}");
}

// ===========================================================================
// Auth: SigV4 signing
// ===========================================================================

/// A connection with SigV4 creds (incl. session token) signs the upstream
/// request: `Authorization` starts `AWS4-HMAC-SHA256`, `x-amz-date` and
/// `x-amz-security-token` are present.
#[tokio::test]
async fn converse_sigv4_signs_request() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let reqs = mount_converse(
        &upstream,
        "/model/eu.amazon.nova-pro-v1%3A0/converse",
        converse_response("ok", "end_turn"),
    )
    .await;
    let _captured = mount_event_sink(&sink).await;

    let conn = converse_connection(
        "bedrock-converse",
        upstream.uri(),
        vec!["eu.amazon.nova-pro-v1:0"],
        vec![],
        Some(("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", Some("FwoSESSIONTOKEN"))),
    );
    let config = Arc::new(base_config(conn, "sk-drgtw-converse02", &sink, pii_off()));

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-converse02",
            r#"{"model":"eu.amazon.nova-pro-v1:0","messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let reqs = reqs.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    let auth = reqs[0].authorization.as_deref().expect("authorization present");
    assert!(auth.starts_with("AWS4-HMAC-SHA256"), "auth was: {auth}");
    assert!(auth.contains("/bedrock/aws4_request"), "credential scope: {auth}");
    assert!(reqs[0].amz_date.is_some(), "x-amz-date present");
    assert_eq!(
        reqs[0].amz_security_token.as_deref(),
        Some("FwoSESSIONTOKEN"),
        "session token header present"
    );
}

/// A bearer-only `bedrock_converse` connection sends `Authorization: Bearer`
/// (NOT AWS4) and no `x-amz-date`.
#[tokio::test]
async fn converse_bearer_fallback() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let reqs = mount_converse(
        &upstream,
        "/model/eu.amazon.nova-pro-v1%3A0/converse",
        converse_response("ok", "end_turn"),
    )
    .await;
    let _captured = mount_event_sink(&sink).await;

    let conn = converse_connection(
        "bedrock-converse",
        upstream.uri(),
        vec!["eu.amazon.nova-pro-v1:0"],
        vec![],
        None, // bearer
    );
    let config = Arc::new(base_config(conn, "sk-drgtw-converse03", &sink, pii_off()));

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-converse03",
            r#"{"model":"eu.amazon.nova-pro-v1:0","messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let reqs = reqs.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].authorization.as_deref(), Some("Bearer bedrock-bearer-token"));
    assert!(reqs[0].amz_date.is_none(), "no x-amz-date on bearer requests");
}

// ===========================================================================
// Streaming: ConverseStream binary eventstream -> OpenAI SSE
// ===========================================================================

/// `stream: true` on a `bedrock_converse` connection now WORKS: the upstream
/// returns a binary eventstream, the gateway re-frames it into OpenAI SSE
/// chunks (text reassembled, `[DONE]` terminus), and the usage event carries
/// the metadata token counts. This is the failure that motivated 0.0.3.
#[tokio::test]
async fn converse_streaming_reframes_to_openai_sse() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let captured = mount_event_sink(&sink).await;

    let stream_body = converse_stream_body(&["Hello, ", "world!"], "end_turn", 9, 3);

    Mock::given(method("POST"))
        .and(path("/model/eu.amazon.nova-pro-v1%3A0/converse-stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(stream_body, "application/vnd.amazon.eventstream")
                .insert_header("content-type", "application/vnd.amazon.eventstream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let conn = converse_connection(
        "bedrock-converse",
        upstream.uri(),
        vec!["eu.amazon.nova-pro-v1:0"],
        vec![("eu.amazon.nova-pro-v1:0", 0.8, 3.2)],
        None,
    );
    let config = Arc::new(base_config(conn, "sk-drgtw-converse04", &sink, pii_off()));

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-converse04",
            r#"{"model":"eu.amazon.nova-pro-v1:0","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Client sees text/event-stream, never the upstream eventstream type.
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.contains("text/event-stream"), "content-type was: {ct}");

    let body_bytes = collect_body(resp).await;
    assert_eq!(join_sse_content(&body_bytes), "Hello, world!");
    assert!(body_bytes.ends_with(b"data: [DONE]\n\n"), "stream ends with [DONE]");

    // Usage event carries the metadata token counts.
    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["input_tokens"], 9);
    assert_eq!(events[0]["output_tokens"], 3);
    assert_eq!(events[0]["streamed"], true);
    let cost = events[0]["cost_usd"].as_f64().expect("cost present");
    let expected = 9.0 / 1e6 * 0.8 + 3.0 / 1e6 * 3.2;
    assert!((cost - expected).abs() < 1e-12, "cost was {cost}");
}

// ===========================================================================
// PII: pseudonymize through Converse, restore on the way back
// ===========================================================================

/// PII enabled: the email in the OpenAI request is pseudonymized BEFORE the
/// Converse translation, so the upstream Converse body carries the placeholder
/// (never the raw email). The Converse response echoes the placeholder, and the
/// client response has the email restored.
#[tokio::test]
async fn converse_pii_pseudonymize_and_restore() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let _captured = mount_event_sink(&sink).await;

    // The mock echoes a placeholder back inside a Converse response. We must
    // learn the placeholder the gateway assigns; instead of guessing, the mock
    // reflects the FIRST message text it received back as the output text, so
    // whatever placeholder the gateway sent upstream is what comes back, and the
    // restore maps it to the original email.
    #[derive(Clone)]
    struct EchoConverse(Arc<Mutex<Vec<CapturedReq>>>);
    impl Respond for EchoConverse {
        fn respond(&self, req: &WmRequest) -> ResponseTemplate {
            let body = serde_json::from_slice::<Value>(&req.body).unwrap_or(Value::Null);
            let echoed = body["messages"][0]["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            self.0.lock().unwrap().push(CapturedReq {
                body: body.clone(),
                authorization: None,
                amz_date: None,
                amz_security_token: None,
            });
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "output": { "message": { "role": "assistant", "content": [{ "text": echoed }] }},
                    "stopReason": "end_turn",
                    "usage": { "inputTokens": 5, "outputTokens": 5, "totalTokens": 10 }
                }))
                .insert_header("content-type", "application/json")
        }
    }

    let reqs = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/model/eu.amazon.nova-pro-v1%3A0/converse"))
        .respond_with(EchoConverse(Arc::clone(&reqs)))
        .mount(&upstream)
        .await;

    let conn = converse_connection(
        "bedrock-converse",
        upstream.uri(),
        vec!["eu.amazon.nova-pro-v1:0"],
        vec![],
        None,
    );
    let config = Arc::new(base_config(conn, "sk-drgtw-converse05", &sink, pii_on()));

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-converse05",
            r#"{"model":"eu.amazon.nova-pro-v1:0","messages":[{"role":"user","content":"Email me at max.mustermann@example.com please"}]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Upstream Converse body must NOT contain the raw email.
    let reqs = reqs.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    let upstream_text = reqs[0].body["messages"][0]["content"][0]["text"].as_str().unwrap();
    assert!(
        !upstream_text.contains("max.mustermann@example.com"),
        "raw email leaked upstream: {upstream_text}"
    );

    // Client response has the email restored.
    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    let restored = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        restored.contains("max.mustermann@example.com"),
        "email not restored in client response: {restored}"
    );
}

// ===========================================================================
// Alias + Converse combined routing
// ===========================================================================

/// A model alias resolves to a Bedrock id served by a `bedrock_converse`
/// connection: the alias is rewritten, the resolved id lands in the Converse
/// URL path, and the round-trip succeeds.
#[tokio::test]
async fn converse_alias_routing_sanity() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let reqs = mount_converse(
        &upstream,
        "/model/eu.amazon.nova-pro-v1%3A0/converse",
        converse_response("aliased ok", "end_turn"),
    )
    .await;
    let captured = mount_event_sink(&sink).await;

    let conn = converse_connection(
        "bedrock-converse",
        upstream.uri(),
        vec!["eu.amazon.nova-pro-v1:0"],
        vec![("eu.amazon.nova-pro-v1:0", 0.8, 3.2)],
        None,
    );
    let mut config = base_config(conn, "sk-drgtw-converse06", &sink, pii_off());
    config
        .model_aliases
        .insert("nova".into(), "eu.amazon.nova-pro-v1:0".into());
    let config = Arc::new(config);

    let app = test_router(config);
    let resp = app
        .oneshot(chat_request(
            "sk-drgtw-converse06",
            r#"{"model":"nova","messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // The mock at the RESOLVED id path was hit (alias rewrite reached routing).
    assert_eq!(reqs.lock().unwrap().len(), 1);

    let body: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "aliased ok");
    // Usage event reports the RESOLVED model id.
    wait_for_events(&captured, 1).await;
    let events = captured.lock().unwrap().clone();
    assert_eq!(events[0]["model"], "eu.amazon.nova-pro-v1:0");
}

// ---------------------------------------------------------------------------
// Incomplete upstream stream still terminates the client SSE with [DONE]
// ---------------------------------------------------------------------------

/// Upstream disconnects mid-stream (no `metadata` terminal event): the
/// gateway synthesizes the trailing `[DONE]` so the client SSE stream is
/// always complete.
#[tokio::test]
async fn converse_truncated_stream_still_emits_done() {
    let upstream = MockServer::start().await;
    let sink = MockServer::start().await;
    let _captured = mount_event_sink(&sink).await;

    // Only messageStart + one delta — NO messageStop/metadata frames.
    let mut stream_body = Vec::new();
    stream_body.extend(event_frame(
        "messageStart",
        serde_json::json!({"role": "assistant"}),
    ));
    stream_body.extend(event_frame(
        "contentBlockDelta",
        serde_json::json!({"contentBlockIndex": 0, "delta": {"text": "partial"}}),
    ));

    Mock::given(method("POST"))
        .and(path("/model/eu.amazon.nova-pro-v1%3A0/converse-stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(stream_body, "application/vnd.amazon.eventstream")
                .insert_header("content-type", "application/vnd.amazon.eventstream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let conn = converse_connection(
        "bedrock-converse",
        upstream.uri(),
        vec!["eu.amazon.nova-pro-v1:0"],
        vec![],
        None,
    );
    let config = Arc::new(base_config(conn, "sk-drgtw-converse07", &sink, pii_off()));

    let resp = test_router(config)
        .oneshot(chat_request(
            "sk-drgtw-converse07",
            r#"{"model":"eu.amazon.nova-pro-v1:0","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = collect_body(resp).await;
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("partial"), "delta content must reach client: {text}");
    assert!(
        text.trim_end().ends_with("data: [DONE]"),
        "stream must terminate with [DONE]: {text}"
    );
}
