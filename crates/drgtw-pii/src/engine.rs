//! Engine facade: recognizer registry + merged scan. WP 3.1 / WP 4.4.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use std::collections::HashSet;

use crate::ner_bridge::NerRecognizer;
use crate::recognizers::{
    CreditCardRecognizer, CustomRegexRecognizer, DateTimeRecognizer, EmailRecognizer,
    IbanRecognizer, IpAddressRecognizer, PhoneRecognizer,
};
use crate::{DetectError, Detection, EntityKind, Recognizer};
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
    /// Content-guardrail engine failed to build (e.g. an invalid guardrail
    /// regex pattern). Carried as a string so `drgtw-pii` need not depend on
    /// `drgtw-guardrails`; the proxy maps the concrete error into this variant.
    /// Boot-fatal, like the other variants.
    #[error("guardrail build error: {0}")]
    Guardrail(String),
}

/// Recognizer registry. Built once at startup, shared across requests.
pub struct PiiEngine {
    recognizers: Vec<Box<dyn Recognizer>>,
    /// When `Some`, only detections whose kind is in the set are kept (the
    /// `pii.entities` allow-list). When `None`, all detected kinds are kept.
    allowed_kinds: Option<HashSet<EntityKind>>,
    /// When `Some`, the NER recognizer only runs for messages whose role is in
    /// this set (lowercased; the `pii.ner.scan_roles` allow-list). When `None`,
    /// NER runs for every role. Regex recognizers are unaffected — they always
    /// run regardless of role.
    ner_scan_roles: Option<HashSet<String>>,
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
        push_if_enabled!("ip_address", IpAddressRecognizer::new());
        push_if_enabled!("datetime", DateTimeRecognizer::new());

        for cr in &config.custom_recognizers {
            let rec = CustomRegexRecognizer::new(&cr.name, &cr.pattern).map_err(|source| {
                EngineError::InvalidRegex {
                    name: cr.name.clone(),
                    source,
                }
            })?;
            recognizers.push(Box::new(rec));
        }

        let allowed_kinds = build_allowed_kinds(config);
        let ner_scan_roles = build_ner_scan_roles(config);

        Ok(Self {
            recognizers,
            allowed_kinds,
            ner_scan_roles,
        })
    }

    /// Run all recognizers, merge results, resolve overlaps:
    /// longest span wins; ties broken by recognizer registration order.
    /// Returned detections are sorted by `start` and non-overlapping.
    ///
    /// This method uses the infallible `detect` path. Recognizers that may
    /// fail (e.g. NER with fail-closed mode) must be called via [`try_scan`].
    pub fn scan(&self, text: &str) -> Vec<Detection> {
        self.scan_impl(text, true)
    }

    /// Like [`scan`](Self::scan) but scopes the NER recognizer by message
    /// `role` per the `pii.ner.scan_roles` allow-list. Regex recognizers always
    /// run. When no `scan_roles` is configured this is identical to `scan`.
    ///
    /// `role` is the chat message role (`"system"`, `"user"`, `"assistant"`,
    /// …). `None` (unknown role) is treated as in-scope, so NER still runs —
    /// scoping never silently drops masking on an unrecognised role.
    pub fn scan_for_role(&self, text: &str, role: Option<&str>) -> Vec<Detection> {
        self.scan_impl(text, self.ner_enabled_for_role(role))
    }

    /// Shared scan body. `include_ner` selects whether the `ner` recognizer
    /// participates; all other recognizers always run.
    fn scan_impl(&self, text: &str, include_ner: bool) -> Vec<Detection> {
        // Collect all candidates tagged with their recognizer index so we can
        // break ties by registration order.
        let tagged: Vec<(usize, Detection)> = self
            .recognizers
            .iter()
            .enumerate()
            .filter(|(_, r)| include_ner || r.name() != "ner")
            .flat_map(|(idx, r)| r.detect(text).into_iter().map(move |d| (idx, d)))
            .collect();

        self.filter_allowed(Self::merge_tagged(tagged))
    }

    /// Fallible scan: runs all recognizers via [`Recognizer::try_detect`] and
    /// propagates the **first** error encountered. Overlap resolution is the
    /// same as [`scan`].
    ///
    /// Use this for engines that include fail-closed NER recognizers; the
    /// result is the same as `scan` when all recognizers succeed.
    pub fn try_scan(&self, text: &str) -> Result<Vec<Detection>, DetectError> {
        self.try_scan_impl(text, true)
    }

    /// Fallible counterpart to [`scan_for_role`](Self::scan_for_role): runs
    /// every recognizer via [`Recognizer::try_detect`], scoping the NER
    /// recognizer by `role`. Used by the request body walker so fail-closed
    /// NER errors propagate.
    pub fn try_scan_for_role(
        &self,
        text: &str,
        role: Option<&str>,
    ) -> Result<Vec<Detection>, DetectError> {
        self.try_scan_impl(text, self.ner_enabled_for_role(role))
    }

    fn try_scan_impl(&self, text: &str, include_ner: bool) -> Result<Vec<Detection>, DetectError> {
        let mut tagged: Vec<(usize, Detection)> = Vec::new();
        for (idx, r) in self.recognizers.iter().enumerate() {
            if !include_ner && r.name() == "ner" {
                continue;
            }
            let dets = r.try_detect(text)?;
            tagged.extend(dets.into_iter().map(|d| (idx, d)));
        }
        Ok(self.filter_allowed(Self::merge_tagged(tagged)))
    }

    /// Whether the NER recognizer should run for a message of the given `role`,
    /// per the `pii.ner.scan_roles` allow-list.
    fn ner_enabled_for_role(&self, role: Option<&str>) -> bool {
        match &self.ner_scan_roles {
            // No scoping configured: NER runs everywhere.
            None => true,
            Some(roles) => match role {
                Some(r) => roles.contains(r.to_ascii_lowercase().as_str()),
                // Unknown / missing role: scan to be safe (never silently skip).
                None => true,
            },
        }
    }

    /// Apply the `pii.entities` allow-list (no-op when `allowed_kinds` is None).
    fn filter_allowed(&self, dets: Vec<Detection>) -> Vec<Detection> {
        match &self.allowed_kinds {
            None => dets,
            Some(allowed) => dets
                .into_iter()
                .filter(|d| allowed.contains(&d.kind))
                .collect(),
        }
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

/// Build the `allowed_kinds` set from `config.entities`.
///
/// Returns `None` when no allow-list is configured (keep everything). Each name
/// is a presidio-style built-in (mapped via the config canonicaliser →
/// [`EntityKind`]) or a `custom_recognizers` name (→ [`EntityKind::Custom`] with
/// the recognizer's exact casing). Unknown names are skipped — validation in
/// `drgtw-config` already rejects them before this runs.
fn build_allowed_kinds(config: &PiiConfig) -> Option<HashSet<EntityKind>> {
    let names = config.entities.as_ref()?;
    let mut set = HashSet::new();
    for name in names {
        if let Some(canon) = drgtw_config::canonical_pii_entity_name(name) {
            if let Some(kind) = EntityKind::from_canonical_name(canon) {
                set.insert(kind);
            }
        } else if let Some(cr) = config
            .custom_recognizers
            .iter()
            .find(|cr| cr.name.eq_ignore_ascii_case(name.trim()))
        {
            set.insert(EntityKind::Custom(std::sync::Arc::from(cr.name.as_str())));
        }
    }
    Some(set)
}

/// Build the lowercased `ner_scan_roles` set from `config.ner.scan_roles`.
///
/// Returns `None` when NER is absent or `scan_roles` is unset (scan every
/// role). Empty lists are rejected by config validation, so a present list is
/// always non-empty here.
fn build_ner_scan_roles(config: &PiiConfig) -> Option<HashSet<String>> {
    let roles = config.ner.as_ref()?.scan_roles.as_ref()?;
    Some(
        roles
            .iter()
            .map(|r| r.trim().to_ascii_lowercase())
            .collect(),
    )
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

        let recognizer = NerRecognizer::with_cache(
            pool,
            ner_cfg.score_threshold,
            ner_cfg.fail_mode,
            ner_cfg.cache_capacity,
        );
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

    use drgtw_config::{CustomRecognizer, FailMode, NerConfig, PiiConfig};

    use super::*;
    use crate::{Detection, EntityKind, Recognizer};

    /// Stand-in for the real NER recognizer in scoping tests: named `"ner"` so
    /// the engine's role-scoping treats it as the NER recognizer, and emits one
    /// `Person` detection covering the whole input. Avoids loading a model.
    struct FakeNer;
    impl Recognizer for FakeNer {
        fn name(&self) -> &str {
            "ner"
        }
        fn detect(&self, text: &str) -> Vec<Detection> {
            if text.is_empty() {
                return vec![];
            }
            vec![Detection {
                start: 0,
                end: text.len(),
                kind: EntityKind::Person,
            }]
        }
    }

    fn ner_cfg_with_roles(scan_roles: Option<Vec<&str>>) -> NerConfig {
        NerConfig {
            model_dir: "models/ner".to_string(),
            score_threshold: 0.5,
            fail_mode: FailMode::Open,
            timeout_ms: 5000,
            workers: 2,
            queue_capacity: 64,
            scan_roles: scan_roles.map(|v| v.into_iter().map(str::to_string).collect()),
            cache_capacity: 0,
        }
    }

    fn engine_with_fake_ner(scan_roles: Option<Vec<&str>>) -> PiiEngine {
        let cfg = PiiConfig {
            ner: Some(ner_cfg_with_roles(scan_roles)),
            ..PiiConfig::default()
        };
        let mut engine = PiiEngine::from_config(&cfg).unwrap();
        engine.push_recognizer(Box::new(FakeNer));
        engine
    }

    fn has_person(dets: &[Detection]) -> bool {
        dets.iter().any(|d| d.kind == EntityKind::Person)
    }

    fn default_config() -> PiiConfig {
        PiiConfig::default()
    }

    fn config_with_disabled(names: &[&str]) -> PiiConfig {
        PiiConfig {
            enabled_by_default: true,
            disabled_recognizers: names.iter().map(|s| s.to_string()).collect(),
            custom_recognizers: vec![],
            entities: None,
            ner: None,
            vault: None,
            embeddings_require_vault: false,
            require_ner: false,
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
            entities: None,
            ner: None,
            vault: None,
            embeddings_require_vault: false,
            require_ner: false,
        }
    }

    fn config_with_entities(entities: &[&str]) -> PiiConfig {
        PiiConfig {
            enabled_by_default: true,
            disabled_recognizers: vec![],
            custom_recognizers: vec![],
            entities: Some(entities.iter().map(|s| s.to_string()).collect()),
            ner: None,
            vault: None,
            embeddings_require_vault: false,
            require_ner: false,
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
            entities: None,
            ner: None,
            vault: None,
            embeddings_require_vault: false,
            require_ner: false,
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
            entities: None,
            ner: None,
            vault: None,
            embeddings_require_vault: false,
            require_ner: false,
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

    // ── entities allow-list ────────────────────────────────────────────────

    #[test]
    fn entities_none_keeps_all_kinds() {
        let engine = PiiEngine::from_config(&default_config()).unwrap();
        let text = "mail alice@example.com card 4111 1111 1111 1111";
        let kinds: Vec<_> = engine.scan(text).into_iter().map(|d| d.kind).collect();
        assert!(kinds.contains(&EntityKind::Email));
        assert!(kinds.contains(&EntityKind::CreditCard));
    }

    #[test]
    fn entities_allow_list_filters_to_subset() {
        let cfg = config_with_entities(&["EMAIL_ADDRESS"]);
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let text = "mail alice@example.com card 4111 1111 1111 1111";
        let kinds: Vec<_> = engine.scan(text).into_iter().map(|d| d.kind).collect();
        assert!(kinds.contains(&EntityKind::Email), "email kept");
        assert!(
            !kinds.contains(&EntityKind::CreditCard),
            "credit card filtered out"
        );
    }

    #[test]
    fn entities_allow_list_accepts_aliases() {
        // "EMAIL" alias + "CC" alias.
        let cfg = config_with_entities(&["EMAIL", "CC"]);
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let text = "mail alice@example.com card 4111 1111 1111 1111 phone +49 89 1234567";
        let kinds: Vec<_> = engine.scan(text).into_iter().map(|d| d.kind).collect();
        assert!(kinds.contains(&EntityKind::Email));
        assert!(kinds.contains(&EntityKind::CreditCard));
        assert!(!kinds.contains(&EntityKind::Phone), "phone filtered out");
    }

    #[test]
    fn entities_allow_list_includes_new_ip_kind() {
        let cfg = config_with_entities(&["IP_ADDRESS"]);
        let engine = PiiEngine::from_config(&cfg).unwrap();
        let text = "host 10.0.0.1 mail a@b.de";
        let kinds: Vec<_> = engine.scan(text).into_iter().map(|d| d.kind).collect();
        assert!(kinds.contains(&EntityKind::IpAddress));
        assert!(!kinds.contains(&EntityKind::Email));
    }

    #[test]
    fn scan_detects_ip_and_date_by_default() {
        let engine = PiiEngine::from_config(&default_config()).unwrap();
        let kinds: Vec<_> = engine
            .scan("server 192.168.0.1 on 31.12.2024")
            .into_iter()
            .map(|d| d.kind)
            .collect();
        assert!(kinds.contains(&EntityKind::IpAddress));
        assert!(kinds.contains(&EntityKind::DateTime));
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

    // ── NER scan-role scoping (R1) ─────────────────────────────────────────

    #[test]
    fn scan_for_role_no_scoping_runs_ner_on_every_role() {
        let engine = engine_with_fake_ner(None);
        assert!(has_person(&engine.scan_for_role("Angela Merkel", Some("system"))));
        assert!(has_person(&engine.scan_for_role("Angela Merkel", Some("user"))));
        // Plain scan also unaffected.
        assert!(has_person(&engine.scan("Angela Merkel")));
    }

    #[test]
    fn scan_for_role_scopes_ner_to_listed_roles() {
        let engine = engine_with_fake_ner(Some(vec!["user", "assistant"]));
        // In-scope roles still run NER.
        assert!(has_person(&engine.scan_for_role("Angela Merkel", Some("user"))));
        assert!(has_person(&engine.scan_for_role("Angela Merkel", Some("assistant"))));
        // Excluded role: NER skipped → no Person detection.
        assert!(!has_person(&engine.scan_for_role("Angela Merkel", Some("system"))));
    }

    #[test]
    fn scan_for_role_is_case_insensitive() {
        let engine = engine_with_fake_ner(Some(vec!["User"]));
        assert!(has_person(&engine.scan_for_role("Angela Merkel", Some("USER"))));
        assert!(!has_person(&engine.scan_for_role("Angela Merkel", Some("System"))));
    }

    #[test]
    fn scan_for_role_unknown_role_still_scans() {
        // Safety: a missing/None role must never silently drop NER.
        let engine = engine_with_fake_ner(Some(vec!["user"]));
        assert!(has_person(&engine.scan_for_role("Angela Merkel", None)));
    }

    #[test]
    fn scan_for_role_keeps_regex_on_excluded_role() {
        // Regex recognizers always run, even on a role NER is scoped out of.
        let engine = engine_with_fake_ner(Some(vec!["user"]));
        let dets = engine.scan_for_role("mail alice@example.com", Some("system"));
        assert!(
            dets.iter().any(|d| d.kind == EntityKind::Email),
            "email regex must still fire on the system role"
        );
        assert!(
            !has_person(&dets),
            "but NER (person) must be scoped out on the system role"
        );
    }

    #[test]
    fn try_scan_for_role_scopes_ner() {
        let engine = engine_with_fake_ner(Some(vec!["user"]));
        let user = engine.try_scan_for_role("Angela Merkel", Some("user")).unwrap();
        assert!(has_person(&user));
        let sys = engine
            .try_scan_for_role("Angela Merkel", Some("system"))
            .unwrap();
        assert!(!has_person(&sys));
    }
}
