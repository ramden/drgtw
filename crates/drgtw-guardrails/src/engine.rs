//! The [`GuardrailEngine`]: builds rules from config and evaluates them.

use std::collections::HashSet;
use std::sync::Arc;

use drgtw_config::{GuardrailAction, GuardrailKind, GuardrailPhase, GuardrailsConfig};

use crate::builtins::{BannedContentGuardrail, ContactInfoGuardrail, PromptInjectionGuardrail};
use crate::guardrail::Guardrail;
use crate::outcome::GuardrailOutcome;

/// Error returned while building a [`GuardrailEngine`] from config.
#[derive(Debug, thiserror::Error)]
pub enum GuardrailBuildError {
    /// A rule's regex pattern failed to compile.
    #[error("guardrail `{name}`: invalid regex `{pattern}`: {source}")]
    InvalidPattern {
        name: String,
        pattern: String,
        #[source]
        source: regex::Error,
    },
}

/// A compiled rule: metadata plus the backing guardrail.
struct CompiledRule {
    name: String,
    phase: GuardrailPhase,
    action: GuardrailAction,
    guardrail: Box<dyn Guardrail>,
}

impl CompiledRule {
    /// `true` when this rule runs in the request (pre) phase.
    fn runs_pre(&self) -> bool {
        matches!(self.phase, GuardrailPhase::Pre | GuardrailPhase::Both)
    }

    /// `true` when this rule runs in the response (post) phase.
    fn runs_post(&self) -> bool {
        matches!(self.phase, GuardrailPhase::Post | GuardrailPhase::Both)
    }
}

/// Built once at startup, shared across requests. Holds rules in config order.
pub struct GuardrailEngine {
    rules: Vec<CompiledRule>,
}

impl std::fmt::Debug for GuardrailEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let rules: Vec<&str> = self.rules.iter().map(|r| r.name.as_str()).collect();
        f.debug_struct("GuardrailEngine")
            .field("rules", &rules)
            .finish()
    }
}

impl GuardrailEngine {
    /// Build from config. `pii` is shared (Arc) with the proxy's PII engine and
    /// used by the `contact_info` guardrail to detect entities.
    pub fn from_config(
        cfg: &GuardrailsConfig,
        pii: Arc<drgtw_pii::PiiEngine>,
    ) -> Result<Self, GuardrailBuildError> {
        let mut rules = Vec::with_capacity(cfg.rules.len());

        for rule in &cfg.rules {
            let guardrail: Box<dyn Guardrail> = match rule.kind {
                GuardrailKind::PromptInjection => {
                    Box::new(PromptInjectionGuardrail::new(&rule.patterns).map_err(|source| {
                        invalid_pattern(&rule.name, &rule.patterns, source)
                    })?)
                }
                GuardrailKind::BannedContent => {
                    Box::new(BannedContentGuardrail::new(&rule.patterns).map_err(|source| {
                        invalid_pattern(&rule.name, &rule.patterns, source)
                    })?)
                }
                GuardrailKind::ContactInfo => {
                    let kinds = build_kinds(&rule.entities);
                    Box::new(ContactInfoGuardrail::new(Arc::clone(&pii), kinds))
                }
            };

            rules.push(CompiledRule {
                name: rule.name.clone(),
                phase: rule.phase,
                action: rule.action,
                guardrail,
            });
        }

        Ok(Self { rules })
    }

    /// `true` when no guardrail rules are configured.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Run every rule whose phase is `Pre` or `Both` against request `text`.
    pub fn check_request(&self, text: &str) -> GuardrailOutcome {
        self.evaluate(text, CompiledRule::runs_pre)
    }

    /// Run every rule whose phase is `Post` or `Both` against response `text`.
    pub fn check_response(&self, text: &str) -> GuardrailOutcome {
        self.evaluate(text, CompiledRule::runs_post)
    }

    /// Shared evaluation core. `selector` decides whether a rule runs in this
    /// phase. Rules run in config order; the first `Block` short-circuits.
    fn evaluate(&self, text: &str, selector: fn(&CompiledRule) -> bool) -> GuardrailOutcome {
        let mut redactions: Vec<(usize, usize)> = Vec::new();
        let mut flags: Vec<String> = Vec::new();

        for rule in self.rules.iter().filter(|r| selector(r)) {
            let spans = rule.guardrail.find(text);
            if spans.is_empty() {
                continue;
            }
            match rule.action {
                GuardrailAction::Block => {
                    return GuardrailOutcome::Block(format!(
                        "guardrail `{}` blocked content",
                        rule.name
                    ));
                }
                GuardrailAction::Redact => redactions.extend(spans),
                GuardrailAction::Flag => flags.push(format!(
                    "guardrail `{}` matched ({} span(s))",
                    rule.name,
                    spans.len()
                )),
            }
        }

        if !redactions.is_empty() {
            GuardrailOutcome::Redact(merge_spans(redactions))
        } else if !flags.is_empty() {
            GuardrailOutcome::Flag(flags.join("; "))
        } else {
            GuardrailOutcome::Allow
        }
    }
}

/// Map config `entities` names to a set of [`drgtw_pii::EntityKind`]. Names that
/// don't map to a built-in kind are skipped.
fn build_kinds(entities: &[String]) -> HashSet<drgtw_pii::EntityKind> {
    let mut set = HashSet::new();
    for name in entities {
        if let Some(canon) = drgtw_config::canonical_pii_entity_name(name)
            && let Some(kind) = drgtw_pii::EntityKind::from_canonical_name(canon)
        {
            set.insert(kind);
        }
    }
    set
}

/// Build an [`GuardrailBuildError::InvalidPattern`]. The offending pattern is
/// reported by joining the rule's patterns (the regex error names the cause).
fn invalid_pattern(name: &str, patterns: &[String], source: regex::Error) -> GuardrailBuildError {
    GuardrailBuildError::InvalidPattern {
        name: name.to_string(),
        pattern: patterns.join(", "),
        source,
    }
}

/// Sort, dedupe, and merge overlapping/adjacent-overlapping spans so the result
/// is non-overlapping and ordered by start. Touching-but-not-overlapping spans
/// (`a.end == b.start`) are kept separate.
fn merge_spans(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.sort_unstable();
    spans.dedup();

    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        if let Some(last) = merged.last_mut()
            && start < last.1
        {
            // Overlap: extend the previous span.
            if end > last.1 {
                last.1 = end;
            }
            continue;
        }
        merged.push((start, end));
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use drgtw_config::GuardrailRule;

    fn pii() -> Arc<drgtw_pii::PiiEngine> {
        Arc::new(drgtw_pii::PiiEngine::from_config(&drgtw_config::PiiConfig::default()).unwrap())
    }

    fn rule(
        name: &str,
        kind: GuardrailKind,
        phase: GuardrailPhase,
        action: GuardrailAction,
        patterns: &[&str],
        entities: &[&str],
    ) -> GuardrailRule {
        GuardrailRule {
            name: name.to_string(),
            kind,
            phase,
            action,
            patterns: patterns.iter().map(|s| s.to_string()).collect(),
            entities: entities.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn is_empty_true_for_empty_config() {
        let engine = GuardrailEngine::from_config(&GuardrailsConfig::default(), pii()).unwrap();
        assert!(engine.is_empty());
        assert_eq!(engine.check_request("anything"), GuardrailOutcome::Allow);
    }

    #[test]
    fn from_config_builds_rules() {
        let cfg = GuardrailsConfig {
            rules: vec![
                rule(
                    "inj",
                    GuardrailKind::PromptInjection,
                    GuardrailPhase::Pre,
                    GuardrailAction::Block,
                    &[],
                    &[],
                ),
                rule(
                    "ban",
                    GuardrailKind::BannedContent,
                    GuardrailPhase::Pre,
                    GuardrailAction::Flag,
                    &["(?i)foo"],
                    &[],
                ),
                rule(
                    "contact",
                    GuardrailKind::ContactInfo,
                    GuardrailPhase::Post,
                    GuardrailAction::Redact,
                    &[],
                    &["EMAIL_ADDRESS"],
                ),
            ],
        };
        let engine = GuardrailEngine::from_config(&cfg, pii()).unwrap();
        assert!(!engine.is_empty());
    }

    #[test]
    fn invalid_regex_in_rule_errors() {
        let cfg = GuardrailsConfig {
            rules: vec![rule(
                "bad",
                GuardrailKind::BannedContent,
                GuardrailPhase::Pre,
                GuardrailAction::Block,
                &["[unclosed"],
                &[],
            )],
        };
        let err = GuardrailEngine::from_config(&cfg, pii()).unwrap_err();
        match err {
            GuardrailBuildError::InvalidPattern { name, .. } => assert_eq!(name, "bad"),
        }
    }

    #[test]
    fn check_request_short_circuits_on_first_block() {
        let cfg = GuardrailsConfig {
            rules: vec![
                rule(
                    "blocker",
                    GuardrailKind::BannedContent,
                    GuardrailPhase::Pre,
                    GuardrailAction::Block,
                    &["(?i)secret"],
                    &[],
                ),
                // A redact rule that would also match — must not run.
                rule(
                    "redactor",
                    GuardrailKind::BannedContent,
                    GuardrailPhase::Pre,
                    GuardrailAction::Redact,
                    &["(?i)secret"],
                    &[],
                ),
            ],
        };
        let engine = GuardrailEngine::from_config(&cfg, pii()).unwrap();
        match engine.check_request("this is secret") {
            GuardrailOutcome::Block(reason) => assert!(reason.contains("blocker")),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn redact_spans_from_multiple_rules_merged_and_sorted() {
        // Two redact rules over the same text; spans must come back sorted,
        // deduped and merged.
        let cfg = GuardrailsConfig {
            rules: vec![
                rule(
                    "r1",
                    GuardrailKind::BannedContent,
                    GuardrailPhase::Pre,
                    GuardrailAction::Redact,
                    &["bar"],
                    &[],
                ),
                rule(
                    "r2",
                    GuardrailKind::BannedContent,
                    GuardrailPhase::Pre,
                    GuardrailAction::Redact,
                    &["foo"],
                    &[],
                ),
            ],
        };
        let engine = GuardrailEngine::from_config(&cfg, pii()).unwrap();
        let text = "foo and bar"; // foo=(0,3), bar=(8,11)
        match engine.check_request(text) {
            GuardrailOutcome::Redact(spans) => {
                assert_eq!(spans, vec![(0, 3), (8, 11)]);
            }
            other => panic!("expected Redact, got {other:?}"),
        }
    }

    #[test]
    fn overlapping_redact_spans_are_merged() {
        // "foobar" matched by "foob" (0,4) and "obar" (2,6) → merged (0,6).
        let cfg = GuardrailsConfig {
            rules: vec![
                rule(
                    "a",
                    GuardrailKind::BannedContent,
                    GuardrailPhase::Pre,
                    GuardrailAction::Redact,
                    &["foob"],
                    &[],
                ),
                rule(
                    "b",
                    GuardrailKind::BannedContent,
                    GuardrailPhase::Pre,
                    GuardrailAction::Redact,
                    &["obar"],
                    &[],
                ),
            ],
        };
        let engine = GuardrailEngine::from_config(&cfg, pii()).unwrap();
        match engine.check_request("foobar") {
            GuardrailOutcome::Redact(spans) => assert_eq!(spans, vec![(0, 6)]),
            other => panic!("expected Redact, got {other:?}"),
        }
    }

    #[test]
    fn flag_messages_accumulate() {
        let cfg = GuardrailsConfig {
            rules: vec![
                rule(
                    "f1",
                    GuardrailKind::BannedContent,
                    GuardrailPhase::Pre,
                    GuardrailAction::Flag,
                    &["foo"],
                    &[],
                ),
                rule(
                    "f2",
                    GuardrailKind::BannedContent,
                    GuardrailPhase::Pre,
                    GuardrailAction::Flag,
                    &["bar"],
                    &[],
                ),
            ],
        };
        let engine = GuardrailEngine::from_config(&cfg, pii()).unwrap();
        match engine.check_request("foo bar") {
            GuardrailOutcome::Flag(reason) => {
                assert!(reason.contains("f1"), "reason: {reason}");
                assert!(reason.contains("f2"), "reason: {reason}");
                assert!(reason.contains(';'), "messages should be joined: {reason}");
            }
            other => panic!("expected Flag, got {other:?}"),
        }
    }

    #[test]
    fn post_phase_rule_does_not_fire_on_check_request() {
        let cfg = GuardrailsConfig {
            rules: vec![rule(
                "post-only",
                GuardrailKind::BannedContent,
                GuardrailPhase::Post,
                GuardrailAction::Block,
                &["secret"],
                &[],
            )],
        };
        let engine = GuardrailEngine::from_config(&cfg, pii()).unwrap();
        assert_eq!(engine.check_request("secret"), GuardrailOutcome::Allow);
        // But it fires on the response phase.
        assert!(matches!(
            engine.check_response("secret"),
            GuardrailOutcome::Block(_)
        ));
    }

    #[test]
    fn both_phase_fires_on_request_and_response() {
        let cfg = GuardrailsConfig {
            rules: vec![rule(
                "both",
                GuardrailKind::BannedContent,
                GuardrailPhase::Both,
                GuardrailAction::Block,
                &["secret"],
                &[],
            )],
        };
        let engine = GuardrailEngine::from_config(&cfg, pii()).unwrap();
        assert!(matches!(
            engine.check_request("secret"),
            GuardrailOutcome::Block(_)
        ));
        assert!(matches!(
            engine.check_response("secret"),
            GuardrailOutcome::Block(_)
        ));
    }
}
