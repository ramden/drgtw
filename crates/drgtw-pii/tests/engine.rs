//! Integration tests for PiiEngine (external test crate).

use std::sync::Arc;

use drgtw_config::{CustomRecognizer, PiiConfig};
use drgtw_pii::{EntityKind, PiiEngine, engine::EngineError};

fn default_config() -> PiiConfig {
    PiiConfig::default()
}

// ─────────────────────────────────────────────────────────────────────────────
// Smoke tests via public API
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn engine_scans_email_in_text() {
    let engine = PiiEngine::from_config(&default_config()).unwrap();
    let text = "Contact alice@example.com for more info.";
    let dets = engine.scan(text);
    assert_eq!(dets.len(), 1);
    assert_eq!(dets[0].kind, EntityKind::Email);
    assert_eq!(&text[dets[0].start..dets[0].end], "alice@example.com");
}

#[test]
fn engine_scans_iban_in_text() {
    let engine = PiiEngine::from_config(&default_config()).unwrap();
    let text = "Please transfer to DE89370400440532013000.";
    let dets = engine.scan(text);
    assert!(dets.iter().any(|d| d.kind == EntityKind::Iban));
}

#[test]
fn engine_scans_credit_card_in_text() {
    let engine = PiiEngine::from_config(&default_config()).unwrap();
    let text = "Charge 4111 1111 1111 1111 now.";
    let dets = engine.scan(text);
    assert!(dets.iter().any(|d| d.kind == EntityKind::CreditCard));
}

#[test]
fn engine_disabled_recognizers_suppressed() {
    let cfg = PiiConfig {
        enabled_by_default: true,
        disabled_recognizers: vec!["email".to_string(), "iban".to_string()],
        custom_recognizers: vec![],
        ner: None,
        vault: None,
        embeddings_require_vault: false,
    };
    let engine = PiiEngine::from_config(&cfg).unwrap();
    let text = "alice@example.com DE89370400440532013000";
    let dets = engine.scan(text);
    // Neither email nor IBAN should appear.
    assert!(
        dets.iter().all(|d| d.kind != EntityKind::Email),
        "email should be disabled"
    );
    assert!(
        dets.iter().all(|d| d.kind != EntityKind::Iban),
        "iban should be disabled"
    );
}

#[test]
fn engine_custom_recognizer_fires() {
    let cfg = PiiConfig {
        enabled_by_default: true,
        disabled_recognizers: vec![],
        custom_recognizers: vec![CustomRecognizer {
            name: "order".to_string(),
            pattern: r"ORD-\d{6}".to_string(),
        }],
        ner: None,
        vault: None,
        embeddings_require_vault: false,
    };
    let engine = PiiEngine::from_config(&cfg).unwrap();
    let text = "Your order ORD-123456 is shipped.";
    let dets = engine.scan(text);
    assert_eq!(dets.len(), 1);
    assert_eq!(dets[0].kind, EntityKind::Custom(Arc::from("order")));
    assert_eq!(&text[dets[0].start..dets[0].end], "ORD-123456");
}

#[test]
fn engine_invalid_custom_regex_is_engine_error() {
    let cfg = PiiConfig {
        enabled_by_default: true,
        disabled_recognizers: vec![],
        custom_recognizers: vec![CustomRecognizer {
            name: "broken".to_string(),
            pattern: r"(?P<".to_string(), // definitely invalid
        }],
        ner: None,
        vault: None,
        embeddings_require_vault: false,
    };
    let result = PiiEngine::from_config(&cfg);
    assert!(matches!(result, Err(EngineError::InvalidRegex { .. })));
}

#[test]
fn engine_scan_invariants_sorted_non_overlapping() {
    let engine = PiiEngine::from_config(&default_config()).unwrap();
    let text = "Email: alice@example.com IBAN: DE89370400440532013000 Card: 4111 1111 1111 1111";
    let dets = engine.scan(text);
    for w in dets.windows(2) {
        assert!(w[0].start < w[1].start, "not sorted by start");
        assert!(w[1].start >= w[0].end, "overlapping detections found");
    }
}

#[test]
fn engine_scan_byte_spans_valid_utf8() {
    let engine = PiiEngine::from_config(&default_config()).unwrap();
    // Multi-byte chars before the entities; entity local-parts are ASCII.
    let text = "🌍 Nono: alice@example.com — Konto: DE89370400440532013000";
    let dets = engine.scan(text);
    for d in &dets {
        // Must not panic (would panic if not on char boundary).
        let _ = &text[d.start..d.end];
    }
    let kinds: Vec<&EntityKind> = dets.iter().map(|d| &d.kind).collect();
    assert!(kinds.contains(&&EntityKind::Email));
    assert!(kinds.contains(&&EntityKind::Iban));
}

#[test]
fn engine_debug_output_contains_recognizer_names() {
    let engine = PiiEngine::from_config(&default_config()).unwrap();
    let s = format!("{engine:?}");
    assert!(s.contains("email"));
    assert!(s.contains("phone"));
    assert!(s.contains("iban"));
    assert!(s.contains("credit_card"));
}

#[test]
fn engine_empty_text_returns_empty() {
    let engine = PiiEngine::from_config(&default_config()).unwrap();
    assert!(engine.scan("").is_empty());
}

#[test]
fn engine_no_pii_text_returns_empty() {
    let engine = PiiEngine::from_config(&default_config()).unwrap();
    let text = "Hello world, the quick brown fox jumps over the lazy dog.";
    let dets = engine.scan(text);
    assert!(dets.is_empty(), "no PII in this sentence, got: {dets:?}");
}
