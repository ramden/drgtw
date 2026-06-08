//! Polished empty-state pages for nav entries that are not yet backed by data.
//!
//! Two flavours:
//! - **Coming soon** (◐) — the feature is planned; renders an amber badge.
//! - **Requires PostgreSQL** (🔒) — needs `[ui.history]`. When history IS
//!   configured the page flips to a "connected — coming soon" state; otherwise
//!   it shows the config snippet to unlock it.
//!
//! Every description is specific to what the feature will do in drgtw.

use maud::{Markup, PreEscaped, html};

use crate::UiState;
use crate::layout::{self, Nav, empty_state, page_header, shell};

// --------------------------------------------------------------- coming soon

fn soon(state: &UiState, nav: Nav, title: &str, subtitle: &str, icon: &str, desc: Markup) -> Markup {
    let unlocked = state.config.ui.history.is_some();
    let username = state.config.ui.auth.as_ref().map(|a| a.username.as_str());
    let body = html! {
        (page_header(title, subtitle))
        (empty_state(icon, "warn", "◐ Coming soon", title, desc))
    };
    shell(title, title, nav, unlocked, username, body)
}

pub fn pii_insights(state: &UiState) -> Markup {
    soon(
        state, Nav::PiiInsights, "PII Insights",
        "Visibility into what the redaction engine catches.",
        layout::ICON_SHIELD,
        html! {
            "See which entity types are detected across your traffic — emails, phone numbers, "
            "IBANs, credit cards, and your custom recognizers — with per-type redaction rates "
            "and trends over time. Spot recognizers that never fire and tune coverage before it matters."
        },
    )
}

pub fn cost_budgets(state: &UiState) -> Markup {
    soon(
        state, Nav::CostBudgets, "Cost & Budgets",
        "Track spend and cap it per key.",
        layout::ICON_COINS,
        html! {
            "Attribute USD spend by virtual key, connection, and model using the per-model cost "
            "table. Set hard and soft budgets, watch burn-down, and get alerted before a key "
            "blows its monthly cap."
        },
    )
}

pub fn rate_limits(state: &UiState) -> Markup {
    soon(
        state, Nav::RateLimits, "Rate Limits",
        "Throttle per key, per model, per window.",
        layout::ICON_GAUGE2,
        html! {
            "Configure requests-per-minute and concurrency limits for each virtual key, "
            "preview which keys are approaching their ceiling, and inspect recent throttle "
            "events without leaving the console."
        },
    )
}

pub fn mcp_servers(state: &UiState) -> Markup {
    soon(
        state, Nav::McpServers, "MCP Servers",
        "Manage aggregated upstream MCP endpoints.",
        layout::ICON_SERVER,
        html! {
            "Browse the Model Context Protocol servers the gateway aggregates, inspect the "
            "tools each one exposes, check upstream auth and reachability, and review the "
            "merged tool catalogue clients see through a single endpoint."
        },
    )
}

pub fn webhooks(state: &UiState) -> Markup {
    soon(
        state, Nav::Webhooks, "Webhooks",
        "React to gateway events downstream.",
        layout::ICON_WEBHOOK,
        html! {
            "Register HTTPS endpoints that fire on budget thresholds, key changes, and PII "
            "detections. Inspect delivery attempts, replay failures, and rotate signing "
            "secrets — wired to the existing event sink."
        },
    )
}

pub fn team_access(state: &UiState) -> Markup {
    soon(
        state, Nav::TeamAccess, "Team & Access",
        "SSO and role-based console access.",
        layout::ICON_USERS,
        html! {
            "Invite operators, assign roles, and connect your identity provider over SAML or "
            "OIDC for single sign-on. Every console action will be attributable to a real "
            "person and surfaced in the audit log."
        },
    )
}

// ------------------------------------------------------------ Postgres-gated

fn postgres_page(
    state: &UiState,
    nav: Nav,
    title: &str,
    subtitle: &str,
    icon: &str,
    desc: Markup,
) -> Markup {
    let history = &state.config.ui.history;
    let unlocked = history.is_some();

    let body = html! {
        (page_header(title, subtitle))
        @match history {
            // Unlocked: section present → "connected, coming soon".
            Some(h) => {
                (empty_state(icon, "ok", "Connected · coming soon", title, html! {
                    p { (desc) }
                    div class="mt-5 flex justify-center" {
                        span class="inline-flex items-center gap-2 text-xs text-muted-foreground font-mono badge-muted rounded-lg px-3 py-1.5" {
                            span class="live-dot" {}
                            (crate::mask::mask_url(&h.postgres_url))
                        }
                    }
                }))
            },
            // Locked: show how to unlock.
            None => {
                (empty_state(layout::ICON_DATABASE, "muted", "🔒 Requires PostgreSQL", title, html! {
                    p { (desc) }
                    div class="mt-5 flex flex-col items-center" {
                        div class="text-xs text-muted-foreground mb-1.5" { "Configure " code class="font-mono" { "[ui.history]" } " in drgtw.toml:" }
                        pre class="glass rounded-lg p-3 text-left text-[12.5px] font-mono leading-relaxed" {
                            (PreEscaped("<span class=\"text-primary\">[ui.history]</span>\npostgres_url = <span style=\"color:var(--ok)\">\"${DATABASE_URL}\"</span>"))
                        }
                    }
                }))
            },
        }
    };

    let username = state.config.ui.auth.as_ref().map(|a| a.username.as_str());
    shell(title, title, nav, unlocked, username, body)
}

pub fn analytics(state: &UiState) -> Markup {
    postgres_page(
        state, Nav::Analytics, "Analytics",
        "Token, cost, and latency trends over time.",
        layout::ICON_CHART,
        html! {
            "Long-range analytics over your request history: token throughput, USD cost, and "
            "latency percentiles broken down by key, model, and connection. Backed by the "
            "PostgreSQL history store."
        },
    )
}

pub fn traces(state: &UiState) -> Markup {
    postgres_page(
        state, Nav::Traces, "Traces",
        "Browse individual request traces.",
        layout::ICON_ROUTE,
        html! {
            "Browse the JSONL request traces the gateway writes — search by key, model, or "
            "status, and drill into a single request's redaction, routing, and upstream "
            "timing. Indexed in PostgreSQL for fast lookup."
        },
    )
}

pub fn audit_log(state: &UiState) -> Markup {
    postgres_page(
        state, Nav::AuditLog, "Audit Log",
        "Every config and key change — who and when.",
        layout::ICON_SCROLL,
        html! {
            "A tamper-evident record of every configuration edit, virtual-key creation, and "
            "access change: what changed, who changed it, and exactly when. Persisted to "
            "PostgreSQL for compliance and forensics."
        },
    )
}
