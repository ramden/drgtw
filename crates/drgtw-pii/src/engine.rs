//! Engine facade: recognizer registry + merged scan. WP 3.1 / WP 4.4.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::ner_bridge::NerRecognizer;
use crate::recognizers::{
    CreditCardRecognizer, CustomRegexRecognizer, EmailRecognizer, IbanRecognizer, PhoneRecognizer,
};
use crate::{DetectError, Detection, Recognizer};
use drgtw_config::PiiConfig;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("custom recognizer `{name}`: invalid regex: {source}")]
    InvalidRegex {
        name: String,
        #[source]
        source: regex::Error,
    },
}

/// Error returned by [`build_engine_with_ner`] and by proxy-state construction.
#[derive(Debug, thiserror::Error)]
pub enum EngineBuildError {
    #[error("engine config error: {0}")]
    Engine(#[from] EngineError),
    #[error("NER model load error: {0}")]
    Ner(#[from] drgtw_ner::NerError),
    /// Persistent entity-vault could not be opened at boot (WP 9.3).
    ///
    /// Carries a human-readable description. The message never includes key
    /// material — the vault crate's error messages are key-safe, and the
    /// hex-decode failure path is reported generically.
    #[error("vault open error: {0}")]
    Vault(String),
}

/// Recognizer registry. Built once at startup, shared across requests.
pub struct PiiEngine {
    recognizers: Vec<Box<dyn Recognizer>>,
}

impl fmt::Debug for PiiEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.recognizers.iter().map(|r| r.name()).collect();
        f.debug_struct("PiiEngine")
            .field("recognizers", &names)
            .finish()
    }
}

impl PiiEngine {
    /// Build from config: all built-ins (email, iban, credit_card, phone)
    /// minus `disabled_recognizers`, plus compiled `custom_recognizers`.
    ///
    /// Built-in order: email → iban → credit_card → phone.
    /// Iban and credit_card come before phone so that digit-heavy patterns
    /// win on equal-length overlaps when longest-span doesn't decide.
    pub fn from_config(config: &PiiConfig) -> Result<Self, EngineError> {
        let disabled: std::collections::HashSet<&str> = config
            .disabled_recognizers
            .iter()
            .map(String::as_str)
            .collect();

        let mut recognizers: Vec<Box<dyn Recognizer>> = Vec::new();

        macro_rules! push_if_enabled {
            ($name:expr, $ctor:expr) => {
                if !disabled.contains($name) {
                    recognizers.push(Box::new($ctor));
                }
            };
        }

        push_if_enabled!("email", EmailRecognizer::new());
        push_if_enabled!("iban", IbanRecognizer::new());
        push_if_enabled!("credit_card", CreditCardRecognizer::new());
        push_if_enabled!("phone", PhoneRecognizer::new());

        for cr in &config.custom_recognizers {
            let rec = CustomRegexRecognizer::new(&cr.name, &cr.pattern).map_err(|source| {
                EngineError::InvalidRegex {
                    name: cr.name.clone(),
                    source,
                }
            })?;
            recognizers.push(Box::new(rec));
        }

        Ok(Self { recognizers })
    }

    /// Run all recognizers, merge results, resolve overlaps:
    /// longest span wins; ties broken by recognizer registration order.
    /// Returned detections are sorted by `start` and non-overlapping.
    ///
    /// This method uses the infallible `detect` path. Recognizers that may
    /// fail (e.g. NER with fail-closed mode) must be called via [`try_scan`].
    pub fn scan(&self, text: &str) -> Vec<Detection> {
        // Collect all candidates tagged with their recognizer index so we can
        // break ties by registration order.
        let tagged: Vec<(usize, Detection)> = self
            .recognizers
            .iter()
            .enumerate()
            .flat_map(|(idx, r)| r.detect(text).into_iter().map(move |d| (idx, d)))
            .collect();

        Self::merge_tagged(tagged)
    }

    /// Fallible scan: runs all recognizers via [`Recognizer::try_detect`] and
    /// propagates the **first** error encountered. Overlap resolution is the
    /// same as [`scan`].
    ///
    /// Use this for engines that include fail-closed NER recognizers; the
    /// result is the same as `scan` when all recognizers succeed.
    pub fn try_scan(&self, text: &str) -> Result<Vec<Detection>, DetectError> {
        let mut tagged: Vec<(usize, Detection)> = Vec::new();
        for (idx, r) in self.recognizers.iter().enumerate() {
            let dets = r.try_detect(text)?;
            tagged.extend(dets.into_iter().map(|d| (idx, d)));
        }
        Ok(Self::merge_tagged(tagged))
    }

    /// Merge and de-overlap tagged detections (shared by `scan` and `try_scan`).
    fn merge_tagged(mut tagged: Vec<(usize, Detection)>) -> Vec<Detection> {
        // Sort: start asc, span length desc, recognizer order asc.
        tagged.sort_by(|(ai, a), (bi, b)| {
            a.start
                .cmp(&b.start)
                .then_with(|| (b.end - b.start).cmp(&(a.end - a.start)))
                .then_with(|| ai.cmp(bi))
        });

        // Greedy non-overlapping selection.
        let mut result: Vec<Detection> = Vec::new();
        let mut last_end: usize = 0;

        for (_, det) in tagged {
            if det.start >= last_end {
                last_end = det.end;
                result.push(det);
            }
        }

        result
    }

    /// Add a recognizer to this engine after construction. Used by
    /// `build_engine_with_ner` to attach the NER bridge recognizer without
    /// going through `from_config` again.
    pub fn push_recognizer(&mut self, recognizer: Box<dyn Recognizer>) {
        self.recognizers.push(recognizer);
    }
}

/// Build a [`PiiEngine`] from config and, when `pii_cfg.ner` is `Some`, load
/// the NER model, build the pool, and attach a [`NerRecognizer`].
///
/// `base_dir` is used to resolve relative `model_dir` paths. Absolute
/// `model_dir` values are used as-is.
///
/// This is the entry point for the proxy — it does not need to know about
/// `drgtw_ner` types directly.
pub fn build_engine_with_ner(
    pii_cfg: &PiiConfig,
    base_dir: &Path,
) -> Result<PiiEngine, EngineBuildError> {
    let mut engine = PiiEngine::from_config(pii_cfg)?;

    if let Some(ner_cfg) = &pii_cfg.ner {
        let model_path = {
            let p = Path::new(&ner_cfg.model_dir);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                base_dir.join(p)
            }
        };

        let model = drgtw_ner::NerModel::load(&model_path)?;

        let pool_cfg = drgtw_ner::NerPoolConfig {
            workers: ner_cfg.workers,
            queue_capacity: ner_cfg.queue_capacity,
            timeout: Duration::from_millis(ner_cfg.timeout_ms),
        };
        let pool = Arc::new(drgtw_ner::NerPool::new(model, pool_cfg));

        let recognizer = NerRecognizer::new(pool, ner_cfg.score_threshold, ner_cfg.fail_mode);
        engine.push_recognizer(Box::new(recognizer));
    }

    Ok(engine)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use drgtw_config::{CustomRecognizer, PiiConfig};

    use super::*;
    use crate::EntityKind;

    fn default_config() -> PiiConfig {
        PiiConfig::default()
    }

    fn config_with_disabled(names: &[&str]) -> PiiConfig {
        PiiConfig {
            enabled_by_default: true,
            disabled_recognizers: names.iter().map(|s| s.to_string()).collect(),
            custom_recognizers: vec![],
            ner: None,
            vault: None,
            embeddings_require_vault: false,
        }
    }

    fn config_with_custom(name: &str, pattern: &str) -> PiiConfig {
        PiiConfig {
            enabled_by_default: true,
            disabled_recognizers: vec![],
            custom_recognizers: vec![CustomRecognizer {
                name: name.to_string(),
                pattern: pattern.to_string(),
            }],
            ner: None,
            vault: None,
            embeddings_require_vault: false,
        }
    }

    // ── from_config ──────────────────────────────────────────────────────────

    #[test]
    fn engine_builds_with_default_config() {
        let engine = PiiEngine::from_config(&default_config()).unwrap();
        // Should have all 4 built-ins.
        let names: Vec<&str> = engine.recognizers.iter().map(|r| r.name()).collect();
        assert!(names.contains(&"email"));
        assert!(names.contains(&"phone"));
        assert!(names.contains(&"iban"));
        assert!(names.contains(&"credit_card"));
    }

    #[test]
    fn engine_disabled_recognizer_drops_builtin() {
        let cfg = config_with_disabled(&["phone"]);
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let names: Vec<&str> = engine.recognizers.iter().map(|r| r.name()).collect();
        assert!(!names.contains(&"phone"), "phone should be disabled");
        assert!(names.contains(&"email"));
        assert!(names.contains(&"iban"));
        assert!(names.contains(&"credit_card"));
    }

    #[test]
    fn engine_custom_recognizer_added() {
        let cfg = config_with_custom("ticket", r"TKT-\d+");
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let names: Vec<&str> = engine.recognizers.iter().map(|r| r.name()).collect();
        assert!(names.contains(&"ticket"));
    }

    #[test]
    fn engine_invalid_custom_regex_returns_error() {
        let cfg = config_with_custom("bad", r"[unclosed");
        let result = PiiEngine::from_config(&cfg);
        assert!(
            result.is_err(),
            "invalid regex should produce EngineError::InvalidRegex"
        );
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bad"),
            "error message should mention name 'bad'"
        );
    }

    #[test]
    fn engine_debug_shows_recognizer_names() {
        let engine = PiiEngine::from_config(&default_config()).unwrap();
        let debug = format!("{engine:?}");
        assert!(debug.contains("email"));
        assert!(debug.contains("phone"));
    }

    // ── scan ─────────────────────────────────────────────────────────────────

    #[test]
    fn scan_detects_email() {
        let engine = PiiEngine::from_config(&default_config()).unwrap();
        let text = "Send to alice@example.com please";
        let dets = engine.scan(text);
        assert!(dets.iter().any(|d| d.kind == EntityKind::Email));
    }

    #[test]
    fn scan_detects_iban() {
        let engine = PiiEngine::from_config(&default_config()).unwrap();
        let text = "Transfer to DE89370400440532013000 today";
        let dets = engine.scan(text);
        assert!(dets.iter().any(|d| d.kind == EntityKind::Iban));
    }

    #[test]
    fn scan_detects_credit_card() {
        let engine = PiiEngine::from_config(&default_config()).unwrap();
        let text = "Card: 4111 1111 1111 1111";
        let dets = engine.scan(text);
        assert!(dets.iter().any(|d| d.kind == EntityKind::CreditCard));
    }

    #[test]
    fn scan_result_sorted_by_start() {
        let engine = PiiEngine::from_config(&default_config()).unwrap();
        let text = "Email alice@example.com card 4111 1111 1111 1111 done";
        let dets = engine.scan(text);
        for w in dets.windows(2) {
            assert!(w[0].start < w[1].start, "result must be sorted by start");
        }
    }

    #[test]
    fn scan_result_non_overlapping() {
        let engine = PiiEngine::from_config(&default_config()).unwrap();
        let text = "alice@example.com and 4111 1111 1111 1111 and DE89370400440532013000";
        let dets = engine.scan(text);
        for w in dets.windows(2) {
            assert!(
                w[1].start >= w[0].end,
                "detections must be non-overlapping: {:?} overlaps {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn scan_custom_recognizer_kind_name() {
        let cfg = config_with_custom("ticket", r"TKT-\d+");
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let text = "Handle TKT-9999 now";
        let dets = engine.scan(text);
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].kind, EntityKind::Custom(Arc::from("ticket")));
    }

    #[test]
    fn scan_overlap_resolution_longest_wins() {
        // Build an engine with only a custom recognizer that matches a longer
        // span overlapping with a shorter one, to test longest-wins logic.
        let cfg = PiiConfig {
            enabled_by_default: true,
            disabled_recognizers: vec![
                "email".to_string(),
                "phone".to_string(),
                "iban".to_string(),
                "credit_card".to_string(),
            ],
            custom_recognizers: vec![
                CustomRecognizer {
                    name: "short".to_string(),
                    pattern: r"hello".to_string(),
                },
                CustomRecognizer {
                    name: "long".to_string(),
                    pattern: r"hello world".to_string(),
                },
            ],
            ner: None,
            vault: None,
            embeddings_require_vault: false,
        };
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let text = "say hello world!";
        let dets = engine.scan(text);
        assert_eq!(dets.len(), 1, "only one detection after overlap resolution");
        assert_eq!(dets[0].kind, EntityKind::Custom(Arc::from("long")));
    }

    #[test]
    fn scan_overlap_earlier_recognizer_wins_on_equal_span() {
        // Two recognizers that produce the exact same span → earlier one wins.
        let cfg = PiiConfig {
            enabled_by_default: true,
            disabled_recognizers: vec![
                "email".to_string(),
                "phone".to_string(),
                "iban".to_string(),
                "credit_card".to_string(),
            ],
            custom_recognizers: vec![
                CustomRecognizer {
                    name: "first".to_string(),
                    pattern: r"TOKEN".to_string(),
                },
                CustomRecognizer {
                    name: "second".to_string(),
                    pattern: r"TOKEN".to_string(),
                },
            ],
            ner: None,
            vault: None,
            embeddings_require_vault: false,
        };
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let text = "check TOKEN here";
        let dets = engine.scan(text);
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].kind, EntityKind::Custom(Arc::from("first")));
    }

    // ── Realistic sentences ───────────────────────────────────────────────────

    #[test]
    fn scan_german_realistic_sentence() {
        let engine = PiiEngine::from_config(&default_config()).unwrap();
        let text = "Guten Tag, bitte überweisen Sie 150€ auf DE89370400440532013000. \
                    Bei Fragen erreichen Sie uns unter support@muster-bank.de \
                    oder telefonisch +49 89 1234567.";
        let dets = engine.scan(text);
        let kinds: Vec<&EntityKind> = dets.iter().map(|d| &d.kind).collect();
        assert!(
            kinds.contains(&&EntityKind::Iban),
            "should detect IBAN: {kinds:?}"
        );
        assert!(
            kinds.contains(&&EntityKind::Email),
            "should detect email: {kinds:?}"
        );
        // Phone may or may not fire depending on context; just verify no panic.
        for w in dets.windows(2) {
            assert!(w[1].start >= w[0].end, "non-overlapping invariant violated");
        }
    }

    #[test]
    fn scan_english_realistic_sentence() {
        let engine = PiiEngine::from_config(&default_config()).unwrap();
        let text = "Please charge card 4111 1111 1111 1111 and send receipt to \
                    billing@acme.com. Account GB29NWBK60161331926819.";
        let dets = engine.scan(text);
        let kinds: Vec<&EntityKind> = dets.iter().map(|d| &d.kind).collect();
        assert!(
            kinds.contains(&&EntityKind::CreditCard),
            "missing CreditCard"
        );
        assert!(kinds.contains(&&EntityKind::Email), "missing Email");
        assert!(kinds.contains(&&EntityKind::Iban), "missing IBAN");
        for w in dets.windows(2) {
            assert!(w[1].start >= w[0].end, "non-overlapping invariant violated");
        }
    }

    #[test]
    fn scan_mixed_multibyte_entities() {
        let engine = PiiEngine::from_config(&default_config()).unwrap();
        // Emoji + umlauts in surrounding text; entities themselves are ASCII-clean.
        // müller is multibyte but the email local part uses ASCII chars.
        let text = "🏦 Konto: DE89370400440532013000 📧 E-Mail: mueller@beispiel.de";
        let dets = engine.scan(text);
        for d in &dets {
            // Every span must be valid UTF-8 char boundaries.
            let _slice = &text[d.start..d.end];
        }
        let kinds: Vec<&EntityKind> = dets.iter().map(|d| &d.kind).collect();
        assert!(kinds.contains(&&EntityKind::Iban));
        assert!(kinds.contains(&&EntityKind::Email));
    }
}
