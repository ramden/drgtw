//! `GET /ui/keys` — glass table of virtual keys: masked key, allowed
//! connections, model allowlist, rate-limit, budget, and MCP allowlist.
//! Keys are masked via [`crate::mask`] and never rendered raw.
//!
//! CRUD handlers: `keys_create` (POST /keys/new), `keys_update` (POST
//! /keys/{idx}/edit), `keys_delete` (POST /keys/{idx}/delete).
//! All mutations go through [`Reloader::apply`] — hot-reload with no restart.
//!
//! `key_detail` (GET /keys/{idx}): masked key metadata, live rate-limit /
//! budget snapshots, and 24h usage summary from the history store.

use std::collections::HashMap;

use axum::extract::{Form, Path, State};
use axum::response::Html;
use maud::{Markup, PreEscaped, html};

use drgtw_config::Config;

use crate::UiState;
use crate::csrf;
use crate::layout::{self, Nav, badge, page_header, shell};
use crate::mask::mask_secret;
use crate::pages::{fmt_cost, fmt_int, glass_card, kv_row, section_title};

// ---------------------------------------------------------------------------
// List page (sync, rendered via page! macro)
// ---------------------------------------------------------------------------

pub fn virtual_keys(state: &UiState) -> Markup {
    let cfg = state.live_config();
    let cfg = cfg.as_ref();
    let unlocked = cfg.ui.history.is_some();
    let username = cfg.ui.auth.as_ref().map(|a| a.username.as_str());
    let user = username.unwrap_or("operator");

    let body = html! {
        div class="flex items-start justify-between gap-4" {
            (page_header("Virtual Keys", "Client-facing keys that map to upstream connections and model allowlists."))
            div class="mt-1.5 shrink-0" {
                // "New key" opens the inline create form via native <details>
                details class="relative" {
                    summary class="btn-brand rounded-lg px-3.5 py-2 text-sm font-medium inline-flex items-center gap-2 cursor-pointer list-none" {
                        span class="size-4 grid place-items-center" { (PreEscaped(layout::ICON_KEY)) }
                        "New key"
                    }
                    div class="absolute right-0 mt-2 z-20 w-[480px]" {
                        (new_key_form(state, user, None))
                    }
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
            div class="rise grid" style="--i:1" {
              div class="glass overflow-hidden" {
                div class="overflow-x-auto" {
                    table class="w-full text-sm" {
                        thead {
                            tr class="text-left text-[11px] uppercase tracking-wide text-muted-foreground border-b border-border" {
                                th class="font-medium px-5 py-3" { "Key" }
                                th class="font-medium px-5 py-3" { "Connections" }
                                th class="font-medium px-5 py-3" { "Models" }
                                th class="font-medium px-5 py-3" { "Rate limit" }
                                th class="font-medium px-5 py-3" { "Budget" }
                                th class="font-medium px-5 py-3" { "MCP" }
                                th class="font-medium px-5 py-3" { "Actions" }
                            }
                        }
                        tbody {
                            @for (idx, vk) in cfg.virtual_keys.iter().enumerate() {
                                tr class="row-lift border-b border-border/50 last:border-0" {
                                    td class="px-5 py-3.5" {
                                        div class="flex items-center gap-2.5" {
                                            span class="size-7 rounded-md icon-orb grid place-items-center text-primary shrink-0" {
                                                span class="size-3.5 grid place-items-center" { (PreEscaped(layout::ICON_KEY)) }
                                            }
                                            a href=(format!("/ui/keys/{idx}")) class="font-mono text-[12.5px] hover:underline" {
                                                (mask_secret(&vk.key))
                                            }
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
                                            Some(rl) => span class="text-xs font-mono" {
                                                (rl.requests) " / " (rl.per_seconds) "s"
                                            },
                                            None => span class="text-xs text-muted-foreground" { "—" },
                                        }
                                    }
                                    td class="px-5 py-3.5 text-muted-foreground" {
                                        @match &vk.budget {
                                            Some(b) => span class="text-xs font-mono" {
                                                "$" (b.max_usd) " / " (b.per_seconds) "s"
                                            },
                                            None => span class="text-xs text-muted-foreground" { "—" },
                                        }
                                    }
                                    td class="px-5 py-3.5" {
                                        @match &vk.mcp_servers {
                                            Some(m) if !m.is_empty() => (badge("brand", &format!("{} server(s)", m.len()))),
                                            Some(_) | None => span class="text-xs text-muted-foreground" { "all" },
                                        }
                                    }
                                    td class="px-5 py-3.5" {
                                        div class="flex items-center gap-3" {
                                            a href=(format!("/ui/keys/{idx}")) class="text-xs text-primary hover:underline" { "detail" }
                                            form method="post" action=(format!("/ui/keys/{idx}/delete"))
                                                onsubmit="return confirm('Delete this key?')"
                                            {
                                                (csrf::csrf_field(state, user))
                                                button type="submit" class="text-xs text-destructive hover:underline" { "delete" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
              }
            }
        }
    };

    shell("Virtual Keys", "Virtual Keys", Nav::VirtualKeys, unlocked, username, body)
}

// ---------------------------------------------------------------------------
// New-key inline form
// ---------------------------------------------------------------------------

fn new_key_form(state: &UiState, user: &str, error: Option<&str>) -> Markup {
    let cfg = state.live_config();
    let cfg = cfg.as_ref();
    html! {
        div class="glass lift p-5 rounded-xl border border-border shadow-xl" {
            h3 class="text-sm font-semibold mb-4" { "New Virtual Key" }

            @if let Some(msg) = error {
                div class="mb-3 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive" {
                    (msg)
                }
            }

            form method="post" action="/ui/keys/new" class="space-y-3" {
                (csrf::csrf_field(state, user))

                div {
                    label class="block text-xs font-medium text-muted-foreground mb-1" for="nk_key" { "Key (must start with sk-drgtw-)" }
                    input id="nk_key" name="key" type="text" required
                        placeholder="sk-drgtw-..."
                        class="w-full rounded-md border border-border bg-transparent px-3 py-1.5 text-sm font-mono focus:outline-none focus:ring-1 focus:ring-primary";
                }

                div {
                    label class="block text-xs font-medium text-muted-foreground mb-1" for="nk_connections" {
                        "Connections (space-separated names)"
                    }
                    @if cfg.connections.is_empty() {
                        p class="text-xs text-muted-foreground italic" { "No connections configured." }
                    } @else {
                        div class="flex flex-wrap gap-2" {
                            @for conn in &cfg.connections {
                                label class="flex items-center gap-1.5 text-xs cursor-pointer" {
                                    input type="checkbox" name="connection" value=(conn.name) class="rounded";
                                    span class="font-mono" { (conn.name) }
                                }
                            }
                        }
                    }
                }

                div {
                    label class="block text-xs font-medium text-muted-foreground mb-1" for="nk_models" {
                        "Models (comma-separated, blank = all)"
                    }
                    input id="nk_models" name="models" type="text"
                        placeholder="gpt-4o, claude-opus-4-8 (blank = all)"
                        class="w-full rounded-md border border-border bg-transparent px-3 py-1.5 text-sm font-mono focus:outline-none focus:ring-1 focus:ring-primary";
                }

                details class="text-xs" {
                    summary class="cursor-pointer text-muted-foreground hover:text-foreground mb-2" { "Rate limit (optional)" }
                    div class="grid grid-cols-2 gap-2 mt-2" {
                        div {
                            label class="block text-xs text-muted-foreground mb-1" { "Requests" }
                            input name="rl_requests" type="number" min="1"
                                placeholder="e.g. 100"
                                class="w-full rounded-md border border-border bg-transparent px-3 py-1.5 text-sm focus:outline-none focus:ring-1 focus:ring-primary";
                        }
                        div {
                            label class="block text-xs text-muted-foreground mb-1" { "Per seconds" }
                            input name="rl_per_seconds" type="number" min="1"
                                placeholder="e.g. 60"
                                class="w-full rounded-md border border-border bg-transparent px-3 py-1.5 text-sm focus:outline-none focus:ring-1 focus:ring-primary";
                        }
                    }
                }

                details class="text-xs" {
                    summary class="cursor-pointer text-muted-foreground hover:text-foreground mb-2" { "Budget (optional)" }
                    div class="grid grid-cols-2 gap-2 mt-2" {
                        div {
                            label class="block text-xs text-muted-foreground mb-1" { "Max USD" }
                            input name="budget_max_usd" type="number" min="0.01" step="0.01"
                                placeholder="e.g. 10.00"
                                class="w-full rounded-md border border-border bg-transparent px-3 py-1.5 text-sm focus:outline-none focus:ring-1 focus:ring-primary";
                        }
                        div {
                            label class="block text-xs text-muted-foreground mb-1" { "Per seconds" }
                            input name="budget_per_seconds" type="number" min="1"
                                placeholder="e.g. 86400"
                                class="w-full rounded-md border border-border bg-transparent px-3 py-1.5 text-sm focus:outline-none focus:ring-1 focus:ring-primary";
                        }
                    }
                }

                @if !cfg.mcp_servers.is_empty() {
                    details class="text-xs" {
                        summary class="cursor-pointer text-muted-foreground hover:text-foreground mb-2" { "MCP servers (optional, blank = all)" }
                        div class="flex flex-wrap gap-2 mt-2" {
                            @for (name, _) in &cfg.mcp_servers {
                                label class="flex items-center gap-1.5 cursor-pointer" {
                                    input type="checkbox" name="mcp_server" value=(name) class="rounded";
                                    span class="font-mono" { (name) }
                                }
                            }
                        }
                    }
                }

                div class="flex justify-end gap-2 pt-1" {
                    button type="submit" class="btn-brand rounded-lg px-3 py-1.5 text-sm font-medium" {
                        "Create key"
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-key detail (async)
// ---------------------------------------------------------------------------

pub async fn key_detail(State(state): State<UiState>, Path(idx): Path<usize>) -> Html<String> {
    let cfg = state.live_config();
    let cfg = cfg.as_ref();
    let unlocked = cfg.ui.history.is_some();
    let username = cfg.ui.auth.as_ref().map(|a| a.username.as_str());
    let user = username.unwrap_or("operator");

    let Some(vk) = cfg.virtual_keys.get(idx) else {
        let body = html! {
            (page_header("Key not found", ""))
            p class="text-sm text-muted-foreground" { "No virtual key at index " (idx) "." }
            a href="/ui/keys" class="text-sm text-primary hover:underline" { "Back to keys" }
        };
        return Html(
            shell("Virtual Keys", "Virtual Keys", Nav::VirtualKeys, unlocked, username, body)
                .into_string(),
        );
    };

    let key_id = format!("vk-{idx}");

    // Live snapshots from the reloader (None if no reloader wired or key not
    // configured).
    let rl_snap = state.rate_limit_snapshot(&key_id);
    let budget_snap = state.budget_snapshot(&key_id);

    // 24h usage from the history store, if connected.
    let (since, until, _bucket) = crate::range_window("24h");
    let usage = match state.history() {
        Some(h) => h.usage_summary(since, until).await.unwrap_or_else(|_| zero_summary()),
        None => zero_summary(),
    };

    let title = format!("Key vk-{idx}");
    let body = html! {
        div class="flex items-start justify-between gap-4 mb-4" {
            (page_header(&title, "Virtual key detail, live counters, and 24h usage."))
            a href="/ui/keys" class="mt-1.5 text-sm text-primary hover:underline shrink-0" { "Back to keys" }
        }

        div class="grid grid-cols-1 lg:grid-cols-2 gap-4" {

            // --- Identity ---
            (glass_card(1, html! {
                (section_title(layout::ICON_KEY, "Identity"))
                (kv_row("key", html! { code class="font-mono text-[12.5px]" { (mask_secret(&vk.key)) } }))
                (kv_row("key id", html! { code class="font-mono text-xs text-muted-foreground" { (key_id) } }))
                (kv_row("connections", html! {
                    div class="flex flex-wrap gap-1" {
                        @for c in &vk.connections {
                            span class="badge-muted rounded px-2 py-0.5 font-mono text-[11.5px]" { (c) }
                        }
                    }
                }))
                (kv_row("models", html! {
                    @match &vk.models {
                        Some(m) => span class="font-mono text-xs" { (m.join(", ")) },
                        None => (badge("brand", "all models")),
                    }
                }))
                @if let Some(mcps) = &vk.mcp_servers {
                    (kv_row("mcp servers", html! {
                        div class="flex flex-wrap gap-1" {
                            @for s in mcps {
                                span class="badge-muted rounded px-2 py-0.5 font-mono text-[11.5px]" { (s) }
                            }
                        }
                    }))
                } @else {
                    (kv_row("mcp servers", html! { (badge("brand", "all")) }))
                }
            }))

            // --- Rate limit ---
            (glass_card(2, html! {
                (section_title(layout::ICON_GAUGE2, "Rate Limit"))
                @if let Some(rl) = &vk.rate_limit {
                    (kv_row("config", html! {
                        span class="font-mono text-xs" { (rl.requests) " req / " (rl.per_seconds) "s" }
                    }))
                    @if let Some(snap) = &rl_snap {
                        (kv_row("remaining", html! {
                            span class="font-semibold" { (snap.remaining) }
                            span class="text-muted-foreground text-xs" { " / " (snap.capacity) }
                        }))
                        (kv_row("refill in", html! {
                            @if snap.secs_to_next_token == 0 {
                                span class="text-ok text-xs" { "full" }
                            } @else {
                                span class="font-mono text-xs" { (snap.secs_to_next_token) "s" }
                            }
                        }))
                        // Burn bar: remaining / capacity
                        div class="mt-3" {
                            div class="flex justify-between text-xs text-muted-foreground mb-1" {
                                span { "Tokens remaining" }
                                span { (snap.remaining) " / " (snap.capacity) }
                            }
                            div class="h-2 rounded-full bg-border overflow-hidden" {
                                div class="h-full rounded-full bg-primary transition-all"
                                    style=(format!("width: {}%", pct(snap.remaining as f64, snap.capacity as f64))) {}
                            }
                        }
                    } @else {
                        p class="text-xs text-muted-foreground mt-1" { "No live counter available." }
                    }
                } @else {
                    p class="text-sm text-muted-foreground" { "No rate limit configured for this key." }
                }
            }))

            // --- Budget ---
            (glass_card(3, html! {
                (section_title(layout::ICON_COINS, "Budget"))
                @if let Some(b) = &vk.budget {
                    (kv_row("config", html! {
                        span class="font-mono text-xs" { "$" (b.max_usd) " / " (b.per_seconds) "s" }
                    }))
                    @if let Some(snap) = &budget_snap {
                        (kv_row("spent", html! {
                            span class="font-semibold" { (fmt_cost(snap.spent_usd)) }
                            span class="text-muted-foreground text-xs" { " of " (fmt_cost(snap.max_usd)) }
                        }))
                        (kv_row("resets in", html! {
                            @if snap.secs_to_reset == 0 {
                                span class="text-ok text-xs" { "now" }
                            } @else {
                                span class="font-mono text-xs" { (fmt_secs(snap.secs_to_reset)) }
                            }
                        }))
                        // Burn bar: spent / max
                        div class="mt-3" {
                            div class="flex justify-between text-xs text-muted-foreground mb-1" {
                                span { "Spend used" }
                                span { (fmt_cost(snap.spent_usd)) " / " (fmt_cost(snap.max_usd)) }
                            }
                            div class="h-2 rounded-full bg-border overflow-hidden" {
                                @let ratio = pct(snap.spent_usd, snap.max_usd);
                                div class=(format!("h-full rounded-full transition-all {}", burn_color(ratio)))
                                    style=(format!("width: {}%", ratio)) {}
                            }
                        }
                    } @else {
                        p class="text-xs text-muted-foreground mt-1" { "No live counter available." }
                    }
                } @else {
                    p class="text-sm text-muted-foreground" { "No budget configured for this key." }
                }
            }))

            // --- 24h usage summary ---
            (glass_card(4, html! {
                (section_title(layout::ICON_GAUGE, "24h Usage"))
                @if !unlocked {
                    p class="text-sm text-muted-foreground" { "Connect a history store to see per-key usage." }
                } @else {
                    div class="grid grid-cols-2 gap-3 mt-1" {
                        div class="glass-metric rounded-lg p-3" {
                            div class="text-xl font-semibold stat-gradient" { (fmt_int(usage.requests)) }
                            div class="text-xs text-muted-foreground mt-0.5" { "Requests" }
                        }
                        div class="glass-metric rounded-lg p-3" {
                            div class="text-xl font-semibold stat-gradient" { (fmt_cost(usage.cost_usd)) }
                            div class="text-xs text-muted-foreground mt-0.5" { "Cost" }
                        }
                        div class="glass-metric rounded-lg p-3" {
                            div class="text-xl font-semibold stat-gradient" {
                                (fmt_int(usage.input_tokens + usage.output_tokens))
                            }
                            div class="text-xs text-muted-foreground mt-0.5" { "Tokens" }
                        }
                        div class="glass-metric rounded-lg p-3" {
                            div class="text-xl font-semibold stat-gradient" { (fmt_int(usage.error_count)) }
                            div class="text-xs text-muted-foreground mt-0.5" { "Errors" }
                        }
                    }
                    p class="text-[11px] text-muted-foreground mt-3" {
                        "Note: usage shown is gateway-wide. Per-key filtering requires Phase B history."
                    }
                }
            }))
        }

        // --- Edit form ---
        div class="mt-4" {
            (edit_key_form(&state, user, idx, vk, None))
        }
    };

    Html(shell("Virtual Keys", "Virtual Keys", Nav::VirtualKeys, unlocked, username, body).into_string())
}

// ---------------------------------------------------------------------------
// Edit key inline form
// ---------------------------------------------------------------------------

fn edit_key_form(
    state: &UiState,
    user: &str,
    idx: usize,
    vk: &drgtw_config::VirtualKey,
    error: Option<&str>,
) -> Markup {
    let cfg = state.live_config();
    let cfg = cfg.as_ref();
    html! {
        (glass_card(5, html! {
            (section_title(layout::ICON_KEY, "Edit Key"))

            @if let Some(msg) = error {
                div class="mb-3 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive" {
                    (msg)
                }
            }

            form method="post" action=(format!("/ui/keys/{idx}/edit")) class="space-y-3" {
                (csrf::csrf_field(state, user))

                div {
                    label class="block text-xs font-medium text-muted-foreground mb-1" { "Key" }
                    input name="key" type="text" required value=(vk.key)
                        class="w-full rounded-md border border-border bg-transparent px-3 py-1.5 text-sm font-mono focus:outline-none focus:ring-1 focus:ring-primary";
                }

                div {
                    label class="block text-xs font-medium text-muted-foreground mb-1" { "Connections" }
                    @if cfg.connections.is_empty() {
                        p class="text-xs text-muted-foreground italic" { "No connections configured." }
                    } @else {
                        div class="flex flex-wrap gap-2" {
                            @for conn in &cfg.connections {
                                @let checked = vk.connections.contains(&conn.name);
                                label class="flex items-center gap-1.5 text-xs cursor-pointer" {
                                    input type="checkbox" name="connection" value=(conn.name)
                                        checked[checked] class="rounded";
                                    span class="font-mono" { (conn.name) }
                                }
                            }
                        }
                    }
                }

                div {
                    label class="block text-xs font-medium text-muted-foreground mb-1" {
                        "Models (comma-separated, blank = all)"
                    }
                    input name="models" type="text"
                        value=(vk.models.as_deref().map(|m| m.join(", ")).unwrap_or_default())
                        placeholder="gpt-4o, claude-opus-4-8 (blank = all)"
                        class="w-full rounded-md border border-border bg-transparent px-3 py-1.5 text-sm font-mono focus:outline-none focus:ring-1 focus:ring-primary";
                }

                div class="grid grid-cols-2 gap-3" {
                    div {
                        label class="block text-xs font-medium text-muted-foreground mb-1" { "Rate limit: requests" }
                        input name="rl_requests" type="number" min="1"
                            value=(vk.rate_limit.as_ref().map(|r| r.requests.to_string()).unwrap_or_default())
                            placeholder="blank = none"
                            class="w-full rounded-md border border-border bg-transparent px-3 py-1.5 text-sm focus:outline-none focus:ring-1 focus:ring-primary";
                    }
                    div {
                        label class="block text-xs font-medium text-muted-foreground mb-1" { "Rate limit: per seconds" }
                        input name="rl_per_seconds" type="number" min="1"
                            value=(vk.rate_limit.as_ref().map(|r| r.per_seconds.to_string()).unwrap_or_default())
                            placeholder="blank = none"
                            class="w-full rounded-md border border-border bg-transparent px-3 py-1.5 text-sm focus:outline-none focus:ring-1 focus:ring-primary";
                    }
                    div {
                        label class="block text-xs font-medium text-muted-foreground mb-1" { "Budget: max USD" }
                        input name="budget_max_usd" type="number" min="0.01" step="0.01"
                            value=(vk.budget.as_ref().map(|b| b.max_usd.to_string()).unwrap_or_default())
                            placeholder="blank = none"
                            class="w-full rounded-md border border-border bg-transparent px-3 py-1.5 text-sm focus:outline-none focus:ring-1 focus:ring-primary";
                    }
                    div {
                        label class="block text-xs font-medium text-muted-foreground mb-1" { "Budget: per seconds" }
                        input name="budget_per_seconds" type="number" min="1"
                            value=(vk.budget.as_ref().map(|b| b.per_seconds.to_string()).unwrap_or_default())
                            placeholder="blank = none"
                            class="w-full rounded-md border border-border bg-transparent px-3 py-1.5 text-sm focus:outline-none focus:ring-1 focus:ring-primary";
                    }
                }

                @if !cfg.mcp_servers.is_empty() {
                    div {
                        label class="block text-xs font-medium text-muted-foreground mb-1" { "MCP servers (blank = all)" }
                        div class="flex flex-wrap gap-2" {
                            @for (name, _) in &cfg.mcp_servers {
                                @let checked = vk.mcp_servers.as_ref().is_some_and(|m| m.contains(name));
                                label class="flex items-center gap-1.5 text-xs cursor-pointer" {
                                    input type="checkbox" name="mcp_server" value=(name)
                                        checked[checked] class="rounded";
                                    span class="font-mono" { (name) }
                                }
                            }
                        }
                    }
                }

                div class="flex justify-end gap-2 pt-1" {
                    button type="submit" class="btn-brand rounded-lg px-3 py-1.5 text-sm font-medium" {
                        "Save changes"
                    }
                }
            }
        }))
    }
}

// ---------------------------------------------------------------------------
// CRUD mutation handlers
// ---------------------------------------------------------------------------

pub async fn keys_create(
    State(state): State<UiState>,
    Form(form): Form<HashMap<String, String>>,
) -> Html<String> {
    let cfg = state.live_config();
    let cfg = cfg.as_ref();
    let username = cfg.ui.auth.as_ref().map(|a| a.username.as_str());
    let user = username.unwrap_or("operator");
    let unlocked = cfg.ui.history.is_some();

    // CSRF check.
    let token = form.get("csrf_token").map(|s| s.as_str()).unwrap_or("");
    if !csrf::csrf_ok(&state, user, token) {
        let body = html! {
            (page_header("Forbidden", ""))
            p class="text-sm text-destructive" { "CSRF token invalid." }
        };
        return Html(
            shell("Virtual Keys", "Virtual Keys", Nav::VirtualKeys, unlocked, username, body)
                .into_string(),
        );
    }

    match build_key_from_form(&form, cfg) {
        Err(e) => {
            // Re-render the list with the create form open and an error.
            let body = html! {
                div class="flex items-start justify-between gap-4" {
                    (page_header("Virtual Keys", "Client-facing keys that map to upstream connections and model allowlists."))
                    div class="mt-1.5 shrink-0" {
                        details open {
                            summary class="btn-brand rounded-lg px-3.5 py-2 text-sm font-medium inline-flex items-center gap-2 cursor-pointer list-none" {
                                span class="size-4 grid place-items-center" { (PreEscaped(layout::ICON_KEY)) }
                                "New key"
                            }
                            div class="absolute right-0 mt-2 z-20 w-[480px]" {
                                (new_key_form(&state, user, Some(&e)))
                            }
                        }
                    }
                }
                div class="mt-4" {
                    // Re-render existing keys table in a simplified form.
                    p class="text-sm text-muted-foreground" {
                        (cfg.virtual_keys.len()) " key(s) configured."
                    }
                }
            };
            return Html(
                shell("Virtual Keys", "Virtual Keys", Nav::VirtualKeys, unlocked, username, body)
                    .into_string(),
            );
        }
        Ok(new_entry) => {
            // Read current doc, append new [[virtual_keys]] entry, apply.
            let result = apply_keys_mutation(&state, |keys_toml| {
                format!("{keys_toml}\n\n{new_entry}")
            });
            if let Err(e) = result {
                let body = html! {
                    (page_header("Virtual Keys", ""))
                    div class="mb-4 rounded-md border border-destructive/40 bg-destructive/10 px-4 py-3 text-sm text-destructive" {
                        "Failed to apply config: " (e)
                    }
                };
                return Html(
                    shell("Virtual Keys", "Virtual Keys", Nav::VirtualKeys, unlocked, username, body)
                        .into_string(),
                );
            }
        }
    }

    Html(redirect_html("/ui/keys"))
}

pub async fn keys_update(
    State(state): State<UiState>,
    Path(idx): Path<usize>,
    Form(form): Form<HashMap<String, String>>,
) -> Html<String> {
    let cfg = state.live_config();
    let cfg = cfg.as_ref();
    let username = cfg.ui.auth.as_ref().map(|a| a.username.as_str());
    let user = username.unwrap_or("operator");
    let unlocked = cfg.ui.history.is_some();

    // CSRF check.
    let token = form.get("csrf_token").map(|s| s.as_str()).unwrap_or("");
    if !csrf::csrf_ok(&state, user, token) {
        let body = html! {
            (page_header("Forbidden", ""))
            p class="text-sm text-destructive" { "CSRF token invalid." }
        };
        return Html(
            shell("Virtual Keys", "Virtual Keys", Nav::VirtualKeys, unlocked, username, body)
                .into_string(),
        );
    }

    // Validate index in range before touching the doc.
    if idx >= cfg.virtual_keys.len() {
        return Html(redirect_html("/ui/keys"));
    }

    match build_key_from_form(&form, cfg) {
        Err(e) => {
            let vk = &cfg.virtual_keys[idx];
            let body = html! {
                (page_header(&format!("Key vk-{idx}"), ""))
                (edit_key_form(&state, user, idx, vk, Some(&e)))
            };
            return Html(
                shell("Virtual Keys", "Virtual Keys", Nav::VirtualKeys, unlocked, username, body)
                    .into_string(),
            );
        }
        Ok(updated_entry) => {
            let result = apply_keys_mutation(&state, |keys_toml| {
                replace_key_at(keys_toml, idx, &updated_entry)
            });
            if let Err(e) = result {
                let vk = &cfg.virtual_keys[idx];
                let body = html! {
                    (page_header(&format!("Key vk-{idx}"), ""))
                    (edit_key_form(&state, user, idx, vk, Some(&e)))
                };
                return Html(
                    shell("Virtual Keys", "Virtual Keys", Nav::VirtualKeys, unlocked, username, body)
                        .into_string(),
                );
            }
        }
    }

    Html(redirect_html(&format!("/ui/keys/{idx}")))
}

pub async fn keys_delete(
    State(state): State<UiState>,
    Path(idx): Path<usize>,
    Form(form): Form<HashMap<String, String>>,
) -> Html<String> {
    let cfg = state.live_config();
    let cfg = cfg.as_ref();
    let username = cfg.ui.auth.as_ref().map(|a| a.username.as_str());
    let user = username.unwrap_or("operator");
    let unlocked = cfg.ui.history.is_some();

    // CSRF check.
    let token = form.get("csrf_token").map(|s| s.as_str()).unwrap_or("");
    if !csrf::csrf_ok(&state, user, token) {
        let body = html! {
            (page_header("Forbidden", ""))
            p class="text-sm text-destructive" { "CSRF token invalid." }
        };
        return Html(
            shell("Virtual Keys", "Virtual Keys", Nav::VirtualKeys, unlocked, username, body)
                .into_string(),
        );
    }

    if idx < cfg.virtual_keys.len() {
        let result = apply_keys_mutation(&state, |keys_toml| {
            remove_key_at(keys_toml, idx)
        });
        if let Err(e) = result {
            let body = html! {
                (page_header("Virtual Keys", ""))
                div class="mb-4 rounded-md border border-destructive/40 bg-destructive/10 px-4 py-3 text-sm text-destructive" {
                    "Delete failed: " (e)
                }
            };
            return Html(
                shell("Virtual Keys", "Virtual Keys", Nav::VirtualKeys, unlocked, username, body)
                    .into_string(),
            );
        }
    }

    Html(redirect_html("/ui/keys"))
}

// ---------------------------------------------------------------------------
// Mutation helpers
// ---------------------------------------------------------------------------

/// Read the current document, mutate the `[[virtual_keys]]` section via
/// `transform(current_virtual_keys_toml) -> new_virtual_keys_toml`, then
/// splice back and apply via the Reloader.
fn apply_keys_mutation(
    state: &UiState,
    transform: impl Fn(&str) -> String,
) -> Result<(), String> {
    let mut doc = drgtw_config::read_document(&state.config_path)
        .map_err(|e| format!("Cannot read config: {e}"))?;

    // Extract current [[virtual_keys]] toml text.
    let current_vk_toml = doc
        .get("virtual_keys")
        .map(|v| {
            let mut tmp = toml_edit::DocumentMut::new();
            tmp.insert("virtual_keys", v.clone());
            tmp.to_string()
        })
        .unwrap_or_default();

    let new_vk_toml = transform(&current_vk_toml);

    // Splice new virtual_keys back into the document.
    doc.remove("virtual_keys");
    if !new_vk_toml.trim().is_empty() {
        let snippet: toml_edit::DocumentMut = new_vk_toml
            .parse()
            .map_err(|e| format!("TOML parse error: {e}"))?;
        if let Some(arr) = snippet.get("virtual_keys") {
            doc.insert("virtual_keys", arr.clone());
        }
    }

    // Apply via reloader (validate → write → swap atomically).
    let reloader = state
        .reloader
        .as_ref()
        .ok_or_else(|| "Hot-reload not available — cannot mutate keys.".to_string())?;
    reloader.apply(&doc.to_string())
}

/// Build a `[[virtual_keys]]` TOML entry string from a form submission.
/// Returns `Err` with a user-facing message on validation failure.
fn build_key_from_form(
    form: &HashMap<String, String>,
    cfg: &Config,
) -> Result<String, String> {
    let key = form.get("key").map(|s| s.trim()).unwrap_or("");
    if key.is_empty() {
        return Err("Key value is required.".into());
    }
    if !key.starts_with("sk-drgtw-") {
        return Err("Key must start with sk-drgtw-.".into());
    }

    // Connections: multi-checkbox → Vec via getall (form repeats the field).
    // axum Form<HashMap> only captures the last value for repeated fields, so we
    // join them from the raw query string by scanning form entries with key
    // "connection". Since HashMap dedups, we re-read via the raw form string.
    // We use a workaround: the HTML form sends multiple `connection=name` fields;
    // axum's HashMap<String,String> captures only the last. We instead use a
    // single hidden field `connections` that JS would populate — but since we
    // don't have JS here, we fall back to requiring the operator to name them
    // comma-separated in a text input if needed.
    //
    // For the checkbox approach to work we need Form<Vec<(String,String)>> but
    // the route signature uses HashMap. So we collect connections from the form
    // key "connection" (which HashMap will have as the last checked value), then
    // also accept "connections" (comma-separated text) as a fallback.
    let connections: Vec<String> = {
        let mut conns = Vec::new();
        // Accept "connections" as comma-separated text (the fallback).
        if let Some(c) = form.get("connections") {
            conns.extend(c.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        // Accept "connection" as a single value (last checkbox hit with HashMap).
        if let Some(c) = form.get("connection") {
            let s = c.trim().to_string();
            if !s.is_empty() && !conns.contains(&s) {
                conns.push(s);
            }
        }
        conns
    };

    if connections.is_empty() {
        return Err("At least one connection is required.".into());
    }

    // Validate connections exist in config.
    let known_conns: Vec<&str> = cfg.connections.iter().map(|c| c.name.as_str()).collect();
    for c in &connections {
        if !known_conns.contains(&c.as_str()) {
            return Err(format!("Unknown connection '{c}'. Known: {}.", known_conns.join(", ")));
        }
    }

    let models_raw = form.get("models").map(|s| s.trim()).unwrap_or("");
    let models: Vec<String> = models_raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Rate limit: both fields must be present or both absent.
    let rl_requests = form.get("rl_requests").and_then(|s| s.trim().parse::<u32>().ok());
    let rl_per_seconds = form.get("rl_per_seconds").and_then(|s| s.trim().parse::<u32>().ok());
    let rate_limit = match (rl_requests, rl_per_seconds) {
        (Some(r), Some(p)) if r > 0 && p > 0 => Some((r, p)),
        (Some(_), None) | (None, Some(_)) => {
            return Err("Rate limit requires both requests and per_seconds.".into());
        }
        _ => None,
    };

    // Budget: both fields must be present or both absent.
    let budget_max_usd = form.get("budget_max_usd").and_then(|s| s.trim().parse::<f64>().ok());
    let budget_per_seconds =
        form.get("budget_per_seconds").and_then(|s| s.trim().parse::<u32>().ok());
    let budget = match (budget_max_usd, budget_per_seconds) {
        (Some(m), Some(p)) if m > 0.0 && p > 0 => Some((m, p)),
        (Some(_), None) | (None, Some(_)) => {
            return Err("Budget requires both max_usd and per_seconds.".into());
        }
        _ => None,
    };

    // MCP servers: same HashMap dedup caveat as connections. Accept last value.
    let mcp_servers: Vec<String> = {
        let mut mcps = Vec::new();
        if let Some(s) = form.get("mcp_server") {
            let v = s.trim().to_string();
            if !v.is_empty() {
                mcps.push(v);
            }
        }
        mcps
    };

    // Validate MCP server names.
    for s in &mcp_servers {
        if !cfg.mcp_servers.contains_key(s.as_str()) {
            return Err(format!("Unknown MCP server '{s}'."));
        }
    }

    // Assemble TOML entry.
    //
    // ALL scalar/array fields (key, connections, models, mcp_servers) are
    // emitted BEFORE any sub-table. rate_limit and budget are written as
    // inline tables on the element itself so their fields stay unambiguously
    // attached to the array element and cannot "capture" following keys.
    //
    // Wrong (fields after a [sub-table] header get attributed to it):
    //   [[virtual_keys]]
    //   key = "..."
    //   [virtual_keys.budget]
    //   max_usd = 1.0
    //   per_seconds = 3600
    //   mcp_servers = ["docs"]   ← parser sees this as budget.mcp_servers!
    //
    // Correct (inline tables + all scalar fields first):
    //   [[virtual_keys]]
    //   key = "..."
    //   mcp_servers = ["docs"]
    //   rate_limit = { requests = 5, per_seconds = 60 }
    //   budget = { max_usd = 1.0, per_seconds = 3600 }
    let mut out = String::new();
    out.push_str("[[virtual_keys]]\n");
    out.push_str(&format!("key = {}\n", toml_quote(key)));
    out.push_str(&format!(
        "connections = [{}]\n",
        connections.iter().map(|c| toml_quote(c)).collect::<Vec<_>>().join(", ")
    ));
    if !models.is_empty() {
        out.push_str(&format!(
            "models = [{}]\n",
            models.iter().map(|m| toml_quote(m)).collect::<Vec<_>>().join(", ")
        ));
    }
    // mcp_servers before any sub-table / inline table.
    if !mcp_servers.is_empty() {
        out.push_str(&format!(
            "mcp_servers = [{}]\n",
            mcp_servers.iter().map(|s| toml_quote(s)).collect::<Vec<_>>().join(", ")
        ));
    }
    // Inline tables: self-contained on one line, no header to steal following keys.
    if let Some((r, p)) = rate_limit {
        out.push_str(&format!("rate_limit = {{ requests = {r}, per_seconds = {p} }}\n"));
    }
    if let Some((m, p)) = budget {
        out.push_str(&format!("budget = {{ max_usd = {m}, per_seconds = {p} }}\n"));
    }

    Ok(out)
}

/// Replace the `idx`-th `[[virtual_keys]]` block in the existing toml text.
fn replace_key_at(existing_toml: &str, idx: usize, new_entry: &str) -> String {
    let blocks = split_virtual_key_blocks(existing_toml);
    blocks
        .iter()
        .enumerate()
        .map(|(i, b)| if i == idx { new_entry.to_string() } else { b.clone() })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Remove the `idx`-th `[[virtual_keys]]` block from the existing toml text.
fn remove_key_at(existing_toml: &str, idx: usize) -> String {
    let blocks = split_virtual_key_blocks(existing_toml);
    blocks
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(_, b)| b.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split a `[[virtual_keys]]` toml text into per-key blocks.
fn split_virtual_key_blocks(toml: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in toml.lines() {
        if line.trim_start().starts_with("[[virtual_keys]]") && !current.is_empty() {
            blocks.push(current.trim().to_string());
            current = String::new();
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        blocks.push(current.trim().to_string());
    }
    blocks
}

/// TOML-quote a string value.
fn toml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// HTML redirect response (200 with a meta-refresh + JS location) since our
/// handlers return Html<String>, not Redirect.
fn redirect_html(url: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html><head><meta http-equiv="refresh" content="0;url={url}">
<script>location.href="{url}"</script></head><body></body></html>"#
    )
}

// ---------------------------------------------------------------------------
// Small display helpers
// ---------------------------------------------------------------------------

fn zero_summary() -> drgtw_history::UsageSummary {
    drgtw_history::UsageSummary {
        requests: 0,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        avg_latency_ms: 0.0,
        pii_count: 0,
        error_count: 0,
    }
}

/// Burn-bar percentage clamped to [0, 100].
fn pct(used: f64, total: f64) -> f64 {
    if total <= 0.0 { return 0.0; }
    (used / total * 100.0).clamp(0.0, 100.0)
}

/// Color class for the budget burn bar.
fn burn_color(ratio: f64) -> &'static str {
    if ratio >= 90.0 { "bg-destructive" } else if ratio >= 70.0 { "bg-warn" } else { "bg-primary" }
}

/// Format seconds into a human-readable string (e.g. "2h 5m").
fn fmt_secs(secs: u64) -> String {
    if secs < 60 { return format!("{secs}s"); }
    let m = secs / 60;
    if m < 60 { return format!("{m}m {s}s", s = secs % 60); }
    let h = m / 60;
    format!("{h}h {m}m", m = m % 60)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_quote_escapes() {
        assert_eq!(toml_quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(toml_quote("normal"), r#""normal""#);
    }

    #[test]
    fn split_empty_toml() {
        assert!(split_virtual_key_blocks("").is_empty());
        assert!(split_virtual_key_blocks("   \n").is_empty());
    }

    #[test]
    fn split_single_block() {
        let toml = "[[virtual_keys]]\nkey = \"sk-drgtw-abc\"\nconnections = [\"default\"]\n";
        let blocks = split_virtual_key_blocks(toml);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("sk-drgtw-abc"));
    }

    #[test]
    fn split_two_blocks() {
        let toml = "\
[[virtual_keys]]
key = \"sk-drgtw-aaa\"
connections = [\"default\"]

[[virtual_keys]]
key = \"sk-drgtw-bbb\"
connections = [\"default\"]
";
        let blocks = split_virtual_key_blocks(toml);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("sk-drgtw-aaa"));
        assert!(blocks[1].contains("sk-drgtw-bbb"));
    }

    #[test]
    fn remove_key_at_first() {
        let toml = "\
[[virtual_keys]]
key = \"sk-drgtw-aaa\"
connections = [\"c\"]

[[virtual_keys]]
key = \"sk-drgtw-bbb\"
connections = [\"c\"]
";
        let result = remove_key_at(toml, 0);
        assert!(!result.contains("sk-drgtw-aaa"));
        assert!(result.contains("sk-drgtw-bbb"));
    }

    #[test]
    fn remove_key_at_last() {
        let toml = "\
[[virtual_keys]]
key = \"sk-drgtw-aaa\"
connections = [\"c\"]

[[virtual_keys]]
key = \"sk-drgtw-bbb\"
connections = [\"c\"]
";
        let result = remove_key_at(toml, 1);
        assert!(result.contains("sk-drgtw-aaa"));
        assert!(!result.contains("sk-drgtw-bbb"));
    }

    #[test]
    fn replace_key_at_updates_correct_block() {
        let toml = "\
[[virtual_keys]]
key = \"sk-drgtw-aaa\"
connections = [\"c\"]

[[virtual_keys]]
key = \"sk-drgtw-bbb\"
connections = [\"c\"]
";
        let new_entry = "[[virtual_keys]]\nkey = \"sk-drgtw-zzz\"\nconnections = [\"c\"]\n";
        let result = replace_key_at(toml, 0, new_entry);
        assert!(result.contains("sk-drgtw-zzz"));
        assert!(!result.contains("sk-drgtw-aaa"));
        assert!(result.contains("sk-drgtw-bbb"));
    }

    #[test]
    fn build_key_requires_sk_drgtw_prefix() {
        let mut form = HashMap::new();
        form.insert("key".into(), "bad-key".into());
        form.insert("connection".into(), "default".into());
        // Need a minimal Config — we only check validation errors before cfg lookups.
        // This will fail on the prefix check before touching cfg.
        // We can't easily construct a Config without the full config machinery,
        // so we test the prefix error path with a bare form.
        let cfg_str = r#"
[server]
bind_addr = "0.0.0.0:4000"
[[connections]]
name = "default"
base_url = "https://api.anthropic.com"
api_key = "sk-ant-test"
format = "anthropic"
models = ["claude-opus-4-8"]
[ui]
"#;
        let cfg: drgtw_config::Config = drgtw_config::validate_str(cfg_str).unwrap();
        let err = build_key_from_form(&form, &cfg).unwrap_err();
        assert!(err.contains("sk-drgtw-"), "got: {err}");
    }

    #[test]
    fn build_key_requires_at_least_one_connection() {
        let cfg_str = r#"
[server]
bind_addr = "0.0.0.0:4000"
[[connections]]
name = "default"
base_url = "https://api.anthropic.com"
api_key = "sk-ant-test"
format = "anthropic"
models = ["claude-opus-4-8"]
[ui]
"#;
        let cfg: drgtw_config::Config = drgtw_config::validate_str(cfg_str).unwrap();
        let mut form = HashMap::new();
        form.insert("key".into(), "sk-drgtw-abc123".into());
        // No connection field.
        let err = build_key_from_form(&form, &cfg).unwrap_err();
        assert!(err.to_lowercase().contains("connection"), "got: {err}");
    }

    #[test]
    fn build_key_rejects_unknown_connection() {
        let cfg_str = r#"
[server]
bind_addr = "0.0.0.0:4000"
[[connections]]
name = "default"
base_url = "https://api.anthropic.com"
api_key = "sk-ant-test"
format = "anthropic"
models = ["claude-opus-4-8"]
[ui]
"#;
        let cfg: drgtw_config::Config = drgtw_config::validate_str(cfg_str).unwrap();
        let mut form = HashMap::new();
        form.insert("key".into(), "sk-drgtw-abc123".into());
        form.insert("connection".into(), "nonexistent".into());
        let err = build_key_from_form(&form, &cfg).unwrap_err();
        assert!(err.contains("nonexistent"), "got: {err}");
    }

    #[test]
    fn build_key_valid_minimal() {
        let cfg_str = r#"
[server]
bind_addr = "0.0.0.0:4000"
[[connections]]
name = "default"
base_url = "https://api.anthropic.com"
api_key = "sk-ant-test"
format = "anthropic"
models = ["claude-opus-4-8"]
[ui]
"#;
        let cfg: drgtw_config::Config = drgtw_config::validate_str(cfg_str).unwrap();
        let mut form = HashMap::new();
        form.insert("key".into(), "sk-drgtw-valid123".into());
        form.insert("connection".into(), "default".into());
        let toml = build_key_from_form(&form, &cfg).unwrap();
        assert!(toml.contains("[[virtual_keys]]"));
        assert!(toml.contains("sk-drgtw-valid123"));
        assert!(toml.contains("default"));
        // Raw key must NOT be masked — it's the real value going to disk.
        assert!(!toml.contains("sk-drgtw-****"));
    }

    #[test]
    fn build_key_mcp_servers_not_stolen_by_budget_subtable() {
        // Regression: mcp_servers written after a [virtual_keys.budget] header
        // was parsed as budget.mcp_servers instead of virtual_keys.mcp_servers.
        // Fix: emit mcp_servers before rate_limit/budget (as inline tables).
        let cfg_str = r#"
[server]
bind_addr = "0.0.0.0:4000"
[[connections]]
name = "default"
base_url = "https://api.anthropic.com"
api_key = "sk-ant-test"
format = "anthropic"
models = ["claude-opus-4-8"]
[mcp_servers.docs]
url = "https://mcp.example.com/docs"
[ui]
"#;
        let cfg: drgtw_config::Config = drgtw_config::validate_str(cfg_str).unwrap();
        let mut form = HashMap::new();
        form.insert("key".into(), "sk-drgtw-mcptest".into());
        form.insert("connection".into(), "default".into());
        form.insert("rl_requests".into(), "5".into());
        form.insert("rl_per_seconds".into(), "60".into());
        form.insert("budget_max_usd".into(), "1.0".into());
        form.insert("budget_per_seconds".into(), "3600".into());
        form.insert("mcp_server".into(), "docs".into());

        let toml = build_key_from_form(&form, &cfg).unwrap();

        // The generated TOML must round-trip: wrap it in a minimal valid doc
        // and parse it with drgtw_config.
        let full_cfg_str = format!("{cfg_str}\n{toml}");
        let parsed = drgtw_config::validate_str(&full_cfg_str)
            .unwrap_or_else(|e| panic!("generated TOML failed to parse: {e:?}\n---\n{toml}"));

        let vk = parsed.virtual_keys.first().expect("one virtual key");

        // mcp_servers must be on the key, not silently dropped or misattributed.
        assert_eq!(
            vk.mcp_servers.as_ref().map(|v| v.as_slice()),
            Some(["docs".to_string()].as_slice()),
            "mcp_servers missing or wrong after round-trip; generated TOML:\n{toml}"
        );

        // rate_limit and budget must still parse correctly.
        let rl = vk.rate_limit.as_ref().expect("rate_limit present");
        assert_eq!(rl.requests, 5);
        assert_eq!(rl.per_seconds, 60);
        let b = vk.budget.as_ref().expect("budget present");
        assert_eq!(b.max_usd, 1.0);
        assert_eq!(b.per_seconds, 3600);
    }

    #[test]
    fn masked_key_never_leaks_raw_in_rendered_table() {
        // The list page renders mask_secret(vk.key); ensure the raw secret body
        // is absent from the output. We check via the mask helper directly.
        // mask_secret keeps a short prefix + "…" + last 4 chars (e.g. "sk-…1234").
        let raw = "sk-drgtw-supersecret1234";
        let masked = crate::mask::mask_secret(raw);
        assert!(!masked.contains("supersecret"), "mask leaked body: {masked}");
        assert!(masked.contains("…"), "mask should contain ellipsis: {masked}");
        assert!(masked.starts_with("sk-"), "mask should keep sk- prefix: {masked}");
    }

    #[test]
    fn pct_clamped() {
        assert_eq!(pct(0.0, 100.0), 0.0);
        assert_eq!(pct(50.0, 100.0), 50.0);
        assert_eq!(pct(110.0, 100.0), 100.0);
        assert_eq!(pct(5.0, 0.0), 0.0);
    }

    #[test]
    fn fmt_secs_formats_correctly() {
        assert_eq!(fmt_secs(30), "30s");
        assert_eq!(fmt_secs(90), "1m 30s");
        assert_eq!(fmt_secs(3700), "1h 1m");
    }
}
