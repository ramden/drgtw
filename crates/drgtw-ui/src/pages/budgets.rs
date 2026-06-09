//! `GET /ui/budgets` — Cost & Budgets.
//!
//! Async page (already wired as `get(pages::cost_budgets)`). Shows every
//! virtual key with its `budget` config (max_usd / per_seconds), the live
//! spend + window-reset countdown from [`UiState::budget_snapshot`], and a
//! burn-down bar. Also shows the 24h gateway-wide cost summary from the
//! history store (per-key cost queries require Phase B history additions).

use axum::extract::State;
use axum::response::Html;
use maud::{Markup, PreEscaped, html};

use crate::UiState;
use crate::layout::{self, Nav, badge, empty_state, page_header, shell};
use crate::mask::mask_secret;
use crate::pages::{fmt_cost, glass_card, section_title};

pub async fn cost_budgets(State(state): State<UiState>) -> Html<String> {
    // Fetch 24h gateway-wide cost summary (fallback to zero if history disabled).
    let (since, until, _bucket) = crate::range_window("24h");
    let gateway_cost = match state.history() {
        Some(h) => h.usage_summary(since, until).await.map(|s| s.cost_usd).unwrap_or(0.0),
        None => 0.0,
    };

    Html(render(&state, gateway_cost).into_string())
}

fn render(state: &UiState, gateway_cost_24h: f64) -> Markup {
    let cfg = state.live_config();
    let cfg = cfg.as_ref();
    let unlocked = cfg.ui.history.is_some();
    let username = cfg.ui.auth.as_ref().map(|a| a.username.as_str());

    let any_configured = cfg.virtual_keys.iter().any(|vk| vk.budget.is_some());

    let inner = html! {
        (page_header("Cost & Budgets", "Per-key USD spend caps and live burn-down. Gateway-wide 24h cost shown at a glance."))

        // Gateway-wide 24h cost card.
        div class="mb-4" {
            (glass_card(1, html! {
                (section_title(layout::ICON_COINS, "Gateway · 24h Cost"))
                div class="text-3xl font-semibold stat-gradient mt-1" { (fmt_cost(gateway_cost_24h)) }
                p class="text-xs text-muted-foreground mt-1" {
                    @if unlocked {
                        "Aggregated across all keys. Per-key breakdown requires Phase B history."
                    } @else {
                        "Connect a history store ([ui.history]) to track spend."
                    }
                }
            }))
        }

        // Per-key budget table.
        @if cfg.virtual_keys.is_empty() {
            (empty_state(
                layout::ICON_COINS, "muted", "No keys",
                "No virtual keys configured",
                html! {
                    "Add " code class="font-mono" { "[[virtual_keys]]" }
                    " entries to drgtw.toml, then add a "
                    code class="font-mono" { "[virtual_keys.budget]" }
                    " block to cap spend."
                }
            ))
        } @else if !any_configured {
            (empty_state(
                layout::ICON_COINS, "muted", "None configured",
                "No budgets configured",
                html! {
                    "Add a " code class="font-mono" { "budget = { max_usd = N, per_seconds = W }" }
                    " field to a " code class="font-mono" { "[[virtual_keys]]" }
                    " block to cap its spend. "
                    a href="/ui/keys" class="underline" { "Manage keys" }
                }
            ))
        } @else {
            div class="rise grid" style="--i:2" {
              div class="glass overflow-hidden" {
                div class="overflow-x-auto" {
                    table class="w-full text-sm" {
                        thead {
                            tr class="text-left text-[11px] uppercase tracking-wide text-muted-foreground border-b border-border" {
                                th class="font-medium px-5 py-3" { "Key" }
                                th class="font-medium px-5 py-3" { "Max USD" }
                                th class="font-medium px-5 py-3" { "Window" }
                                th class="font-medium px-5 py-3" { "Spent" }
                                th class="font-medium px-5 py-3" { "Remaining" }
                                th class="font-medium px-5 py-3" { "Resets in" }
                                th class="font-medium px-5 py-3" { "Burn bar" }
                            }
                        }
                        tbody {
                            @for (idx, vk) in cfg.virtual_keys.iter().enumerate() {
                                tr class="row-lift border-b border-border/50 last:border-0" {
                                    td class="px-5 py-3.5" {
                                        div class="flex items-center gap-2.5" {
                                            span class="size-6 rounded-md icon-orb grid place-items-center text-primary shrink-0" {
                                                span class="size-3 grid place-items-center" {
                                                    (PreEscaped(layout::ICON_KEY))
                                                }
                                            }
                                            a href=(format!("/ui/keys/{idx}"))
                                                class="font-mono text-[12.5px] hover:underline"
                                            {
                                                (mask_secret(&vk.key))
                                            }
                                        }
                                    }

                                    @if let Some(b) = &vk.budget {
                                        td class="px-5 py-3.5 font-mono text-xs" {
                                            (fmt_cost(b.max_usd))
                                        }
                                        td class="px-5 py-3.5 text-xs text-muted-foreground" {
                                            (fmt_window(b.per_seconds))
                                        }

                                        @let key_id = format!("vk-{idx}");
                                        @let snap = state.budget_snapshot(&key_id);

                                        @if let Some(s) = &snap {
                                            @let spent_pct = burn_pct(s.spent_usd, s.max_usd);
                                            td class="px-5 py-3.5 font-semibold" {
                                                (fmt_cost(s.spent_usd))
                                            }
                                            td class="px-5 py-3.5 text-xs text-muted-foreground" {
                                                (fmt_cost((s.max_usd - s.spent_usd).max(0.0)))
                                            }
                                            td class="px-5 py-3.5" {
                                                @if s.secs_to_reset == 0 {
                                                    (badge("ok", "now"))
                                                } @else {
                                                    span class="font-mono text-xs" {
                                                        (fmt_secs(s.secs_to_reset))
                                                    }
                                                }
                                            }
                                            td class="px-5 py-3.5 w-36" {
                                                div {
                                                    div class="flex justify-between text-[10px] text-muted-foreground mb-0.5" {
                                                        span { (format!("{:.0}%", spent_pct)) }
                                                        span { (fmt_cost(s.max_usd)) }
                                                    }
                                                    div class="h-2 rounded-full bg-border overflow-hidden" {
                                                        div class=(format!("h-full rounded-full transition-all {}", burn_color(spent_pct)))
                                                            style=(format!("width: {}%", spent_pct)) {}
                                                    }
                                                }
                                            }
                                        } @else {
                                            // Reloader not wired or key not yet active.
                                            td class="px-5 py-3.5 text-muted-foreground text-xs" { "—" }
                                            td class="px-5 py-3.5 text-muted-foreground text-xs" {
                                                (fmt_cost(b.max_usd))
                                            }
                                            td class="px-5 py-3.5 text-muted-foreground text-xs" { "—" }
                                            td class="px-5 py-3.5 text-muted-foreground text-xs" { "—" }
                                        }
                                    } @else {
                                        // No budget on this key.
                                        td class="px-5 py-3.5 text-muted-foreground text-xs" { "—" }
                                        td class="px-5 py-3.5 text-muted-foreground text-xs" { "—" }
                                        td class="px-5 py-3.5 text-muted-foreground text-xs" { "—" }
                                        td class="px-5 py-3.5 text-muted-foreground text-xs" { "—" }
                                        td class="px-5 py-3.5 text-muted-foreground text-xs" { "—" }
                                        td class="px-5 py-3.5" {}
                                    }
                                }
                            }
                        }
                    }
                }
              }
            }

            p class="mt-3 text-[11px] text-muted-foreground" {
                "Live spend counters are read from the in-process budget tracker. "
                "Reload the page to refresh. Keys without a budget block show "
                span class="font-mono" { "—" } "."
            }
        }
    };

    shell("Cost & Budgets", "Cost & Budgets", Nav::CostBudgets, unlocked, username, inner)
}

// ---------------------------------------------------------------------------
// Small display helpers
// ---------------------------------------------------------------------------

/// Burn percentage clamped to [0, 100].
fn burn_pct(spent: f64, max: f64) -> f64 {
    if max <= 0.0 { return 0.0; }
    (spent / max * 100.0).clamp(0.0, 100.0)
}

/// Colour class for the burn bar (more spent = worse = red).
fn burn_color(pct: f64) -> &'static str {
    if pct >= 90.0 { "bg-destructive" } else if pct >= 70.0 { "bg-warn" } else { "bg-primary" }
}

/// Format a window in seconds as a human-readable label.
fn fmt_window(secs: u32) -> String {
    if secs < 60 { return format!("{secs}s"); }
    let m = secs / 60;
    if m < 60 { return format!("{m}m"); }
    let h = m / 60;
    if h < 24 { return format!("{h}h"); }
    format!("{}d", h / 24)
}

/// Format seconds to human-readable countdown.
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
    fn burn_pct_clamped() {
        assert_eq!(burn_pct(0.0, 10.0), 0.0);
        assert_eq!(burn_pct(5.0, 10.0), 50.0);
        assert_eq!(burn_pct(12.0, 10.0), 100.0);
        assert_eq!(burn_pct(5.0, 0.0), 0.0);
    }

    #[test]
    fn burn_color_bands() {
        assert_eq!(burn_color(0.0), "bg-primary");
        assert_eq!(burn_color(69.0), "bg-primary");
        assert_eq!(burn_color(70.0), "bg-warn");
        assert_eq!(burn_color(89.0), "bg-warn");
        assert_eq!(burn_color(90.0), "bg-destructive");
        assert_eq!(burn_color(100.0), "bg-destructive");
    }

    #[test]
    fn fmt_window_formats() {
        assert_eq!(fmt_window(30), "30s");
        assert_eq!(fmt_window(60), "1m");
        assert_eq!(fmt_window(3600), "1h");
        assert_eq!(fmt_window(86400), "1d");
    }

    #[test]
    fn fmt_secs_formats() {
        assert_eq!(fmt_secs(30), "30s");
        assert_eq!(fmt_secs(90), "1m 30s");
        assert_eq!(fmt_secs(3700), "1h 1m");
    }

    #[test]
    fn cost_budgets_renders_empty_state_no_keys() {
        use std::sync::Arc;

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
        let cfg = drgtw_config::validate_str(cfg_str).unwrap();
        let state = crate::UiState::new(
            std::time::Instant::now(),
            Arc::new(cfg),
            std::path::PathBuf::from("/tmp/drgtw.toml"),
            crate::PgGate::NotConfigured,
        );

        let html = render(&state, 0.0).into_string();
        assert!(html.contains("No virtual keys configured"), "got: {html}");
    }

    #[test]
    fn cost_budgets_shows_configured_budget() {
        use std::sync::Arc;

        let cfg_str = r#"
[server]
bind_addr = "0.0.0.0:4000"
[[connections]]
name = "default"
base_url = "https://api.anthropic.com"
api_key = "sk-ant-test"
format = "anthropic"
models = ["claude-opus-4-8"]
[[virtual_keys]]
key = "sk-drgtw-budgeted"
connections = ["default"]
[virtual_keys.budget]
max_usd = 25.0
per_seconds = 86400
[ui]
"#;
        let cfg = drgtw_config::validate_str(cfg_str).unwrap();
        let state = crate::UiState::new(
            std::time::Instant::now(),
            Arc::new(cfg),
            std::path::PathBuf::from("/tmp/drgtw.toml"),
            crate::PgGate::NotConfigured,
        );

        let html = render(&state, 1.23).into_string();

        // Budget table should appear with the max_usd value.
        assert!(html.contains("25"), "max_usd missing in: {html}");
        // Gateway cost card.
        assert!(html.contains("1.23") || html.contains("$1"), "gateway cost missing in: {html}");
        // Raw key must not appear.
        assert!(!html.contains("sk-drgtw-budgeted"), "raw key leaked in: {html}");
    }

    #[test]
    fn cost_budgets_no_budget_key_shows_dash() {
        use std::sync::Arc;

        let cfg_str = r#"
[server]
bind_addr = "0.0.0.0:4000"
[[connections]]
name = "default"
base_url = "https://api.anthropic.com"
api_key = "sk-ant-test"
format = "anthropic"
models = ["claude-opus-4-8"]
[[virtual_keys]]
key = "sk-drgtw-nobudget"
connections = ["default"]
[ui]
"#;
        let cfg = drgtw_config::validate_str(cfg_str).unwrap();
        let state = crate::UiState::new(
            std::time::Instant::now(),
            Arc::new(cfg),
            std::path::PathBuf::from("/tmp/drgtw.toml"),
            crate::PgGate::NotConfigured,
        );

        let html = render(&state, 0.0).into_string();
        // No budget configured — should show the "None configured" empty state.
        assert!(html.contains("No budgets configured"), "got: {html}");
    }
}
