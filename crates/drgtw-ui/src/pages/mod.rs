//! Page bodies and handlers for the embedded admin UI.
//!
//! Pure render functions and (for the config page) async POST handlers. Live
//! pages (dashboard, config, connections, keys, settings) derive real values
//! from the loaded config; secrets are masked via [`crate::mask`]. Every other
//! nav entry renders a polished empty state (coming-soon or Postgres-gated).

mod coming;
mod config;
mod connections;
mod dashboard;
mod keys;
mod settings;

pub use coming::{
    analytics, audit_log, cost_budgets, mcp_servers, pii_insights, rate_limits, team_access,
    traces, webhooks,
};
pub use config::{config_save, config_view};
pub use connections::connections;
pub use dashboard::dashboard;
pub use keys::virtual_keys;
pub use settings::settings;

use maud::{Markup, PreEscaped, html};

/// A reusable glass card with optional fade-rise stagger index.
pub(crate) fn glass_card(stagger: usize, inner: Markup) -> Markup {
    html! {
        div class="glass lift rise p-5" style=(format!("--i:{stagger}")) { (inner) }
    }
}

/// A key/value definition row used by the config + detail views.
pub(crate) fn kv_row(key: &str, value: Markup) -> Markup {
    html! {
        div class="grid grid-cols-[10rem_1fr] gap-x-4 gap-y-1 py-1.5 border-b border-border/60 last:border-0 text-sm" {
            div class="text-muted-foreground font-mono text-[12.5px]" { (key) }
            div class="min-w-0 break-words" { (value) }
        }
    }
}

/// Section heading inside a page body.
pub(crate) fn section_title(icon: &str, title: &str) -> Markup {
    html! {
        div class="flex items-center gap-2 mb-3 mt-1" {
            span class="size-4 grid place-items-center text-primary" { (PreEscaped(icon)) }
            h3 class="text-sm font-semibold uppercase tracking-wide text-muted-foreground" { (title) }
        }
    }
}
