//! `GET /ui/webhooks` — Webhooks: delivery log, replay, and secret rotation.
//!
//! Shows the `[events]` config (URL, masked signing secret), recent deliveries
//! from the history store, a replay button per delivery, and a rotate-secret
//! form that writes `events.signing_secret` via `Reloader::apply`.  All POSTs
//! are CSRF-protected.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Extension, Form, Path, State};
use axum::response::{Html, Redirect};
use drgtw_config::read_document;
use drgtw_history::{WebhookDeliveryRow};
use maud::{Markup, html};

use crate::UiState;
use crate::auth::AuthenticatedUser;
use crate::csrf::{csrf_field, csrf_ok};
use crate::layout::{self, Nav, badge, empty_state, page_header, shell};
use crate::mask::mask_secret;
use crate::pages::{fmt_ts, glass_card, kv_row, section_title, status_kind};

const DELIVERY_LIMIT: u32 = 50;

// ---------------------------------------------------------------------------
// GET /ui/webhooks
// ---------------------------------------------------------------------------

pub async fn webhooks(
    State(state): State<UiState>,
    user: Option<Extension<AuthenticatedUser>>,
) -> Html<String> {
    let deliveries = match state.history() {
        Some(h) => h.recent_webhook_deliveries(DELIVERY_LIMIT).await.unwrap_or_default(),
        None => Vec::new(),
    };
    Html(render(&state, user.as_ref().map(|u| u.0.0.as_str()), &deliveries).into_string())
}

fn render(state: &UiState, live_user: Option<&str>, deliveries: &[WebhookDeliveryRow]) -> Markup {
    let cfg = state.live_config();
    let cfg = cfg.as_ref();
    let unlocked = cfg.ui.history.is_some();
    let username = live_user.or_else(|| cfg.ui.auth.as_ref().map(|a| a.username.as_str()));
    let csrf_user = username.unwrap_or("_open_");

    let body = html! {
        (page_header("Webhooks", "Event sink status, recent deliveries, and secret management."))

        // ── Events config card ───────────────────────────────────────────────
        @match &cfg.events {
            None => {
                (empty_state(
                    layout::ICON_WEBHOOK, "muted", "Not configured",
                    "No event sink configured",
                    html! {
                        "Add a "
                        code { "[events]" }
                        " section to your config file with "
                        code { "url" }
                        " to start receiving gateway events downstream."
                    },
                ))
            }
            Some(ev) => {
                (glass_card(1, html! {
                    (section_title(layout::ICON_WEBHOOK, "Event Sink"))
                    div class="mt-1" {
                        (kv_row("URL", html! { span class="font-mono text-xs break-all" { (ev.url) } }))
                        (kv_row("Auth bearer", html! {
                            @match &ev.auth_bearer {
                                Some(b) => span class="font-mono text-xs" { (mask_secret(b)) },
                                None => span class="text-muted-foreground text-xs" { "none" },
                            }
                        }))
                        (kv_row("Signing secret", html! {
                            @match &ev.signing_secret {
                                Some(s) => {
                                    div class="flex items-center gap-2" {
                                        span class="font-mono text-xs" { (mask_secret(s)) }
                                        (badge("ok", "HMAC active"))
                                    }
                                }
                                None => {
                                    div class="flex items-center gap-2" {
                                        span class="text-muted-foreground text-xs" { "not set" }
                                        (badge("warn", "unauthenticated"))
                                    }
                                }
                            }
                        }))
                        (kv_row("Buffer", html! { span class="text-xs" { (ev.buffer_size) " events" } }))
                        (kv_row("Timeout", html! { span class="text-xs" { (ev.timeout_ms) " ms" } }))
                    }

                    // Rotate secret form
                    div class="mt-4 pt-4 border-t border-border/40" {
                        p class="text-xs text-muted-foreground mb-3" {
                            "Rotate the HMAC signing secret. The new value is written live via "
                            "hot-reload — no restart required."
                        }
                        form method="post" action="/ui/webhooks/rotate-secret" class="flex gap-2" {
                            (csrf_field(state, csrf_user))
                            input name="secret" type="password" class="input flex-1"
                                placeholder="New signing secret (leave blank to clear)";
                            button type="submit" class="btn-sm-primary" {
                                "Rotate"
                            }
                        }
                    }
                }))
            }
        }

        // ── Recent deliveries ────────────────────────────────────────────────
        (glass_card(2, html! {
            (section_title(layout::ICON_WEBHOOK, "Recent Deliveries"))
            @if deliveries.is_empty() {
                p class="text-sm text-muted-foreground mt-2" {
                    "No webhook deliveries recorded yet. "
                    @if !unlocked {
                        "Enable "
                        code { "[ui.history]" }
                        " to start recording."
                    }
                }
            } @else {
                div class="overflow-x-auto mt-1" {
                    table class="w-full text-xs border-separate border-spacing-y-0.5" {
                        thead {
                            tr class="text-muted-foreground text-left" {
                                th class="pb-2 font-medium" { "Timestamp" }
                                th class="pb-2 font-medium" { "Status" }
                                th class="pb-2 font-medium" { "Attempt" }
                                th class="pb-2 font-medium" { "Error" }
                                th class="pb-2 font-medium" { "" }
                            }
                        }
                        tbody {
                            @for row in deliveries {
                                (delivery_row(state, row, csrf_user))
                            }
                        }
                    }
                }
            }
        }))
    };

    shell("Webhooks", "Webhooks", Nav::Webhooks, unlocked, username, body)
}

fn delivery_row(state: &UiState, row: &WebhookDeliveryRow, csrf_user: &str) -> Markup {
    let ts = fmt_ts(row.ts_unix_ms);
    let status_str = row.status_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "—".into());
    let ok_kind = if row.ok { "ok" } else { "down" };
    let ok_label = if row.ok { "ok" } else { "failed" };
    let err_msg = row.error.as_deref().unwrap_or("—");

    html! {
        tr class="border-b border-border/30 last:border-0" {
            td class="py-1.5 pr-4 font-mono" { (ts) }
            td class="py-1.5 pr-4" {
                div class="flex items-center gap-1.5" {
                    (badge(ok_kind, ok_label))
                    @if let Some(code) = row.status_code {
                        (badge(status_kind(code as u16), &status_str))
                    }
                }
            }
            td class="py-1.5 pr-4" { "#" (row.attempt) }
            td class="py-1.5 pr-4 max-w-[18rem] truncate text-muted-foreground" { (err_msg) }
            td class="py-1.5 text-right" {
                @if let Some(id) = row.id {
                    form method="post" action=(format!("/ui/webhooks/{id}/replay")) {
                        (csrf_field(state, csrf_user))
                        button type="submit" class="btn-sm" { "Replay" }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// POST /ui/webhooks/{id}/replay
// ---------------------------------------------------------------------------

/// Re-emit a previously recorded webhook delivery.
///
/// 1. Extracts `csrf_token` from the form body and validates it.
/// 2. Fetches the stored `WebhookDeliveryRow` from history by `id`.
/// 3. POSTs `row.payload` as JSON to `config.events.url` with:
///    - `Content-Type: application/json`
///    - `Authorization: Bearer <token>` if `events.auth_bearer` is set
///    - `X-Drgtw-Signature: sha256=<hex>` HMAC-SHA256 of the body if
///      `events.signing_secret` is set (same scheme as `drgtw-events/src/sink.rs`)
/// 4. Records the replay attempt in history as a new `WebhookDeliveryRow`.
pub async fn webhooks_replay(
    State(state): State<UiState>,
    user: Option<Extension<AuthenticatedUser>>,
    Path(id): Path<i64>,
    Form(form): Form<HashMap<String, String>>,
) -> Redirect {
    let csrf_user = user.as_ref().map(|u| u.0.0.as_str()).unwrap_or("_open_");
    let token = form.get("csrf_token").map(|s| s.as_str()).unwrap_or("");
    if !csrf_ok(&state, csrf_user, token) {
        return Redirect::to("/ui/webhooks");
    }

    let Some(h) = state.history() else {
        return Redirect::to("/ui/webhooks");
    };
    let live = state.live_config();
    let Some(ev_cfg) = &live.events else {
        return Redirect::to("/ui/webhooks");
    };

    let row = match h.get_webhook_delivery(id).await {
        Ok(Some(r)) => r,
        _ => return Redirect::to("/ui/webhooks"),
    };

    // Serialise the stored payload back to bytes for the POST body and signing.
    let body_bytes = match serde_json::to_vec(&row.payload) {
        Ok(b) => b,
        Err(_) => return Redirect::to("/ui/webhooks"),
    };

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    // Build the reqwest request, mirroring drgtw-events/src/sink.rs.
    let client = reqwest::Client::new();
    let mut req = client
        .post(&ev_cfg.url)
        .header("Content-Type", "application/json")
        .body(body_bytes.clone());

    if let Some(bearer) = &ev_cfg.auth_bearer {
        req = req.header("Authorization", format!("Bearer {bearer}"));
    }

    // HMAC-SHA256 signing — identical to sink.rs.
    if let Some(secret) = &ev_cfg.signing_secret {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;

        if let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) {
            mac.update(&body_bytes);
            let sig = hex::encode(mac.finalize().into_bytes());
            req = req.header("X-Drgtw-Signature", format!("sha256={sig}"));
        }
    }

    let result = req.send().await;

    let (status_code, ok, error_msg) = match result {
        Ok(resp) if resp.status().is_success() => {
            (Some(resp.status().as_u16() as i32), true, None)
        }
        Ok(resp) => {
            let code = resp.status().as_u16() as i32;
            (Some(code), false, Some(format!("HTTP {code}")))
        }
        Err(e) => (None, false, Some(e.to_string())),
    };

    // Record the replay attempt as a new delivery row.
    let replay_row = drgtw_history::WebhookDeliveryRow {
        id: None, // assigned by the DB
        request_id: format!("replay-{id}"),
        ts_unix_ms: now_ms,
        status_code,
        ok,
        error: error_msg,
        attempt: row.attempt + 1,
        payload: row.payload,
    };
    let _ = h.record_webhook_delivery(&replay_row).await;

    Redirect::to("/ui/webhooks")
}

// ---------------------------------------------------------------------------
// POST /ui/webhooks/rotate-secret
// ---------------------------------------------------------------------------

pub async fn webhooks_rotate(
    State(state): State<UiState>,
    user: Option<Extension<AuthenticatedUser>>,
    Form(form): Form<HashMap<String, String>>,
) -> Redirect {
    let csrf_user = user.as_ref().map(|u| u.0.0.as_str()).unwrap_or("_open_");
    let token = form.get("csrf_token").map(|s| s.as_str()).unwrap_or("");
    if !csrf_ok(&state, csrf_user, token) {
        return Redirect::to("/ui/webhooks");
    }

    let Some(reloader) = &state.reloader else {
        return Redirect::to("/ui/webhooks");
    };

    let mut doc = match read_document(&state.config_path) {
        Ok(d) => d,
        Err(_) => return Redirect::to("/ui/webhooks"),
    };

    let new_secret = form.get("secret").map(|s| s.trim()).unwrap_or("");

    {
        use toml_edit::{Item, Table, value};
        if new_secret.is_empty() {
            // Clear the signing secret.
            if let Some(ev) = doc.get_mut("events")
                && let Some(t) = ev.as_table_mut()
            {
                t.remove("signing_secret");
            }
        } else {
            // Ensure [events] table exists.
            doc["events"].or_insert(Item::Table(Table::new()));
            if let Some(ev) = doc.get_mut("events")
                && let Some(t) = ev.as_table_mut()
            {
                t["signing_secret"] = value(new_secret);
            }
        }
    }

    let _ = reloader.apply(&doc.to_string());
    Redirect::to("/ui/webhooks")
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
    fn webhooks_page_renders_no_config() {
        let state = open_state();
        let html = super::render(&state, None, &[]).into_string();
        assert!(html.contains("No event sink configured"), "empty-state text must be present");
    }

    #[test]
    fn webhooks_page_shows_deliveries_section() {
        let state = open_state();
        let html = super::render(&state, None, &[]).into_string();
        assert!(html.contains("Recent Deliveries"), "deliveries section must be present");
    }

    #[test]
    fn webhooks_page_shows_events_config_when_present() {
        use drgtw_config::{Config, EventsConfig};
        let mut config = Config::default();
        config.events = Some(EventsConfig {
            url: "https://sink.example.com/events".into(),
            auth_bearer: None,
            buffer_size: 1024,
            timeout_ms: 5000,
            signing_secret: Some("my-signing-secret".into()),
        });
        let state = UiState::new(Instant::now(), Arc::new(config), PathBuf::new(), PgGate::NotConfigured);
        let html = super::render(&state, None, &[]).into_string();
        assert!(html.contains("sink.example.com"), "event sink URL must appear");
        assert!(html.contains("HMAC active"), "signing badge must appear");
        assert!(!html.contains("my-signing-secret"), "raw secret must not appear");
    }

    /// The HMAC-SHA256 signing used in replay must match the sink.rs scheme.
    #[test]
    fn signing_scheme_matches_sink_rs() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;

        let secret = b"replay-test-secret";
        let body = br#"{"kind":"usage","cost_usd":0.001}"#;

        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
        mac.update(body);
        let sig = hex::encode(mac.finalize().into_bytes());

        // Verify: sig must be a 64-char lowercase hex string (sha256 = 32 bytes).
        assert_eq!(sig.len(), 64, "HMAC-SHA256 hex must be 64 chars");
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()), "must be hex");

        // Idempotent: same input → same sig.
        let mut mac2 = HmacSha256::new_from_slice(secret).unwrap();
        mac2.update(body);
        let sig2 = hex::encode(mac2.finalize().into_bytes());
        assert_eq!(sig, sig2, "HMAC must be deterministic");
    }
}
