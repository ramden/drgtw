//! TRUE end-to-end test: proxy → Postgres → UI.
//!
//! Proves the full WP-D path against a LIVE Postgres history store:
//!   1. A real `History` is connected and shared (via `PgGate::Connected`) into
//!      BOTH the proxy state (fire-and-forget usage recording) and the UI state
//!      (history queries) — exactly as `server::run()` wires it at boot.
//!   2. A chat-completion is POSTed through `/v1/chat/completions` with a
//!      virtual key; the upstream is a wiremock mock returning usage tokens.
//!   3. We poll `recent_usage` until the recorded row appears (recording is
//!      `tokio::spawn`-detached, so it is eventually-consistent).
//!   4. We GET `/ui/traces` and assert the recorded request surfaces in the
//!      rendered HTML — proving proxy → PG → UI end to end.
//!
//! GATED on `DATABASE_URL`: skips cleanly (passes, prints a notice) when unset
//! or when Postgres is unreachable, mirroring the WP-C tests in `ui.rs`.
//!
//! A unique model marker (nanos suffix) isolates each run's assertions from
//! any other rows already in the shared database.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::load;
use drgtw_history::History;
use drgtw_ui::PgGate;
use tower::ServiceExt; // for `.oneshot()`
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// --------------------------------------------------------------------------- helpers

fn nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
}

/// Connect a live `History`, or return `None` (and print a skip notice) when
/// `DATABASE_URL` is unset / Postgres unreachable.
async fn connect_or_skip() -> Option<History> {
    let url = match std::env::var("DATABASE_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("DATABASE_URL not set — skipping e2e_postgres test");
            return None;
        }
    };
    match History::connect(&url).await {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("Could not connect to Postgres ({e}) — skipping e2e_postgres test");
            None
        }
    }
}

/// Write a temp config TOML and load it. `history_url` is `Some` to enable the
/// Postgres-backed history store, `None` for the disabled case.
fn load_config(mock_base_url: &str, virtual_key: &str, model: &str, history_url: Option<&str>) -> Arc<drgtw_config::Config> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);

    let history_block = match history_url {
        Some(url) => format!("[ui.history]\npostgres_url = \"{url}\"\n"),
        None => String::new(),
    };

    let toml_content = format!(
        "
[server]
bind_addr = \"127.0.0.1:0\"

[[connections]]
name = \"mock-upstream\"
base_url = \"{mock_base_url}/v1\"
api_key = \"upstream-secret\"
format = \"open_ai\"
models = [\"{model}\"]

[[virtual_keys]]
key = \"{virtual_key}\"
connections = [\"mock-upstream\"]
models = [\"{model}\"]

[ui]
enabled = true
{history_block}
"
    );

    let path = std::env::temp_dir().join(format!("drgtw-e2e-pg-{n}-{}.toml", nanos()));
    let mut f = std::fs::File::create(&path).expect("create temp config");
    f.write_all(toml_content.as_bytes()).expect("write temp config");
    Arc::new(load(&path).expect("load temp config"))
}

/// Mount a wiremock chat-completion mock that echoes usage tokens for `model`.
async fn mount_chat_mock(mock_server: &MockServer, model: &str) {
    let body = serde_json::json!({
        "id": "chatcmpl-e2e-pg",
        "object": "chat.completion",
        "model": model,
        "choices": [{"message": {"role": "assistant", "content": "pong"}, "finish_reason": "stop", "index": 0}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18},
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&body)
                .insert_header("content-type", "application/json"),
        )
        .mount(mock_server)
        .await;
}

fn chat_request(virtual_key: &str, json_body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {virtual_key}"))
        .header("Content-Type", "application/json")
        .body(Body::from(json_body.to_owned()))
        .unwrap()
}

async fn body_to_string(resp: axum::response::Response) -> String {
    use http_body_util::BodyExt;
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

// --------------------------------------------------------------------------- the test

/// Full proxy → Postgres → UI round trip against a live database.
#[tokio::test]
async fn proxy_records_to_postgres_and_ui_renders_it() {
    let Some(history) = connect_or_skip().await else { return };

    // Unique markers isolate this run from any pre-existing rows.
    let suffix = nanos();
    let model = format!("e2e-pg-model-{suffix}");
    let virtual_key = "sk-drgtw-e2epg01";

    let mock_server = MockServer::start().await;
    mount_chat_mock(&mock_server, &model).await;

    let db_url = std::env::var("DATABASE_URL").unwrap();
    let cfg = load_config(&mock_server.uri(), virtual_key, &model, Some(&db_url));

    // Share ONE history handle into both proxy (recording) and UI (querying),
    // exactly as server::run() does. `gate.history()` hands the same Arc to
    // ProxyState via `router_with_gate`; we keep our own clone for polling.
    let history = Arc::new(history);
    let gate = PgGate::Connected(Arc::clone(&history));
    let app = drgtw::server::router_with_gate(
        Arc::clone(&cfg),
        std::path::Path::new("."),
        std::path::PathBuf::new(),
        gate,
    )
    .expect("router build failed");

    // 1) POST a chat completion through the proxy.
    let req = chat_request(
        virtual_key,
        &format!(r#"{{"model":"{model}","messages":[{{"role":"user","content":"ping"}}]}}"#),
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "chat completion must succeed");

    // 2) Recording is fire-and-forget (tokio::spawn). Poll until our row lands.
    let deadline = Instant::now() + Duration::from_secs(5);
    let recorded = loop {
        let rows = history.recent_usage(50).await.expect("recent_usage");
        if let Some(ev) = rows.into_iter().find(|e| e.model == model) {
            break Some(ev);
        }
        if Instant::now() >= deadline {
            break None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let ev = recorded.expect("usage row with unique model marker must appear in Postgres within 5s");
    assert_eq!(ev.model, model, "recorded model marker");
    assert_eq!(ev.status, 200, "recorded status should be the 200 the caller saw");
    assert_eq!(ev.input_tokens, Some(11), "recorded prompt tokens");
    assert_eq!(ev.output_tokens, Some(7), "recorded completion tokens");

    // 3) GET /ui/traces and assert the unique marker is in the rendered HTML —
    //    proves the proxy → PG → UI path end to end.
    let traces_req = Request::builder()
        .method("GET")
        .uri("/ui/traces")
        .body(Body::empty())
        .unwrap();
    let traces_resp = app.oneshot(traces_req).await.unwrap();
    assert_eq!(traces_resp.status(), StatusCode::OK);
    let html = body_to_string(traces_resp).await;
    assert!(
        html.contains(&model),
        "the /ui/traces page must render the unique model marker recorded via the proxy"
    );
}

/// History disabled (gate NOT connected): the same POST still returns 200 and
/// nothing is recorded — recording is non-fatal / best-effort.
#[tokio::test]
async fn proxy_succeeds_and_records_nothing_when_history_disabled() {
    // This case needs no live DB — it asserts the absence of recording.
    let suffix = nanos();
    let model = format!("e2e-pg-nohist-{suffix}");
    let virtual_key = "sk-drgtw-e2epg02";

    let mock_server = MockServer::start().await;
    mount_chat_mock(&mock_server, &model).await;

    // No [ui.history] → NotConfigured gate → ProxyState.history is None.
    let cfg = load_config(&mock_server.uri(), virtual_key, &model, None);
    let app = drgtw::server::router_with_gate(
        cfg,
        std::path::Path::new("."),
        std::path::PathBuf::new(),
        PgGate::NotConfigured,
    )
    .expect("router build failed");

    let req = chat_request(
        virtual_key,
        &format!(r#"{{"model":"{model}","messages":[{{"role":"user","content":"ping"}}]}}"#),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "proxy must still succeed when history is disabled"
    );

    // If a DB happens to be available, confirm nothing leaked in under the
    // disabled gate (best-effort — skipped silently when no DB).
    if let Some(h) = connect_or_skip().await {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let rows = h.recent_usage(100).await.expect("recent_usage");
        assert!(
            !rows.iter().any(|e| e.model == model),
            "no row should be recorded when the history gate is not connected"
        );
    }
}
