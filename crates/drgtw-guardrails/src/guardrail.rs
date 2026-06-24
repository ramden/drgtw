//! The [`Guardrail`] trait: a content matcher that reports byte spans.

/// A content guardrail. Implementations scan text and report the byte spans
/// they matched. Stateless and thread-safe; compile any regexes once at
/// construction.
pub trait Guardrail: Send + Sync {
    /// Operator-facing name (appears in logs/traces when the rule fires).
    fn name(&self) -> &str;

    /// Byte spans matched in `text`. Empty vec = no match. Spans must be valid
    /// UTF-8 char boundaries (regex match offsets already are). Spans may
    /// overlap; the engine merges them.
    fn find(&self, text: &str) -> Vec<(usize, usize)>;
}
