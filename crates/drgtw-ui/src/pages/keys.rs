//! `GET /ui/keys` — glass table of virtual keys: masked key, allowed
//! connections, and model allowlist (or "all"). Keys are masked via
//! [`crate::mask`] and never rendered raw.

use maud::{Markup, PreEscaped, html};

use crate::UiState;
use crate::layout::{self, Nav, badge, page_header, shell};
use crate::mask::mask_secret;

pub fn virtual_keys(state: &UiState) -> Markup {
    let cfg = &state.config;
    let unlocked = cfg.ui.history.is_some();

    let body = html! {
        div class="flex items-start justify-between gap-4" {
            (page_header("Virtual Keys", "Client-facing keys that map to upstream connections and model allowlists."))
            div class="mt-1.5 shrink-0" {
                button type="button" class="btn-brand rounded-lg px-3.5 py-2 text-sm font-medium inline-flex items-center gap-2" {
                    span class="size-4 grid place-items-center" { (PreEscaped(layout::ICON_KEY)) } "New key"
                    span class="badge-muted rounded px-1.5 py-0.5 text-[10px] ml-1" { "◐ soon" }
                }
            }
        }

        @if cfg.virtual_keys.is_empty() {
            (layout::empty_state(
                layout::ICON_KEY, "muted", "No virtual keys",
                "No virtual keys configured",
                html! { "Add a " code class="font-mono" { "[[virtual_keys]]" } " block to drgtw.toml. Keys must start with " code class="font-mono" { "sk-drgtw-" } "." }
            ))
        } @else {
            div class="glass rise overflow-hidden" style="--i:1" {
                div class="overflow-x-auto" {
                    table class="w-full text-sm" {
                        thead {
                            tr class="text-left text-[11px] uppercase tracking-wide text-muted-foreground border-b border-border" {
                                th class="font-medium px-5 py-3" { "Key" }
                                th class="font-medium px-5 py-3" { "Connections" }
                                th class="font-medium px-5 py-3" { "Models" }
                                th class="font-medium px-5 py-3" { "Rate limit" }
                                th class="font-medium px-5 py-3" { "Status" }
                            }
                        }
                        tbody {
                            @for vk in &cfg.virtual_keys {
                                tr class="row-lift border-b border-border/50 last:border-0" {
                                    td class="px-5 py-3.5" {
                                        div class="flex items-center gap-2.5" {
                                            span class="size-7 rounded-md icon-orb grid place-items-center text-primary shrink-0" {
                                                span class="size-3.5 grid place-items-center" { (PreEscaped(layout::ICON_KEY)) }
                                            }
                                            span class="font-mono text-[12.5px]" { (mask_secret(&vk.key)) }
                                        }
                                    }
                                    td class="px-5 py-3.5" {
                                        div class="flex flex-wrap gap-1.5" {
                                            @for c in &vk.connections {
                                                span class="font-mono text-[11.5px] rounded-md badge-muted px-2 py-0.5" { (c) }
                                            }
                                        }
                                    }
                                    td class="px-5 py-3.5" {
                                        @match &vk.models {
                                            Some(m) => span class="font-mono text-[12.5px]" { (m.join(", ")) },
                                            None => (badge("brand", "all models")),
                                        }
                                    }
                                    td class="px-5 py-3.5 text-muted-foreground" {
                                        @match &vk.rate_limit {
                                            Some(_) => span class="text-xs" { "configured" },
                                            None => span class="text-xs text-muted-foreground" { "—" },
                                        }
                                    }
                                    td class="px-5 py-3.5" { (badge("ok", "active")) }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    shell("Virtual Keys", "Virtual Keys", Nav::VirtualKeys, unlocked, cfg.ui.auth.as_ref().map(|a| a.username.as_str()), body)
}
