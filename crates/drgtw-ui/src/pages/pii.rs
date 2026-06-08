//! `GET /ui/pii`  — PII Playground page.
//! `POST /ui/pii/scan` — pseudonymize submitted text and return the result panel.
//!
//! The page lets operators configure which recognizers are active, paste sample
//! text, and watch the PII engine pseudonymize it in real time. The response
//! also restores the placeholders back to the originals to prove reversibility.
//!
//! NER (Person/Org/Location) requires an ONNX model on disk (`[pii.ner]`) and
//! is intentionally NOT enabled here even when configured — building a per-
//! request NER engine from scratch would block the request thread for seconds.
//! The toggle is shown but permanently disabled with a hint.

use axum::Form;
use axum::extract::State;
use axum::response::Html;
use drgtw_config::{CustomRecognizer, PiiConfig};
use drgtw_pii::{EntityMap, PiiEngine};
use maud::{Markup, PreEscaped, html};
use serde::Deserialize;

use crate::UiState;
use crate::layout::{self, Nav, page_header, shell};
use crate::pages::{glass_card, section_title};

// ---------------------------------------------------------------------------
// Prefilled example text
// ---------------------------------------------------------------------------

const EXAMPLE_TEXT: &str = "Email max.mustermann@example.com or call +49 151 23456789. \
IBAN DE89370400440532013000, card 4111 1111 1111 1111. \
— Max Mustermann, Example Corp.";

// ---------------------------------------------------------------------------
// GET handler
// ---------------------------------------------------------------------------

pub fn pii_playground(state: &UiState) -> Markup {
    let unlocked = state.config.ui.history.is_some();
    let username = state.config.ui.auth.as_ref().map(|a| a.username.as_str());
    let body = html! {
        (page_header("PII Playground", "Configure recognizers, paste text, watch entities get pseudonymized."))
        (render_playground(state, EXAMPLE_TEXT, None))
    };
    shell("PII Playground", "PII Playground", Nav::PiiInsights, unlocked, username, body)
}

// ---------------------------------------------------------------------------
// POST handler form data
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ScanForm {
    text: String,
    // Built-in toggle checkboxes — absent = disabled (HTML checkbox behaviour).
    #[serde(default)]
    enable_email: bool,
    #[serde(default)]
    enable_phone: bool,
    #[serde(default)]
    enable_iban: bool,
    #[serde(default)]
    enable_card: bool,
    // Temporary custom rule (name + regex), session-only.
    #[serde(default)]
    tmp_rule_name: String,
    #[serde(default)]
    tmp_rule_pattern: String,
}

// ---------------------------------------------------------------------------
// POST handler
// ---------------------------------------------------------------------------

pub async fn pii_scan(
    State(state): State<UiState>,
    Form(form): Form<ScanForm>,
) -> Html<String> {
    let result = run_scan(&state, &form);
    Html(render_playground_with_result(&state, &form, result).into_string())
}

// ---------------------------------------------------------------------------
// Engine execution
// ---------------------------------------------------------------------------

struct ScanResult {
    pseudonymized: String,
    restored: String,
    /// (placeholder, kind_label, original)
    mapping: Vec<(String, String, String)>,
    /// (kind_label, count)
    summary: Vec<(String, usize)>,
}

fn run_scan(state: &UiState, form: &ScanForm) -> Result<ScanResult, String> {
    let pii_cfg = build_pii_config(state, form)?;
    let engine = PiiEngine::from_config(&pii_cfg).map_err(|e| e.to_string())?;

    let detections = engine.scan(&form.text);
    let mut map = EntityMap::new();
    let pseudonymized = map.pseudonymize(&form.text, &detections);
    let restored = map.restore(&pseudonymized);

    // Collect mapping sorted by placeholder for stable display.
    let mut pairs: Vec<(String, String)> = map
        .iter()
        .map(|(ph, orig)| (ph.to_owned(), orig.to_owned()))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));

    // Derive kind label from placeholder prefix (e.g. "EMAIL_1" → "Email").
    let mapping: Vec<(String, String, String)> = pairs
        .into_iter()
        .map(|(ph, orig)| {
            let kind = kind_label_from_placeholder(&ph);
            (ph, kind, orig)
        })
        .collect();

    // Count per kind.
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (_, kind, _) in &mapping {
        *counts.entry(kind.clone()).or_insert(0) += 1;
    }
    let mut summary: Vec<(String, usize)> = counts.into_iter().collect();
    summary.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(ScanResult { pseudonymized, restored, mapping, summary })
}

fn build_pii_config(state: &UiState, form: &ScanForm) -> Result<PiiConfig, String> {
    let mut disabled: Vec<String> = Vec::new();
    if !form.enable_email { disabled.push("email".into()); }
    if !form.enable_phone { disabled.push("phone".into()); }
    if !form.enable_iban  { disabled.push("iban".into());  }
    if !form.enable_card  { disabled.push("credit_card".into()); }

    // Start from config's custom recognizers (read-only originals), then add
    // any temporary rule from the form.
    let mut custom: Vec<CustomRecognizer> = state.config.pii.custom_recognizers.clone();
    if !form.tmp_rule_name.trim().is_empty() && !form.tmp_rule_pattern.trim().is_empty() {
        custom.push(CustomRecognizer {
            name: form.tmp_rule_name.trim().to_owned(),
            pattern: form.tmp_rule_pattern.trim().to_owned(),
        });
    }

    Ok(PiiConfig {
        enabled_by_default: true,
        disabled_recognizers: disabled,
        custom_recognizers: custom,
        // NER omitted intentionally — ONNX model loading is too heavy for
        // per-request use in a playground.
        ner: None,
        vault: None,
        embeddings_require_vault: false,
    })
}

/// Derive a human-readable kind label from a placeholder like "EMAIL_1".
fn kind_label_from_placeholder(ph: &str) -> String {
    let prefix = ph.rfind('_').map(|i| &ph[..i]).unwrap_or(ph);
    match prefix {
        "EMAIL"    => "Email".into(),
        "PHONE"    => "Phone".into(),
        "IBAN"     => "IBAN".into(),
        "CARD"     => "Credit card".into(),
        "PERSON"   => "Person".into(),
        "ORG"      => "Organisation".into(),
        "LOCATION" => "Location".into(),
        other      => {
            let mut s = other.to_lowercase();
            if let Some(c) = s.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            s
        }
    }
}

// ---------------------------------------------------------------------------
// Markup helpers
// ---------------------------------------------------------------------------

/// Full two-column layout: controls (left) + empty result panel (right).
fn render_playground(state: &UiState, text: &str, result: Option<Result<ScanResult, String>>) -> Markup {
    let form = ScanForm {
        text: text.to_owned(),
        enable_email: true,
        enable_phone: true,
        enable_iban:  true,
        enable_card:  true,
        tmp_rule_name: String::new(),
        tmp_rule_pattern: String::new(),
    };
    render_playground_with_result(state, &form, result.unwrap_or(Ok(ScanResult {
        pseudonymized: String::new(),
        restored: String::new(),
        mapping: Vec::new(),
        summary: Vec::new(),
    })))
}

fn render_playground_with_result(state: &UiState, form: &ScanForm, result: Result<ScanResult, String>) -> Markup {
    html! {
        div class="grid grid-cols-1 lg:grid-cols-2 gap-6 items-start" {
            // ── Left column: controls ─────────────────────────────────────
            (glass_card(1, render_controls(state, form)))
            // ── Right column: results ─────────────────────────────────────
            (glass_card(2, render_results(form, result)))
        }
    }
}

fn render_controls(state: &UiState, form: &ScanForm) -> Markup {
    let has_ner = state.config.pii.ner.is_some();
    html! {
        (section_title(layout::ICON_SHIELD, "Controls"))

        // Value proposition
        div class="mb-5 p-3 rounded-lg bg-primary/10 border border-primary/20 text-sm text-foreground leading-relaxed" {
            span class="font-semibold text-primary" { "How it works: " }
            "The provider only ever sees the placeholders. Your app gets the real values back."
        }

        form method="post" action="/ui/pii/scan" class="flex flex-col gap-5" {

            // Recognizer toggles
            div {
                h4 class="text-xs font-semibold uppercase tracking-wide text-muted-foreground mb-3" { "Built-in recognizers" }
                div class="flex flex-col gap-2" {
                    (toggle("enable_email", "Email", "EMAIL", form.enable_email))
                    (toggle("enable_phone", "Phone", "PHONE", form.enable_phone))
                    (toggle("enable_iban",  "IBAN",  "IBAN",  form.enable_iban))
                    (toggle("enable_card",  "Credit card", "CARD", form.enable_card))
                }
                // NER toggle — always disabled in playground
                div class="flex items-center justify-between py-2 opacity-50 cursor-not-allowed select-none" {
                    div class="flex items-center gap-2" {
                        input type="checkbox" disabled class="rounded border-border" id="ner_toggle";
                        label for="ner_toggle" class="text-sm font-medium text-muted-foreground cursor-not-allowed" {
                            "Person / Org / Location (NER)"
                        }
                    }
                    span class="text-[11px] text-muted-foreground font-mono" {
                        @if has_ner { "model found — disabled (slow)" } @else { "requires [pii.ner]" }
                    }
                }
            }

            // Config custom recognizers (read-only)
            @if !state.config.pii.custom_recognizers.is_empty() {
                div {
                    h4 class="text-xs font-semibold uppercase tracking-wide text-muted-foreground mb-2" {
                        "Custom recognizers (from config)"
                    }
                    div class="flex flex-col gap-1.5" {
                        @for cr in &state.config.pii.custom_recognizers {
                            div class="flex items-center justify-between rounded-md px-3 py-1.5 bg-accent/30 border border-border/60 text-sm" {
                                span class="font-medium" { (cr.name.to_uppercase()) }
                                span class="font-mono text-[11px] text-muted-foreground truncate max-w-[14rem]" { (cr.pattern.clone()) }
                            }
                        }
                    }
                    a href="/ui/config#pii" class="text-xs text-primary underline-offset-2 hover:underline mt-1 inline-block" {
                        "Edit rules in Configuration → [pii]"
                    }
                }
            }

            // Temporary custom rule
            div {
                h4 class="text-xs font-semibold uppercase tracking-wide text-muted-foreground mb-2" {
                    "Add a temporary rule (this session only)"
                }
                div class="flex flex-col gap-2" {
                    div class="flex flex-col gap-1" {
                        label class="text-[12px] text-muted-foreground font-mono" for="tmp_rule_name" { "Name" }
                        input
                            type="text"
                            id="tmp_rule_name"
                            name="tmp_rule_name"
                            value=(form.tmp_rule_name)
                            placeholder="e.g. TICKET"
                            class="w-full rounded-md border border-border bg-background px-3 py-1.5 text-sm font-mono focus:outline-none focus:ring-1 focus:ring-primary/60";
                    }
                    div class="flex flex-col gap-1" {
                        label class="text-[12px] text-muted-foreground font-mono" for="tmp_rule_pattern" { "Regex pattern" }
                        input
                            type="text"
                            id="tmp_rule_pattern"
                            name="tmp_rule_pattern"
                            value=(form.tmp_rule_pattern)
                            placeholder=r"e.g. TICK-\d+"
                            class="w-full rounded-md border border-border bg-background px-3 py-1.5 text-sm font-mono focus:outline-none focus:ring-1 focus:ring-primary/60";
                    }
                }
                p class="text-[11px] text-muted-foreground mt-1" {
                    "Not persisted. "
                    a href="/ui/config#pii" class="text-primary underline-offset-2 hover:underline" { "Edit permanent rules in Configuration → [pii]" }
                }
            }

            // Input text area
            div {
                h4 class="text-xs font-semibold uppercase tracking-wide text-muted-foreground mb-2" { "Input text" }
                textarea
                    name="text"
                    rows="6"
                    class="w-full rounded-md border border-border bg-background px-3 py-2 text-sm font-mono leading-relaxed focus:outline-none focus:ring-1 focus:ring-primary/60 resize-y"
                {
                    (form.text)
                }
            }

            // Submit
            button
                type="submit"
                class="w-full rounded-lg bg-primary text-primary-foreground px-4 py-2.5 text-sm font-semibold hover:bg-primary/90 transition-colors"
            {
                (PreEscaped(layout::ICON_SHIELD))
                " Pseudonymize"
            }
        }
    }
}

fn render_results(form: &ScanForm, result: Result<ScanResult, String>) -> Markup {
    match result {
        Err(e) => html! {
            div class="text-sm text-destructive font-mono p-4 rounded-lg bg-destructive/10 border border-destructive/30" {
                "Engine error: " (e)
            }
        },
        Ok(res) if res.pseudonymized.is_empty() && res.mapping.is_empty() => html! {
            div class="flex flex-col items-center justify-center py-16 text-center text-muted-foreground" {
                div class="icon-orb mx-auto size-12 rounded-xl grid place-items-center mb-4" {
                    span class="size-6" { (PreEscaped(layout::ICON_SHIELD)) }
                }
                p class="text-sm" { "Submit text to see results." }
            }
        },
        Ok(res) => {
            let fully_restored = res.restored == form.text;
            html! {
                div class="flex flex-col gap-5" {
                    // What the provider sees
                    div {
                        (section_title(layout::ICON_SHIELD, "What the provider sees"))
                        div class="rounded-lg border border-border bg-background p-3 text-sm font-mono leading-relaxed break-all whitespace-pre-wrap" {
                            (render_highlighted(&res.pseudonymized))
                        }
                    }

                    // What your app sees (restored)
                    div {
                        (section_title(layout::ICON_BOLT, "What your app sees"))
                        div class="rounded-lg border border-border bg-background p-3 text-sm font-mono leading-relaxed break-all whitespace-pre-wrap" {
                            (res.restored.clone())
                        }
                        div class="mt-2 flex items-center gap-1.5 text-xs" {
                            @if fully_restored {
                                span class="text-ok font-semibold" { "✓ Fully restored" }
                                span class="text-muted-foreground" { "— round-trip identical to input." }
                            } @else {
                                span class="text-destructive font-semibold" { "✗ Restore mismatch" }
                            }
                        }
                    }

                    // Mapping table
                    @if !res.mapping.is_empty() {
                        div {
                            (section_title(layout::ICON_SLIDERS, "Entity mapping"))
                            div class="rounded-lg border border-border overflow-hidden text-sm" {
                                table class="w-full" {
                                    thead {
                                        tr class="border-b border-border bg-accent/30" {
                                            th class="text-left px-3 py-2 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground" { "Placeholder" }
                                            th class="text-left px-3 py-2 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground" { "Kind" }
                                            th class="text-left px-3 py-2 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground" { "Original value" }
                                        }
                                    }
                                    tbody {
                                        @for (ph, kind, orig) in &res.mapping {
                                            tr class="border-b border-border/60 last:border-0" {
                                                td class="px-3 py-2 font-mono text-primary text-[12.5px]" { (ph) }
                                                td class="px-3 py-2 text-muted-foreground text-[12.5px]" { (kind) }
                                                td class="px-3 py-2 font-mono text-[12.5px] break-all" { (orig) }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Summary counts
                    @if !res.summary.is_empty() {
                        div {
                            (section_title(layout::ICON_CHART, "Detection summary"))
                            div class="flex flex-wrap gap-2" {
                                @for (kind, count) in &res.summary {
                                    span class="inline-flex items-center gap-1 rounded-full px-3 py-1 text-xs font-medium badge-ok" {
                                        (kind) ": " (count)
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

/// Wrap placeholder tokens in accent chips; pass non-placeholder text through.
fn render_highlighted(text: &str) -> Markup {
    // Simple pattern: word chars that look like UPPER_WORD_123.
    // We walk the string looking for runs of UPPERCASE_DIGIT_ that match a
    // placeholder shape (PREFIX_N).
    let mut result = String::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut run_start = 0;
    let mut parts: Vec<(bool, String)> = Vec::new(); // (is_placeholder, text)

    while i < len {
        // Check if we're at the start of a placeholder: uppercase letter.
        if bytes[i].is_ascii_uppercase() {
            // Scan to find end of an ALL_CAPS_DIGITS_ token.
            let tok_start = i;
            while i < len && (bytes[i].is_ascii_uppercase() || bytes[i] == b'_' || bytes[i].is_ascii_digit()) {
                i += 1;
            }
            let tok = &text[tok_start..i];
            // Check it looks like PREFIX_N (has underscore + trailing digits).
            if is_placeholder_shaped(tok) {
                // Flush previous verbatim run.
                if run_start < tok_start {
                    parts.push((false, text[run_start..tok_start].to_owned()));
                }
                parts.push((true, tok.to_owned()));
                run_start = i;
            }
            // else: continue, the chars were already consumed.
        } else {
            i += 1;
        }
    }
    if run_start < len {
        parts.push((false, text[run_start..].to_owned()));
    }

    for (is_ph, part) in parts {
        if is_ph {
            result.push_str(&format!(
                r#"<span class="inline-block rounded px-1 py-0.5 text-[11px] font-semibold badge-brand mx-0.5">{}</span>"#,
                maud::escape_html(&part)
            ));
        } else {
            result.push_str(&maud::escape_html(&part));
        }
    }

    PreEscaped(result)
}

fn is_placeholder_shaped(s: &str) -> bool {
    // Must contain exactly one underscore separating an uppercase word from
    // a digit sequence: e.g. EMAIL_1, CARD_2, TICK_1.
    if let Some(last_under) = s.rfind('_') {
        let prefix = &s[..last_under];
        let suffix = &s[last_under + 1..];
        !prefix.is_empty()
            && prefix.chars().all(|c| c.is_ascii_uppercase() || c == '_')
            && !suffix.is_empty()
            && suffix.chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
}

fn toggle(name: &str, label: &str, prefix: &str, checked: bool) -> Markup {
    html! {
        div class="flex items-center justify-between py-2 border-b border-border/40 last:border-0" {
            label class="flex items-center gap-2 cursor-pointer select-none" for=(name) {
                input
                    type="checkbox"
                    id=(name)
                    name=(name)
                    value="true"
                    class="rounded border-border"
                    checked[checked];
                span class="text-sm font-medium" { (label) }
            }
            span class="text-[11px] font-mono text-muted-foreground" { (prefix) "_N" }
        }
    }
}

// ---------------------------------------------------------------------------
// Icon refs used in this page (sourced from layout constants)
// ---------------------------------------------------------------------------

use crate::layout::{ICON_BOLT, ICON_CHART, ICON_SLIDERS};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── unit: placeholder detection helper ──────────────────────────────────

    #[test]
    fn is_placeholder_shaped_recognizes_standard_forms() {
        assert!(is_placeholder_shaped("EMAIL_1"));
        assert!(is_placeholder_shaped("PHONE_2"));
        assert!(is_placeholder_shaped("IBAN_1"));
        assert!(is_placeholder_shaped("CARD_1"));
        assert!(is_placeholder_shaped("TICKET_1"));
    }

    #[test]
    fn is_placeholder_shaped_rejects_non_placeholders() {
        assert!(!is_placeholder_shaped("EMAIL"));
        assert!(!is_placeholder_shaped("EMAIL_"));
        assert!(!is_placeholder_shaped("_1"));
        assert!(!is_placeholder_shaped("email_1"));
        assert!(!is_placeholder_shaped(""));
    }

    // ── unit: kind label derivation ─────────────────────────────────────────

    #[test]
    fn kind_label_from_placeholder_maps_builtins() {
        assert_eq!(kind_label_from_placeholder("EMAIL_1"), "Email");
        assert_eq!(kind_label_from_placeholder("PHONE_1"), "Phone");
        assert_eq!(kind_label_from_placeholder("IBAN_1"), "IBAN");
        assert_eq!(kind_label_from_placeholder("CARD_1"), "Credit card");
    }

    #[test]
    fn kind_label_from_placeholder_custom() {
        assert_eq!(kind_label_from_placeholder("TICKET_1"), "Ticket");
    }

    // ── unit: engine integration ─────────────────────────────────────────────

    fn default_pii_config() -> PiiConfig {
        PiiConfig {
            enabled_by_default: true,
            disabled_recognizers: vec![],
            custom_recognizers: vec![],
            ner: None,
            vault: None,
            embeddings_require_vault: false,
        }
    }

    #[test]
    fn engine_detects_email_and_pseudonymizes() {
        let cfg = default_pii_config();
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let text = "Contact alice@example.com for details.";
        let detections = engine.scan(text);
        let mut map = EntityMap::new();
        let pseudonymized = map.pseudonymize(text, &detections);
        let restored = map.restore(&pseudonymized);
        assert!(pseudonymized.contains("EMAIL_1"), "expected EMAIL_1 in: {pseudonymized}");
        assert!(!pseudonymized.contains("alice@example.com"));
        assert_eq!(restored, text, "round-trip must match original");
    }

    #[test]
    fn engine_disabled_email_leaves_email_literal() {
        let cfg = PiiConfig {
            disabled_recognizers: vec!["email".to_owned()],
            ..default_pii_config()
        };
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let text = "Contact alice@example.com.";
        let detections = engine.scan(text);
        let mut map = EntityMap::new();
        let pseudonymized = map.pseudonymize(text, &detections);
        assert!(pseudonymized.contains("alice@example.com"), "email must remain when toggle off: {pseudonymized}");
        assert!(!pseudonymized.contains("EMAIL_1"));
    }

    #[test]
    fn engine_iban_detected_on_example_text() {
        let cfg = default_pii_config();
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let text = "IBAN DE89370400440532013000 is the account.";
        let detections = engine.scan(text);
        let mut map = EntityMap::new();
        let pseudonymized = map.pseudonymize(text, &detections);
        assert!(pseudonymized.contains("IBAN_1"), "expected IBAN_1: {pseudonymized}");
        assert_eq!(map.restore(&pseudonymized), text);
    }

    #[test]
    fn engine_credit_card_detected() {
        let cfg = default_pii_config();
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let text = "Card 4111 1111 1111 1111 is accepted.";
        let detections = engine.scan(text);
        let mut map = EntityMap::new();
        let pseudonymized = map.pseudonymize(text, &detections);
        assert!(pseudonymized.contains("CARD_1"), "expected CARD_1: {pseudonymized}");
        assert_eq!(map.restore(&pseudonymized), text);
    }

    #[test]
    fn engine_custom_rule_temp_ticket() {
        let cfg = PiiConfig {
            custom_recognizers: vec![CustomRecognizer {
                name: "TICKET".to_owned(),
                pattern: r"TICK-\d+".to_owned(),
            }],
            ..default_pii_config()
        };
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let text = "See TICK-1234 for details.";
        let detections = engine.scan(text);
        let mut map = EntityMap::new();
        let pseudonymized = map.pseudonymize(text, &detections);
        assert!(pseudonymized.contains("TICKET_1"), "expected TICKET_1: {pseudonymized}");
        assert_eq!(map.restore(&pseudonymized), text);
    }

    #[test]
    fn entity_map_iter_returns_all_pairs() {
        let cfg = default_pii_config();
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let text = "Email alice@example.com and bob@example.com here.";
        let detections = engine.scan(text);
        let mut map = EntityMap::new();
        map.pseudonymize(text, &detections);
        let mut pairs: Vec<(String, String)> = map
            .iter()
            .map(|(ph, orig)| (ph.to_owned(), orig.to_owned()))
            .collect();
        pairs.sort();
        assert_eq!(pairs.len(), 2);
        assert!(pairs.iter().any(|(ph, orig)| ph == "EMAIL_1" && (orig == "alice@example.com" || orig == "bob@example.com")));
    }
}
