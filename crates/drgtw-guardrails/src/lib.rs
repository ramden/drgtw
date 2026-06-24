//! Content guardrails for the drgtw gateway.
//!
//! A [`GuardrailEngine`] is built once from [`drgtw_config::GuardrailsConfig`]
//! and evaluated on request text ([`GuardrailEngine::check_request`]) and
//! response text ([`GuardrailEngine::check_response`]). Each rule is backed by a
//! built-in [`Guardrail`] selected by its [`drgtw_config::GuardrailKind`]:
//!
//! - [`PromptInjectionGuardrail`] — heuristic jailbreak detection.
//! - [`BannedContentGuardrail`] — operator-supplied regex blocklist.
//! - [`ContactInfoGuardrail`] — reuses the shared PII engine for contact info.
//!
//! Evaluation runs rules in config order. The first `Block` short-circuits;
//! otherwise `Redact` spans are merged and `Flag` reasons accumulate. The
//! result is a [`GuardrailOutcome`].

mod builtins;
mod engine;
mod guardrail;
mod outcome;

pub use builtins::{BannedContentGuardrail, ContactInfoGuardrail, PromptInjectionGuardrail};
pub use engine::{GuardrailBuildError, GuardrailEngine};
pub use guardrail::Guardrail;
pub use outcome::GuardrailOutcome;
