//! `GET /ui/settings` — a few inert-but-real-looking setting rows (appearance,
//! theme toggle, telemetry, version). Nothing here mutates state in the concept.

use maud::{Markup, PreEscaped, html};

use crate::UiState;
use crate::layout::{self, Nav, badge, page_header, shell};
use crate::pages::{glass_card, section_title};

pub fn settings(state: &UiState) -> Markup {
    let cfg = &state.config;
    let unlocked = cfg.ui.history.is_some();

    let body = html! {
        (page_header("Settings", "Console preferences and gateway metadata."))

        div class="grid grid-cols-1 gap-4 max-w-2xl" {

            (glass_card(1, html! {
                (section_title(layout::ICON_SUN_MOON, "Appearance"))
                (setting_row(
                    "Theme",
                    "Dark is the default. Switch to light any time — your choice is remembered locally.",
                    html! {
                        button type="button" class="btn-outline btn-sm inline-flex items-center gap-2" onclick="window.__drgtwToggleTheme()" {
                            span class="size-4 grid place-items-center" { (PreEscaped(layout::ICON_SUN_MOON)) }
                            "Toggle theme"
                            span class="kbd" { "⌥T" }
                        }
                    }
                ))
                (setting_row(
                    "Sidebar",
                    "Collapse the navigation to icons-only with the toggle in the header bar.",
                    html! { (badge("muted", "header control")) }
                ))
            }))

            (glass_card(2, html! {
                (section_title(layout::ICON_SHIELD, "Privacy & telemetry"))
                (setting_row(
                    "PII redaction default",
                    "When clients send no x-drgtw-pii header, redaction follows this default.",
                    html! { (if cfg.pii.enabled_by_default { badge("ok", "on by default") } else { badge("muted", "off by default") }) }
                ))
                (setting_row(
                    "Anonymous usage stats",
                    "Send aggregated, non-identifying usage counts. Editable soon.",
                    html! { (toggle_pill(false)) }
                ))
            }))

            (glass_card(3, html! {
                (section_title(layout::ICON_COG, "About"))
                (setting_row(
                    "Version",
                    "Build of the gateway currently serving this console.",
                    html! { span class="font-mono text-[12.5px]" { "drgtw " (env!("CARGO_PKG_VERSION")) } }
                ))
                (setting_row(
                    "Listening on",
                    "Bind address from the loaded configuration.",
                    html! { span class="font-mono text-[12.5px]" { (cfg.server.bind_addr.to_string()) } }
                ))
            }))
        }
    };

    shell("Settings", "Settings", Nav::Settings, unlocked, cfg.ui.auth.as_ref().map(|a| a.username.as_str()), body)
}

fn setting_row(title: &str, desc: &str, control: Markup) -> Markup {
    html! {
        div class="flex items-center justify-between gap-6 py-3.5 border-b border-border/60 last:border-0" {
            div class="min-w-0" {
                div class="text-sm font-medium" { (title) }
                div class="text-xs text-muted-foreground mt-0.5" { (desc) }
            }
            div class="shrink-0" { (control) }
        }
    }
}

/// A decorative on/off pill (inert in the concept).
fn toggle_pill(on: bool) -> Markup {
    let cls = if on { "btn-brand" } else { "badge-muted" };
    html! {
        span class=(format!("inline-flex items-center rounded-full px-2.5 py-0.5 text-[11px] font-medium {cls}")) {
            (if on { "Enabled" } else { "Disabled" })
        }
    }
}
