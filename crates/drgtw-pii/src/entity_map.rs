//! Per-request entity↔placeholder map. WP 3.2 / WP 9.2 (store integration).

use std::collections::HashMap;
use std::sync::Arc;

use crate::store::EntityStore;
use crate::{DetectError, Detection, EntityKind};

/// Bidirectional map of original values to placeholders, scoped to one
/// request. Same value (same kind) always gets the same placeholder within
/// a map. Placeholder format: `{PREFIX}_{n}`, n starting at 1 in
/// first-seen order per prefix.
///
/// # Restore boundary rule
///
/// [`EntityMap::restore`] must not replace `EMAIL_1` when it appears as a
/// strict prefix of `EMAIL_12`. The implementation does a single left-to-right
/// positional scan: at each candidate position we check every placeholder that
/// starts at that position (sorted longest-first so the longest match wins),
/// and a match is only accepted when the character **immediately following**
/// the placeholder in the input is **not an ASCII digit** and not `_`.
/// This prevents `EMAIL_1` from being consumed when the next byte is `2`,
/// which would corrupt `EMAIL_12`.
///
/// # Persistent store (WP 9.2)
///
/// When constructed with [`EntityMap::with_store`] the map delegates
/// placeholder assignment to the provided [`EntityStore`]. The store is
/// the source of truth for `(kind_prefix, value)` → placeholder; the local
/// `forward`/`backward` maps act as a per-request cache so that
/// `restore`, `max_placeholder_len`, and streaming work unchanged within
/// the request.
///
/// Storeless behaviour (constructed with [`EntityMap::new`]) is identical
/// to the original implementation.
#[derive(Default)]
pub struct EntityMap {
    /// `(prefix, original_value)` → placeholder string
    forward: HashMap<(String, String), String>,
    /// placeholder → original_value
    backward: HashMap<String, String>,
    /// prefix → next counter (1-based); only used on the storeless path.
    counters: HashMap<String, u32>,
    /// Optional persistent store for cross-request stability (WP 9.2).
    store: Option<Arc<dyn EntityStore>>,
}

impl std::fmt::Debug for EntityMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntityMap")
            .field("forward_len", &self.forward.len())
            .field("backward_len", &self.backward.len())
            .field("has_store", &self.store.is_some())
            .finish()
    }
}

impl EntityMap {
    /// Create a new, storeless map (current-request scope only).
    ///
    /// Behaviour is identical to previous versions: counters are local,
    /// no cross-request stability.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a map backed by a persistent [`EntityStore`].
    ///
    /// Placeholder assignment is delegated to the store via
    /// [`EntityStore::get_or_assign`], which guarantees that the same
    /// `(kind, value)` pair always maps to the same placeholder across
    /// requests. The result is cached locally so `restore`,
    /// `max_placeholder_len`, and streaming work within the request
    /// without additional store round-trips.
    ///
    /// # Fail-closed
    ///
    /// Because silent fallback to local counters would corrupt cross-request
    /// identity, store errors bubble up through [`EntityMap::try_pseudonymize`]
    /// rather than being swallowed. Use `try_pseudonymize` (and
    /// [`crate::body::try_pseudonymize_body`]) when a store is configured.
    pub fn with_store(store: Arc<dyn EntityStore>) -> Self {
        Self {
            store: Some(store),
            ..Default::default()
        }
    }

    /// Rewrite `text`, replacing each detection span with its placeholder.
    /// Detections must be sorted + non-overlapping (engine guarantees this).
    /// Reuses existing placeholders for already-seen (kind, value) pairs.
    pub fn pseudonymize(&mut self, text: &str, detections: &[Detection]) -> String {
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;

        for det in detections {
            debug_assert!(det.start >= cursor, "detections not sorted/non-overlapping");
            debug_assert!(det.end <= text.len(), "detection end out of bounds");
            debug_assert!(
                text.is_char_boundary(det.start),
                "detection start not on char boundary"
            );
            debug_assert!(
                text.is_char_boundary(det.end),
                "detection end not on char boundary"
            );

            // Copy unchanged text before this span.
            out.push_str(&text[cursor..det.start]);

            let original = &text[det.start..det.end];
            let placeholder = self.get_or_insert(&det.kind, original);
            out.push_str(&placeholder);

            cursor = det.end;
        }

        // Remainder after last detection.
        out.push_str(&text[cursor..]);
        out
    }

    /// Fallible variant of [`pseudonymize`](Self::pseudonymize) for use when a
    /// persistent [`EntityStore`] is configured.
    ///
    /// Behaves identically to `pseudonymize` on the storeless path. When a
    /// store is present, placeholder assignment calls
    /// [`EntityStore::get_or_assign`], which may fail. On failure the error
    /// is wrapped as a [`DetectError`] and returned; the body must be treated
    /// as unusable by the caller (fail-closed).
    ///
    /// # When to use
    ///
    /// Prefer `pseudonymize` for the storeless case (no store configured).
    /// Use `try_pseudonymize` when a store **may** be configured — the body
    /// module calls this variant via `try_pseudonymize_body`.
    pub fn try_pseudonymize(
        &mut self,
        text: &str,
        detections: &[Detection],
    ) -> Result<String, DetectError> {
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;

        for det in detections {
            debug_assert!(det.start >= cursor, "detections not sorted/non-overlapping");
            debug_assert!(det.end <= text.len(), "detection end out of bounds");
            debug_assert!(
                text.is_char_boundary(det.start),
                "detection start not on char boundary"
            );
            debug_assert!(
                text.is_char_boundary(det.end),
                "detection end not on char boundary"
            );

            out.push_str(&text[cursor..det.start]);

            let original = &text[det.start..det.end];
            let placeholder = self.try_get_or_insert(&det.kind, original)?;
            out.push_str(&placeholder);

            cursor = det.end;
        }

        out.push_str(&text[cursor..]);
        Ok(out)
    }

    /// Replace every known placeholder occurrence in `text` with its
    /// original value. Unknown placeholder-shaped strings stay untouched.
    ///
    /// # Boundary rule
    ///
    /// A placeholder match at position `p` is accepted only when the byte
    /// immediately following the placeholder is **not** an ASCII digit (`0-9`)
    /// and not `_`. This ensures `EMAIL_1` inside `EMAIL_12` is skipped
    /// (next char is `2`), while `EMAIL_1` at end-of-string or followed by a
    /// space/punctuation is correctly replaced.
    ///
    /// Placeholders are checked longest-first at each position so that the
    /// longer `EMAIL_12` (if mapped) wins over `EMAIL_1` at the same offset.
    pub fn restore(&self, text: &str) -> String {
        if self.backward.is_empty() {
            return text.to_owned();
        }

        // Sort placeholders longest-first so that at each scan position the
        // longest candidate is tried first.
        let mut placeholders: Vec<(&str, &str)> = self
            .backward
            .iter()
            .map(|(ph, orig)| (ph.as_str(), orig.as_str()))
            .collect();
        placeholders.sort_unstable_by_key(|(ph, _)| std::cmp::Reverse(ph.len()));

        let bytes = text.as_bytes();
        let len = bytes.len();
        let mut out = String::with_capacity(text.len());
        // Start of the current verbatim (non-placeholder) run.
        let mut run_start = 0usize;
        let mut i = 0usize;

        'outer: while i < len {
            // Try each placeholder at position i (longest first).
            for &(ph, orig) in &placeholders {
                let ph_bytes = ph.as_bytes();
                let ph_len = ph_bytes.len();
                if i + ph_len > len {
                    continue;
                }
                if &bytes[i..i + ph_len] == ph_bytes {
                    // Boundary check: character immediately after must NOT be
                    // an ASCII digit or underscore, otherwise this placeholder
                    // is a strict prefix of a longer placeholder token.
                    let after = i + ph_len;
                    if after < len {
                        let next = bytes[after];
                        if next.is_ascii_digit() || next == b'_' {
                            // This match is a prefix of a longer token — skip.
                            continue;
                        }
                    }
                    // Flush the verbatim run, then the replacement.
                    // Placeholders are ASCII, so i and i+ph_len are always
                    // char boundaries of the original UTF-8 text.
                    out.push_str(&text[run_start..i]);
                    out.push_str(orig);
                    i += ph_len;
                    run_start = i;
                    continue 'outer;
                }
            }
            // No placeholder at i; extend the verbatim run by one byte.
            // (Slicing happens only at match points, which are ASCII-aligned,
            // so multibyte sequences are never split.)
            i += 1;
        }

        out.push_str(&text[run_start..]);
        out
    }

    /// Number of distinct entities mapped.
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Length in bytes of the longest placeholder (0 when empty).
    /// Streaming holdback sizing depends on this.
    pub fn max_placeholder_len(&self) -> usize {
        self.backward.keys().map(|k| k.len()).max().unwrap_or(0)
    }

    /// Iterate `(placeholder, original)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.backward
            .iter()
            .map(|(ph, orig)| (ph.as_str(), orig.as_str()))
    }

    // ── internal ────────────────────────────────────────────────────────────

    /// Look up or create a placeholder for `(kind, original)` using local
    /// counters only (storeless path). Infallible.
    fn get_or_insert(&mut self, kind: &EntityKind, original: &str) -> String {
        let prefix = kind.placeholder_prefix();
        let key = (prefix.clone(), original.to_owned());

        if let Some(ph) = self.forward.get(&key) {
            return ph.clone();
        }

        let n = self.counters.entry(prefix.clone()).or_insert(0);
        *n += 1;
        let placeholder = format!("{}_{}", prefix, n);

        self.forward.insert(key, placeholder.clone());
        self.backward
            .insert(placeholder.clone(), original.to_owned());
        placeholder
    }

    /// Look up or create a placeholder for `(kind, original)`, delegating to
    /// the store when one is configured. Fallible.
    ///
    /// * **Store present**: calls [`EntityStore::get_or_assign`] for the
    ///   authoritative placeholder, then caches it locally.
    /// * **No store**: falls back to [`get_or_insert`](Self::get_or_insert)
    ///   (local counters, infallible).
    fn try_get_or_insert(
        &mut self,
        kind: &EntityKind,
        original: &str,
    ) -> Result<String, DetectError> {
        let prefix = kind.placeholder_prefix();
        let key = (prefix.clone(), original.to_owned());

        // Check local cache first — avoids a store round-trip for repeated
        // occurrences of the same value within this request.
        if let Some(ph) = self.forward.get(&key) {
            return Ok(ph.clone());
        }

        if let Some(store) = &self.store {
            // Delegate to the persistent store for a stable placeholder.
            let placeholder = store
                .get_or_assign(&prefix, original)
                .map_err(|e| DetectError(format!("entity store: {e}")))?;

            // Cache locally so subsequent occurrences in the same request are
            // served from memory (no extra store calls).
            self.forward.insert(key, placeholder.clone());
            self.backward
                .insert(placeholder.clone(), original.to_owned());
            Ok(placeholder)
        } else {
            // Storeless path: use local counters (infallible).
            Ok(self.get_or_insert(kind, original))
        }
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Detection, EntityKind};

    fn det(start: usize, end: usize, kind: EntityKind) -> Detection {
        Detection { start, end, kind }
    }

    // ── pseudonymize ────────────────────────────────────────────────────────

    #[test]
    fn pseudonymize_single_email() {
        let mut map = EntityMap::new();
        let text = "hello alice@example.com world";
        // "alice@example.com" is bytes 6..23
        let dets = vec![det(6, 23, EntityKind::Email)];
        let result = map.pseudonymize(text, &dets);
        assert_eq!(result, "hello EMAIL_1 world");
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn pseudonymize_reuses_placeholder_same_value() {
        let mut map = EntityMap::new();
        let text = "alice@example.com alice@example.com";
        let dets = vec![
            det(0, 17, EntityKind::Email),
            det(18, 35, EntityKind::Email),
        ];
        let result = map.pseudonymize(text, &dets);
        assert_eq!(result, "EMAIL_1 EMAIL_1");
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn pseudonymize_different_values_different_placeholders() {
        let mut map = EntityMap::new();
        let text = "alice@example.com bob@example.com";
        let dets = vec![
            det(0, 17, EntityKind::Email),
            det(18, 33, EntityKind::Email),
        ];
        let result = map.pseudonymize(text, &dets);
        assert_eq!(result, "EMAIL_1 EMAIL_2");
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn pseudonymize_same_value_across_calls() {
        let mut map = EntityMap::new();
        let text1 = "contact alice@example.com now";
        let dets1 = vec![det(8, 25, EntityKind::Email)];
        let r1 = map.pseudonymize(text1, &dets1);
        assert!(r1.contains("EMAIL_1"));

        let text2 = "also alice@example.com here";
        let dets2 = vec![det(5, 22, EntityKind::Email)];
        let r2 = map.pseudonymize(text2, &dets2);
        assert!(
            r2.contains("EMAIL_1"),
            "same value must reuse placeholder: {r2}"
        );
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn pseudonymize_counters_per_prefix() {
        let mut map = EntityMap::new();
        let text = "+1-555-0100 alice@x.com +1-555-0200";
        // phone: 0..11, email: 12..23, phone: 24..35
        let dets = vec![
            det(0, 11, EntityKind::Phone),
            det(12, 23, EntityKind::Email),
            det(24, 35, EntityKind::Phone),
        ];
        let result = map.pseudonymize(text, &dets);
        assert_eq!(result, "PHONE_1 EMAIL_1 PHONE_2");
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn pseudonymize_no_detections() {
        let mut map = EntityMap::new();
        let text = "nothing to see here";
        let result = map.pseudonymize(text, &[]);
        assert_eq!(result, text);
        assert!(map.is_empty());
    }

    #[test]
    fn pseudonymize_multibyte_text() {
        let mut map = EntityMap::new();
        // "héllo " is 7 bytes (é = 2 bytes), then "a@b.com" at byte 7..14
        let text = "héllo a@b.com";
        assert_eq!(&text[7..14], "a@b.com");
        let dets = vec![det(7, 14, EntityKind::Email)];
        let result = map.pseudonymize(text, &dets);
        assert_eq!(result, "héllo EMAIL_1");
    }

    #[test]
    fn pseudonymize_span_at_start_and_end() {
        let mut map = EntityMap::new();
        let text = "a@b.com hello c@d.com";
        let dets = vec![det(0, 7, EntityKind::Email), det(14, 21, EntityKind::Email)];
        let result = map.pseudonymize(text, &dets);
        assert_eq!(result, "EMAIL_1 hello EMAIL_2");
    }

    #[test]
    fn pseudonymize_custom_kind() {
        let mut map = EntityMap::new();
        use std::sync::Arc;
        let kind = EntityKind::Custom(Arc::from("ssn"));
        let text = "ssn=123-45-6789";
        let dets = vec![det(4, 15, kind)];
        let result = map.pseudonymize(text, &dets);
        assert_eq!(result, "ssn=SSN_1");
    }

    // ── restore ─────────────────────────────────────────────────────────────

    #[test]
    fn restore_basic() {
        let mut map = EntityMap::new();
        map.pseudonymize("alice@example.com", &[det(0, 17, EntityKind::Email)]);
        let result = map.restore("contact EMAIL_1 please");
        assert_eq!(result, "contact alice@example.com please");
    }

    #[test]
    fn restore_unknown_placeholder_untouched() {
        let mut map = EntityMap::new();
        map.pseudonymize("alice@example.com", &[det(0, 17, EntityKind::Email)]);
        // EMAIL_99 not in map → stays as-is
        let result = map.restore("EMAIL_1 and EMAIL_99 here");
        assert_eq!(result, "alice@example.com and EMAIL_99 here");
    }

    #[test]
    fn restore_boundary_email1_does_not_corrupt_email12() {
        // Only EMAIL_1 is known; input contains EMAIL_12 which is unknown.
        // EMAIL_1 is followed by '2' → boundary check skips it → EMAIL_12 untouched.
        let mut map = EntityMap::new();
        map.pseudonymize("alice@example.com", &[det(0, 17, EntityKind::Email)]);
        let result = map.restore("EMAIL_12");
        assert_eq!(
            result, "EMAIL_12",
            "EMAIL_12 must stay untouched when only EMAIL_1 is mapped"
        );
    }

    #[test]
    fn restore_boundary_both_email1_and_email12_mapped() {
        // Build a map with 12 distinct emails so EMAIL_1 and EMAIL_12 both exist.
        let mut map = EntityMap::new();
        // 12 distinct single-char-local emails: a@b.com … m@b.com (skipping j)
        let emails: Vec<String> = (b'a'..=b'm')
            .filter(|&c| c != b'j') // skip j to avoid confusing offsets
            .take(12)
            .map(|c| format!("{}@b.com", c as char))
            .collect();
        for (i, email) in emails.iter().enumerate() {
            // Each "text" is just the email itself; offsets 0..len
            map.pseudonymize(email, &[det(0, email.len(), EntityKind::Email)]);
            let expected_ph = format!("EMAIL_{}", i + 1);
            assert_eq!(
                map.restore(&expected_ph),
                *email,
                "EMAIL_{} should restore to {}",
                i + 1,
                email
            );
        }
        // Now both EMAIL_1 and EMAIL_12 exist. Test that EMAIL_12 restores correctly.
        let ph12 = "EMAIL_12";
        let orig12 = &emails[11]; // 12th email
        let restored = map.restore(ph12);
        assert_eq!(
            restored, *orig12,
            "EMAIL_12 should restore to {orig12}, got {restored}"
        );

        // And EMAIL_1 by itself (followed by space) should still restore.
        let restored1 = map.restore("EMAIL_1 was here");
        assert!(
            restored1.starts_with(&emails[0]),
            "EMAIL_1 followed by space must restore: {restored1}"
        );
    }

    #[test]
    fn restore_placeholder_at_string_end() {
        let mut map = EntityMap::new();
        map.pseudonymize("alice@example.com", &[det(0, 17, EntityKind::Email)]);
        let result = map.restore("contact EMAIL_1");
        assert_eq!(result, "contact alice@example.com");
    }

    #[test]
    fn restore_multiple_occurrences() {
        let mut map = EntityMap::new();
        map.pseudonymize("a@b.com", &[det(0, 7, EntityKind::Email)]);
        let result = map.restore("EMAIL_1 and EMAIL_1 again");
        assert_eq!(result, "a@b.com and a@b.com again");
    }

    #[test]
    fn restore_empty_map_returns_text_unchanged() {
        let map = EntityMap::new();
        let text = "no placeholders here";
        assert_eq!(map.restore(text), text);
    }

    /// Regression: restore must not corrupt multibyte UTF-8 around (and
    /// between) placeholders. The original implementation pushed raw bytes
    /// as chars (`bytes[i] as char`), mangling every non-ASCII character.
    #[test]
    fn restore_preserves_multibyte_text() {
        let mut map = EntityMap::new();
        map.pseudonymize("a@b.de", &[det(0, 6, EntityKind::Email)]);
        let text = "Grüße 🦀 EMAIL_1 — schöne Straße ümläüt";
        let result = map.restore(text);
        assert_eq!(result, "Grüße 🦀 a@b.de — schöne Straße ümläüt");
    }

    // ── max_placeholder_len / iter ──────────────────────────────────────────

    #[test]
    fn max_placeholder_len_empty() {
        let map = EntityMap::new();
        assert_eq!(map.max_placeholder_len(), 0);
    }

    #[test]
    fn max_placeholder_len_correct() {
        let mut map = EntityMap::new();
        // EMAIL_1 = 7 bytes, PHONE_1 = 7 bytes
        map.pseudonymize(
            "a@b.com +1-555-0100",
            &[det(0, 7, EntityKind::Email), det(8, 19, EntityKind::Phone)],
        );
        assert_eq!(map.max_placeholder_len(), 7);
    }

    #[test]
    fn iter_yields_all_pairs() {
        let mut map = EntityMap::new();
        map.pseudonymize(
            "a@b.com c@d.com",
            &[det(0, 7, EntityKind::Email), det(8, 15, EntityKind::Email)],
        );
        let mut pairs: Vec<(String, String)> = map
            .iter()
            .map(|(ph, orig)| (ph.to_owned(), orig.to_owned()))
            .collect();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("EMAIL_1".to_owned(), "a@b.com".to_owned()),
                ("EMAIL_2".to_owned(), "c@d.com".to_owned()),
            ]
        );
    }
}
