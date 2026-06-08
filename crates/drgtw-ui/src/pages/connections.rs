//! `GET /ui/connections` — glass table of the real upstream connections from
//! config: name, wire format, model count, masked base URL, status.

use maud::{Markup, PreEscaped, html};

use crate::UiState;
use crate::layout::{self, Nav, badge, page_header, shell};

pub fn connections(state: &UiState) -> Markup {
    let cfg = &state.config;
    let unlocked = cfg.ui.history.is_some();

    let body = html! {
        (page_header("Connections", "Upstream provider connections the gateway routes to."))

        @if cfg.connections.is_empty() {
            (layout::empty_state(
                layout::ICON_PLUG, "muted", "No connections",
                "No upstream connections",
                html! { "Add a " code class="font-mono" { "[[connections]]" } " block to drgtw.toml to route traffic to a provider." }
            ))
        } @else {
            div class="glass rise overflow-hidden" style="--i:1" {
                div class="overflow-x-auto" {
                    table class="w-full text-sm" {
                        thead {
                            tr class="text-left text-[11px] uppercase tracking-wide text-muted-foreground border-b border-border" {
                                th class="font-medium px-5 py-3" { "Name" }
                                th class="font-medium px-5 py-3" { "Format" }
                                th class="font-medium px-5 py-3" { "Base URL" }
                                th class="font-medium px-5 py-3 text-right" { "Models" }
                                th class="font-medium px-5 py-3" { "Status" }
                            }
                        }
                        tbody {
                            @for conn in &cfg.connections {
                                tr class="row-lift border-b border-border/50 last:border-0" {
                                    td class="px-5 py-3.5" {
                                        div class="flex items-center gap-2.5" {
                                            span class="size-7 rounded-md icon-orb grid place-items-center text-primary shrink-0" {
                                                span class="size-3.5 grid place-items-center" { (PreEscaped(layout::ICON_PLUG)) }
                                            }
                                            span class="font-medium" { (conn.name) }
                                        }
                                    }
                                    td class="px-5 py-3.5" { (badge("brand", &format!("{:?}", conn.format))) }
                                    td class="px-5 py-3.5 font-mono text-[12.5px] text-muted-foreground" { (host_only(&conn.base_url)) }
                                    td class="px-5 py-3.5 text-right tnum" { (conn.models.len()) }
                                    td class="px-5 py-3.5" { (badge("ok", "ready")) }
                                }
                            }
                        }
                    }
                }
            }

            // Per-connection model chips.
            div class="grid grid-cols-1 md:grid-cols-2 gap-4 mt-4" {
                @for (i, conn) in cfg.connections.iter().enumerate() {
                    div class="glass lift rise p-5" style=(format!("--i:{}", i + 2)) {
                        div class="flex items-center justify-between mb-3" {
                            div class="font-medium" { (conn.name) }
                            (badge("ok", "ready"))
                        }
                        div class="flex flex-wrap gap-1.5" {
                            @if conn.models.is_empty() {
                                span class="text-xs text-muted-foreground" { "no model allowlist (all)" }
                            } @else {
                                @for m in &conn.models {
                                    span class="font-mono text-[11.5px] rounded-md badge-muted px-2 py-0.5" { (m) }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    shell("Connections", "Connections", Nav::Connections, unlocked, cfg.ui.auth.as_ref().map(|a| a.username.as_str()), body)
}

/// Show scheme://host only — base URLs are not secret, but trimming the path
/// keeps the table tidy.
fn host_only(url: &str) -> String {
    if let Some(rest) = url.split("://").nth(1) {
        let host = rest.split('/').next().unwrap_or(rest);
        let scheme = url.split("://").next().unwrap_or("https");
        format!("{scheme}://{host}")
    } else {
        url.to_owned()
    }
}
