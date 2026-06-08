//! `GET /ui/config` — editable configuration page.
//! `POST /ui/config/save` — save a config section form.
//!
//! The page renders sectioned forms (Server, PII, Tracing, OTel, Fallback,
//! Events, Connections, Virtual Keys). Each section is an HTML form that POSTs
//! to `/ui/config/save`. The save handler:
//!
//! 1. Reads the current TOML file via `read_document` (preserves comments +
//!    `${ENV}` literals).
//! 2. Applies changed scalar fields via `set_value`, or for array-of-tables
//!    sections splices an edited TOML textarea.
//! 3. Calls `validate_str` — on error, re-renders the form with inline
//!    `FieldError` messages; the original file is never touched.
//! 4. On success, calls `write_safe` (validate → backup → atomic rename) and
//!    shows a success banner with restart-required sections if any.
//!
//! Secret fields: if the stored value is a `${ENV}` placeholder it is shown
//! verbatim (editable). If it is a literal secret it renders as a `password`
//! input — the real value is NOT echoed into HTML; the form carries only the
//! masked display. A new value is written only when the operator types one.

use std::collections::HashMap;

use axum::Form;
use axum::extract::State;
use axum::response::Html;
use maud::{Markup, html};

use drgtw_config::{FieldError, read_document, restart_required_changes, set_value, validate_str, write_safe};

use crate::UiState;
use crate::layout::{self, Nav, badge, page_header, shell};
use crate::mask::mask_secret;
use crate::pages::{glass_card, section_title};

// ---------------------------------------------------------------------------
// GET handler
// ---------------------------------------------------------------------------

pub fn config_view(state: &UiState) -> Markup {
    render_page(state, None, None)
}

// ---------------------------------------------------------------------------
// POST handler
// ---------------------------------------------------------------------------

pub async fn config_save(
    State(state): State<UiState>,
    Form(fields): Form<HashMap<String, String>>,
) -> Html<String> {
    let section = fields.get("_section").cloned().unwrap_or_default();

    // Load current document (comment-preserving).
    let mut doc = match read_document(&state.config_path) {
        Ok(d) => d,
        Err(e) => {
            let msg = format!("Cannot read config file: {e}");
            return Html(render_page_with_error(&state, &section, &msg).into_string());
        }
    };

    // Parse current config for restart-required comparison later.
    let current_toml = doc.to_string();
    let old_config = match validate_str(&current_toml) {
        Ok(c) => c,
        Err(_) => state.config.as_ref().clone(),
    };

    // For array-of-tables sections (connections, virtual_keys) we receive a
    // single `toml_text` textarea. For scalar sections we apply set_value per field.
    let result = if let Some(toml_text) = fields.get("toml_text") {
        // Textarea mode: splice the section text into the document.
        apply_section_textarea(&mut doc, &section, toml_text)
    } else {
        // Scalar field mode: apply each submitted key.
        apply_scalar_fields(&mut doc, &section, &fields)
    };

    if let Err(e) = result {
        return Html(render_page_with_error(&state, &section, &e).into_string());
    }

    let new_toml = doc.to_string();

    // Validate before writing.
    let new_config = match validate_str(&new_toml) {
        Ok(c) => c,
        Err(errors) => {
            return Html(render_page_with_errors(&state, &section, &errors).into_string());
        }
    };

    // Atomic write (validate → backup → rename).
    if let Err(e) = write_safe(&state.config_path, &new_toml) {
        return Html(render_page_with_error(&state, &section, &format!("Write failed: {e}")).into_string());
    }

    // Compute which sections require a restart.
    let restart_sections = restart_required_changes(&old_config, &new_config);

    Html(render_page_saved(&state, &section, &restart_sections).into_string())
}

// ---------------------------------------------------------------------------
// Field application helpers
// ---------------------------------------------------------------------------

fn apply_scalar_fields(
    doc: &mut toml_edit::DocumentMut,
    section: &str,
    fields: &HashMap<String, String>,
) -> Result<(), String> {
    for (key, value) in fields {
        if key.starts_with('_') {
            continue; // skip meta-fields like _section
        }
        // Secret sentinel: if the value is the mask placeholder, skip (keep existing).
        if value == SECRET_SENTINEL {
            continue;
        }
        let dotted = format!("{section}.{key}");
        set_value(doc, &dotted, value)
            .map_err(|e| format!("Cannot set {dotted}: {e}"))?;
    }
    Ok(())
}

fn apply_section_textarea(
    doc: &mut toml_edit::DocumentMut,
    section: &str,
    toml_text: &str,
) -> Result<(), String> {
    // Parse the user-supplied snippet as a TOML document fragment.
    let snippet: toml_edit::DocumentMut = toml_text
        .parse()
        .map_err(|e| format!("TOML parse error in {section}: {e}"))?;

    // Replace the section in the main document.
    match section {
        "connections" => {
            doc.remove("connections");
            if let Some(arr) = snippet.get("connections") {
                doc.insert("connections", arr.clone());
            }
        }
        "virtual_keys" => {
            doc.remove("virtual_keys");
            if let Some(arr) = snippet.get("virtual_keys") {
                doc.insert("virtual_keys", arr.clone());
            }
        }
        _ => {
            return Err(format!("Textarea edit not supported for section `{section}`"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Sentinel value submitted by masked secret inputs when the user has not
/// changed the value. The save handler skips fields with this value.
const SECRET_SENTINEL: &str = "\x00drgtw-secret-unchanged\x00";

fn render_page(state: &UiState, success_section: Option<&str>, restart_sections: Option<&[&str]>) -> Markup {
    render_page_inner(state, success_section, restart_sections, None, None)
}

fn render_page_saved(state: &UiState, section: &str, restart_sections: &[&str]) -> Markup {
    render_page_inner(state, Some(section), Some(restart_sections), None, None)
}

fn render_page_with_error(state: &UiState, section: &str, msg: &str) -> Markup {
    let errs = vec![FieldError { path: "(document)".into(), message: msg.into() }];
    render_page_inner(state, None, None, Some(section), Some(&errs))
}

fn render_page_with_errors(state: &UiState, section: &str, errors: &[FieldError]) -> Markup {
    render_page_inner(state, None, None, Some(section), Some(errors))
}

fn render_page_inner(
    state: &UiState,
    success_section: Option<&str>,
    restart_sections: Option<&[&str]>,
    error_section: Option<&str>,
    errors: Option<&[FieldError]>,
) -> Markup {
    let cfg = &state.config;
    let unlocked = cfg.ui.history.is_some();

    // Read the live document to get raw toml_edit values (preserves ${ENV}).
    let doc = read_document(&state.config_path).ok();

    let body = html! {
        div class="flex items-start justify-between gap-4" {
            (page_header("Configuration", "Edit gateway settings. Each section saves independently. Secret placeholders (${ENV}) are shown as-is; literal secrets are masked."))
        }

        // Global success banner (top).
        @if let Some(section) = success_section {
            (success_banner(section, restart_sections.unwrap_or(&[])))
        }

        div class="grid grid-cols-1 lg:grid-cols-2 gap-4" {

            // --- Server ---
            (section_form(
                state, "server", "Server", layout::ICON_SERVER, 1,
                success_section, error_section, errors,
                html! {
                    (text_field("bind_addr", "bind_addr", &cfg.server.bind_addr.to_string(), false))
                    (number_field("max_body_bytes", "max_body_bytes", cfg.server.max_body_bytes as i64))
                }
            ))

            // --- PII ---
            (section_form(
                state, "pii", "PII", layout::ICON_SHIELD, 2,
                success_section, error_section, errors,
                html! {
                    (bool_field("enabled_by_default", "enabled_by_default", cfg.pii.enabled_by_default))
                }
            ))

            // --- Tracing ---
            (section_form(
                state, "tracing", "Tracing", layout::ICON_ROUTE, 3,
                success_section, error_section, errors,
                html! {
                    (bool_field("enabled", "enabled", cfg.tracing.enabled))
                    (text_field("dir", "dir", &cfg.tracing.dir, false))
                    (number_field("retention_days", "retention_days", cfg.tracing.retention_days as i64))
                    (number_field("rotate_max_bytes", "rotate_max_bytes", cfg.tracing.rotate_max_bytes as i64))
                }
            ))

            // --- Fallback ---
            (section_form(
                state, "fallback", "Fallback", layout::ICON_ROUTE, 4,
                success_section, error_section, errors,
                html! {
                    (bool_field("enabled", "enabled", cfg.fallback.enabled))
                }
            ))
        }

        // --- OTel (full width) ---
        div class="mt-4" {
            (section_form(
                state, "otel", "OTel", layout::ICON_GAUGE, 5,
                success_section, error_section, errors,
                html! {
                    div class="grid grid-cols-1 md:grid-cols-2 gap-x-6" {
                        div {
                            (bool_field("enabled", "enabled", cfg.otel.enabled))
                            (text_field("endpoint", "endpoint", &cfg.otel.endpoint, false))
                            (text_field("service_name", "service_name", &cfg.otel.service_name, false))
                            (number_field("export_interval_ms", "export_interval_ms", cfg.otel.export_interval_ms as i64))
                            (number_field("export_timeout_ms", "export_timeout_ms", cfg.otel.export_timeout_ms as i64))
                        }
                        div {
                            (bool_field("traces", "traces", cfg.otel.traces))
                            (bool_field("metrics", "metrics", cfg.otel.metrics))
                            (bool_field("metrics_include_key_id", "metrics_include_key_id", cfg.otel.metrics_include_key_id))
                            (float_field("sample_ratio", "sample_ratio", cfg.otel.sample_ratio as f64))
                        }
                    }
                }
            ))
        }

        // --- Events (full width, optional section) ---
        div class="mt-4" {
            @if let Some(ev) = &cfg.events {
                (section_form(
                    state, "events", "Events", layout::ICON_WEBHOOK, 6,
                    success_section, error_section, errors,
                    html! {
                        (text_field("url", "url", &ev.url, false))
                        (secret_field_opt("auth_bearer", "auth_bearer", ev.auth_bearer.as_deref()))
                        (number_field("buffer_size", "buffer_size", ev.buffer_size as i64))
                        (number_field("timeout_ms", "timeout_ms", ev.timeout_ms as i64))
                    }
                ))
            } @else {
                (glass_card(6, html! {
                    (section_title(layout::ICON_WEBHOOK, "Events"))
                    div class="text-sm text-muted-foreground py-2" {
                        "No [events] section configured. Add one to the TOML file to enable the event webhook."
                    }
                }))
            }
        }

        // --- Connections (textarea per card) ---
        div class="mt-4" {
            (connections_form(state, &doc, success_section, error_section, errors))
        }

        // --- Virtual Keys (textarea) ---
        div class="mt-4" {
            (virtual_keys_form(state, &doc, success_section, error_section, errors))
        }
    };

    shell("Configuration", "Configuration", Nav::Configuration, unlocked, cfg.ui.auth.as_ref().map(|a| a.username.as_str()), body)
}

// ---------------------------------------------------------------------------
// Section form wrapper
// ---------------------------------------------------------------------------

fn section_form(
    _state: &UiState,
    section: &str,
    title: &str,
    icon: &str,
    stagger: usize,
    success_section: Option<&str>,
    error_section: Option<&str>,
    errors: Option<&[FieldError]>,
    fields: Markup,
) -> Markup {
    let is_success = success_section == Some(section);
    let is_error = error_section == Some(section);
    let section_errors: Vec<&FieldError> = errors
        .map(|e| e.iter().filter(|fe| fe.path.starts_with(section) || fe.path == "(document)").collect())
        .unwrap_or_default();

    html! {
        (glass_card(stagger, html! {
            (section_title(icon, title))

            // Inline error banner for this section.
            @if is_error && !section_errors.is_empty() {
                div class="mb-3 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive" {
                    @for fe in &section_errors {
                        div { (fe.message.clone()) }
                    }
                }
            }

            // Success pill for this section.
            @if is_success {
                div class="mb-3" { (badge("ok", "Saved")) }
            }

            form method="post" action="/ui/config/save" {
                input type="hidden" name="_section" value=(section);
                div class="flex flex-col gap-0" {
                    (fields)
                }
                div class="mt-4" {
                    button
                        type="submit"
                        class="inline-flex items-center gap-1.5 rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90 transition-colors"
                        { "Save " (title) }
                }
            }
        }))
    }
}

// ---------------------------------------------------------------------------
// Connections textarea form
// ---------------------------------------------------------------------------

fn connections_form(
    state: &UiState,
    doc: &Option<toml_edit::DocumentMut>,
    success_section: Option<&str>,
    error_section: Option<&str>,
    errors: Option<&[FieldError]>,
) -> Markup {
    let cfg = &state.config;
    let is_success = success_section == Some("connections");
    let is_error = error_section == Some("connections");
    let section_errors: Vec<&FieldError> = errors
        .map(|e| e.iter().filter(|fe| fe.path.starts_with("connections") || fe.path == "(document)").collect())
        .unwrap_or_default();

    // Build the current connections TOML snippet from the live document, or fall
    // back to a redacted reconstruction (api_key as placeholder).
    let connections_toml = doc
        .as_ref()
        .and_then(|d| d.get("connections"))
        .map(|v| {
            let mut tmp = toml_edit::DocumentMut::new();
            tmp.insert("connections", v.clone());
            tmp.to_string()
        })
        .unwrap_or_else(|| {
            // Reconstruct from parsed config, using mask_secret for api_key.
            let mut out = String::new();
            for conn in &cfg.connections {
                out.push_str("[[connections]]\n");
                out.push_str(&format!("name = {:?}\n", conn.name));
                out.push_str(&format!("base_url = {:?}\n", conn.base_url));
                // Use the raw value from doc if available, else show placeholder.
                out.push_str(&format!("api_key = {:?}\n", mask_secret(&conn.api_key)));
                out.push_str(&format!("format = {:?}\n", format!("{:?}", conn.format).to_ascii_lowercase()));
                if !conn.models.is_empty() {
                    out.push_str(&format!("models = {:?}\n", conn.models));
                }
                out.push('\n');
            }
            out
        });

    html! {
        (glass_card(7, html! {
            (section_title(layout::ICON_PLUG, "Connections"))

            @if is_error && !section_errors.is_empty() {
                div class="mb-3 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive" {
                    @for fe in &section_errors {
                        div { (fe.message.clone()) }
                    }
                }
            }
            @if is_success {
                div class="mb-3" { (badge("ok", "Saved")) }
            }

            p class="text-xs text-muted-foreground mb-3" {
                "Edit the full [[connections]] TOML below. "
                code class="font-mono" { "${ENV_VAR}" }
                " placeholders are preserved verbatim; literal api_key values are shown as-is from the file."
            }

            form method="post" action="/ui/config/save" {
                input type="hidden" name="_section" value="connections";
                textarea
                    name="toml_text"
                    rows="16"
                    class="w-full font-mono text-xs rounded-md border border-border bg-card/60 px-3 py-2 resize-y focus:outline-none focus:ring-1 focus:ring-primary"
                    spellcheck="false"
                    { (connections_toml) }
                div class="mt-3" {
                    button
                        type="submit"
                        class="inline-flex items-center gap-1.5 rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90 transition-colors"
                        { "Save Connections" }
                }
            }
        }))
    }
}

// ---------------------------------------------------------------------------
// Virtual keys textarea form
// ---------------------------------------------------------------------------

fn virtual_keys_form(
    state: &UiState,
    _doc: &Option<toml_edit::DocumentMut>,
    success_section: Option<&str>,
    error_section: Option<&str>,
    errors: Option<&[FieldError]>,
) -> Markup {
    let cfg = &state.config;
    let is_success = success_section == Some("virtual_keys");
    let is_error = error_section == Some("virtual_keys");
    let section_errors: Vec<&FieldError> = errors
        .map(|e| e.iter().filter(|fe| fe.path.starts_with("virtual_keys") || fe.path == "(document)").collect())
        .unwrap_or_default();

    // Virtual keys are secrets — always reconstruct with masked key values.
    // Operators must type a full new key (sk-drgtw-…) to rotate; masked keys
    // are rejected by validate_str so accidental submission of a masked value
    // returns a clear validation error rather than writing garbage.
    let vkeys_toml = {
        let mut out = String::new();
        for vk in &cfg.virtual_keys {
            out.push_str("[[virtual_keys]]\n");
            out.push_str(&format!("key = {:?}\n", mask_secret(&vk.key)));
            out.push_str(&format!("connections = {:?}\n", vk.connections));
            if let Some(models) = &vk.models {
                out.push_str(&format!("models = {:?}\n", models));
            }
            out.push('\n');
        }
        out
    };

    html! {
        (glass_card(8, html! {
            (section_title(layout::ICON_KEY, "Virtual Keys"))

            @if is_error && !section_errors.is_empty() {
                div class="mb-3 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive" {
                    @for fe in &section_errors {
                        div { (fe.message.clone()) }
                    }
                }
            }
            @if is_success {
                div class="mb-3" { (badge("ok", "Saved")) }
            }

            p class="text-xs text-muted-foreground mb-3" {
                "Edit [[virtual_keys]] entries below. Keys must start with "
                code class="font-mono" { "sk-drgtw-" }
                "."
            }

            form method="post" action="/ui/config/save" {
                input type="hidden" name="_section" value="virtual_keys";
                textarea
                    name="toml_text"
                    rows="10"
                    class="w-full font-mono text-xs rounded-md border border-border bg-card/60 px-3 py-2 resize-y focus:outline-none focus:ring-1 focus:ring-primary"
                    spellcheck="false"
                    { (vkeys_toml) }
                div class="mt-3" {
                    button
                        type="submit"
                        class="inline-flex items-center gap-1.5 rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90 transition-colors"
                        { "Save Virtual Keys" }
                }
            }
        }))
    }
}

// ---------------------------------------------------------------------------
// Field helpers
// ---------------------------------------------------------------------------

fn text_field(label: &str, name: &str, value: &str, secret: bool) -> Markup {
    let input_type = if secret { "password" } else { "text" };
    html! {
        div class="grid grid-cols-[10rem_1fr] gap-x-4 gap-y-1 py-1.5 border-b border-border/60 last:border-0 text-sm" {
            label class="text-muted-foreground font-mono text-[12.5px] self-center" for=(name) { (label) }
            input
                type=(input_type)
                id=(name)
                name=(name)
                value=(if secret && !value.contains("${") { SECRET_SENTINEL } else { value })
                placeholder=(if secret && !value.contains("${") { mask_secret(value) } else { String::new() })
                class="font-mono text-[12.5px] rounded border border-border/60 bg-card/60 px-2 py-1 focus:outline-none focus:ring-1 focus:ring-primary w-full";
        }
    }
}

fn secret_field_opt(label: &str, name: &str, value: Option<&str>) -> Markup {
    match value {
        Some(v) => {
            let is_placeholder = v.contains("${");
            let display = if is_placeholder { v.to_owned() } else { SECRET_SENTINEL.to_owned() };
            let placeholder = if is_placeholder { String::new() } else { mask_secret(v) };
            html! {
                div class="grid grid-cols-[10rem_1fr] gap-x-4 gap-y-1 py-1.5 border-b border-border/60 last:border-0 text-sm" {
                    label class="text-muted-foreground font-mono text-[12.5px] self-center" for=(name) { (label) }
                    input
                        type="password"
                        id=(name)
                        name=(name)
                        value=(display)
                        placeholder=(placeholder)
                        class="font-mono text-[12.5px] rounded border border-border/60 bg-card/60 px-2 py-1 focus:outline-none focus:ring-1 focus:ring-primary w-full";
                }
            }
        }
        None => html! {
            div class="grid grid-cols-[10rem_1fr] gap-x-4 gap-y-1 py-1.5 border-b border-border/60 last:border-0 text-sm" {
                label class="text-muted-foreground font-mono text-[12.5px] self-center" for=(name) { (label) }
                span class="text-muted-foreground text-[12.5px]" { "— not configured —" }
            }
        },
    }
}

fn number_field(label: &str, name: &str, value: i64) -> Markup {
    html! {
        div class="grid grid-cols-[10rem_1fr] gap-x-4 gap-y-1 py-1.5 border-b border-border/60 last:border-0 text-sm" {
            label class="text-muted-foreground font-mono text-[12.5px] self-center" for=(name) { (label) }
            input
                type="number"
                id=(name)
                name=(name)
                value=(value)
                class="font-mono text-[12.5px] tnum rounded border border-border/60 bg-card/60 px-2 py-1 focus:outline-none focus:ring-1 focus:ring-primary w-full";
        }
    }
}

fn float_field(label: &str, name: &str, value: f64) -> Markup {
    html! {
        div class="grid grid-cols-[10rem_1fr] gap-x-4 gap-y-1 py-1.5 border-b border-border/60 last:border-0 text-sm" {
            label class="text-muted-foreground font-mono text-[12.5px] self-center" for=(name) { (label) }
            input
                type="number"
                id=(name)
                name=(name)
                value=(value)
                step="0.01"
                min="0"
                max="1"
                class="font-mono text-[12.5px] tnum rounded border border-border/60 bg-card/60 px-2 py-1 focus:outline-none focus:ring-1 focus:ring-primary w-full";
        }
    }
}

fn bool_field(label: &str, name: &str, value: bool) -> Markup {
    html! {
        div class="grid grid-cols-[10rem_1fr] gap-x-4 gap-y-1 py-1.5 border-b border-border/60 last:border-0 text-sm" {
            label class="text-muted-foreground font-mono text-[12.5px] self-center" for=(name) { (label) }
            select
                id=(name)
                name=(name)
                class="font-mono text-[12.5px] rounded border border-border/60 bg-card/60 px-2 py-1 focus:outline-none focus:ring-1 focus:ring-primary w-full"
            {
                option value="true" selected[value] { "true" }
                option value="false" selected[!value] { "false" }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Success / error banners
// ---------------------------------------------------------------------------

fn success_banner(section: &str, restart_sections: &[&str]) -> Markup {
    html! {
        div class="mb-4 rounded-md border border-ok/40 bg-ok/10 px-4 py-3 text-sm" {
            div class="font-medium text-ok" { "Saved." }
            @if !restart_sections.is_empty() {
                // TODO(hot-reload): live reload of model_aliases/events/tracing/otel without restart.
                div class="mt-1 text-muted-foreground" {
                    "Restart required for: "
                    span class="font-mono" { (restart_sections.join(", ")) }
                    ". Changes take effect on next gateway start."
                }
            }
            div class="mt-1 text-muted-foreground" {
                "Section: " span class="font-mono" { (section) }
            }
        }
    }
}

