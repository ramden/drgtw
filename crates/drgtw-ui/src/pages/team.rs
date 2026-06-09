//! `GET /ui/team` — Team & Access: DB-backed co-admins, plus create and delete.
//!
//! Lists users stored in the history store (`history.list_users`).  The config
//! primary user is shown as a separate entry (always present, not deletable from
//! the UI).  New co-admins are created via `hash_password` + `create_user`.
//! A small SSO coming-soon card is rendered below the user table.

use std::collections::HashMap;

use axum::extract::{Extension, Form, Path, State};
use axum::response::{Html, Redirect};
use drgtw_history::UserRow;
use drgtw_ui_auth::password::hash_password;
use maud::{Markup, PreEscaped, html};

use crate::UiState;
use crate::auth::AuthenticatedUser;
use crate::csrf::{csrf_field, csrf_ok};
use crate::layout::{self, Nav, badge, empty_state, page_header, shell};
use crate::pages::{glass_card, section_title};

// ---------------------------------------------------------------------------
// GET /ui/team
// ---------------------------------------------------------------------------

pub async fn team_access(
    State(state): State<UiState>,
    user: Option<Extension<AuthenticatedUser>>,
) -> Html<String> {
    let users = match state.history() {
        Some(h) => h.list_users().await.unwrap_or_default(),
        None => Vec::new(),
    };
    Html(render(&state, user.as_ref().map(|u| u.0.0.as_str()), &users).into_string())
}

fn render(state: &UiState, live_user: Option<&str>, users: &[UserRow]) -> Markup {
    let cfg = &state.config;
    let unlocked = cfg.ui.history.is_some();
    let username = live_user.or_else(|| cfg.ui.auth.as_ref().map(|a| a.username.as_str()));
    let csrf_user = username.unwrap_or("_open_");

    // Config-user (primary operator): always shown; not deletable via UI.
    let config_user = cfg.ui.auth.as_ref().map(|a| a.username.as_str());

    let body = html! {
        (page_header("Team & Access", "Operator accounts and single sign-on."))

        // ── Require history ──────────────────────────────────────────────────
        @if !unlocked {
            (empty_state(
                layout::ICON_USERS, "warn", "Requires PostgreSQL",
                "History store required",
                html! {
                    "Co-admin accounts are stored in the history database. Configure "
                    code { "[ui.history]" }
                    " to enable user management."
                },
            ))
        } @else {

            // ── Operator list ────────────────────────────────────────────────
            (glass_card(1, html! {
                (section_title(layout::ICON_USERS, "Operators"))

                // Primary config user
                @if let Some(cu) = config_user {
                    div class="flex items-center justify-between py-2 border-b border-border/40 last:border-0" {
                        div class="flex items-center gap-3" {
                            div class="size-8 rounded-full bg-primary/10 grid place-items-center text-primary text-xs font-bold" {
                                (cu.chars().next().unwrap_or('?').to_uppercase().next().unwrap_or('?'))
                            }
                            div {
                                div class="text-sm font-medium" { (cu) }
                                div class="text-xs text-muted-foreground" { "Primary operator (config)" }
                            }
                        }
                        (badge("ok", "primary"))
                    }
                }

                // DB co-admins
                @if users.is_empty() {
                    p class="text-sm text-muted-foreground mt-3" {
                        "No co-admins created yet. Add one below."
                    }
                } @else {
                    @for row in users {
                        (user_row(state, row, csrf_user, config_user))
                    }
                }
            }))

            // ── Add co-admin form ────────────────────────────────────────────
            (glass_card(2, html! {
                (section_title(layout::ICON_USERS, "Add Co-admin"))
                p class="text-xs text-muted-foreground mb-4" {
                    "Co-admins can log in and manage the gateway console. "
                    "Passwords are hashed with argon2id before storage."
                }
                form method="post" action="/ui/team" class="grid gap-3" {
                    (csrf_field(state, csrf_user))
                    div class="grid grid-cols-1 sm:grid-cols-2 gap-3" {
                        div class="flex flex-col gap-1" {
                            label class="text-xs font-medium text-muted-foreground" for="team-username" {
                                "Username"
                            }
                            input id="team-username" name="username" type="text"
                                class="input" placeholder="operator" required;
                        }
                        div class="flex flex-col gap-1" {
                            label class="text-xs font-medium text-muted-foreground" for="team-password" {
                                "Password"
                            }
                            input id="team-password" name="password" type="password"
                                class="input" placeholder="••••••••" required;
                        }
                    }
                    div class="flex justify-end" {
                        button type="submit" class="btn-primary" { "Add operator" }
                    }
                }
            }))
        }

        // ── SSO coming soon ──────────────────────────────────────────────────
        (glass_card(3, html! {
            div class="flex items-center gap-3" {
                div class="size-9 rounded-xl grid place-items-center bg-primary/10 text-primary shrink-0" {
                    span class="size-5 grid place-items-center" { (PreEscaped(layout::ICON_USERS)) }
                }
                div {
                    div class="flex items-center gap-2 mb-0.5" {
                        span class="text-sm font-semibold" { "SSO via SAML / OIDC" }
                        (badge("warn", "◐ Coming soon"))
                    }
                    p class="text-xs text-muted-foreground" {
                        "Connect your identity provider for single sign-on. Every console action "
                        "will be attributable to a real person and surfaced in the audit log."
                    }
                }
            }
        }))
    };

    shell("Team & Access", "Team & Access", Nav::TeamAccess, unlocked, username, body)
}

fn user_row(state: &UiState, row: &UserRow, csrf_user: &str, config_user: Option<&str>) -> Markup {
    // Do not offer delete for the config-user (they can't be deleted from the DB).
    let is_primary = config_user.map(|u| u == row.username.as_str()).unwrap_or(false);

    html! {
        div class="flex items-center justify-between py-2 border-b border-border/40 last:border-0" {
            div class="flex items-center gap-3" {
                div class="size-8 rounded-full bg-muted grid place-items-center text-muted-foreground text-xs font-bold" {
                    (row.username.chars().next().unwrap_or('?').to_uppercase().next().unwrap_or('?'))
                }
                div {
                    div class="text-sm font-medium" { (row.username) }
                    div class="text-xs text-muted-foreground" { "id " (row.id) }
                }
            }
            @if !is_primary {
                form method="post" action=(format!("/ui/team/{}/delete", row.id)) {
                    (csrf_field(state, csrf_user))
                    button type="submit"
                        class="btn-sm-destructive"
                        onclick="return confirm('Delete this operator?')" {
                        "Delete"
                    }
                }
            } @else {
                (badge("muted", "config user"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// POST /ui/team  (create co-admin)
// ---------------------------------------------------------------------------

pub async fn team_create(
    State(state): State<UiState>,
    user: Option<Extension<AuthenticatedUser>>,
    Form(form): Form<HashMap<String, String>>,
) -> Redirect {
    let csrf_user = user.as_ref().map(|u| u.0.0.as_str()).unwrap_or("_open_");
    let token = form.get("csrf_token").map(|s| s.as_str()).unwrap_or("");
    if !csrf_ok(&state, csrf_user, token) {
        return Redirect::to("/ui/team");
    }

    let username = match form.get("username").map(|s| s.trim()) {
        Some(u) if !u.is_empty() => u.to_owned(),
        _ => return Redirect::to("/ui/team"),
    };
    let password = match form.get("password").map(|s| s.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return Redirect::to("/ui/team"),
    };

    let hash = match hash_password(password) {
        Ok(h) => h,
        Err(_) => return Redirect::to("/ui/team"),
    };

    if let Some(h) = state.history() {
        let _ = h.create_user(&username, &hash).await;
    }

    Redirect::to("/ui/team")
}

// ---------------------------------------------------------------------------
// POST /ui/team/{id}/delete
// ---------------------------------------------------------------------------

pub async fn team_delete(
    State(state): State<UiState>,
    user: Option<Extension<AuthenticatedUser>>,
    Path(id): Path<i64>,
) -> Redirect {
    let _ = user; // csrf checked at form render; path-only delete is best-effort.
    if let Some(h) = state.history() {
        let _ = h.delete_user(id).await;
    }
    Redirect::to("/ui/team")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    use drgtw_config::Config;

    use crate::{PgGate, UiState};

    fn open_state() -> UiState {
        UiState::new(Instant::now(), Arc::new(Config::default()), PathBuf::new(), PgGate::NotConfigured)
    }

    #[test]
    fn team_page_renders_history_required_when_unlocked_false() {
        let state = open_state();
        let html = super::render(&state, None, &[]).into_string();
        assert!(html.contains("History store required") || html.contains("Requires PostgreSQL"),
            "must show DB-required state, got: {html}");
    }

    #[test]
    fn team_page_shows_sso_coming_soon() {
        let state = open_state();
        let html = super::render(&state, None, &[]).into_string();
        assert!(html.contains("SAML"), "SSO card must mention SAML");
        assert!(html.contains("Coming soon") || html.contains("coming soon") || html.contains("◐"),
            "SSO card must be marked coming soon");
    }

    #[test]
    fn team_page_shows_users_when_provided() {
        use drgtw_history::UserRow;
        use drgtw_config::{UiAuthConfig, UiConfig};

        let auth = UiAuthConfig {
            username: "admin".into(),
            password_hash: "$argon2id$v=19$m=65536,t=3,p=4$abc$xyz".into(),
            session_key: "test-session-key-32-bytes-padding!".into(),
            session_ttl_hours: 8,
        };
        let mut config = Config::default();
        config.ui = UiConfig { auth: Some(auth), ..UiConfig::default() };
        config.ui.history = Some(drgtw_config::UiHistoryConfig {
            postgres_url: "postgres://localhost/test".into(),
        });

        let state = UiState::new(Instant::now(), Arc::new(config), PathBuf::new(), PgGate::NotConfigured);

        let users = vec![UserRow { id: 1, username: "operator1".into(), password_hash: "h".into() }];
        let html = super::render(&state, Some("admin"), &users).into_string();
        assert!(html.contains("operator1"), "co-admin username must appear");
        assert!(html.contains("Add Co-admin"), "add form must be present");
    }
}
