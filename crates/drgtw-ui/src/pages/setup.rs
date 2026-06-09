//! Locked-mode setup page.
//!
//! Rendered for every path when the UI has no live Postgres history store
//! (see [`crate::PgGate`]). It explains, per gate variant, exactly what to do
//! to unlock the console. Styled with the same `shell` + `empty_state`
//! primitives as the rest of the UI so the locked state still looks polished
//! (and `/ui/assets/*` keeps serving the CSS/JS).

use maud::{Markup, PreEscaped, html};

use crate::layout::{Nav, empty_state, page_header, shell, ICON_DATABASE};
use crate::{PgGate, UiState};

/// The `[ui.history]` TOML snippet, copied verbatim from the Postgres-gated
/// coming-soon pages so the unlock instructions are consistent across the UI.
fn history_snippet() -> Markup {
    html! {
        div class="mt-5 flex flex-col items-center" {
            div class="text-xs text-muted-foreground mb-1.5" {
                "Add " code class="font-mono" { "[ui.history]" } " to drgtw.toml, set " code class="font-mono" { "DATABASE_URL" } ", and restart:"
            }
            pre class="glass rounded-lg p-3 text-left text-[12.5px] font-mono leading-relaxed" {
                (PreEscaped("<span class=\"text-primary\">[ui.history]</span>\npostgres_url = <span style=\"color:var(--ok)\">\"${DATABASE_URL}\"</span>"))
            }
        }
    }
}

/// Render the locked-mode setup page for the current gate.
///
/// Variants:
/// - [`PgGate::NotConfigured`] → "Requires PostgreSQL" + the `[ui.history]`
///   snippet + restart hint.
/// - [`PgGate::Unreachable`] → "Cannot reach PostgreSQL" + the masked URL +
///   "check the database and DATABASE_URL, then restart".
/// - [`PgGate::FeatureOff`] → "UI built without Postgres support" + rebuild hint.
/// - [`PgGate::Connected`] → never reached in locked mode; renders the
///   NotConfigured copy defensively rather than panicking.
pub fn setup_page(state: &UiState) -> Markup {
    let (badge_kind, badge_label, title, body): (&str, &str, &str, Markup) = match &state.gate {
        PgGate::NotConfigured | PgGate::Connected(_) => (
            "warn",
            "🔒 Requires PostgreSQL",
            "Requires PostgreSQL",
            html! {
                p {
                    "The admin console needs a PostgreSQL history store. "
                    "PostgreSQL required — without it the gateway still proxies traffic, "
                    "but the UI stays locked."
                }
                (history_snippet())
            },
        ),
        PgGate::Unreachable { masked_url } => (
            "down",
            "🔒 Cannot reach PostgreSQL",
            "Cannot reach PostgreSQL",
            html! {
                p {
                    "The configured PostgreSQL history store could not be reached at boot. "
                    "Check the database is running and " code class="font-mono" { "DATABASE_URL" }
                    " is correct, then restart the gateway."
                }
                div class="mt-5 flex justify-center" {
                    span class="inline-flex items-center gap-2 text-xs text-muted-foreground font-mono badge-muted rounded-lg px-3 py-1.5" {
                        (masked_url.clone())
                    }
                }
            },
        ),
        PgGate::FeatureOff => (
            "down",
            "🔒 No Postgres support",
            "UI built without Postgres support",
            html! {
                p {
                    "This binary was compiled without the Postgres history store, "
                    "so the admin console cannot connect to a database. "
                    "Rebuild with default features (the " code class="font-mono" { "ui" } " feature) to enable it."
                }
                div class="mt-5 flex flex-col items-center" {
                    pre class="glass rounded-lg p-3 text-left text-[12.5px] font-mono leading-relaxed" {
                        (PreEscaped("cargo build --release  <span class=\"text-muted-foreground\"># default features include `ui`</span>"))
                    }
                }
            },
        ),
    };

    let username = state.config.ui.auth.as_ref().map(|a| a.username.as_str());
    let body = html! {
        (page_header("Setup required", "Connect a PostgreSQL history store to unlock the console."))
        (empty_state(ICON_DATABASE, badge_kind, badge_label, title, body))
    };
    // `history_unlocked = false`: the sidebar shows the Postgres-gated entries
    // as locked, matching the inert state.
    shell("Setup", "Setup", Nav::Dashboard, false, username, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    fn state_with(gate: PgGate) -> UiState {
        // A minimal default Config is enough — the setup page only reads
        // `config.ui.auth`, which is None by default.
        let config = Arc::new(drgtw_config::Config::default());
        UiState::new(Instant::now(), config, PathBuf::new(), gate)
    }

    #[test]
    fn not_configured_shows_requires_postgres_and_snippet() {
        let html = setup_page(&state_with(PgGate::NotConfigured)).into_string();
        assert!(html.contains("Requires PostgreSQL"), "title present");
        assert!(html.contains("PostgreSQL required"), "spec phrase present");
        assert!(html.contains("[ui.history]"), "unlock snippet present");
        assert!(html.contains("${DATABASE_URL}"), "DATABASE_URL placeholder present");
    }

    #[test]
    fn unreachable_shows_masked_url_and_check_hint() {
        let gate = PgGate::Unreachable {
            masked_url: "postgres://user:••••@db:5432/x".to_owned(),
        };
        let html = setup_page(&state_with(gate)).into_string();
        assert!(html.contains("Cannot reach PostgreSQL"), "title present");
        assert!(html.contains("postgres://user:••••@db:5432/x"), "masked url shown");
        assert!(html.contains("DATABASE_URL"), "check hint mentions DATABASE_URL");
        // The password must never leak even here.
        assert!(!html.contains(":secret@"), "password must stay masked");
    }

    #[test]
    fn feature_off_shows_rebuild_hint() {
        let html = setup_page(&state_with(PgGate::FeatureOff)).into_string();
        assert!(html.contains("without Postgres support"), "title present");
        assert!(html.contains("ui"), "mentions the ui feature");
        assert!(html.contains("default features"), "rebuild hint present");
    }
}
