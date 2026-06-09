//! `GET /ui/limits` — Rate Limits.
//!
//! Table of every virtual key with its `rate_limit` config (requests /
//! per_seconds) plus the live token-bucket snapshot (remaining / capacity,
//! secs_to_next_token) from [`UiState::rate_limit_snapshot`]. Keys with no
//! `rate_limit` configured show "—". Sync page rendered via the `page!` macro.

use maud::{Markup, PreEscaped, html};

use crate::UiState;
use crate::layout::{self, Nav, badge, empty_state, page_header, shell};
use crate::mask::mask_secret;

pub fn rate_limits(state: &UiState) -> Markup {
    let cfg = state.live_config();
    let cfg = cfg.as_ref();
    let unlocked = cfg.ui.history.is_some();
    let username = cfg.ui.auth.as_ref().map(|a| a.username.as_str());

    let any_configured = cfg.virtual_keys.iter().any(|vk| vk.rate_limit.is_some());

    let body = html! {
        (page_header("Rate Limits", "Per-key request throttle windows. Live counters refresh on page load."))

        @if cfg.virtual_keys.is_empty() {
            (empty_state(
                layout::ICON_GAUGE2, "muted", "No keys",
                "No virtual keys configured",
                html! {
                    "Add " code class="font-mono" { "[[virtual_keys]]" }
                    " entries to drgtw.toml, then add a "
                    code class="font-mono" { "[virtual_keys.rate_limit]" }
                    " block to each key you want to throttle."
                }
            ))
        } @else if !any_configured {
            (empty_state(
                layout::ICON_GAUGE2, "muted", "None configured",
                "No rate limits configured",
                html! {
                    "Add a " code class="font-mono" { "rate_limit = { requests = N, per_seconds = W }" }
                    " field to a " code class="font-mono" { "[[virtual_keys]]" }
                    " block to enable throttling. "
                    a href="/ui/keys" class="underline" { "Manage keys" }
                }
            ))
        } @else {
            div class="rise grid" style="--i:1" {
              div class="glass overflow-hidden" {
                div class="overflow-x-auto" {
                    table class="w-full text-sm" {
                        thead {
                            tr class="text-left text-[11px] uppercase tracking-wide text-muted-foreground border-b border-border" {
                                th class="font-medium px-5 py-3" { "Key" }
                                th class="font-medium px-5 py-3" { "Config" }
                                th class="font-medium px-5 py-3" { "Remaining" }
                                th class="font-medium px-5 py-3" { "Capacity" }
                                th class="font-medium px-5 py-3" { "Refill in" }
                                th class="font-medium px-5 py-3" { "Fill bar" }
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

                                    @if let Some(rl) = &vk.rate_limit {
                                        td class="px-5 py-3.5 font-mono text-xs" {
                                            (rl.requests) " / " (rl.per_seconds) "s"
                                        }

                                        @let key_id = format!("vk-{idx}");
                                        @let snap = state.rate_limit_snapshot(&key_id);

                                        @if let Some(s) = &snap {
                                            td class="px-5 py-3.5 font-semibold" { (s.remaining) }
                                            td class="px-5 py-3.5 text-muted-foreground text-xs" { (s.capacity) }
                                            td class="px-5 py-3.5" {
                                                @if s.secs_to_next_token == 0 {
                                                    (badge("ok", "full"))
                                                } @else {
                                                    span class="font-mono text-xs" { (s.secs_to_next_token) "s" }
                                                }
                                            }
                                            td class="px-5 py-3.5 w-32" {
                                                @let ratio = fill_pct(s.remaining as f64, s.capacity as f64);
                                                div class="h-2 rounded-full bg-border overflow-hidden" {
                                                    div class=(format!("h-full rounded-full transition-all {}", fill_color(ratio)))
                                                        style=(format!("width: {}%", ratio)) {}
                                                }
                                            }
                                        } @else {
                                            // Reloader not wired or key not yet active.
                                            td class="px-5 py-3.5 text-muted-foreground text-xs" { "—" }
                                            td class="px-5 py-3.5 text-muted-foreground text-xs" {
                                                (rl.requests)
                                            }
                                            td class="px-5 py-3.5 text-muted-foreground text-xs" { "—" }
                                            td class="px-5 py-3.5 text-muted-foreground text-xs" { "—" }
                                        }
                                    } @else {
                                        // No rate limit on this key.
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

            // Legend.
            p class="mt-3 text-[11px] text-muted-foreground" {
                "Live counters are read from the in-process token-bucket. "
                "Reload the page to refresh. Keys without a rate_limit block show "
                span class="font-mono" { "—" } "."
            }
        }
    };

    shell("Rate Limits", "Rate Limits", Nav::RateLimits, unlocked, username, body)
}

/// Fill percentage clamped to [0, 100].
fn fill_pct(remaining: f64, capacity: f64) -> f64 {
    if capacity <= 0.0 { return 0.0; }
    (remaining / capacity * 100.0).clamp(0.0, 100.0)
}

/// Colour class for the fill bar (more full = healthier = green).
fn fill_color(pct: f64) -> &'static str {
    if pct >= 60.0 { "bg-ok" } else if pct >= 20.0 { "bg-warn" } else { "bg-destructive" }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_pct_clamped() {
        assert_eq!(fill_pct(0.0, 100.0), 0.0);
        assert_eq!(fill_pct(50.0, 100.0), 50.0);
        assert_eq!(fill_pct(120.0, 100.0), 100.0);
        assert_eq!(fill_pct(5.0, 0.0), 0.0);
    }

    #[test]
    fn fill_color_bands() {
        assert_eq!(fill_color(100.0), "bg-ok");
        assert_eq!(fill_color(60.0), "bg-ok");
        assert_eq!(fill_color(59.0), "bg-warn");
        assert_eq!(fill_color(20.0), "bg-warn");
        assert_eq!(fill_color(19.0), "bg-destructive");
        assert_eq!(fill_color(0.0), "bg-destructive");
    }

    #[test]
    fn rate_limits_page_renders_empty_state_for_no_keys() {
        use std::sync::Arc;

        // Build a minimal UiState with zero virtual keys.
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

        let markup = rate_limits(&state);
        let html = markup.into_string();

        // Should show the empty state, not a table.
        assert!(html.contains("No virtual keys configured"), "got: {html}");
    }

    #[test]
    fn rate_limits_page_renders_no_config_empty_state() {
        use std::sync::Arc;

        // A key exists but has no rate_limit.
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
key = "sk-drgtw-test1"
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

        let markup = rate_limits(&state);
        let html = markup.into_string();

        assert!(html.contains("No rate limits configured"), "got: {html}");
    }

    #[test]
    fn rate_limits_page_shows_key_with_configured_limit() {
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
key = "sk-drgtw-limited"
connections = ["default"]
[virtual_keys.rate_limit]
requests = 100
per_seconds = 60
[ui]
"#;
        let cfg = drgtw_config::validate_str(cfg_str).unwrap();
        let state = crate::UiState::new(
            std::time::Instant::now(),
            Arc::new(cfg),
            std::path::PathBuf::from("/tmp/drgtw.toml"),
            crate::PgGate::NotConfigured,
        );

        let markup = rate_limits(&state);
        let html = markup.into_string();

        // Config column should show "100 / 60s".
        assert!(html.contains("100"), "got: {html}");
        assert!(html.contains("60"), "got: {html}");
        // Masked key must appear; raw key must not.
        assert!(!html.contains("sk-drgtw-limited"), "raw key leaked in: {html}");
    }
}
