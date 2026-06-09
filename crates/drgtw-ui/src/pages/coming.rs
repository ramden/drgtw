//! Polished empty-state pages for nav entries that are not yet backed by data.
//!
//! These are the **coming soon** (◐) features — planned but not yet built; each
//! renders an amber badge. The Postgres-backed Observe pages (Analytics,
//! Traces, Audit) live in their own modules and query the history store.
//!
//! Every description is specific to what the feature will do in drgtw.

use maud::{Markup, html};

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

