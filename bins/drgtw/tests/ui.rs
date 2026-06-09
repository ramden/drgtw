//! Bin-level smoke tests for the embedded admin UI mount (concept).
//!
//! `server::router` mounts `/ui` only when `config.ui.enabled`. These tests
//! build the router via the public `drgtw::server::router` and drive it with
//! `tower::ServiceExt::oneshot`, the same pattern as the proxy/e2e suites.
//! Config is built by writing a temp TOML file and loading it with
//! `drgtw_config::load`.

use std::io::Write as _;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drgtw_config::load;
use http_body_util::BodyExt as _;
use tempfile::NamedTempFile;
use tower::ServiceExt; // for `.oneshot()`

/// Write a temp TOML config and load it.
fn load_config(toml: &str) -> Arc<drgtw_config::Config> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);

    let path = std::env::temp_dir().join(format!("drgtw-ui-test-{n}.toml"));
    let mut f = std::fs::File::create(&path).expect("create temp config");
    f.write_all(toml.as_bytes()).expect("write temp config");
    let cfg = load(&path).expect("load temp config");
    Arc::new(cfg)
}

/// A gate that unlocks the full UI without a live database: a `Connected`
/// variant backed by the no-op disabled history handle. Page-level rendering
/// (coming.rs) still keys its locked/unlocked state on `config.ui.history`, so
/// the Postgres-gated page tests behave exactly as before — this only flips the
/// router out of the WP-B "locked setup page" mode so functional/auth/config
/// tests exercise the real pages.
fn connected_gate() -> drgtw_ui::PgGate {
    drgtw_ui::PgGate::Connected(std::sync::Arc::new(drgtw_history::History::disabled()))
}

fn router(cfg: Arc<drgtw_config::Config>) -> axum::Router {
    drgtw::server::router_with_gate(
        cfg,
        std::path::Path::new("."),
        std::path::PathBuf::new(),
        connected_gate(),
    )
    .expect("build router")
}

/// Build a router with an explicit gate (WP-B locked-mode tests).
fn router_gated(cfg: Arc<drgtw_config::Config>, gate: drgtw_ui::PgGate) -> axum::Router {
    drgtw::server::router_with_gate(cfg, std::path::Path::new("."), std::path::PathBuf::new(), gate)
        .expect("build router")
}

async fn get(app: axum::Router, uri: &str) -> StatusCode {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

/// Fetch a page and return (status, body-as-string).
async fn fetch(app: axum::Router, uri: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Every nav page route, with the leading `/ui` mount prefix.
const ALL_PAGES: &[&str] = &[
    "/ui",
    "/ui/config",
    "/ui/connections",
    "/ui/keys",
    "/ui/analytics",
    "/ui/traces",
    "/ui/pii",
    "/ui/audit",
    "/ui/budgets",
    "/ui/limits",
    "/ui/mcp",
    "/ui/webhooks",
    "/ui/team",
    "/ui/settings",
];

#[tokio::test]
async fn ui_dashboard_is_200_when_enabled() {
    let cfg = load_config("[ui]\nenabled = true\n");
    let (status, html) = fetch(router(cfg), "/ui").await;
    assert_eq!(status, StatusCode::OK);
    // Key dashboard elements present.
    assert!(html.contains("Dashboard"), "page title");
    assert!(html.contains("trafficChart"), "chart canvas mounted");
    assert!(html.contains("Recent requests"), "recent requests table");
    assert!(html.contains("Operational"), "live status pill");
    // Vendored chart + fonts are linked.
    assert!(html.contains("chart.umd.min.js"), "chart.js vendored script");
}

#[tokio::test]
async fn all_nav_pages_return_200_when_enabled() {
    for uri in ALL_PAGES {
        let cfg = load_config("[ui]\nenabled = true\n");
        let (status, html) = fetch(router(cfg), uri).await;
        assert_eq!(status, StatusCode::OK, "route {uri} should be 200");
        // Every page renders through the shell — assert the brand + nav exist.
        assert!(html.contains("drgtw"), "{uri}: brand mark present");
        assert!(html.contains("app.css"), "{uri}: stylesheet linked");
    }
}

#[tokio::test]
async fn coming_soon_pages_show_badges() {
    let cfg = load_config("[ui]\nenabled = true\n");
    let (_, html) = fetch(router(cfg), "/ui/pii").await;
    assert!(html.contains("Coming soon"), "PII Insights coming-soon badge");
    assert!(html.contains("redaction"), "PII Insights description");
}

#[tokio::test]
async fn postgres_pages_render_empty_state_without_data() {
    // WP-C: analytics/traces/audit are now real Postgres-backed pages. With a
    // connected-but-disabled store (no rows) they must render their own empty
    // state — 200, never a 500, and not the old "Requires PostgreSQL" lock.
    let expected_empty = [
        ("/ui/analytics", "Nothing to chart"),
        ("/ui/traces", "No request traces yet"),
        ("/ui/audit", "No audit activity yet"),
    ];
    for (uri, marker) in expected_empty {
        let cfg = load_config("[ui]\nenabled = true\n");
        let (status, html) = fetch(router(cfg), uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: must be 200");
        assert!(html.contains(marker), "{uri}: empty-state marker `{marker}` present");
    }
}

// ---------------------------------------------------------------------------
// WP-B: router-level gating on the boot-time Postgres connect (PgGate).
// When no live history store is connected, the UI locks: every path serves the
// setup page (assets excepted), and the real login/config/dashboard are absent.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gate_not_configured_locks_all_pages_to_setup() {
    // ui.enabled = true, but the gate is NotConfigured (no live store). Every
    // page — index, /login, /config, /analytics — must render the setup page.
    // (`/ui/` with a trailing slash is not a canonical route under axum's nest:
    // it 404s even for the connected full router, so the index is tested as
    // `/ui` only — matching every existing UI test.)
    for uri in ["/ui", "/ui/login", "/ui/config", "/ui/analytics"] {
        let cfg = load_config("[ui]\nenabled = true\n");
        let (status, html) = fetch(router_gated(cfg, drgtw_ui::PgGate::NotConfigured), uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: setup page should be 200");
        assert!(
            html.contains("Requires PostgreSQL"),
            "{uri}: setup page title present"
        );
        assert!(
            html.contains("${DATABASE_URL}"),
            "{uri}: DATABASE_URL unlock snippet present"
        );
        // The real login form must NOT be served in locked mode.
        assert!(
            !html.contains("csrf_token"),
            "{uri}: real login form must not be served while locked"
        );
        assert!(
            !html.contains("trafficChart"),
            "{uri}: real dashboard must not be served while locked"
        );
    }
}

#[tokio::test]
async fn gate_not_configured_still_serves_assets() {
    // CSS/JS must still load so the setup page is styled.
    let cfg = load_config("[ui]\nenabled = true\n");
    let app = router_gated(cfg, drgtw_ui::PgGate::NotConfigured);
    assert_eq!(
        get(app, "/ui/assets/vendor/app.css").await,
        StatusCode::OK,
        "assets must serve in locked mode"
    );
}

#[tokio::test]
async fn gate_unreachable_shows_masked_url() {
    let cfg = load_config("[ui]\nenabled = true\n");
    let gate = drgtw_ui::PgGate::Unreachable {
        masked_url: "postgres://user:••••@db:5432/x".to_owned(),
    };
    let (status, html) = fetch(router_gated(cfg, gate), "/ui").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Cannot reach PostgreSQL"), "unreachable title present");
    assert!(html.contains("postgres://user:••••@db:5432/x"), "masked url shown");
    // No password leak.
    assert!(!html.contains(":secret@"), "password must stay masked");
}

#[tokio::test]
async fn gate_feature_off_shows_rebuild_hint() {
    let cfg = load_config("[ui]\nenabled = true\n");
    let (status, html) = fetch(router_gated(cfg, drgtw_ui::PgGate::FeatureOff), "/ui").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("without Postgres support"),
        "feature-off title present"
    );
    assert!(html.contains("default features"), "rebuild hint present");
}

#[tokio::test]
async fn ui_absent_when_disabled() {
    // Default config: ui disabled → no /ui route.
    let cfg = load_config("");
    assert_eq!(get(router(cfg), "/ui").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ui_assets_served_when_enabled() {
    // Datastar, Chart.js, and a vendored font all serve.
    for asset in [
        "/ui/assets/vendor/datastar.js",
        "/ui/assets/vendor/chart.umd.min.js",
        "/ui/assets/vendor/fonts/Geist-Variable.woff2",
    ] {
        let cfg = load_config("[ui]\nenabled = true\n");
        assert_eq!(get(router(cfg), asset).await, StatusCode::OK, "{asset}");
    }
}

#[tokio::test]
async fn config_viewer_masks_api_keys() {
    let toml = r#"
[ui]
enabled = true

[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "sk-secret-value-1234"
format = "open_ai"
models = ["gpt-4o", "gpt-4o-mini"]

[[virtual_keys]]
key = "sk-drgtw-uitest00001"
connections = ["openai"]
"#;
    let cfg = load_config(toml);
    let (status, html) = fetch(router(cfg), "/ui/config").await;
    assert_eq!(status, StatusCode::OK);
    // The raw api_key must never appear; it is masked.
    assert!(
        !html.contains("sk-secret-value-1234"),
        "config viewer must not leak the raw api_key"
    );
    assert!(html.contains("sk-…1234"), "masked key shape expected");
}

#[tokio::test]
async fn connections_and_keys_pages_render_data() {
    let toml = r#"
[ui]
enabled = true

[[connections]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "sk-secret-value-9999"
format = "open_ai"
models = ["gpt-4o"]

[[virtual_keys]]
key = "sk-drgtw-secretkey00001"
connections = ["openai"]
"#;
    let cfg = load_config(toml);
    let (cs, conn_html) = fetch(router(cfg), "/ui/connections").await;
    assert_eq!(cs, StatusCode::OK);
    assert!(conn_html.contains("openai"), "connection name rendered");
    assert!(
        !conn_html.contains("sk-secret-value-9999"),
        "connections page must not leak api_key"
    );

    let cfg = load_config(toml);
    let (ks, keys_html) = fetch(router(cfg), "/ui/keys").await;
    assert_eq!(ks, StatusCode::OK);
    assert!(
        !keys_html.contains("sk-drgtw-secretkey00001"),
        "virtual keys page must mask the raw key"
    );
    assert!(keys_html.contains("Virtual Keys"), "page title present");
}

// ---------------------------------------------------------------------------
// Config-editor tests (editable forms + POST /ui/config/save)
// ---------------------------------------------------------------------------

/// Shared TOML for config-editor tests.
/// Uses a literal api_key secret so `load()` succeeds without env vars.
/// The connections textarea in the UI will show the raw literal from the file;
/// secret masking is tested separately via the connections_and_keys_pages tests.
const EDITOR_TOML: &str = r#"
[ui]
enabled = true

[server]
bind_addr = "127.0.0.1:8080"
max_body_bytes = 1048576

[tracing]
enabled = true
dir = "traces"
retention_days = 90
rotate_max_bytes = 52428800

[pii]
enabled_by_default = true

[fallback]
enabled = true

[otel]
enabled = false
endpoint = "http://localhost:4317"
service_name = "drgtw"
traces = true
metrics = true
sample_ratio = 1.0
export_interval_ms = 10000
export_timeout_ms = 5000
metrics_include_key_id = false

[[connections]]
name = "primary"
base_url = "https://api.example.com/v1"
api_key = "sk-literal-secret-api-key-xyz"
format = "open_ai"
models = ["gpt-4o"]

[[virtual_keys]]
key = "sk-drgtw-editortest00001"
connections = ["primary"]
"#;

/// TOML with an ${ENV} placeholder in the connections api_key.
/// Written to a file so `read_document` (not `load`) can read it verbatim.
/// Only used for the read_document verbatim-placeholder test.
const EDITOR_TOML_ENV_PLACEHOLDER: &str = r#"
[ui]
enabled = true

[server]
bind_addr = "127.0.0.1:8080"
max_body_bytes = 1048576

[[connections]]
name = "primary"
base_url = "https://api.example.com/v1"
api_key = "${API_KEY}"
format = "open_ai"
models = ["gpt-4o"]

[[virtual_keys]]
key = "sk-drgtw-editortest00001"
connections = ["primary"]
"#;

/// Write a NamedTempFile and load the config from it, returning both the
/// loaded config and the file (so the path remains valid).
fn load_config_with_path(toml: &str) -> (Arc<drgtw_config::Config>, NamedTempFile) {
    let mut f = NamedTempFile::new().expect("tempfile");
    f.write_all(toml.as_bytes()).expect("write temp config");
    let cfg = load(f.path()).expect("load temp config");
    (Arc::new(cfg), f)
}

/// Build a router wired to a specific config file path (for config-editor tests).
fn router_with_path(cfg: Arc<drgtw_config::Config>, path: &std::path::Path) -> axum::Router {
    drgtw::server::router_with_gate(
        cfg,
        std::path::Path::new("."),
        path.to_path_buf(),
        connected_gate(),
    )
    .expect("build router")
}

/// POST a form body to a route and return (status, body).
async fn post_form(app: axum::Router, uri: &str, body: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

// --- Test 1: GET /ui/config renders inputs with current values ---

#[tokio::test]
async fn config_page_renders_editable_form() {
    let (cfg, f) = load_config_with_path(EDITOR_TOML);
    let (status, html) = fetch(router_with_path(cfg, f.path()), "/ui/config").await;
    assert_eq!(status, StatusCode::OK);
    // Page must contain form inputs (not just plain text kv rows).
    assert!(html.contains("<input"), "should render input elements");
    assert!(html.contains("<form"), "should render a form");
    // Current server values appear in inputs.
    assert!(html.contains("127.0.0.1:8080"), "bind_addr rendered in input");
    assert!(html.contains("1048576"), "max_body_bytes rendered in input");
}

// --- Test 2: ${ENV} placeholder shown verbatim; literal secret NOT in HTML ---

#[tokio::test]
async fn config_page_env_placeholder_shown_verbatim() {
    // Write a file with ${API_KEY} in connections. Load a config with a literal
    // key so `load()` succeeds, but point config_path at the placeholder file.
    // The GET handler reads config_path via read_document → shows ${API_KEY}.
    let mut placeholder_file = NamedTempFile::new().expect("tempfile");
    placeholder_file.write_all(EDITOR_TOML_ENV_PLACEHOLDER.as_bytes()).expect("write");
    let placeholder_path = placeholder_file.path().to_path_buf();

    // Load a config with a literal key for the Arc<Config> (no env var needed).
    let (cfg, _cfg_file) = load_config_with_path(EDITOR_TOML);
    let app = router_with_path(cfg, &placeholder_path);

    let (status, html) = fetch(app, "/ui/config").await;
    assert_eq!(status, StatusCode::OK);

    // ${API_KEY} is a placeholder — must appear verbatim in the connections textarea.
    assert!(html.contains("${API_KEY}"), "ENV placeholder shown verbatim in connections textarea");
}

#[tokio::test]
async fn config_page_literal_secret_not_in_html() {
    // Literal api_key must not appear verbatim — it is shown as password input
    // with the SECRET_SENTINEL value, never the real secret.
    let (cfg, f) = load_config_with_path(EDITOR_TOML);
    let (status, html) = fetch(router_with_path(cfg, f.path()), "/ui/config").await;
    assert_eq!(status, StatusCode::OK);

    // The literal api_key (in the connections textarea, from the file) may
    // appear there, but the literal virtual key must not appear verbatim.
    assert!(
        !html.contains("sk-drgtw-editortest00001"),
        "literal virtual key must not be echoed verbatim in HTML"
    );
}

// --- Test 3: POST valid change → 200, file updated, .bak created ---

#[tokio::test]
async fn config_save_valid_change_updates_file_and_creates_backup() {
    let (cfg, f) = load_config_with_path(EDITOR_TOML);
    let path = f.path().to_path_buf();
    let app = router_with_path(cfg, &path);

    let body = "_section=server&bind_addr=127.0.0.1%3A9090&max_body_bytes=2097152";
    let (status, html) = post_form(app, "/ui/config/save", body).await;
    assert_eq!(status, StatusCode::OK);

    // Success banner in response.
    assert!(html.contains("Saved"), "success banner present");

    // File must contain the new bind_addr.
    let updated = std::fs::read_to_string(&path).expect("read updated file");
    assert!(updated.contains("9090"), "new bind_addr written to file");

    // A .bak file must exist in the same directory.
    let dir = path.parent().unwrap();
    let bak_exists = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().contains(".bak."));
    assert!(bak_exists, "backup file created");
}

// --- Test 4: POST invalid change → errors shown, original file unchanged ---

#[tokio::test]
async fn config_save_invalid_change_shows_error_leaves_file_unchanged() {
    let (cfg, f) = load_config_with_path(EDITOR_TOML);
    let path = f.path().to_path_buf();
    let original_bytes = std::fs::read(&path).expect("read original");
    let app = router_with_path(cfg, &path);

    // bad bind_addr — serde rejects this during validate_str
    let body = "_section=server&bind_addr=not-a-valid-addr&max_body_bytes=1048576";
    let (status, html) = post_form(app, "/ui/config/save", body).await;
    assert_eq!(status, StatusCode::OK, "returns 200 even on error (re-rendered page)");

    // Error message must be present.
    assert!(
        html.contains("error") || html.contains("invalid") || html.contains("Error"),
        "error message present in response: {html}"
    );

    // Original file bytes must be unchanged.
    let after = std::fs::read(&path).expect("read file after");
    assert_eq!(after, original_bytes, "original file must not be modified on validation failure");
}

// --- Test 5: Restart-required banner for server change ---

#[tokio::test]
async fn config_save_server_change_shows_restart_required_banner() {
    let (cfg, f) = load_config_with_path(EDITOR_TOML);
    let path = f.path().to_path_buf();
    let app = router_with_path(cfg, &path);

    let body = "_section=server&bind_addr=127.0.0.1%3A9191&max_body_bytes=1048576";
    let (status, html) = post_form(app, "/ui/config/save", body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Saved"), "saved banner");
    assert!(
        html.contains("Restart required") || html.contains("restart"),
        "restart-required notice present for server change"
    );
}

// --- Test 6: Tracing-only change does NOT show restart-required banner ---

#[tokio::test]
async fn config_save_tracing_only_change_no_restart_required() {
    let (cfg, f) = load_config_with_path(EDITOR_TOML);
    let path = f.path().to_path_buf();
    let app = router_with_path(cfg, &path);

    let body = "_section=tracing&enabled=true&dir=new-traces&retention_days=90&rotate_max_bytes=52428800";
    let (status, html) = post_form(app, "/ui/config/save", body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Saved"), "saved banner");
    // Tracing is hot-reloadable — restart banner must NOT appear.
    assert!(
        !html.contains("Restart required"),
        "no restart-required notice for tracing-only change: {html}"
    );
}

// =============================================================================
// Auth tests
// =============================================================================

/// Build the [ui.auth] TOML block with a freshly hashed password.
/// Returns (toml_string, phc_string) so tests can also mint valid cookies.
fn auth_toml(username: &str, password: &str, session_key: &str) -> String {
    let phc = drgtw_ui_auth::password::hash_password(password)
        .expect("hash_password in test");
    format!(
        "[ui]\nenabled = true\n\n[ui.auth]\nusername = \"{username}\"\npassword_hash = \"{phc}\"\nsession_key = \"{session_key}\"\nsession_ttl_hours = 24\n"
    )
}

/// Helper: build a valid session cookie value for use in auth tests.
fn mint_cookie(username: &str, session_key: &str) -> String {
    use drgtw_ui_auth::session::{Session, sign_session};
    let exp_unix = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let session = Session { sub: username.to_owned(), exp_unix };
    sign_session(&session, session_key.as_bytes())
}

/// Helper: GET with an optional Cookie header, return (status, Location, body).
async fn get_with_cookie(
    app: axum::Router,
    uri: &str,
    cookie: Option<&str>,
) -> (StatusCode, Option<String>, String) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(c) = cookie {
        builder = builder.header("cookie", format!("drgtw_ui_session={c}"));
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, location, String::from_utf8_lossy(&bytes).into_owned())
}

// --- Auth: open mode (no [ui.auth]) preserves existing behaviour ---

#[tokio::test]
async fn auth_open_mode_get_ui_is_200() {
    // No [ui.auth] → unauthenticated GET /ui must still be 200.
    let cfg = load_config("[ui]\nenabled = true\n");
    let (status, _loc, _body) = get_with_cookie(router(cfg), "/ui", None).await;
    assert_eq!(status, StatusCode::OK, "open mode: GET /ui must be 200 without a cookie");
}

// --- Auth: locked mode redirects unauthenticated requests ---

#[tokio::test]
async fn auth_locked_mode_get_ui_without_cookie_is_303_to_login() {
    let toml = auth_toml("admin", "s3cr3t", "test-session-key-32bytes-padding!");
    let cfg = load_config(&toml);
    let (status, loc, _body) = get_with_cookie(router(cfg), "/ui", None).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "locked mode: GET /ui without cookie must be 303");
    assert_eq!(loc.as_deref(), Some("/ui/login"), "must redirect to /ui/login");
}

#[tokio::test]
async fn auth_locked_mode_get_login_is_200() {
    let toml = auth_toml("admin", "s3cr3t", "test-session-key-32bytes-padding!");
    let cfg = load_config(&toml);
    let (status, _loc, html) = get_with_cookie(router(cfg), "/ui/login", None).await;
    assert_eq!(status, StatusCode::OK, "GET /ui/login must be 200 without a cookie");
    assert!(html.contains("Sign in"), "login page must contain 'Sign in'");
    assert!(html.contains("csrf_token"), "login page must contain CSRF hidden field");
}

// --- Auth: POST /ui/login with wrong password re-renders with error ---

#[tokio::test]
async fn auth_login_wrong_password_rerenders_error() {
    let session_key = "test-session-key-32bytes-padding!";
    let toml = auth_toml("admin", "correct-password", session_key);
    let cfg = load_config(&toml);

    // Build a valid CSRF token for the login form.
    let csrf = drgtw_ui_auth::csrf::csrf_token(session_key.as_bytes(), "admin");
    let body = format!(
        "username=admin&password=wrong-password&csrf_token={csrf}",
        csrf = urlenccode(&csrf),
    );
    let (status, set_cookie, html) = post_form_full(router(cfg), "/ui/login", &body).await;
    // Must re-render the page (200), not redirect.
    assert_eq!(status, StatusCode::OK, "wrong password: expect 200 re-render");
    assert!(html.contains("Invalid credentials"), "must show error message");
    // No session cookie must be set.
    assert!(
        set_cookie.is_none(),
        "wrong password: must not set a session cookie"
    );
}

// --- Auth: POST /ui/login correct password → 303 + Set-Cookie ---

#[tokio::test]
async fn auth_login_correct_password_sets_cookie_and_redirects() {
    let session_key = "test-session-key-32bytes-padding!";
    let password = "correct-password";
    let toml = auth_toml("admin", password, session_key);
    let cfg = load_config(&toml);

    let csrf = drgtw_ui_auth::csrf::csrf_token(session_key.as_bytes(), "admin");
    let body = format!(
        "username=admin&password={pw}&csrf_token={csrf}",
        pw = urlenccode(password),
        csrf = urlenccode(&csrf),
    );
    let (status, loc, set_cookie) = post_form_redirect(router(cfg), "/ui/login", &body).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "correct login: must redirect");
    assert_eq!(loc.as_deref(), Some("/ui"), "must redirect to /ui");
    assert!(set_cookie.is_some(), "correct login: must set a session cookie");
    let cookie_val = set_cookie.unwrap();
    assert!(cookie_val.contains("drgtw_ui_session="), "cookie must be named drgtw_ui_session");
    assert!(cookie_val.contains("HttpOnly"), "cookie must be HttpOnly");
}

// --- Auth: GET /ui WITH a valid session cookie → 200 ---

#[tokio::test]
async fn auth_valid_cookie_allows_access() {
    let session_key = "test-session-key-32bytes-padding!";
    let toml = auth_toml("admin", "s3cr3t", session_key);
    let cfg = load_config(&toml);

    let token = mint_cookie("admin", session_key);
    let (status, _loc, html) = get_with_cookie(router(cfg), "/ui", Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "valid cookie: GET /ui must be 200");
    assert!(html.contains("Dashboard"), "must render the dashboard");
}

// --- Auth: expired cookie → redirect to login ---

#[tokio::test]
async fn auth_expired_cookie_redirects_to_login() {
    use drgtw_ui_auth::session::{Session, sign_session};
    let session_key = "test-session-key-32bytes-padding!";
    let toml = auth_toml("admin", "s3cr3t", session_key);
    let cfg = load_config(&toml);

    // exp_unix = 1 (already expired in 2024).
    let session = Session { sub: "admin".to_owned(), exp_unix: 1 };
    let expired_token = sign_session(&session, session_key.as_bytes());

    let (status, loc, _body) = get_with_cookie(router(cfg), "/ui", Some(&expired_token)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "expired cookie: must redirect");
    assert_eq!(loc.as_deref(), Some("/ui/login"), "must redirect to /ui/login");
}

// --- Auth: tampered cookie → redirect to login ---

#[tokio::test]
async fn auth_tampered_cookie_redirects_to_login() {
    let session_key = "test-session-key-32bytes-padding!";
    let toml = auth_toml("admin", "s3cr3t", session_key);
    let cfg = load_config(&toml);

    let (status, loc, _body) = get_with_cookie(router(cfg), "/ui", Some("garbage.token")).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "tampered cookie: must redirect");
    assert_eq!(loc.as_deref(), Some("/ui/login"), "must redirect to /ui/login");
}

// --- Auth: /ui/assets/* reachable without a cookie ---

#[tokio::test]
async fn auth_assets_reachable_without_cookie() {
    let session_key = "test-session-key-32bytes-padding!";
    let toml = auth_toml("admin", "s3cr3t", session_key);
    let cfg = load_config(&toml);

    // app.css must load so the login page can be styled.
    let (status, _loc, _body) =
        get_with_cookie(router(cfg), "/ui/assets/vendor/app.css", None).await;
    assert_eq!(status, StatusCode::OK, "/ui/assets/vendor/app.css must be 200 without a cookie");
}

// --- Config validation: [ui.auth] valid ---

#[tokio::test]
async fn config_ui_auth_valid_section_loads() {
    let toml = auth_toml("admin", "hunter2", "a-32-byte-key-for-hmac-padding!!");
    // load_config panics on error, so success means validation passed.
    let cfg = load_config(&toml);
    assert!(cfg.ui.auth.is_some(), "[ui.auth] must be Some after loading");
}

// --- Config validation: bad password_hash → load error ---

#[tokio::test]
async fn config_ui_auth_bad_password_hash_is_invalid() {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1000);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);

    let toml = "[ui]\nenabled = true\n\n[ui.auth]\nusername = \"admin\"\npassword_hash = \"notaphcstring\"\nsession_key = \"key\"\n";
    let path = std::env::temp_dir().join(format!("drgtw-ui-auth-badphc-{n}.toml"));
    let mut f = std::fs::File::create(&path).expect("create temp config");
    f.write_all(toml.as_bytes()).expect("write temp config");
    let result = drgtw_config::load(&path);
    assert!(result.is_err(), "bad password_hash must cause a load error");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("argon2") || msg.contains("password_hash") || msg.contains("invalid"),
        "error must mention the field: {msg}"
    );
}

// --- Config validation: empty username → load error ---

#[tokio::test]
async fn config_ui_auth_empty_username_is_invalid() {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(2000);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);

    let phc = drgtw_ui_auth::password::hash_password("pw").expect("hash");
    let toml = format!("[ui]\nenabled = true\n\n[ui.auth]\nusername = \"\"\npassword_hash = \"{phc}\"\nsession_key = \"key\"\n");
    let path = std::env::temp_dir().join(format!("drgtw-ui-auth-emptyusr-{n}.toml"));
    let mut f = std::fs::File::create(&path).expect("create temp config");
    f.write_all(toml.as_bytes()).expect("write temp config");
    let result = drgtw_config::load(&path);
    assert!(result.is_err(), "empty username must cause a load error");
}

// ---------------------------------------------------------------------------
// Helper: POST a form and return (status, Option<set-cookie header value>, body).
// Used by tests that need to inspect the Set-Cookie header.
// ---------------------------------------------------------------------------

async fn post_form_full(
    app: axum::Router,
    uri: &str,
    body: &str,
) -> (StatusCode, Option<String>, String) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, set_cookie, String::from_utf8_lossy(&bytes).into_owned())
}

/// POST a form and return (status, Location header, Set-Cookie header).
/// Used for login redirect tests.
async fn post_form_redirect(
    app: axum::Router,
    uri: &str,
    body: &str,
) -> (StatusCode, Option<String>, Option<String>) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (status, location, set_cookie)
}

/// Minimal percent-encode for form values (encode `+`, `=`, `&`, space, `$`, `{`, `}`).
fn urlenccode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// WP-C: real-data UI pages, backed by a LIVE Postgres history store.
//
// These tests are gated on DATABASE_URL (skipped when unset, mirroring
// drgtw-history's `connect_or_skip`). They connect a real `History`, seed a few
// usage events + an audit entry through the handle, build a router whose gate is
// `Connected(<that handle>)`, then assert the seeded data surfaces in the
// rendered pages and the JSON API.
// ---------------------------------------------------------------------------

mod wp_c_postgres {
    use super::*;
    use drgtw_events::UsageEvent;
    use drgtw_history::{AuditEntry, History};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ms() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
    }

    async fn connect_or_skip() -> Option<History> {
        let url = match std::env::var("DATABASE_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!("DATABASE_URL not set — skipping WP-C Postgres UI tests");
                return None;
            }
        };
        match History::connect(&url).await {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("Could not connect to Postgres ({e}) — skipping WP-C UI tests");
                None
            }
        }
    }

    /// A usage event with a unique, recent timestamp so it lands inside the
    /// dashboard's 24h and analytics' 7d windows.
    fn seed_event(model: &str, connection: &str, status: u16, latency_ms: u64, pii: bool) -> UsageEvent {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        UsageEvent {
            request_id: format!("wpc-req-{n}-{}", now_ms()),
            key_id: "wpc-key".to_owned(),
            endpoint: "chat_completions".to_owned(),
            model: model.to_owned(),
            connection: connection.to_owned(),
            status,
            input_tokens: Some(120),
            output_tokens: Some(80),
            cost_usd: Some(0.0123),
            latency_ms,
            pii,
            streamed: true,
            fallback_attempts: 0,
            ts_unix_ms: now_ms() as u64,
            metadata: None,
        }
    }

    /// Build a router whose gate is `Connected(history)` — exercises the real
    /// async DB-backed handlers against the live store.
    fn router_connected(cfg: Arc<drgtw_config::Config>, history: History) -> axum::Router {
        let gate = drgtw_ui::PgGate::Connected(std::sync::Arc::new(history));
        router_gated(cfg, gate)
    }

    #[tokio::test]
    async fn dashboard_shows_real_hero_numbers_and_recent_rows() {
        let Some(h) = connect_or_skip().await else { return };
        // Unique model name so we can find it in the recent-requests table.
        let model = format!("wpc-model-dash-{}", now_ms());
        for _ in 0..3 {
            h.record_usage(&seed_event(&model, "wpc-conn", 200, 410, true))
                .await
                .expect("record_usage");
        }
        let cfg = load_config("[ui]\nenabled = true\n");
        let (status, html) = fetch(router_connected(cfg, h), "/ui").await;
        assert_eq!(status, StatusCode::OK);
        // Seeded model appears in the recent-requests table.
        assert!(html.contains(&model), "dashboard recent table should show seeded model");
        // The traffic series is injected as JSON.
        assert!(html.contains("window.__traffic"), "dashboard injects server-rendered traffic JSON");
        // Hero cards are not all the placeholder dash — requests > 0 renders digits.
        assert!(html.contains("Requests · 24h"), "hero requests card present");
    }

    #[tokio::test]
    async fn traces_shows_seeded_request() {
        let Some(h) = connect_or_skip().await else { return };
        let model = format!("wpc-model-trace-{}", now_ms());
        let ev = seed_event(&model, "wpc-conn-trace", 503, 999, false);
        let rid = ev.request_id.clone();
        h.record_usage(&ev).await.expect("record_usage");
        let cfg = load_config("[ui]\nenabled = true\n");
        let (status, html) = fetch(router_connected(cfg, h), "/ui/traces").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(&model), "traces table should show the seeded model");
        // request_id is truncated to its first 10 chars in the table.
        let short: String = rid.chars().take(10).collect();
        assert!(html.contains(&short), "traces table should show the truncated request id");
    }

    #[tokio::test]
    async fn analytics_shows_breakdowns() {
        let Some(h) = connect_or_skip().await else { return };
        let model = format!("wpc-model-an-{}", now_ms());
        for _ in 0..4 {
            h.record_usage(&seed_event(&model, "wpc-conn-an", 200, 300, false))
                .await
                .expect("record_usage");
        }
        let cfg = load_config("[ui]\nenabled = true\n");
        let (status, html) = fetch(router_connected(cfg, h), "/ui/analytics").await;
        assert_eq!(status, StatusCode::OK);
        // The bar-chart payloads are injected as JSON and include the seeded label.
        assert!(html.contains("window.__byModel"), "analytics injects model breakdown JSON");
        assert!(html.contains(&model), "analytics should reference the seeded model label");
        assert!(html.contains("Requests · 7d"), "analytics summary card present");
    }

    #[tokio::test]
    async fn audit_shows_seeded_entry() {
        let Some(h) = connect_or_skip().await else { return };
        let actor = format!("wpc-actor-{}", now_ms());
        let entry = AuditEntry {
            ts_unix_ms: now_ms(),
            actor: actor.clone(),
            action: "config.save".to_owned(),
            target: "drgtw.toml".to_owned(),
            detail: serde_json::json!({ "sections": ["server"] }),
        };
        h.append_audit(&entry).await.expect("append_audit");
        let cfg = load_config("[ui]\nenabled = true\n");
        let (status, html) = fetch(router_connected(cfg, h), "/ui/audit").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(&actor), "audit table should show the seeded actor");
        assert!(html.contains("config.save"), "audit table should show the action");
    }

    #[tokio::test]
    async fn api_timeseries_returns_json_arrays() {
        let Some(h) = connect_or_skip().await else { return };
        h.record_usage(&seed_event("wpc-model-api", "wpc-conn-api", 200, 250, false))
            .await
            .expect("record_usage");
        let cfg = load_config("[ui]\nenabled = true\n");
        let (status, body) = fetch(router_connected(cfg, h), "/ui/api/timeseries?range=24h").await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body).expect("timeseries API returns JSON");
        for key in ["labels", "requests", "input_tokens", "output_tokens", "cost_usd", "avg_latency_ms"] {
            assert!(json.get(key).map(|v| v.is_array()).unwrap_or(false), "key `{key}` is a JSON array");
        }
        // With at least one seeded request in the last 24h, the requests array is non-empty.
        assert!(
            !json["requests"].as_array().unwrap().is_empty(),
            "timeseries requests array should have buckets after seeding"
        );
    }

    #[tokio::test]
    async fn login_emits_audit_entry() {
        let Some(h) = connect_or_skip().await else { return };
        let session_key = "test-session-key-32bytes-padding!";
        let username = format!("wpc-login-{}", now_ms());
        let password = "correct-password";
        let toml = auth_toml(&username, password, session_key);
        let cfg = load_config(&toml);

        let csrf = drgtw_ui_auth::csrf::csrf_token(session_key.as_bytes(), &username);
        let body = format!(
            "username={u}&password={pw}&csrf_token={csrf}",
            u = urlenccode(&username),
            pw = urlenccode(password),
            csrf = urlenccode(&csrf),
        );
        // Hold a clone of the handle to query the audit log after the login.
        let probe = drgtw_history::History::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .expect("second connect");
        let (status, _loc, _cookie) =
            post_form_redirect(router_connected(cfg, h), "/ui/login", &body).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "login should redirect on success");

        let rows = probe.recent_audit(50).await.expect("recent_audit");
        assert!(
            rows.iter().any(|e| e.actor == username && e.action == "login.success"),
            "a login.success audit row should exist for the seeded user"
        );
    }
}

