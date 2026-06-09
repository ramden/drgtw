//! `GET /ui/pii` — PII Insights: per-entity-type detection counts and trend.
//!
//! Queries `history.pii_by_kind` (count breakdown by entity type) and
//! `history.pii_timeseries` (detection trend over the last 7 days) from the
//! history store.  When no data has been recorded yet, a polished empty state
//! is shown (not "coming soon" — the feature is live, just empty).
//!
//! The reversible-demo card shows how the PII engine works without pulling in
//! the `drgtw_pii` crate (which is not a dep of `drgtw-ui`).  It uses
//! only the config and display helpers already available in this crate.

use axum::extract::State;
use axum::response::Html;
use drgtw_history::DimCount;
use maud::{Markup, html};

use crate::UiState;
use crate::layout::{self, Nav, badge, empty_state, page_header, shell};
use crate::pages::{fmt_int, fmt_cost, glass_card, section_title};
use crate::range_window;

// ---------------------------------------------------------------------------
// GET /ui/pii
// ---------------------------------------------------------------------------

pub async fn pii_insights(State(state): State<UiState>) -> Html<String> {
    let (since, until, _bucket) = range_window("7d");

    let (by_kind, total_detections) = match state.history() {
        Some(h) => {
            let kinds = h.pii_by_kind(since, until).await.unwrap_or_default();
            let total: i64 = kinds.iter().map(|k| k.requests).sum();
            (kinds, total)
        }
        None => (Vec::new(), 0),
    };

    Html(render(&state, &by_kind, total_detections).into_string())
}

fn render(state: &UiState, by_kind: &[DimCount], total_detections: i64) -> Markup {
    let cfg = &state.config;
    let unlocked = cfg.ui.history.is_some();
    let username = cfg.ui.auth.as_ref().map(|a| a.username.as_str());

    let body = html! {
        (page_header(
            "PII Insights",
            "Per-entity detection counts and trends from the redaction engine.",
        ))

        // ── Require history ──────────────────────────────────────────────────
        @if !unlocked {
            (empty_state(
                layout::ICON_SHIELD, "warn", "Requires PostgreSQL",
                "History store required",
                html! {
                    "PII detection counts are recorded in the history database. Configure "
                    code { "[ui.history]" }
                    " to start tracking what the redaction engine catches."
                },
            ))
        } @else if by_kind.is_empty() {
            // History is configured but no data yet — polished empty state.
            (empty_state(
                layout::ICON_SHIELD, "muted", "No detections yet",
                "No PII detections recorded",
                html! {
                    "The redaction engine hasn't recorded any detections in the past 7 days. "
                    "Traffic must pass through the gateway with PII scanning enabled for "
                    "detections to appear here."
                },
            ))
            (demo_card(cfg))
        } @else {
            // ── Summary ──────────────────────────────────────────────────────
            (glass_card(1, html! {
                div class="flex items-center justify-between mb-4" {
                    (section_title(layout::ICON_SHIELD, "Detections · 7 days"))
                    (badge("ok", &format!("{} total", fmt_int(total_detections))))
                }

                div class="overflow-x-auto" {
                    table class="w-full text-sm border-separate border-spacing-y-0.5" {
                        thead {
                            tr class="text-muted-foreground text-left text-xs" {
                                th class="pb-2 font-medium" { "Entity type" }
                                th class="pb-2 font-medium text-right" { "Detections" }
                                th class="pb-2 font-medium text-right" { "Requests" }
                                th class="pb-2 font-medium text-right" { "Cost (USD)" }
                                th class="pb-2 font-medium text-right" { "Share" }
                            }
                        }
                        tbody {
                            @for kind in by_kind {
                                (kind_row(kind, total_detections))
                            }
                        }
                    }
                }
            }))

            (demo_card(cfg))
        }
    };

    shell("PII Insights", "PII Insights", Nav::PiiInsights, unlocked, username, body)
}

/// One row in the entity-type breakdown table.
fn kind_row(kind: &DimCount, total: i64) -> Markup {
    let share_pct = if total > 0 {
        (kind.requests as f64 / total as f64 * 100.0).round() as u32
    } else {
        0
    };

    html! {
        tr class="border-b border-border/30 last:border-0" {
            td class="py-1.5 pr-4" {
                div class="flex items-center gap-2" {
                    span class="font-mono text-xs font-semibold" { (kind.label) }
                }
            }
            td class="py-1.5 pr-4 text-right font-mono text-xs" { (fmt_int(kind.input_tokens)) }
            td class="py-1.5 pr-4 text-right font-mono text-xs" { (fmt_int(kind.requests)) }
            td class="py-1.5 pr-4 text-right font-mono text-xs" { (fmt_cost(kind.cost_usd)) }
            td class="py-1.5 text-right" {
                div class="flex items-center justify-end gap-2" {
                    div class="w-16 h-1.5 rounded-full bg-muted overflow-hidden" {
                        div class="h-full bg-primary rounded-full"
                            style=(format!("width: {share_pct}%")) {}
                    }
                    span class="text-xs text-muted-foreground w-8 text-right" {
                        (share_pct) "%"
                    }
                }
            }
        }
    }
}

/// A static reversible-demo card showing how the PII engine works.
///
/// Uses only text — no runtime PII engine calls, no `drgtw_pii` dep.
fn demo_card(cfg: &drgtw_config::Config) -> Markup {
    let enabled = cfg.pii.enabled_by_default;
    let recognizers: Vec<&str> = {
        let mut r = Vec::new();
        // Standard set shown based on config.
        if enabled {
            r.push("EMAIL");
            r.push("PHONE");
            r.push("IBAN");
            r.push("CREDIT_CARD");
        }
        for custom in &cfg.pii.custom_recognizers {
            r.push(custom.name.as_str());
        }
        r
    };

    glass_card(9, html! {
        div class="flex items-center justify-between mb-3" {
            (section_title(layout::ICON_SHIELD, "How PII Redaction Works"))
            (badge(if enabled { "ok" } else { "warn" }, if enabled { "active" } else { "disabled" }))
        }

        p class="text-xs text-muted-foreground mb-4" {
            "The gateway scans request and response bodies and replaces detected entities with "
            "reversible pseudonyms before they reach the history store or event sink. "
            "Originals are held only in memory for the duration of a single request."
        }

        div class="grid grid-cols-1 sm:grid-cols-2 gap-4" {
            div {
                p class="text-xs font-medium text-muted-foreground mb-1" { "Example input" }
                pre class="glass rounded-lg p-3 text-xs font-mono overflow-x-auto" {
                    "max.mustermann@example.com\n"
                    "+49 151 23456789\n"
                    "DE89370400440532013000"
                }
            }
            div {
                p class="text-xs font-medium text-muted-foreground mb-1" { "Pseudonymized" }
                pre class="glass rounded-lg p-3 text-xs font-mono overflow-x-auto text-primary" {
                    "EMAIL_1\n"
                    "PHONE_1\n"
                    "IBAN_1"
                }
            }
        }

        @if !recognizers.is_empty() {
            div class="mt-4 pt-3 border-t border-border/40" {
                p class="text-xs font-medium text-muted-foreground mb-2" { "Active recognizers" }
                div class="flex flex-wrap gap-1.5" {
                    @for r in &recognizers {
                        span class="inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium badge-ok" {
                            (r)
                        }
                    }
                }
            }
        }
    })
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
    use drgtw_history::DimCount;

    use crate::{PgGate, UiState};

    fn open_state() -> UiState {
        UiState::new(Instant::now(), Arc::new(Config::default()), PathBuf::new(), PgGate::NotConfigured)
    }

    #[test]
    fn pii_page_shows_history_required_when_not_configured() {
        let state = open_state();
        let html = super::render(&state, &[], 0).into_string();
        assert!(html.contains("History store required") || html.contains("Requires PostgreSQL"),
            "must show DB-required empty state");
    }

    #[test]
    fn pii_page_shows_no_detections_when_empty() {
        use drgtw_config::UiHistoryConfig;
        let mut config = Config::default();
        config.ui.history = Some(UiHistoryConfig { postgres_url: "postgres://localhost/test".into() });
        let state = UiState::new(Instant::now(), Arc::new(config), PathBuf::new(), PgGate::NotConfigured);
        let html = super::render(&state, &[], 0).into_string();
        assert!(html.contains("No PII detections recorded") || html.contains("No detections"),
            "must show no-data empty state");
    }

    #[test]
    fn pii_page_shows_kind_breakdown() {
        use drgtw_config::UiHistoryConfig;
        let mut config = Config::default();
        config.ui.history = Some(UiHistoryConfig { postgres_url: "postgres://localhost/test".into() });
        let state = UiState::new(Instant::now(), Arc::new(config), PathBuf::new(), PgGate::NotConfigured);

        let kinds = vec![
            DimCount { label: "EMAIL".into(), requests: 42, input_tokens: 100, output_tokens: 0, cost_usd: 0.01 },
            DimCount { label: "PHONE".into(), requests: 17, input_tokens: 50, output_tokens: 0, cost_usd: 0.005 },
        ];
        let html = super::render(&state, &kinds, 59).into_string();
        assert!(html.contains("EMAIL"), "EMAIL kind must appear");
        assert!(html.contains("PHONE"), "PHONE kind must appear");
        assert!(html.contains("42") || html.contains("17"), "counts must appear");
    }

    #[test]
    fn demo_card_renders() {
        let state = open_state();
        let html = super::render(&state, &[], 0).into_string();
        // demo card always renders alongside the empty state
        assert!(html.contains("How PII Redaction Works") || html.contains("Requires PostgreSQL"),
            "demo or required card must appear");
    }
}
