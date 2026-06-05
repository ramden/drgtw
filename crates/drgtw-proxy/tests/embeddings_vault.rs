//! WP 9.3 integration tests: /v1/embeddings endpoint + persistent vault wiring.
//!
//! End-to-end through the full axum router (ProxyState + handlers) with a
//! wiremock upstream and a tempfile-backed vault.
//!
//! Coverage:
//!  1.  embeddings non-stream round-trip (input string), response relayed verbatim.
//!  2.  embeddings input array of strings → each pseudonymized at the mock.
//!  3.  pseudonymized input asserted at mock (storeless → sequential EMAIL_1).
//!  4.  token-id-array input passthrough untouched.
//!  5.  PII off → byte-identical passthrough.
//!  6.  401 (bad key) / 404 (unknown model) paths.
//!  7.  vault stability: two separate requests, same email → SAME placeholder.
//!  8.  restart simulation: rebuild ProxyState with same vault file → same placeholder.
//!  9.  past-request restore (RAG): chat A maps email→vault; chat B response
//!      echoes the captured placeholder (never in B's request) → client B gets the real email.
//! 10.  usage event endpoint="embeddings", input tokens only.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::{
    ApiFormat, Config, Connection, ModelCost, PiiConfig, ServerConfig, VaultConfig, VirtualKey,
};
use drgtw_proxy::{ProxyState, router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt; // `.oneshot()`
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const VKEY: &str = "sk-drgtw-embtest001";
/// 64 hex chars = 32 bytes. Valid vault key.
const VAULT_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn default_server_config() -> ServerConfig {
    ServerConfig {
        bind_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
        ..Default::default()
    }
}

/// OpenAI config; optional vault (path absolute), optional model costs.
fn openai_config(
    mock_base_url: &str,
    pii_enabled: bool,
    vault: Option<VaultConfig>,
    with_costs: bool,
) -> Arc<Config> {
    let mut model_costs = std::collections::HashMap::new();
    if with_costs {
        model_costs.insert(
            "text-embedding-3-small".to_string(),
            ModelCost {
                input_per_1m: 0.02,
                output_per_1m: 0.0,
            },
        );
    }
    Arc::new(Config {
        server: default_server_config(),
        connections: vec![Connection {
            name: "mock-openai".into(),
            base_url: format!("{mock_base_url}/v1"),
            api_key: "upstream-secret".into(),
            format: ApiFormat::OpenAi,
            models: vec!["text-embedding-3-small".into(), "gpt-4o".into()],
            model_costs,
            region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
        }],
        virtual_keys: vec![VirtualKey {
            key: VKEY.into(),
            connections: vec!["mock-openai".into()],
            models: Some(vec!["text-embedding-3-small".into(), "gpt-4o".into()]),
            rate_limit: None,
            budget: None,
        }],
        pii: PiiConfig {
            enabled_by_default: pii_enabled,
            disabled_recognizers: vec![],
            custom_recognizers: vec![],
            ner: None,
            vault,
        },
        events: None,
        fallback: Default::default(),
        mcp_servers: Default::default(),
        tracing: drgtw_config::TracingConfig { enabled: false, ..Default::default() },
        model_aliases: Default::default(),
        otel: Default::default(),
    })
}

fn vault_config(dir: &TempDir, key: &str) -> VaultConfig {
    VaultConfig {
        path: dir.path().join("vault.db").to_string_lossy().into_owned(),
        key: key.to_owned(),
    }
}

fn build_router(config: Arc<Config>) -> axum::Router {
    let state = Arc::new(ProxyState::new(config, Path::new(".")).expect("ProxyState::new failed"));
    router(state)
}

fn embeddings_request(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("Authorization", format!("Bearer {VKEY}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn chat_request(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {VKEY}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn collect_body(resp: axum::response::Response) -> bytes::Bytes {
    use http_body_util::BodyExt;
    resp.into_body().collect().await.unwrap().to_bytes()
}

/// Extract a high-entropy vault placeholder (`PREFIX_<digits>`) from `body`.
///
/// The suffix is all decimal digits: the vault assigns a random, non-sequential
/// integer, and the suffix MUST stay digits-only so the response-restore regex
/// (`\b([A-Z][A-Z0-9_]*_[0-9]+)\b`) can recognise and restore it. Panics if
/// none is found — callers expect exactly one.
fn extract_placeholder(body: &str, prefix: &str) -> String {
    let re = regex::Regex::new(&format!(r"{prefix}_[0-9]+")).unwrap();
    re.find(body)
        .unwrap_or_else(|| panic!("no {prefix}_<digits> placeholder in: {body}"))
        .as_str()
        .to_owned()
}

fn embeddings_response() -> Value {
    json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}],
        "model": "text-embedding-3-small",
        "usage": {"prompt_tokens": 8, "total_tokens": 8}
    })
}

async fn mount_embeddings_ok(mock: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(embeddings_response())
                .insert_header("content-type", "application/json"),
        )
        .mount(mock)
        .await;
}

// ---------------------------------------------------------------------------
// 1. Non-stream round-trip (input string)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_embeddings_string_round_trip() {
    let mock = MockServer::start().await;
    mount_embeddings_ok(&mock).await;

    let cfg = openai_config(&mock.uri(), false, None, false);
    let app = build_router(cfg);

    let body = json!({"model": "text-embedding-3-small", "input": "hello world"});
    let resp = app
        .oneshot(embeddings_request(&body.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let out: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    assert_eq!(out["object"], "list");
    assert_eq!(out["data"][0]["embedding"][0], 0.1);
}

// ---------------------------------------------------------------------------
// 2 + 3. Input array of strings → each pseudonymized (storeless → EMAIL_1) at mock
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_embeddings_array_input_pseudonymized() {
    let mock = MockServer::start().await;
    mount_embeddings_ok(&mock).await;

    // PII on by default, no vault (local counters fine for this assertion).
    let cfg = openai_config(&mock.uri(), true, None, false);
    let app = build_router(cfg);

    let body = json!({
        "model": "text-embedding-3-small",
        "input": ["mail max@example.com", "ping again max@example.com"]
    });
    let resp = app
        .oneshot(embeddings_request(&body.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let received = mock.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let up: Value = serde_json::from_slice(&received[0].body).unwrap();
    let first = up["input"][0].as_str().unwrap();
    let second = up["input"][1].as_str().unwrap();
    // No vault → storeless per-request counter placeholders (EMAIL_1, ...).
    let re = regex::Regex::new(r"EMAIL_[0-9]+").unwrap();
    assert!(
        re.is_match(first),
        "first input must use a placeholder: {first}"
    );
    assert!(
        !first.contains("max@example.com"),
        "raw email must not leak: {first}"
    );
    // Same email in both elements → same placeholder (reuse).
    let placeholder = re.find(first).unwrap().as_str().to_string();
    assert!(
        second.contains(&placeholder),
        "second input must reuse placeholder {placeholder}: {second}"
    );
}

// ---------------------------------------------------------------------------
// 4. Token-id-array input passthrough untouched
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_embeddings_token_array_passthrough() {
    let mock = MockServer::start().await;
    mount_embeddings_ok(&mock).await;

    // PII on — but token-id arrays carry no text to scan, so they must pass
    // through unchanged.
    let cfg = openai_config(&mock.uri(), true, None, false);
    let app = build_router(cfg);

    let body = json!({"model": "text-embedding-3-small", "input": [[1, 2, 3], [4, 5, 6]]});
    let resp = app
        .oneshot(embeddings_request(&body.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let received = mock.received_requests().await.unwrap();
    let up: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        up["input"],
        json!([[1, 2, 3], [4, 5, 6]]),
        "token ids untouched"
    );
}

// ---------------------------------------------------------------------------
// 5. PII off → byte-identical passthrough
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_embeddings_pii_off_byte_identical() {
    let mock = MockServer::start().await;

    let raw_body = r#"{"model":"text-embedding-3-small","input":"email max@example.com"}"#;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(wiremock::matchers::body_string(raw_body))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(embeddings_response())
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&mock)
        .await;

    // PII disabled by default → no header → passthrough.
    let cfg = openai_config(&mock.uri(), false, None, false);
    let app = build_router(cfg);

    let resp = app.oneshot(embeddings_request(raw_body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    mock.verify().await;
}

// ---------------------------------------------------------------------------
// 6. 401 (bad key) and 404 (unknown model)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_embeddings_bad_key_401() {
    let mock = MockServer::start().await;
    let cfg = openai_config(&mock.uri(), false, None, false);
    let app = build_router(cfg);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("Authorization", "Bearer sk-drgtw-nope")
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"model":"text-embedding-3-small","input":"hi"}"#.to_owned(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_embeddings_unknown_model_404() {
    let mock = MockServer::start().await;
    // Use a key with NO model allowlist so the model passes the key gate and
    // routing fails with UnknownModel (404) rather than ModelNotAllowed (403).
    let mut cfg = (*openai_config(&mock.uri(), false, None, false)).clone();
    cfg.virtual_keys[0].models = None;
    let app = build_router(Arc::new(cfg));

    let body = json!({"model": "no-such-model", "input": "hi"});
    let resp = app
        .oneshot(embeddings_request(&body.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// 7. Vault stability: two SEPARATE requests, same email → SAME placeholder
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_vault_stable_placeholder_across_requests() {
    let mock = MockServer::start().await;
    mount_embeddings_ok(&mock).await;

    let dir = TempDir::new().unwrap();
    let cfg = openai_config(
        &mock.uri(),
        true,
        Some(vault_config(&dir, VAULT_KEY)),
        false,
    );
    let app = build_router(cfg);

    let body = json!({"model": "text-embedding-3-small", "input": "to max@example.com"});

    // Request A.
    let resp = app
        .clone()
        .oneshot(embeddings_request(&body.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Request B (separate request, same router → same vault).
    let resp = app
        .oneshot(embeddings_request(&body.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let received = mock.received_requests().await.unwrap();
    assert_eq!(received.len(), 2);
    let a: Value = serde_json::from_slice(&received[0].body).unwrap();
    let b: Value = serde_json::from_slice(&received[1].body).unwrap();
    let pa = a["input"].as_str().unwrap();
    let pb = b["input"].as_str().unwrap();
    assert_eq!(
        pa, pb,
        "vault must yield the same placeholder across requests"
    );
    // High-entropy, digits-only suffix (restore-regex contract); never EMAIL_1.
    let re = regex::Regex::new(r"EMAIL_[0-9]{12,}").unwrap();
    assert!(re.is_match(pa), "random digit placeholder present: {pa}");
    assert!(
        !pa.contains("EMAIL_1 "),
        "must not be sequential token: {pa}"
    );
    assert!(
        !pa.contains("max@example.com"),
        "raw email must not leak: {pa}"
    );
}

// ---------------------------------------------------------------------------
// 8. Restart simulation: rebuild ProxyState with same vault file → same placeholder
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_vault_stable_across_restart() {
    let mock = MockServer::start().await;
    mount_embeddings_ok(&mock).await;

    let dir = TempDir::new().unwrap();
    let body = json!({"model": "text-embedding-3-small", "input": "ring max@example.com"});

    // First "process": fresh router over the vault file.
    {
        let cfg = openai_config(
            &mock.uri(),
            true,
            Some(vault_config(&dir, VAULT_KEY)),
            false,
        );
        let app = build_router(cfg);
        let resp = app
            .oneshot(embeddings_request(&body.to_string()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    // Second "process": brand-new ProxyState over the SAME vault file.
    {
        let cfg = openai_config(
            &mock.uri(),
            true,
            Some(vault_config(&dir, VAULT_KEY)),
            false,
        );
        let app = build_router(cfg);
        let resp = app
            .oneshot(embeddings_request(&body.to_string()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let received = mock.received_requests().await.unwrap();
    assert_eq!(received.len(), 2);
    let a: Value = serde_json::from_slice(&received[0].body).unwrap();
    let b: Value = serde_json::from_slice(&received[1].body).unwrap();
    assert_eq!(
        a["input"].as_str().unwrap(),
        b["input"].as_str().unwrap(),
        "placeholder must survive a process restart (same vault file)"
    );
}

// ---------------------------------------------------------------------------
// 9. Past-request restore (RAG): chat A maps email; chat B response carries
//    the captured placeholder (never sent in B's request) → client B gets the real email.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_past_request_restore_via_vault() {
    let mock = MockServer::start().await;

    // Chat A: response echoes nothing special; we just need A to populate the
    // vault with max@example.com → its hash placeholder.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "id": "chatcmpl-a",
                    "object": "chat.completion",
                    "choices": [{"message": {"role": "assistant", "content": "ok"}}]
                }))
                .insert_header("content-type", "application/json"),
        )
        .up_to_n_times(1)
        .mount(&mock)
        .await;

    let dir = TempDir::new().unwrap();
    let cfg = openai_config(
        &mock.uri(),
        true,
        Some(vault_config(&dir, VAULT_KEY)),
        false,
    );
    let app = build_router(cfg);

    // Request A: contains the email so the vault stores max@example.com → its
    // hash placeholder.
    let body_a = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "write to max@example.com"}]
    });
    let resp = app
        .clone()
        .oneshot(chat_request(&body_a.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Capture the placeholder the gateway actually sent upstream in request A;
    // the hash cannot be hardcoded.
    let received = mock.received_requests().await.unwrap();
    let up_a = String::from_utf8(received[0].body.clone()).unwrap();
    assert!(
        !up_a.contains("max@example.com"),
        "raw email must not leak upstream: {up_a}"
    );
    let placeholder = extract_placeholder(&up_a, "EMAIL");

    // Now mock chat B: its RESPONSE contains that captured placeholder even
    // though B's request never mentioned the email — simulating a RAG lookup
    // that returned stored pseudonymized text.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "id": "chatcmpl-b",
                    "object": "chat.completion",
                    "choices": [{"message": {"role": "assistant", "content": format!("found {placeholder} in records")}}]
                }))
                .insert_header("content-type", "application/json"),
        )
        .mount(&mock)
        .await;

    // Request B: no PII in the request body → current map is empty, BUT the
    // vault restore second-pass must turn the placeholder in the response back
    // into the real email.
    let body_b = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "any records?"}]
    });
    let resp = app
        .oneshot(chat_request(&body_b.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let out: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    let content = out["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        content.contains("max@example.com"),
        "vault past-request restore must yield the real email, got: {content}"
    );
    assert!(
        !content.contains(&placeholder),
        "placeholder must not leak to client, got: {content}"
    );
}

// ---------------------------------------------------------------------------
// 9b. SECURITY: a GUESSED sequential token in a response must NOT exfiltrate
//     another request's value. With the old `PERSON_1`/`EMAIL_1` counter,
//     a model emitting `EMAIL_1` (hallucinated or prompt-injected) would have
//     a real value spliced into the caller's response. The random,
//     non-sequential, ~63-bit suffix makes such tokens unguessable, so the
//     low-numbered guess resolves to nothing and passes through untouched.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_guessed_sequential_token_does_not_exfiltrate() {
    let mock = MockServer::start().await;

    let dir = TempDir::new().unwrap();
    let cfg = openai_config(
        &mock.uri(),
        true,
        Some(vault_config(&dir, VAULT_KEY)),
        false,
    );
    let app = build_router(cfg);

    // Request A populates the vault with a real, sensitive email.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "id": "chatcmpl-a",
                    "object": "chat.completion",
                    "choices": [{"message": {"role": "assistant", "content": "ok"}}]
                }))
                .insert_header("content-type", "application/json"),
        )
        .up_to_n_times(1)
        .mount(&mock)
        .await;

    let body_a = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "mail victim-secret@example.com"}]
    });
    let resp = app
        .clone()
        .oneshot(chat_request(&body_a.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Confirm the vault assigned a RANDOM (non-sequential) token to A's email.
    let received = mock.received_requests().await.unwrap();
    let up_a = String::from_utf8(received[0].body.clone()).unwrap();
    let real_token = extract_placeholder(&up_a, "EMAIL");
    assert_ne!(
        real_token, "EMAIL_1",
        "token must not be the guessable sequential value"
    );

    // Attacker's request B: the response carries GUESSED low-numbered tokens
    // that an attacker would try (EMAIL_1..EMAIL_5). None of these is the real
    // random token, so none must resolve to A's secret email.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "id": "chatcmpl-b",
                    "object": "chat.completion",
                    "choices": [{"message": {"role": "assistant",
                        "content": "leak attempt EMAIL_1 EMAIL_2 EMAIL_3 EMAIL_4 EMAIL_5"}}]
                }))
                .insert_header("content-type", "application/json"),
        )
        .mount(&mock)
        .await;

    let body_b = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "any records?"}]
    });
    let resp = app
        .oneshot(chat_request(&body_b.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let out: Value = serde_json::from_slice(&collect_body(resp).await).unwrap();
    let content = out["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        !content.contains("victim-secret@example.com"),
        "guessed sequential tokens must NOT exfiltrate another request's value, got: {content}"
    );
}

// ---------------------------------------------------------------------------
// Boot failures: vault open errors must fail ProxyState construction.
// ---------------------------------------------------------------------------

/// Wrong (but valid-hex) key against an existing vault → BadKey at boot.
#[tokio::test]
async fn test_boot_fails_on_wrong_vault_key() {
    let dir = TempDir::new().unwrap();
    // Create the vault with the correct key by booting once.
    {
        let cfg = openai_config(
            "http://127.0.0.1:1",
            true,
            Some(vault_config(&dir, VAULT_KEY)),
            false,
        );
        ProxyState::new(cfg, Path::new(".")).expect("first boot creates vault");
    }
    // Boot again with a DIFFERENT valid-hex key → BadKey.
    let wrong_key = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let cfg = openai_config(
        "http://127.0.0.1:1",
        true,
        Some(vault_config(&dir, wrong_key)),
        false,
    );
    let err = ProxyState::new(cfg, Path::new(".")).expect_err("wrong key must fail boot");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("vault"),
        "error mentions vault: {msg}"
    );
}

/// A vault path whose parent directory does not exist → clear boot error.
#[tokio::test]
async fn test_boot_fails_on_missing_vault_parent_dir() {
    let dir = TempDir::new().unwrap();
    let bogus = dir.path().join("does-not-exist").join("vault.db");
    let vault = VaultConfig {
        path: bogus.to_string_lossy().into_owned(),
        key: VAULT_KEY.to_owned(),
    };
    let cfg = openai_config("http://127.0.0.1:1", true, Some(vault), false);
    let err = ProxyState::new(cfg, Path::new(".")).expect_err("missing parent dir must fail boot");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("vault"),
        "error mentions vault: {msg}"
    );
}

/// A key that is not valid hex → clear boot error (no key material leaked).
#[tokio::test]
async fn test_boot_fails_on_non_hex_key() {
    let dir = TempDir::new().unwrap();
    // 64 chars but contains non-hex characters.
    let bad = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
    let cfg = openai_config(
        "http://127.0.0.1:1",
        true,
        Some(vault_config(&dir, bad)),
        false,
    );
    let err = ProxyState::new(cfg, Path::new(".")).expect_err("non-hex key must fail boot");
    let msg = err.to_string();
    assert!(msg.contains("hex"), "error mentions hex: {msg}");
}

// ---------------------------------------------------------------------------
// 10. Usage cost: input tokens only (no output price applied)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_embeddings_input_only_cost_does_not_error() {
    // We can't easily read the event sink here (events=None), but we can verify
    // the request succeeds with model_costs present (input-only pricing path
    // is exercised). A panic/error in cost computation would surface as != 200.
    let mock = MockServer::start().await;
    mount_embeddings_ok(&mock).await;

    let cfg = openai_config(&mock.uri(), false, None, true);
    let app = build_router(cfg);

    let body = json!({"model": "text-embedding-3-small", "input": "cost path"});
    let resp = app
        .oneshot(embeddings_request(&body.to_string()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
