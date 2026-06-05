//! Pluggable entity-store trait for cross-request placeholder stability. WP 9.2.
//!
//! # Purpose
//!
//! Within a single request the [`EntityMap`] already guarantees that the same
//! PII value always gets the same placeholder. That is enough for the
//! stateless proxy case. When a persistent vault (e.g. the SQLite-backed
//! `drgtw-vault` crate) is configured, we need **cross-request** stability:
//!
//! * Embeddings / RAG pipelines send the same document in multiple requests;
//!   the same email must always become `EMAIL_1` so vectors stay comparable.
//! * A previous request may have left placeholders in stored text; `restore`
//!   for those placeholders must work even though the in-memory map is fresh.
//!
//! # Design
//!
//! [`EntityStore`] is a minimal trait with two methods:
//!
//! * [`EntityStore::get_or_assign`] — stable placeholder assignment.
//! * [`EntityStore::lookup_placeholder`] — reverse lookup for restore.
//!
//! The concrete implementation lives in `drgtw-vault`; the pii crate only
//! sees this trait. The `drgtw-pii` crate is a pure dependency of the vault,
//! so there is no circular reference.
//!
//! # Fail-closed contract
//!
//! A store error **must** abort the request. Silently falling back to
//! local-only counters would produce a different placeholder for the same
//! value across requests, corrupting cross-request identity and the
//! embeddings store. Therefore [`EntityMap::try_pseudonymize`] (the store
//! path) returns `Result` and propagates [`StoreError`] up through
//! [`DetectError`].
//!
//! [`EntityMap`]: crate::EntityMap
//! [`DetectError`]: crate::DetectError

/// Persistent, cross-request entity↔placeholder store.
///
/// Implementors must be [`Send`] + [`Sync`] because they are shared across
/// Tokio tasks via `Arc<dyn EntityStore>`.
///
/// # Fail-closed guarantee
///
/// Both methods return `Result`. The pii crate propagates errors as
/// [`crate::DetectError`] so that the request handler can fail fast rather
/// than silently generating a divergent placeholder that would corrupt
/// cross-request identity.
pub trait EntityStore: Send + Sync {
    /// Return the stable placeholder for `(kind_prefix, value)`, creating a
    /// new assignment if this `(kind_prefix, value)` pair has never been seen.
    ///
    /// `kind_prefix` is the uppercase string prefix, e.g. `"EMAIL"`,
    /// `"PHONE"`, or a custom recognizer name.
    ///
    /// The returned string must satisfy the placeholder format
    /// `{kind_prefix}_{n}` where `n ≥ 1`.
    fn get_or_assign(&self, kind_prefix: &str, value: &str) -> Result<String, StoreError>;

    /// Reverse lookup: given a placeholder string, return the original value,
    /// or `None` if the placeholder is not known to this store.
    ///
    /// Used by [`restore_body_with_store`](crate::body::restore_body_with_store)
    /// to restore placeholders from **past** requests that are not present in
    /// the current request's in-memory [`EntityMap`](crate::EntityMap).
    fn lookup_placeholder(&self, placeholder: &str) -> Result<Option<String>, StoreError>;
}

/// Error returned by [`EntityStore`] operations.
///
/// Wraps a human-readable message. The pii crate converts this to a
/// [`crate::DetectError`] so that a single error type flows through the
/// request pipeline.
#[derive(Debug, thiserror::Error)]
#[error("entity store error: {0}")]
pub struct StoreError(pub String);
