//! `GET /ui/traces` — a glass table of the 100 most recent request traces from
//! the Postgres history store. Async handler: queries `recent_usage(100)`,
//! renders an empty state when there are no rows (or no store connected).

use axum::extract::State;
use axum::response::Html;
use maud::{Markup, html};

use drgtw_events::UsageEvent;

use crate::pages::{fmt_int, fmt_latency, fmt_ts, status_kind};
use crate::layout::{self, Nav, badge, empty_state, page_header, shell};
use crate::UiState;

pub async fn traces(State(state): State<UiState>) -> Html<String> {
    let rows = match state.history() {
        Some(h) => h.recent_usage(100).await.unwrap_or_default(),
        None => Vec::new(),
    };
    Html(render(&state, &rows).into_string())
}

fn render(state: &UiState, rows: &[UsageEvent]) -> Markup {
    let cfg = &state.config;
    let unlocked = cfg.ui.history.is_some();
    let username = cfg.ui.auth.as_ref().map(|a| a.username.as_str());

    let body = html! {
        (page_header("Traces", "Browse individual request traces from the history store."))

        @if rows.is_empty() {
            (empty_state(
                layout::ICON_ROUTE, "muted", "No traces",
                "No request traces yet",
                html! { "Once the gateway forwards traffic, individual request traces will appear here." }
            ))
        } @else {
            div class="rise grid" style="--i:1" {
              div class="glass overflow-hidden" {
                div class="flex items-center justify-between px-5 py-3.5 border-b border-border" {
                    h3 class="text-base font-semibold" { "Recent traces" }
                    span class="text-xs text-muted-foreground" { (format!("{} requests", rows.len())) }
                }
                div class="overflow-x-auto" {
                    table class="w-full text-sm" {
                        thead {
                            tr class="text-left text-[11px] uppercase tracking-wide text-muted-foreground border-b border-border" {
                                th class="font-medium px-4 py-3" { "Request" }
                                th class="font-medium px-4 py-3" { "Time" }
                                th class="font-medium px-4 py-3" { "Endpoint" }
                                th class="font-medium px-4 py-3" { "Model" }
                                th class="font-medium px-4 py-3" { "Connection" }
                                th class="font-medium px-4 py-3" { "Status" }
                                th class="font-medium px-4 py-3 text-right" { "Latency" }
                                th class="font-medium px-4 py-3 text-right" { "In" }
                                th class="font-medium px-4 py-3 text-right" { "Out" }
                                th class="font-medium px-4 py-3" { "PII" }
                                th class="font-medium px-4 py-3" { "Stream" }
                                th class="font-medium px-4 py-3 text-right" { "Fallbacks" }
                            }
                        }
                        tbody {
                            @for ev in rows {
                                (trace_row(ev))
                            }
                        }
                    }
                }
              }
            }
        }
    };

    shell("Traces", "Traces", Nav::Traces, unlocked, username, body)
}

fn trace_row(ev: &UsageEvent) -> Markup {
    // Truncated, monospaced request id (first 10 chars).
    let short_id: String = ev.request_id.chars().take(10).collect();
    let id_full = ev.request_id.clone();
    html! {
        tr class="row-lift border-b border-border/50 last:border-0" {
            td class="px-4 py-3 font-mono text-[12px] text-muted-foreground" title=(id_full) { (short_id) }
            td class="px-4 py-3 font-mono text-[12px] text-muted-foreground" { (fmt_ts(ev.ts_unix_ms as i64)) }
            td class="px-4 py-3 font-mono text-[12px]" { (ev.endpoint) }
            td class="px-4 py-3 font-mono text-[12px]" { (ev.model) }
            td class="px-4 py-3 font-mono text-[12px] text-muted-foreground" { (ev.connection) }
            td class="px-4 py-3" { (badge(status_kind(ev.status), &ev.status.to_string())) }
            td class="px-4 py-3 text-right tnum text-muted-foreground" { (fmt_latency(ev.latency_ms as f64)) }
            td class="px-4 py-3 text-right tnum" { (fmt_int(ev.input_tokens.unwrap_or(0) as i64)) }
            td class="px-4 py-3 text-right tnum" { (fmt_int(ev.output_tokens.unwrap_or(0) as i64)) }
            td class="px-4 py-3" {
                @if ev.pii { (badge("warn", "PII")) } @else { span class="text-muted-foreground" { "—" } }
            }
            td class="px-4 py-3" {
                @if ev.streamed { (badge("brand", "stream")) } @else { span class="text-muted-foreground" { "—" } }
            }
            td class="px-4 py-3 text-right tnum text-muted-foreground" { (ev.fallback_attempts) }
        }
    }
}
