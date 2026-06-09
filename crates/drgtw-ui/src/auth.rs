//! Session auth for the admin UI.
//!
//! When `[ui.auth]` is configured, every `/ui` route except `/ui/login` and
//! `/ui/assets/*` is gated behind a signed session cookie. Authentication flow:
//!
//! 1. Unauthenticated request → 303 redirect to `/ui/login`.
//! 2. `GET /ui/login` → renders a login form with a CSRF hidden field.
//! 3. `POST /ui/login` → verifies CSRF + credentials; on success sets the
//!    `drgtw_ui_session` cookie and redirects (303) to `/ui`.
//! 4. `POST /ui/logout` → clears the cookie and redirects to `/ui/login`.
//!
//! In open mode (`[ui.auth]` absent) the middleware is a no-op pass-through.

use std::time::SystemTime;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use drgtw_config::Config;
use drgtw_ui_auth::cookie::{clear_cookie, session_cookie};
use drgtw_ui_auth::csrf::{csrf_token, verify_csrf};
use drgtw_ui_auth::password::verify_password;
use drgtw_ui_auth::session::{Session, sign_session, verify_session};
use maud::{DOCTYPE, Markup, html};
use serde::Deserialize;

use crate::UiState;

/// Append an audit entry if a live history store is connected. Awaited but
/// best-effort — errors are ignored (auth is not a hot path, and the audit log
/// must never block or fail a login/logout).
async fn audit(state: &UiState, actor: &str, action: &str, detail: serde_json::Value) {
    if let Some(h) = state.history() {
        let entry = drgtw_history::AuditEntry {
            ts_unix_ms: unix_now_ms(),
            actor: actor.to_owned(),
            action: action.to_owned(),
            target: "ui".to_owned(),
            detail,
        };
        let _ = h.append_audit(&entry).await;
    }
}

/// Current epoch time in milliseconds.
fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ----------------------------------------------------------------- constants

const COOKIE_NAME: &str = "drgtw_ui_session";

// ----------------------------------------------------------------- middleware

/// Axum middleware: enforce auth when `[ui.auth]` is configured.
///
/// Routes `/ui/login` (GET + POST) and `/ui/assets/*` always pass through.
/// All other `/ui` routes require a valid, unexpired session cookie.
/// The verified username is inserted into request extensions so pages can
/// display it in the sidebar footer.
pub async fn auth_layer(
    State(state): State<UiState>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(auth_cfg) = &state.config.ui.auth else {
        // Open mode — no auth configured, pass through.
        return next.run(req).await;
    };

    let path = req.uri().path().to_owned();

    // The middleware runs inside the /ui-nested router, so paths arrive
    // with the /ui prefix already stripped: /login, /assets/*, etc.
    if path == "/login" || path.starts_with("/assets/") {
        return next.run(req).await;
    }

    // Extract cookie header.
    let cookie_header = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();

    let token = extract_cookie(&cookie_header, COOKIE_NAME);

    let now_unix = unix_now();
    let key = auth_cfg.session_key.as_bytes();

    match token.and_then(|t| verify_session(t, key, now_unix)) {
        Some(session) => {
            // Valid session — store username in extensions for page rendering.
            req.extensions_mut().insert(AuthenticatedUser(session.sub));
            next.run(req).await
        }
        None => {
            // No cookie or invalid/expired — redirect to login.
            Redirect::to("/ui/login").into_response()
        }
    }
}

/// Request extension inserted by `auth_layer` when auth is enabled and valid.
#[derive(Clone)]
pub struct AuthenticatedUser(pub String);

// ----------------------------------------------------------------- login page

/// `GET /ui/login` — render the login form.
pub async fn get_login(State(state): State<UiState>) -> impl IntoResponse {
    Html(render_login_page(&state.config, None).into_string())
}

// ----------------------------------------------------------------- login POST

#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
    csrf_token: String,
}

/// `POST /ui/login` — verify CSRF + credentials; set cookie on success.
pub async fn post_login(
    State(state): State<UiState>,
    Form(form): Form<LoginForm>,
) -> Response {
    let Some(auth_cfg) = &state.config.ui.auth else {
        // Auth not configured — redirect to UI directly.
        return Redirect::to("/ui").into_response();
    };

    let key = auth_cfg.session_key.as_bytes();

    // Derive a stable session id for CSRF from the username (no real session id
    // yet — we use the username as the binding because we're stateless).
    let csrf_session_id = &auth_cfg.username;

    // Verify CSRF first.
    if !verify_csrf(&form.csrf_token, key, csrf_session_id) {
        audit(&state, &form.username, "login.failure", serde_json::json!({"reason": "csrf"})).await;
        return Html(render_login_page(&state.config, Some("Invalid credentials.")).into_string())
            .into_response();
    }

    // Verify username + password against the config-user first (constant-time
    // on password; username is compared with == which is fine for admin use).
    let config_username_ok = form.username == auth_cfg.username;
    let config_password_ok = verify_password(&form.password, &auth_cfg.password_hash);

    // If config-user check passes, proceed immediately.  Otherwise fall through
    // to the DB co-admin lookup so team members can log in too.
    let authed_username: String;
    if config_username_ok && config_password_ok {
        authed_username = auth_cfg.username.clone();
    } else {
        // Try the history store (co-admins created via the Team page).
        let db_match = if let Some(h) = state.history() {
            match h.find_user(&form.username).await {
                Ok(Some(row)) if verify_password(&form.password, &row.password_hash) => {
                    // Persist the session record so it can be revoked later.
                    let session_id = format!("ui-{}", unix_now());
                    let ttl_secs = auth_cfg.session_ttl_hours * 3600;
                    let expires_ms = unix_now_ms() + (ttl_secs as i64) * 1000;
                    let _ = h.create_session(&session_id, row.id, expires_ms).await;
                    Some(row.username)
                }
                _ => None,
            }
        } else {
            None
        };

        match db_match {
            Some(u) => authed_username = u,
            None => {
                audit(&state, &form.username, "login.failure", serde_json::json!({"reason": "credentials"})).await;
                return Html(render_login_page(&state.config, Some("Invalid credentials.")).into_string())
                    .into_response();
            }
        }
    }

    audit(&state, &authed_username, "login.success", serde_json::json!({})).await;

    // Mint a session token.
    let ttl_secs = auth_cfg.session_ttl_hours * 3600;
    let exp_unix = unix_now() + ttl_secs;
    let session = Session { sub: authed_username.clone(), exp_unix };
    let token = sign_session(&session, key);

    // Build the Set-Cookie header. `secure=false` for localhost dev; operators
    // running behind TLS can add Secure themselves (or we can add a config flag later).
    let cookie_str = session_cookie(COOKIE_NAME, &token, ttl_secs, false);

    let mut response = Redirect::to("/ui").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie_str).expect("cookie header value is valid ASCII"),
    );
    response
}

// ----------------------------------------------------------------- logout POST

/// `POST /ui/logout` — clear cookie and redirect to login.
pub async fn post_logout(State(state): State<UiState>) -> Response {
    let actor = state.config.ui.auth.as_ref().map(|a| a.username.clone()).unwrap_or_else(|| "operator".to_owned());
    audit(&state, &actor, "logout", serde_json::json!({})).await;

    let clear = clear_cookie(COOKIE_NAME);
    let mut response = Redirect::to("/ui/login").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear).expect("clear cookie header value is valid ASCII"),
    );
    response
}

// ----------------------------------------------------------------- login page template

fn render_login_page(config: &Config, error: Option<&str>) -> Markup {
    let auth_cfg = config.ui.auth.as_ref();
    // CSRF token bound to the (would-be) username so it's deterministic per operator.
    let csrf = auth_cfg
        .map(|a| csrf_token(a.session_key.as_bytes(), &a.username))
        .unwrap_or_default();

    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "drgtw — Sign in" }
                link rel="stylesheet" href="/ui/assets/vendor/app.css";
            }
            body class="bg-background text-foreground antialiased min-h-screen grid place-items-center" {
                div class="w-full max-w-sm px-4" {
                    // Brand
                    div class="mb-8 text-center" {
                        div class="brand-gradient shimmer size-14 rounded-2xl grid place-items-center text-white font-bold text-2xl shadow-lg mx-auto mb-4" { "d" }
                        div class="font-semibold brand-text text-xl" { "drgtw" }
                        div class="text-sm text-muted-foreground mt-1" { "LLM privacy gateway" }
                    }

                    // Login card
                    div class="glass rounded-2xl p-8" {
                        h1 class="text-lg font-semibold mb-6" { "Sign in" }

                        @if let Some(msg) = error {
                            div class="mb-4 rounded-lg px-4 py-3 text-sm badge-err" {
                                (msg)
                            }
                        }

                        form method="post" action="/ui/login" class="flex flex-col gap-4" {
                            input type="hidden" name="csrf_token" value=(csrf);

                            div class="flex flex-col gap-1.5" {
                                label for="username" class="text-sm font-medium" { "Username" }
                                input
                                    type="text"
                                    id="username"
                                    name="username"
                                    autocomplete="username"
                                    required
                                    class="input w-full"
                                    placeholder="admin";
                            }

                            div class="flex flex-col gap-1.5" {
                                label for="password" class="text-sm font-medium" { "Password" }
                                input
                                    type="password"
                                    id="password"
                                    name="password"
                                    autocomplete="current-password"
                                    required
                                    class="input w-full"
                                    placeholder="••••••••";
                            }

                            button
                                type="submit"
                                class="btn-primary w-full mt-2"
                            { "Sign in" }
                        }
                    }
                }
            }
        }
    }
}

// ----------------------------------------------------------------- helpers

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Extract a named cookie value from a raw `Cookie:` header string.
fn extract_cookie<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some(rest) = pair.strip_prefix(name) {
            if let Some(val) = rest.strip_prefix('=') {
                return Some(val.trim());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use drgtw_ui_auth::password::hash_password;

    #[test]
    fn extract_cookie_finds_named_value() {
        let hdr = "foo=bar; drgtw_ui_session=tok123; baz=qux";
        assert_eq!(extract_cookie(hdr, "drgtw_ui_session"), Some("tok123"));
    }

    #[test]
    fn extract_cookie_missing_returns_none() {
        let hdr = "foo=bar";
        assert_eq!(extract_cookie(hdr, "drgtw_ui_session"), None);
    }

    #[test]
    fn extract_cookie_empty_header_returns_none() {
        assert_eq!(extract_cookie("", "drgtw_ui_session"), None);
    }

    // ---- DB-login credential helpers ----------------------------------------

    /// The happy path: a hash produced by `hash_password` is accepted by
    /// `verify_password` — the argon2id round-trip that the DB-login branch
    /// relies on.
    #[test]
    fn password_hash_round_trip() {
        let pw = "hunter2-secret";
        let hash = hash_password(pw).expect("hash succeeds");
        assert!(verify_password(pw, &hash), "correct pw must verify");
    }

    /// Wrong password must not verify — DB-login sad path.
    #[test]
    fn wrong_password_does_not_verify() {
        let hash = hash_password("correct-pw").expect("hash succeeds");
        assert!(!verify_password("wrong-pw", &hash));
    }

    /// `unix_now` must return a plausible epoch (after 2024-01-01).
    #[test]
    fn unix_now_is_sane() {
        let now = unix_now();
        assert!(now > 1_700_000_000, "unix_now must be after 2023: {now}");
    }
}
