//! The result of evaluating guardrails against a piece of text.

/// Outcome of running a phase's guardrail rules against text.
///
/// Produced by [`crate::GuardrailEngine::check_request`] and
/// [`crate::GuardrailEngine::check_response`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardrailOutcome {
    /// No rule matched (or every matching rule was a no-op). Pass through.
    Allow,
    /// One or more `Redact` rules matched. Byte spans `(start, end)` into the
    /// scanned text. Sorted, deduped, and merged so no two spans overlap.
    Redact(Vec<(usize, usize)>),
    /// A `Block` rule matched. Carries a human-readable reason. Short-circuits
    /// evaluation — no later rule runs.
    Block(String),
    /// One or more `Flag` rules matched (and no `Redact`/`Block` did). Carries a
    /// human-readable reason; content still passes through.
    Flag(String),
}
