//! `GET /ui/audit` — a glass table of the 100 most recent audit-log entries
//! from the Postgres history store: who did what, when, and the detail payload.
//! Async handler: queries `recent_audit(100)`, empty state when none.

use axum::extract::State;
use axum::response::Html;
use maud::{Markup, html};

use drgtw_history::AuditEntry;

use crate::pages::fmt_ts;
use crate::layout::{self, Nav, badge, empty_state, page_header, shell};
use crate::UiState;

pub async fn audit_log(State(state): State<UiState>) -> Html<String> {
    let rows = match state.history() {
        Some(h) => h.recent_audit(100).await.unwrap_or_default(),
        None => Vec::new(),
    };
    Html(render(&state, &rows).into_string())
}

fn render(state: &UiState, rows: &[AuditEntry]) -> Markup {
    let cfg = &state.config;
    let unlocked = cfg.ui.history.is_some();
    let username = cfg.ui.auth.as_ref().map(|a| a.username.as_str());

    let body = html! {
        (page_header("Audit Log", "Every config and key change — who and when."))

        @if rows.is_empty() {
            (empty_state(
                layout::ICON_SCROLL, "muted", "No audit entries",
                "No audit activity yet",
                html! { "Console actions — logins, config saves, key changes — will be recorded here." }
            ))
        } @else {
            div class="rise grid" style="--i:1" {
              div class="glass overflow-hidden" {
                div class="flex items-center justify-between px-5 py-3.5 border-b border-border" {
                    h3 class="text-base font-semibold" { "Recent activity" }
                    span class="text-xs text-muted-foreground" { (format!("{} entries", rows.len())) }
                }
                div class="overflow-x-auto" {
                    table class="w-full text-sm" {
                        thead {
                            tr class="text-left text-[11px] uppercase tracking-wide text-muted-foreground border-b border-border" {
                                th class="font-medium px-5 py-3" { "Time" }
                                th class="font-medium px-5 py-3" { "Actor" }
                                th class="font-medium px-5 py-3" { "Action" }
                                th class="font-medium px-5 py-3" { "Target" }
                                th class="font-medium px-5 py-3" { "Detail" }
                            }
                        }
                        tbody {
                            @for e in rows {
                                (audit_row(e))
                            }
                        }
                    }
                }
              }
            }
        }
    };

    shell("Audit Log", "Audit Log", Nav::AuditLog, unlocked, username, body)
}

/// Choose a badge kind from the action verb (failures red, the rest brand).
fn action_kind(action: &str) -> &'static str {
    if action.contains("failure") || action.contains("delete") {
        "down"
    } else if action.contains("login") || action.contains("logout") {
        "ok"
    } else {
        "brand"
    }
}

fn audit_row(e: &AuditEntry) -> Markup {
    // Compact detail: a JSON object renders as `k=v` pairs; anything else is
    // shown verbatim. Empty objects render as a dash.
    let detail = match &e.detail {
        serde_json::Value::Object(map) if map.is_empty() => "—".to_owned(),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}={}", compact_value(v)))
            .collect::<Vec<_>>()
            .join(", "),
        serde_json::Value::Null => "—".to_owned(),
        other => compact_value(other),
    };

    html! {
        tr class="row-lift border-b border-border/50 last:border-0" {
            td class="px-5 py-3 font-mono text-[12px] text-muted-foreground" { (fmt_ts(e.ts_unix_ms)) }
            td class="px-5 py-3 font-mono text-[12.5px]" { (e.actor) }
            td class="px-5 py-3" { (badge(action_kind(&e.action), &e.action)) }
            td class="px-5 py-3 font-mono text-[12.5px] text-muted-foreground" { (e.target) }
            td class="px-5 py-3 font-mono text-[12px] text-muted-foreground break-all" { (detail) }
        }
    }
}

/// Render a scalar JSON value without surrounding quotes; arrays/objects fall
/// back to compact JSON.
fn compact_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".to_owned(),
        other => other.to_string(),
    }
}
